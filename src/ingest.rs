use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::chunking;
use crate::config::{self, Parser};
use crate::llm::LlmBackend;
use crate::markdown;
use crate::minhash;
use crate::prompts;
use crate::storage;
use crate::types::*;
use crate::util;

#[derive(Deserialize)]
struct PreprocessorOutput {
	entries: Vec<PreprocessorEntry>,
}

#[derive(Deserialize)]
struct PreprocessorEntry {
	body: String,
	#[serde(default)]
	author: Option<String>,
	#[serde(default)]
	timestamp: Option<String>,
	#[serde(default)]
	heading_title: Option<String>,
	#[serde(default)]
	heading_level: Option<u8>,
}

pub fn run_preprocessor(script_path: &str, file_path: &Path) -> Result<SegmentationResult> {
	let output = Command::new("python3")
		.arg(script_path)
		.arg(file_path)
		.output()
		.with_context(|| format!("failed to run preprocessor: {}", script_path))?;

	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr);
		anyhow::bail!("preprocessor failed: {}", stderr);
	}

	let stdout = String::from_utf8(output.stdout)
		.context("preprocessor output is not valid UTF-8")?;

	let parsed: PreprocessorOutput = serde_json::from_str(&stdout)
		.with_context(|| format!("failed to parse preprocessor JSON: {}", &stdout[..stdout.len().min(200)]))?;

	let entries: Vec<SegmentedEntry> = parsed.entries
		.into_iter()
		.enumerate()
		.filter(|(_, e)| !e.body.trim().is_empty())
		.map(|(i, e)| SegmentedEntry {
			start_line: i + 1,
			end_line: i + 1,
			body: e.body,
			author: e.author,
			timestamp: e.timestamp,
			heading_title: e.heading_title,
			heading_level: e.heading_level,
			is_quote: false,
		})
		.collect();

	Ok(SegmentationResult { entries })
}

pub fn parse_source_header(first_line: &str) -> Option<String> {
	let captures = first_line.strip_prefix("# source:")?;
	Some(captures.trim().to_string())
}

pub fn parse_clip_date(filename: &str) -> Option<chrono::NaiveDateTime> {
	let stem = Path::new(filename)
		.file_stem()?
		.to_str()?;
	let formats = [
		"%Y%m%d_%H-%M-%S",
		"%Y%m%d_%H%M%S",
	];
	for format in &formats {
		if let Ok(date) = chrono::NaiveDateTime::parse_from_str(stem, format) {
			return Some(date);
		}
	}
	None
}

pub fn parse_copilot_email_summary(text: &str) -> Vec<SegmentedEntry> {
	let outlook_suffix = Regex::new(r"\s*\[.*?\|\s*Outlook\]\s*$").unwrap();

	let chunks: Vec<&str> = text.split("\nEMAIL\n")
		.flat_map(|s| s.split("\nEMAIL\r\n"))
		.flat_map(|s| s.split("\n#EMAIL\n"))
		.flat_map(|s| s.split("\n### EMAIL\n"))
		.flat_map(|s| s.split("\n##EMAIL\n"))
		.collect();

	let mut entries = Vec::new();

	for chunk in chunks {
		let chunk = chunk.trim();
		if chunk.is_empty() {
			continue;
		}

		let lines: Vec<&str> = chunk.lines().collect();
		if lines.is_empty() {
			continue;
		}

		let mut from: Option<String> = None;
		let mut date: Option<String> = None;
		let mut subject: Option<String> = None;
		let mut body_start = 0;

		for (i, line) in lines.iter().enumerate() {
			let line_lower = line.to_lowercase();
			if line_lower.starts_with("from:") {
				from = Some(line[5..].trim().to_string());
			} else if line_lower.starts_with("date:") {
				date = Some(line[5..].trim().to_string());
			} else if line_lower.starts_with("subject:") {
				subject = Some(line[8..].trim().to_string());
			} else if line.trim().is_empty() && (from.is_some() || date.is_some() || subject.is_some()) {
				body_start = i + 1;
				break;
			} else if !line_lower.starts_with("to:") && !line_lower.starts_with("cc:") {
				body_start = i;
				break;
			}
		}

		let body = lines[body_start..].join("\n");
		let body = outlook_suffix.replace_all(&body, "").trim().to_string();

		if body.is_empty() && from.is_none() && date.is_none() {
			continue;
		}

		entries.push(SegmentedEntry {
			start_line: 0,
			end_line: 0,
			body,
			author: from,
			timestamp: date,
			is_quote: false,
			heading_level: None,
			heading_title: subject,
		});
	}

	entries
}

