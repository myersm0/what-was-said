use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use crate::types::*;

pub fn initialize(connection: &Connection) -> Result<()> {
	connection.execute_batch(
		"
		CREATE TABLE IF NOT EXISTS documents (
			id INTEGER PRIMARY KEY,
			title TEXT,
			source_title TEXT NOT NULL,
			doctype_name TEXT,
			merge_strategy TEXT NOT NULL CHECK (merge_strategy IN ('none', 'positional', 'timestamped')),
			origin_path TEXT,
			clip_date TEXT NOT NULL
		);

		CREATE TABLE IF NOT EXISTS entries (
			id INTEGER PRIMARY KEY,
			document_id INTEGER NOT NULL REFERENCES documents(id),
			body TEXT NOT NULL,
			author TEXT,
			timestamp TEXT,
			source_title TEXT NOT NULL,
			clip_date TEXT NOT NULL,
			file_path TEXT NOT NULL,
			position INTEGER NOT NULL,
			heading_level INTEGER,
			heading_title TEXT,
			is_quote INTEGER NOT NULL DEFAULT 0,
			minhash BLOB NOT NULL
		);

		CREATE TABLE IF NOT EXISTS chunks (
			id INTEGER PRIMARY KEY,
			entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
			chunk_index INTEGER NOT NULL,
			start_char INTEGER NOT NULL,
			end_char INTEGER NOT NULL,
			body TEXT NOT NULL
		);

		CREATE INDEX IF NOT EXISTS chunks_entry_id ON chunks(entry_id);

		CREATE TABLE IF NOT EXISTS media (
			id INTEGER PRIMARY KEY,
			file_path TEXT NOT NULL,
			media_type TEXT NOT NULL CHECK (media_type IN ('screenshot', 'audio', 'transcript_segment')),
			timestamp TEXT NOT NULL,
			duration_seconds REAL,
			document_id INTEGER REFERENCES documents(id)
		);

		CREATE TABLE IF NOT EXISTS timeline_links (
			media_id INTEGER NOT NULL REFERENCES media(id),
			entry_id INTEGER NOT NULL REFERENCES entries(id),
			PRIMARY KEY (media_id, entry_id)
		);

		CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
			body,
			content=chunks,
			content_rowid=id
		);

		CREATE TRIGGER IF NOT EXISTS chunks_fts_insert AFTER INSERT ON chunks BEGIN
			INSERT INTO chunks_fts(rowid, body)
			VALUES (new.id, new.body);
		END;

		CREATE TRIGGER IF NOT EXISTS chunks_fts_delete AFTER DELETE ON chunks BEGIN
			INSERT INTO chunks_fts(chunks_fts, rowid, body)
			VALUES ('delete', old.id, old.body);
		END;

		CREATE TRIGGER IF NOT EXISTS chunks_fts_update AFTER UPDATE ON chunks BEGIN
			INSERT INTO chunks_fts(chunks_fts, rowid, body)
			VALUES ('delete', old.id, old.body);
			INSERT INTO chunks_fts(rowid, body)
			VALUES (new.id, new.body);
		END;

		CREATE TABLE IF NOT EXISTS document_tags (
			document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
			tag TEXT NOT NULL,
			PRIMARY KEY (document_id, tag)
		);

		CREATE INDEX IF NOT EXISTS document_tags_tag ON document_tags(tag);

		CREATE TABLE IF NOT EXISTS chunk_embeddings (
			chunk_id INTEGER PRIMARY KEY REFERENCES chunks(id) ON DELETE CASCADE,
			embedding BLOB NOT NULL
		);
		",
	)?;
	Ok(())
}

fn merge_strategy_to_str(strategy: MergeStrategy) -> &'static str {
	match strategy {
		MergeStrategy::None => "none",
		MergeStrategy::Positional => "positional",
		MergeStrategy::Timestamped => "timestamped",
	}
}

pub fn insert_document(
	connection: &Connection,
	title: Option<&str>,
	source_title: &str,
	doctype_name: Option<&str>,
	merge_strategy: MergeStrategy,
	origin_path: Option<&str>,
	clip_date: &str,
) -> Result<DocumentId> {
	connection.execute(
		"INSERT INTO documents (title, source_title, doctype_name, merge_strategy, origin_path, clip_date)
		 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
		params![
			title,
			source_title,
			doctype_name,
			merge_strategy_to_str(merge_strategy),
			origin_path,
			clip_date,
		],
	)?;
	Ok(DocumentId(connection.last_insert_rowid()))
}

