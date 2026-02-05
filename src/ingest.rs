use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize};
use std::path::Path;

use crate::types::{SegmentedEntry, SegmentationResult};

fn bool_from_flexible<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<bool, D::Error> {
	#[derive(Deserialize)]
	#[serde(untagged)]
	enum FlexBool {
		Bool(bool),
		Str(String),
	}
	match FlexBool::deserialize(deserializer)? {
		FlexBool::Bool(b) => Ok(b),
		FlexBool::Str(s) => match s.to_lowercase().as_str() {
			"true" | "1" | "yes" => Ok(true),
			_ => Ok(false),
		},
	}
}

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
	#[serde(default, deserialize_with = "bool_from_flexible")]
	is_quote: bool,
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

		let parsed: SegmentationJson = serde_json::from_str(&response.response)
			.or_else(|_| {
				let entries: Vec<SegmentedEntryJson> = serde_json::from_str(&response.response)?;
				Ok(SegmentationJson { entries })
			})
			.map_err(|error: serde_json::Error| {
				let preview: String = response.response.chars().take(300).collect();
				anyhow::anyhow!("failed to parse segmentation response: {}\nollama returned: {}", error, preview)
			})?;

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

fn segmentation_system_prompt() -> &'static str {
	include_str!("prompts/segmentation.txt")
}