const merge_min_chars: usize = 150;
const dup_jaccard_high: f64 = 0.7;
const dup_jaccard_low: f64 = 0.4;
const dup_shared_block_words: usize = 300;
const dup_window_days: i64 = 180;

pub struct IngestOptions<'a> {
	pub force: bool,
	pub interactive: bool,
	pub backend: Option<&'a dyn LlmBackend>,
	pub model: String,
}

struct DiffSummary {
	text: String,
	model: String,
	prompt_hash: String,
}

pub enum IngestOutcome {
	Ingested,
	Skipped,
	Quit,
}

pub struct GrayZoneCase {
	pub new_path: String,
	pub existing_id: i64,
	pub existing_path: String,
	pub similarity: f64,
	pub shared_block_words: usize,
}

enum GrayZoneResolution {
	Supersede,
	KeepBoth,
	Quit,
}

fn find_overlap(
	existing: &[storage::ExistingEntry],
	new_entries: &[SegmentedEntry],
) -> Option<(usize, usize)> {
	if existing.is_empty() || new_entries.is_empty() {
		return None;
	}

	fn entries_match(existing: &storage::ExistingEntry, new: &SegmentedEntry) -> bool {
		existing.body.trim() == new.body.trim() && existing.author == new.author
	}

	let mut best_run_start: Option<usize> = None;
	let mut best_run_len = 0usize;
	let mut best_run_chars = 0usize;

	for new_start in 0..new_entries.len() {
		for exist_start in 0..existing.len() {
			if entries_match(&existing[exist_start], &new_entries[new_start]) {
				let mut run_len = 1;
				let mut run_chars = new_entries[new_start].body.len();

				while exist_start + run_len < existing.len()
					&& new_start + run_len < new_entries.len()
					&& entries_match(&existing[exist_start + run_len], &new_entries[new_start + run_len])
				{
					run_chars += new_entries[new_start + run_len].body.len();
					run_len += 1;
				}

				if run_chars > best_run_chars {
					best_run_start = Some(new_start);
					best_run_len = run_len;
					best_run_chars = run_chars;
				}
			}
		}
	}

	if best_run_chars >= merge_min_chars {
		best_run_start.map(|start| (start, best_run_len))
	} else {
		None
	}
}

fn join_entry_bodies(entries: &[SegmentedEntry]) -> String {
	entries.iter()
		.map(|e| e.body.as_str())
		.collect::<Vec<_>>()
		.join("\n")
}

fn print_block_diff(new_text: &str, existing_text: &str, existing_id: i64) {
	let (added, removed) = util::diff_regions(new_text, existing_text);
	eprintln!("  --- only in new file ---");
	for line in added.lines() {
		eprintln!("  + {}", line);
	}
	eprintln!("  --- only in doc {} ---", existing_id);
	for line in removed.lines() {
		eprintln!("  - {}", line);
	}
}

