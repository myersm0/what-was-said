use anyhow::Result;
use rusqlite::Connection;

/// Keyword input that FTS5 cannot parse (column selectors like `project:x`,
/// unbalanced quotes, stray operators). A client error, not an internal one.
#[derive(Debug)]
pub struct FtsQueryError {
	pub query: String,
	pub detail: String,
}

impl std::fmt::Display for FtsQueryError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "invalid keyword query {:?}: {}", self.query, self.detail)
	}
}

impl std::error::Error for FtsQueryError {}

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
	pub doc_status: Option<String>,
}

pub fn raw_fts_search(
	connection: &Connection,
	query: &str,
	author_like: Option<&str>,
	date_from: Option<&str>,
	date_to: Option<&str>,
	project_filter: Option<&str>,
	excluded_tags: &[String],
	required_tags: &[String],
) -> Result<Vec<ChunkSearchResult>> {
	let prefix_query: String = query
		.split_whitespace()
		.map(|word| format!("{}*", word))
		.collect::<Vec<_>>()
		.join(" ");

	let mut conditions = vec!["chunks_fts MATCH ?1".to_string()];
	let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(prefix_query.clone())];

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
	if let Some(project) = project_filter {
		conditions.push(format!("d.project = ?{}", param_values.len() + 1));
		param_values.push(Box::new(project.to_string()));
	}
	super::push_tag_conditions(&mut conditions, &mut param_values, excluded_tags, required_tags);

	let sql = format!(
		"SELECT c.id, c.entry_id, e.document_id, c.body, c.chunk_index, e.position,
		        e.author, e.source_title, e.clip_date, e.heading_title, f.rank,
		        snippet(chunks_fts, 0, '\x02', '\x03', '\x01', 12),
		        d.doc_status
		 FROM chunks_fts f
		 JOIN chunks c ON c.id = f.rowid
		 JOIN entries e ON e.id = c.entry_id
		 JOIN documents d ON d.id = e.document_id
		 WHERE {}
		 ORDER BY f.rank
		 LIMIT 200",
		conditions.join(" AND "),
	);

	let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|p| p.as_ref()).collect();
	run_fts_query(connection, &sql, &param_refs)
		.map_err(|error| classify_fts_failure(connection, query, &prefix_query, error))
}

fn run_fts_query(
	connection: &Connection,
	sql: &str,
	param_refs: &[&dyn rusqlite::types::ToSql],
) -> rusqlite::Result<Vec<ChunkSearchResult>> {
	let mut statement = connection.prepare(sql)?;
	let rows = statement
		.query_map(param_refs, |row| {
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
				doc_status: row.get(12)?,
			})
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	Ok(rows)
}

/// Re-runs the MATCH alone against the bare FTS table: if that fails too, the
/// input is at fault. Only runs on the failure path.
fn classify_fts_failure(
	connection: &Connection,
	query: &str,
	prefix_query: &str,
	error: rusqlite::Error,
) -> anyhow::Error {
	let probe = connection
		.prepare("SELECT rowid FROM chunks_fts WHERE chunks_fts MATCH ?1 LIMIT 1")
		.and_then(|mut statement| {
			let mut rows = statement.query([prefix_query])?;
			rows.next()?;
			Ok(())
		});
	match probe {
		Err(probe_error) => anyhow::Error::new(FtsQueryError {
			query: query.to_string(),
			detail: probe_error.to_string(),
		}),
		Ok(()) => error.into(),
	}
}
