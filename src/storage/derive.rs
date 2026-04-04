use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

#[derive(Debug, Clone)]
pub struct DerivedContent {
	pub id: i64,
	pub document_id: i64,
	pub content_type: String,
	pub body: String,
	pub model: String,
	pub prompt_version: String,
	pub source_hash: Option<String>,
	pub parent_id: Option<i64>,
	pub quality: String,
	pub created_at: String,
}

pub fn insert_derived_content(
	connection: &Connection,
	document_id: i64,
	content_type: &str,
	body: &str,
	model: &str,
	prompt_version: &str,
	source_hash: Option<&str>,
	parent_id: Option<i64>,
) -> Result<i64> {
	let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
	connection.execute(
		"INSERT INTO derived_content (document_id, content_type, body, model, prompt_version, source_hash, parent_id, quality, created_at)
		 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'ok', ?8)",
		params![document_id, content_type, body, model, prompt_version, source_hash, parent_id, now],
	)?;
	Ok(connection.last_insert_rowid())
}

pub fn update_derived_content(
	connection: &Connection,
	id: i64,
	body: &str,
	model: &str,
	prompt_version: &str,
	source_hash: Option<&str>,
) -> Result<()> {
	let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
	connection.execute(
		"UPDATE derived_content SET body = ?1, model = ?2, prompt_version = ?3, source_hash = ?4, quality = 'ok', created_at = ?5 WHERE id = ?6",
		params![body, model, prompt_version, source_hash, now, id],
	)?;
	Ok(())
}

pub fn get_derived_content(
	connection: &Connection,
	document_id: i64,
	content_type: &str,
) -> Result<Option<DerivedContent>> {
	connection
		.query_row(
			"SELECT id, document_id, content_type, body, model, prompt_version, source_hash, parent_id, quality, created_at
			 FROM derived_content WHERE document_id = ?1 AND content_type = ?2",
			params![document_id, content_type],
			|row| {
				Ok(DerivedContent {
					id: row.get(0)?,
					document_id: row.get(1)?,
					content_type: row.get(2)?,
					body: row.get(3)?,
					model: row.get(4)?,
					prompt_version: row.get(5)?,
					source_hash: row.get(6)?,
					parent_id: row.get(7)?,
					quality: row.get(8)?,
					created_at: row.get(9)?,
				})
			},
		)
		.optional()
		.map_err(Into::into)
}

pub fn get_derived_content_by_id(connection: &Connection, id: i64) -> Result<Option<DerivedContent>> {
	connection
		.query_row(
			"SELECT id, document_id, content_type, body, model, prompt_version, source_hash, parent_id, quality, created_at
			 FROM derived_content WHERE id = ?1",
			params![id],
			|row| {
				Ok(DerivedContent {
					id: row.get(0)?,
					document_id: row.get(1)?,
					content_type: row.get(2)?,
					body: row.get(3)?,
					model: row.get(4)?,
					prompt_version: row.get(5)?,
					source_hash: row.get(6)?,
					parent_id: row.get(7)?,
					quality: row.get(8)?,
					created_at: row.get(9)?,
				})
			},
		)
		.optional()
		.map_err(Into::into)
}

pub fn set_derived_quality(connection: &Connection, id: i64, quality: &str) -> Result<()> {
	connection.execute(
		"UPDATE derived_content SET quality = ?1 WHERE id = ?2",
		params![quality, id],
	)?;
	Ok(())
}

pub fn delete_derived_content(connection: &Connection, id: i64) -> Result<()> {
	connection.execute("DELETE FROM derived_content WHERE id = ?1", params![id])?;
	Ok(())
}

