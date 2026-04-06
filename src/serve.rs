use anyhow::Result;
use axum::{
	extract::{Path, Query, State},
	http::StatusCode,
	response::{IntoResponse, Json},
	routing::get,
	Router,
};
use std::sync::{Arc, Mutex};

use crate::llm::LlmBackend;
use crate::storage;

struct AppState {
	connection: Mutex<rusqlite::Connection>,
	backend: Box<dyn LlmBackend>,
	embed_model: String,
}

type SharedState = Arc<AppState>;

fn err_500(e: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
	(StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()})))
}

fn err_404(msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
	(StatusCode::NOT_FOUND, Json(serde_json::json!({"error": msg.into()})))
}

#[derive(serde::Deserialize)]
struct SearchParams {
	q: String,
	sort: Option<String>,
}

async fn search_handler(
	State(state): State<SharedState>,
	Query(params): Query<SearchParams>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let sort = match params.sort.as_deref() {
		Some("date") => storage::SearchSortColumn::Date,
		_ => storage::SearchSortColumn::Score,
	};
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	let mut results = storage::search(&connection, &params.q, sort)
		.map_err(|e| err_500(e))?;
	for doc in &mut results {
		for chunk in &mut doc.chunks {
			chunk.snippet = crate::util::strip_fts_markers(&chunk.snippet);
		}
	}
	Ok(Json(results))
}

#[derive(serde::Deserialize)]
struct SimilarParams {
	q: String,
	limit: Option<usize>,
}

async fn similar_handler(
	State(state): State<SharedState>,
	Query(params): Query<SimilarParams>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let limit = params.limit.unwrap_or(10);
	let query_embedding = state.backend.embed(&params.q, &state.embed_model)
		.map_err(|e| err_500(e))?;
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	if !storage::vec_table_exists(&connection) {
		return Err(err_500("no embeddings yet - run 'commonplace embed' first"));
	}
	let results = storage::find_similar_chunks(&connection, &query_embedding, limit)
		.map_err(|e| err_500(e))?;
	Ok(Json(results))
}

async fn get_handler(
	State(state): State<SharedState>,
	Path(id): Path<i64>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	let doc = storage::get_document(&connection, id)
		.map_err(|e| err_500(e))?
		.ok_or_else(|| err_404(format!("no document with id {}", id)))?;
	Ok(Json(doc))
}

async fn entries_handler(
	State(state): State<SharedState>,
	Path(doc_id): Path<i64>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	let doc = storage::get_document(&connection, doc_id)
		.map_err(|e| err_500(e))?
		.ok_or_else(|| err_404(format!("no document with id {}", doc_id)))?;
	Ok(Json(doc.entries))
}

async fn stats_handler(
	State(state): State<SharedState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	let documents = storage::document_count(&connection).map_err(|e| err_500(e))?;
	let entries = storage::entry_count(&connection).map_err(|e| err_500(e))?;
	let chunks = storage::chunk_count(&connection).map_err(|e| err_500(e))?;
	let embeddings = storage::count_chunks_with_embeddings(&connection).map_err(|e| err_500(e))?;
	Ok(Json(serde_json::json!({
		"documents": documents,
		"entries": entries,
		"chunks": chunks,
		"embeddings": embeddings,
		"embeddings_total": chunks,
	})))
}

async fn derive_status_handler(
	State(state): State<SharedState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	let status = storage::get_derive_status(&connection).map_err(|e| err_500(e))?;
	Ok(Json(status))
}

pub fn run(
	connection: rusqlite::Connection,
	backend: Box<dyn LlmBackend>,
	embed_model: String,
	port: u16,
) -> Result<()> {
	let state = Arc::new(AppState {
		connection: Mutex::new(connection),
		backend,
		embed_model,
	});

	let app = Router::new()
		.route("/search", get(search_handler))
		.route("/similar", get(similar_handler))
		.route("/get/:id", get(get_handler))
		.route("/entries/:doc_id", get(entries_handler))
		.route("/stats", get(stats_handler))
		.route("/derive/status", get(derive_status_handler))
		.with_state(state);

	let runtime = tokio::runtime::Runtime::new()?;
	runtime.block_on(async {
		let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port)).await?;
		eprintln!("serving on http://127.0.0.1:{}", port);
		axum::serve(listener, app).await?;
		Ok::<(), anyhow::Error>(())
	})?;

	Ok(())
}
