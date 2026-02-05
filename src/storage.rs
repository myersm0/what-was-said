use anyhow::Result;
use rusqlite::Connection;

pub fn initialize(connection: &Connection) -> Result<()> {
	connection.execute_batch(
		"
		CREATE TABLE IF NOT EXISTS documents (
			id INTEGER PRIMARY KEY,
			collection TEXT NOT NULL CHECK (collection IN ('personal', 'work')),
			source_title TEXT NOT NULL,
			merge_strategy TEXT NOT NULL CHECK (merge_strategy IN ('none', 'positional', 'timestamped')),
			origin_path TEXT
		);

		CREATE TABLE IF NOT EXISTS entries (
			id INTEGER PRIMARY KEY,
			document_id INTEGER NOT NULL REFERENCES documents(id),
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
			is_contaminated INTEGER NOT NULL DEFAULT 0,
			minhash BLOB NOT NULL
		);

		CREATE TABLE IF NOT EXISTS media (
			id INTEGER PRIMARY KEY,
			file_path TEXT NOT NULL,
			media_type TEXT NOT NULL CHECK (media_type IN ('screenshot', 'audio', 'transcript_segment')),
			timestamp TEXT NOT NULL,
			duration_seconds REAL,
			document_id INTEGER REFERENCES documents(id)
		);

		CREATE TABLE IF NOT EXISTS timeline_links (
			media_id INTEGER NOT NULL REFERENCES media(id),
			entry_id INTEGER NOT NULL REFERENCES entries(id),
			PRIMARY KEY (media_id, entry_id)
		);

		CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
			body,
			author,
			source_title,
			content=entries,
			content_rowid=id
		);

		CREATE TRIGGER IF NOT EXISTS entries_fts_insert AFTER INSERT ON entries BEGIN
			INSERT INTO entries_fts(rowid, body, author, source_title)
			VALUES (new.id, new.body, new.author, new.source_title);
		END;

		CREATE TRIGGER IF NOT EXISTS entries_fts_delete AFTER DELETE ON entries BEGIN
			INSERT INTO entries_fts(entries_fts, rowid, body, author, source_title)
			VALUES ('delete', old.id, old.body, old.author, old.source_title);
		END;

		CREATE TRIGGER IF NOT EXISTS entries_fts_update AFTER UPDATE ON entries BEGIN
			INSERT INTO entries_fts(entries_fts, rowid, body, author, source_title)
			VALUES ('delete', old.id, old.body, old.author, old.source_title);
			INSERT INTO entries_fts(rowid, body, author, source_title)
			VALUES (new.id, new.body, new.author, new.source_title);
		END;
		",
	)?;
	Ok(())
}
