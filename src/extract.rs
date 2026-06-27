use anyhow::Result;
use rusqlite::Connection;

use crate::config::ExtractConfig;
use crate::llm::LlmBackend;
use crate::storage;
use crate::util;

pub struct ExtractOptions {
	pub force: bool,
	pub limit: Option<usize>,
	pub status: bool,
}

pub fn run_status(connection: &Connection) -> Result<()> {
	let total_docs: i64 = storage::document_count(connection)?;
	let docs_with_claims = storage::documents_with_claims_count(connection)?;
	let total_claims = storage::claim_count(connection)?;
	println!("Extraction status:");
	println!("  total documents:    {}", total_docs);
	println!("  with claims:        {}", docs_with_claims);
	println!("  missing:            {}", total_docs - docs_with_claims);
	println!("  total claims:       {}", total_claims);
	Ok(())
}

pub fn run(
	connection: &Connection,
	backend: &dyn LlmBackend,
	config: &ExtractConfig,
	options: &ExtractOptions,
	skip_doctypes: &std::collections::HashSet<String>,
) -> Result<()> {
	let prompt_hash = config.prompt_hash();

	let doc_ids: Vec<i64> = if options.force {
		let mut stmt = connection.prepare("SELECT id FROM documents")?;
		let ids = stmt.query_map([], |r| r.get(0))?.filter_map(|r| r.ok()).collect();
		ids
	} else {
		storage::get_documents_needing_extraction(connection, &config.model, &prompt_hash)?
	};

	if doc_ids.is_empty() {
		println!("no documents need extraction");
		return Ok(());
	}

	let doc_ids: Vec<i64> = match options.limit {
		Some(lim) => doc_ids.into_iter().take(lim).collect(),
		None => doc_ids,
	};

	println!("extracting claims from {} documents...", doc_ids.len());
	println!("  model: {}", config.model);

	let rules = config.get_rules();
	let mut framing_cache: std::collections::HashMap<String, Option<String>> = std::collections::HashMap::new();

	let mut total_claims = 0usize;
	let mut skipped = 0usize;
	for (i, doc_id) in doc_ids.iter().enumerate() {
		let (source_title, doctype_name): (String, Option<String>) = connection.query_row(
			"SELECT source_title, doctype_name FROM documents WHERE id = ?1",
			[doc_id],
			|row| Ok((row.get(0)?, row.get(1)?)),
		)?;

		if let Some(ref dt) = doctype_name {
			if skip_doctypes.contains(dt) {
				skipped += 1;
				continue;
			}
		}

		let framing_key = doctype_name.as_deref().unwrap_or("").to_string();
		let framing = framing_cache.entry(framing_key)
			.or_insert_with(|| config.get_framing(doctype_name.as_deref()))
			.clone();

		eprint!(
			"\r  [{}/{}] {}...",
			i + 1,
			doc_ids.len(),
			util::truncate_str(&source_title, 40),
		);

		storage::delete_claims_for_document(connection, *doc_id)?;

		let count = extract_document(
			connection, backend, config, *doc_id, &rules, framing.as_deref(), &prompt_hash,
		)?;
		total_claims += count;
	}
	eprintln!();
	if skipped > 0 {
		println!("done — {} claims extracted, {} documents skipped (no-extract doctype)", total_claims, skipped);
	} else {
		println!("done — {} claims extracted", total_claims);
	}
	Ok(())
}

fn extract_document(
	connection: &Connection,
	backend: &dyn LlmBackend,
	config: &ExtractConfig,
	document_id: i64,
	rules: &str,
	framing: Option<&str>,
	prompt_hash: &str,
) -> Result<usize> {
	let full_text = storage::get_document_full_text(connection, document_id)?;
	let prompt = crate::prompts::claim_extraction_prompt(&full_text, rules, framing);
	let response = backend.chat(&prompt, &config.model)?;
	let claims = parse_claims(&response);

	let author = resolve_author(connection, document_id);

	for content in &claims {
		storage::insert_claim(
			connection,
			document_id,
			None,
			author.as_deref(),
			content,
			&config.model,
			prompt_hash,
		)?;
	}

	Ok(claims.len())
}

fn strip_line_prefix(line: &str) -> &str {
	let s = line.trim();
	if s.starts_with("- ") {
		return s[2..].trim();
	}
	if s.starts_with("* ") {
		return s[2..].trim();
	}
	if let Some(rest) = s.strip_prefix('[') {
		if let Some(bracket_end) = rest.find(']') {
			let after = rest[bracket_end + 1..].trim();
			if !after.is_empty() {
				return after;
			}
		}
	}
	if let Some(dot_pos) = s.find(". ") {
		let prefix = &s[..dot_pos];
		if prefix.len() <= 3 && prefix.chars().all(|c| c.is_ascii_digit()) {
			return s[dot_pos + 2..].trim();
		}
	}
	s
}

fn parse_claims(response: &str) -> Vec<String> {
	let mut claims = Vec::new();
	for line in response.lines() {
		let stripped = strip_line_prefix(line);
		if stripped.is_empty() {
			continue;
		}
		if looks_like_preamble(stripped) {
			continue;
		}
		claims.push(stripped.to_string());
	}
	claims
}

fn looks_like_preamble(line: &str) -> bool {
	let lower = line.to_lowercase();
	lower.starts_with("here are")
		|| lower.starts_with("the following")
		|| lower.starts_with("below are")
		|| lower.starts_with("claims:")
		|| lower.starts_with("claims extracted")
		|| lower.starts_with("note:")
		|| lower.starts_with("---")
}

fn resolve_author(connection: &Connection, document_id: i64) -> Option<String> {
	let mut stmt = connection.prepare(
		"SELECT DISTINCT author FROM entries WHERE document_id = ?1 AND author IS NOT NULL"
	).ok()?;
	let authors: Vec<String> = stmt
		.query_map([document_id], |r| r.get(0))
		.ok()?
		.filter_map(|r| r.ok())
		.collect();
	if authors.len() == 1 {
		Some(authors.into_iter().next().unwrap())
	} else {
		None
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn strip_plain_line() {
		assert_eq!(strip_line_prefix("A simple claim."), "A simple claim.");
	}

	#[test]
	fn strip_bullet_dash() {
		assert_eq!(strip_line_prefix("- A claim with dash."), "A claim with dash.");
	}

	#[test]
	fn strip_bullet_star() {
		assert_eq!(strip_line_prefix("* A claim with star."), "A claim with star.");
	}

	#[test]
	fn strip_numbered() {
		assert_eq!(strip_line_prefix("1. First claim."), "First claim.");
		assert_eq!(strip_line_prefix("12. Twelfth claim."), "Twelfth claim.");
	}

	#[test]
	fn strip_bracket_label() {
		assert_eq!(
			strip_line_prefix("[observation] The F1 score was 0.91."),
			"The F1 score was 0.91.",
		);
	}

	#[test]
	fn strip_preserves_content_only_bracket() {
		assert_eq!(strip_line_prefix("[standalone]"), "[standalone]");
	}

	#[test]
	fn parse_claims_filters_preamble_and_blanks() {
		let response = "\
Here are the claims extracted from the document:

The system uses SQLite for storage.
- Claims are extracted per document.
1. Embeddings enable semantic search.

Note: some claims may overlap.";
		let claims = parse_claims(response);
		assert_eq!(claims.len(), 3);
		assert_eq!(claims[0], "The system uses SQLite for storage.");
		assert_eq!(claims[1], "Claims are extracted per document.");
		assert_eq!(claims[2], "Embeddings enable semantic search.");
	}
}
