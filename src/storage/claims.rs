use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Claim {
	pub id: i64,
	pub document_id: i64,
	pub entry_id: Option<i64>,
	pub author: Option<String>,
	pub content: String,
	pub created_at: String,
	pub model: String,
	pub prompt_hash: String,
}

pub fn insert_claim(
	connection: &Connection,
	document_id: i64,
	entry_id: Option<i64>,
	author: Option<&str>,
	content: &str,
	model: &str,
	prompt_hash: &str,
) -> Result<i64> {
	let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
	connection.execute(
		"INSERT INTO claims (document_id, entry_id, author, content, created_at, model, prompt_hash)
		 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
		params![document_id, entry_id, author, content, now, model, prompt_hash],
	)?;
	Ok(connection.last_insert_rowid())
}

pub fn get_claims_for_document(connection: &Connection, document_id: i64) -> Result<Vec<Claim>> {
	let mut stmt = connection.prepare(
		"SELECT id, document_id, entry_id, author, content, created_at, model, prompt_hash
		 FROM claims WHERE document_id = ?1 ORDER BY id"
	)?;
	let claims = stmt
		.query_map(params![document_id], |row| {
			Ok(Claim {
				id: row.get(0)?,
				document_id: row.get(1)?,
				entry_id: row.get(2)?,
				author: row.get(3)?,
				content: row.get(4)?,
				created_at: row.get(5)?,
				model: row.get(6)?,
				prompt_hash: row.get(7)?,
			})
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	Ok(claims)
}

/// Also removes the claims' vectors, since claim ids are reused after a
/// delete and a leftover vector would be taken for the new claim's, and the
/// extraction record, so an interrupted re-extraction is retried.
pub fn delete_claims_for_document(connection: &Connection, document_id: i64) -> Result<usize> {
	if super::vec_claims_table_exists(connection) {
		connection.execute(
			"DELETE FROM vec_claims WHERE claim_id IN (SELECT id FROM claims WHERE document_id = ?1)",
			params![document_id],
		)?;
	}
	connection.execute("DELETE FROM claim_extractions WHERE document_id = ?1", params![document_id])?;
	let count = connection.execute(
		"DELETE FROM claims WHERE document_id = ?1",
		params![document_id],
	)?;
	Ok(count)
}

/// The last extraction of a document, recorded even when it found no claims.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractionState {
	pub document_id: i64,
	pub doctype_name: Option<String>,
	/// `(model, prompt_hash)` of the last extraction, if any.
	pub extracted_with: Option<(String, String)>,
}

pub fn extraction_states(connection: &Connection) -> Result<Vec<ExtractionState>> {
	let mut stmt = connection.prepare(
		"SELECT d.id, d.doctype_name, x.model, x.prompt_hash
		 FROM documents d LEFT JOIN claim_extractions x ON x.document_id = d.id
		 ORDER BY d.id",
	)?;
	let states = stmt
		.query_map([], |row| {
			let model: Option<String> = row.get(2)?;
			let prompt_hash: Option<String> = row.get(3)?;
			Ok(ExtractionState {
				document_id: row.get(0)?,
				doctype_name: row.get(1)?,
				extracted_with: model.zip(prompt_hash),
			})
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	Ok(states)
}

pub fn record_extraction(
	connection: &Connection,
	document_id: i64,
	model: &str,
	prompt_hash: &str,
	claim_count: usize,
) -> Result<()> {
	let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
	connection.execute(
		"INSERT INTO claim_extractions (document_id, model, prompt_hash, extracted_at, claim_count)
		 VALUES (?1, ?2, ?3, ?4, ?5)
		 ON CONFLICT(document_id) DO UPDATE SET
			model = excluded.model, prompt_hash = excluded.prompt_hash,
			extracted_at = excluded.extracted_at, claim_count = excluded.claim_count",
		params![document_id, model, prompt_hash, now, claim_count as i64],
	)?;
	Ok(())
}

pub fn extracted_documents_count(connection: &Connection) -> Result<i64> {
	Ok(connection.query_row("SELECT COUNT(*) FROM claim_extractions", [], |row| row.get(0))?)
}

pub fn claim_count(connection: &Connection) -> Result<i64> {
	let count: i64 = connection.query_row(
		"SELECT COUNT(*) FROM claims", [], |row| row.get(0),
	)?;
	Ok(count)
}

pub fn documents_with_claims_count(connection: &Connection) -> Result<i64> {
	let count: i64 = connection.query_row(
		"SELECT COUNT(DISTINCT document_id) FROM claims", [], |row| row.get(0),
	)?;
	Ok(count)
}

