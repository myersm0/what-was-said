use anyhow::Result;
use rusqlite::Connection;

use std::collections::HashMap;

use crate::config::ExtractConfig;
use crate::llm::LlmBackend;
use crate::prompts;
use crate::storage;
use crate::util;

pub struct ExtractOptions {
	pub force: bool,
	pub limit: Option<usize>,
	pub status: bool,
}

pub fn run_status(connection: &Connection) -> Result<()> {
	let total_docs: i64 = storage::document_count(connection)?;
	let extracted = storage::extracted_documents_count(connection)?;
	let docs_with_claims = storage::documents_with_claims_count(connection)?;
	let total_claims = storage::claim_count(connection)?;
	println!("Extraction status:");
	println!("  total documents:    {}", total_docs);
	println!("  extracted:          {}", extracted);
	println!("  with claims:        {}", docs_with_claims);
	println!("  never extracted:    {}", total_docs - extracted);
	println!("  total claims:       {}", total_claims);
	Ok(())
}

/// A doctype's framing and the prompt hash it implies, read once per run.
struct Framing {
	text: Option<String>,
	prompt_hash: String,
}

fn framing_for<'a>(
	framings: &'a mut HashMap<Option<String>, Framing>,
	config: &ExtractConfig,
	rules: &str,
	doctype: &Option<String>,
) -> &'a Framing {
	framings.entry(doctype.clone()).or_insert_with(|| {
		let text = config.get_framing(doctype.as_deref());
		let prompt_hash = prompts::compute_extract_prompt_hash(rules, text.as_deref());
		Framing { text, prompt_hash }
	})
}

pub fn run(
	connection: &Connection,
	backend: &dyn LlmBackend,
	config: &ExtractConfig,
	options: &ExtractOptions,
	skip_doctypes: &std::collections::HashSet<String>,
) -> Result<()> {
	let rules = config.get_rules();
	let mut framings: HashMap<Option<String>, Framing> = HashMap::new();

	let mut doc_ids = Vec::new();
	let mut skipped = 0usize;
	for state in storage::extraction_states(connection)? {
		if state.doctype_name.as_ref().is_some_and(|doctype| skip_doctypes.contains(doctype)) {
			skipped += 1;
			continue;
		}
		let expected_hash = &framing_for(&mut framings, config, &rules, &state.doctype_name).prompt_hash;
		let current = state.extracted_with
			.is_some_and(|(model, prompt_hash)| model == config.model && &prompt_hash == expected_hash);
		if options.force || !current {
			doc_ids.push((state.document_id, state.doctype_name));
		}
	}

	if doc_ids.is_empty() {
		println!("no documents need extraction");
		return Ok(());
	}
	if skipped > 0 {
		println!("  skipping {} documents with a no-extract doctype", skipped);
	}
	if let Some(limit) = options.limit {
		doc_ids.truncate(limit);
	}

	println!("extracting claims from {} documents...", doc_ids.len());
	println!("  model: {}", config.model);

	let mut total_claims = 0usize;
	for (i, (doc_id, doctype)) in doc_ids.iter().enumerate() {
		let source_title: String = connection.query_row(
			"SELECT source_title FROM documents WHERE id = ?1",
			[doc_id],
			|row| row.get(0),
		)?;
		eprint!(
			"\r  [{}/{}] {}...",
			i + 1,
			doc_ids.len(),
			util::truncate_str(&source_title, 40),
		);

		let framing = framing_for(&mut framings, config, &rules, doctype);
		storage::delete_claims_for_document(connection, *doc_id)?;
		let count = extract_document(
			connection, backend, config, *doc_id, &rules, framing.text.as_deref(), &framing.prompt_hash,
		)?;
		storage::record_extraction(connection, *doc_id, &config.model, &framing.prompt_hash, count)?;
		total_claims += count;
	}
	eprintln!();
	println!("done — {} claims extracted", total_claims);
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
	let prompt = prompts::claim_extraction_prompt(&full_text, rules, framing);
	let response = backend.chat_sampled(&prompt, &config.model, config.sampling)?;
	let claims = parse_claims(&response);

	let authors = document_authors(connection, document_id)?;
	let sole_author = (authors.len() == 1).then(|| authors[0].clone());

	for claim in &claims {
		let author = claim.speaker.as_deref()
			.and_then(|speaker| match_author(speaker, &authors))
			.or(sole_author.as_deref());
		storage::insert_claim(
			connection,
			document_id,
			None,
			author,
			&claim.content,
			&config.model,
			prompt_hash,
		)?;
	}

	Ok(claims.len())
}

