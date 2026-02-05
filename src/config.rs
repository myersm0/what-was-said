use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::types::MergeStrategy;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Parser {
	Ollama,
	Markdown,
	Whisper,
	Whole,
}

#[derive(Debug, Deserialize)]
struct DoctypeToml {
	name: String,
	source_pattern: Option<String>,
	extension: Option<String>,
	parser: String,
	merge_strategy: String,
}

#[derive(Debug)]
pub struct Doctype {
	pub name: String,
	pub source_pattern: Option<Regex>,
	pub extension: Option<String>,
	pub parser: Parser,
	pub merge_strategy: MergeStrategy,
}

#[derive(Debug, Deserialize)]
struct ConfigToml {
	#[serde(default)]
	doctype: Vec<DoctypeToml>,
}

#[derive(Debug)]
pub struct Config {
	pub doctypes: Vec<Doctype>,
}

pub struct DoctypeMatch {
	pub parser: Parser,
	pub merge_strategy: MergeStrategy,
}

fn parse_parser(value: &str) -> Result<Parser> {
	match value {
		"ollama" => Ok(Parser::Ollama),
		"markdown" => Ok(Parser::Markdown),
		"whisper" => Ok(Parser::Whisper),
		"whole" => Ok(Parser::Whole),
		other => anyhow::bail!("unknown parser: {}", other),
	}
}

fn parse_merge_strategy(value: &str) -> Result<MergeStrategy> {
	match value {
		"none" => Ok(MergeStrategy::None),
		"positional" => Ok(MergeStrategy::Positional),
		"timestamped" => Ok(MergeStrategy::Timestamped),
		other => anyhow::bail!("unknown merge_strategy: {}", other),
	}
}

impl Config {
	pub fn load(path: &Path) -> Result<Self> {
		let text = std::fs::read_to_string(path)
			.with_context(|| format!("reading config from {}", path.display()))?;
		Self::parse(&text)
	}

	pub fn parse(text: &str) -> Result<Self> {
		let raw: ConfigToml = toml::from_str(text)
			.context("parsing config TOML")?;
		let mut doctypes = Vec::new();
		for entry in raw.doctype {
			let source_pattern = entry.source_pattern
				.map(|pattern| Regex::new(&pattern))
				.transpose()
				.with_context(|| format!("invalid regex in doctype '{}'", entry.name))?;
			doctypes.push(Doctype {
				name: entry.name.clone(),
				source_pattern,
				extension: entry.extension,
				parser: parse_parser(&entry.parser)
					.with_context(|| format!("in doctype '{}'", entry.name))?,
				merge_strategy: parse_merge_strategy(&entry.merge_strategy)
					.with_context(|| format!("in doctype '{}'", entry.name))?,
			});
		}
		Ok(Config { doctypes })
	}

	pub fn detect(&self, source_title: &str, file_extension: Option<&str>) -> Option<DoctypeMatch> {
		for doctype in &self.doctypes {
			let source_match = doctype.source_pattern.as_ref()
				.map(|regex| regex.is_match(source_title))
				.unwrap_or(false);
			let extension_match = match (&doctype.extension, file_extension) {
				(Some(expected), Some(actual)) => expected == actual,
				_ => false,
			};
			if source_match || extension_match {
				return Some(DoctypeMatch {
					parser: doctype.parser,
					merge_strategy: doctype.merge_strategy,
				});
			}
		}
		None
	}
}

pub fn default_config_path() -> PathBuf {
	dirs::config_dir()
		.unwrap_or_else(|| PathBuf::from("."))
		.join("cathedrals")
		.join("config.toml")
}

pub fn default_config() -> Config {
	Config::parse(include_str!("default_config.toml"))
		.expect("built-in default config should be valid")
}

pub fn load_or_default(path: Option<&Path>) -> Result<Config> {
	match path {
		Some(path) => Config::load(path),
		None => {
			let default_path = default_config_path();
			if default_path.exists() {
				Config::load(&default_path)
			} else {
				Ok(default_config())
			}
		}
	}
}
