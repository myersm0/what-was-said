use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
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

#[derive(Parser)]
#[command(name = "cathedrals", about = "Personal knowledge base with full-text and semantic search")]
struct Cli {
	#[arg(long, value_name = "PATH", help = "Database path")]
	db: Option<PathBuf>,

	#[arg(long, value_name = "PATH", help = "Config file path")]
	config: Option<PathBuf>,

	#[arg(long, default_value = "http://localhost:11434", help = "Ollama endpoint")]
	ollama: String,

	#[arg(long, default_value = "mistral-nemo", help = "Ollama model for segmentation")]
	model: String,

	#[arg(long, default_value = "qwen3-embedding:8b", help = "Ollama model for embeddings")]
	embed_model: String,

	#[arg(long, help = "Output as JSON")]
	json: bool,

	#[command(subcommand)]
	command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
	/// Interactive TUI
	Browse {
		#[arg(long, value_delimiter = ',', help = "Only show docs matching these tags")]
		tags: Option<Vec<String>>,

		#[arg(long, value_delimiter = ',', help = "Exclude docs matching these tags")]
		exclude: Vec<String>,

		#[arg(long, help = "Ignore default tag exclusions")]
		include_all: bool,
	},

	/// Ingest new files from directory
	Ingest {
		directory: PathBuf,

		#[arg(long, help = "Re-ingest files even if already in database")]
		force: bool,
	},

	/// Keyword search chunks
	Search {
		query: Vec<String>,
	},

	/// Semantic search using embeddings
	Similar {
		query: Vec<String>,
	},

	/// Get document by id
	Get {
		id: i64,
	},

	/// Dump documents
	Dump {
		filter: Vec<String>,
	},

	/// Show database statistics
	Stats,

	/// Compute embeddings for chunks
	Embed {
		#[arg(long, help = "Limit number of chunks to embed")]
		limit: Option<usize>,
	},

	/// Generate LLM summaries
	Derive {
		#[arg(long, help = "Generate for docs without summaries (default)")]
		missing: bool,

		#[arg(long, help = "Regenerate where source content changed")]
		stale: bool,

		#[arg(long, help = "Regenerate detailed summaries marked bad")]
		bad_detailed: bool,

		#[arg(long, help = "Regenerate brief summaries marked bad")]
		bad_brief: bool,

		#[arg(long, help = "Regenerate all summaries")]
		force: bool,

		#[arg(long, help = "Show derivation status only")]
		status: bool,

		#[arg(long, help = "Limit number of documents to derive")]
		limit: Option<usize>,
	},
}

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

fn main() -> Result<()> {
	let cli = Cli::parse();

	let db_path = cli.db.unwrap_or_else(default_db_path);
	let config = config::load_or_default(cli.config.as_deref())?;
	let connection = open_db(&db_path)?;
	let json_output = cli.json;

	match cli.command {
		Some(Command::Ingest { directory, force }) => {
			let ollama = OllamaClient::new(&cli.ollama, &cli.model);
			let (ingested, skipped) = ingest::ingest_directory(
				&connection, &ollama, &directory, &config, force,
			)?;
			if skipped > 0 {
				eprintln!("ingested {} files, skipped {} (already in db)", ingested, skipped);
			} else {
				eprintln!("ingested {} files", ingested);
			}
		}
		Some(Command::Search { query }) => {
			let query = query.join(" ");
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
		Some(Command::Stats) => {
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
		Some(Command::Dump { filter }) => {
			let query = filter.join(" ");
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
		Some(Command::Embed { limit }) => {
			let ollama = OllamaClient::new(&cli.ollama, &cli.model);
			let pending = storage::count_chunks_without_embeddings(&connection)?;
			let existing = storage::count_chunks_with_embeddings(&connection)?;
			println!("embeddings: {} existing, {} pending", existing, pending);

			if pending == 0 {
				println!("all chunks have embeddings");
				return Ok(());
			}

			let chunks = storage::get_chunks_without_embeddings(&connection, limit)?;
			let total = chunks.len();
			println!("computing embeddings for {} chunks using {}...", total, cli.embed_model);

			for (i, chunk) in chunks.iter().enumerate() {
				let embedding = ollama.embed(&chunk.body, &cli.embed_model)?;
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
		Some(Command::Similar { query }) => {
			let query = query.join(" ");
			if query.is_empty() {
				anyhow::bail!("similar requires a query");
			}

			if !storage::vec_table_exists(&connection) {
				anyhow::bail!("no embeddings yet - run 'cathedrals embed' first");
			}

			let ollama = OllamaClient::new(&cli.ollama, &cli.model);
			let query_embedding = ollama.embed(&query, &cli.embed_model)?;
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
		Some(Command::Derive { missing, stale, bad_detailed, bad_brief, force, status, limit }) => {
			let derive_config = config::DeriveConfig::load(
				cli.config.as_ref().map(|p| p.as_path()).unwrap_or(Path::new(""))
			)?;

			if status {
				if json_output {
					let derive_status = storage::get_derive_status(&connection)?;
					println!("{}", serde_json::to_string_pretty(&derive_status)?);
				} else {
					derive::run_status(&connection)?;
				}
				return Ok(());
			}

			let ollama = OllamaClient::new(&cli.ollama, &cli.model);
			derive::run(&connection, &ollama, &derive_config, &DeriveOptions {
				force,
				missing,
				stale,
				bad_detailed,
				bad_brief,
				limit,
			})?;
		}
		Some(Command::Get { id }) => {
			let doc = storage::get_document(&connection, id)?
				.context(format!("no document with id {}", id))?;
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
		Some(Command::Browse { tags, exclude, include_all }) => {
			let filter = tui::GlobalFilter {
				include: tags,
				exclude,
				include_all,
			};
			let search_config = tui::SearchConfig {
				ollama_url: cli.ollama,
				embed_model: cli.embed_model,
			};
			tui::run(&connection, filter, search_config)?;
		}
		None => {
			let filter = tui::GlobalFilter {
				include: None,
				exclude: Vec::new(),
				include_all: false,
			};
			let search_config = tui::SearchConfig {
				ollama_url: cli.ollama,
				embed_model: cli.embed_model,
			};
			tui::run(&connection, filter, search_config)?;
		}
	}

	Ok(())
}
