use anyhow::Result;
use axum::{
	extract::{Path, Query, State},
	http::StatusCode,
	response::{IntoResponse, Json},
	routing::get,
	Router,
};
use std::sync::{Arc, Mutex};

use crate::config::TagConfig;
use crate::llm::LlmBackend;
use crate::projects::ProjectRegistration;
use crate::query::{self, DocumentListOrder, FtsQueryError, SearchSortColumn};
use crate::storage;

struct AppState {
	connection: Mutex<rusqlite::Connection>,
	backend: Box<dyn LlmBackend>,
	embed_model: String,
	tag_config: TagConfig,
	default_excluded_tags: Vec<String>,
	projects: Vec<ProjectRegistration>,
}

impl AppState {
	fn excluded_tags(&self, include_hidden: Option<bool>) -> Vec<String> {
		if include_hidden.unwrap_or(false) {
			Vec::new()
		} else {
			self.default_excluded_tags.clone()
		}
	}

	fn required_tags(&self, tag: Option<&str>) -> Vec<String> {
		tag.map(|t| self.tag_config.expand_required_tag(t)).unwrap_or_default()
	}

	fn filters(
		&self,
		author: Option<String>,
		date_from: Option<String>,
		date_to: Option<String>,
		project: Option<String>,
		tag: Option<&str>,
		include_hidden: Option<bool>,
	) -> query::SearchFilters {
		query::SearchFilters {
			author,
			date_from,
			date_to,
			project,
			excluded_tags: self.excluded_tags(include_hidden),
			required_tags: self.required_tags(tag),
		}
	}
}

type SharedState = Arc<AppState>;

fn err_500(e: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
	(StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()})))
}

/// 400 for input the caller can fix, 500 for everything else.
fn err_query(e: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
	match e.downcast_ref::<FtsQueryError>() {
		Some(fts_error) => (
			StatusCode::BAD_REQUEST,
			Json(serde_json::json!({
				"error": fts_error.to_string(),
				"hint": "use plain words; pass filters as separate parameters, not field:value syntax",
			})),
		),
		None => err_500(e),
	}
}

fn err_400(msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
	(StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": msg.into()})))
}

fn err_404(msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
	(StatusCode::NOT_FOUND, Json(serde_json::json!({"error": msg.into()})))
}

#[derive(serde::Deserialize)]
struct SearchParams {
	q: String,
	sort: Option<String>,
	author: Option<String>,
	date_from: Option<String>,
	date_to: Option<String>,
	project: Option<String>,
	tag: Option<String>,
	include_hidden: Option<bool>,
}

async fn search_handler(
	State(state): State<SharedState>,
	Query(params): Query<SearchParams>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let sort = match params.sort.as_deref() {
		Some("date") => SearchSortColumn::Date,
		_ => SearchSortColumn::Score,
	};
	let filters = state.filters(
		params.author, params.date_from, params.date_to, params.project,
		params.tag.as_deref(), params.include_hidden,
	);
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	let mut results = query::search_filtered(&connection, &params.q, sort, &filters)
		.map_err(err_query)?;
	query::strip_fts_markers(&mut results);
	Ok(Json(results))
}

#[derive(serde::Deserialize)]
struct SimilarParams {
	q: String,
	limit: Option<usize>,
	author: Option<String>,
	date_from: Option<String>,
	date_to: Option<String>,
	project: Option<String>,
	tag: Option<String>,
	include_hidden: Option<bool>,
}

async fn similar_handler(
	State(state): State<SharedState>,
	Query(params): Query<SimilarParams>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let limit = params.limit.unwrap_or(10);
	let filters = state.filters(
		params.author, params.date_from, params.date_to, params.project,
		params.tag.as_deref(), params.include_hidden,
	);
	let query_embedding = state.backend.embed(&params.q, &state.embed_model)
		.map_err(|e| err_500(e))?;
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	if !storage::vec_table_exists(&connection) {
		return Err(err_500("no embeddings yet - run 'what-was-said embed' first"));
	}
	let results = query::find_similar_grouped_filtered(
		&connection, &query_embedding, limit, &filters,
	).map_err(|e| err_500(e))?;
	Ok(Json(results))
}

const DEFAULT_LIST_LIMIT: usize = 20;
const MAX_LIST_LIMIT: usize = 500;

#[derive(serde::Deserialize)]
struct DocumentsParams {
	project: Option<String>,
	tag: Option<String>,
	doctype: Option<String>,
	date_from: Option<String>,
	date_to: Option<String>,
	sort: Option<String>,
	limit: Option<usize>,
	include_hidden: Option<bool>,
}

async fn documents_handler(
	State(state): State<SharedState>,
	Query(params): Query<DocumentsParams>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let order = match params.sort.as_deref() {
		None | Some("date") => DocumentListOrder::DateDescending,
		Some("date_asc") => DocumentListOrder::DateAscending,
		Some("title") => DocumentListOrder::Title,
		Some(other) => return Err(err_400(format!("unknown sort '{}' (expected date, date_asc, or title)", other))),
	};
	let filters = query::ListFilters {
		project: params.project,
		doctype: params.doctype,
		date_from: params.date_from,
		date_to: params.date_to,
		excluded_tags: state.excluded_tags(params.include_hidden),
		required_tags: state.required_tags(params.tag.as_deref()),
	};
	let limit = params.limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT);
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	let documents = query::list_documents(&connection, &filters, order, limit)
		.map_err(|e| err_500(e))?;
	Ok(Json(documents))
}

