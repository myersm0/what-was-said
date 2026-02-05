use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::types::{SegmentedEntry, SegmentationResult, SourceInfo};

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
	start_line: usize,
	end_line: usize,
	author: Option<String>,
	timestamp: Option<String>,
	body: String,
	is_quote: bool,
	is_contaminated: bool,
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
	) -> Result<SegmentationResult> {
		let numbered: String = text
			.lines()
			.enumerate()
			.map(|(index, line)| format!("{}: {}", index + 1, line))
			.collect::<Vec<_>>()
			.join("\n");

		let prompt = format!(
			"Window title: {}\n\nText (with line numbers):\n{}",
			source_title, numbered
		);

		let request = OllamaRequest {
			model: self.model.clone(),
			prompt,
			system: segmentation_system_prompt().to_string(),
			stream: false,
			format: "json".to_string(),
		};

		let response: OllamaResponse = self
			.client
			.post(format!("{}/api/generate", self.base_url))
			.json(&request)
			.send()?
			.json()?;

		let parsed: SegmentationJson = serde_json::from_str(&response.response)?;

		let entries = parsed
			.entries
			.into_iter()
			.map(|entry| SegmentedEntry {
				start_line: entry.start_line,
				end_line: entry.end_line,
				author: entry.author,
				timestamp: entry.timestamp,
				body: entry.body,
				is_quote: entry.is_quote,
				is_contaminated: entry.is_contaminated,
				heading_level: None,
				heading_title: None,
			})
			.collect();

		Ok(SegmentationResult { entries })
	}
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

pub fn infer_merge_strategy(source_title: &str) -> crate::types::MergeStrategy {
	let lower = source_title.to_lowercase();
	if lower.contains("slack") {
		return crate::types::MergeStrategy::Positional;
	}
	if lower.contains("outlook") || lower.contains("mail") || lower.contains("gmail") {
		return crate::types::MergeStrategy::Positional;
	}
	if lower.contains("zoom") || lower.contains("teams meeting") {
		return crate::types::MergeStrategy::Timestamped;
	}
	crate::types::MergeStrategy::None
}

fn segmentation_system_prompt() -> &'static str {
	include_str!("prompts/segmentation.txt")
}