pub fn update_document_title(connection: &Connection, document_id: DocumentId, title: &str) -> Result<()> {
	connection.execute(
		"UPDATE documents SET title = ?1 WHERE id = ?2",
		params![title, document_id.0],
	)?;
	Ok(())
}

pub fn insert_entry(
	connection: &Connection,
	document_id: DocumentId,
	entry: &SegmentedEntry,
	position: u32,
	source_title: &str,
	clip_date: &str,
	file_path: &str,
	minhash: &MinHashSignature,
) -> Result<EntryId> {
	let minhash_bytes: Vec<u8> = minhash
		.iter()
		.flat_map(|v| v.to_le_bytes())
		.collect();
	connection.execute(
		"INSERT INTO entries (
			document_id, body, author, timestamp, source_title,
			clip_date, file_path, position, heading_level, heading_title,
			is_quote, minhash
		) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
		params![
			document_id.0,
			entry.body,
			entry.author,
			entry.timestamp,
			source_title,
			clip_date,
			file_path,
			position,
			entry.heading_level.map(|l| l as i32),
			entry.heading_title,
			entry.is_quote as i32,
			minhash_bytes,
		],
	)?;
	Ok(EntryId(connection.last_insert_rowid()))
}

pub fn insert_chunks(
	connection: &Connection,
	entry_id: EntryId,
	chunks: &[crate::chunking::Chunk],
) -> Result<()> {
	for chunk in chunks {
		connection.execute(
			"INSERT INTO chunks (entry_id, chunk_index, start_char, end_char, body)
			 VALUES (?1, ?2, ?3, ?4, ?5)",
			params![
				entry_id.0,
				chunk.chunk_index,
				chunk.start_char,
				chunk.end_char,
				chunk.body,
			],
		)?;
	}
	Ok(())
}

pub struct ChunkSearchResult {
	pub chunk_id: i64,
	pub entry_id: i64,
	pub document_id: i64,
	pub chunk_body: String,
	pub snippet: String,
	pub chunk_index: u32,
	pub entry_position: u32,
	pub author: Option<String>,
	pub source_title: String,
	pub clip_date: String,
	pub heading_title: Option<String>,
	pub rank: f64,
}

