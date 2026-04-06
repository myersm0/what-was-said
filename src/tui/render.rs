use ratatui::{
	layout::Rect,
	style::{Color, Modifier, Style},
	text::{Line, Span},
	widgets::Paragraph,
	Frame,
};

use super::App;
use super::theme::Theme;

fn styled_help<'a>(text: &str, theme: &Theme) -> Vec<Span<'a>> {
	let mut spans = Vec::new();
	let mut rest = text;
	while let Some(open) = rest.find('[') {
		if open > 0 {
			spans.push(Span::styled(rest[..open].to_string(), Style::default().fg(theme.status_bar_fg)));
		}
		if let Some(close) = rest[open..].find(']') {
			spans.push(Span::styled(
				rest[open..open + close + 1].to_string(),
				Style::default().fg(theme.key_binding),
			));
			rest = &rest[open + close + 1..];
		} else {
			break;
		}
	}
	if !rest.is_empty() {
		spans.push(Span::styled(rest.to_string(), Style::default().fg(theme.status_bar_fg)));
	}
	spans
}

pub(super) fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
	let read_help = if app.group_docs.len() > 1 {
		"[↑↓/gG] chunk  [◀▶] group  [d] summary  [y/Y] yank  [t] tags  [b] back  [q] quit"
	} else {
		"[↑↓/gG] chunk  [PgUp/Dn] jump  [d] summary  [y/Y] yank  [t] tags  [b] back  [q] quit"
	};

	let help_text = match app.mode {
		super::Mode::Browse => "[↑↓/gG] move  [m] mark  [d] summary  [Enter] open  [/] search  [f] filter  [s] sort  [q] quit",
		super::Mode::Read => read_help,
		super::Mode::Search => "[↑↓] move  [Tab] field  [`] mode  [Enter] open  [Esc] back",
		super::Mode::TagEdit => "[type] add tag  [Enter] save  [Esc] cancel",
		super::Mode::TagFilter => "[↑↓/gG] select  [Enter] apply  [c] clear  [Esc] cancel",
		super::Mode::SummaryView => "[↑↓] scroll  [d] toggle  [y] copy  [x] mark bad  [Esc] close",
	};

	let status = if let Some(ref msg) = app.status_message {
		let mut spans = vec![
			Span::styled(msg, Style::default().fg(app.theme.status_message)),
			Span::styled("  │  ", Style::default().fg(app.theme.status_bar_fg)),
		];
		spans.extend(styled_help(help_text, &app.theme));
		Line::from(spans)
	} else {
		Line::from(styled_help(help_text, &app.theme))
	};

	let paragraph = Paragraph::new(status)
		.style(Style::default().bg(app.theme.status_bar_bg));
	frame.render_widget(paragraph, area);
}

pub(super) fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
	let popup_width = area.width * percent_x / 100;
	let x = (area.width.saturating_sub(popup_width)) / 2;
	let y = (area.height.saturating_sub(height)) / 2;
	Rect::new(
		area.x + x,
		area.y + y,
		popup_width.min(area.width),
		height.min(area.height),
	)
}

pub(super) fn expand_tabs(s: &str) -> String {
	s.replace('\t', "    ")
}

pub(super) fn align_markdown_tables(text: &str) -> String {
	let lines: Vec<&str> = text.lines().collect();
	let mut result: Vec<String> = Vec::new();
	let mut i = 0;

	while i < lines.len() {
		if is_table_row(lines[i]) {
			let table_start = i;
			while i < lines.len() && is_table_row(lines[i]) {
				i += 1;
			}
			let table_lines = &lines[table_start..i];
			result.extend(align_table(table_lines));
		} else {
			result.push(lines[i].to_string());
			i += 1;
		}
	}

	result.join("\n")
}

fn is_table_row(line: &str) -> bool {
	let trimmed = line.trim();
	trimmed.starts_with('|') || (trimmed.contains('|') && is_separator_row(trimmed))
}

fn is_separator_row(line: &str) -> bool {
	let trimmed = line.trim().trim_matches('|');
	!trimmed.is_empty() && trimmed.chars().all(|c| c == '-' || c == ':' || c == '|' || c == ' ')
}

fn align_table(rows: &[&str]) -> Vec<String> {
	let parsed: Vec<Vec<String>> = rows
		.iter()
		.map(|row| parse_table_row(row))
		.collect();

	if parsed.is_empty() {
		return Vec::new();
	}

	let column_count = parsed.iter().map(|r| r.len()).max().unwrap_or(0);
	if column_count == 0 {
		return rows.iter().map(|s| s.to_string()).collect();
	}

	let cell_widths: Vec<Vec<usize>> = parsed
		.iter()
		.zip(rows.iter())
		.filter(|(_, orig)| !is_separator_row(orig))
		.map(|(cells, _)| {
			cells.iter().map(|c| c.chars().count()).collect()
		})
		.collect();

	if cell_widths.len() >= 2 {
		let first_widths = &cell_widths[0];
		let already_aligned = cell_widths[1..].iter().all(|row_widths| {
			row_widths.len() == first_widths.len()
				&& row_widths.iter().zip(first_widths.iter()).all(|(a, b)| a == b)
		});
		if already_aligned {
			return rows.iter().map(|s| s.to_string()).collect();
		}
	}

	let mut widths: Vec<usize> = vec![0; column_count];
	for row in &parsed {
		for (col_index, cell) in row.iter().enumerate() {
			if col_index < widths.len() {
				widths[col_index] = widths[col_index].max(cell.trim().chars().count());
			}
		}
	}

	parsed
		.iter()
		.zip(rows.iter())
		.map(|(cells, original)| {
			if is_separator_row(original) {
				let sep_cells: Vec<String> = widths
					.iter()
					.map(|w| "-".repeat(*w))
					.collect();
				format!("| {} |", sep_cells.join(" | "))
			} else {
				let padded: Vec<String> = (0..column_count)
					.map(|col_index| {
						let cell = cells.get(col_index).map(|s| s.trim()).unwrap_or("");
						let width = widths.get(col_index).copied().unwrap_or(0);
						format!("{:width$}", cell, width = width)
					})
					.collect();
				format!("| {} |", padded.join(" | "))
			}
		})
		.collect()
}

