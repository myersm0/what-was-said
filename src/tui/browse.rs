use anyhow::Result;
use crossterm::event::KeyCode;
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	style::{Modifier, Style},
	text::{Line, Span},
	widgets::{Block, Borders, List, ListItem, Paragraph},
	Frame,
};
use rusqlite::Connection;

use crate::storage::{self, SortColumn, SortDirection};
use crate::util::truncate_str;
use super::{App, Mode, SummaryType};
use super::theme::parse_color;

pub(super) fn handle_keys(app: &mut App, key_code: KeyCode, connection: &Connection) -> Result<()> {
	match key_code {
		KeyCode::Char('q') => app.should_quit = true,
		KeyCode::Up => {
			if let Some(selected) = app.browse_state.selected() {
				if selected > 0 {
					app.browse_state.select(Some(selected - 1));
				}
			}
		}
		KeyCode::Down => {
			let max_idx = app.filtered_documents().len().saturating_sub(1);
			if let Some(selected) = app.browse_state.selected() {
				if selected < max_idx {
					app.browse_state.select(Some(selected + 1));
				}
			}
		}
		KeyCode::Enter => {
			app.open_selected_document(connection)?;
		}
		KeyCode::Char('/') => {
			app.mode = Mode::Search;
			app.search_query.clear();
			app.search_results.clear();
		}
		KeyCode::Char('s') => {
			app.cycle_sort();
			app.load_documents(connection)?;
		}
		KeyCode::Char('S') => {
			app.toggle_sort_direction();
			app.load_documents(connection)?;
		}
		KeyCode::Char('f') => {
			app.load_all_tags(connection)?;
			if !app.all_tags.is_empty() {
				app.tag_filter_index = 0;
				app.mode = Mode::TagFilter;
			} else {
				app.status_message = Some("No tags defined".to_string());
			}
		}
		KeyCode::Char('F') => {
			app.clear_tag_filter();
			app.browse_state.select(Some(0));
			app.status_message = Some("Filter cleared".to_string());
		}
		KeyCode::Char('g') => {
			app.browse_state.select(Some(0));
		}
		KeyCode::Char('G') => {
			let max_idx = app.filtered_documents().len().saturating_sub(1);
			app.browse_state.select(Some(max_idx));
		}
		KeyCode::Char('m') => {
			if let Some(selected) = app.browse_state.selected() {
				let filtered = app.filtered_documents();
				if let Some(doc) = filtered.get(selected) {
					let doc_id = doc.id;
					app.toggle_mark(doc_id);
					let mark_count = app.marked_docs.len();
					app.status_message = Some(format!("{} docs marked", mark_count));
				}
			}
		}
		KeyCode::Char('M') => {
			app.clear_marks();
			app.status_message = Some("Marks cleared".to_string());
		}
		KeyCode::Char('d') => {
			if let Some(selected) = app.browse_state.selected() {
				let filtered = app.filtered_documents();
				if let Some(doc) = filtered.get(selected) {
					let doc_id = doc.id;
					if let Ok(Some(brief)) = storage::get_derived_content(connection, doc_id, "brief") {
						app.summary_content = Some(brief.body);
						app.summary_type = SummaryType::Brief;
						app.summary_doc_id = Some(doc_id);
						app.summary_scroll = 0;
						app.summary_return_mode = Mode::Browse;
						app.mode = Mode::SummaryView;
					} else {
						app.status_message = Some("No summary - run 'what-was-said derive'".to_string());
					}
				}
			}
		}
		_ => {}
	}
	Ok(())
}