async fn projects_handler(
	State(state): State<SharedState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	let projects = query::list_projects(&connection, &state.projects).map_err(|e| err_500(e))?;
	Ok(Json(projects))
}

async fn tags_handler(
	State(state): State<SharedState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	let tags = query::list_tags(&connection, &state.tag_config).map_err(|e| err_500(e))?;
	Ok(Json(tags))
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
	let mut response = serde_json::json!({
		"document_id": doc.id,
		"superseded": doc.superseded,
		"entries": doc.entries,
	});
	if let Some(current) = doc.current_document_id {
		response["current_document_id"] = serde_json::json!(current);
	}
	Ok(Json(response))
}

async fn stats_handler(
	State(state): State<SharedState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	let documents = storage::document_count(&connection).map_err(|e| err_500(e))?;
	let entries = storage::entry_count(&connection).map_err(|e| err_500(e))?;
	let chunks = storage::chunk_count(&connection).map_err(|e| err_500(e))?;
	let embeddings = storage::count_chunks_with_embeddings(&connection).map_err(|e| err_500(e))?;
	let claims = storage::claim_count(&connection).map_err(|e| err_500(e))?;
	let claim_embeddings = storage::count_claims_with_embeddings(&connection).map_err(|e| err_500(e))?;
	Ok(Json(serde_json::json!({
		"documents": documents,
		"entries": entries,
		"chunks": chunks,
		"embeddings": embeddings,
		"embeddings_total": chunks,
		"claims": claims,
		"claim_embeddings": claim_embeddings,
	})))
}

async fn derive_status_handler(
	State(state): State<SharedState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	let status = storage::get_derive_status(&connection).map_err(|e| err_500(e))?;
	Ok(Json(status))
}

async fn claims_handler(
	State(state): State<SharedState>,
	Path(doc_id): Path<i64>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	let claims = storage::get_claims_for_document(&connection, doc_id)
		.map_err(|e| err_500(e))?;
	let supersession = storage::supersession_status(&connection, doc_id)
		.map_err(|e| err_500(e))?;
	let mut response = serde_json::json!({
		"document_id": doc_id,
		"superseded": supersession.superseded,
		"claims": claims,
	});
	if let Some(current) = supersession.current_document_id {
		response["current_document_id"] = serde_json::json!(current);
	}
	Ok(Json(response))
}

async fn similar_claims_handler(
	State(state): State<SharedState>,
	Query(params): Query<SimilarParams>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
	let limit = params.limit.unwrap_or(10);
	let filters = state.filters(None, None, None, None, params.tag.as_deref(), params.include_hidden);
	let query_embedding = state.backend.embed(&params.q, &state.embed_model)
		.map_err(|e| err_500(e))?;
	let connection = state.connection.lock().map_err(|e| err_500(e))?;
	if !storage::vec_claims_table_exists(&connection) {
		return Err(err_500("no claim embeddings yet - run 'what-was-said extract' then 'what-was-said embed'"));
	}
	let results = query::find_similar_claims_filtered(&connection, &query_embedding, limit, &filters)
		.map_err(|e| err_500(e))?;
	Ok(Json(results))
}

pub fn run(
	connection: rusqlite::Connection,
	backend: Box<dyn LlmBackend>,
	embed_model: String,
	port: u16,
	tag_config: TagConfig,
	projects: Vec<ProjectRegistration>,
) -> Result<()> {
	let default_excluded_tags = tag_config.expanded_default_exclude();
	let state = Arc::new(AppState {
		connection: Mutex::new(connection),
		backend,
		embed_model,
		tag_config,
		default_excluded_tags,
		projects,
	});

	let app = Router::new()
		.route("/search", get(search_handler))
		.route("/documents", get(documents_handler))
		.route("/projects", get(projects_handler))
		.route("/tags", get(tags_handler))
		.route("/similar", get(similar_handler))
		.route("/get/:id", get(get_handler))
		.route("/entries/:doc_id", get(entries_handler))
		.route("/stats", get(stats_handler))
		.route("/derive/status", get(derive_status_handler))
		.route("/claims/doc/:doc_id", get(claims_handler))
		.route("/claims/similar", get(similar_claims_handler))
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn fts_query_errors_map_to_400_and_others_to_500() {
		let fts_error = anyhow::Error::new(FtsQueryError {
			query: "project:montre".to_string(),
			detail: "no such column: project".to_string(),
		});
		assert_eq!(err_query(fts_error).0, StatusCode::BAD_REQUEST);
		assert_eq!(err_query(anyhow::anyhow!("disk I/O error")).0, StatusCode::INTERNAL_SERVER_ERROR);
	}
}
