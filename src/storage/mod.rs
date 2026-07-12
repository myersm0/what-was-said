mod documents;
mod search;
mod embed;
mod tags;
mod derive;
mod claims;

pub use documents::*;
pub use search::*;
pub use embed::*;
pub use tags::*;
pub use derive::*;
pub use claims::*;

use anyhow::Result;
use rusqlite::Connection;

use crate::types::MergeStrategy;

/// The single SQL form of tag-based hiding, shared by every search path:
/// a `NOT EXISTS` against `document_tags` over an already-expanded tag list.
pub(crate) struct TagExclusion {
	tags: Vec<String>,
	first_param: usize,
}

pub(crate) fn tag_exclusion_clause(excluded_tags: &[String], first_param: usize) -> TagExclusion {
	TagExclusion { tags: excluded_tags.to_vec(), first_param }
}

impl TagExclusion {
	pub(crate) fn condition(&self) -> Option<String> {
		if self.tags.is_empty() {
			return None;
		}
		let placeholders: Vec<String> = (0..self.tags.len())
			.map(|offset| format!("?{}", self.first_param + offset))
			.collect();
		Some(format!(
			"NOT EXISTS (SELECT 1 FROM document_tags dt WHERE dt.document_id = d.id AND dt.tag IN ({}))",
			placeholders.join(", "),
		))
	}

	pub(crate) fn where_fragment(&self, prefix: &str) -> String {
		match self.condition() {
			Some(condition) => format!("{} {}", prefix, condition),
			None => String::new(),
		}
	}

	pub(crate) fn push_params(&self, params: &mut Vec<Box<dyn rusqlite::types::ToSql>>) {
		for tag in &self.tags {
			params.push(Box::new(tag.clone()));
		}
	}
}

fn resign_stale_minhash_signatures(connection: &Connection) -> Result<()> {
	let signature_bytes = (crate::types::MINHASH_SIZE * 8) as i64;
	let mut stmt = connection.prepare(
		"SELECT id FROM documents WHERE document_minhash IS NOT NULL AND length(document_minhash) != ?1",
	)?;
	let document_ids: Vec<i64> = stmt
		.query_map([signature_bytes], |row| row.get(0))?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	for id in document_ids {
		let entries = get_entries_for_document(connection, id)?;
		let body = entries
			.iter()
			.map(|e| e.body.as_str())
			.collect::<Vec<_>>()
			.join("\n");
		let signature = crate::minhash::minhash(&body);
		let blob: Vec<u8> = signature.iter().flat_map(|v| v.to_le_bytes()).collect();
		connection.execute(
			"UPDATE documents SET document_minhash = ?1 WHERE id = ?2",
			rusqlite::params![blob, id],
		)?;
	}
	let mut stmt = connection.prepare(
		"SELECT id, body FROM entries WHERE minhash IS NOT NULL AND length(minhash) != ?1",
	)?;
	let entry_rows: Vec<(i64, String)> = stmt
		.query_map([signature_bytes], |row| Ok((row.get(0)?, row.get(1)?)))?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	for (id, body) in entry_rows {
		let signature = crate::minhash::minhash(&body);
		let blob: Vec<u8> = signature.iter().flat_map(|v| v.to_le_bytes()).collect();
		connection.execute(
			"UPDATE entries SET minhash = ?1 WHERE id = ?2",
			rusqlite::params![blob, id],
		)?;
	}
	Ok(())
}

fn backfill_shingle_counts(connection: &Connection) -> Result<()> {
	let mut stmt = connection
		.prepare("SELECT id FROM documents WHERE document_minhash IS NOT NULL")?;
	let ids: Vec<i64> = stmt
		.query_map([], |row| row.get(0))?
		.collect::<std::result::Result<Vec<_>, _>>()?;
	for id in ids {
		let entries = get_entries_for_document(connection, id)?;
		let body = entries
			.iter()
			.map(|e| e.body.as_str())
			.collect::<Vec<_>>()
			.join("\n");
		let count = crate::minhash::distinct_shingle_count(&body) as i64;
		connection.execute(
			"UPDATE documents SET document_shingle_count = ?1 WHERE id = ?2",
			rusqlite::params![count, id],
		)?;
	}
	Ok(())
}

