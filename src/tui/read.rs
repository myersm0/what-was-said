use anyhow::Result;
use crossterm::event::KeyCode;
use ratatui::{
	layout::Rect,
	style::{Modifier, Style},
	text::{Line, Span, Text},
	widgets::{Block, Borders, Paragraph, Wrap},
	Frame,
};
use rusqlite::Connection;

use crate::storage;
use crate::util::{extract_group_key, truncate_str};
use super::{App, Mode, SummaryType};
use super::render::{
	align_markdown_tables, expand_tabs, render_markdown_line,
};

pub(super) fn handle_keys(app: &mut App, key_code: KeyCode, connection: &Connection) -> Result<()> {
	match key_code {
		KeyCode::Char('q') => app.should_quit = true,
		KeyCode::Char('b') | KeyCode::Esc => {
			app.mode = app.previous_mode;
			app.current_document = None;
		}
		KeyCode::Up => {
			if app.current_chunk_index > 0 {
				app.current_chunk_index -= 1;
			}
		}
		KeyCode::Down => {
			if app.current_chunk_index + 1 < app.total_chunks {
				app.current_chunk_index += 1;
			}
		}
		KeyCode::PageUp => {
			app.current_chunk_index = app.current_chunk_index.saturating_sub(5);
		}
		KeyCode::PageDown => {
			app.current_chunk_index = (app.current_chunk_index + 5).min(app.total_chunks.saturating_sub(1));
		}
		KeyCode::Char('y') => {
			yank_current_chunk(app);
		}
		KeyCode::Char('Y') => {
			yank_full_document(app);
		}
		KeyCode::Char('/') => {
			app.mode = Mode::Search;
		}
		KeyCode::Left => {
			app.navigate_group(connection, -1)?;
		}
		KeyCode::Right => {
			app.navigate_group(connection, 1)?;
		}
		KeyCode::Char('t') => {
			app.tag_input.clear();
			app.mode = Mode::TagEdit;
		}
		KeyCode::Char('g') => {
			app.current_chunk_index = 0;
		}
		KeyCode::Char('G') => {
			app.current_chunk_index = app.total_chunks.saturating_sub(1);
		}
		KeyCode::Char('d') => {
			if let Some(ref doc) = app.current_document {
				let doc_id = doc.id;
				if let Ok(Some(detailed)) = storage::get_derived_content(connection, doc_id, "detailed") {
					app.summary_content = Some(detailed.body);
					app.summary_type = SummaryType::Detailed;
					app.summary_doc_id = Some(doc_id);
					app.summary_scroll = 0;
					app.summary_return_mode = Mode::Read;
					app.mode = Mode::SummaryView;
				} else {
					app.status_message = Some("No summary - run 'commonplace derive'".to_string());
				}
			}
		}
		_ => {}
	}
	Ok(())
}

fn yank_current_chunk(app: &mut App) {
	if let Some(ref doc) = app.current_document {
		let mut chunk_counter = 0usize;
		let mut chunk_text: Option<String> = None;

		'outer: for entry in &doc.entries {
			if entry.chunks.is_empty() {
				if chunk_counter == app.current_chunk_index {
					chunk_text = Some(entry.body.clone());
					break 'outer;
				}
				chunk_counter += 1;
			} else {
				for chunk in &entry.chunks {
					if chunk_counter == app.current_chunk_index {
						chunk_text = Some(chunk.body.clone());
						break 'outer;
					}
					chunk_counter += 1;
				}
			}
		}

		if let Some(text) = chunk_text {
			if let Ok(mut clipboard) = arboard::Clipboard::new() {
				if clipboard.set_text(&text).is_ok() {
					app.status_message = Some("Copied chunk to clipboard".to_string());
				} else {
					app.status_message = Some("Failed to copy".to_string());
				}
			}
		}
	}
}

fn yank_full_document(app: &mut App) {
	if let Some(ref doc) = app.current_document {
		let full_text: String = doc.entries.iter()
			.map(|entry| {
				let mut parts = Vec::new();
				if let Some(ref heading) = entry.heading_title {
					let level = entry.heading_level.unwrap_or(1);
					let prefix = "#".repeat(level as usize);
					parts.push(format!("{} {}", prefix, heading));
				}
				if !entry.body.trim().is_empty() {
					parts.push(entry.body.clone());
				}
				parts.join("\n\n")
			})
			.filter(|s| !s.is_empty())
			.collect::<Vec<_>>()
			.join("\n\n");

		if let Ok(mut clipboard) = arboard::Clipboard::new() {
			if clipboard.set_text(&full_text).is_ok() {
				app.status_message = Some("Copied full document to clipboard".to_string());
			} else {
				app.status_message = Some("Failed to copy".to_string());
			}
		}
	}
}

