use crate::types::SegmentedEntry;

pub fn parse_markdown_sections(text: &str) -> Vec<SegmentedEntry> {
	let lines: Vec<&str> = text.lines().collect();
	let mut entries = Vec::new();
	let mut current_body = Vec::new();
	let mut current_heading_level: Option<u8> = Option::None;
	let mut current_heading_title: Option<String> = Option::None;
	let mut current_start: usize = 1;

	let mut in_code_fence = false;

	for (index, line) in lines.iter().enumerate() {
		let line_number = index + 1;
		let trimmed = line.trim_start();
		if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
			in_code_fence = !in_code_fence;
			current_body.push(*line);
			continue;
		}
		if !in_code_fence {
			if let Some((level, title)) = parse_heading(line) {
				if !current_body.is_empty() || current_heading_level.is_some() {
					let body = finish_body(&current_body);
					if !body.is_empty() || current_heading_level.is_some() {
						entries.push(make_entry(
							current_start,
							line_number.saturating_sub(1),
							&body,
							current_heading_level,
							current_heading_title.take(),
						));
					}
				}
				current_heading_level = Some(level);
				current_heading_title = Some(title);
				current_body.clear();
				current_start = line_number;
				continue;
			}
		}
		current_body.push(*line);
	}

	let body = finish_body(&current_body);
	if !body.is_empty() || current_heading_level.is_some() {
		entries.push(make_entry(
			current_start,
			lines.len(),
			&body,
			current_heading_level,
			current_heading_title,
		));
	}

	entries
}

fn parse_heading(line: &str) -> Option<(u8, String)> {
	let trimmed = line.trim_start();
	if !trimmed.starts_with('#') {
		return Option::None;
	}
	let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
	if hashes > 6 {
		return Option::None;
	}
	let rest = trimmed[hashes..].trim();
	if rest.is_empty() {
		return Option::None;
	}
	Some((hashes as u8, rest.to_string()))
}

fn finish_body(lines: &[&str]) -> String {
	let mut start = 0;
	while start < lines.len() && lines[start].trim().is_empty() {
		start += 1;
	}
	let mut end = lines.len();
	while end > start && lines[end - 1].trim().is_empty() {
		end -= 1;
	}
	lines[start..end].join("\n")
}

fn make_entry(
	start_line: usize,
	end_line: usize,
	body: &str,
	heading_level: Option<u8>,
	heading_title: Option<String>,
) -> SegmentedEntry {
	SegmentedEntry {
		start_line,
		end_line,
		author: Option::None,
		timestamp: Option::None,
		body: body.to_string(),
		is_quote: false,
		heading_level,
		heading_title,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn splits_on_headings() {
		let input = "# Introduction\nSome intro text.\n\n## Details\nDetail text here.\n\n## Conclusion\nWrap up.";
		let sections = parse_markdown_sections(input);
		assert_eq!(sections.len(), 3);
		assert_eq!(sections[0].heading_title.as_deref(), Some("Introduction"));
		assert_eq!(sections[0].heading_level, Some(1));
		assert_eq!(sections[0].body, "Some intro text.");
		assert_eq!(sections[1].heading_title.as_deref(), Some("Details"));
		assert_eq!(sections[1].heading_level, Some(2));
		assert_eq!(sections[2].body, "Wrap up.");
	}

	#[test]
	fn handles_preamble_before_first_heading() {
		let input = "Some preamble.\n\n# First heading\nContent.";
		let sections = parse_markdown_sections(input);
		assert_eq!(sections.len(), 2);
		assert_eq!(sections[0].heading_level, Option::None);
		assert_eq!(sections[0].body, "Some preamble.");
		assert_eq!(sections[1].heading_title.as_deref(), Some("First heading"));
	}

	#[test]
	fn no_headings() {
		let input = "Just a plain text document.\nWith multiple lines.";
		let sections = parse_markdown_sections(input);
		assert_eq!(sections.len(), 1);
		assert_eq!(sections[0].body, input);
		assert_eq!(sections[0].heading_level, Option::None);
	}
}
