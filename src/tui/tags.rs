use anyhow::Result;
use crossterm::event::KeyCode;
use ratatui::{
	layout::Rect,
	style::{Modifier, Style},
	text::{Line, Span, Text},
	widgets::{Block, Borders, Clear, Paragraph},
	Frame,
};
use rusqlite::Connection;

use crate::storage;
use super::{App, Mode};
use super::render::centered_rect;

pub(super) fn handle_edit_keys(app: &mut App, key_code: KeyCode, connection: &Connection) -> Result<()> {
	match key_code {
		KeyCode::Esc => {
			app.mode = Mode::Read;
			app.tag_input.clear();
		}
		KeyCode::Enter => {
			add_current_tag(app, connection)?;
			app.load_all_tags(connection)?;
		}
		KeyCode::Backspace => {
			app.tag_input.pop();
		}
		KeyCode::Char(c) => {
			app.tag_input.push(c);
		}
		_ => {}
	}
	Ok(())
}

pub(super) fn handle_filter_keys(app: &mut App, key_code: KeyCode) -> Result<()> {
	match key_code {
		KeyCode::Esc => {
			app.mode = Mode::Browse;
		}
		KeyCode::Enter => {
			apply_tag_filter(app);
			app.browse_state.select(Some(0));
			app.mode = Mode::Browse;
		}
		KeyCode::Up => {
			if app.tag_filter_index > 0 {
				app.tag_filter_index -= 1;
			}
		}
		KeyCode::Down => {
			if app.tag_filter_index + 1 < app.all_tags.len() {
				app.tag_filter_index += 1;
			}
		}
		KeyCode::Char('g') => {
			app.tag_filter_index = 0;
		}
		KeyCode::Char('G') => {
			app.tag_filter_index = app.all_tags.len().saturating_sub(1);
		}
		KeyCode::Char('c') => {
			app.clear_tag_filter();
			app.browse_state.select(Some(0));
			app.mode = Mode::Browse;
			app.status_message = Some("Filter cleared".to_string());
		}
		_ => {}
	}
	Ok(())
}

fn add_current_tag(app: &mut App, connection: &Connection) -> Result<()> {
	let input = app.tag_input.trim().to_lowercase().replace(' ', "-");
	if input.is_empty() {
		return Ok(());
	}

	if let Some(ref doc) = app.current_document {
		if let Ok(idx) = input.parse::<usize>() {
			if idx > 0 && idx <= app.current_doc_tags.len() {
				let tag = app.current_doc_tags[idx - 1].clone();
				storage::remove_tag(connection, doc.id, &tag)?;
				app.current_doc_tags.remove(idx - 1);
				app.tag_input.clear();
				app.load_documents(connection)?;
				app.status_message = Some(format!("Removed tag: {}", tag));
				return Ok(());
			}
		}

		if !app.current_doc_tags.contains(&input) {
			storage::add_tag(connection, doc.id, &input)?;
			app.current_doc_tags.push(input.clone());
			app.current_doc_tags.sort();
			app.status_message = Some(format!("Added tag: {}", input));
		}
		app.tag_input.clear();
		app.load_documents(connection)?;
	}
	Ok(())
}

fn apply_tag_filter(app: &mut App) {
	if app.all_tags.is_empty() {
		app.tag_filter = None;
	} else if app.tag_filter_index < app.all_tags.len() {
		app.tag_filter = Some(app.all_tags[app.tag_filter_index].0.clone());
	}
}

pub(super) fn draw_edit_popup(frame: &mut Frame, app: &App) {
	let theme = &app.theme;
	let area = centered_rect(50, 12, frame.area());
	frame.render_widget(Clear, area);

	let mut lines: Vec<Line> = vec![
		Line::from(Span::styled("Current tags:", Style::default().fg(theme.text))),
		Line::from(""),
	];

	if app.current_doc_tags.is_empty() {
		lines.push(Line::from(Span::styled("  (none)", Style::default().fg(theme.text_muted))));
	} else {
		for (i, tag) in app.current_doc_tags.iter().enumerate() {
			lines.push(Line::from(Span::styled(
				format!("  {}. {}", i + 1, tag),
				Style::default().fg(theme.text),
			)));
		}
	}

	lines.push(Line::from(""));
	lines.push(Line::from(Span::styled("Type tag name + Enter to add", Style::default().fg(theme.text_muted))));
	lines.push(Line::from(Span::styled("Type number + Enter to remove", Style::default().fg(theme.text_muted))));
	lines.push(Line::from(Span::styled(
		format!("> {}_", app.tag_input),
		Style::default().fg(theme.text_accent),
	)));

	let popup = Paragraph::new(Text::from(lines))
		.block(
			Block::default()
				.title(Span::styled(" Edit Tags ", Style::default().fg(theme.title)))
				.borders(Borders::ALL)
				.border_style(Style::default().fg(theme.border_popup))
				.style(Style::default().bg(theme.background)),
		);

	frame.render_widget(popup, area);
}

pub(super) fn draw_filter_popup(frame: &mut Frame, app: &App) {
	let theme = &app.theme;
	let max_height = (frame.area().height * 70 / 100).max(10);
	let height = (app.all_tags.len() as u16 + 4).min(max_height);
	let area = centered_rect(50, height, frame.area());
	frame.render_widget(Clear, area);

	let inner_height = height.saturating_sub(4) as usize;
	let scroll_offset = if app.all_tags.len() <= inner_height {
		0
	} else {
		let half = inner_height / 2;
		if app.tag_filter_index < half {
			0
		} else if app.tag_filter_index >= app.all_tags.len() - half {
			app.all_tags.len() - inner_height
		} else {
			app.tag_filter_index - half
		}
	};

	let mut lines: Vec<Line> = vec![
		Line::from(Span::styled(
			format!("Select tag ({}/{}):", app.tag_filter_index + 1, app.all_tags.len()),
			Style::default().fg(theme.text),
		)),
		Line::from(""),
	];

	for (i, (tag, count)) in app.all_tags.iter().enumerate().skip(scroll_offset).take(inner_height) {
		let style = if i == app.tag_filter_index {
			Style::default().bg(theme.highlight_bg).fg(theme.highlight_fg).add_modifier(Modifier::BOLD)
		} else {
			Style::default().fg(theme.text)
		};
		let marker = if i == app.tag_filter_index { "> " } else { "  " };
		lines.push(Line::from(Span::styled(
			format!("{}{} ({})", marker, tag, count),
			style,
		)));
	}

	let popup = Paragraph::new(Text::from(lines))
		.block(
			Block::default()
				.title(Span::styled(" Filter by Tag ", Style::default().fg(theme.title)))
				.borders(Borders::ALL)
				.border_style(Style::default().fg(theme.border_popup))
				.style(Style::default().bg(theme.background)),
		);

	frame.render_widget(popup, area);
}