pub(super) fn draw(frame: &mut Frame, app: &App, area: Rect) {
	let theme = &app.theme;
	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([Constraint::Length(2), Constraint::Min(1)])
		.split(area);

	let sort_indicator = |col: SortColumn| {
		if app.sort_column == col {
			match app.sort_direction {
				SortDirection::Ascending => " ▲",
				SortDirection::Descending => " ▼",
			}
		} else {
			"  "
		}
	};

	let header_style = Style::default().fg(theme.column_header).add_modifier(Modifier::BOLD);

	const max_dots: usize = 4;

	let header = Line::from(vec![
		Span::raw("    "),
		Span::raw(format!("{:<width$}", "", width = max_dots)),
		Span::styled(
			format!("{:<43}", format!("Source{}", sort_indicator(SortColumn::Source))),
			header_style,
		),
		Span::styled("│ ", Style::default().fg(theme.border)),
		Span::styled(
			format!("{:<10}", format!("Type{}", sort_indicator(SortColumn::Doctype))),
			header_style,
		),
		Span::styled("│ ", Style::default().fg(theme.border)),
		Span::styled(
			format!("{:<12}", format!("Date{}", sort_indicator(SortColumn::Date))),
			header_style,
		),
		Span::styled("│ ", Style::default().fg(theme.border)),
		Span::styled("Preview", header_style),
	]);

	let filtered = app.filtered_documents();
	let total_after_global = app.documents.iter()
		.filter(|d| app.passes_global_filter(d))
		.count();

	let title = if app.global_include.is_some() || !app.global_exclude.is_empty() {
		match &app.tag_filter {
			Some(tag) => format!(" BROWSE ({}/{} docs, filter: {}) [restricted] ", filtered.len(), total_after_global, tag),
			None => format!(" BROWSE ({}/{} docs) [restricted] ", filtered.len(), app.documents.len()),
		}
	} else {
		match &app.tag_filter {
			Some(tag) => format!(" BROWSE ({}/{} docs, filter: {}) ", filtered.len(), total_after_global, tag),
			None => format!(" BROWSE ({} documents) ", filtered.len()),
		}
	};

	let header_para = Paragraph::new(header)
		.block(Block::default()
			.title(Span::styled(title, Style::default().fg(theme.title)))
			.borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
			.border_style(Style::default().fg(theme.border)));
	frame.render_widget(header_para, chunks[0]);

	let items: Vec<ListItem> = filtered
		.iter()
		.map(|doc| {
			let mark_indicator = if app.marked_docs.contains(&doc.id) { "* " } else { "  " };
			let doctype = doc.doctype_name.as_deref().unwrap_or("-");
			let date = &doc.clip_date[..10.min(doc.clip_date.len())];

			let mark_style = if app.marked_docs.contains(&doc.id) {
				Style::default().fg(theme.mark)
			} else {
				Style::default().fg(theme.text)
			};

			let mut tag_dots: Vec<Span> = Vec::new();
			for tag in &doc.tags {
				if tag_dots.len() >= max_dots {
					break;
				}
				if let Some(color) = app.tag_config.colors.get(tag.as_str()).and_then(|c| parse_color(c)) {
					tag_dots.push(Span::styled("*", Style::default().fg(color)));
				}
			}
			let padding = max_dots.saturating_sub(tag_dots.len());

			let source = truncate_str(&doc.source_title, 41);

			let preview = doc.brief_summary.as_deref()
				.unwrap_or_else(|| {
					doc.first_line.as_deref()
						.and_then(|s| s.lines().next())
						.map(|s| s.trim())
						.unwrap_or("")
				});

			let mut spans = vec![Span::styled(mark_indicator, mark_style)];
			spans.extend(tag_dots);
			if padding > 0 {
				spans.push(Span::raw(format!("{:<width$}", "", width = padding)));
			}
			spans.push(Span::styled(format!("{:<43}", source), Style::default().fg(theme.text)));
			spans.push(Span::styled("│ ", Style::default().fg(theme.border)));
			spans.push(Span::styled(format!("{:<10}", truncate_str(doctype, 8)), Style::default().fg(theme.text_muted)));
			spans.push(Span::styled("│ ", Style::default().fg(theme.border)));
			spans.push(Span::styled(format!("{:<12}", date), Style::default().fg(theme.text_muted)));
			spans.push(Span::styled("│ ", Style::default().fg(theme.border)));
			spans.push(Span::styled(
				truncate_str(preview, 60).to_string(),
				Style::default().fg(theme.text_muted),
			));

			ListItem::new(Line::from(spans))
		})
		.collect();

	let list = List::new(items)
		.block(Block::default()
			.borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
			.border_style(Style::default().fg(theme.border)))
		.highlight_style(
			Style::default()
				.bg(theme.highlight_bg)
				.fg(theme.highlight_fg)
				.add_modifier(Modifier::BOLD),
		)
		.highlight_symbol("> ");

	let mut state = app.browse_state.clone();
	if let Some(selected) = state.selected() {
		let view_height = chunks[1].height.saturating_sub(2) as usize;
		let offset = selected.saturating_sub(view_height / 2);
		*state.offset_mut() = offset;
	}
	frame.render_stateful_widget(list, chunks[1], &mut state);
}
