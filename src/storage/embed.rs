use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;
use zerocopy::IntoBytes;

use super::documents::chunk_count;
use super::{push_tag_conditions, where_clause, PassageFilters};

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
	/// The document's latest capture; `clip_date` is this entry's first.
	pub document_clip_date: String,
}

pub fn find_similar_chunks(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
) -> Result<Vec<SimilarChunk>> {
	find_similar_chunks_filtered(connection, query_embedding, limit, &PassageFilters::default())
}

/// Above this many candidate chunks, a project or required-tag filter falls
/// back from an exact scan to KNN with an enlarged fetch, trading recall for
/// bounded cost. At 4096 dimensions the exact ceiling is ~80 MB of embeddings
/// read per query.
const EXACT_SCAN_MAX_CHUNKS: i64 = 5_000;

pub fn find_similar_chunks_filtered(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
	filters: &PassageFilters,
) -> Result<Vec<SimilarChunk>> {
	find_similar_chunks_routed(connection, query_embedding, limit, filters, EXACT_SCAN_MAX_CHUNKS)
}

fn find_similar_chunks_routed(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
	filters: &PassageFilters,
	exact_scan_max_chunks: i64,
) -> Result<Vec<SimilarChunk>> {
	check_vector_dimension(connection, "vec_chunks", query_embedding.len())?;
	let scoped = filters.project.is_some() || !filters.required_tags.is_empty();
	let exact = scoped && count_scoped_chunks(connection, filters)? <= exact_scan_max_chunks;
	let results = if exact {
		find_similar_chunks_exact(connection, query_embedding, filters)?
	} else {
		find_similar_chunks_knn(connection, query_embedding, limit, filters)?
	};
	Ok(finish_similar_chunks(results, limit, filters))
}

/// Chunks in the project and/or carrying a required tag. O(total chunks):
/// entries has no document_id index, so this walks chunks and probes
/// `documents` and `document_tags` by primary key.
fn count_scoped_chunks(connection: &Connection, filters: &PassageFilters) -> Result<i64> {
	let mut conditions = Vec::new();
	let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
	if let Some(project) = filters.project {
		conditions.push(format!("d.project = ?{}", param_values.len() + 1));
		param_values.push(Box::new(project.to_string()));
	}
	push_tag_conditions(&mut conditions, &mut param_values, &[], filters.required_tags);
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
	filters: &PassageFilters,
) -> Result<Vec<SimilarChunk>> {
	let has_post_knn_filter = filters.author_like.is_some()
		|| filters.date_from.is_some()
		|| filters.date_to.is_some()
		|| filters.project.is_some()
		|| !filters.excluded_tags.is_empty()
		|| !filters.required_tags.is_empty();
	let fetch_limit = if has_post_knn_filter { limit * 5 } else { limit };

	let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
		Box::new(query_embedding.as_bytes().to_vec()),
		Box::new(fetch_limit as i64),
	];
	let mut conditions = Vec::new();
	if let Some(project) = filters.project {
		conditions.push(format!("d.project = ?{}", param_values.len() + 1));
		param_values.push(Box::new(project.to_string()));
	}
	push_tag_conditions(&mut conditions, &mut param_values, filters.excluded_tags, filters.required_tags);
	let sql = format!(
		"WITH knn AS (
			SELECT chunk_id, distance
			FROM vec_chunks
			WHERE embedding MATCH ?1 AND k = ?2
		)
		SELECT knn.chunk_id, knn.distance, c.body, e.document_id,
		       e.source_title, e.clip_date, e.author, e.position, c.chunk_index,
		       d.doc_status, d.project, c.start_char, c.end_char, d.clip_date
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
	filters: &PassageFilters,
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
		        d.doc_status, d.project, c.start_char, c.end_char, d.clip_date
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
		document_clip_date: row.get(13)?,
	})
}

