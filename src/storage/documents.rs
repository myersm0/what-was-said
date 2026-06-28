use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::types::*;

pub struct InsertDocumentParams<'a> {
	pub title: Option<&'a str>,
	pub source_title: &'a str,
	pub doctype_name: Option<&'a str>,
	pub merge_strategy: MergeStrategy,
	pub origin_path: Option<&'a str>,
	pub clip_date: &'a str,
	pub document_minhash: Option<&'a MinHashSignature>,
	pub project: Option<&'a str>,
	pub relative_path: Option<&'a str>,
	pub content_hash: Option<&'a str>,
	pub doc_status: Option<&'a str>,
	pub doc_role: Option<&'a str>,
	pub synced_at: Option<&'a str>,
}

impl<'a> InsertDocumentParams<'a> {
	pub fn captured(
		title: Option<&'a str>,
		source_title: &'a str,
		doctype_name: Option<&'a str>,
		merge_strategy: MergeStrategy,
		origin_path: Option<&'a str>,
		clip_date: &'a str,
		document_minhash: Option<&'a MinHashSignature>,
	) -> Self {
		Self {
			title,
			source_title,
			doctype_name,
			merge_strategy,
			origin_path,
			clip_date,
			document_minhash,
			project: None,
			relative_path: None,
			content_hash: None,
			doc_status: None,
			doc_role: None,
			synced_at: None,
		}
	}

	pub fn project(
		project: &'a str,
		relative_path: &'a str,
		content_hash: &'a str,
		doc_status: &'a str,
		doc_role: Option<&'a str>,
		synced_at: &'a str,
	) -> Self {
		Self {
			title: None,
			source_title: relative_path,
			doctype_name: Some("project_markdown"),
			merge_strategy: MergeStrategy::None,
			origin_path: None,
			clip_date: synced_at,
			document_minhash: None,
			project: Some(project),
			relative_path: Some(relative_path),
			content_hash: Some(content_hash),
			doc_status: Some(doc_status),
			doc_role,
			synced_at: Some(synced_at),
		}
	}
}

pub fn insert_document_with_params(
	connection: &Connection,
	params: &InsertDocumentParams<'_>,
) -> Result<DocumentId> {
	let minhash_bytes: Option<Vec<u8>> = params.document_minhash.map(|sig| {
		sig.iter().flat_map(|v| v.to_le_bytes()).collect()
	});
	connection.execute(
		"INSERT INTO documents (
			title, source_title, doctype_name, merge_strategy, origin_path,
			clip_date, document_minhash, project, relative_path, content_hash,
			doc_status, doc_role, synced_at
		) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
		params![
			params.title,
			params.source_title,
			params.doctype_name,
			super::merge_strategy_to_str(params.merge_strategy),
			params.origin_path,
			params.clip_date,
			minhash_bytes,
			params.project,
			params.relative_path,
			params.content_hash,
			params.doc_status,
			params.doc_role,
			params.synced_at,
		],
	)?;
	Ok(DocumentId(connection.last_insert_rowid()))
}

pub fn insert_document(
	connection: &Connection,
	title: Option<&str>,
	source_title: &str,
	doctype_name: Option<&str>,
	merge_strategy: MergeStrategy,
	origin_path: Option<&str>,
	clip_date: &str,
	document_minhash: Option<&MinHashSignature>,
) -> Result<DocumentId> {
	insert_document_with_params(
		connection,
		&InsertDocumentParams::captured(
			title, source_title, doctype_name, merge_strategy,
			origin_path, clip_date, document_minhash,
		),
	)
}

pub fn insert_project_document(
	connection: &Connection,
	project: &str,
	relative_path: &str,
	content_hash: &str,
	doc_status: &str,
	doc_role: Option<&str>,
	synced_at: &str,
) -> Result<DocumentId> {
	insert_document_with_params(
		connection,
		&InsertDocumentParams::project(
			project, relative_path, content_hash, doc_status, doc_role, synced_at,
		),
	)
}

pub fn get_project_document(
	connection: &Connection,
	project: &str,
	relative_path: &str,
) -> Result<Option<(i64, Option<String>)>> {
	let row = connection
		.query_row(
			"SELECT id, content_hash FROM documents WHERE project = ?1 AND relative_path = ?2",
			params![project, relative_path],
			|row| Ok((row.get(0)?, row.get(1)?)),
		)
		.optional()?;
	Ok(row)
}

