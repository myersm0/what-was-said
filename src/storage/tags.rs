use anyhow::Result;
use rusqlite::{params, Connection};

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
