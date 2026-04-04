use anyhow::Result;
use rusqlite::{params, Connection};
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

#[derive(Debug, Clone)]
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
}

pub fn find_similar_chunks(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
) -> Result<Vec<SimilarChunk>> {
	find_similar_chunks_filtered(connection, query_embedding, limit, None, None, None)
}

pub fn find_similar_chunks_filtered(
	connection: &Connection,
	query_embedding: &[f32],
	limit: usize,
	author_like: Option<&str>,
	date_from: Option<&str>,
	date_to: Option<&str>,
) -> Result<Vec<SimilarChunk>> {
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
		       e.source_title, e.clip_date, e.author, e.position, c.chunk_index
		FROM knn
		JOIN chunks c ON c.id = knn.chunk_id
		JOIN entries e ON e.id = c.entry_id
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

	results.truncate(limit);
	Ok(results)
}
