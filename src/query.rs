use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

use crate::storage;
use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchSortColumn {
	Score,
	Date,
}

#[derive(Serialize)]
pub struct GroupedSearchResult {
	pub document_id: i64,
	pub source_title: String,
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
}

impl SearchFilters {
	fn author_ref(&self) -> Option<&str> {
		self.author.as_deref()
	}
	fn date_from_ref(&self) -> Option<&str> {
		self.date_from.as_deref()
	}
	fn date_to_ref(&self) -> Option<&str> {
		self.date_to.as_deref()
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
	let rows = storage::raw_fts_search(
		connection, query, filters.author_ref(), filters.date_from_ref(), filters.date_to_ref(),
		filters.project_ref(), &filters.excluded_tags,
	)?;
	let mut results = group_fts_results(rows, sort_by);
	annotate_supersession(connection, &mut results)?;
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
	let chunks = storage::find_similar_chunks_filtered(
		connection, query_embedding, limit,
		filters.author_ref(), filters.date_from_ref(), filters.date_to_ref(),
		filters.project_ref(), &filters.excluded_tags,
	)?;
	let mut results = group_similar_results(chunks);
	annotate_supersession(connection, &mut results)?;
	Ok(results)
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
				clip_date: row.clip_date,
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
				clip_date: chunk.clip_date,
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
			},
			storage::SimilarChunk {
				chunk_id: 2, document_id: 1, source_title: "Doc A".into(),
				clip_date: "2024-01-01".into(), body: "second".into(),
				similarity: 0.7, author: None, entry_position: 1, chunk_index: 0,
				doc_status: None, project: None, start_char: 0, end_char: 0,
			},
			storage::SimilarChunk {
				chunk_id: 3, document_id: 2, source_title: "Doc B".into(),
				clip_date: "2024-02-01".into(), body: "other".into(),
				similarity: 0.8, author: None, entry_position: 0, chunk_index: 0,
				doc_status: None, project: None, start_char: 0, end_char: 0,
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
	use crate::types::{MergeStrategy, SegmentedEntry};

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
}