fn prompt_gray_zone(
	new_path: &str,
	existing_id: i64,
	existing_path: &str,
	existing_date: &str,
	similarity: f64,
	shared_block_words: usize,
	new_text: &str,
	existing_text: &str,
	backend: Option<&dyn LlmBackend>,
	model: &str,
) -> (GrayZoneResolution, Option<DiffSummary>) {
	use std::io::Write;

	eprintln!("Near-duplicate detected:");
	eprintln!("  existing: {} (doc {}, ingested {})", existing_path, existing_id, existing_date);
	eprintln!("  new:      {} (current file)", new_path);
	eprintln!("  similarity: {:.2}, shared block: ~{} words", similarity, shared_block_words);

	let mut summary: Option<DiffSummary> = None;
	loop {
		eprint!("\n  [s]upersede old  [k]eep both  [d]iff  [v]iew LLM summary  [q]uit  ");
		let _ = std::io::stderr().flush();

		let mut input = String::new();
		if std::io::stdin().read_line(&mut input).unwrap_or(0) == 0 {
			return (GrayZoneResolution::KeepBoth, summary);
		}
		match input.trim().chars().next() {
			Some('s') | Some('S') => return (GrayZoneResolution::Supersede, summary),
			Some('k') | Some('K') => return (GrayZoneResolution::KeepBoth, summary),
			Some('q') | Some('Q') => return (GrayZoneResolution::Quit, summary),
			Some('d') | Some('D') => print_block_diff(new_text, existing_text, existing_id),
			Some('v') | Some('V') => {
				if let Some(ref existing) = summary {
					eprintln!("\n{}\n", existing.text);
				} else {
					match backend {
						Some(llm) => {
							let (added, removed) = util::diff_regions(new_text, existing_text);
							let instructions = prompts::default_diff_instructions();
							let prompt = prompts::document_diff_prompt(&added, &removed, instructions);
							eprintln!("  generating summary...");
							match llm.chat(&prompt, model) {
								Ok(text) => {
									eprintln!("\n{}\n", text);
									summary = Some(DiffSummary {
										text,
										model: model.to_string(),
										prompt_hash: prompts::compute_prompt_hash(instructions),
									});
								}
								Err(error) => eprintln!("  LLM summary failed: {}", error),
							}
						}
						None => eprintln!("  no LLM backend configured"),
					}
				}
			}
			_ => eprintln!("  unrecognized choice"),
		}
	}
}

pub fn print_gray_zone_summary(cases: &[GrayZoneCase]) {
	if cases.is_empty() {
		return;
	}
	eprintln!("\n{} gray-zone near-duplicate(s) kept for manual review:", cases.len());
	for case in cases {
		eprintln!(
			"  {}\n    vs doc {} {} (similarity: {:.2}, shared block: ~{} words)",
			case.new_path,
			case.existing_id,
			case.existing_path,
			case.similarity,
			case.shared_block_words,
		);
	}
}

