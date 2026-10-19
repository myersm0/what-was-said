use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Result;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::config::{McpConfig, TagConfig};
use crate::llm::LlmBackend;
use crate::projects::ProjectRegistration;
use crate::query::{self, DocumentListOrder, FtsQueryError, GroupedSearchResult, SearchSortColumn};
use crate::storage;

const PROJECT_DESCRIPTION: &str = "Exact identifier of a registered curated project, as returned by \
	list_projects. Omit it unless you have that identifier. Do not fill it in because the user called \
	something a project: captured emails, Slack threads and notes about a topic are not a project. It \
	never matches tags; use `tag` for those.";
const TAG_DESCRIPTION: &str = "Only documents carrying this tag, or a tag it includes. Use an exact \
	name from list_tags.";
const AUTHOR_DESCRIPTION: &str = "Case-insensitive substring of a message author's name.";
const DATE_FROM_DESCRIPTION: &str = "YYYY-MM-DD, inclusive. Matches passages captured on or after \
	this date. A thread or page clipped repeatedly has passages captured at different times, so a hit's \
	`captured` date can be earlier than its document's clip_date (the latest capture).";
const DATE_TO_DESCRIPTION: &str = "YYYY-MM-DD, inclusive. Matches passages captured on or before \
	this date.";
const LIST_DATE_FROM_DESCRIPTION: &str = "YYYY-MM-DD, inclusive. Documents whose clip_date (latest \
	capture) is on or after this date.";
const LIST_DATE_TO_DESCRIPTION: &str = "YYYY-MM-DD, inclusive. Documents whose clip_date (latest \
	capture) is on or before this date.";
