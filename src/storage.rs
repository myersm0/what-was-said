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

	let mut grouped: Vec<GroupedSearchResult> = Vec::new();
	for row in rows {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
	Title,
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
		SortColumn::Title => "COALESCE(d.title, d.source_title)",
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
		        COUNT(c.id) as chunk_count
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
			Ok(DocumentSummary {
				id: row.get(0)?,
				title: row.get(1)?,
				source_title: row.get(2)?,
				doctype_name: row.get(3)?,
				clip_date: row.get(4)?,
				entry_count: row.get(5)?,
				chunk_count: row.get(6)?,
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