pub fn compute_document_source_hash(connection: &Connection, document_id: i64) -> Result<String> {
	let mut stmt = connection.prepare(
		"SELECT body FROM entries WHERE document_id = ?1 ORDER BY position"
	)?;
	let bodies: Vec<String> = stmt
		.query_map(params![document_id], |row| row.get(0))?
		.collect::<std::result::Result<Vec<_>, _>>()?;

	use std::collections::hash_map::DefaultHasher;
	use std::hash::{Hash, Hasher};
	let mut hasher = DefaultHasher::new();
	for body in &bodies {
		body.hash(&mut hasher);
	}
	Ok(format!("{:016x}", hasher.finish()))
}

#[derive(Debug)]
pub struct DeriveStatus {
	pub total_docs: i64,
	pub with_detailed: i64,
	pub with_brief: i64,
	pub detailed_bad: i64,
	pub brief_bad: i64,
	pub detailed_stale: i64,
}

pub fn get_derive_status(connection: &Connection) -> Result<DeriveStatus> {
	let total_docs: i64 = connection.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?;
	let with_detailed: i64 = connection.query_row(
		"SELECT COUNT(*) FROM derived_content WHERE content_type = 'detailed'", [], |r| r.get(0)
	)?;
	let with_brief: i64 = connection.query_row(
		"SELECT COUNT(*) FROM derived_content WHERE content_type = 'brief'", [], |r| r.get(0)
	)?;
	let detailed_bad: i64 = connection.query_row(
		"SELECT COUNT(*) FROM derived_content WHERE content_type = 'detailed' AND quality = 'bad'", [], |r| r.get(0)
	)?;
	let brief_bad: i64 = connection.query_row(
		"SELECT COUNT(*) FROM derived_content WHERE content_type = 'brief' AND quality = 'bad'", [], |r| r.get(0)
	)?;
	Ok(DeriveStatus {
		total_docs,
		with_detailed,
		with_brief,
		detailed_bad,
		brief_bad,
		detailed_stale: 0,
	})
}

pub fn get_documents_needing_derivation(
	connection: &Connection,
	missing: bool,
	stale: bool,
	bad_detailed: bool,
	bad_brief: bool,
) -> Result<Vec<i64>> {
	let mut doc_ids: Vec<i64> = Vec::new();

	if missing {
		let mut stmt = connection.prepare(
			"SELECT d.id FROM documents d
			 WHERE NOT EXISTS (SELECT 1 FROM derived_content dc WHERE dc.document_id = d.id AND dc.content_type = 'detailed')"
		)?;
		let ids: Vec<i64> = stmt.query_map([], |r| r.get(0))?.filter_map(|r| r.ok()).collect();
		doc_ids.extend(ids);
	}

	if bad_detailed {
		let mut stmt = connection.prepare(
			"SELECT document_id FROM derived_content WHERE content_type = 'detailed' AND quality = 'bad'"
		)?;
		let ids: Vec<i64> = stmt.query_map([], |r| r.get(0))?.filter_map(|r| r.ok()).collect();
		for id in ids {
			if !doc_ids.contains(&id) {
				doc_ids.push(id);
			}
		}
	}

	if bad_brief {
		let mut stmt = connection.prepare(
			"SELECT document_id FROM derived_content WHERE content_type = 'brief' AND quality = 'bad'"
		)?;
		let ids: Vec<i64> = stmt.query_map([], |r| r.get(0))?.filter_map(|r| r.ok()).collect();
		for id in ids {
			if !doc_ids.contains(&id) {
				doc_ids.push(id);
			}
		}
	}

	if stale {
		let mut stmt = connection.prepare(
			"SELECT dc.document_id, dc.source_hash FROM derived_content dc WHERE dc.content_type = 'detailed' AND dc.source_hash IS NOT NULL"
		)?;
		let rows: Vec<(i64, String)> = stmt
			.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
			.filter_map(|r| r.ok())
			.collect();
		for (doc_id, stored_hash) in rows {
			let current_hash = compute_document_source_hash(connection, doc_id)?;
			if current_hash != stored_hash && !doc_ids.contains(&doc_id) {
				doc_ids.push(doc_id);
			}
		}
	}

	Ok(doc_ids)
}