pub fn list_project_documents(connection: &Connection, project: &str) -> Result<Vec<(i64, String)>> {
	let mut stmt = connection.prepare(
		"SELECT id, relative_path FROM documents
		 WHERE project = ?1 AND relative_path IS NOT NULL",
	)?;
	let rows = stmt
		.query_map(params![project], |row| Ok((row.get(0)?, row.get(1)?)))?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	Ok(rows)
}

pub fn replace_document_children(connection: &Connection, document_id: i64) -> Result<()> {
	if super::vec_table_exists(connection) {
		let chunk_ids = chunk_ids_for_document(connection, document_id)?;
		let mut stmt = connection.prepare("DELETE FROM vec_chunks WHERE chunk_id = ?1")?;
		for id in chunk_ids {
			stmt.execute(params![id])?;
		}
	}
	if super::vec_claims_table_exists(connection) {
		let claim_ids = claim_ids_for_document(connection, document_id)?;
		let mut stmt = connection.prepare("DELETE FROM vec_claims WHERE claim_id = ?1")?;
		for id in claim_ids {
			stmt.execute(params![id])?;
		}
	}
	connection.execute("DELETE FROM claims WHERE document_id = ?1", params![document_id])?;
	connection.execute("DELETE FROM entries WHERE document_id = ?1", params![document_id])?;
	connection.execute("DELETE FROM derived_content WHERE document_id = ?1", params![document_id])?;
	Ok(())
}

fn chunk_ids_for_document(connection: &Connection, document_id: i64) -> Result<Vec<i64>> {
	let mut stmt = connection.prepare(
		"SELECT c.id FROM chunks c JOIN entries e ON e.id = c.entry_id WHERE e.document_id = ?1",
	)?;
	let ids = stmt
		.query_map(params![document_id], |row| row.get(0))?
		.collect::<std::result::Result<Vec<i64>, _>>()?;
	Ok(ids)
}

fn claim_ids_for_document(connection: &Connection, document_id: i64) -> Result<Vec<i64>> {
	let mut stmt = connection.prepare("SELECT id FROM claims WHERE document_id = ?1")?;
	let ids = stmt
		.query_map(params![document_id], |row| row.get(0))?
		.collect::<std::result::Result<Vec<i64>, _>>()?;
	Ok(ids)
}

pub fn update_project_document(
	connection: &Connection,
	document_id: i64,
	content_hash: &str,
	doc_status: &str,
	doc_role: Option<&str>,
	synced_at: &str,
) -> Result<()> {
	connection.execute(
		"UPDATE documents
		 SET content_hash = ?1, doc_status = ?2, doc_role = ?3, synced_at = ?4
		 WHERE id = ?5",
		params![content_hash, doc_status, doc_role, synced_at, document_id],
	)?;
	Ok(())
}

pub fn set_document_missing(connection: &Connection, document_id: i64, synced_at: &str) -> Result<()> {
	connection.execute(
		"UPDATE documents SET doc_status = 'missing', synced_at = ?1 WHERE id = ?2",
		params![synced_at, document_id],
	)?;
	Ok(())
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

#[derive(Serialize)]
pub struct DumpEntry {
	pub body: String,
	pub author: Option<String>,
	pub heading_title: Option<String>,
	pub position: u32,
}

#[derive(Serialize)]
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
	pub project: Option<String>,
	pub doc_status: Option<String>,
	pub doc_role: Option<String>,
	pub relative_path: Option<String>,
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
		        (SELECT SUBSTR(dc.body, 1, 200) FROM derived_content dc WHERE dc.document_id = d.id AND dc.content_type = 'brief' AND dc.quality = 'ok') as brief_summary,
		        d.project, d.doc_status, d.doc_role, d.relative_path
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
				project: row.get(10)?,
				doc_status: row.get(11)?,
				doc_role: row.get(12)?,
				relative_path: row.get(13)?,
			})
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	Ok(results)
}