pub fn initialize(connection: &Connection) -> Result<()> {
	connection.execute_batch("PRAGMA foreign_keys = ON;")?;
	connection.execute_batch("PRAGMA journal_mode=WAL;")?;
	connection.execute_batch(
		"
		CREATE TABLE IF NOT EXISTS documents (
			id INTEGER PRIMARY KEY,
			title TEXT,
			source_title TEXT NOT NULL,
			doctype_name TEXT,
			merge_strategy TEXT NOT NULL CHECK (merge_strategy IN ('none', 'positional', 'timestamped')),
			origin_path TEXT,
			clip_date TEXT NOT NULL,
			document_minhash BLOB,
			project TEXT,
			relative_path TEXT,
			content_hash TEXT,
			doc_status TEXT,
			doc_role TEXT,
			synced_at TEXT,
			clip_date_source TEXT NOT NULL DEFAULT 'ingest_fallback'
				CHECK (clip_date_source IN ('filename', 'content', 'metadata', 'ingest_fallback')),
			document_shingle_count INTEGER
		);

		CREATE TABLE IF NOT EXISTS entries (
			id INTEGER PRIMARY KEY,
			document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
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

		CREATE TABLE IF NOT EXISTS derived_content (
			id INTEGER PRIMARY KEY,
			document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
			content_type TEXT NOT NULL CHECK (content_type IN ('detailed', 'brief')),
			body TEXT NOT NULL,
			model TEXT NOT NULL,
			prompt_version TEXT NOT NULL,
			source_hash TEXT,
			parent_id INTEGER REFERENCES derived_content(id) ON DELETE CASCADE,
			quality TEXT NOT NULL DEFAULT 'ok' CHECK (quality IN ('ok', 'bad')),
			created_at TEXT NOT NULL
		);

		CREATE INDEX IF NOT EXISTS derived_content_document_id ON derived_content(document_id);
		CREATE UNIQUE INDEX IF NOT EXISTS derived_content_doc_type ON derived_content(document_id, content_type);

		CREATE TABLE IF NOT EXISTS claims (
			id INTEGER PRIMARY KEY,
			document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
			entry_id INTEGER REFERENCES entries(id) ON DELETE CASCADE,
			author TEXT,
			content TEXT NOT NULL,
			created_at TEXT NOT NULL,
			model TEXT NOT NULL,
			prompt_hash TEXT NOT NULL
		);

		CREATE INDEX IF NOT EXISTS claims_document_id ON claims(document_id);

		CREATE TABLE IF NOT EXISTS document_relations (
			id INTEGER PRIMARY KEY,
			from_document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
			to_document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
			relation TEXT NOT NULL,
			similarity REAL,
			shared_block_words INTEGER,
			resolution TEXT NOT NULL CHECK (resolution IN ('superseded', 'kept_both')),
			summary TEXT,
			summary_model TEXT,
			summary_prompt_hash TEXT,
			summarized_at TEXT,
			created_at TEXT NOT NULL,
			UNIQUE (from_document_id, to_document_id)
		);

		CREATE INDEX IF NOT EXISTS document_relations_from ON document_relations(from_document_id);
		CREATE INDEX IF NOT EXISTS document_relations_to ON document_relations(to_document_id);
		",
	)?;
	migrate(connection)?;
	Ok(())
}

fn migrate(connection: &Connection) -> Result<()> {
	let entries_sql: String = connection.query_row(
		"SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'entries'",
		[],
		|row| row.get(0),
	)?;
	if !entries_sql.contains("ON DELETE CASCADE") {
		eprintln!("migrating entries table to add ON DELETE CASCADE");
		connection.execute_batch("PRAGMA foreign_keys = OFF;")?;
		connection.execute_batch(
			"
			ALTER TABLE entries RENAME TO entries_old;
			CREATE TABLE entries (
				id INTEGER PRIMARY KEY,
				document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
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
			INSERT INTO entries SELECT * FROM entries_old;
			DROP TABLE entries_old;
			",
		)?;
		connection.execute_batch("PRAGMA foreign_keys = ON;")?;
	}
	let doc_sql: String = connection.query_row(
		"SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'documents'",
		[],
		|row| row.get(0),
	)?;
	if !doc_sql.contains("document_minhash") {
		connection.execute_batch(
			"ALTER TABLE documents ADD COLUMN document_minhash BLOB;",
		)?;
	}
	if !doc_sql.contains("project") {
		for column in [
			"project TEXT",
			"relative_path TEXT",
			"content_hash TEXT",
			"doc_status TEXT",
			"doc_role TEXT",
			"synced_at TEXT",
		] {
			connection.execute_batch(&format!("ALTER TABLE documents ADD COLUMN {};", column))?;
		}
	}
	if !doc_sql.contains("clip_date_source") {
		connection.execute_batch(
			"ALTER TABLE documents ADD COLUMN clip_date_source TEXT NOT NULL DEFAULT 'ingest_fallback'
				CHECK (clip_date_source IN ('filename', 'content', 'metadata', 'ingest_fallback'));",
		)?;
	}
	if !doc_sql.contains("document_shingle_count") {
		connection.execute_batch(
			"ALTER TABLE documents ADD COLUMN document_shingle_count INTEGER;",
		)?;
		backfill_shingle_counts(connection)?;
	}
	resign_stale_minhash_signatures(connection)?;
	connection.execute_batch(
		"CREATE UNIQUE INDEX IF NOT EXISTS idx_documents_project_path
		 ON documents(project, relative_path)
		 WHERE project IS NOT NULL AND relative_path IS NOT NULL;",
	)?;
	let orphaned_entries: i64 = connection.query_row(
		"SELECT COUNT(*) FROM entries WHERE document_id NOT IN (SELECT id FROM documents)",
		[],
		|row| row.get(0),
	)?;
	if orphaned_entries > 0 {
		eprintln!("cleaning up {} orphaned entries", orphaned_entries);
		connection.execute(
			"DELETE FROM entries WHERE document_id NOT IN (SELECT id FROM documents)",
			[],
		)?;
		connection.execute(
			"DELETE FROM chunks WHERE entry_id NOT IN (SELECT id FROM entries)",
			[],
		)?;
		connection.execute_batch("INSERT INTO chunks_fts(chunks_fts) VALUES('rebuild');")?;
	}
	let claims_exists: bool = connection
		.query_row(
			"SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'claims'",
			[], |_| Ok(()),
		)
		.is_ok();
	if claims_exists {
		let claims_sql: String = connection.query_row(
			"SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'claims'",
			[], |row| row.get(0),
		)?;
		if claims_sql.contains("kind") || !claims_sql.contains("prompt_hash") {
			eprintln!("migrating claims table (dropping old schema, re-extract to repopulate)");
			connection.execute_batch("DROP TABLE claims;")?;
			connection.execute_batch(
				"CREATE TABLE claims (
					id INTEGER PRIMARY KEY,
					document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
					entry_id INTEGER REFERENCES entries(id) ON DELETE CASCADE,
					author TEXT,
					content TEXT NOT NULL,
					created_at TEXT NOT NULL,
					model TEXT NOT NULL,
					prompt_hash TEXT NOT NULL
				);
				CREATE INDEX IF NOT EXISTS claims_document_id ON claims(document_id);",
			)?;
		}
	}
	let relations_exists: bool = connection
		.query_row(
			"SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'document_relations'",
			[], |_| Ok(()),
		)
		.is_ok();
	if relations_exists {
		let relations_sql: String = connection.query_row(
			"SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'document_relations'",
			[], |row| row.get(0),
		)?;
		if !relations_sql.contains("summary") {
			for column in ["summary TEXT", "summary_model TEXT", "summary_prompt_hash TEXT", "summarized_at TEXT"] {
				connection.execute_batch(&format!("ALTER TABLE document_relations ADD COLUMN {};", column))?;
			}
		}
		if relations_sql.contains("'pending'") {
			eprintln!("migrating document_relations to retire the 'pending' resolution");
			connection.execute_batch("PRAGMA foreign_keys = OFF;")?;
			connection.execute_batch(
				"
				ALTER TABLE document_relations RENAME TO document_relations_old;
				CREATE TABLE document_relations (
					id INTEGER PRIMARY KEY,
					from_document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
					to_document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
					relation TEXT NOT NULL,
					similarity REAL,
					shared_block_words INTEGER,
					resolution TEXT NOT NULL CHECK (resolution IN ('superseded', 'kept_both')),
					summary TEXT,
					summary_model TEXT,
					summary_prompt_hash TEXT,
					summarized_at TEXT,
					created_at TEXT NOT NULL,
					UNIQUE (from_document_id, to_document_id)
				);
				INSERT INTO document_relations
					SELECT id, from_document_id, to_document_id, relation, similarity, shared_block_words,
						CASE WHEN resolution = 'pending' THEN 'kept_both' ELSE resolution END,
						summary, summary_model, summary_prompt_hash, summarized_at, created_at
					FROM document_relations_old;
				DROP TABLE document_relations_old;
				CREATE INDEX IF NOT EXISTS document_relations_from ON document_relations(from_document_id);
				CREATE INDEX IF NOT EXISTS document_relations_to ON document_relations(to_document_id);
				",
			)?;
			connection.execute_batch("PRAGMA foreign_keys = ON;")?;
		}
	}
	Ok(())
}

fn merge_strategy_to_str(strategy: MergeStrategy) -> &'static str {
	match strategy {
		MergeStrategy::None => "none",
		MergeStrategy::Positional => "positional",
		MergeStrategy::Timestamped => "timestamped",
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::chunking;
	use crate::minhash;
	use crate::types::*;

	fn setup_db() -> rusqlite::Connection {
		unsafe {
			rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
				sqlite_vec::sqlite3_vec_init as *const (),
			)));
		}
		let connection = rusqlite::Connection::open_in_memory().unwrap();
		initialize(&connection).unwrap();
		connection
	}

	fn make_entry(body: &str, author: Option<&str>) -> SegmentedEntry {
		SegmentedEntry {
			start_line: 1,
			end_line: 1,
			body: body.to_string(),
			author: author.map(|s| s.to_string()),
			timestamp: None,
			is_quote: false,
			heading_level: None,
			heading_title: None,
		}
	}

	fn insert_test_document(connection: &rusqlite::Connection, title: &str, body: &str) -> DocumentId {
		let entry = make_entry(body, None);
		let hash = minhash::minhash(body);
		let doc_id = insert_document(
			connection, None, title, Some("test"), MergeStrategy::None,
			Some("/test"), "2024-01-01 00:00:00", None,
		).unwrap();
		let entry_id = insert_entry(
			connection, doc_id, &entry, 0, title,
			"2024-01-01 00:00:00", "/test", &hash,
		).unwrap();
		let chunks = chunking::chunk_text(body);
		insert_chunks(connection, entry_id, &chunks).unwrap();
		doc_id
	}

	#[test]
	fn insert_and_retrieve_document() {
		let db = setup_db();
		let doc_id = insert_test_document(&db, "Test Doc", "Hello world content");
		let doc = get_document(&db, doc_id.0).unwrap().unwrap();
		assert_eq!(doc.source_title, "Test Doc");
		assert_eq!(doc.entries.len(), 1);
		assert!(doc.entries[0].body.contains("Hello world"));
	}

	#[test]
	fn document_count_tracks_inserts() {
		let db = setup_db();
		assert_eq!(document_count(&db).unwrap(), 0);
		insert_test_document(&db, "Doc 1", "content one");
		assert_eq!(document_count(&db).unwrap(), 1);
		insert_test_document(&db, "Doc 2", "content two");
		assert_eq!(document_count(&db).unwrap(), 2);
	}

	#[test]
	fn fts5_search_finds_content() {
		let db = setup_db();
		insert_test_document(&db, "Rust Guide", "Rust is a systems programming language");
		insert_test_document(&db, "Python Guide", "Python is a dynamic programming language");
		let results = crate::query::search(&db, "rust", crate::query::SearchSortColumn::Score).unwrap();
		assert_eq!(results.len(), 1);
		assert_eq!(results[0].source_title, "Rust Guide");
	}

	#[test]
	fn fts5_search_no_results() {
		let db = setup_db();
		insert_test_document(&db, "Doc", "some content");
		let results = crate::query::search(&db, "nonexistent", crate::query::SearchSortColumn::Score).unwrap();
		assert!(results.is_empty());
	}

	#[test]
	fn fts5_search_prefix_matching() {
		let db = setup_db();
		insert_test_document(&db, "Doc", "the cathedral and the bazaar");
		let results = crate::query::search(&db, "cathed", crate::query::SearchSortColumn::Score).unwrap();
		assert_eq!(results.len(), 1);
	}

	#[test]
	fn tag_operations() {
		let db = setup_db();
		let doc_id = insert_test_document(&db, "Doc", "content");
		add_tag(&db, doc_id.0, "research").unwrap();
		add_tag(&db, doc_id.0, "rust").unwrap();

		let tags = get_tags_for_document(&db, doc_id.0).unwrap();
		assert_eq!(tags, vec!["research", "rust"]);

		remove_tag(&db, doc_id.0, "research").unwrap();
		let tags = get_tags_for_document(&db, doc_id.0).unwrap();
		assert_eq!(tags, vec!["rust"]);
	}

	#[test]
	fn duplicate_tag_ignored() {
		let db = setup_db();
		let doc_id = insert_test_document(&db, "Doc", "content");
		add_tag(&db, doc_id.0, "test").unwrap();
		add_tag(&db, doc_id.0, "test").unwrap();
		let tags = get_tags_for_document(&db, doc_id.0).unwrap();
		assert_eq!(tags.len(), 1);
	}

	#[test]
	fn list_all_tags_with_counts() {
		let db = setup_db();
		let doc1 = insert_test_document(&db, "Doc1", "content");
		let doc2 = insert_test_document(&db, "Doc2", "content");
		add_tag(&db, doc1.0, "shared").unwrap();
		add_tag(&db, doc2.0, "shared").unwrap();
		add_tag(&db, doc1.0, "unique").unwrap();
		let tags = list_all_tags(&db).unwrap();
		assert_eq!(tags.len(), 2);
		let shared = tags.iter().find(|(t, _)| t == "shared").unwrap();
		assert_eq!(shared.1, 2);
	}

	#[test]
	fn document_exists_by_path_check() {
		let db = setup_db();
		assert!(!document_exists_by_path(&db, "/test").unwrap());
		insert_test_document(&db, "Doc", "content");
		assert!(document_exists_by_path(&db, "/test").unwrap());
	}

	#[test]
	fn embedding_insert_and_knn_search() {
		let db = setup_db();
		insert_test_document(&db, "Doc A", "rust memory safety borrow checker");
		insert_test_document(&db, "Doc B", "python garbage collection runtime");

		let dim = 8;
		ensure_vec_table(&db, dim).unwrap();

		let chunk_ids: Vec<i64> = db.prepare("SELECT id FROM chunks ORDER BY id").unwrap()
			.query_map([], |r| r.get(0)).unwrap()
			.filter_map(|r| r.ok()).collect();
		assert_eq!(chunk_ids.len(), 2);

		let emb_a: Vec<f32> = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
		let emb_b: Vec<f32> = vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
		insert_embedding(&db, chunk_ids[0], &emb_a).unwrap();
		insert_embedding(&db, chunk_ids[1], &emb_b).unwrap();

		assert_eq!(count_chunks_with_embeddings(&db).unwrap(), 2);
		assert_eq!(count_chunks_without_embeddings(&db).unwrap(), 0);

		let query: Vec<f32> = vec![0.9, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
		let results = find_similar_chunks(&db, &query, 2).unwrap();
		assert_eq!(results.len(), 2);
		assert_eq!(results[0].source_title, "Doc A");
	}

	#[test]
	fn vec_table_lifecycle() {
		let db = setup_db();
		assert!(!vec_table_exists(&db));
		ensure_vec_table(&db, 4).unwrap();
		assert!(vec_table_exists(&db));
		ensure_vec_table(&db, 4).unwrap();
	}

	#[test]
	fn derived_content_lifecycle() {
		let db = setup_db();
		let doc_id = insert_test_document(&db, "Doc", "content");
		assert!(get_derived_content(&db, doc_id.0, "detailed").unwrap().is_none());

		insert_derived_content(
			&db, doc_id.0, "detailed", "A detailed summary",
			"test-model", "v1", Some("hash123"), None,
		).unwrap();

		let content = get_derived_content(&db, doc_id.0, "detailed").unwrap().unwrap();
		assert_eq!(content.body, "A detailed summary");
		assert_eq!(content.quality, "ok");

		set_derived_quality(&db, content.id, "bad").unwrap();
		let content = get_derived_content(&db, doc_id.0, "detailed").unwrap().unwrap();
		assert_eq!(content.quality, "bad");

		update_derived_content(
			&db, content.id, "Updated summary", "test-model", "v2", Some("hash456"),
		).unwrap();
		let content = get_derived_content(&db, doc_id.0, "detailed").unwrap().unwrap();
		assert_eq!(content.body, "Updated summary");
		assert_eq!(content.quality, "ok");
	}

	#[test]
	fn list_documents_includes_brief_summary() {
		let db = setup_db();
		let doc_id = insert_test_document(&db, "Doc", "content");
		insert_derived_content(
			&db, doc_id.0, "brief", "A brief summary",
			"test-model", "v1", None, None,
		).unwrap();

		let docs = list_documents(&db, SortColumn::Date, SortDirection::Descending).unwrap();
		assert_eq!(docs.len(), 1);
		assert_eq!(docs[0].brief_summary.as_deref(), Some("A brief summary"));
	}

	#[test]
	fn list_documents_sorts_correctly() {
		let db = setup_db();
		let entry = make_entry("content", None);
		let hash = minhash::minhash("content");

		let doc1 = insert_document(
			&db, None, "Beta", Some("test"), MergeStrategy::None, None, "2024-01-01 00:00:00", None,
		).unwrap();
		insert_entry(&db, doc1, &entry, 0, "Beta", "2024-01-01 00:00:00", "/a", &hash).unwrap();

		let doc2 = insert_document(
			&db, None, "Alpha", Some("test"), MergeStrategy::None, None, "2024-06-01 00:00:00", None,
		).unwrap();
		insert_entry(&db, doc2, &entry, 0, "Alpha", "2024-06-01 00:00:00", "/b", &hash).unwrap();

		let by_date = list_documents(&db, SortColumn::Date, SortDirection::Descending).unwrap();
		assert_eq!(by_date[0].source_title, "Alpha");

		let by_source = list_documents(&db, SortColumn::Source, SortDirection::Ascending).unwrap();
		assert_eq!(by_source[0].source_title, "Alpha");
	}

	#[test]
	fn claim_insert_and_retrieve() {
		let db = setup_db();
		let doc_id = insert_test_document(&db, "Doc", "Some content about testing");
		let id = insert_claim(
			&db, doc_id.0, None, Some("Alice"), "Testing improves code quality.", "test-model", "abc123",
		).unwrap();
		assert!(id > 0);

		let claims = get_claims_for_document(&db, doc_id.0).unwrap();
		assert_eq!(claims.len(), 1);
		assert_eq!(claims[0].content, "Testing improves code quality.");
		assert_eq!(claims[0].author.as_deref(), Some("Alice"));
		assert_eq!(claims[0].prompt_hash, "abc123");
	}

	#[test]
	fn claim_delete_by_document() {
		let db = setup_db();
		let doc_id = insert_test_document(&db, "Doc", "content");
		insert_claim(&db, doc_id.0, None, None, "Claim one.", "m", "h").unwrap();
		insert_claim(&db, doc_id.0, None, None, "Claim two.", "m", "h").unwrap();
		assert_eq!(claim_count(&db).unwrap(), 2);

		let deleted = delete_claims_for_document(&db, doc_id.0).unwrap();
		assert_eq!(deleted, 2);
		assert_eq!(claim_count(&db).unwrap(), 0);
	}

	#[test]
	fn claims_cascade_on_document_delete() {
		let db = setup_db();
		let doc_id = insert_test_document(&db, "Doc", "content");
		insert_claim(&db, doc_id.0, None, None, "A claim.", "m", "h").unwrap();
		assert_eq!(claim_count(&db).unwrap(), 1);

		db.execute("DELETE FROM documents WHERE id = ?1", [doc_id.0]).unwrap();
		assert_eq!(claim_count(&db).unwrap(), 0);
	}

	#[test]
	fn documents_needing_extraction_checks_staleness() {
		let db = setup_db();
		let doc1 = insert_test_document(&db, "Doc1", "content one");
		let doc2 = insert_test_document(&db, "Doc2", "content two");

		let needing = get_documents_needing_extraction(&db, "model-a", "hash-1").unwrap();
		assert_eq!(needing.len(), 2);

		insert_claim(&db, doc1.0, None, None, "A claim.", "model-a", "hash-1").unwrap();
		let needing = get_documents_needing_extraction(&db, "model-a", "hash-1").unwrap();
		assert_eq!(needing.len(), 1);
		assert_eq!(needing[0], doc2.0);

		// changing model makes doc1 stale again
		let needing = get_documents_needing_extraction(&db, "model-b", "hash-1").unwrap();
		assert_eq!(needing.len(), 2);

		// changing prompt_hash makes doc1 stale again
		let needing = get_documents_needing_extraction(&db, "model-a", "hash-2").unwrap();
		assert_eq!(needing.len(), 2);
	}

	#[test]
	fn claim_embedding_lifecycle() {
		let db = setup_db();
		let doc_id = insert_test_document(&db, "Doc", "content");
		let claim_id = insert_claim(
			&db, doc_id.0, None, None, "Rust uses a borrow checker for memory safety.", "m", "h",
		).unwrap();

		assert!(!vec_claims_table_exists(&db));
		assert_eq!(count_claims_without_embeddings(&db).unwrap(), 1);

		let dim = 8;
		ensure_vec_claims_table(&db, dim).unwrap();
		assert!(vec_claims_table_exists(&db));

		let embedding: Vec<f32> = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
		insert_claim_embedding(&db, claim_id, &embedding).unwrap();

		assert_eq!(count_claims_with_embeddings(&db).unwrap(), 1);
		assert_eq!(count_claims_without_embeddings(&db).unwrap(), 0);
	}

	#[test]
	fn find_similar_claims_returns_ranked() {
		let db = setup_db();
		let doc_id = insert_test_document(&db, "Doc", "content");
		let c1 = insert_claim(
			&db, doc_id.0, None, None, "Memory safety via borrow checker.", "m", "h",
		).unwrap();
		let c2 = insert_claim(
			&db, doc_id.0, None, None, "Python uses garbage collection.", "m", "h",
		).unwrap();

		let dim = 8;
		ensure_vec_claims_table(&db, dim).unwrap();

		let emb_a: Vec<f32> = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
		let emb_b: Vec<f32> = vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
		insert_claim_embedding(&db, c1, &emb_a).unwrap();
		insert_claim_embedding(&db, c2, &emb_b).unwrap();

		let query: Vec<f32> = vec![0.9, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
		let results = find_similar_claims(&db, &query, 2, &[]).unwrap();
		assert_eq!(results.len(), 2);
		assert_eq!(results[0].claim_id, c1);
		assert!(results[0].similarity > results[1].similarity);
	}

	fn insert_dated_document(connection: &rusqlite::Connection, title: &str, clip_date: &str) -> i64 {
		insert_document(
			connection, None, title, Some("test"), MergeStrategy::None,
			Some("/test"), clip_date, None,
		).unwrap().0
	}

	fn link_superseded(connection: &rusqlite::Connection, newer: i64, older: i64) {
		insert_document_relation(connection, newer, older, "near_duplicate", 0.8, None, "superseded").unwrap();
	}

	#[test]
	fn supersession_status_reports_current_family_member() {
		let db = setup_db();
		let old_version = insert_dated_document(&db, "v1", "2024-01-01 00:00:00");
		let new_version = insert_dated_document(&db, "v2", "2024-02-01 00:00:00");
		link_superseded(&db, new_version, old_version);
		add_tag(&db, old_version, "superseded").unwrap();

		let old_status = supersession_status(&db, old_version).unwrap();
		assert!(old_status.superseded);
		assert_eq!(old_status.current_document_id, Some(new_version));

		let new_status = supersession_status(&db, new_version).unwrap();
		assert!(!new_status.superseded);
		assert!(new_status.current_document_id.is_none());
	}

	#[test]
	fn supersession_status_derives_current_from_clip_date_not_insertion_order() {
		let db = setup_db();
		let newest_inserted_first = insert_dated_document(&db, "v2", "2024-06-01 00:00:00");
		let oldest_inserted_last = insert_dated_document(&db, "v1", "2024-01-01 00:00:00");
		link_superseded(&db, oldest_inserted_last, newest_inserted_first);
		add_tag(&db, oldest_inserted_last, "superseded").unwrap();

		let status = supersession_status(&db, oldest_inserted_last).unwrap();
		assert!(status.superseded);
		assert_eq!(status.current_document_id, Some(newest_inserted_first));
	}

	#[test]
	fn get_document_carries_supersession_annotation() {
		let db = setup_db();
		let old_version = insert_test_document(&db, "v1", "walrus content").0;
		let new_version = insert_test_document(&db, "v2", "walrus content revised").0;
		db.execute("UPDATE documents SET clip_date = '2024-02-01 00:00:00' WHERE id = ?1", [new_version]).unwrap();
		link_superseded(&db, new_version, old_version);
		add_tag(&db, old_version, "superseded").unwrap();

		let old_document = get_document(&db, old_version).unwrap().unwrap();
		assert!(old_document.superseded);
		assert_eq!(old_document.current_document_id, Some(new_version));
		assert!(!old_document.entries.is_empty());

		let new_document = get_document(&db, new_version).unwrap().unwrap();
		assert!(!new_document.superseded);
		assert!(new_document.current_document_id.is_none());
	}

	#[test]
	fn dump_documents_carry_supersession_annotation() {
		let db = setup_db();
		let old_version = insert_test_document(&db, "v1", "walrus content").0;
		let new_version = insert_test_document(&db, "v2", "walrus content revised").0;
		db.execute("UPDATE documents SET clip_date = '2024-02-01 00:00:00' WHERE id = ?1", [new_version]).unwrap();
		link_superseded(&db, new_version, old_version);
		add_tag(&db, old_version, "superseded").unwrap();

		let dumped = dump_document(&db, None).unwrap();
		let old_dump = dumped.iter().find(|d| d.document_id == old_version).unwrap();
		assert!(old_dump.superseded);
		assert_eq!(old_dump.current_document_id, Some(new_version));
		let new_dump = dumped.iter().find(|d| d.document_id == new_version).unwrap();
		assert!(!new_dump.superseded);
	}

	#[test]
	fn similar_claims_hide_excluded_source_documents_and_annotate_when_included() {
		let db = setup_db();
		let old_version = insert_test_document(&db, "v1", "walrus content").0;
		let new_version = insert_test_document(&db, "v2", "walrus content revised").0;
		db.execute("UPDATE documents SET clip_date = '2024-02-01 00:00:00' WHERE id = ?1", [new_version]).unwrap();
		link_superseded(&db, new_version, old_version);
		add_tag(&db, old_version, "superseded").unwrap();

		let old_claim = insert_claim(&db, old_version, None, None, "Walruses migrate.", "m", "h").unwrap();
		let new_claim = insert_claim(&db, new_version, None, None, "Walruses migrate seasonally.", "m", "h").unwrap();
		ensure_vec_claims_table(&db, 4).unwrap();
		insert_claim_embedding(&db, old_claim, &[1.0, 0.0, 0.0, 0.0]).unwrap();
		insert_claim_embedding(&db, new_claim, &[0.9, 0.1, 0.0, 0.0]).unwrap();
		let query = vec![1.0, 0.0, 0.0, 0.0];

		let hidden = find_similar_claims(&db, &query, 10, &["superseded".to_string()]).unwrap();
		assert_eq!(hidden.len(), 1);
		assert_eq!(hidden[0].claim_id, new_claim);
		assert!(!hidden[0].superseded);

		let shown = find_similar_claims(&db, &query, 10, &[]).unwrap();
		assert_eq!(shown.len(), 2);
		let old_hit = shown.iter().find(|c| c.claim_id == old_claim).unwrap();
		assert!(old_hit.superseded);
		assert_eq!(old_hit.current_document_id, Some(new_version));
	}

	#[test]
	fn connected_component_spans_transitive_family() {
		let db = setup_db();
		let v0 = insert_dated_document(&db, "v0", "2024-01-01 00:00:00");
		let v1 = insert_dated_document(&db, "v1", "2024-02-01 00:00:00");
		let v2 = insert_dated_document(&db, "v2", "2024-03-01 00:00:00");
		let v3 = insert_dated_document(&db, "v3", "2024-04-01 00:00:00");
		link_superseded(&db, v1, v0);
		link_superseded(&db, v2, v0);
		link_superseded(&db, v3, v0);

		let mut expected = vec![v0, v1, v2, v3];
		expected.sort();
		assert_eq!(connected_component(&db, v0).unwrap(), expected);
		assert_eq!(connected_component(&db, v2).unwrap(), expected);
	}

	#[test]
	fn kept_both_does_not_join_family() {
		let db = setup_db();
		let a = insert_dated_document(&db, "a", "2024-01-01 00:00:00");
		let b = insert_dated_document(&db, "b", "2024-02-01 00:00:00");
		insert_document_relation(&db, b, a, "near_duplicate", 0.5, Some(120), "kept_both").unwrap();

		assert_eq!(connected_component(&db, a).unwrap(), vec![a]);
		assert_eq!(connected_component(&db, b).unwrap(), vec![b]);
	}

	#[test]
	fn superseded_family_ordered_yields_chronological_order() {
		let db = setup_db();
		let v0 = insert_dated_document(&db, "v0", "2024-01-01 00:00:00");
		let v2 = insert_dated_document(&db, "v2", "2024-03-01 00:00:00");
		let v1 = insert_dated_document(&db, "v1", "2024-02-01 00:00:00");
		link_superseded(&db, v2, v0);
		link_superseded(&db, v1, v0);
		link_superseded(&db, v1, v2);

		let ordered = superseded_family_ordered(&db, v1).unwrap();
		assert_eq!(ordered.iter().map(|m| m.id).collect::<Vec<_>>(), vec![v0, v1, v2]);
		assert_eq!(ordered.last().unwrap().id, v2);
		assert!(ordered.iter().all(|m| m.clip_date_source == "ingest_fallback"));
	}

	#[test]
	fn backfill_populates_null_shingle_counts() {
		let db = setup_db();
		let body = "alpha beta gamma delta epsilon zeta eta theta";
		let sig = minhash::minhash(body);
		let doc_id = insert_document(
			&db, None, "Doc", Some("test"), MergeStrategy::None,
			Some("/test"), "2024-01-01 00:00:00", Some(&sig),
		).unwrap();
		let entry = make_entry(body, None);
		let entry_hash = minhash::minhash(body);
		insert_entry(
			&db, doc_id, &entry, 0, "Doc", "2024-01-01 00:00:00", "/test", &entry_hash,
		).unwrap();

		let before: Option<i64> = db.query_row(
			"SELECT document_shingle_count FROM documents WHERE id = ?1",
			[doc_id.0], |row| row.get(0),
		).unwrap();
		assert_eq!(before, None);

		backfill_shingle_counts(&db).unwrap();

		let after: Option<i64> = db.query_row(
			"SELECT document_shingle_count FROM documents WHERE id = ?1",
			[doc_id.0], |row| row.get(0),
		).unwrap();
		assert_eq!(after, Some(minhash::distinct_shingle_count(body) as i64));
	}

	#[test]
	fn resign_recomputes_legacy_width_signatures() {
		let db = setup_db();
		let body = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
		let sig = minhash::minhash(body);
		let doc_id = insert_document(
			&db, None, "Doc", Some("test"), MergeStrategy::None,
			Some("/test"), "2024-01-01 00:00:00", Some(&sig),
		).unwrap();
		let entry = make_entry(body, None);
		insert_entry(
			&db, doc_id, &entry, 0, "Doc", "2024-01-01 00:00:00", "/test", &sig,
		).unwrap();

		let legacy_blob: Vec<u8> = sig.iter().take(32).flat_map(|v| v.to_le_bytes()).collect();
		db.execute(
			"UPDATE documents SET document_minhash = ?1 WHERE id = ?2",
			rusqlite::params![legacy_blob, doc_id.0],
		).unwrap();
		db.execute(
			"UPDATE entries SET minhash = ?1 WHERE document_id = ?2",
			rusqlite::params![legacy_blob, doc_id.0],
		).unwrap();

		resign_stale_minhash_signatures(&db).unwrap();

		let expected_bytes = (crate::types::MINHASH_SIZE * 8) as i64;
		let doc_len: i64 = db.query_row(
			"SELECT length(document_minhash) FROM documents WHERE id = ?1",
			[doc_id.0], |row| row.get(0),
		).unwrap();
		let entry_len: i64 = db.query_row(
			"SELECT length(minhash) FROM entries WHERE document_id = ?1",
			[doc_id.0], |row| row.get(0),
		).unwrap();
		assert_eq!(doc_len, expected_bytes);
		assert_eq!(entry_len, expected_bytes);

		let candidates = find_dup_candidates(&db, "2024-01-02 00:00:00", 180).unwrap();
		let candidate = candidates.iter().find(|c| c.id == doc_id.0).unwrap();
		assert!((minhash::jaccard(&candidate.document_minhash, &sig) - 1.0).abs() < f64::EPSILON);
	}
}