#[derive(Debug, PartialEq)]
struct ParsedClaim {
	/// A leading `[...]` label, which the conversation framing asks to be the
	/// speaker. Kept as the author only if it names one of the document's.
	speaker: Option<String>,
	content: String,
}

/// Strips a bullet or number, then splits off a leading `[label]`.
fn split_claim_line(line: &str) -> (Option<&str>, &str) {
	let mut s = line.trim();
	if let Some(rest) = s.strip_prefix("- ").or_else(|| s.strip_prefix("* ")) {
		s = rest.trim();
	} else if let Some(dot_pos) = s.find(". ") {
		let prefix = &s[..dot_pos];
		if !prefix.is_empty() && prefix.len() <= 3 && prefix.chars().all(|c| c.is_ascii_digit()) {
			s = s[dot_pos + 2..].trim();
		}
	}
	if let Some(rest) = s.strip_prefix('[') {
		if let Some(bracket_end) = rest.find(']') {
			let after = rest[bracket_end + 1..].trim();
			if !after.is_empty() {
				return (Some(rest[..bracket_end].trim()), after);
			}
		}
	}
	(None, s)
}

fn parse_claims(response: &str) -> Vec<ParsedClaim> {
	response
		.lines()
		.map(split_claim_line)
		.filter(|(_, content)| !content.is_empty() && !looks_like_preamble(content))
		.map(|(speaker, content)| ParsedClaim {
			speaker: speaker.filter(|s| !s.is_empty()).map(str::to_string),
			content: content.to_string(),
		})
		.collect()
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

fn document_authors(connection: &Connection, document_id: i64) -> Result<Vec<String>> {
	let mut stmt = connection.prepare(
		"SELECT DISTINCT author FROM entries WHERE document_id = ?1 AND author IS NOT NULL ORDER BY author",
	)?;
	let authors = stmt
		.query_map([document_id], |row| row.get(0))?
		.collect::<std::result::Result<Vec<String>, _>>()?;
	Ok(authors)
}

/// Case, spacing and a trailing `<address>` are ignored, so a speaker the
/// model shortened from "Alice Wilson <alice@example.org>" still matches.
fn normalize_name(name: &str) -> String {
	let without_address = match (name.find('<'), name.rfind('>')) {
		(Some(start), Some(end)) if start < end && name[end + 1..].trim().is_empty() => &name[..start],
		_ => name,
	};
	without_address.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn match_author<'a>(speaker: &str, authors: &'a [String]) -> Option<&'a str> {
	let speaker = normalize_name(speaker);
	if speaker.is_empty() {
		return None;
	}
	authors.iter().find(|author| normalize_name(author) == speaker).map(String::as_str)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn content(line: &str) -> &str {
		split_claim_line(line).1
	}

	#[test]
	fn strips_bullets_numbers_and_labels() {
		assert_eq!(content("A simple claim."), "A simple claim.");
		assert_eq!(content("- A claim with dash."), "A claim with dash.");
		assert_eq!(content("* A claim with star."), "A claim with star.");
		assert_eq!(content("1. First claim."), "First claim.");
		assert_eq!(content("12. Twelfth claim."), "Twelfth claim.");
		assert_eq!(split_claim_line("[observation] The F1 score was 0.91."), (Some("observation"), "The F1 score was 0.91."));
		assert_eq!(split_claim_line("- [Alice Wilson] Use --no-stat-cache."), (Some("Alice Wilson"), "Use --no-stat-cache."));
		assert_eq!(split_claim_line("[standalone]"), (None, "[standalone]"));
	}

	#[test]
	fn parse_claims_filters_preamble_and_blanks() {
		let response = "\
Here are the claims extracted from the document:

The system uses SQLite for storage.
- Claims are extracted per document.
1. [Bob Park] Embeddings enable semantic search.

Note: some claims may overlap.";
		let claims = parse_claims(response);
		let contents: Vec<&str> = claims.iter().map(|claim| claim.content.as_str()).collect();
		assert_eq!(contents, vec![
			"The system uses SQLite for storage.",
			"Claims are extracted per document.",
			"Embeddings enable semantic search.",
		]);
		assert_eq!(claims[2].speaker.as_deref(), Some("Bob Park"));
		assert_eq!(claims[0].speaker, None);
	}

   #[test]
	fn speakers_match_only_the_documents_own_authors() {
		let authors = vec!["Alice Wilson".to_string(), "Patel, Ravi <patelr@example.org>".to_string()];
		assert_eq!(match_author("alice  wilson", &authors), Some("Alice Wilson"));
		assert_eq!(match_author("Patel, Ravi", &authors), Some("Patel, Ravi <patelr@example.org>"));
		assert_eq!(match_author("Ravi Patel", &authors), None);
		assert_eq!(match_author("observation", &authors), None);
		assert_eq!(match_author("", &authors), None);
	}

	struct ScriptedBackend {
		response: String,
		sampling: std::sync::Mutex<Option<crate::llm::Sampling>>,
	}

	impl LlmBackend for ScriptedBackend {
		fn generate(&self, _: &str, _: &str, _: Option<&str>, _: Option<&str>) -> Result<String> {
			Ok(self.response.clone())
		}

		fn chat_sampled(&self, _: &str, _: &str, sampling: crate::llm::Sampling) -> Result<String> {
			*self.sampling.lock().unwrap() = Some(sampling);
			Ok(self.response.clone())
		}

		fn embed(&self, _: &str, _: &str) -> Result<Vec<f32>> {
			anyhow::bail!("not used")
		}
	}

	fn setup_db() -> Connection {
		let connection = Connection::open_in_memory().unwrap();
		storage::initialize(&connection).unwrap();
		connection
	}

	fn insert_thread(connection: &Connection, messages: &[(&str, &str)]) -> i64 {
		let clip_date = "2026-10-01 09:00:00";
		let document_id = storage::insert_document(
			connection, None, "thread", Some("slack"), crate::types::MergeStrategy::None,
			Some("/test"), clip_date, None,
		).unwrap();
		for (position, (author, body)) in messages.iter().enumerate() {
			let entry = crate::types::SegmentedEntry {
				start_line: 1,
				end_line: 1,
				body: body.to_string(),
				author: Some(author.to_string()),
				timestamp: None,
				is_quote: false,
				heading_level: None,
				heading_title: None,
			};
			storage::insert_entry(
				connection, document_id, &entry, position as u32, "thread", clip_date, "/test",
				&crate::minhash::minhash(body),
			).unwrap();
		}
		document_id.0
	}

	#[test]
	fn extraction_attributes_matched_speakers_and_records_every_run() {
		let db = setup_db();
		let thread = insert_thread(&db, &[
			("Alice Wilson", "Try --no-stat-cache."),
			("Bob Park", "That fixed it."),
		]);
		let backend = ScriptedBackend {
			response: "[Alice Wilson] Alice Wilson suggested the --no-stat-cache flag.\n\
				[Carol] The flag is undocumented.\n\
				The rebuild problem was resolved.".to_string(),
			sampling: std::sync::Mutex::new(None),
		};
		let config = ExtractConfig::default();
		let options = ExtractOptions { force: false, limit: None, status: false };
		run(&db, &backend, &config, &options, &Default::default()).unwrap();

		let claims = storage::get_claims_for_document(&db, thread).unwrap();
		let authors: Vec<Option<&str>> = claims.iter().map(|claim| claim.author.as_deref()).collect();
		assert_eq!(authors, vec![Some("Alice Wilson"), None, None]);
		assert_eq!(claims[1].content, "The flag is undocumented.");
		assert_eq!(*backend.sampling.lock().unwrap(), Some(config.sampling));

		let empty = ScriptedBackend { response: String::new(), sampling: std::sync::Mutex::new(None) };
		run(&db, &empty, &config, &ExtractOptions { force: true, ..options }, &Default::default()).unwrap();
		assert_eq!(storage::extracted_documents_count(&db).unwrap(), 1);
		assert!(storage::extraction_states(&db).unwrap()[0].extracted_with.is_some());
		run(&db, &backend, &config, &options, &Default::default()).unwrap();
		assert!(storage::get_claims_for_document(&db, thread).unwrap().is_empty(), "a zero-claim run is not repeated");
	}
}
