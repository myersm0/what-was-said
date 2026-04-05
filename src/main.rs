use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Once;

use cathedrals::config;
use cathedrals::derive::{self, DeriveOptions};
use cathedrals::ingest;
use cathedrals::ollama::OllamaClient;
use cathedrals::storage::{self, SearchSortColumn};
use cathedrals::tui;
use cathedrals::util;

static VEC_INIT: Once = Once::new();

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
	VEC_INIT.call_once(|| unsafe {
		rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
			sqlite_vec::sqlite3_vec_init as *const (),
		)));
	});
	let connection = rusqlite::Connection::open(path)?;
	storage::initialize(&connection)?;
	Ok(connection)
}

fn print_usage() {
	eprintln!(
		"usage:
  cathedrals browse                    interactive TUI (default)
  cathedrals ingest <directory>        ingest new files from directory
  cathedrals embed                     compute embeddings for chunks
  cathedrals derive [options]          generate LLM summaries
  cathedrals similar <query>           semantic search using embeddings
  cathedrals search <query>            keyword search chunks
  cathedrals get <id>                  get document by id
  cathedrals dump [filter]             dump documents
  cathedrals stats                     show database statistics

derive options:
  --missing              generate for docs without summaries (default)
  --stale                regenerate where source content changed
  --bad-detailed         regenerate detailed summaries marked bad
  --bad-brief            regenerate brief summaries marked bad
  --force                regenerate all summaries
  --status               show derivation status only

options:
  --db <path>          database path (default: $XDG_DATA_HOME/cathedrals/cathedrals.db)
  --config <path>      config file (default: $XDG_CONFIG_HOME/cathedrals/config.toml)
  --ollama <url>       ollama endpoint (default: http://localhost:11434)
  --model <n>          ollama model for segmentation (default: mistral-nemo)
  --embed-model <n>    ollama model for embeddings (default: qwen3-embedding:8b)
  --force              re-ingest files even if already in database
  --limit <n>          limit number of items to process
  --json               output as JSON (for search, similar, get, dump, stats, derive --status)

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
	let mut embed_model = "qwen3-embedding:8b".to_string();
	let mut force = false;
	let mut json_output = false;
	let mut tags_include: Option<Vec<String>> = None;
	let mut tags_exclude: Vec<String> = Vec::new();
	let mut include_all = false;
	let mut limit: Option<usize> = None;
	let mut derive_missing = false;
	let mut derive_stale = false;
	let mut derive_bad_detailed = false;
	let mut derive_bad_brief = false;
	let mut derive_status_only = false;

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
			"--force" => force = true,
			"--json" => json_output = true,
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
			"--include-all" => include_all = true,
			"--missing" => derive_missing = true,
			"--stale" => derive_stale = true,
			"--bad-detailed" => derive_bad_detailed = true,
			"--bad-brief" => derive_bad_brief = true,
			"--status" => derive_status_only = true,
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
			let (ingested, skipped) = ingest::ingest_directory(
				&connection, &ollama, Path::new(directory), &config, force,
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
			if json_output {
				let mut results = results;
				for doc in &mut results {
					for chunk in &mut doc.chunks {
						chunk.snippet = util::strip_fts_markers(&chunk.snippet);
					}
				}
				println!("{}", serde_json::to_string_pretty(&results)?);
			} else if results.is_empty() {
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
			if json_output {
				println!("{}", serde_json::to_string_pretty(&serde_json::json!({
					"database": db_path.display().to_string(),
					"documents": documents,
					"entries": entries,
					"chunks": chunks,
					"embeddings": embeddings,
					"embeddings_total": chunks,
				}))?);
			} else {
				println!("database: {}", db_path.display());
				println!("documents: {}", documents);
				println!("entries: {}", entries);
				println!("chunks: {}", chunks);
				println!("embeddings: {}/{}", embeddings, chunks);
			}
		}
		Some("dump") => {
			let query = positional[1..].join(" ");
			let filter = if query.is_empty() { None } else { Some(query.as_str()) };
			let results = storage::dump_document(&connection, filter)?;
			if json_output {
				println!("{}", serde_json::to_string_pretty(&results)?);
			} else {
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
				if i == 0 {
					storage::ensure_vec_table(&connection, embedding.len())?;
				}
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

			if !storage::vec_table_exists(&connection) {
				anyhow::bail!("no embeddings yet - run 'cathedrals embed' first");
			}

			let ollama = OllamaClient::new(&ollama_url, &model_name);
			let query_embedding = ollama.embed(&query, &embed_model)?;
			let results = storage::find_similar_chunks(&connection, &query_embedding, 10)?;

			if json_output {
				println!("{}", serde_json::to_string_pretty(&results)?);
			} else if results.is_empty() {
				println!("no results");
			} else {
				for result in &results {
					println!(
						"--- [{:.3}] {} | {} ---",
						result.similarity,
						result.source_title,
						util::truncate_str(&result.clip_date, 10),
					);
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
		Some("derive") => {
			let derive_config = config::DeriveConfig::load(
				config_path.as_ref().map(|p| p.as_path()).unwrap_or(Path::new(""))
			)?;

			if derive_status_only {
				if json_output {
					let status = storage::get_derive_status(&connection)?;
					println!("{}", serde_json::to_string_pretty(&status)?);
				} else {
					derive::run_status(&connection)?;
				}
				return Ok(());
			}

			let ollama = OllamaClient::new(&ollama_url, &model_name);
			derive::run(&connection, &ollama, &derive_config, &DeriveOptions {
				force,
				missing: derive_missing,
				stale: derive_stale,
				bad_detailed: derive_bad_detailed,
				bad_brief: derive_bad_brief,
				limit,
			})?;
		}
		Some("get") => {
			let id_str = positional
				.get(1)
				.context("get requires a document id")?;
			let document_id: i64 = id_str.parse().context("get requires a numeric document id")?;
			let doc = storage::get_document(&connection, document_id)?
				.context(format!("no document with id {}", document_id))?;
			if json_output {
				println!("{}", serde_json::to_string_pretty(&doc)?);
			} else {
				println!("=== {} ===", doc.source_title);
				println!("id: {}  doctype: {}  date: {}",
					doc.id,
					doc.doctype_name.as_deref().unwrap_or("unknown"),
					doc.clip_date,
				);
				println!();
				for entry in &doc.entries {
					if let Some(heading) = &entry.heading_title {
						println!("## {}", heading);
					}
					if let Some(author) = &entry.author {
						print!("[{}]", author);
						if let Some(ts) = &entry.timestamp {
							print!(" {}", ts);
						}
						println!();
					}
					for line in entry.body.lines() {
						println!("  {}", line);
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
			let search_config = tui::SearchConfig {
				ollama_url,
				embed_model,
			};
			tui::run(&connection, filter, search_config)?;
		}
		_ => {
			print_usage();
			std::process::exit(1);
		}
	}

	Ok(())
}
