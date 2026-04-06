use anyhow::{Context, Result};
use ratatui::style::Color;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct ThemeToml {
	background: String,
	border: String,
	border_focused: String,
	border_popup: String,
	title: String,
	status_bar_bg: String,
	status_bar_fg: String,
	text: String,
	text_muted: String,
	text_accent: String,
	highlight_bg: String,
	highlight_fg: String,
	heading: String,
	code: String,
	search_match: String,
	mark: String,
	status_message: String,
	key_binding: String,
	column_header: String,
}

#[derive(Debug, Clone)]
pub struct Theme {
	pub background: Color,
	pub border: Color,
	pub border_focused: Color,
	pub border_popup: Color,
	pub title: Color,
	pub status_bar_bg: Color,
	pub status_bar_fg: Color,
	pub text: Color,
	pub text_muted: Color,
	pub text_accent: Color,
	pub highlight_bg: Color,
	pub highlight_fg: Color,
	pub heading: Color,
	pub code: Color,
	pub search_match: Color,
	pub mark: Color,
	pub status_message: Color,
	pub key_binding: Color,
	pub column_header: Color,
}

fn parse_hex_color(s: &str) -> Option<Color> {
	let hex = s.strip_prefix('#')?;
	if hex.len() != 6 {
		return None;
	}
	let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
	let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
	let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
	Some(Color::Rgb(r, g, b))
}

pub fn parse_color(name: &str) -> Option<Color> {
	if name.starts_with('#') {
		return parse_hex_color(name);
	}
	match name.to_lowercase().as_str() {
		"red" => Some(Color::Red),
		"green" => Some(Color::Green),
		"blue" => Some(Color::Blue),
		"cyan" => Some(Color::Cyan),
		"magenta" => Some(Color::Magenta),
		"yellow" => Some(Color::Yellow),
		"white" => Some(Color::White),
		"gray" | "grey" => Some(Color::Gray),
		"dark_gray" | "dark_grey" => Some(Color::DarkGray),
		"light_red" => Some(Color::LightRed),
		"light_green" => Some(Color::LightGreen),
		"light_blue" => Some(Color::LightBlue),
		"light_cyan" => Some(Color::LightCyan),
		"light_magenta" => Some(Color::LightMagenta),
		"light_yellow" => Some(Color::LightYellow),
		"black" => Some(Color::Black),
		_ => None,
	}
}

fn require_color(value: &str, field: &str) -> Result<Color> {
	parse_color(value)
		.with_context(|| format!("invalid color '{}' for theme field '{}'", value, field))
}

impl Theme {
	fn from_toml(t: ThemeToml) -> Result<Self> {
		Ok(Theme {
			background: require_color(&t.background, "background")?,
			border: require_color(&t.border, "border")?,
			border_focused: require_color(&t.border_focused, "border_focused")?,
			border_popup: require_color(&t.border_popup, "border_popup")?,
			title: require_color(&t.title, "title")?,
			status_bar_bg: require_color(&t.status_bar_bg, "status_bar_bg")?,
			status_bar_fg: require_color(&t.status_bar_fg, "status_bar_fg")?,
			text: require_color(&t.text, "text")?,
			text_muted: require_color(&t.text_muted, "text_muted")?,
			text_accent: require_color(&t.text_accent, "text_accent")?,
			highlight_bg: require_color(&t.highlight_bg, "highlight_bg")?,
			highlight_fg: require_color(&t.highlight_fg, "highlight_fg")?,
			heading: require_color(&t.heading, "heading")?,
			code: require_color(&t.code, "code")?,
			search_match: require_color(&t.search_match, "search_match")?,
			mark: require_color(&t.mark, "mark")?,
			status_message: require_color(&t.status_message, "status_message")?,
			key_binding: require_color(&t.key_binding, "key_binding")?,
			column_header: require_color(&t.column_header, "column_header")?,
		})
	}

	pub fn load(text: &str) -> Result<Self> {
		let toml: ThemeToml = toml::from_str(text).context("parsing theme TOML")?;
		Self::from_toml(toml)
	}

	pub fn load_file(path: &Path) -> Result<Self> {
		let text = std::fs::read_to_string(path)
			.with_context(|| format!("reading theme from {}", path.display()))?;
		Self::load(&text)
	}
}

fn builtin_theme(name: &str) -> Option<&'static str> {
	match name {
		"gruvbox" => Some(include_str!("themes/gruvbox.toml")),
		"nord" => Some(include_str!("themes/nord.toml")),
		"dracula" => Some(include_str!("themes/dracula.toml")),
		"solarized" => Some(include_str!("themes/solarized.toml")),
		"light" => Some(include_str!("themes/light.toml")),
		_ => None,
	}
}

fn themes_dir() -> PathBuf {
	dirs::config_dir()
		.unwrap_or_else(|| PathBuf::from("."))
		.join("commonplace")
		.join("themes")
}

pub fn load_theme(name: Option<&str>) -> Theme {
	let name = name.unwrap_or("dracula");

	if let Some(text) = builtin_theme(name) {
		if let Ok(theme) = Theme::load(text) {
			return theme;
		}
	}

	let as_path = Path::new(name);
	if as_path.is_absolute() || as_path.exists() {
		if let Ok(theme) = Theme::load_file(as_path) {
			return theme;
		}
	}

	let in_dir = themes_dir().join(format!("{}.toml", name));
	if in_dir.exists() {
		if let Ok(theme) = Theme::load_file(&in_dir) {
			return theme;
		}
	}

	Theme::load(builtin_theme("dracula").unwrap())
		.expect("built-in dracula theme must be valid")
}
