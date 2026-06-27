use anyhow::{Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::chunking;
use crate::markdown;
use crate::minhash;
use crate::projects::{self, Manifest, ProjectRegistration};
use crate::storage;
use crate::types::{DocumentId, SegmentedEntry};

pub struct SyncSummary {
	pub new: usize,
	pub updated: usize,
	pub unchanged: usize,
	pub missing: usize,
}

pub fn run(connection: &Connection, config_dir: &Path, project_filter: Option<&str>) -> Result<()> {
	let registrations = projects::load_registry(config_dir)?;
	if registrations.is_empty() {
		eprintln!("no projects registered (add [[project]] entries to projects.toml)");
		return Ok(());
	}

	let selected: Vec<ProjectRegistration> = match project_filter {
		Some(name) => registrations.into_iter().filter(|r| r.name == name).collect(),
		None => registrations,
	};
	if selected.is_empty() {
		if let Some(name) = project_filter {
			anyhow::bail!("no registered project named '{}'", name);
		}
		return Ok(());
	}

	for registration in &selected {
		let summary = sync_project(connection, registration)?;
		eprintln!(
			"{}: {} new, {} updated, {} unchanged, {} missing",
			registration.name, summary.new, summary.updated, summary.unchanged, summary.missing,
		);
	}
	Ok(())
}

fn sync_project(connection: &Connection, registration: &ProjectRegistration) -> Result<SyncSummary> {
	let manifest = Manifest::load(&registration.manifest_path)?;

	let mut files = Vec::new();
	collect_files(&registration.root, &registration.root, &mut files)?;
	let on_disk: HashSet<String> = files.iter().map(|(_, relative)| relative.clone()).collect();

	let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
	let mut summary = SyncSummary { new: 0, updated: 0, unchanged: 0, missing: 0 };

	for (absolute, relative) in files {
		let Some(rule) = manifest.match_path(&relative) else {
			continue;
		};

		let bytes = std::fs::read(&absolute)
			.with_context(|| format!("reading {}", absolute.display()))?;
		let content_hash = hash_bytes(&bytes);
		let status = rule.status.as_str();
		let role = rule.role.as_deref();

		match storage::get_project_document(connection, &registration.name, &relative)? {
			Some((document_id, existing_hash)) => {
				if existing_hash.as_deref() == Some(content_hash.as_str()) {
					storage::update_project_document(connection, document_id, &content_hash, status, role, &now)?;
					summary.unchanged += 1;
				} else {
					let entries = parse_markdown(&bytes);
					let transaction = connection.unchecked_transaction()?;
					storage::replace_document_children(&transaction, document_id)?;
					insert_entries(&transaction, DocumentId(document_id), &relative, &absolute, &now, &entries)?;
					storage::update_project_document(&transaction, document_id, &content_hash, status, role, &now)?;
					transaction.commit()?;
					summary.updated += 1;
				}
			}
			None => {
				let entries = parse_markdown(&bytes);
				let transaction = connection.unchecked_transaction()?;
				let document_id = storage::insert_project_document(
					&transaction, &registration.name, &relative, &content_hash, status, role, &now,
				)?;
				insert_entries(&transaction, document_id, &relative, &absolute, &now, &entries)?;
				transaction.commit()?;
				summary.new += 1;
			}
		}
	}

	for (document_id, relative_path) in storage::list_project_documents(connection, &registration.name)? {
		if !on_disk.contains(&relative_path) {
			storage::set_document_missing(connection, document_id, &now)?;
			summary.missing += 1;
		}
	}

	Ok(summary)
}

fn parse_markdown(bytes: &[u8]) -> Vec<SegmentedEntry> {
	let body = String::from_utf8_lossy(bytes);
	markdown::parse_markdown_sections(&body)
}

fn insert_entries(
	connection: &Connection,
	document_id: DocumentId,
	source_title: &str,
	absolute: &Path,
	clip_date: &str,
	entries: &[SegmentedEntry],
) -> Result<()> {
	let file_path = absolute.to_string_lossy();
	for (position, entry) in entries.iter().enumerate() {
		let hash = minhash::minhash(&entry.body);
		let entry_id = storage::insert_entry(
			connection,
			document_id,
			entry,
			position as u32,
			source_title,
			clip_date,
			&file_path,
			&hash,
		)?;
		let chunks = chunking::chunk_text(&entry.body);
		storage::insert_chunks(connection, entry_id, &chunks)?;
	}
	Ok(())
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, String)>) -> Result<()> {
	let read = std::fs::read_dir(dir)
		.with_context(|| format!("reading directory {}", dir.display()))?;
	for entry in read {
		let entry = entry?;
		let name = entry.file_name();
		if name.to_string_lossy().starts_with('.') {
			continue;
		}
		let path = entry.path();
		let file_type = entry.file_type()?;
		if file_type.is_dir() {
			collect_files(root, &path, out)?;
		} else if file_type.is_file() {
			if let Some(relative) = relative_path(root, &path) {
				out.push((path, relative));
			}
		}
	}
	Ok(())
}

fn relative_path(root: &Path, path: &Path) -> Option<String> {
	let stripped = path.strip_prefix(root).ok()?;
	let parts: Vec<String> = stripped
		.components()
		.map(|c| c.as_os_str().to_string_lossy().into_owned())
		.collect();
	Some(parts.join("/"))
}

fn hash_bytes(bytes: &[u8]) -> String {
	use std::fmt::Write;
	let mut hasher = Sha256::new();
	hasher.update(bytes);
	let mut hex = String::new();
	for byte in hasher.finalize() {
		let _ = write!(hex, "{:02x}", byte);
	}
	hex
}
