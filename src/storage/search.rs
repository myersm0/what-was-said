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