fn finish_similar_chunks(
	mut results: Vec<SimilarChunk>,
	limit: usize,
	filters: &PassageFilters,
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

// --- embedding model guard ---

const EMBEDDING_MODEL_KEY: &str = "embedding_model";

/// A query or stored vector that can't be compared with the vectors already in
/// the database: a different model or dimension. Not fixable by retrying.
#[derive(Debug)]
pub struct EmbeddingMismatch(pub String);

impl std::fmt::Display for EmbeddingMismatch {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.0)
	}
}

impl std::error::Error for EmbeddingMismatch {}

pub fn recorded_embedding_model(connection: &Connection) -> Result<Option<String>> {
	match connection.query_row(
		"SELECT value FROM meta WHERE key = ?1", [EMBEDDING_MODEL_KEY], |row| row.get(0),
	) {
		Ok(model) => Ok(Some(model)),
		Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
		Err(error) => Err(error.into()),
	}
}

/// Refuses a configured embed model other than the one recorded by `embed`.
/// Databases embedded before the model was recorded pass until the next `embed`.
pub fn check_embedding_model(connection: &Connection, model: &str) -> Result<()> {
	match recorded_embedding_model(connection)? {
		Some(recorded) if recorded != model => Err(EmbeddingMismatch(format!(
			"stored embeddings were computed with {} but the configured embed model is {}; set \
			 embed_model back to {} (changing models means re-embedding the whole corpus; see \
			 \"Rebuild embeddings\" in the README)",
			recorded, model, recorded,
		)).into()),
		_ => Ok(()),
	}
}

/// The `float[N]` dimension `table` was created with, if it exists.
fn vec_table_dimension(connection: &Connection, table: &str) -> Result<Option<usize>> {
	let sql: Option<String> = match connection.query_row(
		"SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1", [table], |row| row.get(0),
	) {
		Ok(sql) => sql,
		Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
		Err(error) => return Err(error.into()),
	};
	Ok(sql.and_then(|sql| {
		let start = sql.find("float[")? + "float[".len();
		let end = start + sql[start..].find(']')?;
		sql[start..end].trim().parse().ok()
	}))
}

fn check_vector_dimension(connection: &Connection, table: &str, dimension: usize) -> Result<()> {
	match vec_table_dimension(connection, table)? {
		Some(declared) if declared != dimension => Err(EmbeddingMismatch(format!(
			"{} holds {}-dimension vectors but the query embedding has {}; the configured embed \
			 model is not the one the corpus was embedded with",
			table, declared, dimension,
		)).into()),
		_ => Ok(()),
	}
}

/// Called by `embed` once it knows the model's dimension: checks the model and
/// dimension against what is stored, then records the model if nothing was.
pub fn adopt_embedding_model(connection: &Connection, model: &str, dimension: usize) -> Result<()> {
	check_embedding_model(connection, model)?;
	check_vector_dimension(connection, "vec_chunks", dimension)?;
	check_vector_dimension(connection, "vec_claims", dimension)?;
	if recorded_embedding_model(connection)?.is_none() {
		if count_chunks_with_embeddings(connection)? + count_claims_with_embeddings(connection)? > 0 {
			eprintln!(
				"recording {} as the embedding model; existing vectors predate model tracking and are \
				 assumed to come from it",
				model,
			);
		}
		connection.execute(
			"INSERT INTO meta (key, value) VALUES (?1, ?2)",
			params![EMBEDDING_MODEL_KEY, model],
		)?;
	}
	Ok(())
}

/// Drops the claim vector table; the next `embed` recreates and refills it.
/// vec0 tables need the sqlite-vec module to drop, so the stock `sqlite3`
/// shell can't do this.
pub fn drop_claim_vectors(connection: &Connection) -> Result<()> {
	connection.execute_batch("DROP TABLE IF EXISTS vec_claims;")?;
	Ok(())
}

