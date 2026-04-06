use anyhow::Result;
use crossterm::event::KeyCode;
use ratatui::{
	layout::Rect,
	style::Style,
	text::{Line, Span, Text},
	widgets::{Block, Borders, Clear, Paragraph, Wrap},
	Frame,
};
use rusqlite::Connection;

use crate::storage;
use super::{App, SummaryType};

pub(super) fn handle_keys(app: &mut App, key_code: KeyCode, connection: &Connection) -> Result<()> {
	match key_code {
		KeyCode::Esc | KeyCode::Char('q') => {
			app.mode = app.summary_return_mode;
			app.summary_content = None;
		}
		KeyCode::Up => {
			app.summary_scroll = app.summary_scroll.saturating_sub(1);
		}
		KeyCode::Down => {
			app.summary_scroll += 1;
		}
		KeyCode::Char('d') => {
			if let Some(doc_id) = app.summary_doc_id {
				let new_type = match app.summary_type {
					SummaryType::Brief => SummaryType::Detailed,
					SummaryType::Detailed => SummaryType::Brief,
				};
				let content_type = match new_type {
					SummaryType::Brief => "brief",
					SummaryType::Detailed => "detailed",
				};
				if let Ok(Some(content)) = storage::get_derived_content(connection, doc_id, content_type) {
					app.summary_content = Some(content.body);
					app.summary_type = new_type;
					app.summary_scroll = 0;
				}
			}
		}
		KeyCode::Char('y') => {
			if let Some(ref content) = app.summary_content {
				if let Ok(mut clipboard) = arboard::Clipboard::new() {
					let _ = clipboard.set_text(content.clone());
					app.status_message = Some("Summary copied".to_string());
				}
			}
		}
		KeyCode::Char('x') => {
			if let Some(doc_id) = app.summary_doc_id {
				let content_type = match app.summary_type {
					SummaryType::Brief => "brief",
					SummaryType::Detailed => "detailed",
				};
				if let Ok(Some(content)) = storage::get_derived_content(connection, doc_id, content_type) {
					let _ = storage::set_derived_quality(connection, content.id, "bad");
					app.status_message = Some("Marked as bad".to_string());
					app.mode = app.summary_return_mode;
					app.summary_content = None;
				}
			}
		}
		_ => {}
	}
	Ok(())
}

pub(super) fn draw_popup(frame: &mut Frame, app: &App) {
	let theme = &app.theme;
	let height = (frame.area().height * 70 / 100).max(10);
	let width = (frame.area().width * 80 / 100).max(40);
	let area = Rect::new(
		(frame.area().width.saturating_sub(width)) / 2,
		(frame.area().height.saturating_sub(height)) / 2,
		width,
		height,
	);
	frame.render_widget(Clear, area);

	let type_str = match app.summary_type {
		SummaryType::Brief => "Brief",
		SummaryType::Detailed => "Detailed",
	};

	let content = app.summary_content.as_deref().unwrap_or("(no content)");
	let lines: Vec<Line> = content
		.lines()
		.skip(app.summary_scroll)
		.take(height.saturating_sub(2) as usize)
		.map(|line| Line::from(Span::styled(line.to_string(), Style::default().fg(theme.text))))
		.collect();

	let popup = Paragraph::new(Text::from(lines))
		.block(
			Block::default()
				.title(Span::styled(
					format!(" {} Summary [d: toggle] [y: copy] [x: mark bad] ", type_str),
					Style::default().fg(theme.title),
				))
				.borders(Borders::ALL)
				.border_style(Style::default().fg(theme.border_popup))
				.style(Style::default().bg(theme.background)),
		)
		.wrap(Wrap { trim: false });

	frame.render_widget(popup, area);
}
