use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use crate::types::*;

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
			super::merge_strategy_to_str(merge_strategy),
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
	pub brief_summary: Option<String>,
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
		        (SELECT GROUP_CONCAT(tag, ',') FROM document_tags WHERE document_id = d.id) as tags,
		        (SELECT SUBSTR(dc.body, 1, 200) FROM derived_content dc WHERE dc.document_id = d.id AND dc.content_type = 'brief' AND dc.quality = 'ok') as brief_summary
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
				brief_summary: row.get(9)?,
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

pub fn get_document_full_text(connection: &Connection, document_id: i64) -> Result<String> {
	let mut stmt = connection.prepare(
		"SELECT body, author, heading_title FROM entries WHERE document_id = ?1 ORDER BY position"
	)?;
	let entries: Vec<(String, Option<String>, Option<String>)> = stmt
		.query_map(params![document_id], |row| {
			Ok((row.get(0)?, row.get(1)?, row.get(2)?))
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;

	let mut text = String::new();
	for (body, author, heading) in entries {
		if let Some(h) = heading {
			text.push_str(&format!("## {}\n\n", h));
		}
		if let Some(a) = author {
			text.push_str(&format!("[{}]\n", a));
		}
		text.push_str(&body);
		text.push_str("\n\n");
	}
	Ok(text)
}
