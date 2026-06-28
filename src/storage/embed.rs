use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;
use zerocopy::IntoBytes;

use super::documents::chunk_count;

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
	find_similar_chunks_filtered(connection, query_embedding, limit, None, None, None, None)
}

pub fn find_similar_chunks_filtered(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
	author_like: Option<&str>,
	date_from: Option<&str>,
	date_to: Option<&str>,
	project_filter: Option<&str>,
) -> Result<Vec<SimilarChunk>> {
	if let Some(project) = project_filter {
		return find_similar_chunks_in_project(
			connection, query_embedding, limit, project, author_like, date_from, date_to,
		);
	}

	let fetch_limit = if author_like.is_some() || date_from.is_some() || date_to.is_some() {
		limit * 5
	} else {
		limit
	};

	let mut stmt = connection.prepare(
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
		ORDER BY knn.distance"
	)?;

	let mut results: Vec<SimilarChunk> = stmt
		.query_map(params![query_embedding.as_bytes(), fetch_limit as i64], |row| {
			let distance: f32 = row.get(1)?;
			Ok(SimilarChunk {
				chunk_id: row.get(0)?,
				document_id: row.get(3)?,
				source_title: row.get(4)?,
				clip_date: row.get(5)?,
				body: row.get(2)?,
				similarity: 1.0 - distance,
				author: row.get(6)?,
				entry_position: row.get(7)?,
				chunk_index: row.get(8)?,
				doc_status: row.get(9)?,
				project: row.get(10)?,
				start_char: row.get(11)?,
				end_char: row.get(12)?,
			})
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;

	if let Some(author_pattern) = author_like {
		let pattern_lower = author_pattern.to_lowercase();
		results.retain(|r| {
			r.author.as_ref()
				.map(|a| a.to_lowercase().contains(&pattern_lower))
				.unwrap_or(false)
		});
	}

	if let Some(from) = date_from {
		results.retain(|r| r.clip_date.as_str() >= from);
	}

	if let Some(to) = date_to {
		results.retain(|r| r.clip_date.as_str() <= to);
	}

	let mut results = dedup_overlapping_chunks(results);
	results.truncate(limit);
	Ok(results)
}

fn find_similar_chunks_in_project(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
	project: &str,
	author_like: Option<&str>,
	date_from: Option<&str>,
	date_to: Option<&str>,
) -> Result<Vec<SimilarChunk>> {
	let mut stmt = connection.prepare(
		"SELECT v.chunk_id, v.embedding, c.body, e.document_id,
		        e.source_title, e.clip_date, e.author, e.position, c.chunk_index,
		        d.doc_status, d.project, c.start_char, c.end_char
		 FROM vec_chunks v
		 JOIN chunks c ON c.id = v.chunk_id
		 JOIN entries e ON e.id = c.entry_id
		 JOIN documents d ON d.id = e.document_id
		 WHERE d.project = ?1"
	)?;

	let mut results: Vec<SimilarChunk> = stmt
		.query_map(params![project], |row| {
			let blob: Vec<u8> = row.get(1)?;
			let embedding = decode_embedding(&blob);
			let similarity = cosine_similarity(&embedding, query_embedding);
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
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;

	if let Some(author_pattern) = author_like {
		let pattern_lower = author_pattern.to_lowercase();
		results.retain(|r| {
			r.author.as_ref()
				.map(|a| a.to_lowercase().contains(&pattern_lower))
				.unwrap_or(false)
		});
	}
	if let Some(from) = date_from {
		results.retain(|r| r.clip_date.as_str() >= from);
	}
	if let Some(to) = date_to {
		results.retain(|r| r.clip_date.as_str() <= to);
	}

	let mut results = dedup_overlapping_chunks(results);
	results.truncate(limit);
	Ok(results)
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
}

pub fn find_similar_claims(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
) -> Result<Vec<SimilarClaim>> {
	let mut stmt = connection.prepare(
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
		ORDER BY knn.distance"
	)?;

	let results: Vec<SimilarClaim> = stmt
		.query_map(params![query_embedding.as_bytes(), limit as i64], |row| {
			let distance: f32 = row.get(1)?;
			Ok(SimilarClaim {
				claim_id: row.get(0)?,
				document_id: row.get(4)?,
				source_title: row.get(5)?,
				content: row.get(2)?,
				author: row.get(3)?,
				similarity: 1.0 - distance,
			})
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;

	Ok(results)
}
