mod documents;
mod search;
mod embed;
mod tags;
mod derive;

pub use documents::*;
pub use search::*;
pub use embed::*;
pub use tags::*;
pub use derive::*;

use anyhow::Result;
use rusqlite::Connection;

use crate::types::MergeStrategy;

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
			clip_date TEXT NOT NULL
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

		CREATE TABLE IF NOT EXISTS chunk_embeddings (
			chunk_id INTEGER PRIMARY KEY REFERENCES chunks(id) ON DELETE CASCADE,
			embedding BLOB NOT NULL
		);

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
			Some("/test"), "2024-01-01 00:00:00",
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
		let results = search(&db, "rust", SearchSortColumn::Score).unwrap();
		assert_eq!(results.len(), 1);
		assert_eq!(results[0].source_title, "Rust Guide");
	}

	#[test]
	fn fts5_search_no_results() {
		let db = setup_db();
		insert_test_document(&db, "Doc", "some content");
		let results = search(&db, "nonexistent", SearchSortColumn::Score).unwrap();
		assert!(results.is_empty());
	}

	#[test]
	fn fts5_search_prefix_matching() {
		let db = setup_db();
		insert_test_document(&db, "Doc", "the cathedral and the bazaar");
		let results = search(&db, "cathed", SearchSortColumn::Score).unwrap();
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
			&db, None, "Beta", Some("test"), MergeStrategy::None, None, "2024-01-01 00:00:00",
		).unwrap();
		insert_entry(&db, doc1, &entry, 0, "Beta", "2024-01-01 00:00:00", "/a", &hash).unwrap();

		let doc2 = insert_document(
			&db, None, "Alpha", Some("test"), MergeStrategy::None, None, "2024-06-01 00:00:00",
		).unwrap();
		insert_entry(&db, doc2, &entry, 0, "Alpha", "2024-06-01 00:00:00", "/b", &hash).unwrap();

		let by_date = list_documents(&db, SortColumn::Date, SortDirection::Descending).unwrap();
		assert_eq!(by_date[0].source_title, "Alpha");

		let by_source = list_documents(&db, SortColumn::Source, SortDirection::Ascending).unwrap();
		assert_eq!(by_source[0].source_title, "Alpha");
	}
}
