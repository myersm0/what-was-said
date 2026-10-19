use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

use crate::storage;
use crate::util;

pub use crate::storage::{DocumentListOrder, FtsQueryError, ListedDocument};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchSortColumn {
	Score,
	Date,
}

#[derive(Serialize)]
pub struct GroupedSearchResult {
	pub document_id: i64,
	pub source_title: String,
	/// The document's latest capture, as in listings.
	pub clip_date: String,
	pub best_rank: f64,
	pub superseded: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub current_document_id: Option<i64>,
	pub chunks: Vec<ChunkHit>,
}

#[derive(Serialize)]
pub struct ChunkHit {
	pub entry_id: i64,
	pub entry_position: u32,
	pub chunk_index: u32,
	pub chunk_body: String,
	pub snippet: String,
	pub author: Option<String>,
	pub heading_title: Option<String>,
	pub rank: f64,
	/// When this entry was first captured. Search date filters apply to this,
	/// so a merged thread matches by when each message arrived.
	pub captured: String,
}

#[derive(Debug, Clone, Default)]
pub struct SearchFilters {
	pub author: Option<String>,
	pub date_from: Option<String>,
	pub date_to: Option<String>,
	pub project: Option<String>,
	/// Document tags to hide from results, already expanded through
	/// `TagConfig::expand_filter_tags`. Empty means no tag-based hiding
	/// (the `--include-all` / `include_hidden=true` case).
	pub excluded_tags: Vec<String>,
	/// The expanded form of one required tag (`TagConfig::expand_required_tag`):
	/// a document passes if it carries any of these. Empty means no requirement.
	pub required_tags: Vec<String>,
}

impl SearchFilters {
	fn author_ref(&self) -> Option<&str> {
		self.author.as_deref()
	}
	fn date_from_ref(&self) -> Option<&str> {
		self.date_from.as_deref()
	}
	fn date_to_bound(&self) -> Option<String> {
		self.date_to.as_deref().map(inclusive_upper_bound)
	}
	fn project_ref(&self) -> Option<&str> {
		self.project.as_deref()
	}
}

pub fn search(
	connection: &Connection,
	query: &str,
	sort_by: SearchSortColumn,
) -> Result<Vec<GroupedSearchResult>> {
	search_filtered(connection, query, sort_by, &SearchFilters::default())
}

pub fn search_filtered(
	connection: &Connection,
	query: &str,
	sort_by: SearchSortColumn,
	filters: &SearchFilters,
) -> Result<Vec<GroupedSearchResult>> {
	let date_to = filters.date_to_bound();
	let rows = storage::raw_fts_search(
		connection, query, filters.author_ref(), filters.date_from_ref(), date_to.as_deref(),
		filters.project_ref(), &filters.excluded_tags, &filters.required_tags,
	)?;
	let mut results = group_fts_results(rows, sort_by);
	annotate_supersession(connection, &mut results)?;
	if sort_by == SearchSortColumn::Score {
		demote_superseded(&mut results);
	}
	Ok(results)
}

pub fn find_similar_grouped(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
) -> Result<Vec<GroupedSearchResult>> {
	find_similar_grouped_filtered(connection, query_embedding, limit, &SearchFilters::default())
}

pub fn find_similar_grouped_filtered(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
	filters: &SearchFilters,
) -> Result<Vec<GroupedSearchResult>> {
	let date_to = filters.date_to_bound();
	let chunks = storage::find_similar_chunks_filtered(
		connection, query_embedding, limit,
		filters.author_ref(), filters.date_from_ref(), date_to.as_deref(),
		filters.project_ref(), &filters.excluded_tags, &filters.required_tags,
	)?;
	let mut results = group_similar_results(chunks);
	annotate_supersession(connection, &mut results)?;
	demote_superseded(&mut results);
	Ok(results)
}

/// Claims carry no author/date/project scoping; only the tag filters apply.
pub fn find_similar_claims_filtered(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
	filters: &SearchFilters,
) -> Result<Vec<storage::SimilarClaim>> {
	storage::find_similar_claims(
		connection, query_embedding, limit, &filters.excluded_tags, &filters.required_tags,
	)
}

/// A date-only upper bound covers that whole day: '~' sorts after any time part.
fn inclusive_upper_bound(date_to: &str) -> String {
	if date_to.len() == 10 {
		format!("{}~", date_to)
	} else {
		date_to.to_string()
	}
}

const BRIEF_SUMMARY_CHARS: usize = 300;

#[derive(Debug, Clone, Default)]
pub struct ListFilters {
	pub project: Option<String>,
	pub doctype: Option<String>,
	pub date_from: Option<String>,
	pub date_to: Option<String>,
	pub excluded_tags: Vec<String>,
	pub required_tags: Vec<String>,
}

pub fn list_documents(
	connection: &Connection,
	filters: &ListFilters,
	order: DocumentListOrder,
	limit: usize,
) -> Result<Vec<ListedDocument>> {
	let date_to = filters.date_to.as_deref().map(inclusive_upper_bound);
	let mut documents = storage::list_documents_filtered(connection, &storage::DocumentListQuery {
		project: filters.project.as_deref(),
		doctype: filters.doctype.as_deref(),
		date_from: filters.date_from.as_deref(),
		date_to: date_to.as_deref(),
		excluded_tags: &filters.excluded_tags,
		required_tags: &filters.required_tags,
		order,
		limit,
	})?;
	for document in documents.iter_mut() {
		if let Some(brief) = document.brief_summary.as_mut() {
			truncate_with_ellipsis(brief, BRIEF_SUMMARY_CHARS);
		}
		let status = storage::supersession_status(connection, document.id)?;
		document.superseded = status.superseded;
		document.current_document_id = status.current_document_id;
	}
	Ok(documents)
}

