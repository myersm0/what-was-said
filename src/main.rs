use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::sync::Once;

use what_was_said::config::{self, BackendConfig, BackendKind};
use what_was_said::derive::{self, DeriveOptions};
use what_was_said::embed;
use what_was_said::extract::{self, ExtractOptions};
use what_was_said::ingest;
use what_was_said::llm::LlmBackend;
use what_was_said::ollama::OllamaClient;
use what_was_said::openai::OpenAiClient;
use what_was_said::query::{self, SearchSortColumn};
use what_was_said::storage;
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

	/// Ingest new files from a file or directory
	Ingest {
		path: PathBuf,

		#[arg(long, help = "Re-ingest files even if already in database")]
		force: bool,

		#[arg(long, help = "Never prompt on gray-zone near-duplicates; keep both")]
		non_interactive: bool,
	},

	/// Search the collection
	About {
		query: Vec<String>,

		#[arg(long, value_enum, default_value = "semantic", help = "Search method")]
		method: SearchMethod,
	},

	/// Show document by id
	In {
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SearchMethod {
	Exact,
	Semantic,
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
		Some(Command::Ingest { path, force, non_interactive }) => {
			let options = ingest::IngestOptions { force, interactive: !non_interactive };
			if path.is_dir() {
				let (ingested, skipped) = ingest::ingest_directory(
					&connection, &path, &config, &options,
				)?;
				if skipped > 0 {
					eprintln!("ingested {} files, skipped {} (already in db)", ingested, skipped);
				} else {
					eprintln!("ingested {} files", ingested);
				}
			} else {
				let mut gray_zones = Vec::new();
				let outcome = ingest::ingest_file(&connection, &path, &config, &options, &mut gray_zones)?;
				match outcome {
					ingest::IngestOutcome::Ingested => eprintln!("ingested 1 file"),
					ingest::IngestOutcome::Skipped => eprintln!("skipped 1 file"),
					ingest::IngestOutcome::Quit => eprintln!("aborted"),
				}
				ingest::print_gray_zone_summary(&gray_zones);
			}
		}
		Some(Command::About { query, method }) => {
			let query = query.join(" ");
			if query.is_empty() {
				anyhow::bail!("about requires a query");
			}

			match method {
				SearchMethod::Exact => {
					let results = query::search(&connection, &query, SearchSortColumn::Score)?;
					if json_output {
						let mut results = results;
						query::strip_fts_markers(&mut results);
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
				SearchMethod::Semantic => {
					if !storage::vec_table_exists(&connection) {
						anyhow::bail!("no embeddings yet - run 'what-was-said embed' first");
					}

					let backend = create_backend(&backend_config)?;
					let query_embedding = backend.embed(&query, &embed_model)?;
					let results = query::find_similar_grouped(&connection, &query_embedding, 10)?;

					if json_output {
						println!("{}", serde_json::to_string_pretty(&results)?);
					} else if results.is_empty() {
						println!("no results");
					} else {
						for doc in &results {
							println!(
								"--- [{:.3}] {} | {} ---",
								-doc.best_rank,
								doc.source_title,
								util::truncate_str(&doc.clip_date, 10),
							);
							for chunk in &doc.chunks {
								for line in chunk.chunk_body.lines().take(5) {
									println!("  {}", line);
								}
								if chunk.chunk_body.lines().count() > 5 {
									println!("  ...");
								}
							}
							println!();
						}
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
			embed::run(&connection, backend.as_ref(), &embed_model, limit)?;
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
			let skip_doctypes = config.no_extract_doctypes();
			extract::run(&connection, backend.as_ref(), &extract_config, &ExtractOptions {
				force,
				limit,
				status: false,
			}, &skip_doctypes)?;
		}
		Some(Command::In { id }) => {
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
			let filter = tui::GlobalFilter::default();
			let search_config = tui::SearchConfig {
				embed_model: embed_model.clone(),
				backend: backend.as_ref(),
			};
			tui::run(&connection, filter, search_config, cli.theme.as_deref())?;
		}
	}

	Ok(())
}
