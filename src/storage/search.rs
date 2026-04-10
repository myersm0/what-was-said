use anyhow::Result;
use rusqlite::Connection;

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

pub fn raw_fts_search(
	connection: &Connection,
	query: &str,
	author_like: Option<&str>,
	date_from: Option<&str>,
	date_to: Option<&str>,
) -> Result<Vec<ChunkSearchResult>> {
	let prefix_query: String = query
		.split_whitespace()
		.map(|word| format!("{}*", word))
		.collect::<Vec<_>>()
		.join(" ");

	let mut conditions = vec!["chunks_fts MATCH ?1".to_string()];
	let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(prefix_query)];

	if let Some(author) = author_like {
		conditions.push(format!("LOWER(e.author) LIKE ?{}", param_values.len() + 1));
		param_values.push(Box::new(format!("%{}%", author.to_lowercase())));
	}
	if let Some(from) = date_from {
		conditions.push(format!("e.clip_date >= ?{}", param_values.len() + 1));
		param_values.push(Box::new(from.to_string()));
	}
	if let Some(to) = date_to {
		conditions.push(format!("e.clip_date <= ?{}", param_values.len() + 1));
		param_values.push(Box::new(to.to_string()));
	}

	let sql = format!(
		"SELECT c.id, c.entry_id, e.document_id, c.body, c.chunk_index, e.position,
		        e.author, e.source_title, e.clip_date, e.heading_title, f.rank,
		        snippet(chunks_fts, 0, '\x02', '\x03', '\x01', 12)
		 FROM chunks_fts f
		 JOIN chunks c ON c.id = f.rowid
		 JOIN entries e ON e.id = c.entry_id
		 WHERE {}
		 ORDER BY f.rank
		 LIMIT 200",
		conditions.join(" AND "),
	);

	let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|p| p.as_ref()).collect();
	let mut statement = connection.prepare(&sql)?;
	let rows: Vec<ChunkSearchResult> = statement
		.query_map(param_refs.as_slice(), |row| {
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

	Ok(rows)
}