fn truncate_with_ellipsis(text: &mut String, max_chars: usize) {
	if let Some((byte_index, _)) = text.char_indices().nth(max_chars) {
		text.truncate(byte_index);
		text.push('…');
	}
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectListing {
	pub name: String,
	/// False for a project that has documents but is no longer in projects.toml.
	pub registered: bool,
	pub documents: i64,
	pub by_status: std::collections::BTreeMap<String, i64>,
}

pub fn list_projects(
	connection: &Connection,
	registrations: &[crate::projects::ProjectRegistration],
) -> Result<Vec<ProjectListing>> {
	let mut listings: Vec<ProjectListing> = registrations
		.iter()
		.map(|r| ProjectListing {
			name: r.name.clone(),
			registered: true,
			documents: 0,
			by_status: Default::default(),
		})
		.collect();
	for (project, status, count) in storage::project_status_counts(connection)? {
		let index = match listings.iter().position(|l| l.name == project) {
			Some(index) => index,
			None => {
				listings.push(ProjectListing {
					name: project,
					registered: false,
					documents: 0,
					by_status: Default::default(),
				});
				listings.len() - 1
			}
		};
		let listing = &mut listings[index];
		listing.documents += count;
		*listing.by_status.entry(status).or_insert(0) += count;
	}
	Ok(listings)
}

#[derive(Debug, Clone, Serialize)]
pub struct TagListing {
	pub tag: String,
	/// Documents carrying this exact tag.
	pub documents: i64,
	/// Children from `[includes]`; filtering by this tag also matches them.
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub includes: Vec<String>,
	/// Documents matched when filtering by this tag, children included.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub documents_with_includes: Option<i64>,
	pub hidden_by_default: bool,
}

/// Every tag in use plus every `[includes]` parent, even one no document
/// carries directly, so a filter value never has to be guessed.
pub fn list_tags(
	connection: &Connection,
	tag_config: &crate::config::TagConfig,
) -> Result<Vec<TagListing>> {
	let hidden = tag_config.expanded_default_exclude();
	let mut counts: std::collections::BTreeMap<String, i64> =
		storage::list_all_tags(connection)?.into_iter().collect();
	for parent in tag_config.includes.keys() {
		counts.entry(parent.clone()).or_insert(0);
	}
	let mut listings = Vec::with_capacity(counts.len());
	for (tag, documents) in counts {
		let includes = tag_config.includes.get(&tag).cloned().unwrap_or_default();
		let documents_with_includes = if includes.is_empty() {
			None
		} else {
			Some(storage::count_documents_with_any_tag(connection, &tag_config.expand_filter_tags(&[tag.clone()]))?)
		};
		listings.push(TagListing {
			hidden_by_default: hidden.contains(&tag),
			tag,
			documents,
			includes,
			documents_with_includes,
		});
	}
	Ok(listings)
}

/// Same scale as `status_weight`: an older version's near-identical text
/// should not outrank the current one when hidden documents are included.
const SUPERSEDED_WEIGHT: f64 = 0.5;

fn demote_superseded(results: &mut [GroupedSearchResult]) {
	if !results.iter().any(|r| r.superseded) {
		return;
	}
	for result in results.iter_mut().filter(|r| r.superseded) {
		result.best_rank = if result.best_rank < 0.0 {
			result.best_rank * SUPERSEDED_WEIGHT
		} else {
			result.best_rank / SUPERSEDED_WEIGHT
		};
	}
	results.sort_by(|a, b| a.best_rank.partial_cmp(&b.best_rank).unwrap_or(std::cmp::Ordering::Equal));
}

fn annotate_supersession(
	connection: &Connection,
	results: &mut [GroupedSearchResult],
) -> Result<()> {
	for result in results.iter_mut() {
		let status = storage::supersession_status(connection, result.document_id)?;
		result.superseded = status.superseded;
		result.current_document_id = status.current_document_id;
	}
	Ok(())
}

pub fn strip_fts_markers(results: &mut [GroupedSearchResult]) {
	for doc in results {
		for chunk in &mut doc.chunks {
			chunk.snippet = util::strip_fts_markers(&chunk.snippet);
		}
	}
}

pub fn group_fts_results(
	rows: Vec<storage::ChunkSearchResult>,
	sort_by: SearchSortColumn,
) -> Vec<GroupedSearchResult> {
	let mut grouped: Vec<GroupedSearchResult> = Vec::new();
	for row in rows {
		let weight = status_weight(row.doc_status.as_deref());
		let weighted_rank = row.rank * weight;
		let hit = ChunkHit {
			entry_id: row.entry_id,
			entry_position: row.entry_position,
			chunk_index: row.chunk_index,
			chunk_body: row.chunk_body,
			snippet: row.snippet,
			author: row.author,
			heading_title: row.heading_title,
			rank: row.rank,
			captured: row.clip_date,
		};
		match grouped.iter_mut().find(|d| d.document_id == row.document_id) {
			Some(doc) => {
				let dominated = doc.chunks.iter().any(|c| {
					(c.entry_id == hit.entry_id && c.chunk_index == hit.chunk_index)
						|| snippets_similar(&c.snippet, &hit.snippet)
				});
				if !dominated {
					doc.chunks.retain(|c| {
						!snippets_similar(&hit.snippet, &c.snippet) || hit.rank >= c.rank
					});
					if weighted_rank < doc.best_rank {
						doc.best_rank = weighted_rank;
					}
					doc.chunks.push(hit);
				}
			}
			None => grouped.push(GroupedSearchResult {
				document_id: row.document_id,
				source_title: row.source_title,
				clip_date: row.document_clip_date,
				best_rank: weighted_rank,
				superseded: false,
				current_document_id: None,
				chunks: vec![hit],
			}),
		}
	}

	for doc in &mut grouped {
		doc.chunks.sort_by_key(|c| (c.entry_position, c.chunk_index));
	}

	match sort_by {
		SearchSortColumn::Score => {
			grouped.sort_by(|a, b| {
				a.best_rank.partial_cmp(&b.best_rank).unwrap_or(std::cmp::Ordering::Equal)
			});
		}
		SearchSortColumn::Date => {
			grouped.sort_by(|a, b| b.clip_date.cmp(&a.clip_date));
		}
	}

	grouped
}

pub fn group_similar_results(
	chunks: Vec<storage::SimilarChunk>,
) -> Vec<GroupedSearchResult> {
	let mut grouped: Vec<GroupedSearchResult> = Vec::new();
	for chunk in chunks {
		let rank = -(chunk.similarity as f64);
		let weight = status_weight(chunk.doc_status.as_deref());
		let weighted_rank = rank * weight;
		let hit = ChunkHit {
			entry_id: 0,
			entry_position: chunk.entry_position,
			chunk_index: chunk.chunk_index,
			chunk_body: chunk.body.clone(),
			snippet: chunk.body.chars().take(150).collect(),
			author: chunk.author,
			heading_title: None,
			rank,
			captured: chunk.clip_date,
		};

		match grouped.iter_mut().find(|d| d.document_id == chunk.document_id) {
			Some(doc) => {
				if weighted_rank < doc.best_rank {
					doc.best_rank = weighted_rank;
				}
				doc.chunks.push(hit);
			}
			None => grouped.push(GroupedSearchResult {
				document_id: chunk.document_id,
				source_title: chunk.source_title,
				clip_date: chunk.document_clip_date,
				best_rank: weighted_rank,
				superseded: false,
				current_document_id: None,
				chunks: vec![hit],
			}),
		}
	}

	for doc in &mut grouped {
		doc.chunks.sort_by(|a, b| {
			a.rank.partial_cmp(&b.rank).unwrap_or(std::cmp::Ordering::Equal)
		});
	}
	grouped.sort_by(|a, b| {
		a.best_rank.partial_cmp(&b.best_rank).unwrap_or(std::cmp::Ordering::Equal)
	});

	grouped
}

fn status_weight(status: Option<&str>) -> f64 {
	match status {
		Some("provisional") => 0.8,
		Some("archived") => 0.5,
		Some("missing") => 0.3,
		_ => 1.0,
	}
}

fn snippets_similar(a: &str, b: &str) -> bool {
	let a_clean = a.trim().replace('\x01', "").replace('\x02', "").replace('\x03', "");
	let b_clean = b.trim().replace('\x01', "").replace('\x02', "").replace('\x03', "");

	if a_clean == b_clean {
		return true;
	}

	let a_words: Vec<&str> = a_clean.split_whitespace().collect();
	let b_words: Vec<&str> = b_clean.split_whitespace().collect();

	if a_words.len() < 5 || b_words.len() < 5 {
		return false;
	}

	let overlap = a_words.iter().filter(|w| b_words.contains(w)).count();
	let min_len = a_words.len().min(b_words.len());

	overlap as f64 / min_len as f64 > 0.8
}

#[cfg(test)]
mod tests {
	use super::*;

	fn make_row(doc_id: i64, title: &str, date: &str, snippet: &str, rank: f64) -> storage::ChunkSearchResult {
		storage::ChunkSearchResult {
			chunk_id: 1,
			entry_id: 1,
			document_id: doc_id,
			chunk_body: snippet.to_string(),
			snippet: snippet.to_string(),
			chunk_index: 0,
			entry_position: 0,
			author: None,
			source_title: title.to_string(),
			clip_date: date.to_string(),
			heading_title: None,
			rank,
			doc_status: None,
			document_clip_date: date.to_string(),
		}
	}

	#[test]
	fn groups_by_document() {
		let rows = vec![
			make_row(1, "Doc A", "2024-01-01", "first hit", -5.0),
			{
				let mut r = make_row(1, "Doc A", "2024-01-01", "second hit", -3.0);
				r.entry_id = 2;
				r
			},
			make_row(2, "Doc B", "2024-02-01", "other hit", -4.0),
		];
		let grouped = group_fts_results(rows, SearchSortColumn::Score);
		assert_eq!(grouped.len(), 2);
		assert_eq!(grouped[0].document_id, 1);
		assert_eq!(grouped[0].chunks.len(), 2);
		assert_eq!(grouped[1].document_id, 2);
	}

	#[test]
	fn sorts_by_date() {
		let rows = vec![
			make_row(1, "Old", "2024-01-01", "hit", -5.0),
			make_row(2, "New", "2024-06-01", "hit", -3.0),
		];
		let grouped = group_fts_results(rows, SearchSortColumn::Date);
		assert_eq!(grouped[0].source_title, "New");
	}

	#[test]
	fn deduplicates_similar_snippets() {
		let rows = vec![
			make_row(1, "Doc", "2024-01-01", "the quick brown fox jumps over the lazy dog", -5.0),
			make_row(1, "Doc", "2024-01-01", "the quick brown fox jumps over the lazy cat", -3.0),
		];
		let grouped = group_fts_results(rows, SearchSortColumn::Score);
		assert_eq!(grouped[0].chunks.len(), 1);
	}

	#[test]
	fn groups_similar_chunks() {
		let chunks = vec![
			storage::SimilarChunk {
				chunk_id: 1, document_id: 1, source_title: "Doc A".into(),
				clip_date: "2024-01-01".into(), body: "first".into(),
				similarity: 0.9, author: None, entry_position: 0, chunk_index: 0,
				doc_status: None, project: None, start_char: 0, end_char: 0,
				document_clip_date: "2024-02-01".into(),
			},
			storage::SimilarChunk {
				chunk_id: 2, document_id: 1, source_title: "Doc A".into(),
				clip_date: "2024-01-01".into(), body: "second".into(),
				similarity: 0.7, author: None, entry_position: 1, chunk_index: 0,
				doc_status: None, project: None, start_char: 0, end_char: 0,
				document_clip_date: "2024-02-01".into(),
			},
			storage::SimilarChunk {
				chunk_id: 3, document_id: 2, source_title: "Doc B".into(),
				clip_date: "2024-02-01".into(), body: "other".into(),
				similarity: 0.8, author: None, entry_position: 0, chunk_index: 0,
				doc_status: None, project: None, start_char: 0, end_char: 0,
				document_clip_date: "2024-02-01".into(),
			},
		];
		let grouped = group_similar_results(chunks);
		assert_eq!(grouped.len(), 2);
		assert_eq!(grouped[0].document_id, 1);
		assert_eq!(grouped[0].chunks.len(), 2);
	}

	#[test]
	fn snippets_similar_exact_match() {
		assert!(snippets_similar("hello world", "hello world"));
	}

	#[test]
	fn snippets_similar_high_overlap() {
		assert!(snippets_similar(
			"the quick brown fox jumps over lazy dogs today",
			"the quick brown fox jumps over lazy cats today",
		));
	}

	#[test]
	fn snippets_similar_low_overlap() {
		assert!(!snippets_similar(
			"the quick brown fox jumps over lazy dogs today",
			"a completely different sentence about something else entirely",
		));
	}

	#[test]
	fn snippets_similar_short_strings() {
		assert!(!snippets_similar("hello", "hello world"));
	}
}

#[cfg(test)]
mod visibility_tests {
	use super::*;
	use crate::chunking;
	use crate::minhash;
	use crate::types::{DocumentId, MergeStrategy, SegmentedEntry};

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
			author: None,
			timestamp: None,
			is_quote: false,
			heading_level: None,
			heading_title: None,
		};
		let hash = minhash::minhash(body);
		let document_id = storage::insert_document(
			connection, None, title, Some("test"), MergeStrategy::None,
			Some("/test"), clip_date, None,
		).unwrap();
		let entry_id = storage::insert_entry(
			connection, document_id, &entry, 0, title, clip_date, "/test", &hash,
		).unwrap();
		storage::insert_chunks(connection, entry_id, &chunking::chunk_text(body)).unwrap();
		document_id.0
	}

	fn make_family(connection: &Connection, newer: i64, older: i64) {
		storage::insert_document_relation(
			connection, newer, older, "near_duplicate", 0.9, None, "superseded",
		).unwrap();
		storage::add_tag(connection, older, "superseded").unwrap();
	}

	fn excluded(tags: &[&str]) -> SearchFilters {
		SearchFilters {
			excluded_tags: tags.iter().map(|t| t.to_string()).collect(),
			..Default::default()
		}
	}

	#[test]
	fn fts_search_hides_superseded_by_default_and_annotates_when_included() {
		let db = setup_db();
		let old_version = insert_document(&db, "note v1", "walrus migration patterns", "2024-01-01 00:00:00");
		let new_version = insert_document(&db, "note v2", "walrus migration patterns revised", "2024-02-01 00:00:00");
		make_family(&db, new_version, old_version);

		let hidden = search_filtered(&db, "walrus", SearchSortColumn::Score, &excluded(&["superseded"])).unwrap();
		assert_eq!(hidden.len(), 1);
		assert_eq!(hidden[0].document_id, new_version);
		assert!(!hidden[0].superseded);
		assert!(hidden[0].current_document_id.is_none());

		let shown = search_filtered(&db, "walrus", SearchSortColumn::Score, &excluded(&[])).unwrap();
		assert_eq!(shown.len(), 2);
		let old_result = shown.iter().find(|r| r.document_id == old_version).unwrap();
		assert!(old_result.superseded);
		assert_eq!(old_result.current_document_id, Some(new_version));
		let new_result = shown.iter().find(|r| r.document_id == new_version).unwrap();
		assert!(!new_result.superseded);
		assert!(new_result.current_document_id.is_none());
	}

	#[test]
	fn semantic_search_hides_superseded_by_default_and_annotates_when_included() {
		let db = setup_db();
		let old_version = insert_document(&db, "note v1", "short body", "2024-01-01 00:00:00");
		let new_version = insert_document(&db, "note v2", "short body revised", "2024-02-01 00:00:00");
		make_family(&db, new_version, old_version);

		storage::ensure_vec_table(&db, 4).unwrap();
		let mut chunk_ids = db.prepare(
			"SELECT c.id, e.document_id FROM chunks c JOIN entries e ON e.id = c.entry_id",
		).unwrap()
			.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
			.unwrap()
			.collect::<std::result::Result<Vec<_>, _>>()
			.unwrap();
		chunk_ids.sort();
		for (chunk_id, document_id) in &chunk_ids {
			let embedding = if *document_id == old_version {
				vec![1.0, 0.0, 0.0, 0.0]
			} else {
				vec![0.9, 0.1, 0.0, 0.0]
			};
			storage::insert_embedding(&db, *chunk_id, &embedding).unwrap();
		}
		let query_embedding = vec![1.0, 0.0, 0.0, 0.0];

		let hidden = find_similar_grouped_filtered(&db, &query_embedding, 10, &excluded(&["superseded"])).unwrap();
		assert_eq!(hidden.len(), 1);
		assert_eq!(hidden[0].document_id, new_version);

		let shown = find_similar_grouped_filtered(&db, &query_embedding, 10, &excluded(&[])).unwrap();
		assert_eq!(shown.len(), 2);
		let old_result = shown.iter().find(|r| r.document_id == old_version).unwrap();
		assert!(old_result.superseded);
		assert_eq!(old_result.current_document_id, Some(new_version));
	}

	#[test]
	fn any_excluded_tag_hides_documents_through_the_same_mechanism() {
		let db = setup_db();
		let junk_document = insert_document(&db, "junk note", "pelican sighting log", "2024-01-01 00:00:00");
		let kept_document = insert_document(&db, "real note", "pelican sighting report", "2024-02-01 00:00:00");
		storage::add_tag(&db, junk_document, "junk").unwrap();

		let results = search_filtered(&db, "pelican", SearchSortColumn::Score, &excluded(&["junk"])).unwrap();
		assert_eq!(results.len(), 1);
		assert_eq!(results[0].document_id, kept_document);
		assert!(!results[0].superseded);
	}

	#[test]
	fn project_scoped_semantic_search_applies_exclusion() {
		let db = setup_db();
		let hidden_doc = insert_document(&db, "proj a", "heron notes", "2024-01-01 00:00:00");
		let shown_doc = insert_document(&db, "proj b", "heron notes more", "2024-02-01 00:00:00");
		db.execute("UPDATE documents SET project = 'birds'", []).unwrap();
		storage::add_tag(&db, hidden_doc, "junk").unwrap();

		storage::ensure_vec_table(&db, 4).unwrap();
		let chunk_rows: Vec<(i64, i64)> = db.prepare(
			"SELECT c.id, e.document_id FROM chunks c JOIN entries e ON e.id = c.entry_id",
		).unwrap()
			.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
			.unwrap()
			.collect::<std::result::Result<Vec<_>, _>>()
			.unwrap();
		for (chunk_id, _) in &chunk_rows {
			storage::insert_embedding(&db, *chunk_id, &[1.0, 0.0, 0.0, 0.0]).unwrap();
		}

		let filters = SearchFilters {
			project: Some("birds".to_string()),
			excluded_tags: vec!["junk".to_string()],
			..Default::default()
		};
		let results = find_similar_grouped_filtered(&db, &[1.0, 0.0, 0.0, 0.0], 10, &filters).unwrap();
		assert_eq!(results.len(), 1);
		assert_eq!(results[0].document_id, shown_doc);
	}

	fn embed_all_chunks(db: &Connection, embedding_for: impl Fn(i64) -> Vec<f32>) {
		storage::ensure_vec_table(db, 4).unwrap();
		let chunk_rows: Vec<(i64, i64)> = db.prepare(
			"SELECT c.id, e.document_id FROM chunks c JOIN entries e ON e.id = c.entry_id",
		).unwrap()
			.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
			.unwrap()
			.collect::<std::result::Result<Vec<_>, _>>()
			.unwrap();
		for (chunk_id, document_id) in &chunk_rows {
			storage::insert_embedding(db, *chunk_id, &embedding_for(*document_id)).unwrap();
		}
	}

	fn tag_config_with_includes(parent: &str, children: &[&str]) -> crate::config::TagConfig {
		let mut includes = std::collections::HashMap::new();
		includes.insert(parent.to_string(), children.iter().map(|c| c.to_string()).collect());
		crate::config::TagConfig { includes, ..Default::default() }
	}

	#[test]
	fn keyword_search_requires_tag_through_includes_expansion() {
		let db = setup_db();
		insert_document(&db, "loose note", "ferret husbandry", "2024-01-01 00:00:00");
		let child_tagged = insert_document(&db, "x note", "ferret husbandry", "2024-02-01 00:00:00");
		storage::add_tag(&db, child_tagged, "x-backend").unwrap();

		let tag_config = tag_config_with_includes("project-x", &["x-frontend", "x-backend"]);
		let filters = SearchFilters {
			required_tags: tag_config.expand_required_tag(" Project-X "),
			..Default::default()
		};
		let results = search_filtered(&db, "ferret", SearchSortColumn::Score, &filters).unwrap();
		assert_eq!(results.len(), 1);
		assert_eq!(results[0].document_id, child_tagged);
	}

	#[test]
	fn required_and_excluded_tags_combine() {
		let db = setup_db();
		let hidden = insert_document(&db, "old", "badger setts", "2024-01-01 00:00:00");
		let shown = insert_document(&db, "new", "badger setts", "2024-02-01 00:00:00");
		for id in [hidden, shown] {
			storage::add_tag(&db, id, "fieldwork").unwrap();
		}
		storage::add_tag(&db, hidden, "junk").unwrap();

		let filters = SearchFilters {
			excluded_tags: vec!["junk".to_string()],
			required_tags: vec!["fieldwork".to_string()],
			..Default::default()
		};
		let results = search_filtered(&db, "badger", SearchSortColumn::Score, &filters).unwrap();
		assert_eq!(results.len(), 1);
		assert_eq!(results[0].document_id, shown);
	}

	#[test]
	fn semantic_search_requires_tag_without_project() {
		let db = setup_db();
		let untagged = insert_document(&db, "near", "stoat notes", "2024-01-01 00:00:00");
		let tagged = insert_document(&db, "far", "stoat notes again", "2024-02-01 00:00:00");
		storage::add_tag(&db, tagged, "lab-meeting").unwrap();
		embed_all_chunks(&db, |document_id| {
			if document_id == untagged { vec![1.0, 0.0, 0.0, 0.0] } else { vec![0.5, 0.5, 0.0, 0.0] }
		});

		let filters = SearchFilters { required_tags: vec!["lab-meeting".to_string()], ..Default::default() };
		let results = find_similar_grouped_filtered(&db, &[1.0, 0.0, 0.0, 0.0], 1, &filters).unwrap();
		assert_eq!(results.len(), 1);
		assert_eq!(results[0].document_id, tagged);
		assert!(!results[0].superseded);
	}

	#[test]
	fn punctuated_keywords_match_literally_and_empty_input_is_a_typed_error() {
		let db = setup_db();
		let service = insert_document(&db, "svc", "run it as nrg-svc-hcpx on hcpx-fs01", "2024-01-01 00:00:00");
		let colon = insert_document(&db, "montre", "the project:montre plan and Mike's notes", "2024-01-01 00:00:00");
		let filters = SearchFilters::default();
		for (query, expected) in [
			("nrg-svc-hcpx", service),
			("hcpx-fs", service),
			("project:montre", colon),
			("Mike's", colon),
			("\"project montre", colon),
			("\"project montre\"", colon),
		] {
			let results = search_filtered(&db, query, SearchSortColumn::Score, &filters)
				.unwrap_or_else(|e| panic!("{:?} failed: {}", query, e));
			assert_eq!(results.len(), 1, "query {:?}", query);
			assert_eq!(results[0].document_id, expected, "query {:?}", query);
		}
		assert!(search_filtered(&db, "OR", SearchSortColumn::Score, &filters).unwrap().is_empty());

		let error = search_filtered(&db, "  ", SearchSortColumn::Score, &filters).err().unwrap();
		assert!(error.downcast_ref::<FtsQueryError>().is_some(), "got {}", error);
	}

	#[test]
	fn fts_expression_quotes_words_and_keeps_phrases() {
		assert_eq!(storage::fts_match_expression("nrg-svc-hcpx md5"), "\"nrg-svc-hcpx\"* \"md5\"*");
		assert_eq!(storage::fts_match_expression("\"BALSA packages\" zip"), "\"BALSA packages\" \"zip\"*");
		assert_eq!(storage::fts_match_expression("\"dangling"), "\"dangling\"*");
		assert_eq!(storage::fts_match_expression("\"\" x"), "\"x\"*");
	}

	fn listed_ids(db: &Connection, filters: &ListFilters, order: DocumentListOrder, limit: usize) -> Vec<i64> {
		list_documents(db, filters, order, limit).unwrap().iter().map(|d| d.id).collect()
	}

	#[test]
	fn listing_sorts_by_date_both_ways_and_by_title_and_honors_limit() {
		let db = setup_db();
		let middle = insert_document(&db, "alpha", "x", "2024-02-01 00:00:00");
		let oldest = insert_document(&db, "Charlie", "x", "2024-01-01 00:00:00");
		let newest = insert_document(&db, "Bravo", "x", "2024-03-01 00:00:00");
		let all = ListFilters::default();
		assert_eq!(listed_ids(&db, &all, DocumentListOrder::DateDescending, 10), vec![newest, middle, oldest]);
		assert_eq!(listed_ids(&db, &all, DocumentListOrder::DateAscending, 10), vec![oldest, middle, newest]);
		assert_eq!(listed_ids(&db, &all, DocumentListOrder::Title, 10), vec![middle, newest, oldest]);
		assert_eq!(listed_ids(&db, &all, DocumentListOrder::DateDescending, 2), vec![newest, middle]);
	}

	#[test]
	fn listing_filters_alone_and_combined() {
		let db = setup_db();
		let meeting = insert_document(&db, "2026-10-14-lab-sync.md", "x", "2026-10-14 09:00:00");
		let other_meeting = insert_document(&db, "2026-10-07-lab-sync.md", "x", "2026-10-07 09:00:00");
		let email = insert_document(&db, "re: balsa", "x", "2026-10-14 15:00:00");
		db.execute("UPDATE documents SET project = 'meetings', doc_status = 'canonical' WHERE id IN (?1, ?2)", [meeting, other_meeting]).unwrap();
		db.execute("UPDATE documents SET doctype_name = 'email' WHERE id = ?1", [email]).unwrap();
		storage::add_tag(&db, email, "balsa").unwrap();
		storage::add_tag(&db, other_meeting, "balsa").unwrap();
		let order = DocumentListOrder::DateDescending;

		let by_project = ListFilters { project: Some("meetings".into()), ..Default::default() };
		assert_eq!(listed_ids(&db, &by_project, order, 10), vec![meeting, other_meeting]);
		let by_doctype = ListFilters { doctype: Some("email".into()), ..Default::default() };
		assert_eq!(listed_ids(&db, &by_doctype, order, 10), vec![email]);
		let by_tag = ListFilters { required_tags: vec!["balsa".into()], ..Default::default() };
		assert_eq!(listed_ids(&db, &by_tag, order, 10), vec![email, other_meeting]);
		let one_day = ListFilters {
			date_from: Some("2026-10-14".into()),
			date_to: Some("2026-10-14".into()),
			..Default::default()
		};
		assert_eq!(listed_ids(&db, &one_day, order, 10), vec![email, meeting]);
		let combined = ListFilters {
			project: Some("meetings".into()),
			required_tags: vec!["balsa".into()],
			..Default::default()
		};
		assert_eq!(listed_ids(&db, &combined, order, 10), vec![other_meeting]);

		let listed = list_documents(&db, &by_project, order, 1).unwrap();
		assert_eq!(listed[0].project.as_deref(), Some("meetings"));
		assert_eq!(listed[0].doc_status.as_deref(), Some("canonical"));
	}

	#[test]
	fn listing_hides_by_default_and_annotates_when_included() {
		let db = setup_db();
		let old_version = insert_document(&db, "note v1", "x", "2024-01-01 00:00:00");
		let new_version = insert_document(&db, "note v2", "x", "2024-02-01 00:00:00");
		make_family(&db, new_version, old_version);
		let order = DocumentListOrder::DateDescending;

		let hidden = ListFilters { excluded_tags: vec!["superseded".into()], ..Default::default() };
		let listed = list_documents(&db, &hidden, order, 10).unwrap();
		assert_eq!(listed.len(), 1);
		assert_eq!(listed[0].id, new_version);
		assert!(!listed[0].superseded);

		let shown = list_documents(&db, &ListFilters::default(), order, 10).unwrap();
		let old_listing = shown.iter().find(|d| d.id == old_version).unwrap();
		assert!(old_listing.superseded);
		assert_eq!(old_listing.current_document_id, Some(new_version));
		assert!(old_listing.tags.contains(&"superseded".to_string()));
	}

	#[test]
	fn date_only_upper_bound_covers_the_whole_day_in_search() {
		let db = setup_db();
		let afternoon = insert_document(&db, "late note", "marmot burrow", "2024-02-01 15:30:00");
		let filters = SearchFilters { date_to: Some("2024-02-01".into()), ..Default::default() };
		let results = search_filtered(&db, "marmot", SearchSortColumn::Score, &filters).unwrap();
		assert_eq!(results.len(), 1);
		assert_eq!(results[0].document_id, afternoon);
	}

	#[test]
	fn truncation_counts_characters_and_marks_the_cut() {
		let mut short = "déjà vu".to_string();
		truncate_with_ellipsis(&mut short, 7);
		assert_eq!(short, "déjà vu");
		let mut long = "déjà vu again".to_string();
		truncate_with_ellipsis(&mut long, 4);
		assert_eq!(long, "déjà…");
	}

	#[test]
	fn project_listing_counts_by_status_and_flags_unregistered() {
		let db = setup_db();
		let registration = |name: &str| crate::projects::ProjectRegistration {
			name: name.to_string(),
			manifest_path: std::path::PathBuf::from(format!("/p/{}/documents.toml", name)),
			root: std::path::PathBuf::from(format!("/p/{}", name)),
		};
		for (title, project, status) in [
			("a", "meetings", "canonical"),
			("b", "meetings", "canonical"),
			("c", "meetings", "missing"),
			("d", "retired", "provisional"),
		] {
			let id = insert_document(&db, title, "x", "2024-01-01 00:00:00");
			db.execute(
				"UPDATE documents SET project = ?1, doc_status = ?2 WHERE id = ?3",
				rusqlite::params![project, status, id],
			).unwrap();
		}
		insert_document(&db, "captured", "x", "2024-01-01 00:00:00");

		let listings = list_projects(&db, &[registration("empty"), registration("meetings")]).unwrap();
		let find = |name: &str| listings.iter().find(|l| l.name == name).unwrap();
		assert_eq!(listings.len(), 3);
		assert!(find("empty").registered);
		assert_eq!(find("empty").documents, 0);
		assert_eq!(find("meetings").documents, 3);
		assert_eq!(find("meetings").by_status.get("canonical"), Some(&2));
		assert_eq!(find("meetings").by_status.get("missing"), Some(&1));
		assert!(!find("retired").registered);
	}

	#[test]
	fn tag_listing_includes_parents_and_marks_hidden() {
		let db = setup_db();
		let frontend = insert_document(&db, "f", "x", "2024-01-01 00:00:00");
		let backend = insert_document(&db, "b", "x", "2024-01-01 00:00:00");
		storage::add_tag(&db, frontend, "x-frontend").unwrap();
		storage::add_tag(&db, backend, "x-backend").unwrap();
		storage::add_tag(&db, backend, "superseded").unwrap();
		let mut tag_config = tag_config_with_includes("project-x", &["x-frontend", "x-backend"]);
		tag_config.default_exclude = vec!["superseded".to_string()];

		let listings = list_tags(&db, &tag_config).unwrap();
		let find = |tag: &str| listings.iter().find(|l| l.tag == tag).unwrap();
		assert_eq!(find("project-x").documents, 0);
		assert_eq!(find("project-x").documents_with_includes, Some(2));
		assert_eq!(find("project-x").includes, vec!["x-frontend", "x-backend"]);
		assert_eq!(find("x-frontend").documents, 1);
		assert!(find("x-frontend").documents_with_includes.is_none());
		assert!(find("superseded").hidden_by_default);
		assert!(!find("x-backend").hidden_by_default);
	}

	/// A positional merge: a later clip appends an entry and moves the
	/// document's clip_date forward.
	fn append_clip(connection: &Connection, document_id: i64, position: u32, body: &str, clip_date: &str) {
		let entry = SegmentedEntry {
			start_line: 1,
			end_line: 1,
			body: body.to_string(),
			author: None,
			timestamp: None,
			is_quote: false,
			heading_level: None,
			heading_title: None,
		};
		let entry_id = storage::insert_entry(
			connection, DocumentId(document_id), &entry, position, "thread", clip_date, "/test2", &minhash::minhash(body),
		).unwrap();
		storage::insert_chunks(connection, entry_id, &chunking::chunk_text(body)).unwrap();
		storage::update_document_clip_date(connection, document_id, clip_date, "filename").unwrap();
	}

	#[test]
	fn search_reports_document_date_and_filters_by_entry_capture() {
		let db = setup_db();
		let thread = insert_document(&db, "thread", "lemurs groom each other at dawn", "2026-04-07 15:42:00");
		append_clip(&db, thread, 1, "the keeper fed the lemurs bananas", "2026-04-07 17:02:08");

		let results = search_filtered(&db, "lemurs", SearchSortColumn::Score, &SearchFilters::default()).unwrap();
		assert_eq!(results.len(), 1);
		assert_eq!(results[0].clip_date, "2026-04-07 17:02:08");
		let mut captured: Vec<&str> = results[0].chunks.iter().map(|c| c.captured.as_str()).collect();
		captured.sort();
		assert_eq!(captured, vec!["2026-04-07 15:42:00", "2026-04-07 17:02:08"]);

		let early = SearchFilters { date_to: Some("2026-04-07 16:00:00".into()), ..Default::default() };
		let results = search_filtered(&db, "lemurs", SearchSortColumn::Score, &early).unwrap();
		assert_eq!(results[0].chunks.len(), 1);
		assert_eq!(results[0].chunks[0].captured, "2026-04-07 15:42:00");
		assert_eq!(results[0].clip_date, "2026-04-07 17:02:08");

		embed_all_chunks(&db, |_| vec![1.0, 0.0, 0.0, 0.0]);
		let similar = find_similar_grouped_filtered(&db, &[1.0, 0.0, 0.0, 0.0], 5, &SearchFilters::default()).unwrap();
		assert_eq!(similar[0].clip_date, "2026-04-07 17:02:08");
	}

	#[test]
	fn current_version_outranks_identical_superseded_version() {
		let db = setup_db();
		let old_version = insert_document(&db, "notes v1", "capybara census results", "2024-01-01 00:00:00");
		let new_version = insert_document(&db, "notes v2", "capybara census results", "2024-02-01 00:00:00");
		make_family(&db, new_version, old_version);

		let keyword = search_filtered(&db, "capybara", SearchSortColumn::Score, &SearchFilters::default()).unwrap();
		assert_eq!(keyword.iter().map(|r| r.document_id).collect::<Vec<_>>(), vec![new_version, old_version]);

		embed_all_chunks(&db, |_| vec![1.0, 0.0, 0.0, 0.0]);
		let semantic = find_similar_grouped_filtered(&db, &[1.0, 0.0, 0.0, 0.0], 5, &SearchFilters::default()).unwrap();
		assert_eq!(semantic.iter().map(|r| r.document_id).collect::<Vec<_>>(), vec![new_version, old_version]);
	}
}
