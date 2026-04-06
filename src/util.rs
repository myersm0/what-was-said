const BROWSER_SUFFIXES: &[&str] = &[
	" - Claude - Brave",
	" - Claude",
	"\u{2014} Claude - Brave",
	"\u{2014} Claude",
	" - Brave",
	" - Chrome",
	" - Firefox",
	" - Safari",
	" - Edge",
	" - Arc",
];

pub fn strip_source_suffix(source_title: &str) -> String {
	let mut s = source_title.to_string();

	for suffix in BROWSER_SUFFIXES {
		if let Some(stripped) = s.strip_suffix(suffix) {
			s = stripped.to_string();
			break;
		}
	}

	if let Some(url_start) = s.rfind(" - https://") {
		s = s[..url_start].to_string();
	}
	if let Some(url_start) = s.rfind(" - http://") {
		s = s[..url_start].to_string();
	}

	s.trim().to_string()
}

pub fn normalize_to_ascii(s: &str) -> String {
	s.chars()
		.map(|c| {
			if c.is_ascii() {
				c
			} else {
				match c {
					'\u{2014}' | '\u{2013}' => '-',
					'\u{2018}' | '\u{2019}' => '\'',
					'\u{201C}' | '\u{201D}' => '"',
					'\u{2026}' => '.',
					_ => ' ',
				}
			}
		})
		.collect::<String>()
		.split_whitespace()
		.collect::<Vec<_>>()
		.join(" ")
}

pub fn truncate_str(s: &str, max_len: usize) -> &str {
	if s.len() <= max_len {
		s
	} else {
		let mut end = max_len;
		while end > 0 && !s.is_char_boundary(end) {
			end -= 1;
		}
		&s[..end]
	}
}

pub fn strip_fts_markers(s: &str) -> String {
	s.replace(['\x01', '\x02', '\x03'], "")
}

pub fn extract_group_key(source_title: &str) -> Option<String> {
	let key = strip_source_suffix(source_title);

	if key.is_empty() || key.len() < 3 {
		return None;
	}

	let lower = key.to_lowercase();
	let generic = ["untitled", "new tab", "bash", "zsh", "terminal", "new document"];
	if generic.iter().any(|pattern| lower.contains(pattern)) {
		return None;
	}

	Some(key)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn strip_browser_suffix() {
		assert_eq!(strip_source_suffix("Some Article - Brave"), "Some Article");
		assert_eq!(strip_source_suffix("Chat - Claude - Brave"), "Chat");
		assert_eq!(strip_source_suffix("Docs - Chrome"), "Docs");
		assert_eq!(strip_source_suffix("Page - Firefox"), "Page");
	}

	#[test]
	fn strip_url_suffix() {
		assert_eq!(
			strip_source_suffix("Title - https://example.com"),
			"Title",
		);
		assert_eq!(
			strip_source_suffix("Title - http://example.com"),
			"Title",
		);
	}

	#[test]
	fn strip_browser_then_url() {
		assert_eq!(
			strip_source_suffix("Page - https://x.com - Brave"),
			"Page",
		);
	}

	#[test]
	fn strip_no_suffix() {
		assert_eq!(strip_source_suffix("plain title"), "plain title");
		assert_eq!(strip_source_suffix(""), "");
	}

	#[test]
	fn normalize_curly_quotes() {
		assert_eq!(normalize_to_ascii("\u{201C}hello\u{201D}"), "\"hello\"");
		assert_eq!(normalize_to_ascii("it\u{2019}s"), "it's");
	}

	#[test]
	fn normalize_dashes_and_ellipsis() {
		assert_eq!(normalize_to_ascii("a\u{2014}b"), "a-b");
		assert_eq!(normalize_to_ascii("a\u{2013}b"), "a-b");
		assert_eq!(normalize_to_ascii("wait\u{2026}"), "wait.");
	}

	#[test]
	fn normalize_collapses_whitespace() {
		assert_eq!(normalize_to_ascii("a  \u{00A0}  b"), "a b");
	}

	#[test]
	fn truncate_within_limit() {
		assert_eq!(truncate_str("hello", 10), "hello");
	}

	#[test]
	fn truncate_exact_boundary() {
		assert_eq!(truncate_str("hello", 5), "hello");
	}

	#[test]
	fn truncate_ascii() {
		assert_eq!(truncate_str("hello world", 5), "hello");
	}

	#[test]
	fn truncate_respects_char_boundary() {
		let s = "caf\u{00E9}"; // "café", é is 2 bytes
		let result = truncate_str(s, 4);
		assert_eq!(result, "caf");
	}

	#[test]
	fn truncate_empty() {
		assert_eq!(truncate_str("", 5), "");
	}
}
