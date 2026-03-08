use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

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

#[derive(Serialize)]
struct OllamaEmbeddingRequest {
	model: String,
	prompt: String,
}

#[derive(Deserialize)]
struct OllamaEmbeddingResponse {
	embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct PreprocessorOutput {
	entries: Vec<PreprocessorEntry>,
}

#[derive(Deserialize)]
struct PreprocessorEntry {
	body: String,
	#[serde(default)]
	author: Option<String>,
	#[serde(default)]
	timestamp: Option<String>,
	#[serde(default)]
	heading_title: Option<String>,
	#[serde(default)]
	heading_level: Option<u8>,
}

pub fn run_preprocessor(script_path: &str, file_path: &Path) -> Result<SegmentationResult> {
	let output = Command::new("python3")
		.arg(script_path)
		.arg(file_path)
		.output()
		.with_context(|| format!("failed to run preprocessor: {}", script_path))?;

	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr);
		anyhow::bail!("preprocessor failed: {}", stderr);
	}

	let stdout = String::from_utf8(output.stdout)
		.context("preprocessor output is not valid UTF-8")?;

	let parsed: PreprocessorOutput = serde_json::from_str(&stdout)
		.with_context(|| format!("failed to parse preprocessor JSON: {}", &stdout[..stdout.len().min(200)]))?;

	let entries: Vec<SegmentedEntry> = parsed.entries
		.into_iter()
		.enumerate()
		.filter(|(_, e)| !e.body.trim().is_empty())
		.map(|(i, e)| SegmentedEntry {
			start_line: i + 1,
			end_line: i + 1,
			body: e.body,
			author: e.author,
			timestamp: e.timestamp,
			heading_title: e.heading_title,
			heading_level: e.heading_level,
			is_quote: false,
		})
		.collect();

	Ok(SegmentationResult { entries })
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
				.timeout(std::time::Duration::from_secs(600))
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

	pub fn embed(&self, text: &str, model: &str) -> Result<Vec<f32>> {
		let request = OllamaEmbeddingRequest {
			model: model.to_string(),
			prompt: text.to_string(),
		};

		let response: OllamaEmbeddingResponse = self
			.client
			.post(format!("{}/api/embeddings", self.base_url))
			.json(&request)
			.send()?
			.json()?;

		Ok(response.embedding)
	}

	pub fn chat(&self, prompt: &str, model: &str) -> Result<String> {
		let request = serde_json::json!({
			"model": model,
			"prompt": prompt,
			"stream": false
		});

		let response: serde_json::Value = self
			.client
			.post(format!("{}/api/generate", self.base_url))
			.json(&request)
			.send()?
			.json()?;

		response["response"]
			.as_str()
			.map(|s| s.to_string())
			.ok_or_else(|| anyhow::anyhow!("no response field in ollama output"))
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

pub fn parse_copilot_email_summary(text: &str) -> Vec<SegmentedEntry> {
	let outlook_suffix = Regex::new(r"\s*\[.*?\|\s*Outlook\]\s*$").unwrap();

	let chunks: Vec<&str> = text.split("\nEMAIL\n")
		.flat_map(|s| s.split("\nEMAIL\r\n"))
		.flat_map(|s| s.split("\n#EMAIL\n"))
		.flat_map(|s| s.split("\n### EMAIL\n"))
		.flat_map(|s| s.split("\n##EMAIL\n"))
		.collect();

	let mut entries = Vec::new();

	for chunk in chunks {
		let chunk = chunk.trim();
		if chunk.is_empty() {
			continue;
		}

		let lines: Vec<&str> = chunk.lines().collect();
		if lines.is_empty() {
			continue;
		}

		let mut from: Option<String> = None;
		let mut date: Option<String> = None;
		let mut subject: Option<String> = None;
		let mut body_start = 0;

		for (i, line) in lines.iter().enumerate() {
			let line_lower = line.to_lowercase();
			if line_lower.starts_with("from:") {
				from = Some(line[5..].trim().to_string());
			} else if line_lower.starts_with("date:") {
				date = Some(line[5..].trim().to_string());
			} else if line_lower.starts_with("subject:") {
				subject = Some(line[8..].trim().to_string());
			} else if line.trim().is_empty() && (from.is_some() || date.is_some() || subject.is_some()) {
				body_start = i + 1;
				break;
			} else if !line_lower.starts_with("to:") && !line_lower.starts_with("cc:") {
				body_start = i;
				break;
			}
		}

		let body = lines[body_start..].join("\n");
		let body = outlook_suffix.replace_all(&body, "").trim().to_string();

		if body.is_empty() && from.is_none() && date.is_none() {
			continue;
		}

		entries.push(SegmentedEntry {
			start_line: 0,
			end_line: 0,
			body,
			author: from,
			timestamp: date,
			is_quote: false,
			heading_level: None,
			heading_title: subject,
		});
	}

	entries
}
