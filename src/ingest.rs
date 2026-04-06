use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::chunking;
use crate::config::{self, Parser};
use crate::markdown;
use crate::minhash;
use crate::llm::LlmBackend;
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

#[derive(Deserialize)]
struct SegmentationJson {
	entries: Vec<SegmentedEntryJson>,
}

#[derive(Deserialize)]
struct SegmentedEntryJson {
	#[serde(default)]
	start_line: usize,
	#[serde(default)]
	end_line: usize,
	body_start_line: usize,
	body_end_line: usize,
	author: Option<String>,
	timestamp: Option<String>,
}

pub struct SegmentationOptions {
	pub doctype_prompt: Option<String>,
	pub cleanup_patterns: Vec<Regex>,
	pub merge_consecutive_same_author: bool,
}

impl Default for SegmentationOptions {
	fn default() -> Self {
		SegmentationOptions {
			doctype_prompt: None,
			cleanup_patterns: Vec::new(),
			merge_consecutive_same_author: false,
		}
	}
}

pub fn segment(
	client: &dyn LlmBackend,
	model: &str,
	source_title: &str,
	text: &str,
	options: &SegmentationOptions,
) -> Result<SegmentationResult> {
	let lines: Vec<&str> = text.lines().collect();
	let numbered: String = lines
		.iter()
		.enumerate()
		.map(|(index, line)| format!("{}: {}", index + 1, line))
		.collect::<Vec<_>>()
		.join("\n");

	let prompt = format!(
		"Window title: {}\n\nText (with line numbers):\n{}",
		source_title, numbered
	);

	let mut system_prompt = segmentation_system_prompt().to_string();
	if let Some(doctype_prompt) = &options.doctype_prompt {
		system_prompt.push_str("\n\nADDITIONAL RULES FOR THIS DOCUMENT TYPE:\n");
		system_prompt.push_str(doctype_prompt);
	}

	let response = client.generate(&prompt, model, Some(&system_prompt), Some("json"))?;

	let parsed: SegmentationJson = serde_json::from_str(&response)
		.or_else(|_| {
			let entries: Vec<SegmentedEntryJson> = serde_json::from_str(&response)?;
			Ok(SegmentationJson { entries })
		})
		.map_err(|error: serde_json::Error| {
			let preview: String = response.chars().take(300).collect();
			anyhow::anyhow!("failed to parse segmentation response: {}\nollama returned: {}", error, preview)
		})?;

	let mut entries: Vec<SegmentedEntry> = parsed
		.entries
		.into_iter()
		.filter_map(|entry| {
			let body = extract_body(&lines, entry.body_start_line, entry.body_end_line);
			let body = apply_cleanup(&body, &options.cleanup_patterns);
			let body = body.trim().to_string();
			if body.is_empty() {
				return None;
			}
			Some(SegmentedEntry {
				start_line: entry.body_start_line,
				end_line: entry.body_end_line,
				author: entry.author,
				timestamp: entry.timestamp,
				body,
				is_quote: false,
				heading_level: None,
				heading_title: None,
			})
		})
		.collect();

	if options.merge_consecutive_same_author {
		entries = merge_consecutive_same_author(entries);
	}

	Ok(SegmentationResult { entries })
}

fn extract_body(lines: &[&str], start_line: usize, end_line: usize) -> String {
	if start_line == 0 || end_line == 0 || start_line > end_line {
		return String::new();
	}
	let start_index = start_line.saturating_sub(1);
	let end_index = end_line.min(lines.len());
	if start_index >= lines.len() {
		return String::new();
	}
	lines[start_index..end_index].join("\n")
}

fn apply_cleanup(text: &str, patterns: &[Regex]) -> String {
	let mut result = text.to_string();
	for pattern in patterns {
		result = pattern.replace_all(&result, "").to_string();
	}
	result
}

