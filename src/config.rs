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
	#[serde(rename = "copilot_email")]
	CopilotEmail,
}

#[derive(Debug, Deserialize)]
struct DoctypeToml {
	name: String,
	source_pattern: Option<String>,
	extension: Option<String>,
	parser: String,
	merge_strategy: String,
	#[serde(default)]
	prompt: Option<String>,
	#[serde(default)]
	cleanup_patterns: Vec<String>,
	#[serde(default)]
	merge_consecutive_same_author: bool,
	#[serde(default)]
	preprocessor: Option<String>,
	#[serde(default)]
	skip: bool,
}

#[derive(Debug, Clone)]
pub struct Doctype {
	pub name: String,
	pub source_pattern: Option<Regex>,
	pub extension: Option<String>,
	pub parser: Parser,
	pub merge_strategy: MergeStrategy,
	pub prompt: Option<String>,
	pub cleanup_patterns: Vec<Regex>,
	pub merge_consecutive_same_author: bool,
	pub preprocessor: Option<String>,
	pub skip: bool,
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

#[derive(Debug, Clone)]
pub struct DoctypeMatch {
	pub name: String,
	pub parser: Parser,
	pub merge_strategy: MergeStrategy,
	pub prompt: Option<String>,
	pub cleanup_patterns: Vec<Regex>,
	pub merge_consecutive_same_author: bool,
	pub preprocessor: Option<String>,
	pub skip: bool,
}

fn parse_parser(value: &str) -> Result<Parser> {
	match value {
		"ollama" => Ok(Parser::Ollama),
		"markdown" => Ok(Parser::Markdown),
		"whisper" => Ok(Parser::Whisper),
		"whole" => Ok(Parser::Whole),
		"copilot_email" => Ok(Parser::CopilotEmail),
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

fn expand_tilde(path: &str) -> String {
	if path.starts_with("~/") {
		if let Some(home) = dirs::home_dir() {
			return home.join(&path[2..]).to_string_lossy().to_string();
		}
	}
	path.to_string()
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
			let cleanup_patterns: Vec<Regex> = entry.cleanup_patterns
				.iter()
				.enumerate()
				.map(|(i, pattern)| {
					Regex::new(pattern)
						.with_context(|| format!("invalid cleanup_pattern {} in doctype '{}'", i, entry.name))
				})
				.collect::<Result<Vec<_>>>()?;
			doctypes.push(Doctype {
				name: entry.name.clone(),
				source_pattern,
				extension: entry.extension,
				parser: parse_parser(&entry.parser)
					.with_context(|| format!("in doctype '{}'", entry.name))?,
				merge_strategy: parse_merge_strategy(&entry.merge_strategy)
					.with_context(|| format!("in doctype '{}'", entry.name))?,
				prompt: entry.prompt,
				cleanup_patterns,
				merge_consecutive_same_author: entry.merge_consecutive_same_author,
				preprocessor: entry.preprocessor.map(|p| expand_tilde(&p)),
				skip: entry.skip,
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
					name: doctype.name.clone(),
					parser: doctype.parser,
					merge_strategy: doctype.merge_strategy,
					prompt: doctype.prompt.clone(),
					cleanup_patterns: doctype.cleanup_patterns.clone(),
					merge_consecutive_same_author: doctype.merge_consecutive_same_author,
					preprocessor: doctype.preprocessor.clone(),
					skip: doctype.skip,
				});
			}
		}
		None
	}

