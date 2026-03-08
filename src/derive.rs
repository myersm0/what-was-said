use anyhow::Result;
use rusqlite::Connection;

use crate::config::DeriveConfig;
use crate::ingest::OllamaClient;
use crate::storage;
use crate::util;

pub struct DeriveOptions {
	pub force: bool,
	pub missing: bool,
	pub stale: bool,
	pub bad_detailed: bool,
	pub bad_brief: bool,
	pub limit: Option<usize>,
}

pub fn run_status(connection: &Connection) -> Result<()> {
	let status = storage::get_derive_status(connection)?;
	println!("Derivation status:");
	println!("  total documents: {}", status.total_docs);
	println!("  with detailed:   {}", status.with_detailed);
	println!("  with brief:      {}", status.with_brief);
	println!("  detailed bad:    {}", status.detailed_bad);
	println!("  brief bad:       {}", status.brief_bad);
	println!("  missing:         {}", status.total_docs - status.with_detailed);
	Ok(())
}

pub fn run(
	connection: &Connection,
	ollama: &OllamaClient,
	derive_config: &DeriveConfig,
	options: &DeriveOptions,
) -> Result<()> {
	let do_missing = options.missing
		|| (!options.stale && !options.bad_detailed && !options.bad_brief && !options.force);

	let doc_ids: Vec<i64> = if options.force {
		let mut stmt = connection.prepare("SELECT id FROM documents")?;
		let ids = stmt.query_map([], |r| r.get(0))?.filter_map(|r| r.ok()).collect();
		ids
	} else {
		storage::get_documents_needing_derivation(
			connection,
			do_missing,
			options.stale,
			options.bad_detailed,
			options.bad_brief,
		)?
	};

	if doc_ids.is_empty() {
		println!("no documents need derivation");
		return Ok(());
	}

	let doc_ids: Vec<i64> = match options.limit {
		Some(lim) => doc_ids.into_iter().take(lim).collect(),
		None => doc_ids,
	};

	println!("deriving summaries for {} documents...", doc_ids.len());
	println!("  detailed model: {}", derive_config.detailed_model);
	println!("  brief model: {}", derive_config.brief_model);

	for (i, doc_id) in doc_ids.iter().enumerate() {
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

		let detailed_body = derive_detailed(
			connection, ollama, derive_config, *doc_id, options.force, options.stale,
		)?;

		derive_brief(
			connection, ollama, derive_config, *doc_id, &detailed_body, options.force,
		)?;
	}
	eprintln!();
	println!("done");
	Ok(())
}

fn derive_detailed(
	connection: &Connection,
	ollama: &OllamaClient,
	config: &DeriveConfig,
	doc_id: i64,
	force: bool,
	check_stale: bool,
) -> Result<String> {
	let existing = storage::get_derived_content(connection, doc_id, "detailed")?;

	let need_regen = force
		|| existing.is_none()
		|| existing.as_ref().map(|d| d.quality == "bad").unwrap_or(false)
		|| (check_stale && {
			let current_hash = storage::compute_document_source_hash(connection, doc_id)?;
			existing.as_ref()
				.and_then(|d| d.source_hash.as_ref())
				.map(|h| h != &current_hash)
				.unwrap_or(true)
		});

	if !need_regen {
		return Ok(existing.map(|d| d.body).unwrap_or_default());
	}

	let full_text = storage::get_document_full_text(connection, doc_id)?;
	let prompt = config.get_detailed_prompt(full_text.len());
	let full_prompt = format!("{}\n{}", prompt, full_text);
	let response = ollama.chat(&full_prompt, &config.detailed_model)?;
	let source_hash = storage::compute_document_source_hash(connection, doc_id)?;

	if let Some(row) = existing {
		storage::update_derived_content(
			connection, row.id, &response,
			&config.detailed_model, &config.prompt_version, Some(&source_hash),
		)?;
	} else {
		storage::insert_derived_content(
			connection, doc_id, "detailed", &response,
			&config.detailed_model, &config.prompt_version, Some(&source_hash), None,
		)?;
	}

	Ok(response)
}

fn derive_brief(
	connection: &Connection,
	ollama: &OllamaClient,
	config: &DeriveConfig,
	doc_id: i64,
	detailed_body: &str,
	force: bool,
) -> Result<()> {
	if detailed_body.is_empty() {
		return Ok(());
	}

	let existing = storage::get_derived_content(connection, doc_id, "brief")?;

	let need_regen = force
		|| existing.is_none()
		|| existing.as_ref().map(|b| b.quality == "bad").unwrap_or(false);

	if !need_regen {
		return Ok(());
	}

	let brief_prompt = config.get_brief_prompt();
	let full_prompt = format!("{}\n{}", brief_prompt, detailed_body);
	let response = ollama.chat(&full_prompt, &config.brief_model)?;

	let parent_id = storage::get_derived_content(connection, doc_id, "detailed")?
		.map(|d| d.id);

	if let Some(row) = existing {
		storage::update_derived_content(
			connection, row.id, &response,
			&config.brief_model, &config.prompt_version, None,
		)?;
	} else {
		storage::insert_derived_content(
			connection, doc_id, "brief", &response,
			&config.brief_model, &config.prompt_version, None, parent_id,
		)?;
	}

	Ok(())
}
