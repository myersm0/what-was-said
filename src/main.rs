use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::Once;

use what_was_said::config::{self, BackendConfig, BackendKind};
use what_was_said::derive::{self, DeriveOptions};
use what_was_said::extract::{self, ExtractOptions};
use what_was_said::ingest;
use what_was_said::llm::LlmBackend;
use what_was_said::ollama::OllamaClient;
use what_was_said::openai::OpenAiClient;
use what_was_said::storage::{self, SearchSortColumn};
use what_was_said::serve;
use what_was_said::tui;
use what_was_said::util;

static VEC_INIT: Once = Once::new();

#[derive(Parser)]
#[command(name = "what-was-said", about = "Personal knowledge base with full-text and semantic search")]
struct Cli {
	#[arg(long, global = true, value_name = "PATH", help = "Database path")]
	db: Option<PathBuf>,

	#[arg(long, global = true, value_name = "PATH", help = "Config directory path")]
	config: Option<PathBuf>,

	#[arg(long, global = true, help = "LLM backend (ollama or openai)")]
	backend: Option<String>,

	#[arg(long, global = true, help = "Ollama endpoint")]
	ollama: Option<String>,

	#[arg(long, global = true, help = "Model for generation")]
	model: Option<String>,

	#[arg(long, global = true, help = "Model for embeddings")]
	embed_model: Option<String>,

	#[arg(long, global = true, help = "Output as JSON")]
	json: bool,

	#[arg(long, global = true, help = "TUI theme (dracula, gruvbox, nord, solarized, light, or path)")]
	theme: Option<String>,

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

	/// Start JSON API server
	Serve {
		#[arg(long, default_value = "3030", help = "Port to listen on")]
		port: u16,
	},

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

	/// Extract claims from documents
	Extract {
		#[arg(long, help = "Re-extract all documents")]
		force: bool,

		#[arg(long, help = "Show extraction status only")]
		status: bool,

		#[arg(long, help = "Limit number of documents to extract")]
		limit: Option<usize>,
	},
}