fn parse_table_row(row: &str) -> Vec<String> {
	let trimmed = row.trim();
	let inner = trimmed.trim_start_matches('|').trim_end_matches('|');
	inner.split('|').map(|s| s.to_string()).collect()
}

pub(super) fn render_markdown_line(line: &str, in_code_block: bool, code_color: Color) -> (Line<'static>, bool) {
	let expanded = expand_tabs(line);

	if expanded.trim().starts_with("```") {
		let style = Style::default().fg(code_color);
		return (Line::from(Span::styled(expanded, style)), !in_code_block);
	}

	if in_code_block {
		let style = Style::default().fg(code_color);
		return (Line::from(Span::styled(expanded, style)), true);
	}

	if expanded.trim().starts_with("    ") || expanded.trim().starts_with("\t") {
		let style = Style::default().fg(code_color);
		return (Line::from(Span::styled(expanded, style)), false);
	}

	let spans = parse_inline_markdown(&expanded, code_color);
	(Line::from(spans), false)
}

fn parse_inline_markdown(text: &str, code_color: Color) -> Vec<Span<'static>> {
	let mut spans: Vec<Span<'static>> = Vec::new();
	let mut current = String::new();
	let chars: Vec<char> = text.chars().collect();
	let len = chars.len();
	let mut i = 0;

	while i < len {
		if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
			if !current.is_empty() {
				spans.push(Span::raw(std::mem::take(&mut current)));
			}
			i += 2;
			let mut bold_text = String::new();
			while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '*') {
				bold_text.push(chars[i]);
				i += 1;
			}
			if i + 1 < len {
				i += 2;
			}
			if !bold_text.is_empty() {
				spans.push(Span::styled(bold_text, Style::default().add_modifier(Modifier::BOLD)));
			}
		} else if i + 1 < len && chars[i] == '_' && chars[i + 1] == '_' {
			if !current.is_empty() {
				spans.push(Span::raw(std::mem::take(&mut current)));
			}
			i += 2;
			let mut bold_text = String::new();
			while i + 1 < len && !(chars[i] == '_' && chars[i + 1] == '_') {
				bold_text.push(chars[i]);
				i += 1;
			}
			if i + 1 < len {
				i += 2;
			}
			if !bold_text.is_empty() {
				spans.push(Span::styled(bold_text, Style::default().add_modifier(Modifier::BOLD)));
			}
		} else if chars[i] == '`' {
			if !current.is_empty() {
				spans.push(Span::raw(std::mem::take(&mut current)));
			}
			i += 1;
			let mut code_text = String::new();
			while i < len && chars[i] != '`' {
				code_text.push(chars[i]);
				i += 1;
			}
			if i < len {
				i += 1;
			}
			if !code_text.is_empty() {
				spans.push(Span::styled(code_text, Style::default().fg(code_color)));
			}
		} else if chars[i] == '*' && (i == 0 || chars[i - 1] == ' ') {
			if !current.is_empty() {
				spans.push(Span::raw(std::mem::take(&mut current)));
			}
			i += 1;
			let mut italic_text = String::new();
			while i < len && chars[i] != '*' {
				italic_text.push(chars[i]);
				i += 1;
			}
			if i < len {
				i += 1;
			}
			if !italic_text.is_empty() {
				spans.push(Span::styled(italic_text, Style::default().add_modifier(Modifier::ITALIC)));
			}
		} else {
			current.push(chars[i]);
			i += 1;
		}
	}

	if !current.is_empty() {
		spans.push(Span::raw(current));
	}

	if spans.is_empty() {
		spans.push(Span::raw(""));
	}

	spans
}

pub(super) fn parse_snippet<'a>(snippet: &str, base_style: Style) -> Vec<Span<'a>> {
	let cleaned = expand_tabs(snippet).replace('\n', " ");
	let with_ellipsis = cleaned.replace('\x01', "…");

	let has_start_ellipsis = with_ellipsis.starts_with('…');
	let has_end_ellipsis = with_ellipsis.ends_with('…');

	let mut result = String::new();
	if !has_start_ellipsis {
		result.push_str("… ");
	}
	result.push_str(&with_ellipsis);
	if !has_end_ellipsis {
		result.push_str(" …");
	}

	let bold_style = base_style.add_modifier(Modifier::BOLD);
	let mut spans: Vec<Span<'a>> = Vec::new();
	let mut in_match = false;
	let mut current = String::new();

	for c in result.chars() {
		if c == '\x02' {
			if !current.is_empty() {
				spans.push(Span::styled(std::mem::take(&mut current), base_style));
			}
			in_match = true;
		} else if c == '\x03' {
			if !current.is_empty() {
				spans.push(Span::styled(std::mem::take(&mut current), bold_style));
			}
			in_match = false;
		} else {
			current.push(c);
		}
	}

	if !current.is_empty() {
		let style = if in_match { bold_style } else { base_style };
		spans.push(Span::styled(current, style));
	}

	spans
}