pub struct GroupedSearchResult {
	pub document_id: i64,
	pub source_title: String,
	pub clip_date: String,
	pub best_rank: f64,
	pub chunks: Vec<ChunkHit>,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchSortColumn {
	Score,
	Date,
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
	let prefix_query: String = query
		.split_whitespace()
		.map(|word| format!("{}*", word))
		.collect::<Vec<_>>()
		.join(" ");

	let mut statement = connection.prepare(
		"SELECT c.id, c.entry_id, e.document_id, c.body, c.chunk_index, e.position,
		        e.author, e.source_title, e.clip_date, e.heading_title, f.rank,
		        snippet(chunks_fts, 0, '\x02', '\x03', '\x01', 12)
		 FROM chunks_fts f
		 JOIN chunks c ON c.id = f.rowid
		 JOIN entries e ON e.id = c.entry_id
		 WHERE chunks_fts MATCH ?1
		 ORDER BY f.rank
		 LIMIT 100",
	)?;
	let rows: Vec<ChunkSearchResult> = statement
		.query_map(params![prefix_query], |row| {
			Ok(ChunkSearchResult {
				chunk_id: row.get(0)?,
				entry_id: row.get(1)?,
				document_id: row.get(2)?,
				chunk_body: row.get(3)?,
				chunk_index: row.get(4)?,
				entry_position: row.get(5)?,
				author: row.get(6)?,
				source_title: row.get(7)?,
				clip_date: row.get(8)?,
				heading_title: row.get(9)?,
				rank: row.get(10)?,
				snippet: row.get(11)?,
			})
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;

	let author_pattern = author_like.map(|s| s.to_lowercase());
	let filtered_rows: Vec<ChunkSearchResult> = rows.into_iter()
		.filter(|row| {
			if let Some(ref pattern) = author_pattern {
				if !row.author.as_ref().map(|a| a.to_lowercase().contains(pattern)).unwrap_or(false) {
					return false;
				}
			}
			if let Some(from) = date_from {
				if row.clip_date.as_str() < from {
					return false;
				}
			}
			if let Some(to) = date_to {
				if row.clip_date.as_str() > to {
					return false;
				}
			}
			true
		})
		.collect();

	let mut grouped: Vec<GroupedSearchResult> = Vec::new();
	for row in filtered_rows {
		let doc = grouped.iter_mut().find(|d| d.document_id == row.document_id);
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
		match doc {
			Some(doc) => {
				let dominated_by_existing = doc.chunks.iter().any(|c| {
					(c.entry_id == hit.entry_id && c.chunk_index == hit.chunk_index)
						|| snippets_similar(&c.snippet, &hit.snippet)
				});
				if !dominated_by_existing {
					doc.chunks.retain(|c| !snippets_similar(&hit.snippet, &c.snippet) || hit.rank >= c.rank);
					if hit.rank < doc.best_rank {
						doc.best_rank = hit.rank;
					}
					doc.chunks.push(hit);
				}
			}
			None => grouped.push(GroupedSearchResult {
				document_id: row.document_id,
				source_title: row.source_title,
				clip_date: row.clip_date.clone(),
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
			grouped.sort_by(|a, b| a.best_rank.partial_cmp(&b.best_rank).unwrap_or(std::cmp::Ordering::Equal));
		}
		SearchSortColumn::Date => {
			grouped.sort_by(|a, b| b.clip_date.cmp(&a.clip_date));
		}
	}

	Ok(grouped)
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

pub fn document_count(connection: &Connection) -> Result<i64> {
	Ok(connection.query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))?)
}

pub fn document_exists_by_path(connection: &Connection, origin_path: &str) -> Result<bool> {
	let count: i64 = connection.query_row(
		"SELECT COUNT(*) FROM documents WHERE origin_path = ?1",
		params![origin_path],
		|row| row.get(0),
	)?;
	Ok(count > 0)
}

pub fn entry_count(connection: &Connection) -> Result<i64> {
	Ok(connection.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))?)
}

pub fn chunk_count(connection: &Connection) -> Result<i64> {
	Ok(connection.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?)
}

pub struct DumpEntry {
	pub body: String,
	pub author: Option<String>,
	pub heading_title: Option<String>,
	pub position: u32,
}

pub struct DumpDocument {
	pub document_id: i64,
	pub source_title: String,
	pub merge_strategy: String,
	pub entries: Vec<DumpEntry>,
}

pub fn dump_document(connection: &Connection, title_filter: Option<&str>) -> Result<Vec<DumpDocument>> {
	let (where_clause, filter_param) = match title_filter {
		Some(filter) => ("WHERE d.source_title LIKE ?1", format!("%{}%", filter)),
		None => ("", String::new()),
	};
	let query = format!(
		"SELECT d.id, d.source_title, d.merge_strategy,
		        e.body, e.author, e.heading_title, e.position
		 FROM documents d
		 JOIN entries e ON e.document_id = d.id
		 {} ORDER BY d.id, e.position",
		where_clause
	);
	let mut statement = connection.prepare(&query)?;
	let rows: Vec<(i64, String, String, String, Option<String>, Option<String>, u32)> = if title_filter.is_some() {
		statement.query_map([&filter_param], |row| {
			Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?))
		})?.collect::<std::result::Result<Vec<_>, _>>()?
	} else {
		statement.query_map([], |row| {
			Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?))
		})?.collect::<std::result::Result<Vec<_>, _>>()?
	};

	let mut documents: Vec<DumpDocument> = Vec::new();
	for (doc_id, source_title, merge_strategy, body, author, heading_title, position) in rows {
		let doc = documents.iter_mut().find(|d| d.document_id == doc_id);
		let entry = DumpEntry { body, author, heading_title, position };
		match doc {
			Some(doc) => doc.entries.push(entry),
			None => documents.push(DumpDocument {
				document_id: doc_id,
				source_title,
				merge_strategy,
				entries: vec![entry],
			}),
		}
	}
	Ok(documents)
}