	pub fn detect_with_content(&self, source_title: &str, file_extension: Option<&str>, content: &str) -> Option<DoctypeMatch> {
		if let Some(m) = self.detect(source_title, file_extension) {
			return Some(m);
		}

		if looks_like_copilot_email(content) {
			for doctype in &self.doctypes {
				if doctype.parser == Parser::CopilotEmail {
					return Some(DoctypeMatch {
						name: doctype.name.clone(),
						parser: doctype.parser,
						merge_strategy: doctype.merge_strategy,
						prompt: doctype.prompt.clone(),
						cleanup_patterns: doctype.cleanup_patterns.clone(),
						merge_consecutive_same_author: doctype.merge_consecutive_same_author,
						preprocessor: doctype.preprocessor.clone(),
						skip: doctype.skip,
					});
				}
			}
			return Some(DoctypeMatch {
				name: "copilot_email".to_string(),
				parser: Parser::CopilotEmail,
				merge_strategy: MergeStrategy::Positional,
				prompt: None,
				cleanup_patterns: vec![],
				merge_consecutive_same_author: false,
				preprocessor: None,
				skip: false,
			});
		}

		if looks_like_markdown(content) {
			for doctype in &self.doctypes {
				if doctype.parser == Parser::Markdown {
					return Some(DoctypeMatch {
						name: doctype.name.clone(),
						parser: doctype.parser,
						merge_strategy: doctype.merge_strategy,
						prompt: doctype.prompt.clone(),
						cleanup_patterns: doctype.cleanup_patterns.clone(),
						merge_consecutive_same_author: doctype.merge_consecutive_same_author,
						preprocessor: doctype.preprocessor.clone(),
						skip: doctype.skip,
					});
				}
			}
			return Some(DoctypeMatch {
				name: "markdown".to_string(),
				parser: Parser::Markdown,
				merge_strategy: MergeStrategy::None,
				prompt: None,
				cleanup_patterns: vec![],
				merge_consecutive_same_author: false,
				preprocessor: None,
				skip: false,
			});
		}

		None
	}
}

fn looks_like_copilot_email(content: &str) -> bool {
	let has_email_delimiter = content.contains("\nEMAIL\n") 
		|| content.contains("\n### EMAIL\n")
		|| content.contains("\n##EMAIL\n")
		|| content.contains("\n#EMAIL\n");
	
	if !has_email_delimiter {
		return false;
	}

	let has_from = content.to_lowercase().contains("\nfrom:");
	let has_date = content.to_lowercase().contains("\ndate:");
	
	has_from && has_date
}

fn looks_like_markdown(content: &str) -> bool {
	let mut score = 0;

	for line in content.lines().take(100) {
		let trimmed = line.trim();
		if trimmed.starts_with("# ") || trimmed.starts_with("## ") || trimmed.starts_with("### ") {
			score += 2;
		}
		if trimmed.starts_with("```") {
			score += 2;
		}
		if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("1. ") {
			score += 1;
		}
		if trimmed.contains("**") || trimmed.contains("__") {
			score += 1;
		}
		if trimmed.contains("](") && trimmed.contains("[") {
			score += 1;
		}
		if trimmed.starts_with("|") && trimmed.ends_with("|") {
			score += 1;
		}
		if score >= 3 {
			return true;
		}
	}

	false
}

