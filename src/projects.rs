use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::config::expand_tilde;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocStatus {
	Canonical,
	Provisional,
	Archived,
}

impl DocStatus {
	pub fn as_str(self) -> &'static str {
		match self {
			DocStatus::Canonical => "canonical",
			DocStatus::Provisional => "provisional",
			DocStatus::Archived => "archived",
		}
	}

	fn parse(value: &str) -> Result<Self> {
		match value {
			"canonical" => Ok(DocStatus::Canonical),
			"provisional" => Ok(DocStatus::Provisional),
			"archived" => Ok(DocStatus::Archived),
			other => anyhow::bail!(
				"invalid status '{}' (expected canonical, provisional, or archived)",
				other
			),
		}
	}
}

#[derive(Debug, Deserialize)]
struct ProjectsToml {
	#[serde(default)]
	project: Vec<ProjectEntryToml>,
}

#[derive(Debug, Deserialize)]
struct ProjectEntryToml {
	name: String,
	manifest: String,
}

#[derive(Debug, Clone)]
pub struct ProjectRegistration {
	pub name: String,
	pub manifest_path: PathBuf,
	pub root: PathBuf,
}

pub fn load_registry(config_dir: &Path) -> Result<Vec<ProjectRegistration>> {
	let path = config_dir.join("projects.toml");
	if !path.exists() {
		return Ok(Vec::new());
	}
	let text = std::fs::read_to_string(&path)
		.with_context(|| format!("reading projects registry from {}", path.display()))?;
	parse_registry(&text)
}

fn parse_registry(text: &str) -> Result<Vec<ProjectRegistration>> {
	let raw: ProjectsToml = toml::from_str(text).context("parsing projects.toml")?;
	let mut registrations = Vec::new();
	for entry in raw.project {
		let manifest_path = PathBuf::from(expand_tilde(&entry.manifest));
		let root = manifest_path
			.parent()
			.map(Path::to_path_buf)
			.with_context(|| {
				format!("manifest path for project '{}' has no parent directory", entry.name)
			})?;
		registrations.push(ProjectRegistration {
			name: entry.name,
			manifest_path,
			root,
		});
	}
	Ok(registrations)
}

#[derive(Debug, Deserialize)]
struct ManifestToml {
	#[serde(default)]
	docs: Vec<DocRuleToml>,
}

#[derive(Debug, Deserialize)]
struct DocRuleToml {
	glob: String,
	status: String,
	#[serde(default)]
	role: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DocRule {
	pub glob: String,
	pub pattern: Regex,
	pub status: DocStatus,
	pub role: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Manifest {
	pub rules: Vec<DocRule>,
}

impl Manifest {
	pub fn load(path: &Path) -> Result<Self> {
		let text = std::fs::read_to_string(path)
			.with_context(|| format!("reading manifest from {}", path.display()))?;
		Self::parse(&text)
	}

	pub fn parse(text: &str) -> Result<Self> {
		let raw: ManifestToml = toml::from_str(text).context("parsing manifest TOML")?;
		let mut rules = Vec::new();
		for entry in raw.docs {
			let status = DocStatus::parse(&entry.status)
				.with_context(|| format!("in manifest rule for glob '{}'", entry.glob))?;
			let pattern = glob_to_regex(&entry.glob)
				.with_context(|| format!("invalid glob '{}'", entry.glob))?;
			rules.push(DocRule {
				glob: entry.glob,
				pattern,
				status,
				role: entry.role,
			});
		}
		Ok(Manifest { rules })
	}

	pub fn match_path(&self, relative_path: &str) -> Option<&DocRule> {
		self.rules.iter().find(|rule| rule.pattern.is_match(relative_path))
	}
}

fn glob_to_regex(glob: &str) -> Result<Regex> {
	let mut pattern = String::from("^");
	for c in glob.chars() {
		match c {
			'*' => pattern.push_str("[^/]*"),
			'?' => pattern.push_str("[^/]"),
			_ => pattern.push_str(&regex::escape(&c.to_string())),
		}
	}
	pattern.push('$');
	Ok(Regex::new(&pattern)?)
}