pub fn ingest_file(
	connection: &rusqlite::Connection,
	file_path: &Path,
	config: &config::Config,
	options: &IngestOptions<'_>,
	gray_zones: &mut Vec<GrayZoneCase>,
) -> Result<IngestOutcome> {
	let canonical_path = file_path.canonicalize()
		.unwrap_or_else(|_| file_path.to_path_buf());
	let file_path = canonical_path.as_path();
	let file_path_str = file_path.to_string_lossy();

	if !options.force && storage::document_exists_by_path(connection, &file_path_str)? {
		return Ok(IngestOutcome::Skipped);
	}

	let text = std::fs::read_to_string(file_path)
		.with_context(|| format!("reading {}", file_path.display()))?;

	let lines: Vec<&str> = text.lines().collect();
	if lines.is_empty() {
		return Ok(IngestOutcome::Skipped);
	}

	let source_title = parse_source_header(lines[0])
		.unwrap_or_else(|| file_path.display().to_string());

	let body = if parse_source_header(lines[0]).is_some() {
		lines[1..].join("\n")
	} else {
		text.clone()
	};

	let clip_date = file_path
		.file_name()
		.and_then(|name| name.to_str())
		.and_then(parse_clip_date)
		.unwrap_or_else(|| chrono::Local::now().naive_local());
	let clip_date_str = clip_date.format("%Y-%m-%d %H:%M:%S").to_string();

	let file_extension = file_path
		.extension()
		.and_then(|ext| ext.to_str());

	let doctype_match = config.detect_with_content(&source_title, file_extension, &body);

	if let Some(ref m) = doctype_match {
		if m.skip {
			eprintln!("  skipping (doctype '{}' marked skip)", m.name);
			return Ok(IngestOutcome::Skipped);
		}
	}

	let parser = doctype_match.as_ref()
		.map(|m| m.parser)
		.unwrap_or(Parser::Whole);

	let merge_strategy = doctype_match.as_ref()
		.map(|m| m.merge_strategy)
		.unwrap_or(MergeStrategy::None);

	let preprocessor = doctype_match.as_ref()
		.and_then(|m| m.preprocessor.clone());

	let segmented = if let Some(ref script) = preprocessor {
		let result = run_preprocessor(script, file_path)?;
		result.entries
	} else {
		match parser {
			Parser::Markdown => markdown::parse_markdown_sections(&body),
			Parser::CopilotEmail => parse_copilot_email_summary(&body),
			Parser::Whisper => {
				eprintln!("  whisper parser not yet implemented, skipping");
				return Ok(IngestOutcome::Skipped);
			}
			Parser::Whole => {
				vec![SegmentedEntry {
					start_line: 1,
					end_line: body.lines().count(),
					author: None,
					timestamp: None,
					body: body.clone(),
					is_quote: false,
					heading_level: None,
					heading_title: None,
				}]
			}
		}
	};

	if segmented.is_empty() {
		eprintln!("  no entries found, skipping");
		return Ok(IngestOutcome::Skipped);
	}

	let title = segmented.iter()
		.find(|e| e.heading_title.is_some())
		.and_then(|e| e.heading_title.clone())
		.map(|t| util::normalize_to_ascii(&t));

	let source_title = util::normalize_to_ascii(&source_title);
	let merge_key = util::strip_source_suffix(&source_title);

	let doctype_name = doctype_match.as_ref().map(|m| m.name.as_str());

	if merge_strategy == MergeStrategy::Positional {
		let existing_docs = storage::find_documents_by_merge_key(
			connection,
			util::strip_source_suffix,
			&merge_key,
			"positional",
		)?;

		for &existing_doc_id in &existing_docs {
			let existing_entries = storage::get_entries_for_document(connection, existing_doc_id)?;

			if let Some((overlap_start, overlap_len)) = find_overlap(&existing_entries, &segmented) {
				let new_entries_start = overlap_start + overlap_len;
				let entries_to_add = &segmented[new_entries_start..];

				if entries_to_add.is_empty() {
					eprintln!("  all entries already exist in doc {}, skipping", existing_doc_id);
					return Ok(IngestOutcome::Skipped);
				}

				let transaction = connection.unchecked_transaction()?;

				let max_pos = storage::get_max_entry_position(&transaction, existing_doc_id)?;
				let mut total_chunks = 0usize;

				for (i, entry) in entries_to_add.iter().enumerate() {
					let position = (max_pos + 1 + i as i64) as u32;
					let hash = minhash::minhash(&entry.body);
					let entry_id = storage::insert_entry(
						&transaction,
						DocumentId(existing_doc_id),
						entry,
						position,
						&source_title,
						&clip_date_str,
						&file_path_str,
						&hash,
					)?;

					let chunks = chunking::chunk_text(&entry.body);
					storage::insert_chunks(&transaction, entry_id, &chunks)?;
					total_chunks += chunks.len();
				}

				storage::update_document_clip_date(&transaction, existing_doc_id, &clip_date_str)?;
				transaction.commit()?;

				eprintln!(
					"  merged {} entries ({} chunks) into doc {} (overlap: {} entries)",
					entries_to_add.len(),
					total_chunks,
					existing_doc_id,
					overlap_len,
				);
				return Ok(IngestOutcome::Ingested);
			}
		}
	}

	let doc_hash = minhash::minhash_document(&segmented);
	let new_text = join_entry_bodies(&segmented);
	let new_path = file_path_str.to_string();

	let candidate_path = |candidate: &storage::DupCandidate| -> String {
		candidate.origin_path.clone()
			.unwrap_or_else(|| candidate.source_title.clone())
	};

	enum Resolution {
		Superseded,
		KeptBoth,
		Pending,
	}
	struct NearDupEvent {
		existing_id: i64,
		existing_path: String,
		similarity: f64,
		shared_block_words: Option<usize>,
		resolution: Resolution,
		summary: Option<DiffSummary>,
	}
	struct GrayCandidate {
		id: i64,
		path: String,
		date: String,
		similarity: f64,
		shared_block_words: usize,
		existing_text: String,
	}

	let candidates = storage::find_dup_candidates(connection, &clip_date_str, dup_window_days)?;
	let mut near_dup: Option<NearDupEvent> = None;
	let mut strongest_gray: Option<GrayCandidate> = None;

	for candidate in &candidates {
		let sim = minhash::jaccard(&doc_hash, &candidate.document_minhash);
		if sim >= dup_jaccard_high {
			near_dup = Some(NearDupEvent {
				existing_id: candidate.id,
				existing_path: candidate_path(candidate),
				similarity: sim,
				shared_block_words: None,
				resolution: Resolution::Superseded,
				summary: None,
			});
			break;
		}
		if sim >= dup_jaccard_low {
			let existing_entries = storage::get_entries_for_document(connection, candidate.id)?;
			let existing_text = existing_entries.iter()
				.map(|e| e.body.as_str())
				.collect::<Vec<_>>()
				.join("\n");
			let shared = minhash::longest_shared_block_words(&new_text, &existing_text);
			if shared >= dup_shared_block_words {
				near_dup = Some(NearDupEvent {
					existing_id: candidate.id,
					existing_path: candidate_path(candidate),
					similarity: sim,
					shared_block_words: Some(shared),
					resolution: Resolution::Superseded,
					summary: None,
				});
				break;
			}
			let stronger = strongest_gray.as_ref()
				.map(|g| (shared, sim) > (g.shared_block_words, g.similarity))
				.unwrap_or(true);
			if stronger {
				strongest_gray = Some(GrayCandidate {
					id: candidate.id,
					path: candidate_path(candidate),
					date: candidate.clip_date.clone(),
					similarity: sim,
					shared_block_words: shared,
					existing_text,
				});
			}
		}
	}

	if near_dup.is_none() {
		if let Some(gray) = strongest_gray {
			let (resolution, summary) = if options.interactive {
				let (choice, summary) = prompt_gray_zone(
					&new_path,
					gray.id,
					&gray.path,
					&gray.date,
					gray.similarity,
					gray.shared_block_words,
					&new_text,
					&gray.existing_text,
					options.backend,
					&options.model,
				);
				let resolution = match choice {
					GrayZoneResolution::Supersede => Resolution::Superseded,
					GrayZoneResolution::KeepBoth => {
						eprintln!(
							"  keeping both: doc {} {} (similarity: {:.2}, shared block: ~{} words)",
							gray.id, gray.path, gray.similarity, gray.shared_block_words,
						);
						Resolution::KeptBoth
					}
					GrayZoneResolution::Quit => {
						eprintln!("  aborted at {}", new_path);
						return Ok(IngestOutcome::Quit);
					}
				};
				(resolution, summary)
			} else {
				eprintln!(
					"  near-match: doc {} {} (similarity: {:.2}, shared block: ~{} words) — keeping both",
					gray.id, gray.path, gray.similarity, gray.shared_block_words,
				);
				gray_zones.push(GrayZoneCase {
					new_path: new_path.clone(),
					existing_id: gray.id,
					existing_path: gray.path.clone(),
					similarity: gray.similarity,
					shared_block_words: gray.shared_block_words,
				});
				(Resolution::Pending, None)
			};
			near_dup = Some(NearDupEvent {
				existing_id: gray.id,
				existing_path: gray.path,
				similarity: gray.similarity,
				shared_block_words: Some(gray.shared_block_words),
				resolution,
				summary,
			});
		}
	}

	let transaction = connection.unchecked_transaction()?;

	let document_id = storage::insert_document(
		&transaction,
		title.as_deref(),
		&source_title,
		doctype_name,
		merge_strategy,
		Some(&file_path_str),
		&clip_date_str,
		Some(&doc_hash),
	)?;

	let mut total_chunks = 0usize;
	for (position, entry) in segmented.iter().enumerate() {
		let hash = minhash::minhash(&entry.body);
		let entry_id = storage::insert_entry(
			&transaction,
			document_id,
			entry,
			position as u32,
			&source_title,
			&clip_date_str,
			&file_path_str,
			&hash,
		)?;

		let chunks = chunking::chunk_text(&entry.body);
		storage::insert_chunks(&transaction, entry_id, &chunks)?;
		total_chunks += chunks.len();
	}

	if let Some(event) = near_dup {
		let resolution = match event.resolution {
			Resolution::Superseded => "superseded",
			Resolution::KeptBoth => "kept_both",
			Resolution::Pending => "pending",
		};
		if resolution == "superseded" {
			storage::add_tag(&transaction, event.existing_id, "superseded")?;
			eprintln!(
				"  tagged doc {} {} as superseded (similarity: {:.2})",
				event.existing_id, event.existing_path, event.similarity,
			);
		}
		let relation_id = storage::insert_document_relation(
			&transaction,
			document_id.0,
			event.existing_id,
			"near_duplicate",
			event.similarity,
			event.shared_block_words.map(|words| words as i64),
			resolution,
		)?;
		if let Some(summary) = event.summary {
			storage::set_relation_summary(
				&transaction,
				relation_id,
				&summary.text,
				&summary.model,
				&summary.prompt_hash,
			)?;
		}
	}

	transaction.commit()?;

	eprintln!(
		"  {} entries, {} chunks from \"{}\"",
		segmented.len(),
		total_chunks,
		source_title,
	);
	Ok(IngestOutcome::Ingested)
}

