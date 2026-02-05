use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use cathedrals::config::{self, Parser};
use cathedrals::ingest::{self, OllamaClient};
use cathedrals::markdown;
use cathedrals::minhash;
use cathedrals::storage;
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

fn ingest_file(
	connection: &rusqlite::Connection,
	ollama: &OllamaClient,
	file_path: &Path,
	config: &config::Config,
) -> Result<()> {
	let text = std::fs::read_to_string(file_path)
		.with_context(|| format!("reading {}", file_path.display()))?;

	let lines: Vec<&str> = text.lines().collect();
	if lines.is_empty() {
		return Ok(());
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

	let doctype_match = config.detect(&source_title, file_extension);

	let parser = doctype_match.as_ref()
		.map(|m| m.parser)
		.unwrap_or(Parser::Whole);

	let merge_strategy = doctype_match.as_ref()
		.map(|m| m.merge_strategy)
		.unwrap_or(MergeStrategy::None);

	let segmented = match parser {
		Parser::Markdown => markdown::parse_markdown_sections(&body),
		Parser::Ollama => {
			let result = ollama.segment(&source_title, &body)?;
			result.entries
		}
		Parser::Whisper => {
			eprintln!("  whisper parser not yet implemented, skipping");
			return Ok(());
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
	};

	if segmented.is_empty() {
		eprintln!("  no entries found, skipping");
		return Ok(());
	}

	let file_path_str = file_path.to_string_lossy();

	let document_id = storage::insert_document(
		connection,
		&source_title,
		merge_strategy,
		Some(&file_path_str),
	)?;

	for (position, entry) in segmented.iter().enumerate() {
		let hash = minhash::minhash(&entry.body);
		storage::insert_entry(
			connection,
			document_id,
			entry,
			position as u32,
			&source_title,
			&clip_date_str,
			&file_path_str,
			&hash,
		)?;
	}

	eprintln!(
		"  {} entries from \"{}\"",
		segmented.len(),
		source_title,
	);
	Ok(())
}

fn ingest_directory(
	connection: &rusqlite::Connection,
	ollama: &OllamaClient,
	directory: &Path,
	config: &config::Config,
) -> Result<u32> {
	let mut count = 0u32;
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
		eprint!("ingesting {} ... ", path.display());
		match ingest_file(connection, ollama, path, config) {
			Ok(()) => count += 1,
			Err(error) => eprintln!("  error: {:#}", error),
		}
	}
	Ok(count)
}

fn print_usage() {
	eprintln!(
		"usage:
  cathedrals ingest <directory> [--model <name>]
  cathedrals search <query>
  cathedrals dump [source_title_filter]
  cathedrals stats

options:
  --db <path>          database path (default: $XDG_DATA_HOME/cathedrals/cathedrals.db)
  --config <path>      config file (default: $XDG_CONFIG_HOME/cathedrals/config.toml)
  --ollama <url>       ollama endpoint (default: http://localhost:11434)
  --model <name>       ollama model (default: mistral-nemo)"
	);
}

fn main() -> Result<()> {
	let args: Vec<String> = std::env::args().collect();
	if args.len() < 2 {
		print_usage();
		std::process::exit(1);
	}

	let mut db_path: Option<PathBuf> = None;
	let mut config_path: Option<PathBuf> = None;
	let mut ollama_url = "http://localhost:11434".to_string();
	let mut model_name = "mistral-nemo".to_string();

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
			let count = ingest_directory(
				&connection,
				&ollama,
				Path::new(directory),
				&config,
			)?;
			eprintln!("ingested {} files", count);
		}
		Some("search") => {
			let query = positional[1..].join(" ");
			if query.is_empty() {
				anyhow::bail!("search requires a query");
			}
			let results = storage::search(&connection, &query)?;
			if results.is_empty() {
				println!("no results");
			} else {
				for result in &results {
					println!(
						"--- [{}] {} | {} ---",
						result.source_title,
						result.author.as_deref().unwrap_or("unknown"),
						result.clip_date,
					);
					let preview: String = result
						.body
						.chars()
						.take(200)
						.collect();
					println!("{}", preview);
					println!();
				}
			}
		}
		Some("stats") => {
			let documents = storage::document_count(&connection)?;
			let entries = storage::entry_count(&connection)?;
			println!("database: {}", db_path.display());
			println!("documents: {}", documents);
			println!("entries: {}", entries);
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
		_ => {
			print_usage();
			std::process::exit(1);
		}
	}

	Ok(())
}