/// Drops both vector tables and the recorded embedding model, so the next
/// `embed` starts over with whatever model is configured.
pub fn drop_all_vectors(connection: &Connection) -> Result<()> {
	let transaction = connection.unchecked_transaction()?;
	transaction.execute_batch("DROP TABLE IF EXISTS vec_chunks; DROP TABLE IF EXISTS vec_claims;")?;
	transaction.execute("DELETE FROM meta WHERE key = ?1", [EMBEDDING_MODEL_KEY])?;
	transaction.commit()?;
	Ok(())
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

/// Always fetches `limit * 5` neighbors: filters apply after the KNN, and
/// collapsing a version family's duplicate claims frees slots to refill.
pub fn find_similar_claims(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
	author_like: Option<&str>,
	excluded_tags: &[String],
	required_tags: &[String],
) -> Result<Vec<SimilarClaim>> {
	check_vector_dimension(connection, "vec_claims", query_embedding.len())?;
	let fetch_limit = limit * 5;
	let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
		Box::new(query_embedding.as_bytes().to_vec()),
		Box::new(fetch_limit as i64),
	];
	let mut conditions = Vec::new();
	if let Some(author) = author_like {
		conditions.push(format!("LOWER(c.author) LIKE ?{}", param_values.len() + 1));
		param_values.push(Box::new(format!("%{}%", author.to_lowercase())));
	}
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

	for claim in &mut results {
		let status = super::supersession_status(connection, claim.document_id)?;
		claim.superseded = status.superseded;
		claim.current_document_id = status.current_document_id;
	}
	let mut results = collapse_family_duplicates(results);
	results.truncate(limit);
	Ok(results)
}

