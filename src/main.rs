use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use cathedrals::chunking;
use cathedrals::config::{self, Parser};
use cathedrals::ingest::{self, OllamaClient, SegmentationOptions, run_preprocessor};
use cathedrals::markdown;
use cathedrals::minhash;
use cathedrals::storage::{self, SearchSortColumn};
use cathedrals::tui;
use cathedrals::types::*;

fn default_db_path() -> PathBuf {
	dirs::data_dir()
		.unwrap_or_else(|| PathBuf::from("."))
		.join("cathedrals")
		.join("cathedrals.db")
}

fn open_db(path: &Path) -> Result<rusqlite::Connection> {
	if let Some(parent) = path.parent() {
		std::fs::create_dir_all(parent)?;
	}
	let connection = rusqlite::Connection::open(path)?;
	storage::initialize(&connection)?;
	Ok(connection)
}

fn normalize_to_ascii(s: &str) -> String {
	s.chars()
		.map(|c| {
			if c.is_ascii() {
				c
			} else {
				match c {
					'\u{2014}' | '\u{2013}' => '-',  // em-dash, en-dash
					'\u{2018}' | '\u{2019}' => '\'', // curly single quotes
					'\u{201C}' | '\u{201D}' => '"',  // curly double quotes
					'\u{2026}' => '.',               // ellipsis
					_ => ' ',
				}
			}
		})
		.collect::<String>()
		.split_whitespace()
		.collect::<Vec<_>>()
		.join(" ")
}

fn extract_merge_key(source_title: &str) -> String {
	let browsers = [" - Brave", " - Chrome", " - Firefox", " - Safari", " - Edge", " - Arc"];
	let mut s = source_title.to_string();

	for browser in &browsers {
		if let Some(pos) = s.rfind(browser) {
			s = s[..pos].to_string();
			break;
		}
	}

	if let Some(url_start) = s.rfind(" - https://") {
		s = s[..url_start].to_string();
	}
	if let Some(url_start) = s.rfind(" - http://") {
		s = s[..url_start].to_string();
	}

	s.trim().to_string()
}

const MERGE_MIN_CHARS: usize = 150;

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

	if best_run_chars >= MERGE_MIN_CHARS {
		best_run_start.map(|start| (start, best_run_len))
	} else {
		None
	}
}