#[derive(Debug, Clone, Serialize)]
pub struct DocumentContent {
	pub id: i64,
	pub title: Option<String>,
	pub source_title: String,
	pub doctype_name: Option<String>,
	pub clip_date: String,
	pub project: Option<String>,
	pub doc_status: Option<String>,
	pub doc_role: Option<String>,
	pub relative_path: Option<String>,
	pub entries: Vec<EntryContent>,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct ChunkContent {
	pub id: i64,
	pub chunk_index: u32,
	pub start_char: usize,
	pub end_char: usize,
	pub body: String,
}

pub fn get_document(connection: &Connection, document_id: i64) -> Result<Option<DocumentContent>> {
	let doc_row: Option<(i64, Option<String>, String, Option<String>, String, Option<String>, Option<String>, Option<String>, Option<String>)> = connection
		.query_row(
			"SELECT id, title, source_title, doctype_name, clip_date, project, doc_status, doc_role, relative_path FROM documents WHERE id = ?1",
			params![document_id],
			|row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?)),
		)
		.optional()?;

	let Some((id, title, source_title, doctype_name, clip_date, project, doc_status, doc_role, relative_path)) = doc_row else {
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
		project,
		doc_status,
		doc_role,
		relative_path,
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

pub struct DupCandidate {
	pub id: i64,
	pub source_title: String,
	pub origin_path: Option<String>,
	pub clip_date: String,
	pub document_minhash: MinHashSignature,
}

pub fn find_dup_candidates(
	connection: &Connection,
	clip_date: &str,
	window_days: i64,
) -> Result<Vec<DupCandidate>> {
	let mut stmt = connection.prepare(
		"SELECT id, source_title, origin_path, clip_date, document_minhash FROM documents
		 WHERE document_minhash IS NOT NULL
		 AND ABS(julianday(?1) - julianday(clip_date)) < ?2"
	)?;
	let results = stmt
		.query_map(params![clip_date, window_days], |row| {
			let id: i64 = row.get(0)?;
			let source_title: String = row.get(1)?;
			let origin_path: Option<String> = row.get(2)?;
			let clip_date: String = row.get(3)?;
			let blob: Vec<u8> = row.get(4)?;
			let mut sig = [0u64; crate::types::MINHASH_SIZE];
			for (i, chunk) in blob.chunks_exact(8).enumerate() {
				if i < crate::types::MINHASH_SIZE {
					sig[i] = u64::from_le_bytes(chunk.try_into().unwrap());
				}
			}
			Ok(DupCandidate { id, source_title, origin_path, clip_date, document_minhash: sig })
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	Ok(results)
}

pub fn insert_document_relation(
	connection: &Connection,
	from_document_id: i64,
	to_document_id: i64,
	relation: &str,
	similarity: f64,
	shared_block_words: Option<i64>,
	resolution: &str,
) -> Result<i64> {
	connection.execute(
		"INSERT OR REPLACE INTO document_relations
		 (from_document_id, to_document_id, relation, similarity, shared_block_words, resolution, created_at)
		 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
		params![
			from_document_id,
			to_document_id,
			relation,
			similarity,
			shared_block_words,
			resolution,
			chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
		],
	)?;
	Ok(connection.last_insert_rowid())
}

pub fn set_relation_summary(
	connection: &Connection,
	relation_id: i64,
	summary: &str,
	model: &str,
	prompt_hash: &str,
) -> Result<()> {
	connection.execute(
		"UPDATE document_relations
		 SET summary = ?1, summary_model = ?2, summary_prompt_hash = ?3, summarized_at = ?4
		 WHERE id = ?5",
		params![
			summary,
			model,
			prompt_hash,
			chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
			relation_id,
		],
	)?;
	Ok(())
}

pub struct RelationPair {
	pub id: i64,
	pub from_document_id: i64,
	pub to_document_id: i64,
}

pub fn get_relations_needing_summary(
	connection: &Connection,
	model: &str,
	prompt_hash: &str,
) -> Result<Vec<RelationPair>> {
	let mut stmt = connection.prepare(
		"SELECT id, from_document_id, to_document_id FROM document_relations
		 WHERE summary IS NULL OR summary_model != ?1 OR summary_prompt_hash != ?2"
	)?;
	let rows = stmt
		.query_map(params![model, prompt_hash], |row| {
			Ok(RelationPair {
				id: row.get(0)?,
				from_document_id: row.get(1)?,
				to_document_id: row.get(2)?,
			})
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	Ok(rows)
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
