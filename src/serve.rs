use anyhow::Result;
use axum::{
	extract::{Path, Query, Request, State},
	http::{header::HOST, StatusCode},
	middleware::{self, Next},
	response::{IntoResponse, Json, Response},
	routing::get,
	Router,
};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::config::TagConfig;
use crate::llm::LlmBackend;
use crate::projects::ProjectRegistration;
use crate::query::{self, DocumentListOrder, FilterError, FilterInput, FtsQueryError, SearchSortColumn};
use crate::storage;

struct AppState {
	connection: Mutex<rusqlite::Connection>,
	backend: Box<dyn LlmBackend>,
	embed_model: String,
	tag_config: TagConfig,
	default_excluded_tags: Vec<String>,
	projects: Vec<ProjectRegistration>,
}

type ApiError = (StatusCode, Json<serde_json::Value>);

impl AppState {
	fn lock(&self) -> std::result::Result<MutexGuard<'_, rusqlite::Connection>, ApiError> {
		self.connection.lock().map_err(|_| err_500("database lock poisoned"))
	}

	/// The same validation MCP applies, so the two accept the same filter values.
	fn resolve(&self, connection: &rusqlite::Connection, input: FilterInput) -> std::result::Result<query::SearchFilters, ApiError> {
		let context = query::FilterContext {
			tag_config: &self.tag_config,
			default_excluded_tags: &self.default_excluded_tags,
			projects: &self.projects,
		};
		context.resolve(connection, input).map(|resolved| resolved.filters).map_err(err_query)
	}

	fn embed(&self, text: &str) -> std::result::Result<Vec<f32>, ApiError> {
		self.backend.embed(text, &self.embed_model)
			.map_err(|e| err_503(format!("embedding the query with {} failed: {:#}", self.embed_model, e)))
	}
}

type SharedState = Arc<AppState>;

fn err_500(e: impl std::fmt::Display) -> ApiError {
	(StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()})))
}