fn ingest_file(
	connection: &rusqlite::Connection,
	ollama: &OllamaClient,
	file_path: &Path,
	config: &config::Config,
	force: bool,
) -> Result<bool> {
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

	let source_title = ingest::parse_source_header(lines[0])
		.unwrap_or_else(|| file_path.display().to_string());

	let body = if ingest::parse_source_header(lines[0]).is_some() {
		lines[1..].join("\n")
	} else {
		text.clone()
	};

	let clip_date = file_path
		.file_name()
		.and_then(|name| name.to_str())
		.and_then(ingest::parse_clip_date)
		.unwrap_or_else(|| chrono::Local::now().naive_local());
	let clip_date_str = clip_date.format("%Y-%m-%d %H:%M:%S").to_string();

	let file_extension = file_path
		.extension()
		.and_then(|ext| ext.to_str());

	let doctype_match = config.detect_with_content(&source_title, file_extension, &body);

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
			Parser::CopilotEmail => ingest::parse_copilot_email_summary(&body),
			Parser::Ollama => {
				let result = ollama.segment(&source_title, &body, &segmentation_options)?;
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
		.map(|t| normalize_to_ascii(&t));

	let source_title = normalize_to_ascii(&source_title);
	let merge_key = extract_merge_key(&source_title);

	let doctype_name = doctype_match.as_ref().map(|m| m.name.as_str());

	if merge_strategy == MergeStrategy::Positional {
		let existing_docs = storage::find_documents_by_merge_key(
			connection,
			extract_merge_key,
			&merge_key,
			"positional",
		)?;

		if let Some(&existing_doc_id) = existing_docs.first() {
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

	let transaction = connection.unchecked_transaction()?;

	let document_id = storage::insert_document(
		&transaction,
		title.as_deref(),
		&source_title,
		doctype_name,
		merge_strategy,
		Some(&file_path_str),
		&clip_date_str,
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

	transaction.commit()?;

	eprintln!(
		"  {} entries, {} chunks from \"{}\"",
		segmented.len(),
		total_chunks,
		source_title,
	);
	Ok(true)
}

fn ingest_directory(
	connection: &rusqlite::Connection,
	ollama: &OllamaClient,
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
		match ingest_file(connection, ollama, path, config, force) {
			Ok(true) => {
				ingested += 1;
			}
			Ok(false) => {
				skipped += 1;
			}
			Err(error) => eprintln!("error ingesting {}: {:#}", path.display(), error),
		}
	}
	Ok((ingested, skipped))
}

fn print_usage() {
	eprintln!(
		"usage:
  cathedrals browse                    interactive TUI (default)
  cathedrals ingest <directory>        ingest new files from directory
  cathedrals embed                     compute embeddings for chunks
  cathedrals similar <query>           semantic search using embeddings
  cathedrals search <query>            keyword search chunks
  cathedrals dump [filter]             dump documents
  cathedrals stats                     show database statistics

options:
  --db <path>          database path (default: $XDG_DATA_HOME/cathedrals/cathedrals.db)
  --config <path>      config file (default: $XDG_CONFIG_HOME/cathedrals/config.toml)
  --ollama <url>       ollama endpoint (default: http://localhost:11434)
  --model <n>          ollama model for segmentation (default: mistral-nemo)
  --embed-model <n>    ollama model for embeddings (default: nomic-embed-text)
  --force              re-ingest files even if already in database
  --limit <n>          limit number of chunks to embed

tag filtering (browse mode):
  --tags <t1,t2,...>   only show docs matching these tags
  --exclude <t1,...>   exclude docs matching these tags (overrides config default)
  --include-all        ignore default exclusions, show everything"
	);
}

fn main() -> Result<()> {
	let args: Vec<String> = std::env::args().collect();

	let mut db_path: Option<PathBuf> = None;
	let mut config_path: Option<PathBuf> = None;
	let mut ollama_url = "http://localhost:11434".to_string();
	let mut model_name = "mistral-nemo".to_string();
	let mut embed_model = "nomic-embed-text".to_string();
	let mut force = false;
	let mut tags_include: Option<Vec<String>> = None;
	let mut tags_exclude: Vec<String> = Vec::new();
	let mut include_all = false;
	let mut limit: Option<usize> = None;

	let mut positional = Vec::new();
	let mut index = 1;
	while index < args.len() {
		match args[index].as_str() {
			"--db" => {
				index += 1;
				db_path = Some(PathBuf::from(&args[index]));
			}
			"--config" => {
				index += 1;
				config_path = Some(PathBuf::from(&args[index]));
			}
			"--ollama" => {
				index += 1;
				ollama_url = args[index].clone();
			}
			"--model" => {
				index += 1;
				model_name = args[index].clone();
			}
			"--embed-model" => {
				index += 1;
				embed_model = args[index].clone();
			}
			"--force" => {
				force = true;
			}
			"--limit" => {
				index += 1;
				limit = Some(args[index].parse().context("--limit requires a number")?);
			}
			"--tags" => {
				index += 1;
				tags_include = Some(
					args[index].split(',').map(|s| s.trim().to_lowercase()).collect()
				);
			}
			"--exclude" => {
				index += 1;
				tags_exclude = args[index].split(',').map(|s| s.trim().to_lowercase()).collect();
			}
			"--include-all" => {
				include_all = true;
			}
			"--help" | "-h" => {
				print_usage();
				return Ok(());
			}
			other => positional.push(other.to_string()),
		}
		index += 1;
	}

	let db_path = db_path.unwrap_or_else(default_db_path);
	let config = config::load_or_default(config_path.as_deref())?;
	let connection = open_db(&db_path)?;

	match positional.first().map(|s| s.as_str()) {
		Some("ingest") => {
			let directory = positional
				.get(1)
				.context("ingest requires a directory argument")?;
			let ollama = OllamaClient::new(&ollama_url, &model_name);
			let (ingested, skipped) = ingest_directory(
				&connection,
				&ollama,
				Path::new(directory),
				&config,
				force,
			)?;
			if skipped > 0 {
				eprintln!("ingested {} files, skipped {} (already in db)", ingested, skipped);
			} else {
				eprintln!("ingested {} files", ingested);
			}
		}
		Some("search") => {
			let query = positional[1..].join(" ");
			if query.is_empty() {
				anyhow::bail!("search requires a query");
			}
			let results = storage::search(&connection, &query, SearchSortColumn::Score)?;
			if results.is_empty() {
				println!("no results");
			} else {
				for doc in &results {
					println!("=== [{}] {} ===", doc.source_title, doc.clip_date);
					for chunk in &doc.chunks {
						if let Some(heading) = &chunk.heading_title {
							print!("  ## {} ", heading);
						} else {
							print!("  ");
						}
						if let Some(author) = &chunk.author {
							print!("[{}] ", author);
						}
						println!();
						for line in chunk.chunk_body.lines() {
							println!("    {}", line);
						}
						println!();
					}
				}
			}
		}
		Some("stats") => {
			let documents = storage::document_count(&connection)?;
			let entries = storage::entry_count(&connection)?;
			let chunks = storage::chunk_count(&connection)?;
			let embeddings = storage::count_chunks_with_embeddings(&connection)?;
			println!("database: {}", db_path.display());
			println!("documents: {}", documents);
			println!("entries: {}", entries);
			println!("chunks: {}", chunks);
			println!("embeddings: {}/{}", embeddings, chunks);
		}
		Some("dump") => {
			let query = positional[1..].join(" ");
			let results = storage::dump_document(&connection, if query.is_empty() { None } else { Some(&query) })?;
			for doc in &results {
				println!("=== [{}] {} (id={}) ===", doc.merge_strategy, doc.source_title, doc.document_id);
				for entry in &doc.entries {
					let author_str = entry.author.as_deref().unwrap_or("");
					let heading_str = entry.heading_title.as_deref().unwrap_or("");
					if !author_str.is_empty() || !heading_str.is_empty() {
						print!("  --- ");
						if !author_str.is_empty() {
							print!("{}", author_str);
						}
						if !heading_str.is_empty() {
							if !author_str.is_empty() {
								print!(" | ");
							}
							print!("{}", heading_str);
						}
						println!(" ---");
					}
					for line in entry.body.lines() {
						println!("  {}", line);
					}
					println!();
				}
			}
		}
		Some("embed") => {
			let ollama = OllamaClient::new(&ollama_url, &model_name);
			let pending = storage::count_chunks_without_embeddings(&connection)?;
			let existing = storage::count_chunks_with_embeddings(&connection)?;
			println!("embeddings: {} existing, {} pending", existing, pending);

			if pending == 0 {
				println!("all chunks have embeddings");
				return Ok(());
			}

			let chunks = storage::get_chunks_without_embeddings(&connection, limit)?;
			let total = chunks.len();
			println!("computing embeddings for {} chunks using {}...", total, embed_model);

			for (i, chunk) in chunks.iter().enumerate() {
				let embedding = ollama.embed(&chunk.body, &embed_model)?;
				storage::insert_embedding(&connection, chunk.id, &embedding)?;
				if (i + 1) % 10 == 0 || i + 1 == total {
					eprint!("\r  {}/{}", i + 1, total);
				}
			}
			eprintln!();
			println!("done");
		}
		Some("similar") => {
			let query = positional[1..].join(" ");
			if query.is_empty() {
				anyhow::bail!("similar requires a query");
			}

			let existing = storage::count_chunks_with_embeddings(&connection)?;
			if existing == 0 {
				anyhow::bail!("no embeddings yet - run 'cathedrals embed' first");
			}

			let ollama = OllamaClient::new(&ollama_url, &model_name);
			let query_embedding = ollama.embed(&query, &embed_model)?;
			let results = storage::find_similar_chunks(&connection, &query_embedding, 10)?;

			if results.is_empty() {
				println!("no results");
			} else {
				for result in &results {
					println!("--- [{:.3}] {} | {} ---", result.similarity, result.source_title, &result.clip_date[..10.min(result.clip_date.len())]);
					for line in result.body.lines().take(5) {
						println!("  {}", line);
					}
					if result.body.lines().count() > 5 {
						println!("  ...");
					}
					println!();
				}
			}
		}
		Some("browse") | None => {
			let filter = tui::GlobalFilter {
				include: tags_include,
				exclude: tags_exclude,
				include_all,
			};
			tui::run(&connection, filter)?;
		}
		_ => {
			print_usage();
			std::process::exit(1);
		}
	}

	Ok(())
}