fn normalized_claim(content: &str) -> String {
	content.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Versions of one document often yield the same claim; keep one copy per
/// family, at the best-ranked copy's position, preferring the current
/// version's so the claim cites the document to read.
fn collapse_family_duplicates(results: Vec<SimilarClaim>) -> Vec<SimilarClaim> {
	let mut kept: Vec<SimilarClaim> = Vec::with_capacity(results.len());
	let mut seen: std::collections::HashMap<(i64, String), usize> = std::collections::HashMap::new();
	for claim in results {
		let family = claim.current_document_id.unwrap_or(claim.document_id);
		match seen.entry((family, normalized_claim(&claim.content))) {
			std::collections::hash_map::Entry::Occupied(slot) => {
				let index = *slot.get();
				if kept[index].superseded && !claim.superseded {
					let similarity = kept[index].similarity.max(claim.similarity);
					kept[index] = SimilarClaim { similarity, ..claim };
				}
			}
			std::collections::hash_map::Entry::Vacant(slot) => {
				slot.insert(kept.len());
				kept.push(claim);
			}
		}
	}
	kept
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
		let filters = PassageFilters { required_tags: &required, ..Default::default() };
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
		let required = vec!["lab-meeting".to_string()];
		assert_eq!(count_scoped_chunks(&db, &PassageFilters { required_tags: &required, ..Default::default() }).unwrap(), 1);
	}

	#[test]
	fn project_filter_holds_on_both_exact_and_knn_paths() {
		let db = setup_db();
		ensure_vec_table(&db, 4).unwrap();
		insert_embedded_document(&db, "closest, no project", &[1.0, 0.0, 0.0, 0.0]);
		let in_project = insert_embedded_document(&db, "in project", &[0.5, 0.5, 0.0, 0.0]);
		db.execute("UPDATE documents SET project = 'meetings' WHERE id = ?1", params![in_project]).unwrap();

		let filters = PassageFilters { project: Some("meetings"), ..Default::default() };
		assert_eq!(count_scoped_chunks(&db, &filters).unwrap(), 1);
		let query = [1.0, 0.0, 0.0, 0.0];
		for exact_scan_max_chunks in [0, EXACT_SCAN_MAX_CHUNKS] {
			let results = find_similar_chunks_routed(&db, &query, 5, &filters, exact_scan_max_chunks).unwrap();
			let documents: Vec<i64> = results.iter().map(|r| r.document_id).collect();
			assert_eq!(documents, vec![in_project], "threshold {}", exact_scan_max_chunks);
		}
	}

	#[test]
	fn embedding_model_is_recorded_once_and_enforced() {
		let db = setup_db();
		check_embedding_model(&db, "model-a").unwrap();
		adopt_embedding_model(&db, "model-a", 4).unwrap();
		assert_eq!(recorded_embedding_model(&db).unwrap().as_deref(), Some("model-a"));
		adopt_embedding_model(&db, "model-a", 4).unwrap();

		let error = check_embedding_model(&db, "model-b").unwrap_err();
		assert!(error.downcast_ref::<EmbeddingMismatch>().is_some());
		assert!(error.to_string().contains("model-a"), "{}", error);
		assert!(adopt_embedding_model(&db, "model-b", 4).is_err());
	}

	#[test]
	fn mismatched_dimensions_are_refused_before_searching() {
		let db = setup_db();
		ensure_vec_table(&db, 4).unwrap();
		insert_embedded_document(&db, "doc", &[1.0, 0.0, 0.0, 0.0]);
		assert_eq!(vec_table_dimension(&db, "vec_chunks").unwrap(), Some(4));
		assert_eq!(vec_table_dimension(&db, "vec_claims").unwrap(), None);

		let error = find_similar_chunks(&db, &[1.0, 0.0, 0.0], 5).unwrap_err();
		assert!(error.downcast_ref::<EmbeddingMismatch>().is_some(), "{}", error);
		assert!(adopt_embedding_model(&db, "model-a", 3).is_err());
		assert!(adopt_embedding_model(&db, "model-a", 4).is_ok());
	}

	#[test]
	fn dropping_vectors_resets_what_embed_checks() {
		let db = setup_db();
		adopt_embedding_model(&db, "model-a", 4).unwrap();
		ensure_vec_table(&db, 4).unwrap();
		ensure_vec_claims_table(&db, 4).unwrap();

		drop_claim_vectors(&db).unwrap();
		assert!(!vec_claims_table_exists(&db));
		assert!(vec_table_exists(&db), "chunk vectors survive a claims rebuild");
		assert_eq!(recorded_embedding_model(&db).unwrap().as_deref(), Some("model-a"));
		drop_claim_vectors(&db).unwrap();

		ensure_vec_claims_table(&db, 4).unwrap();
		drop_all_vectors(&db).unwrap();
		assert!(!vec_table_exists(&db));
		assert!(!vec_claims_table_exists(&db));
		assert_eq!(recorded_embedding_model(&db).unwrap(), None);
		adopt_embedding_model(&db, "model-b", 8).unwrap();
		assert_eq!(recorded_embedding_model(&db).unwrap().as_deref(), Some("model-b"));
	}

	fn similar_claim(claim_id: i64, document_id: i64, content: &str, similarity: f32, current: Option<i64>) -> SimilarClaim {
		SimilarClaim {
			claim_id,
			document_id,
			source_title: String::new(),
			content: content.to_string(),
			author: None,
			similarity,
			superseded: current.is_some(),
			current_document_id: current,
		}
	}

	#[test]
	fn family_duplicates_collapse_to_the_current_version() {
		let collapsed = collapse_family_duplicates(vec![
			similar_claim(90, 8, "Cite Montre with the BibTeX entry.", 0.71, Some(9)),
			similar_claim(55, 3, "Unrelated claim.", 0.65, None),
			similar_claim(103, 9, "cite  montre with the BibTeX entry.", 0.70, None),
			similar_claim(104, 4, "Cite Montre with the BibTeX entry.", 0.60, None),
		]);
		let ids: Vec<i64> = collapsed.iter().map(|claim| claim.claim_id).collect();
		assert_eq!(ids, vec![103, 55, 104], "same text in another family is kept");
		assert!(!collapsed[0].superseded);
		assert_eq!(collapsed[0].similarity, 0.71);
	}
}