pub(super) fn draw(frame: &mut Frame, app: &App, area: Rect) {
	let Some(ref doc) = app.current_document else {
		return;
	};

	let theme = &app.theme;
	let mut all_lines: Vec<(Line, bool)> = Vec::new();
	let mut chunk_counter = 0usize;
	let mut in_code_block = false;

	for entry in &doc.entries {
		if let Some(ref heading) = entry.heading_title {
			let level = entry.heading_level.unwrap_or(1);
			let prefix = "#".repeat(level as usize);
			let heading_line = Line::from(Span::styled(
				expand_tabs(&format!("{} {}", prefix, heading)),
				Style::default().fg(theme.heading).add_modifier(Modifier::BOLD),
			));
			all_lines.push((heading_line, false));
			all_lines.push((Line::from(""), false));
		}

		if let Some(ref author) = entry.author {
			all_lines.push((
				Line::from(Span::styled(
					format!("[{}]", author),
					Style::default().fg(theme.text_accent),
				)),
				false,
			));
		}

		if entry.chunks.is_empty() {
			if entry.body.trim().is_empty() {
				continue;
			}
			let is_current = chunk_counter == app.current_chunk_index;
			let aligned_body = align_markdown_tables(&entry.body);
			for body_line in aligned_body.lines() {
				let (line, new_in_code_block) = render_markdown_line(body_line, in_code_block, theme.code);
				in_code_block = new_in_code_block;
				all_lines.push((line, is_current));
			}
			chunk_counter += 1;
		} else {
			for chunk in &entry.chunks {
				let is_current = chunk_counter == app.current_chunk_index;
				let aligned_body = align_markdown_tables(&chunk.body);
				for body_line in aligned_body.lines() {
					let (line, new_in_code_block) = render_markdown_line(body_line, in_code_block, theme.code);
					in_code_block = new_in_code_block;
					all_lines.push((line, is_current));
				}
				chunk_counter += 1;
			}
		}
		all_lines.push((Line::from(""), false));
	}

	let first_current_line = all_lines.iter()
		.enumerate()
		.find(|(_, (_, is_current))| *is_current)
		.map(|(i, _)| i)
		.unwrap_or(0);

	let view_height = area.height.saturating_sub(2) as usize;
	let scroll = first_current_line.saturating_sub(view_height / 3);

	let visible_lines: Vec<Line> = all_lines
		.into_iter()
		.skip(scroll)
		.take(view_height)
		.map(|(line, is_current)| {
			if is_current {
				let spans: Vec<Span> = line.spans.into_iter().map(|span| {
					Span::styled(
						span.content.to_string(),
						span.style.bg(theme.highlight_bg),
					)
				}).collect();
				Line::from(spans)
			} else {
				line
			}
		})
		.collect();

	let group_info = if app.group_docs.len() > 1 {
		format!(" [◀ {}/{} ▶]", app.group_index + 1, app.group_docs.len())
	} else {
		String::new()
	};

	let tags_info = if app.current_doc_tags.is_empty() {
		String::new()
	} else {
		format!(" [{}]", app.current_doc_tags.join(", "))
	};

	let display_title = if app.group_docs.len() > 1 {
		extract_group_key(&doc.source_title)
			.unwrap_or_else(|| doc.title.clone().unwrap_or_else(|| doc.source_title.clone()))
	} else {
		doc.title.clone().unwrap_or_else(|| doc.source_title.clone())
	};

	let date_str = &doc.clip_date[..10.min(doc.clip_date.len())];

	let paragraph = Paragraph::new(Text::from(visible_lines))
		.block(
			Block::default()
				.title(Span::styled(
					format!(
						" {} | {} | chunk {}/{}{}{}",
						truncate_str(&display_title, 35),
						date_str,
						app.current_chunk_index + 1,
						app.total_chunks.max(1),
						tags_info,
						group_info,
					),
					Style::default().fg(theme.title),
				))
				.borders(Borders::ALL)
				.border_style(Style::default().fg(theme.border_focused)),
		)
		.wrap(Wrap { trim: false });

	frame.render_widget(paragraph, area);
}