fn merge_consecutive_same_author(entries: Vec<SegmentedEntry>) -> Vec<SegmentedEntry> {
	if entries.is_empty() {
		return entries;
	}
	let mut merged: Vec<SegmentedEntry> = Vec::new();
	for entry in entries {
		let should_merge = merged.last().map(|last| {
			match (&last.author, &entry.author) {
				(Some(a), Some(b)) => a == b,
				_ => false,
			}
		}).unwrap_or(false);

		if should_merge {
			let last = merged.last_mut().unwrap();
			last.end_line = entry.end_line;
			last.body.push_str("\n\n");
			last.body.push_str(&entry.body);
			if last.timestamp.is_none() {
				last.timestamp = entry.timestamp;
			}
		} else {
			merged.push(entry);
		}
	}
	merged
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

fn segmentation_system_prompt() -> &'static str {
	include_str!("prompts/segmentation.txt")
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
const dup_jaccard_threshold: f64 = 0.7;
const dup_window_days: i64 = 180;

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

pub fn ingest_file(
	connection: &rusqlite::Connection,
	backend: &dyn LlmBackend,
	model: &str,
	file_path: &Path,
	config: &config::Config,
	force: bool,
) -> Result<bool> {
	let canonical_path = file_path.canonicalize()
		.unwrap_or_else(|_| file_path.to_path_buf());
	let file_path = canonical_path.as_path();
	let file_path_str = file_path.to_string_lossy();

	if !force && storage::document_exists_by_path(connection, &file_path_str)? {
		return Ok(false);
	}

	let text = std::fs::read_to_string(file_path)
		.with_context(|| format!("reading {}", file_path.display()))?;

	let lines: Vec<&str> = text.lines().collect();
	if lines.is_empty() {
		return Ok(false);
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
			return Ok(false);
		}
	}

	let parser = doctype_match.as_ref()
		.map(|m| m.parser)
		.unwrap_or(Parser::Whole);

	let merge_strategy = doctype_match.as_ref()
		.map(|m| m.merge_strategy)
		.unwrap_or(MergeStrategy::None);

	let segmentation_options = doctype_match.as_ref()
		.map(|m| SegmentationOptions {
			doctype_prompt: m.prompt.clone(),
			cleanup_patterns: m.cleanup_patterns.clone(),
			merge_consecutive_same_author: m.merge_consecutive_same_author,
		})
		.unwrap_or_default();

	let preprocessor = doctype_match.as_ref()
		.and_then(|m| m.preprocessor.clone());

	let segmented = if let Some(ref script) = preprocessor {
		let result = run_preprocessor(script, file_path)?;
		result.entries
	} else {
		match parser {
			Parser::Markdown => markdown::parse_markdown_sections(&body),
			Parser::CopilotEmail => parse_copilot_email_summary(&body),
			Parser::Ollama => {
				let result = segment(backend, model, &source_title, &body, &segmentation_options)?;
				result.entries
			}
			Parser::Whisper => {
				eprintln!("  whisper parser not yet implemented, skipping");
				return Ok(false);
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
		return Ok(false);
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
					return Ok(false);
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
				return Ok(true);
			}
		}
	}

	let doc_hash = minhash::minhash_document(&segmented);

	let candidates = storage::find_dup_candidates(connection, &clip_date_str, dup_window_days)?;
	let mut superseded_doc: Option<(i64, String, f64)> = None;
	let mut best_sim: Option<(i64, String, f64)> = None;
	for candidate in &candidates {
		let sim = minhash::jaccard(&doc_hash, &candidate.document_minhash);
		if sim >= dup_jaccard_threshold {
			superseded_doc = Some((candidate.id, candidate.source_title.clone(), sim));
			break;
		}
		let dominated = best_sim.as_ref().map(|(_, _, s)| sim > *s).unwrap_or(true);
		if sim >= 0.4 && dominated {
			best_sim = Some((candidate.id, candidate.source_title.clone(), sim));
		}
	}
	if superseded_doc.is_none() {
		if let Some((id, ref title, sim)) = best_sim {
			eprintln!(
				"  near-match: doc {} \"{}\" (similarity: {:.2}, threshold: {:.2})",
				id, title, sim, dup_jaccard_threshold,
			);
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

	if let Some((old_id, ref old_title, sim)) = superseded_doc {
		storage::add_tag(&transaction, old_id, "superseded")?;
		eprintln!(
			"  tagged doc {} \"{}\" as superseded (similarity: {:.2})",
			old_id, old_title, sim,
		);
	}

	transaction.commit()?;

	eprintln!(
		"  {} entries, {} chunks from \"{}\"",
		segmented.len(),
		total_chunks,
		source_title,
	);
	Ok(true)
}

pub fn ingest_directory(
	connection: &rusqlite::Connection,
	backend: &dyn LlmBackend,
	model: &str,
	directory: &Path,
	config: &config::Config,
	force: bool,
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

	for path in &paths {
		match ingest_file(connection, backend, model, path, config, force) {
			Ok(true) => ingested += 1,
			Ok(false) => skipped += 1,
			Err(error) => eprintln!("error ingesting {}: {:#}", path.display(), error),
		}
	}
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

	#[test]
	fn merge_consecutive_same_author_combines() {
		let entries = vec![
			SegmentedEntry {
				start_line: 1, end_line: 2, body: "first".to_string(),
				author: Some("alice".to_string()), timestamp: None,
				is_quote: false, heading_level: None, heading_title: None,
			},
			SegmentedEntry {
				start_line: 3, end_line: 4, body: "second".to_string(),
				author: Some("alice".to_string()), timestamp: None,
				is_quote: false, heading_level: None, heading_title: None,
			},
			SegmentedEntry {
				start_line: 5, end_line: 6, body: "third".to_string(),
				author: Some("bob".to_string()), timestamp: None,
				is_quote: false, heading_level: None, heading_title: None,
			},
		];
		let merged = merge_consecutive_same_author(entries);
		assert_eq!(merged.len(), 2);
		assert!(merged[0].body.contains("first"));
		assert!(merged[0].body.contains("second"));
		assert_eq!(merged[1].author.as_deref(), Some("bob"));
	}
}
