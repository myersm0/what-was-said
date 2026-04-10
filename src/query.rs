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

pub fn search(
	connection: &Connection,
	query: &str,
	sort_by: SearchSortColumn,
) -> Result<Vec<GroupedSearchResult>> {
	search_filtered(connection, query, sort_by, None, None, None)
}

pub fn search_filtered(
	connection: &Connection,
	query: &str,
	sort_by: SearchSortColumn,
	author_like: Option<&str>,
	date_from: Option<&str>,
	date_to: Option<&str>,
) -> Result<Vec<GroupedSearchResult>> {
	let rows = storage::raw_fts_search(connection, query, author_like, date_from, date_to)?;
	Ok(group_fts_results(rows, sort_by))
}

pub fn find_similar_grouped(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
) -> Result<Vec<GroupedSearchResult>> {
	let chunks = storage::find_similar_chunks(connection, query_embedding, limit)?;
	Ok(group_similar_results(chunks))
}

pub fn find_similar_grouped_filtered(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
	author_like: Option<&str>,
	date_from: Option<&str>,
	date_to: Option<&str>,
) -> Result<Vec<GroupedSearchResult>> {
	let chunks = storage::find_similar_chunks_filtered(
		connection, query_embedding, limit, author_like, date_from, date_to,
	)?;
	Ok(group_similar_results(chunks))
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
					if hit.rank < doc.best_rank {
						doc.best_rank = hit.rank;
					}
					doc.chunks.push(hit);
				}
			}
			None => grouped.push(GroupedSearchResult {
				document_id: row.document_id,
				source_title: row.source_title,
				clip_date: row.clip_date,
				best_rank: hit.rank,
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
				if rank < doc.best_rank {
					doc.best_rank = rank;
				}
				doc.chunks.push(hit);
			}
			None => grouped.push(GroupedSearchResult {
				document_id: chunk.document_id,
				source_title: chunk.source_title,
				clip_date: chunk.clip_date,
				best_rank: rank,
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
			},
			storage::SimilarChunk {
				chunk_id: 2, document_id: 1, source_title: "Doc A".into(),
				clip_date: "2024-01-01".into(), body: "second".into(),
				similarity: 0.7, author: None, entry_position: 1, chunk_index: 0,
			},
			storage::SimilarChunk {
				chunk_id: 3, document_id: 2, source_title: "Doc B".into(),
				clip_date: "2024-02-01".into(), body: "other".into(),
				similarity: 0.8, author: None, entry_position: 0, chunk_index: 0,
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
