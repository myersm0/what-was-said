use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::types::{SegmentedEntry, SegmentationResult};

#[derive(Serialize)]
struct OllamaRequest {
	model: String,
	prompt: String,
	system: String,
	stream: bool,
	format: String,
}

#[derive(Deserialize)]
struct OllamaResponse {
	response: String,
}

#[derive(Deserialize)]
struct SegmentationJson {
	entries: Vec<SegmentedEntryJson>,
}

#[derive(Deserialize)]
struct SegmentedEntryJson {
	#[serde(default)]
	start_line: usize,
	#[serde(default)]
	end_line: usize,
	body_start_line: usize,
	body_end_line: usize,
	author: Option<String>,
	timestamp: Option<String>,
}

pub struct SegmentationOptions {
	pub doctype_prompt: Option<String>,
	pub cleanup_patterns: Vec<Regex>,
	pub merge_consecutive_same_author: bool,
}

impl Default for SegmentationOptions {
	fn default() -> Self {
		SegmentationOptions {
			doctype_prompt: None,
			cleanup_patterns: Vec::new(),
			merge_consecutive_same_author: false,
		}
	}
}

pub struct OllamaClient {
	pub base_url: String,
	pub model: String,
	client: reqwest::blocking::Client,
}

impl OllamaClient {
	pub fn new(base_url: &str, model: &str) -> Self {
		OllamaClient {
			base_url: base_url.to_string(),
			model: model.to_string(),
			client: reqwest::blocking::Client::builder()
				.timeout(std::time::Duration::from_secs(120))
				.build()
				.expect("failed to build http client"),
		}
	}

	pub fn segment(
		&self,
		source_title: &str,
		text: &str,
		options: &SegmentationOptions,
	) -> Result<SegmentationResult> {
		let lines: Vec<&str> = text.lines().collect();
		let numbered: String = lines
			.iter()
			.enumerate()
			.map(|(index, line)| format!("{}: {}", index + 1, line))
			.collect::<Vec<_>>()
			.join("\n");

		let prompt = format!(
			"Window title: {}\n\nText (with line numbers):\n{}",
			source_title, numbered
		);

		let mut system_prompt = segmentation_system_prompt().to_string();
		if let Some(doctype_prompt) = &options.doctype_prompt {
			system_prompt.push_str("\n\nADDITIONAL RULES FOR THIS DOCUMENT TYPE:\n");
			system_prompt.push_str(doctype_prompt);
		}

		let request = OllamaRequest {
			model: self.model.clone(),
			prompt,
			system: system_prompt,
			stream: false,
			format: "json".to_string(),
		};

		let response: OllamaResponse = self
			.client
			.post(format!("{}/api/generate", self.base_url))
			.json(&request)
			.send()?
			.json()?;

		let parsed: SegmentationJson = serde_json::from_str(&response.response)
			.or_else(|_| {
				let entries: Vec<SegmentedEntryJson> = serde_json::from_str(&response.response)?;
				Ok(SegmentationJson { entries })
			})
			.map_err(|error: serde_json::Error| {
				let preview: String = response.response.chars().take(300).collect();
				anyhow::anyhow!("failed to parse segmentation response: {}\nollama returned: {}", error, preview)
			})?;

		let mut entries: Vec<SegmentedEntry> = parsed
			.entries
			.into_iter()
			.filter_map(|entry| {
				let body = extract_body(&lines, entry.body_start_line, entry.body_end_line);
				let body = apply_cleanup(&body, &options.cleanup_patterns);
				let body = body.trim().to_string();
				if body.is_empty() {
					return None;
				}
				Some(SegmentedEntry {
					start_line: entry.body_start_line,
					end_line: entry.body_end_line,
					author: entry.author,
					timestamp: entry.timestamp,
					body,
					is_quote: false,
					heading_level: None,
					heading_title: None,
				})
			})
			.collect();

		if options.merge_consecutive_same_author {
			entries = merge_consecutive_same_author(entries);
		}

		Ok(SegmentationResult { entries })
	}
}

fn extract_body(lines: &[&str], start_line: usize, end_line: usize) -> String {
	if start_line == 0 || end_line == 0 || start_line > end_line {
		return String::new();
	}
	let start_index = start_line.saturating_sub(1);
	let end_index = end_line.min(lines.len());
	if start_index >= lines.len() {
		return String::new();
	}
	lines[start_index..end_index].join("\n")
}

fn apply_cleanup(text: &str, patterns: &[Regex]) -> String {
	let mut result = text.to_string();
	for pattern in patterns {
		result = pattern.replace_all(&result, "").to_string();
	}
	result
}

fn merge_consecutive_same_author(entries: Vec<SegmentedEntry>) -> Vec<SegmentedEntry> {
	if entries.is_empty() {
		return entries;
	}
	let mut merged: Vec<SegmentedEntry> = Vec::new();
	for entry in entries {
		let should_merge = merged.last().map(|last| {
			match (&last.author, &entry.author) {
				(Some(a), Some(b)) => a == b,
				_ => false,
			}
		}).unwrap_or(false);

		if should_merge {
			let last = merged.last_mut().unwrap();
			last.end_line = entry.end_line;
			last.body.push_str("\n\n");
			last.body.push_str(&entry.body);
			if last.timestamp.is_none() {
				last.timestamp = entry.timestamp;
			}
		} else {
			merged.push(entry);
		}
	}
	merged
}

pub fn parse_source_header(first_line: &str) -> Option<String> {
	let captures = first_line.strip_prefix("# source:")?;
	Some(captures.trim().to_string())
}

pub fn parse_clip_date(filename: &str) -> Option<chrono::NaiveDateTime> {
	let stem = Path::new(filename)
		.file_stem()?
		.to_str()?;
	let formats = [
		"%Y%m%d_%H-%M-%S",
		"%Y%m%d_%H%M%S",
	];
	for format in &formats {
		if let Ok(date) = chrono::NaiveDateTime::parse_from_str(stem, format) {
			return Some(date);
		}
	}
	None
}

fn segmentation_system_prompt() -> &'static str {
	include_str!("prompts/segmentation.txt")
}