pub fn default_config_path() -> PathBuf {
	dirs::config_dir()
		.unwrap_or_else(|| PathBuf::from("."))
		.join("commonplace")
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

use std::collections::HashMap;

#[derive(Debug, Deserialize, Default)]
struct DefaultsToml {
	#[serde(default)]
	exclude: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct TagConfigToml {
	#[serde(default)]
	includes: HashMap<String, Vec<String>>,
	#[serde(default)]
	defaults: DefaultsToml,
	#[serde(default)]
	colors: HashMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct TagConfig {
	pub includes: HashMap<String, Vec<String>>,
	pub default_exclude: Vec<String>,
	pub colors: HashMap<String, String>,
}

impl TagConfig {
	pub fn load(path: &Path) -> Result<Self> {
		let contents = std::fs::read_to_string(path)
			.with_context(|| format!("failed to read tag config: {}", path.display()))?;
		let toml: TagConfigToml = toml::from_str(&contents)
			.with_context(|| format!("failed to parse tag config: {}", path.display()))?;
		Ok(TagConfig {
			includes: toml.includes,
			default_exclude: toml.defaults.exclude,
			colors: toml.colors,
		})
	}

	pub fn doc_matches_filter(&self, doc_tags: &[String], filter_tag: &str) -> bool {
		if doc_tags.iter().any(|t| t == filter_tag) {
			return true;
		}
		if let Some(included) = self.includes.get(filter_tag) {
			return doc_tags.iter().any(|t| included.contains(t));
		}
		false
	}
}

fn default_tags_config_path() -> PathBuf {
	dirs::data_dir()
		.unwrap_or_else(|| PathBuf::from("."))
		.join("commonplace")
		.join("tags.toml")
}

pub fn load_tag_config(path: Option<&Path>) -> TagConfig {
	let path = path.map(PathBuf::from).unwrap_or_else(default_tags_config_path);
	if path.exists() {
		TagConfig::load(&path).unwrap_or_default()
	} else {
		TagConfig::default()
	}
}

#[derive(Debug, Deserialize)]
struct DeriveConfigToml {
	#[serde(default = "default_detailed_model")]
	detailed_model: String,
	#[serde(default = "default_brief_model")]
	brief_model: String,
	#[serde(default = "default_prompt_version")]
	prompt_version: String,
	#[serde(default)]
	prompts: std::collections::HashMap<String, String>,
	#[serde(default = "default_short_threshold")]
	short_threshold: usize,
	#[serde(default = "default_medium_threshold")]
	medium_threshold: usize,
}

fn default_detailed_model() -> String { "qwen2.5:32b".to_string() }
fn default_brief_model() -> String { "qwen2.5:32b".to_string() }
fn default_prompt_version() -> String { "v1".to_string() }
fn default_short_threshold() -> usize { 1200 }
fn default_medium_threshold() -> usize { 3500 }

#[derive(Debug, Clone)]
pub struct DeriveConfig {
	pub detailed_model: String,
	pub brief_model: String,
	pub prompt_version: String,
	pub prompts: std::collections::HashMap<String, String>,
	pub short_threshold: usize,
	pub medium_threshold: usize,
}

impl Default for DeriveConfig {
	fn default() -> Self {
		DeriveConfig {
			detailed_model: default_detailed_model(),
			brief_model: default_brief_model(),
			prompt_version: default_prompt_version(),
			prompts: std::collections::HashMap::new(),
			short_threshold: default_short_threshold(),
			medium_threshold: default_medium_threshold(),
		}
	}
}

impl DeriveConfig {
	pub fn load(config_dir: &Path) -> Result<Self> {
		let derive_path = config_dir.join("derive.toml");

		if !derive_path.exists() {
			return Ok(DeriveConfig::default());
		}

		let text = std::fs::read_to_string(&derive_path)
			.with_context(|| format!("reading derive config from {}", derive_path.display()))?;

		let raw: DeriveConfigToml = toml::from_str(&text)
			.context("parsing derive.toml")?;

		let prompts: std::collections::HashMap<String, String> = raw.prompts.into_iter()
			.map(|(k, v)| (k, expand_tilde(&v)))
			.collect();

		Ok(DeriveConfig {
			detailed_model: raw.detailed_model,
			brief_model: raw.brief_model,
			prompt_version: raw.prompt_version,
			prompts,
			short_threshold: raw.short_threshold,
			medium_threshold: raw.medium_threshold,
		})
	}

	pub fn get_prompt_for_doctype(&self, doctype: Option<&str>) -> String {
		if let Some(dt) = doctype {
			if let Some(path) = self.prompts.get(dt) {
				if let Ok(text) = std::fs::read_to_string(path) {
					return text;
				}
			}
		}
		if let Some(path) = self.prompts.get("default") {
			if let Ok(text) = std::fs::read_to_string(path) {
				return text;
			}
		}
		DEFAULT_DETAILED_PROMPT.to_string()
	}

	pub fn get_detailed_prompt(&self, content_len: usize) -> &'static str {
		if content_len < self.short_threshold {
			DETAILED_PROMPT_SHORT
		} else if content_len < self.medium_threshold {
			DETAILED_PROMPT_MEDIUM
		} else {
			DETAILED_PROMPT_LONG
		}
	}

	pub fn get_brief_prompt(&self) -> String {
		if let Some(path) = self.prompts.get("brief") {
			if let Ok(text) = std::fs::read_to_string(path) {
				return text;
			}
		}
		DEFAULT_BRIEF_PROMPT.to_string()
	}
}

pub const DETAILED_PROMPT_SHORT: &str = r#"Summarize briefly. 1-2 sentences max. No filler.

Document:
"#;

pub const DETAILED_PROMPT_MEDIUM: &str = r#"Summarize this document. Cover the main points directly.
Be proportional — a few sentences to a paragraph.
No hedging, no meta-commentary.

Document:
"#;

pub const DETAILED_PROMPT_LONG: &str = r#"Summarize this document by section:
1. What is the main topic/claim?
2. What evidence or points are made in the body?
3. What conclusions or outcomes are reached?

Be thorough but not verbose. No filler.

Document:
"#;

pub const DEFAULT_DETAILED_PROMPT: &str = DETAILED_PROMPT_MEDIUM;

pub const DEFAULT_BRIEF_PROMPT: &str = r#"Compress to 1-2 sentences. Output ONLY the summary, then stop.
Never include:
- "---" or separators
- Explanations of your summary
- "Feel free to ask" or similar
- Who/what/why breakdowns

Summary to compress:
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
	Ollama,
	#[serde(rename = "openai")]
	OpenAi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenAiAuth {
	#[serde(rename = "api_key")]
	ApiKey,
	OAuth,
}

#[derive(Debug, Deserialize)]
struct OpenAiConfigToml {
	base_url: Option<String>,
	#[serde(default = "default_openai_auth")]
	auth: OpenAiAuth,
	oauth_token_url: Option<String>,
	oauth_scope: Option<String>,
}

fn default_openai_auth() -> OpenAiAuth { OpenAiAuth::ApiKey }

#[derive(Debug, Deserialize)]
struct BackendConfigToml {
	#[serde(default = "default_backend_kind")]
	backend: BackendKind,
	#[serde(default = "default_ollama_url")]
	ollama_url: String,
	model: Option<String>,
	embed_model: Option<String>,
	openai: Option<OpenAiConfigToml>,
}

fn default_backend_kind() -> BackendKind { BackendKind::Ollama }
fn default_ollama_url() -> String { "http://localhost:11434".to_string() }

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
	pub base_url: String,
	pub auth: OpenAiAuth,
	pub oauth_token_url: Option<String>,
	pub oauth_scope: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BackendConfig {
	pub backend: BackendKind,
	pub ollama_url: String,
	pub model: Option<String>,
	pub embed_model: Option<String>,
	pub openai: OpenAiConfig,
}

impl Default for BackendConfig {
	fn default() -> Self {
		BackendConfig {
			backend: BackendKind::Ollama,
			ollama_url: default_ollama_url(),
			model: None,
			embed_model: None,
			openai: OpenAiConfig {
				base_url: "https://api.openai.com/v1".to_string(),
				auth: OpenAiAuth::ApiKey,
				oauth_token_url: None,
				oauth_scope: None,
			},
		}
	}
}

impl BackendConfig {
	pub fn load(config_dir: &Path) -> Result<Self> {
		let path = config_dir.join("backend.toml");
		if !path.exists() {
			return Ok(BackendConfig::default());
		}
		let text = std::fs::read_to_string(&path)
			.with_context(|| format!("reading backend config from {}", path.display()))?;
		let raw: BackendConfigToml = toml::from_str(&text)
			.context("parsing backend.toml")?;
		let openai = match raw.openai {
			Some(o) => OpenAiConfig {
				base_url: o.base_url
					.unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
				auth: o.auth,
				oauth_token_url: o.oauth_token_url,
				oauth_scope: o.oauth_scope,
			},
			None => OpenAiConfig {
				base_url: "https://api.openai.com/v1".to_string(),
				auth: OpenAiAuth::ApiKey,
				oauth_token_url: None,
				oauth_scope: None,
			},
		};
		Ok(BackendConfig {
			backend: raw.backend,
			ollama_url: raw.ollama_url,
			model: raw.model,
			embed_model: raw.embed_model,
			openai,
		})
	}
}