pub fn ingest_directory(
	connection: &rusqlite::Connection,
	directory: &Path,
	config: &config::Config,
	options: &IngestOptions<'_>,
) -> Result<(u32, u32)> {
	let mut ingested = 0u32;
	let mut skipped = 0u32;
	let mut paths: Vec<PathBuf> = std::fs::read_dir(directory)?
		.filter_map(|entry| entry.ok())
		.map(|entry| entry.path())
		.filter(|path| {
			path.extension()
				.map(|ext| ext == "txt" || ext == "md")
				.unwrap_or(false)
		})
		.collect();
	paths.sort();

	eprintln!("found {} files in {}", paths.len(), directory.display());

	let mut gray_zones = Vec::new();

	for path in &paths {
		match ingest_file(connection, path, config, options, &mut gray_zones) {
			Ok(IngestOutcome::Ingested) => ingested += 1,
			Ok(IngestOutcome::Skipped) => skipped += 1,
			Ok(IngestOutcome::Quit) => {
				eprintln!("aborted, stopping ingest");
				break;
			}
			Err(error) => eprintln!("error ingesting {}: {:#}", path.display(), error),
		}
	}

	print_gray_zone_summary(&gray_zones);
	Ok((ingested, skipped))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parse_source_header_valid() {
		assert_eq!(
			parse_source_header("# source: My Article - Brave"),
			Some("My Article - Brave".to_string()),
		);
	}

	#[test]
	fn parse_source_header_extra_whitespace() {
		assert_eq!(
			parse_source_header("# source:   padded title  "),
			Some("padded title".to_string()),
		);
	}

	#[test]
	fn parse_source_header_missing() {
		assert_eq!(parse_source_header("no source line"), None);
		assert_eq!(parse_source_header("## source: wrong prefix"), None);
	}

	#[test]
	fn parse_clip_date_underscore_dash() {
		let date = parse_clip_date("20240315_14-30-00.txt");
		assert!(date.is_some());
		let date = date.unwrap();
		assert_eq!(date.format("%Y-%m-%d %H:%M:%S").to_string(), "2024-03-15 14:30:00");
	}

	#[test]
	fn parse_clip_date_underscore_nondash() {
		let date = parse_clip_date("20240315_143000.txt");
		assert!(date.is_some());
	}

	#[test]
	fn parse_clip_date_invalid() {
		assert!(parse_clip_date("notes.txt").is_none());
		assert!(parse_clip_date("random_name.txt").is_none());
	}

	#[test]
	fn copilot_email_single_message() {
		let text = "From: Alice\nDate: 2024-01-15\nSubject: Hello\n\nHey there, how are you?";
		let entries = parse_copilot_email_summary(text);
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].author.as_deref(), Some("Alice"));
		assert_eq!(entries[0].timestamp.as_deref(), Some("2024-01-15"));
		assert_eq!(entries[0].heading_title.as_deref(), Some("Hello"));
		assert!(entries[0].body.contains("Hey there"));
	}

	#[test]
	fn copilot_email_multiple_messages() {
		let text = "From: Alice\nDate: 2024-01-15\n\nFirst message\nEMAIL\nFrom: Bob\nDate: 2024-01-16\n\nSecond message";
		let entries = parse_copilot_email_summary(text);
		assert_eq!(entries.len(), 2);
		assert_eq!(entries[0].author.as_deref(), Some("Alice"));
		assert_eq!(entries[1].author.as_deref(), Some("Bob"));
	}

	#[test]
	fn copilot_email_empty() {
		let entries = parse_copilot_email_summary("");
		assert!(entries.is_empty());
	}

}
