use rusqlite::Connection;

use what_was_said::chunking;
use what_was_said::config;
use what_was_said::ingest;
use what_was_said::markdown;
use what_was_said::minhash;
use what_was_said::storage::{self, SearchSortColumn, SortColumn, SortDirection};
use what_was_said::types::*;
use what_was_said::util;

fn setup_db() -> Connection {
	let connection = Connection::open_in_memory().unwrap();
	storage::initialize(&connection).unwrap();
	connection
}

fn ingest_text(
	connection: &Connection,
	source_title: &str,
	body: &str,
	doctype_name: Option<&str>,
) -> DocumentId {
	let entries = markdown::parse_markdown_sections(body);
	let source_title = util::normalize_to_ascii(source_title);
	let doc_id = storage::insert_document(
		connection,
		None,
		&source_title,
		doctype_name,
		MergeStrategy::None,
		Some("/test/file.txt"),
		"2024-06-15 10:00:00",
		None,
	).unwrap();

	for (position, entry) in entries.iter().enumerate() {
		let hash = minhash::minhash(&entry.body);
		let entry_id = storage::insert_entry(
			connection, doc_id, entry, position as u32,
			&source_title, "2024-06-15 10:00:00", "/test/file.txt", &hash,
		).unwrap();
		let chunks = chunking::chunk_text(&entry.body);
		storage::insert_chunks(connection, entry_id, &chunks).unwrap();
	}

	doc_id
}

#[test]
fn ingest_markdown_and_search() {
	let db = setup_db();

	let body = "# Introduction\n\n\
		This document covers Rust memory safety.\n\n\
		# Details\n\n\
		The borrow checker enforces ownership rules at compile time.\n\
		This prevents data races and dangling pointers.";

	ingest_text(&db, "Rust Safety Guide - Brave", &body, Some("markdown"));

	let results = storage::search(&db, "borrow checker", SearchSortColumn::Score).unwrap();
	assert_eq!(results.len(), 1);
	assert!(results[0].chunks.iter().any(|c| c.chunk_body.contains("borrow checker")));
}

#[test]
fn ingest_multiple_documents_search_ranks() {
	let db = setup_db();

	ingest_text(&db, "Rust Book", "Rust provides memory safety through ownership.", Some("markdown"));
	ingest_text(&db, "Python Book", "Python uses garbage collection for memory management.", Some("markdown"));
	ingest_text(&db, "Cooking Guide", "Preheat the oven to 350 degrees.", Some("markdown"));

	let results = storage::search(&db, "memory", SearchSortColumn::Score).unwrap();
	assert_eq!(results.len(), 2);

	let titles: Vec<&str> = results.iter().map(|r| r.source_title.as_str()).collect();
	assert!(titles.contains(&"Rust Book"));
	assert!(titles.contains(&"Python Book"));
	assert!(!titles.contains(&"Cooking Guide"));
}

#[test]
fn source_header_to_ingest_flow() {
	let raw = "# source: My Research Paper \u{2014} ArXiv - Brave\nSome abstract content here.";
	let lines: Vec<&str> = raw.lines().collect();

	let source_title = ingest::parse_source_header(lines[0]).unwrap();
	assert!(source_title.contains("ArXiv"));

	let clean_title = util::normalize_to_ascii(&source_title);
	assert!(clean_title.contains("-"));
	assert!(!clean_title.contains("\u{2014}"));

	let merge_key = util::strip_source_suffix(&clean_title);
	assert!(!merge_key.contains("Brave"));
}

#[test]
fn tag_filtering_in_document_list() {
	let db = setup_db();
	let doc1 = ingest_text(&db, "Tagged Doc", "has a tag", None);
	let doc2 = ingest_text(&db, "Untagged Doc", "no tag", None);

	storage::add_tag(&db, doc1.0, "important").unwrap();

	let all = storage::list_documents(&db, SortColumn::Date, SortDirection::Descending).unwrap();
	assert_eq!(all.len(), 2);

	let tagged_ids = storage::get_document_ids_by_tag(&db, "important").unwrap();
	assert_eq!(tagged_ids, vec![doc1.0]);
}

#[test]
fn copilot_email_ingest_preserves_authors() {
	let db = setup_db();

	let email_text = "From: Alice Smith\nDate: 2024-03-01\nSubject: Project Update\n\n\
		The project is on track.\nEMAIL\n\
		From: Bob Jones\nDate: 2024-03-02\nSubject: Re: Project Update\n\n\
		Sounds good, thanks for the update.";

	let entries = ingest::parse_copilot_email_summary(email_text);
	assert_eq!(entries.len(), 2);

	let doc_id = storage::insert_document(
		&db, None, "Project Thread", Some("copilot_email"),
		MergeStrategy::Positional, None, "2024-03-01 00:00:00", None,
	).unwrap();

	for (i, entry) in entries.iter().enumerate() {
		let hash = minhash::minhash(&entry.body);
		let entry_id = storage::insert_entry(
			&db, doc_id, entry, i as u32,
			"Project Thread", "2024-03-01 00:00:00", "/test", &hash,
		).unwrap();
		let chunks = chunking::chunk_text(&entry.body);
		storage::insert_chunks(&db, entry_id, &chunks).unwrap();
	}

	let doc = storage::get_document(&db, doc_id.0).unwrap().unwrap();
	assert_eq!(doc.entries.len(), 2);
	assert_eq!(doc.entries[0].author.as_deref(), Some("Alice Smith"));
	assert_eq!(doc.entries[1].author.as_deref(), Some("Bob Jones"));

	let results = storage::search(&db, "update", SearchSortColumn::Score).unwrap();
	assert!(!results.is_empty());
}

#[test]
fn doctype_detection() {
	let config = config::default_config();

	let slack = config.detect("Channel - general - Slack - Brave", None);
	assert!(slack.is_some());
	assert_eq!(slack.unwrap().name, "slack");

	let claude = config.detect("My Chat - Claude - Brave", None);
	assert!(claude.is_some());
	assert_eq!(claude.unwrap().name, "claude");

	let unknown = config.detect("Random Page Title", None);
	assert!(unknown.is_none());

	let by_ext = config.detect("notes", Some("md"));
	assert!(by_ext.is_some());
	assert_eq!(by_ext.unwrap().name, "markdown");
}

#[test]
fn content_sniffing_fallback() {
	let config = config::default_config();

	let markdown_content = "# Heading\n\nSome text\n\n## Another heading\n\nMore text";
	let detected = config.detect_with_content("Unknown Title", None, markdown_content);
	assert!(detected.is_some());
	assert_eq!(detected.unwrap().parser, config::Parser::Markdown);
}

#[test]
fn derive_status_on_empty_db() {
	let db = setup_db();
	let status = storage::get_derive_status(&db).unwrap();
	assert_eq!(status.total_docs, 0);
	assert_eq!(status.with_detailed, 0);
	assert_eq!(status.with_brief, 0);
}

#[test]
fn derive_status_tracks_content() {
	let db = setup_db();
	let doc_id = ingest_text(&db, "Doc", "content", None);

	storage::insert_derived_content(
		&db, doc_id.0, "detailed", "A summary", "model", "v1", None, None,
	).unwrap();

	let status = storage::get_derive_status(&db).unwrap();
	assert_eq!(status.total_docs, 1);
	assert_eq!(status.with_detailed, 1);
	assert_eq!(status.with_brief, 0);
}
