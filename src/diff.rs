use anyhow::Result;
use rusqlite::Connection;

use crate::llm::LlmBackend;
use crate::prompts;
use crate::storage::{self, RelationPair};
use crate::util;

pub fn run(connection: &Connection, backend: &dyn LlmBackend, model: &str, force: bool) -> Result<()> {
	let instructions = prompts::default_diff_instructions();
	let prompt_hash = prompts::compute_prompt_hash(instructions);

	let pairs: Vec<RelationPair> = if force {
		let mut stmt = connection.prepare(
			"SELECT id, from_document_id, to_document_id FROM document_relations"
		)?;
		let rows = stmt.query_map([], |row| {
			Ok(RelationPair {
				id: row.get(0)?,
				from_document_id: row.get(1)?,
				to_document_id: row.get(2)?,
			})
		})?
		.collect::<std::result::Result<Vec<_>, _>>()?;
		rows
	} else {
		storage::get_relations_needing_summary(connection, model, &prompt_hash)?
	};

	if pairs.is_empty() {
		println!("no document relations need a diff summary");
		return Ok(());
	}

	println!("summarizing {} document relation(s) with {}...", pairs.len(), model);

	let mut done = 0usize;
	for pair in &pairs {
		let new_text = storage::get_document_full_text(connection, pair.from_document_id)?;
		let existing_text = storage::get_document_full_text(connection, pair.to_document_id)?;
		let (added, removed) = util::diff_regions(&new_text, &existing_text);
		let prompt = prompts::document_diff_prompt(&added, &removed, instructions);
		match backend.chat(&prompt, model) {
			Ok(text) => {
				storage::set_relation_summary(connection, pair.id, &text, model, &prompt_hash)?;
				done += 1;
			}
			Err(error) => eprintln!("  relation {} failed: {}", pair.id, error),
		}
	}

	println!("summarized {} of {}", done, pairs.len());
	Ok(())
}