const INCLUDE_HIDDEN_DESCRIPTION: &str = "Also return documents hidden by default: superseded older \
	versions and anything tagged junk or archived. Default false.";

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub enum KeywordSort {
	Score,
	Date,
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub enum ListSort {
	Date,
	DateAsc,
	Title,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct KeywordParams {
	/// Plain words and/or "quoted phrases". No operators, no field:value filters.
	pub query: String,
	#[schemars(description = AUTHOR_DESCRIPTION)]
	pub author: Option<String>,
	#[schemars(description = DATE_FROM_DESCRIPTION)]
	pub date_from: Option<String>,
	#[schemars(description = DATE_TO_DESCRIPTION)]
	pub date_to: Option<String>,
	#[schemars(description = PROJECT_DESCRIPTION)]
	pub project: Option<String>,
	#[schemars(description = TAG_DESCRIPTION)]
	pub tag: Option<String>,
	#[schemars(description = INCLUDE_HIDDEN_DESCRIPTION)]
	pub include_hidden: Option<bool>,
	/// "score" (best match first, the default) or "date" (newest first).
	pub sort: Option<KeywordSort>,
	/// Number of documents to return. Default 5.
	pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SemanticParams {
	/// A description of what you are looking for, in natural language.
	pub query: String,
	#[schemars(description = AUTHOR_DESCRIPTION)]
	pub author: Option<String>,
	#[schemars(description = DATE_FROM_DESCRIPTION)]
	pub date_from: Option<String>,
	#[schemars(description = DATE_TO_DESCRIPTION)]
	pub date_to: Option<String>,
	#[schemars(description = PROJECT_DESCRIPTION)]
	pub project: Option<String>,
	#[schemars(description = TAG_DESCRIPTION)]
	pub tag: Option<String>,
	#[schemars(description = INCLUDE_HIDDEN_DESCRIPTION)]
	pub include_hidden: Option<bool>,
	/// Number of documents to return. Default 5.
	pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ClaimsParams {
	/// What was asserted, decided or planned, in natural language.
	pub query: String,
	#[schemars(description = TAG_DESCRIPTION)]
	pub tag: Option<String>,
	#[schemars(description = INCLUDE_HIDDEN_DESCRIPTION)]
	pub include_hidden: Option<bool>,
	/// Number of claims to return. Default 10.
	pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct GetDocumentParams {
	/// Document id from a search or list result.
	pub id: i64,
	/// Character offset to start from, for continuing a truncated document. Default 0.
	pub offset: Option<u64>,
	/// Character budget for this response; capped at the server's maximum.
	pub max_chars: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ListParams {
	#[schemars(description = PROJECT_DESCRIPTION)]
	pub project: Option<String>,
	#[schemars(description = TAG_DESCRIPTION)]
	pub tag: Option<String>,
	/// Exact doctype name, e.g. "email" or "slack"; see stats for the doctypes present.
	pub doctype: Option<String>,
	#[schemars(description = LIST_DATE_FROM_DESCRIPTION)]
	pub date_from: Option<String>,
	#[schemars(description = LIST_DATE_TO_DESCRIPTION)]
	pub date_to: Option<String>,
	/// "date" (newest first, the default), "date_asc" (oldest first) or "title".
	pub sort: Option<ListSort>,
	#[schemars(description = INCLUDE_HIDDEN_DESCRIPTION)]
	pub include_hidden: Option<bool>,
	/// Number of documents to return. Default 20.
	pub limit: Option<u32>,
}

type ToolResult = std::result::Result<String, String>;

pub struct McpContext {
	connection: Mutex<Connection>,
	backend: Box<dyn LlmBackend>,
	embed_model: String,
	tag_config: TagConfig,
	default_excluded_tags: Vec<String>,
	projects: Vec<ProjectRegistration>,
	sizing: McpConfig,
	database_path: String,
}

#[derive(Default)]
struct ScopeArgs {
	author: Option<String>,
	date_from: Option<String>,
	date_to: Option<String>,
	project: Option<String>,
	tag: Option<String>,
	include_hidden: Option<bool>,
}

struct Scope {
	filters: query::SearchFilters,
	applied: Map<String, Value>,
}

#[derive(Serialize)]
struct HitOut {
	#[serde(skip_serializing_if = "Option::is_none")]
	heading: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	author: Option<String>,
	/// Only when this passage was captured before the document's clip_date.
	#[serde(skip_serializing_if = "Option::is_none")]
	captured: Option<String>,
	excerpt: String,
}

#[derive(Serialize)]
struct SearchDocumentOut {
	document_id: i64,
	source_title: String,
	clip_date: String,
	superseded: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	current_document_id: Option<i64>,
	hits: Vec<HitOut>,
	#[serde(skip_serializing_if = "is_zero")]
	more_hits: usize,
}

fn is_zero(n: &usize) -> bool {
	*n == 0
}

impl McpContext {
	#[allow(clippy::too_many_arguments)]
	pub fn new(
		connection: Connection,
		backend: Box<dyn LlmBackend>,
		embed_model: String,
		tag_config: TagConfig,
		projects: Vec<ProjectRegistration>,
		sizing: McpConfig,
		database_path: String,
	) -> Self {
		let default_excluded_tags = tag_config.expanded_default_exclude();
		McpContext {
			connection: Mutex::new(connection),
			backend,
			embed_model,
			tag_config,
			default_excluded_tags,
			projects,
			sizing,
			database_path,
		}
	}

	fn lock(&self) -> std::result::Result<MutexGuard<'_, Connection>, String> {
		self.connection.lock().map_err(|_| "internal error: database lock poisoned".to_string())
	}

	pub fn search_keyword(&self, params: KeywordParams) -> ToolResult {
		let query_text = params.query.trim().to_string();
		if query_text.is_empty() {
			return Err("query is empty: give one or more plain words".to_string());
		}
		let connection = self.lock()?;
		let scope = self.resolve_scope(&connection, ScopeArgs {
			author: params.author,
			date_from: params.date_from,
			date_to: params.date_to,
			project: params.project,
			tag: params.tag,
			include_hidden: params.include_hidden,
		})?;
		let sort = match params.sort {
			Some(KeywordSort::Date) => SearchSortColumn::Date,
			_ => SearchSortColumn::Score,
		};
		let results = query::search_filtered(&connection, &query_text, sort, &scope.filters)
			.map_err(|error| match error.downcast_ref::<FtsQueryError>() {
				Some(fts_error) => format!("{}. Use plain words or a \"quoted phrase\".", fts_error),
				None => internal(error),
			})?;
		let documents = bounded(params.limit, self.sizing.search_documents, self.sizing.max_search_documents);
		Ok(self.render_search("keyword", &query_text, scope.applied, results, documents, true, &[
			"every word must appear, so try fewer or shorter words",
			"search_semantic finds paraphrases and related wording",
		]))
	}

	pub fn search_semantic(&self, params: SemanticParams) -> ToolResult {
		let query_text = params.query.trim().to_string();
		if query_text.is_empty() {
			return Err("query is empty: describe what you are looking for".to_string());
		}
		let documents = bounded(params.limit, self.sizing.search_documents, self.sizing.max_search_documents);
		let connection = self.lock()?;
		if !storage::vec_table_exists(&connection) {
			return Err("Semantic search is unavailable: no chunk embeddings exist yet (the user runs \
				`what-was-said embed`). Use search_keyword or list_documents.".to_string());
		}
		let scope = self.resolve_scope(&connection, ScopeArgs {
			author: params.author,
			date_from: params.date_from,
			date_to: params.date_to,
			project: params.project,
			tag: params.tag,
			include_hidden: params.include_hidden,
		})?;
		let embedding = self.embed_query(&query_text)?;
		let chunk_limit = documents * self.sizing.hits_per_document * 3;
		let results = query::find_similar_grouped_filtered(&connection, &embedding, chunk_limit, &scope.filters)
			.map_err(internal)?;
		Ok(self.render_search("semantic", &query_text, scope.applied, results, documents, false, &[
			"search_keyword finds exact names and identifiers",
		]))
	}

	pub fn search_claims(&self, params: ClaimsParams) -> ToolResult {
		let query_text = params.query.trim().to_string();
		if query_text.is_empty() {
			return Err("query is empty: describe the decision or assertion you are looking for".to_string());
		}
		let limit = bounded(params.limit, self.sizing.claims_limit, self.sizing.max_claims_limit);
		let connection = self.lock()?;
		if !storage::vec_claims_table_exists(&connection) {
			return Err("Claim search is unavailable: no claim embeddings exist yet (the user runs \
				`what-was-said extract` then `embed`). Use search_semantic or search_keyword.".to_string());
		}
		let scope = self.resolve_scope(&connection, ScopeArgs {
			tag: params.tag,
			include_hidden: params.include_hidden,
			..Default::default()
		})?;
		let embedding = self.embed_query(&query_text)?;
		let claims = query::find_similar_claims_filtered(&connection, &embedding, limit, &scope.filters)
			.map_err(internal)?;
		let mut response = json!({
			"request": {"method": "claims", "query": query_text, "filters": scope.applied},
			"claims": claims,
		});
		if claims.is_empty() {
			response["note"] = json!(empty_note("claims", &scope.applied, &[
				"only documents processed by extract have claims",
				"search_semantic or search_keyword search the passages themselves",
			]));
		}
		Ok(response.to_string())
	}

	pub fn get_document(&self, params: GetDocumentParams) -> ToolResult {
		let connection = self.lock()?;
		let document = storage::get_document(&connection, params.id)
			.map_err(internal)?
			.ok_or_else(|| format!("No document with id {}. Ids come from search and list results.", params.id))?;
		let tags = storage::get_tags_for_document(&connection, params.id).map_err(internal)?;
		drop(connection);

		let text = render_entries(&document.entries);
		let total_chars = text.chars().count();
		let offset = params.offset.unwrap_or(0) as usize;
		if offset > 0 && offset >= total_chars {
			return Err(format!(
				"offset {} is past the end of document {} ({} characters)",
				offset, params.id, total_chars,
			));
		}
		let budget = params.max_chars
			.map(|requested| requested as usize)
			.unwrap_or(self.sizing.document_chars)
			.clamp(1, self.sizing.document_chars.max(1));
		let mut slice: String = text.chars().skip(offset).take(budget).collect();
		let end = offset + slice.chars().count();
		let truncation = (offset > 0 || end < total_chars).then(|| {
			let mut message = format!("Showing characters {}-{} of {}.", offset, end, total_chars);
			if end < total_chars {
				message.push_str(&format!(
					" Call get_document with id={} and offset={} for the rest.",
					params.id, end,
				));
			}
			message
		});
		if end < total_chars {
			slice.push_str(&format!("\n\n[truncated: {} more characters; continue at offset={}]", total_chars - end, end));
		}

		let mut response = json!({
			"document_id": document.id,
			"title": document.title,
			"source_title": document.source_title,
			"clip_date": document.clip_date,
			"doctype": document.doctype_name,
			"project": document.project,
			"doc_status": document.doc_status,
			"doc_role": document.doc_role,
			"relative_path": document.relative_path,
			"tags": tags,
			"superseded": document.superseded,
			"entries": document.entries.len(),
			"total_chars": total_chars,
			"text": slice,
		});
		if let Some(current) = document.current_document_id {
			response["current_document_id"] = json!(current);
		}
		if let Some(truncation) = truncation {
			response["truncation"] = json!(truncation);
		}
		Ok(response.to_string())
	}

	pub fn list_documents(&self, params: ListParams) -> ToolResult {
		let connection = self.lock()?;
		let scope = self.resolve_scope(&connection, ScopeArgs {
			date_from: params.date_from,
			date_to: params.date_to,
			project: params.project,
			tag: params.tag,
			include_hidden: params.include_hidden,
			..Default::default()
		})?;
		let doctype = present(params.doctype);
		let mut applied = scope.applied;
		if let Some(doctype) = &doctype {
			applied.insert("doctype".to_string(), json!(doctype));
		}
		let order = match params.sort {
			Some(ListSort::DateAsc) => DocumentListOrder::DateAscending,
			Some(ListSort::Title) => DocumentListOrder::Title,
			_ => DocumentListOrder::DateDescending,
		};
		let limit = bounded(params.limit, self.sizing.list_limit, self.sizing.max_list_limit);
		let filters = query::ListFilters {
			project: scope.filters.project,
			doctype,
			date_from: scope.filters.date_from,
			date_to: scope.filters.date_to,
			excluded_tags: scope.filters.excluded_tags,
			required_tags: scope.filters.required_tags,
		};
		let mut documents = query::list_documents(&connection, &filters, order, limit + 1).map_err(internal)?;
		let more = documents.len() > limit;
		documents.truncate(limit);
		let mut response = json!({
			"request": {"filters": applied, "sort": sort_name(order), "limit": limit},
			"documents": documents,
			"more": more,
		});
		if documents.is_empty() {
			response["note"] = json!(empty_note("documents", &applied, &[
				"check exact names with list_projects, list_tags, or stats for doctypes",
			]));
		}
		Ok(response.to_string())
	}

	pub fn list_projects(&self) -> ToolResult {
		let connection = self.lock()?;
		let projects = query::list_projects(&connection, &self.projects).map_err(internal)?;
		let note = if projects.is_empty() {
			"No curated projects are registered. Leave every project parameter unset."
		} else {
			"Only these exact names are valid project values. Captured documents about a topic are not \
			 a project; find them with tag or query terms."
		};
		Ok(json!({"projects": projects, "note": note}).to_string())
	}

	pub fn list_tags(&self) -> ToolResult {
		let connection = self.lock()?;
		let tags = query::list_tags(&connection, &self.tag_config).map_err(internal)?;
		Ok(json!({"tags": tags}).to_string())
	}

	pub fn stats(&self) -> ToolResult {
		let connection = self.lock()?;
		let embeddings = storage::count_chunks_with_embeddings(&connection).map_err(internal)?;
		let claim_embeddings = storage::count_claims_with_embeddings(&connection).map_err(internal)?;
		let doctypes: Map<String, Value> = storage::doctype_counts(&connection)
			.map_err(internal)?
			.into_iter()
			.map(|(name, count)| (name.unwrap_or_else(|| "(none)".to_string()), json!(count)))
			.collect();
		let date_range = storage::clip_date_range(&connection)
			.map_err(internal)?
			.map(|(oldest, newest)| json!({"oldest": oldest, "newest": newest}));
		Ok(json!({
			"database": self.database_path,
			"documents": storage::document_count(&connection).map_err(internal)?,
			"entries": storage::entry_count(&connection).map_err(internal)?,
			"chunks": storage::chunk_count(&connection).map_err(internal)?,
			"chunk_embeddings": embeddings,
			"claims": storage::claim_count(&connection).map_err(internal)?,
			"claim_embeddings": claim_embeddings,
			"semantic_search_available": embeddings > 0,
			"claim_search_available": claim_embeddings > 0,
			"clip_dates": date_range,
			"doctypes": doctypes,
			"registered_projects": self.projects.len(),
			"hidden_by_default": self.default_excluded_tags,
		}).to_string())
	}

	fn embed_query(&self, text: &str) -> std::result::Result<Vec<f32>, String> {
		self.backend.embed(text, &self.embed_model).map_err(|error| format!(
			"Semantic search is unavailable: embedding the query with {} failed ({:#}). Ollama may not be \
			 running. search_keyword and list_documents still work.",
			self.embed_model, error,
		))
	}

	/// Validates and normalizes the shared filters. A project or tag that cannot
	/// match anything is an error rather than a silent empty result.
	fn resolve_scope(&self, connection: &Connection, args: ScopeArgs) -> std::result::Result<Scope, String> {
		let mut applied = Map::new();
		let author = present(args.author);
		let date_from = present(args.date_from);
		let date_to = present(args.date_to);
		let project = present(args.project);
		let tag = present(args.tag);
		for (name, value) in [("date_from", &date_from), ("date_to", &date_to)] {
			if let Some(value) = value {
				check_date(name, value)?;
			}
		}
		if let Some(project) = &project {
			self.check_project(connection, project)?;
		}
		let required_tags = match &tag {
			Some(tag) => {
				let expanded = self.tag_config.expand_required_tag(tag);
				let carrying = storage::count_documents_with_any_tag(connection, &expanded).map_err(internal)?;
				if carrying == 0 {
					return Err(format!(
						"No document carries the tag '{}' or any tag it includes. Use an exact name from list_tags.",
						tag,
					));
				}
				expanded
			}
			None => Vec::new(),
		};
		let excluded_tags = if args.include_hidden.unwrap_or(false) {
			Vec::new()
		} else {
			self.default_excluded_tags.clone()
		};
		for (name, value) in [
			("author", &author),
			("date_from", &date_from),
			("date_to", &date_to),
			("project", &project),
			("tag", &tag),
		] {
			if let Some(value) = value {
				applied.insert(name.to_string(), json!(value));
			}
		}
		if !excluded_tags.is_empty() {
			applied.insert("hidden_tags_excluded".to_string(), json!(excluded_tags));
		}
		Ok(Scope {
			filters: query::SearchFilters {
				author,
				date_from,
				date_to,
				project,
				excluded_tags,
				required_tags,
			},
			applied,
		})
	}

	fn check_project(&self, connection: &Connection, project: &str) -> std::result::Result<(), String> {
		let known = query::list_projects(connection, &self.projects).map_err(internal)?;
		if known.iter().any(|listing| listing.name == project) {
			return Ok(());
		}
		let names: Vec<&str> = known.iter().map(|listing| listing.name.as_str()).collect();
		Err(if names.is_empty() {
			format!(
				"'{}' is not a curated project: none are registered. Leave project unset and use tag or \
				 query terms for the topic.",
				project,
			)
		} else {
			format!(
				"'{}' is not a registered curated project (known: {}). Leave project unset unless you \
				 have an exact name from list_projects; use tag or query terms for the topic.",
				project, names.join(", "),
			)
		})
	}

	#[allow(clippy::too_many_arguments)]
	fn render_search(
		&self,
		method: &str,
		query_text: &str,
		applied: Map<String, Value>,
		results: Vec<GroupedSearchResult>,
		documents: usize,
		anchored: bool,
		suggestions: &[&str],
	) -> String {
		let more_documents = results.len().saturating_sub(documents);
		let shaped: Vec<SearchDocumentOut> = results
			.into_iter()
			.take(documents)
			.map(|result| shape_document(result, self.sizing.hits_per_document, self.sizing.hit_chars, anchored))
			.collect();
		let mut response = json!({
			"request": {"method": method, "query": query_text, "filters": applied},
			"results": shaped,
		});
		if more_documents > 0 {
			response["more_documents"] = json!(more_documents);
		}
		if shaped.is_empty() {
			response["note"] = json!(empty_note("documents", &applied, suggestions));
		}
		response.to_string()
	}
}

fn shape_document(result: GroupedSearchResult, hits: usize, hit_chars: usize, anchored: bool) -> SearchDocumentOut {
	let document_date = result.clip_date;
	let mut chunks = result.chunks;
	chunks.sort_by(|a, b| a.rank.partial_cmp(&b.rank).unwrap_or(std::cmp::Ordering::Equal));
	let more_hits = chunks.len().saturating_sub(hits);
	let hits = chunks
		.into_iter()
		.take(hits)
		.map(|chunk| {
			let anchor = if anchored { snippet_anchor(&chunk.snippet) } else { None };
			HitOut {
				heading: chunk.heading_title,
				author: chunk.author,
				captured: (chunk.captured != document_date).then_some(chunk.captured),
				excerpt: excerpt(
					&chunk.chunk_body,
					anchor.as_ref().map(|(fragment, offset)| (fragment.as_str(), *offset)),
					hit_chars,
				),
			}
		})
		.collect();
	SearchDocumentOut {
		document_id: result.document_id,
		source_title: result.source_title,
		clip_date: document_date,
		superseded: result.superseded,
		current_document_id: result.current_document_id,
		hits,
		more_hits,
	}
}

/// The longest marker-free run of an FTS snippet, a verbatim fragment of the
/// chunk, with the char offset of its first highlighted term.
fn snippet_anchor(snippet: &str) -> Option<(String, usize)> {
	let part = snippet.split('\x01').max_by_key(|part| part.len())?;
	let term_offset = part.find('\x02')
		.map(|index| part[..index].replace('\x03', "").chars().count())
		.unwrap_or(0);
	let fragment = part.replace(['\x02', '\x03'], "");
	let leading_whitespace = fragment.chars().take_while(|c| c.is_whitespace()).count();
	let fragment = fragment.trim().to_string();
	(fragment.chars().count() >= 3).then(|| (fragment, term_offset.saturating_sub(leading_whitespace)))
}

/// At most `max_chars` of `body`, placing the anchored term (when the fragment
/// is found) a third of the way in. Cuts are marked with an ellipsis.
fn excerpt(body: &str, anchor: Option<(&str, usize)>, max_chars: usize) -> String {
	let body = body.trim();
	let total = body.chars().count();
	if total <= max_chars {
		return body.to_string();
	}
	let term_char = anchor
		.and_then(|(fragment, term_offset)| {
			body.find(fragment).map(|byte_index| body[..byte_index].chars().count() + term_offset)
		})
		.unwrap_or(0);
	let start = term_char.saturating_sub(max_chars / 3).min(total - max_chars);
	let mut text: String = body.chars().skip(start).take(max_chars).collect();
	if start + max_chars < total {
		text.push('…');
	}
	if start > 0 {
		text.insert(0, '…');
	}
	text
}

fn render_entries(entries: &[storage::EntryContent]) -> String {
	let mut parts = Vec::with_capacity(entries.len());
	for entry in entries {
		let mut part = String::new();
		if let Some(title) = &entry.heading_title {
			let level = entry.heading_level.unwrap_or(1).clamp(1, 6) as usize;
			part.push_str(&format!("{} {}\n", "#".repeat(level), title));
		}
		let attribution = match (&entry.author, &entry.timestamp) {
			(Some(author), Some(timestamp)) => Some(format!("{} ({})", author, timestamp)),
			(Some(author), None) => Some(author.clone()),
			(None, Some(timestamp)) => Some(timestamp.clone()),
			(None, None) => None,
		};
		if let Some(attribution) = attribution {
			part.push_str(&format!("[{}]\n", attribution));
		}
		part.push_str(entry.body.trim());
		parts.push(part);
	}
	parts.join("\n\n")
}

fn empty_note(what: &str, applied: &Map<String, Value>, suggestions: &[&str]) -> String {
	let narrowing: Vec<String> = applied
		.iter()
		.filter(|(name, _)| name.as_str() != "hidden_tags_excluded")
		.map(|(name, value)| format!("{}={}", name, value))
		.collect();
	let mut note = if narrowing.is_empty() {
		format!("No {} matched this request (no filters applied).", what)
	} else {
		format!(
			"No {} matched this request with filters {}. Try again without some of them.",
			what, narrowing.join(", "),
		)
	};
	for suggestion in suggestions {
		note.push_str(&format!(" Also: {}.", suggestion));
	}
	if applied.contains_key("hidden_tags_excluded") {
		note.push_str(" Hidden documents (superseded versions, junk, archived) were excluded; include_hidden=true adds them.");
	}
	note
}

fn sort_name(order: DocumentListOrder) -> &'static str {
	match order {
		DocumentListOrder::DateDescending => "date",
		DocumentListOrder::DateAscending => "date_asc",
		DocumentListOrder::Title => "title",
	}
}

fn present(value: Option<String>) -> Option<String> {
	value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn bounded(requested: Option<u32>, default: usize, max: usize) -> usize {
	requested.map(|n| n as usize).unwrap_or(default).clamp(1, max.max(1))
}

fn check_date(name: &str, value: &str) -> std::result::Result<(), String> {
	let date_part = value.get(..10).unwrap_or(value);
	chrono::NaiveDate::parse_from_str(date_part, "%Y-%m-%d")
		.map(|_| ())
		.map_err(|_| format!("{} must be a date in YYYY-MM-DD form, got '{}'", name, value))
}

fn internal(error: impl std::fmt::Display) -> String {
	format!("internal error: {}", error)
}

#[derive(Clone)]
pub struct WwsServer {
	context: Arc<McpContext>,
}

impl WwsServer {
	pub fn new(context: McpContext) -> Self {
		WwsServer { context: Arc::new(context) }
	}

	/// Tool logic is synchronous (SQLite, blocking HTTP to Ollama).
	fn blocking<F>(&self, work: F) -> ToolResult
	where
		F: FnOnce(&McpContext) -> ToolResult,
	{
		tokio::task::block_in_place(|| work(&self.context))
	}
}

#[tool_router]
impl WwsServer {
	#[tool(description = "Full-text search for exact words, names, identifiers, commands or phrases. \
		`query` is plain text: every word must appear (prefix-matched) and a \"quoted phrase\" must appear \
		as written. Punctuation inside words is fine (nrg-svc-hcpx, file.zip). There are no operators, and \
		filters never go in the query text (not project:x or author:y); use the separate parameters. \
		Returns a few documents with short excerpts, best match first, or newest first with sort=date. \
		Excerpts are for choosing what to read: open the best two or three with get_document before \
		answering. If results are thin, try search_semantic.")]
	async fn search_keyword(&self, Parameters(params): Parameters<KeywordParams>) -> ToolResult {
		self.blocking(|context| context.search_keyword(params))
	}

	#[tool(description = "Meaning-based search: finds passages about a concept even when the wording \
		differs. Use it for topics, paraphrases and questions, or when search_keyword comes back thin. \
		Same filters as search_keyword. Returns a few documents with short excerpts, most similar first; \
		open the best two or three with get_document before answering.")]
	async fn search_semantic(&self, Parameters(params): Parameters<SemanticParams>) -> ToolResult {
		self.blocking(|context| context.search_semantic(params))
	}

	#[tool(description = "Searches atomic claims extracted from documents: what someone asserted, \
		decided, believed or planned, with attribution. Use it for questions like 'what did we decide \
		about X' or 'who said Y'; to find passages instead, use search_semantic or search_keyword. Each \
		claim names its document; open it with get_document to check the context. Only documents \
		processed by extract have claims (see stats).")]
	async fn search_claims(&self, Parameters(params): Parameters<ClaimsParams>) -> ToolResult {
		self.blocking(|context| context.search_claims(params))
	}

	#[tool(description = "Full text of one document by id: entries in order with headings and \
		authors, plus tags, project status and supersession. Long documents are cut at a character \
		budget; the response then gives the total size and the offset to continue from. If superseded \
		is true, prefer the document at current_document_id.")]
	async fn get_document(&self, Parameters(params): Parameters<GetDocumentParams>) -> ToolResult {
		self.blocking(|context| context.get_document(params))
	}

	#[tool(description = "Lists documents without a search term, newest first by default. Use it for \
		'latest', 'recent', 'what's new', 'where did I leave off' and 'what's in project X'. Rows carry \
		id, titles, clip date, doctype, project status, tags and a brief summary when one exists; open \
		documents with get_document.")]
	async fn list_documents(&self, Parameters(params): Parameters<ListParams>) -> ToolResult {
		self.blocking(|context| context.list_documents(params))
	}

	#[tool(description = "Registered curated projects with document counts by status. Call this \
		before using a project parameter: only these exact names are valid.")]
	async fn list_projects(&self) -> ToolResult {
		self.blocking(|context| context.list_projects())
	}

	#[tool(description = "Tags in use with document counts, including [includes] parent tags and \
		whether each is hidden by default. Use exact names from here for tag parameters.")]
	async fn list_tags(&self) -> ToolResult {
		self.blocking(|context| context.list_tags())
	}

	#[tool(description = "Corpus overview: document, chunk, embedding and claim counts, the clip date \
		range, doctypes, and whether semantic and claim search are available.")]
	async fn stats(&self) -> ToolResult {
		self.blocking(|context| context.stats())
	}
}

#[tool_handler(instructions = "what-was-said is the user's personal archive of work text, read-only \
	here. Captured documents (web clips, emails, Slack threads, Claude chats, notes) are kept as \
	evidence and never edited; when one was captured again later, the older versions are tagged \
	superseded. Curated project documents are files the user maintains, each with a status: canonical \
	(authoritative), provisional (draft), archived, or missing (file gone).\n\nSuperseded and other \
	hidden documents are left out of results by default. A result with superseded: true is an older \
	version; prefer the one at current_document_id. A document's clip_date is its latest capture; a search \
	hit's captured date, when shown, is when that passage first arrived.\n\nFor 'latest', 'recent' or 'where did I leave \
	off', call list_documents (newest first) rather than searching. Search results are short excerpts \
	for choosing documents; read the best two or three with get_document before answering. Take project \
	and tag values from list_projects and list_tags, never from the user's wording.\n\nBefore concluding \
	that something isn't there, broaden at least once: drop filters, or switch between search_keyword \
	and search_semantic.\n\nCite documents as [doc 42].")]
impl ServerHandler for WwsServer {}

pub fn run(context: McpContext) -> Result<()> {
	let runtime = tokio::runtime::Runtime::new()?;
	runtime.block_on(async {
		eprintln!("what-was-said mcp: serving on stdio");
		let service = WwsServer::new(context).serve(rmcp::transport::stdio()).await?;
		service.waiting().await?;
		Ok::<(), anyhow::Error>(())
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::chunking;
	use crate::minhash;
	use crate::types::{MergeStrategy, SegmentedEntry};

	struct FakeBackend {
		embedding: Option<Vec<f32>>,
	}

	impl LlmBackend for FakeBackend {
		fn generate(&self, _: &str, _: &str, _: Option<&str>, _: Option<&str>) -> Result<String> {
			anyhow::bail!("generation is not used by the MCP server")
		}

		fn embed(&self, _: &str, _: &str) -> Result<Vec<f32>> {
			self.embedding.clone().ok_or_else(|| anyhow::anyhow!("connection refused"))
		}
	}

	fn setup_db() -> Connection {
		unsafe {
			rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
				sqlite_vec::sqlite3_vec_init as *const (),
			)));
		}
		let connection = Connection::open_in_memory().unwrap();
		storage::initialize(&connection).unwrap();
		connection
	}

	fn insert_document(connection: &Connection, title: &str, body: &str, clip_date: &str) -> i64 {
		let entry = SegmentedEntry {
			start_line: 1,
			end_line: 1,
			body: body.to_string(),
			author: Some("Lauren Wallace".to_string()),
			timestamp: None,
			is_quote: false,
			heading_level: None,
			heading_title: None,
		};
		let hash = minhash::minhash(body);
		let document_id = storage::insert_document(
			connection, None, title, Some("slack"), MergeStrategy::None,
			Some("/test"), clip_date, None,
		).unwrap();
		let entry_id = storage::insert_entry(
			connection, document_id, &entry, 0, title, clip_date, "/test", &hash,
		).unwrap();
		storage::insert_chunks(connection, entry_id, &chunking::chunk_text(body)).unwrap();
		document_id.0
	}

	fn embed_all_chunks(connection: &Connection) {
		storage::ensure_vec_table(connection, 4).unwrap();
		let chunk_ids: Vec<i64> = connection.prepare("SELECT id FROM chunks").unwrap()
			.query_map([], |row| row.get(0)).unwrap()
			.collect::<std::result::Result<Vec<_>, _>>().unwrap();
		for chunk_id in chunk_ids {
			storage::insert_embedding(connection, chunk_id, &[1.0, 0.0, 0.0, 0.0]).unwrap();
		}
	}

	fn registration(name: &str) -> ProjectRegistration {
		ProjectRegistration {
			name: name.to_string(),
			manifest_path: std::path::PathBuf::from(format!("/p/{}/documents.toml", name)),
			root: std::path::PathBuf::from(format!("/p/{}", name)),
		}
	}

	fn context_with(connection: Connection, embedding: Option<Vec<f32>>, sizing: McpConfig) -> McpContext {
		let tag_config = TagConfig {
			default_exclude: vec!["superseded".to_string()],
			..Default::default()
		};
		McpContext::new(
			connection,
			Box::new(FakeBackend { embedding }),
			"test-embed".to_string(),
			tag_config,
			vec![registration("meetings")],
			sizing,
			":memory:".to_string(),
		)
	}

	fn context(connection: Connection) -> McpContext {
		context_with(connection, Some(vec![1.0, 0.0, 0.0, 0.0]), McpConfig::default())
	}

	fn parse(result: ToolResult) -> Value {
		serde_json::from_str(&result.unwrap()).unwrap()
	}

	fn keyword(query: &str) -> KeywordParams {
		KeywordParams { query: query.to_string(), ..Default::default() }
	}

	#[test]
	fn tool_schemas_serialize_with_project_guidance() {
		let schemas = [
			serde_json::to_value(schemars::schema_for!(KeywordParams)).unwrap(),
			serde_json::to_value(schemars::schema_for!(SemanticParams)).unwrap(),
			serde_json::to_value(schemars::schema_for!(ListParams)).unwrap(),
		];
		for schema in &schemas {
			let project = schema["properties"]["project"].to_string();
			assert!(project.contains("list_projects"), "{}", project);
			assert!(project.contains("tag"), "{}", project);
		}
		for schema in [
			serde_json::to_value(schemars::schema_for!(ClaimsParams)).unwrap(),
			serde_json::to_value(schemars::schema_for!(GetDocumentParams)).unwrap(),
		] {
			assert!(schema["properties"].is_object());
		}
	}

	#[test]
	fn keyword_search_caps_documents_hits_and_excerpt_length() {
		let db = setup_db();
		let filler = "filler words about nothing in particular ".repeat(8);
		for day in 1..=3 {
			insert_document(&db, &format!("thread {}", day), &format!("{}the otter surfaced at noon", filler), &format!("2026-10-0{} 09:00:00", day));
		}
		let sizing = McpConfig { search_documents: 2, hits_per_document: 1, hit_chars: 60, ..Default::default() };
		let context = context_with(db, None, sizing);

		let response = parse(context.search_keyword(keyword("otter")));
		let results = response["results"].as_array().unwrap();
		assert_eq!(results.len(), 2);
		assert_eq!(response["more_documents"], 1);
		for result in results {
			let hits = result["hits"].as_array().unwrap();
			assert_eq!(hits.len(), 1);
			let excerpt = hits[0]["excerpt"].as_str().unwrap();
			assert!(excerpt.chars().count() <= 62, "{}", excerpt);
			assert!(excerpt.contains("otter"), "anchor lost: {}", excerpt);
			assert_eq!(hits[0]["author"], "Lauren Wallace");
		}
	}

	#[test]
	fn superseded_fields_survive_search_and_document_truncation() {
		let db = setup_db();
		let old_version = insert_document(&db, "notes v1", &"walrus migration notes ".repeat(10), "2026-01-01 00:00:00");
		let new_version = insert_document(&db, "notes v2", "walrus migration notes, revised", "2026-02-01 00:00:00");
		storage::insert_document_relation(&db, new_version, old_version, "near_duplicate", 0.9, None, "superseded").unwrap();
		storage::add_tag(&db, old_version, "superseded").unwrap();
		let sizing = McpConfig { document_chars: 40, ..Default::default() };
		let context = context_with(db, None, sizing);

		let hidden = parse(context.search_keyword(keyword("walrus")));
		assert_eq!(hidden["results"].as_array().unwrap().len(), 1);

		let shown = parse(context.search_keyword(KeywordParams { include_hidden: Some(true), ..keyword("walrus") }));
		let old_hit = shown["results"].as_array().unwrap().iter()
			.find(|r| r["document_id"] == old_version).unwrap();
		assert_eq!(old_hit["superseded"], true);
		assert_eq!(old_hit["current_document_id"], new_version);

		let document = parse(context.get_document(GetDocumentParams { id: old_version, ..Default::default() }));
		assert_eq!(document["superseded"], true);
		assert_eq!(document["current_document_id"], new_version);
		assert!(document["truncation"].as_str().unwrap().contains("offset=40"));
	}

	#[test]
	fn get_document_states_size_and_continues_at_offset() {
		let db = setup_db();
		let body = "abcdefghij".repeat(12);
		let id = insert_document(&db, "long", &body, "2026-01-01 00:00:00");
		let sizing = McpConfig { document_chars: 100, ..Default::default() };
		let context = context_with(db, None, sizing);

		let first = parse(context.get_document(GetDocumentParams { id, ..Default::default() }));
		let total = first["total_chars"].as_u64().unwrap();
		let first_text = first["text"].as_str().unwrap();
		assert!(first["truncation"].as_str().unwrap().contains(&format!("of {}", total)));
		assert!(first_text.contains("[truncated:"));

		let second = parse(context.get_document(GetDocumentParams { id, offset: Some(100), ..Default::default() }));
		assert!(second.get("truncation").is_some());
		let second_text = second["text"].as_str().unwrap();
		assert!(!second_text.contains("[truncated:"));
		let rejoined = format!("{}{}", first_text.split("\n\n[truncated:").next().unwrap(), second_text);
		assert_eq!(rejoined.chars().count() as u64, total);
		assert!(rejoined.ends_with(&body));

		assert!(context.get_document(GetDocumentParams { id, offset: Some(total), ..Default::default() }).is_err());
		assert!(context.get_document(GetDocumentParams { id: id + 100, ..Default::default() }).unwrap_err().contains("No document"));
	}

	#[test]
	fn empty_results_name_the_applied_filters() {
		let db = setup_db();
		insert_document(&db, "thread", "zebra crossing", "2026-01-01 00:00:00");
		let context = context(db);
		let response = parse(context.search_keyword(KeywordParams { author: Some("nobody".into()), ..keyword("zebra") }));
		assert!(response["results"].as_array().unwrap().is_empty());
		let note = response["note"].as_str().unwrap();
		assert!(note.contains("author=\"nobody\""), "{}", note);
		assert!(note.contains("include_hidden"), "{}", note);
		assert_eq!(response["request"]["filters"]["author"], "nobody");
	}

	#[test]
	fn guessed_project_tag_and_date_values_are_tool_errors() {
		let db = setup_db();
		insert_document(&db, "thread", "montre strap", "2026-01-01 00:00:00");
		let context = context(db);
		let project_error = context.search_keyword(KeywordParams { project: Some("montre".into()), ..keyword("strap") }).unwrap_err();
		assert!(project_error.contains("not a registered curated project") && project_error.contains("meetings"), "{}", project_error);
		let tag_error = context.list_documents(ListParams { tag: Some("montre".into()), ..Default::default() }).unwrap_err();
		assert!(tag_error.contains("list_tags"), "{}", tag_error);
		let date_error = context.list_documents(ListParams { date_from: Some("last week".into()), ..Default::default() }).unwrap_err();
		assert!(date_error.contains("YYYY-MM-DD"), "{}", date_error);
		let blank_project = context.search_keyword(KeywordParams { project: Some("  ".into()), ..keyword("strap") });
		assert_eq!(parse(blank_project)["results"].as_array().unwrap().len(), 1);
	}

	#[test]
	fn semantic_tools_fail_clearly_without_ollama_while_keyword_and_listing_work() {
		let db = setup_db();
		insert_document(&db, "thread", "heron nesting season", "2026-01-01 00:00:00");
		embed_all_chunks(&db);
		let context = context_with(db, None, McpConfig::default());
		let error = context.search_semantic(SemanticParams { query: "birds".into(), ..Default::default() }).unwrap_err();
		assert!(error.contains("Ollama") && error.contains("search_keyword"), "{}", error);
		assert_eq!(parse(context.search_keyword(keyword("heron")))["results"].as_array().unwrap().len(), 1);
		assert_eq!(parse(context.list_documents(ListParams::default()))["documents"].as_array().unwrap().len(), 1);
	}

	#[test]
	fn semantic_search_needs_embeddings_and_then_returns_shaped_results() {
		let db = setup_db();
		insert_document(&db, "thread", "heron nesting season", "2026-01-01 00:00:00");
		let context = context(db);
		let missing = context.search_semantic(SemanticParams { query: "birds".into(), ..Default::default() }).unwrap_err();
		assert!(missing.contains("no chunk embeddings"), "{}", missing);
		assert!(context.search_claims(ClaimsParams { query: "birds".into(), ..Default::default() }).unwrap_err().contains("no claim embeddings"));

		embed_all_chunks(&context.connection.lock().unwrap());
		let response = parse(context.search_semantic(SemanticParams { query: "birds".into(), ..Default::default() }));
		let results = response["results"].as_array().unwrap();
		assert_eq!(results.len(), 1);
		assert_eq!(results[0]["superseded"], false);
		assert!(results[0]["hits"][0]["excerpt"].as_str().unwrap().contains("heron"));
	}

	#[test]
	fn list_documents_reports_more_and_explains_empty_results() {
		let db = setup_db();
		for day in 1..=3 {
			insert_document(&db, &format!("thread {}", day), "x", &format!("2026-10-0{} 09:00:00", day));
		}
		let context = context(db);
		let listed = parse(context.list_documents(ListParams { limit: Some(2), ..Default::default() }));
		let documents = listed["documents"].as_array().unwrap();
		assert_eq!(documents.len(), 2);
		assert_eq!(listed["more"], true);
		assert_eq!(documents[0]["clip_date"], "2026-10-03 09:00:00");
		assert_eq!(listed["request"]["sort"], "date");

		let empty = parse(context.list_documents(ListParams { doctype: Some("email".into()), ..Default::default() }));
		assert!(empty["note"].as_str().unwrap().contains("doctype=\"email\""));
	}

	#[test]
	fn discovery_tools_describe_the_corpus() {
		let db = setup_db();
		let id = insert_document(&db, "thread", "x", "2026-10-01 09:00:00");
		storage::add_tag(&db, id, "balsa").unwrap();
		let context = context(db);
		let projects = parse(context.list_projects());
		assert_eq!(projects["projects"][0]["name"], "meetings");
		assert_eq!(projects["projects"][0]["documents"], 0);
		let tags = parse(context.list_tags());
		assert_eq!(tags["tags"][0]["tag"], "balsa");
		let stats = parse(context.stats());
		assert_eq!(stats["documents"], 1);
		assert_eq!(stats["semantic_search_available"], false);
		assert_eq!(stats["doctypes"]["slack"], 1);
		assert_eq!(stats["clip_dates"]["newest"], "2026-10-01 09:00:00");
	}

	#[test]
	fn hits_captured_before_the_document_date_say_so() {
		let db = setup_db();
		let thread = insert_document(&db, "thread", "md5 checks on the zip packages", "2025-03-07 11:06:58");
		let entry = SegmentedEntry {
			start_line: 1,
			end_line: 1,
			body: "a later reply without the term".to_string(),
			author: None,
			timestamp: None,
			is_quote: false,
			heading_level: None,
			heading_title: None,
		};
		storage::insert_entry(
			&db, crate::types::DocumentId(thread), &entry, 1, "thread", "2025-03-07 13:33:17", "/test2",
			&minhash::minhash(&entry.body),
		).unwrap();
		storage::update_document_clip_date(&db, thread, "2025-03-07 13:33:17", "filename").unwrap();
		let context = context(db);

		let response = parse(context.search_keyword(keyword("md5")));
		let result = &response["results"][0];
		assert_eq!(result["clip_date"], "2025-03-07 13:33:17");
		assert_eq!(result["hits"][0]["captured"], "2025-03-07 11:06:58");
	}

	#[test]
	fn excerpt_places_anchor_and_marks_cuts() {
		let body = format!("{}needle{}", "a".repeat(100), "b".repeat(100));
		let text = excerpt(&body, Some(("needle", 0)), 30);
		assert!(text.contains("needle"));
		assert!(text.starts_with('…') && text.ends_with('…'));
		assert_eq!(text.chars().count(), 32);
		assert_eq!(excerpt("short", Some(("needle", 0)), 30), "short");
		assert!(excerpt(&body, None, 30).starts_with("aaa"));
		assert_eq!(
			snippet_anchor("\x01 the \x02otter\x03 surfaced\x01"),
			Some(("the otter surfaced".to_string(), 4)),
		);
		let far = format!("{}{}", "word ".repeat(30), "the target ends here");
		let anchored = excerpt(&far, Some(("word word the target", 14)), 20);
		assert!(anchored.contains("target"), "{}", anchored);
	}
}