/// For a dependency that isn't ready (Ollama down, embeddings not computed):
/// the request was fine and may succeed later.
fn err_503(msg: impl Into<String>) -> ApiError {
	(StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": msg.into()})))
}

/// 400 for input the caller can fix, 500 for everything else.
fn err_query(e: anyhow::Error) -> ApiError {
	if let Some(fts_error) = e.downcast_ref::<FtsQueryError>() {
		return (
			StatusCode::BAD_REQUEST,
			Json(serde_json::json!({
				"error": fts_error.to_string(),
				"hint": "use plain words; pass filters as separate parameters, not field:value syntax",
			})),
		);
	}
	if let Some(filter_error) = e.downcast_ref::<FilterError>() {
		return (
			StatusCode::BAD_REQUEST,
			Json(serde_json::json!({"error": filter_error.to_string(), "field": filter_error.field})),
		);
	}
	err_500(e)
}

/// A configured embed model that doesn't match the stored vectors is a 503:
/// semantic search stays unavailable until the configuration is fixed.
fn err_embedding(e: anyhow::Error) -> ApiError {
	match e.downcast_ref::<storage::EmbeddingMismatch>() {
		Some(mismatch) => err_503(mismatch.to_string()),
		None => err_500(e),
	}
}

fn err_400(msg: impl Into<String>) -> ApiError {
	(StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": msg.into()})))
}

fn err_404(msg: impl Into<String>) -> ApiError {
	(StatusCode::NOT_FOUND, Json(serde_json::json!({"error": msg.into()})))
}

const DEFAULT_SIMILAR_LIMIT: usize = 10;
const MAX_SIMILAR_LIMIT: usize = 100;

fn similar_limit(limit: Option<usize>) -> usize {
	limit.unwrap_or(DEFAULT_SIMILAR_LIMIT).clamp(1, MAX_SIMILAR_LIMIT)
}

fn search_sort(sort: Option<&str>) -> std::result::Result<SearchSortColumn, ApiError> {
	match sort {
		None | Some("score") => Ok(SearchSortColumn::Score),
		Some("date") => Ok(SearchSortColumn::Date),
		Some(other) => Err(err_400(format!("unknown sort '{}' (expected score or date)", other))),
	}
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
) -> Result<impl IntoResponse, ApiError> {
	let sort = search_sort(params.sort.as_deref())?;
	let connection = state.lock()?;
	let filters = state.resolve(&connection, FilterInput {
		author: params.author,
		date_from: params.date_from,
		date_to: params.date_to,
		project: params.project,
		tag: params.tag,
		include_hidden: params.include_hidden.unwrap_or(false),
	})?;
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
) -> Result<impl IntoResponse, ApiError> {
	let limit = similar_limit(params.limit);
	let filters = {
		let connection = state.lock()?;
		if !storage::vec_table_exists(&connection) {
			return Err(err_503("no embeddings yet - run 'what-was-said embed' first"));
		}
		storage::check_embedding_model(&connection, &state.embed_model).map_err(err_embedding)?;
		state.resolve(&connection, FilterInput {
			author: params.author,
			date_from: params.date_from,
			date_to: params.date_to,
			project: params.project,
			tag: params.tag,
			include_hidden: params.include_hidden.unwrap_or(false),
		})?
	};
	let query_embedding = state.embed(&params.q)?;
	let connection = state.lock()?;
	let results = query::find_similar_grouped_filtered(
		&connection, &query_embedding, limit, &filters,
	).map_err(err_embedding)?;
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
) -> Result<impl IntoResponse, ApiError> {
	let order = match params.sort.as_deref() {
		None | Some("date") => DocumentListOrder::DateDescending,
		Some("date_asc") => DocumentListOrder::DateAscending,
		Some("title") => DocumentListOrder::Title,
		Some(other) => return Err(err_400(format!("unknown sort '{}' (expected date, date_asc, or title)", other))),
	};
	let limit = params.limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT);
	let connection = state.lock()?;
	let resolved = state.resolve(&connection, FilterInput {
		date_from: params.date_from,
		date_to: params.date_to,
		project: params.project,
		tag: params.tag,
		include_hidden: params.include_hidden.unwrap_or(false),
		..Default::default()
	})?;
	let filters = query::ListFilters {
		project: resolved.project,
		doctype: params.doctype.map(|d| d.trim().to_string()).filter(|d| !d.is_empty()),
		date_from: resolved.date_from,
		date_to: resolved.date_to,
		excluded_tags: resolved.excluded_tags,
		required_tags: resolved.required_tags,
	};
	let documents = query::list_documents(&connection, &filters, order, limit)
		.map_err(err_500)?;
	Ok(Json(documents))
}

async fn projects_handler(
	State(state): State<SharedState>,
) -> Result<impl IntoResponse, ApiError> {
	let connection = state.lock()?;
	let projects = query::list_projects(&connection, &state.projects).map_err(err_500)?;
	Ok(Json(projects))
}

async fn tags_handler(
	State(state): State<SharedState>,
) -> Result<impl IntoResponse, ApiError> {
	let connection = state.lock()?;
	let tags = query::list_tags(&connection, &state.tag_config).map_err(err_500)?;
	Ok(Json(tags))
}

async fn get_handler(
	State(state): State<SharedState>,
	Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
	let connection = state.lock()?;
	let doc = storage::get_document(&connection, id)
		.map_err(err_500)?
		.ok_or_else(|| err_404(format!("no document with id {}", id)))?;
	Ok(Json(doc))
}

async fn entries_handler(
	State(state): State<SharedState>,
	Path(doc_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
	let connection = state.lock()?;
	let doc = storage::get_document(&connection, doc_id)
		.map_err(err_500)?
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
) -> Result<impl IntoResponse, ApiError> {
	let connection = state.lock()?;
	let documents = storage::document_count(&connection).map_err(err_500)?;
	let entries = storage::entry_count(&connection).map_err(err_500)?;
	let chunks = storage::chunk_count(&connection).map_err(err_500)?;
	let embeddings = storage::count_chunks_with_embeddings(&connection).map_err(err_500)?;
	let claims = storage::claim_count(&connection).map_err(err_500)?;
	let claim_embeddings = storage::count_claims_with_embeddings(&connection).map_err(err_500)?;
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
) -> Result<impl IntoResponse, ApiError> {
	let connection = state.lock()?;
	let status = storage::get_derive_status(&connection).map_err(err_500)?;
	Ok(Json(status))
}

async fn claims_handler(
	State(state): State<SharedState>,
	Path(doc_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
	let connection = state.lock()?;
	let claims = storage::get_claims_for_document(&connection, doc_id)
		.map_err(err_500)?;
	let supersession = storage::supersession_status(&connection, doc_id)
		.map_err(err_500)?;
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
) -> Result<impl IntoResponse, ApiError> {
	let limit = similar_limit(params.limit);
	let filters = {
		let connection = state.lock()?;
		if !storage::vec_claims_table_exists(&connection) {
			return Err(err_503("no claim embeddings yet - run 'what-was-said extract' then 'what-was-said embed'"));
		}
		storage::check_embedding_model(&connection, &state.embed_model).map_err(err_embedding)?;
		state.resolve(&connection, FilterInput {
			author: params.author,
			tag: params.tag,
			include_hidden: params.include_hidden.unwrap_or(false),
			..Default::default()
		})?
	};
	let query_embedding = state.embed(&params.q)?;
	let connection = state.lock()?;
	let results = query::find_similar_claims_filtered(&connection, &query_embedding, limit, &filters)
		.map_err(err_embedding)?;
	Ok(Json(results))
}

/// Whether a request's Host header names this server. Binding to 127.0.0.1
/// stops remote clients, but a browser tricked by DNS rebinding still connects
/// locally while sending the attacker's hostname.
fn is_local_host(host: Option<&str>, port: u16) -> bool {
	let Some(host) = host else {
		return false;
	};
	let host = host.to_ascii_lowercase();
	["127.0.0.1", "localhost"]
		.iter()
		.any(|name| host == format!("{}:{}", name, port) || (port == 80 && host == *name))
}

async fn require_local_host(
	State(port): State<u16>,
	request: Request,
	next: Next,
) -> std::result::Result<Response, ApiError> {
	let local = is_local_host(request.headers().get(HOST).and_then(|value| value.to_str().ok()), port);
	if !local {
		return Err((
			StatusCode::FORBIDDEN,
			Json(serde_json::json!({"error": format!("Host must be 127.0.0.1:{} or localhost:{}", port, port)})),
		));
	}
	Ok(next.run(request).await)
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
		.with_state(state)
		.layer(middleware::from_fn_with_state(port, require_local_host));

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
		let filter_error = anyhow::Error::new(FilterError { field: "project", message: "unknown".to_string() });
		assert_eq!(err_query(filter_error).0, StatusCode::BAD_REQUEST);
	}

	#[test]
	fn sort_and_limit_parameters_are_checked() {
		assert_eq!(search_sort(None).unwrap(), SearchSortColumn::Score);
		assert_eq!(search_sort(Some("date")).unwrap(), SearchSortColumn::Date);
		assert_eq!(search_sort(Some("newest")).unwrap_err().0, StatusCode::BAD_REQUEST);
		assert_eq!(similar_limit(None), DEFAULT_SIMILAR_LIMIT);
		assert_eq!(similar_limit(Some(0)), 1);
		assert_eq!(similar_limit(Some(1_000_000)), MAX_SIMILAR_LIMIT);
	}

	#[test]
	fn only_local_host_headers_are_accepted() {
		assert!(is_local_host(Some("127.0.0.1:3030"), 3030));
		assert!(is_local_host(Some("LocalHost:3030"), 3030));
		assert!(!is_local_host(Some("localhost:3031"), 3030));
		assert!(!is_local_host(Some("attacker.example:3030"), 3030));
		assert!(!is_local_host(Some("localhost.attacker.example:3030"), 3030));
		assert!(!is_local_host(Some("localhost"), 3030));
		assert!(!is_local_host(None, 3030));
	}
}