fn default_db_path() -> PathBuf {
	dirs::data_dir()
		.unwrap_or_else(|| PathBuf::from("."))
		.join("what-was-said")
		.join("what-was-said.db")
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

fn create_backend(backend_config: &BackendConfig) -> Result<Box<dyn LlmBackend>> {
	match backend_config.backend {
		BackendKind::Ollama => Ok(Box::new(OllamaClient::new(&backend_config.ollama_url))),
		BackendKind::OpenAi => Ok(Box::new(OpenAiClient::from_config(&backend_config.openai)?)),
	}
}

fn default_config_dir() -> PathBuf {
	dirs::config_dir()
		.unwrap_or_else(|| PathBuf::from("."))
		.join("what-was-said")
}

fn main() -> Result<()> {
	let cli = Cli::parse();

	let config_dir = cli.config.clone().unwrap_or_else(default_config_dir);
	let db_path = cli.db.unwrap_or_else(default_db_path);
	let config_file = config_dir.join("config.toml");
	let config = config::load_or_default(
		Some(config_file.as_path()).filter(|p| p.exists()),
	)?;
	let mut backend_config = BackendConfig::load(&config_dir)?;
	let connection = open_db(&db_path)?;
	let json_output = cli.json;

	if let Some(b) = &cli.backend {
		backend_config.backend = match b.as_str() {
			"ollama" => BackendKind::Ollama,
			"openai" => BackendKind::OpenAi,
			other => anyhow::bail!("unknown backend: {}", other),
		};
	}
	if let Some(url) = cli.ollama {
		backend_config.ollama_url = url;
	}
	if let Some(m) = cli.model {
		backend_config.model = Some(m);
	}
	if let Some(m) = cli.embed_model {
		backend_config.embed_model = Some(m);
	}

	let model = backend_config.model.clone()
		.unwrap_or_else(|| "mistral-nemo".to_string());
	let embed_model = backend_config.embed_model.clone()
		.unwrap_or_else(|| "qwen3-embedding:8b".to_string());

	match cli.command {
		Some(Command::Ingest { directory, force }) => {
			let backend = create_backend(&backend_config)?;
			let (ingested, skipped) = ingest::ingest_directory(
				&connection, backend.as_ref(), &model, &directory, &config, force,
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
			let claims = storage::claim_count(&connection)?;
			let claim_embeddings = storage::count_claims_with_embeddings(&connection)?;
			if json_output {
				println!("{}", serde_json::to_string_pretty(&serde_json::json!({
					"database": db_path.display().to_string(),
					"documents": documents,
					"entries": entries,
					"chunks": chunks,
					"embeddings": embeddings,
					"embeddings_total": chunks,
					"claims": claims,
					"claim_embeddings": claim_embeddings,
				}))?);
			} else {
				println!("database: {}", db_path.display());
				println!("documents: {}", documents);
				println!("entries: {}", entries);
				println!("chunks: {}", chunks);
				println!("embeddings: {}/{}", embeddings, chunks);
				println!("claims: {} (embeddings: {})", claims, claim_embeddings);
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
			let backend = create_backend(&backend_config)?;

			let chunk_pending = storage::count_chunks_without_embeddings(&connection)?;
			let chunk_existing = storage::count_chunks_with_embeddings(&connection)?;
			println!("chunk embeddings: {} existing, {} pending", chunk_existing, chunk_pending);

			if chunk_pending > 0 {
				let chunks = storage::get_chunks_without_embeddings(&connection, limit)?;
				let total = chunks.len();
				println!("computing embeddings for {} chunks using {}...", total, embed_model);

				for (i, chunk) in chunks.iter().enumerate() {
					let embedding = backend.embed(&chunk.body, &embed_model)?;
					if i == 0 {
						storage::ensure_vec_table(&connection, embedding.len())?;
					}
					storage::insert_embedding(&connection, chunk.id, &embedding)?;
					if (i + 1) % 10 == 0 || i + 1 == total {
						eprint!("\r  {}/{}", i + 1, total);
					}
				}
				eprintln!();
			}

			let claim_pending = storage::count_claims_without_embeddings(&connection)?;
			let claim_existing = storage::count_claims_with_embeddings(&connection)?;
			println!("claim embeddings: {} existing, {} pending", claim_existing, claim_pending);

			if claim_pending > 0 {
				let claims = storage::get_claims_without_embeddings(&connection, limit)?;
				let total = claims.len();
				println!("computing embeddings for {} claims using {}...", total, embed_model);

				for (i, claim) in claims.iter().enumerate() {
					let embedding = backend.embed(&claim.content, &embed_model)?;
					if i == 0 {
						storage::ensure_vec_claims_table(&connection, embedding.len())?;
					}
					storage::insert_claim_embedding(&connection, claim.id, &embedding)?;
					if (i + 1) % 10 == 0 || i + 1 == total {
						eprint!("\r  {}/{}", i + 1, total);
					}
				}
				eprintln!();
			}

			if chunk_pending == 0 && claim_pending == 0 {
				println!("all chunks and claims have embeddings");
			} else {
				println!("done");
			}
		}
		Some(Command::Similar { query }) => {
			let query = query.join(" ");
			if query.is_empty() {
				anyhow::bail!("similar requires a query");
			}

			if !storage::vec_table_exists(&connection) {
				anyhow::bail!("no embeddings yet - run 'what-was-said embed' first");
			}

			let backend = create_backend(&backend_config)?;
			let query_embedding = backend.embed(&query, &embed_model)?;
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
			let derive_config = config::DeriveConfig::load(&config_dir)?;

			if status {
				if json_output {
					let derive_status = storage::get_derive_status(&connection)?;
					println!("{}", serde_json::to_string_pretty(&derive_status)?);
				} else {
					derive::run_status(&connection)?;
				}
				return Ok(());
			}

			let backend = create_backend(&backend_config)?;
			derive::run(&connection, backend.as_ref(), &derive_config, &DeriveOptions {
				force,
				missing,
				stale,
				bad_detailed,
				bad_brief,
				limit,
			})?;
		}
		Some(Command::Extract { force, status, limit }) => {
			let extract_config = config::ExtractConfig::load(&config_dir)?;

			if status {
				if json_output {
					let total_docs = storage::document_count(&connection)?;
					let docs_with = storage::documents_with_claims_count(&connection)?;
					let total_claims = storage::claim_count(&connection)?;
					println!("{}", serde_json::to_string_pretty(&serde_json::json!({
						"total_docs": total_docs,
						"docs_with_claims": docs_with,
						"missing": total_docs - docs_with,
						"total_claims": total_claims,
					}))?);
				} else {
					extract::run_status(&connection)?;
				}
				return Ok(());
			}

			let backend = create_backend(&backend_config)?;
			extract::run(&connection, backend.as_ref(), &extract_config, &ExtractOptions {
				force,
				limit,
				status: false,
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
		Some(Command::Serve { port }) => {
			let backend = create_backend(&backend_config)?;
			serve::run(connection, backend, embed_model, port)?;
		}
		Some(Command::Browse { tags, exclude, include_all }) => {
			let backend = create_backend(&backend_config)?;
			let filter = tui::GlobalFilter {
				include: tags,
				exclude,
				include_all,
			};
			let search_config = tui::SearchConfig {
				embed_model: embed_model.clone(),
				backend: backend.as_ref(),
			};
			tui::run(&connection, filter, search_config, cli.theme.as_deref())?;
		}
		None => {
			let backend = create_backend(&backend_config)?;
			let filter = tui::GlobalFilter {
				include: None,
				exclude: Vec::new(),
				include_all: false,
			};
			let search_config = tui::SearchConfig {
				embed_model: embed_model.clone(),
				backend: backend.as_ref(),
			};
			tui::run(&connection, filter, search_config, cli.theme.as_deref())?;
		}
	}

	Ok(())
}
