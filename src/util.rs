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
