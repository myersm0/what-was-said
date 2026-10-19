use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;
use zerocopy::IntoBytes;

use super::documents::chunk_count;
use super::{push_tag_conditions, where_clause};

#[derive(Debug)]
pub struct ChunkForEmbedding {
	pub id: i64,
	pub body: String,
}

pub fn vec_table_exists(connection: &Connection) -> bool {
	connection
		.query_row(
			"SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'vec_chunks'",
			[],
			|_| Ok(()),
		)
		.is_ok()
}

pub fn ensure_vec_table(connection: &Connection, dimension: usize) -> Result<()> {
	if vec_table_exists(connection) {
		return Ok(());
	}
	connection.execute_batch(&format!(
		"CREATE VIRTUAL TABLE vec_chunks USING vec0(
			chunk_id INTEGER PRIMARY KEY,
			embedding float[{}] distance_metric=cosine
		)",
		dimension,
	))?;
	Ok(())
}

pub fn get_chunks_without_embeddings(connection: &Connection, limit: Option<usize>) -> Result<Vec<ChunkForEmbedding>> {
	let base = if vec_table_exists(connection) {
		"SELECT c.id, c.body FROM chunks c
		 WHERE c.id NOT IN (SELECT chunk_id FROM vec_chunks)"
	} else {
		"SELECT c.id, c.body FROM chunks c"
	};
	let query = match limit {
		Some(n) => format!("{} LIMIT {}", base, n),
		None => base.to_string(),
	};
	let mut stmt = connection.prepare(&query)?;
	let chunks = stmt
		.query_map([], |row| {
			Ok(ChunkForEmbedding {
				id: row.get(0)?,
				body: row.get(1)?,
			})
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	Ok(chunks)
}

pub fn count_chunks_without_embeddings(connection: &Connection) -> Result<i64> {
	let total = chunk_count(connection)?;
	let embedded = count_chunks_with_embeddings(connection)?;
	Ok(total - embedded)
}

pub fn count_chunks_with_embeddings(connection: &Connection) -> Result<i64> {
	if !vec_table_exists(connection) {
		return Ok(0);
	}
	let count: i64 = connection.query_row(
		"SELECT COUNT(*) FROM vec_chunks",
		[],
		|row| row.get(0),
	)?;
	Ok(count)
}

pub fn insert_embedding(connection: &Connection, chunk_id: i64, embedding: &[f32]) -> Result<()> {
	connection.execute(
		"INSERT OR REPLACE INTO vec_chunks (chunk_id, embedding) VALUES (?1, ?2)",
		params![chunk_id, embedding.as_bytes()],
	)?;
	Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct SimilarChunk {
	pub chunk_id: i64,
	pub document_id: i64,
	pub source_title: String,
	pub clip_date: String,
	pub body: String,
	pub similarity: f32,
	pub author: Option<String>,
	pub entry_position: u32,
	pub chunk_index: u32,
	pub doc_status: Option<String>,
	pub project: Option<String>,
	pub start_char: usize,
	pub end_char: usize,
}

pub fn find_similar_chunks(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
) -> Result<Vec<SimilarChunk>> {
	find_similar_chunks_filtered(connection, query_embedding, limit, None, None, None, None, &[], &[])
}

/// Above this many candidate chunks, a required-tag filter falls back from an
/// exact scan to KNN with an enlarged fetch, trading recall for bounded cost.
/// At 4096 dimensions the exact ceiling is ~80 MB of embeddings read per query.
const EXACT_SCAN_MAX_CHUNKS: i64 = 5_000;

struct ChunkFilters<'a> {
	author_like: Option<&'a str>,
	date_from: Option<&'a str>,
	date_to: Option<&'a str>,
	project: Option<&'a str>,
	excluded_tags: &'a [String],
	required_tags: &'a [String],
}

pub fn find_similar_chunks_filtered(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
	author_like: Option<&str>,
	date_from: Option<&str>,
	date_to: Option<&str>,
	project_filter: Option<&str>,
	excluded_tags: &[String],
	required_tags: &[String],
) -> Result<Vec<SimilarChunk>> {
	let filters = ChunkFilters {
		author_like, date_from, date_to, project: project_filter, excluded_tags, required_tags,
	};
	find_similar_chunks_routed(connection, query_embedding, limit, &filters, EXACT_SCAN_MAX_CHUNKS)
}

fn find_similar_chunks_routed(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
	filters: &ChunkFilters,
	exact_scan_max_chunks: i64,
) -> Result<Vec<SimilarChunk>> {
	let exact = filters.project.is_some()
		|| (!filters.required_tags.is_empty()
			&& count_tagged_chunks(connection, filters.required_tags)? <= exact_scan_max_chunks);
	let results = if exact {
		find_similar_chunks_exact(connection, query_embedding, filters)?
	} else {
		find_similar_chunks_knn(connection, query_embedding, limit, filters)?
	};
	Ok(finish_similar_chunks(results, limit, filters))
}

/// O(total chunks): entries has no document_id index, so this walks chunks and
/// probes `document_tags` by primary key.
fn count_tagged_chunks(connection: &Connection, required_tags: &[String]) -> Result<i64> {
	let mut conditions = Vec::new();
	let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
	push_tag_conditions(&mut conditions, &mut param_values, &[], required_tags);
	let sql = format!(
		"SELECT COUNT(*) FROM chunks c
		 JOIN entries e ON e.id = c.entry_id
		 JOIN documents d ON d.id = e.document_id
		 {}",
		where_clause(&conditions),
	);
	let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|p| p.as_ref()).collect();
	Ok(connection.query_row(&sql, param_refs.as_slice(), |row| row.get(0))?)
}

fn find_similar_chunks_knn(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
	filters: &ChunkFilters,
) -> Result<Vec<SimilarChunk>> {
	let has_post_knn_filter = filters.author_like.is_some()
		|| filters.date_from.is_some()
		|| filters.date_to.is_some()
		|| !filters.excluded_tags.is_empty()
		|| !filters.required_tags.is_empty();
	let fetch_limit = if has_post_knn_filter { limit * 5 } else { limit };

	let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
		Box::new(query_embedding.as_bytes().to_vec()),
		Box::new(fetch_limit as i64),
	];
	let mut conditions = Vec::new();
	push_tag_conditions(&mut conditions, &mut param_values, filters.excluded_tags, filters.required_tags);
	let sql = format!(
		"WITH knn AS (
			SELECT chunk_id, distance
			FROM vec_chunks
			WHERE embedding MATCH ?1 AND k = ?2
		)
		SELECT knn.chunk_id, knn.distance, c.body, e.document_id,
		       e.source_title, e.clip_date, e.author, e.position, c.chunk_index,
		       d.doc_status, d.project, c.start_char, c.end_char
		FROM knn
		JOIN chunks c ON c.id = knn.chunk_id
		JOIN entries e ON e.id = c.entry_id
		JOIN documents d ON d.id = e.document_id
		{}
		ORDER BY knn.distance",
		where_clause(&conditions),
	);
	let mut stmt = connection.prepare(&sql)?;
	let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|p| p.as_ref()).collect();
	let results = stmt
		.query_map(param_refs.as_slice(), |row| {
			let distance: f32 = row.get(1)?;
			similar_chunk_from_row(row, 1.0 - distance)
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	Ok(results)
}

/// Brute-force cosine over the filtered candidates only. CROSS JOIN pins
/// `vec_chunks` as the inner loop so embeddings are fetched by point lookup
/// per candidate rather than scanned in full.
fn find_similar_chunks_exact(
	connection: &Connection,
	query_embedding: &[f32],
	filters: &ChunkFilters,
) -> Result<Vec<SimilarChunk>> {
	let mut conditions = Vec::new();
	let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
	if let Some(project) = filters.project {
		conditions.push(format!("d.project = ?{}", param_values.len() + 1));
		param_values.push(Box::new(project.to_string()));
	}
	push_tag_conditions(&mut conditions, &mut param_values, filters.excluded_tags, filters.required_tags);
	let sql = format!(
		"SELECT v.chunk_id, v.embedding, c.body, e.document_id,
		        e.source_title, e.clip_date, e.author, e.position, c.chunk_index,
		        d.doc_status, d.project, c.start_char, c.end_char
		 FROM chunks c
		 JOIN entries e ON e.id = c.entry_id
		 JOIN documents d ON d.id = e.document_id
		 CROSS JOIN vec_chunks v ON v.chunk_id = c.id
		 {}",
		where_clause(&conditions),
	);
	let mut stmt = connection.prepare(&sql)?;
	let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|p| p.as_ref()).collect();
	let results = stmt
		.query_map(param_refs.as_slice(), |row| {
			let blob: Vec<u8> = row.get(1)?;
			let similarity = cosine_similarity(&decode_embedding(&blob), query_embedding);
			similar_chunk_from_row(row, similarity)
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	Ok(results)
}

fn similar_chunk_from_row(row: &rusqlite::Row, similarity: f32) -> rusqlite::Result<SimilarChunk> {
	Ok(SimilarChunk {
		chunk_id: row.get(0)?,
		document_id: row.get(3)?,
		source_title: row.get(4)?,
		clip_date: row.get(5)?,
		body: row.get(2)?,
		similarity,
		author: row.get(6)?,
		entry_position: row.get(7)?,
		chunk_index: row.get(8)?,
		doc_status: row.get(9)?,
		project: row.get(10)?,
		start_char: row.get(11)?,
		end_char: row.get(12)?,
	})
}

fn finish_similar_chunks(
	mut results: Vec<SimilarChunk>,
	limit: usize,
	filters: &ChunkFilters,
) -> Vec<SimilarChunk> {
	if let Some(author_pattern) = filters.author_like {
		let pattern_lower = author_pattern.to_lowercase();
		results.retain(|r| {
			r.author.as_ref()
				.map(|a| a.to_lowercase().contains(&pattern_lower))
				.unwrap_or(false)
		});
	}
	if let Some(from) = filters.date_from {
		results.retain(|r| r.clip_date.as_str() >= from);
	}
	if let Some(to) = filters.date_to {
		results.retain(|r| r.clip_date.as_str() <= to);
	}
	let mut results = dedup_overlapping_chunks(results);
	results.truncate(limit);
	results
}

fn dedup_overlapping_chunks(mut chunks: Vec<SimilarChunk>) -> Vec<SimilarChunk> {
	chunks.sort_by(|a, b| {
		b.similarity.partial_cmp(&a.similarity).unwrap_or(std::cmp::Ordering::Equal)
	});
	let mut kept: Vec<SimilarChunk> = Vec::new();
	for chunk in chunks {
		let redundant = kept.iter().any(|k| {
			k.document_id == chunk.document_id
				&& k.entry_position == chunk.entry_position
				&& chunk.start_char < k.end_char
				&& k.start_char < chunk.end_char
		});
		if !redundant {
			kept.push(chunk);
		}
	}
	kept
}

fn decode_embedding(blob: &[u8]) -> Vec<f32> {
	blob.chunks_exact(4)
		.map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
		.collect()
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
	let len = a.len().min(b.len());
	let mut dot = 0.0f32;
	let mut norm_a = 0.0f32;
	let mut norm_b = 0.0f32;
	for i in 0..len {
		dot += a[i] * b[i];
		norm_a += a[i] * a[i];
		norm_b += b[i] * b[i];
	}
	if norm_a == 0.0 || norm_b == 0.0 {
		return 0.0;
	}
	dot / (norm_a.sqrt() * norm_b.sqrt())
}

// --- vec_claims ---

pub fn vec_claims_table_exists(connection: &Connection) -> bool {
	connection
		.query_row(
			"SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'vec_claims'",
			[],
			|_| Ok(()),
		)
		.is_ok()
}

pub fn ensure_vec_claims_table(connection: &Connection, dimension: usize) -> Result<()> {
	if vec_claims_table_exists(connection) {
		return Ok(());
	}
	connection.execute_batch(&format!(
		"CREATE VIRTUAL TABLE vec_claims USING vec0(
			claim_id INTEGER PRIMARY KEY,
			embedding float[{}] distance_metric=cosine
		)",
		dimension,
	))?;
	Ok(())
}

#[derive(Debug)]
pub struct ClaimForEmbedding {
	pub id: i64,
	pub content: String,
}

pub fn get_claims_without_embeddings(connection: &Connection, limit: Option<usize>) -> Result<Vec<ClaimForEmbedding>> {
	let base = if vec_claims_table_exists(connection) {
		"SELECT c.id, c.content FROM claims c
		 WHERE c.id NOT IN (SELECT claim_id FROM vec_claims)"
	} else {
		"SELECT c.id, c.content FROM claims c"
	};
	let query = match limit {
		Some(n) => format!("{} LIMIT {}", base, n),
		None => base.to_string(),
	};
	let mut stmt = connection.prepare(&query)?;
	let claims = stmt
		.query_map([], |row| {
			Ok(ClaimForEmbedding {
				id: row.get(0)?,
				content: row.get(1)?,
			})
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	Ok(claims)
}

pub fn count_claims_with_embeddings(connection: &Connection) -> Result<i64> {
	if !vec_claims_table_exists(connection) {
		return Ok(0);
	}
	let count: i64 = connection.query_row(
		"SELECT COUNT(*) FROM vec_claims", [], |row| row.get(0),
	)?;
	Ok(count)
}

pub fn count_claims_without_embeddings(connection: &Connection) -> Result<i64> {
	let total = super::claims::claim_count(connection)?;
	let embedded = count_claims_with_embeddings(connection)?;
	Ok(total - embedded)
}

pub fn insert_claim_embedding(connection: &Connection, claim_id: i64, embedding: &[f32]) -> Result<()> {
	connection.execute(
		"INSERT OR REPLACE INTO vec_claims (claim_id, embedding) VALUES (?1, ?2)",
		params![claim_id, embedding.as_bytes()],
	)?;
	Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct SimilarClaim {
	pub claim_id: i64,
	pub document_id: i64,
	pub source_title: String,
	pub content: String,
	pub author: Option<String>,
	pub similarity: f32,
	pub superseded: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub current_document_id: Option<i64>,
}

pub fn find_similar_claims(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
	excluded_tags: &[String],
	required_tags: &[String],
) -> Result<Vec<SimilarClaim>> {
	let has_post_knn_filter = !excluded_tags.is_empty() || !required_tags.is_empty();
	let fetch_limit = if has_post_knn_filter { limit * 5 } else { limit };
	let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
		Box::new(query_embedding.as_bytes().to_vec()),
		Box::new(fetch_limit as i64),
	];
	let mut conditions = Vec::new();
	push_tag_conditions(&mut conditions, &mut param_values, excluded_tags, required_tags);
	let sql = format!(
		"WITH knn AS (
			SELECT claim_id, distance
			FROM vec_claims
			WHERE embedding MATCH ?1 AND k = ?2
		)
		SELECT knn.claim_id, knn.distance, c.content, c.author,
		       c.document_id, d.source_title
		FROM knn
		JOIN claims c ON c.id = knn.claim_id
		JOIN documents d ON d.id = c.document_id
		{}
		ORDER BY knn.distance",
		where_clause(&conditions),
	);
	let mut stmt = connection.prepare(&sql)?;
	let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|p| p.as_ref()).collect();

	let mut results: Vec<SimilarClaim> = stmt
		.query_map(param_refs.as_slice(), |row| {
			let distance: f32 = row.get(1)?;
			Ok(SimilarClaim {
				claim_id: row.get(0)?,
				document_id: row.get(4)?,
				source_title: row.get(5)?,
				content: row.get(2)?,
				author: row.get(3)?,
				similarity: 1.0 - distance,
				superseded: false,
				current_document_id: None,
			})
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	results.truncate(limit);

	for claim in &mut results {
		let status = super::supersession_status(connection, claim.document_id)?;
		claim.superseded = status.superseded;
		claim.current_document_id = status.current_document_id;
	}

	Ok(results)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::types::{MergeStrategy, SegmentedEntry};

	fn setup_db() -> Connection {
		unsafe {
			rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
				sqlite_vec::sqlite3_vec_init as *const (),
			)));
		}
		let connection = Connection::open_in_memory().unwrap();
		crate::storage::initialize(&connection).unwrap();
		connection
	}

	fn insert_embedded_document(connection: &Connection, title: &str, embedding: &[f32]) -> i64 {
		let entry = SegmentedEntry {
			start_line: 1,
			end_line: 1,
			body: format!("{} body text", title),
			author: None,
			timestamp: None,
			is_quote: false,
			heading_level: None,
			heading_title: None,
		};
		let hash = crate::minhash::minhash(&entry.body);
		let document_id = crate::storage::insert_document(
			connection, None, title, Some("test"), MergeStrategy::None,
			Some("/test"), "2024-01-01 00:00:00", None,
		).unwrap();
		let entry_id = crate::storage::insert_entry(
			connection, document_id, &entry, 0, title, "2024-01-01 00:00:00", "/test", &hash,
		).unwrap();
		crate::storage::insert_chunks(connection, entry_id, &crate::chunking::chunk_text(&entry.body)).unwrap();
		let chunk_id: i64 = connection.query_row(
			"SELECT id FROM chunks WHERE entry_id = ?1", params![entry_id.0], |row| row.get(0),
		).unwrap();
		insert_embedding(connection, chunk_id, embedding).unwrap();
		document_id.0
	}

	#[test]
	fn required_tag_over_exact_threshold_uses_knn_with_bumped_fetch() {
		let db = setup_db();
		ensure_vec_table(&db, 4).unwrap();
		for offset in 0..3 {
			insert_embedded_document(&db, &format!("near {}", offset), &[1.0, 0.01 * offset as f32, 0.0, 0.0]);
		}
		let tagged = insert_embedded_document(&db, "far", &[0.5, 0.5, 0.0, 0.0]);
		crate::storage::add_tag(&db, tagged, "lab-meeting").unwrap();

		let required = vec!["lab-meeting".to_string()];
		let filters = ChunkFilters {
			author_like: None, date_from: None, date_to: None, project: None,
			excluded_tags: &[], required_tags: &required,
		};
		let query = [1.0, 0.0, 0.0, 0.0];
		for exact_scan_max_chunks in [0, EXACT_SCAN_MAX_CHUNKS] {
			let results = find_similar_chunks_routed(&db, &query, 1, &filters, exact_scan_max_chunks).unwrap();
			assert_eq!(results.len(), 1, "threshold {}", exact_scan_max_chunks);
			assert_eq!(results[0].document_id, tagged, "threshold {}", exact_scan_max_chunks);
		}
	}

	#[test]
	fn counts_only_chunks_of_tagged_documents() {
		let db = setup_db();
		ensure_vec_table(&db, 4).unwrap();
		insert_embedded_document(&db, "plain", &[1.0, 0.0, 0.0, 0.0]);
		let tagged = insert_embedded_document(&db, "tagged", &[1.0, 0.0, 0.0, 0.0]);
		crate::storage::add_tag(&db, tagged, "lab-meeting").unwrap();
		assert_eq!(count_tagged_chunks(&db, &["lab-meeting".to_string()]).unwrap(), 1);
	}
}