#[derive(Debug, Clone)]
pub struct DocumentSummary {
	pub id: i64,
	pub title: Option<String>,
	pub source_title: String,
	pub doctype_name: Option<String>,
	pub clip_date: String,
	pub entry_count: i64,
	pub chunk_count: i64,
	pub first_line: Option<String>,
	pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
	Source,
	Doctype,
	Date,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
	Ascending,
	Descending,
}

pub fn list_documents(
	connection: &Connection,
	sort_column: SortColumn,
	sort_direction: SortDirection,
) -> Result<Vec<DocumentSummary>> {
	let order_col = match sort_column {
		SortColumn::Source => "d.source_title",
		SortColumn::Doctype => "d.doctype_name",
		SortColumn::Date => "d.clip_date",
	};
	let order_dir = match sort_direction {
		SortDirection::Ascending => "ASC",
		SortDirection::Descending => "DESC",
	};
	let query = format!(
		"SELECT d.id, d.title, d.source_title, d.doctype_name, d.clip_date,
		        COUNT(DISTINCT e.id) as entry_count,
		        COUNT(c.id) as chunk_count,
		        (SELECT SUBSTR(body, 1, 100) FROM entries WHERE document_id = d.id AND LENGTH(TRIM(body)) > 0 ORDER BY position LIMIT 1) as first_line,
		        (SELECT GROUP_CONCAT(tag, ',') FROM document_tags WHERE document_id = d.id) as tags
		 FROM documents d
		 LEFT JOIN entries e ON e.document_id = d.id
		 LEFT JOIN chunks c ON c.entry_id = e.id
		 GROUP BY d.id
		 ORDER BY {} {} NULLS LAST",
		order_col, order_dir
	);
	let mut statement = connection.prepare(&query)?;
	let results = statement
		.query_map([], |row| {
			let tags_str: Option<String> = row.get(8)?;
			let tags = tags_str
				.map(|s| s.split(',').map(|t| t.to_string()).collect())
				.unwrap_or_default();
			Ok(DocumentSummary {
				id: row.get(0)?,
				title: row.get(1)?,
				source_title: row.get(2)?,
				doctype_name: row.get(3)?,
				clip_date: row.get(4)?,
				entry_count: row.get(5)?,
				chunk_count: row.get(6)?,
				first_line: row.get(7)?,
				tags,
			})
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	Ok(results)
}

#[derive(Debug, Clone)]
pub struct DocumentContent {
	pub id: i64,
	pub title: Option<String>,
	pub source_title: String,
	pub doctype_name: Option<String>,
	pub clip_date: String,
	pub entries: Vec<EntryContent>,
}

#[derive(Debug, Clone)]
pub struct EntryContent {
	pub id: i64,
	pub position: u32,
	pub body: String,
	pub author: Option<String>,
	pub timestamp: Option<String>,
	pub heading_level: Option<u8>,
	pub heading_title: Option<String>,
	pub chunks: Vec<ChunkContent>,
}

#[derive(Debug, Clone)]
pub struct ChunkContent {
	pub id: i64,
	pub chunk_index: u32,
	pub start_char: usize,
	pub end_char: usize,
	pub body: String,
}

pub fn get_document(connection: &Connection, document_id: i64) -> Result<Option<DocumentContent>> {
	let doc_row: Option<(i64, Option<String>, String, Option<String>, String)> = connection
		.query_row(
			"SELECT id, title, source_title, doctype_name, clip_date FROM documents WHERE id = ?1",
			params![document_id],
			|row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
		)
		.optional()?;

	let Some((id, title, source_title, doctype_name, clip_date)) = doc_row else {
		return Ok(None);
	};

	let mut entry_stmt = connection.prepare(
		"SELECT id, position, body, author, timestamp, heading_level, heading_title
		 FROM entries WHERE document_id = ?1 ORDER BY position"
	)?;
	let entry_rows: Vec<(i64, u32, String, Option<String>, Option<String>, Option<u8>, Option<String>)> = entry_stmt
		.query_map(params![document_id], |row| {
			Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?))
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;

	let mut chunk_stmt = connection.prepare(
		"SELECT id, chunk_index, start_char, end_char, body FROM chunks WHERE entry_id = ?1 ORDER BY chunk_index"
	)?;

	let mut entries = Vec::new();
	for (entry_id, position, body, author, timestamp, heading_level, heading_title) in entry_rows {
		let chunk_rows: Vec<(i64, u32, usize, usize, String)> = chunk_stmt
			.query_map(params![entry_id], |row| {
				Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
			})?
			.collect::<std::result::Result<Vec<_>, _>>()?;

		let chunks: Vec<ChunkContent> = chunk_rows
			.into_iter()
			.map(|(id, chunk_index, start_char, end_char, body)| ChunkContent {
				id,
				chunk_index,
				start_char,
				end_char,
				body,
			})
			.collect();

		entries.push(EntryContent {
			id: entry_id,
			position,
			body,
			author,
			timestamp,
			heading_level,
			heading_title,
			chunks,
		});
	}

	Ok(Some(DocumentContent {
		id,
		title,
		source_title,
		doctype_name,
		clip_date,
		entries,
	}))
}

pub fn add_tag(connection: &Connection, document_id: i64, tag: &str) -> Result<()> {
	connection.execute(
		"INSERT OR IGNORE INTO document_tags (document_id, tag) VALUES (?1, ?2)",
		params![document_id, tag.trim().to_lowercase()],
	)?;
	Ok(())
}

pub fn remove_tag(connection: &Connection, document_id: i64, tag: &str) -> Result<()> {
	connection.execute(
		"DELETE FROM document_tags WHERE document_id = ?1 AND tag = ?2",
		params![document_id, tag.trim().to_lowercase()],
	)?;
	Ok(())
}

pub fn get_tags_for_document(connection: &Connection, document_id: i64) -> Result<Vec<String>> {
	let mut stmt = connection.prepare("SELECT tag FROM document_tags WHERE document_id = ?1 ORDER BY tag")?;
	let tags = stmt
		.query_map(params![document_id], |row| row.get(0))?
		.collect::<std::result::Result<Vec<String>, _>>()?;
	Ok(tags)
}

pub fn list_all_tags(connection: &Connection) -> Result<Vec<(String, i64)>> {
	let mut stmt = connection.prepare(
		"SELECT tag, COUNT(*) as count FROM document_tags GROUP BY tag ORDER BY tag"
	)?;
	let tags = stmt
		.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
		.collect::<std::result::Result<Vec<(String, i64)>, _>>()?;
	Ok(tags)
}

pub fn get_document_ids_by_tag(connection: &Connection, tag: &str) -> Result<Vec<i64>> {
	let mut stmt = connection.prepare("SELECT document_id FROM document_tags WHERE tag = ?1")?;
	let ids = stmt
		.query_map(params![tag.trim().to_lowercase()], |row| row.get(0))?
		.collect::<std::result::Result<Vec<i64>, _>>()?;
	Ok(ids)
}

#[derive(Debug)]
pub struct ChunkForEmbedding {
	pub id: i64,
	pub body: String,
}

pub fn get_chunks_without_embeddings(connection: &Connection, limit: Option<usize>) -> Result<Vec<ChunkForEmbedding>> {
	let query = match limit {
		Some(n) => format!(
			"SELECT c.id, c.body FROM chunks c
			 LEFT JOIN chunk_embeddings ce ON c.id = ce.chunk_id
			 WHERE ce.chunk_id IS NULL
			 LIMIT {}",
			n
		),
		None => "SELECT c.id, c.body FROM chunks c
		         LEFT JOIN chunk_embeddings ce ON c.id = ce.chunk_id
		         WHERE ce.chunk_id IS NULL".to_string(),
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
	let count: i64 = connection.query_row(
		"SELECT COUNT(*) FROM chunks c
		 LEFT JOIN chunk_embeddings ce ON c.id = ce.chunk_id
		 WHERE ce.chunk_id IS NULL",
		[],
		|row| row.get(0),
	)?;
	Ok(count)
}

pub fn count_chunks_with_embeddings(connection: &Connection) -> Result<i64> {
	let count: i64 = connection.query_row(
		"SELECT COUNT(*) FROM chunk_embeddings",
		[],
		|row| row.get(0),
	)?;
	Ok(count)
}

pub fn insert_embedding(connection: &Connection, chunk_id: i64, embedding: &[f32]) -> Result<()> {
	let bytes: Vec<u8> = embedding.iter()
		.flat_map(|f| f.to_le_bytes())
		.collect();
	connection.execute(
		"INSERT OR REPLACE INTO chunk_embeddings (chunk_id, embedding) VALUES (?1, ?2)",
		params![chunk_id, bytes],
	)?;
	Ok(())
}

pub fn get_embedding(connection: &Connection, chunk_id: i64) -> Result<Option<Vec<f32>>> {
	let result: Option<Vec<u8>> = connection
		.query_row(
			"SELECT embedding FROM chunk_embeddings WHERE chunk_id = ?1",
			params![chunk_id],
			|row| row.get(0),
		)
		.optional()?;

	Ok(result.map(|bytes| {
		bytes
			.chunks_exact(4)
			.map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
			.collect()
	}))
}

#[derive(Debug)]
pub struct SimilarChunk {
	pub chunk_id: i64,
	pub document_id: i64,
	pub source_title: String,
	pub clip_date: String,
	pub body: String,
	pub similarity: f32,
	pub author: Option<String>,
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
	let mut stmt = connection.prepare(
		"SELECT ce.chunk_id, ce.embedding, c.body, e.document_id, e.source_title, e.clip_date, e.author
		 FROM chunk_embeddings ce
		 JOIN chunks c ON c.id = ce.chunk_id
		 JOIN entries e ON e.id = c.entry_id"
	)?;

	let mut results: Vec<SimilarChunk> = stmt
		.query_map([], |row| {
			let chunk_id: i64 = row.get(0)?;
			let embedding_bytes: Vec<u8> = row.get(1)?;
			let body: String = row.get(2)?;
			let document_id: i64 = row.get(3)?;
			let source_title: String = row.get(4)?;
			let clip_date: String = row.get(5)?;
			let author: Option<String> = row.get(6)?;

			let embedding: Vec<f32> = embedding_bytes
				.chunks_exact(4)
				.map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
				.collect();

			let similarity = cosine_similarity(query_embedding, &embedding);

			Ok(SimilarChunk {
				chunk_id,
				document_id,
				source_title,
				clip_date,
				body,
				similarity,
				author,
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

	results.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap_or(std::cmp::Ordering::Equal));
	results.truncate(limit);
	Ok(results)
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
	if a.len() != b.len() || a.is_empty() {
		return 0.0;
	}
	let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
	let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
	let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
	if mag_a == 0.0 || mag_b == 0.0 {
		0.0
	} else {
		dot / (mag_a * mag_b)
	}
}

#[derive(Debug, Clone)]
pub struct ExistingEntry {
	pub id: i64,
	pub body: String,
	pub author: Option<String>,
	pub position: i64,
}

pub fn find_documents_by_merge_key<F>(
	connection: &Connection,
	merge_key_fn: F,
	target_key: &str,
	merge_strategy: &str,
) -> Result<Vec<i64>>
where
	F: Fn(&str) -> String,
{
	let mut stmt = connection.prepare(
		"SELECT id, source_title FROM documents WHERE merge_strategy = ?1"
	)?;
	let ids: Vec<i64> = stmt
		.query_map(params![merge_strategy], |row| {
			Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
		})?
		.filter_map(|r| r.ok())
		.filter(|(_, source_title)| merge_key_fn(source_title) == target_key)
		.map(|(id, _)| id)
		.collect();
	Ok(ids)
}

pub fn get_entries_for_document(connection: &Connection, document_id: i64) -> Result<Vec<ExistingEntry>> {
	let mut stmt = connection.prepare(
		"SELECT id, body, author, position FROM entries WHERE document_id = ?1 ORDER BY position"
	)?;
	let entries = stmt
		.query_map(params![document_id], |row| {
			Ok(ExistingEntry {
				id: row.get(0)?,
				body: row.get(1)?,
				author: row.get(2)?,
				position: row.get(3)?,
			})
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	Ok(entries)
}

pub fn get_max_entry_position(connection: &Connection, document_id: i64) -> Result<i64> {
	let max_pos: Option<i64> = connection
		.query_row(
			"SELECT MAX(position) FROM entries WHERE document_id = ?1",
			params![document_id],
			|row| row.get(0),
		)
		.optional()?
		.flatten();
	Ok(max_pos.unwrap_or(0))
}

pub fn update_document_clip_date(connection: &Connection, document_id: i64, clip_date: &str) -> Result<()> {
	connection.execute(
		"UPDATE documents SET clip_date = ?1 WHERE id = ?2",
		params![clip_date, document_id],
	)?;
	Ok(())
}
