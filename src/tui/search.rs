use anyhow::Result;
use crossterm::event::KeyCode;
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	style::{Modifier, Style},
	text::{Line, Span, Text},
	widgets::{Block, Borders, Paragraph},
	Frame,
};
use rusqlite::Connection;

use crate::storage::{self, GroupedSearchResult, SearchSortColumn};
use crate::util::truncate_str;
use super::{App, Mode, SearchConfig, SearchField, SearchMode};
use super::render::parse_snippet;

pub(super) fn handle_keys(
	app: &mut App,
	key_code: KeyCode,
	connection: &Connection,
	search_config: &SearchConfig,
) -> Result<()> {
	match key_code {
		KeyCode::Char('`') => {
			app.search_mode = match app.search_mode {
				SearchMode::Fts5 => SearchMode::Semantic,
				SearchMode::Semantic => SearchMode::Fts5,
			};
			run_search(app, connection, search_config)?;
		}
		KeyCode::Esc => {
			app.mode = Mode::Browse;
		}
		KeyCode::Enter => {
			if app.total_search_chunks > 0 {
				open_search_result(app, connection)?;
			}
		}
		KeyCode::Up => {
			if app.search_chunk_index > 0 {
				app.search_chunk_index -= 1;
			}
		}
		KeyCode::Down => {
			if app.search_chunk_index + 1 < app.total_search_chunks {
				app.search_chunk_index += 1;
			}
		}
		KeyCode::Tab => {
			app.search_field = match app.search_field {
				SearchField::Query => SearchField::Author,
				SearchField::Author => SearchField::DateFrom,
				SearchField::DateFrom => SearchField::DateTo,
				SearchField::DateTo => SearchField::Query,
			};
		}
		KeyCode::BackTab => {
			app.search_field = match app.search_field {
				SearchField::Query => SearchField::DateTo,
				SearchField::Author => SearchField::Query,
				SearchField::DateFrom => SearchField::Author,
				SearchField::DateTo => SearchField::DateFrom,
			};
		}
		KeyCode::Char(c) => {
			match app.search_field {
				SearchField::Query => app.search_query.push(c),
				SearchField::Author => app.author_filter.push(c),
				SearchField::DateFrom => app.date_from.push(c),
				SearchField::DateTo => app.date_to.push(c),
			}
			run_search(app, connection, search_config)?;
		}
		KeyCode::Backspace => {
			match app.search_field {
				SearchField::Query => { app.search_query.pop(); }
				SearchField::Author => { app.author_filter.pop(); }
				SearchField::DateFrom => { app.date_from.pop(); }
				SearchField::DateTo => { app.date_to.pop(); }
			}
			run_search(app, connection, search_config)?;
		}
		_ => {}
	}
	Ok(())
}

fn run_search(app: &mut App, connection: &Connection, search_config: &SearchConfig) -> Result<()> {
	if app.search_query.is_empty() {
		app.search_results.clear();
		app.total_search_chunks = 0;
		return Ok(());
	}

	let author_filter = if app.author_filter.is_empty() { None } else { Some(app.author_filter.as_str()) };
	let date_from = if app.date_from.is_empty() { None } else { Some(app.date_from.as_str()) };
	let date_to = if app.date_to.is_empty() { None } else { Some(app.date_to.as_str()) };

	let allowed_doc_ids: std::collections::HashSet<i64> = app.documents.iter()
		.filter(|d| app.passes_global_filter(d))
		.map(|d| d.id)
		.collect();

	match app.search_mode {
		SearchMode::Fts5 => {
			let all_results = storage::search_filtered(
				connection,
				&app.search_query,
				app.search_sort_column,
				author_filter,
				date_from,
				date_to,
			)?;

			app.search_results = all_results.into_iter()
				.filter(|r| allowed_doc_ids.contains(&r.document_id))
				.collect();

			app.total_search_chunks = app.search_results.iter().map(|r| r.chunks.len()).sum();
		}
		SearchMode::Semantic => {
			if !storage::vec_table_exists(connection) {
				app.status_message = Some("No embeddings - run 'commonplace embed' first".to_string());
				return Ok(());
			}

			let query_embedding = match search_config.backend.embed(&app.search_query, &search_config.embed_model) {
				Ok(emb) => emb,
				Err(e) => {
					app.status_message = Some(format!("Embed error: {}", e));
					return Ok(());
				}
			};

			let all_results = storage::find_similar_chunks_filtered(
				connection,
				&query_embedding,
				50,
				author_filter,
				date_from,
				date_to,
			)?;

			let filtered: Vec<_> = all_results.into_iter()
				.filter(|r| allowed_doc_ids.contains(&r.document_id))
				.take(30)
				.collect();

			let mut grouped: Vec<GroupedSearchResult> = Vec::new();
			for chunk in filtered {
				let rank = -(chunk.similarity as f64);
				let hit = storage::ChunkHit {
					entry_id: 0,
					entry_position: chunk.entry_position,
					chunk_index: chunk.chunk_index,
					chunk_body: chunk.body.clone(),
					snippet: chunk.body.chars().take(150).collect(),
					author: chunk.author,
					heading_title: None,
					rank,
				};

				if let Some(doc) = grouped.iter_mut().find(|d| d.document_id == chunk.document_id) {
					if rank < doc.best_rank {
						doc.best_rank = rank;
					}
					doc.chunks.push(hit);
				} else {
					grouped.push(GroupedSearchResult {
						document_id: chunk.document_id,
						source_title: chunk.source_title,
						clip_date: chunk.clip_date,
						best_rank: rank,
						chunks: vec![hit],
					});
				}
			}

			for doc in &mut grouped {
				doc.chunks.sort_by(|a, b| a.rank.partial_cmp(&b.rank).unwrap_or(std::cmp::Ordering::Equal));
			}
			grouped.sort_by(|a, b| a.best_rank.partial_cmp(&b.best_rank).unwrap_or(std::cmp::Ordering::Equal));

			app.search_results = grouped;
			app.total_search_chunks = app.search_results.iter().map(|r| r.chunks.len()).sum();
		}
	}

	app.search_chunk_index = 0;
	Ok(())
}

fn open_search_result(app: &mut App, connection: &Connection) -> Result<()> {
	let chunk_info = get_selected_chunk(app);
	if let Some((document_id, entry_position, chunk_index)) = chunk_info {
		app.current_document = storage::get_document(connection, document_id)?;
		app.scroll_offset = 0;
		app.total_chunks = app.current_document.as_ref()
			.map(super::document_chunk_count)
			.unwrap_or(0);

		app.current_chunk_index = app.current_document.as_ref()
			.map(|doc| {
				let mut flat_index = 0usize;
				for entry in &doc.entries {
					if entry.position < entry_position {
						flat_index += super::entry_chunk_count(entry);
					} else if entry.position == entry_position {
						flat_index += chunk_index as usize;
						break;
					}
				}
				flat_index
			})
			.unwrap_or(0);

		app.previous_mode = Mode::Search;
		app.mode = Mode::Read;
	}
	Ok(())
}

fn get_selected_chunk(app: &App) -> Option<(i64, u32, u32)> {
	let mut chunk_counter = 0usize;
	for result in &app.search_results {
		for chunk in &result.chunks {
			if chunk_counter == app.search_chunk_index {
				return Some((result.document_id, chunk.entry_position, chunk.chunk_index));
			}
			chunk_counter += 1;
		}
	}
	None
}

pub(super) fn draw(frame: &mut Frame, app: &App, area: Rect) {
	let theme = &app.theme;
	let layout = Layout::default()
		.direction(Direction::Vertical)
		.constraints([Constraint::Length(5), Constraint::Min(1)])
		.split(area);

	let mode_str = match app.search_mode {
		SearchMode::Fts5 => "FTS5",
		SearchMode::Semantic => "Semantic",
	};

	let input_layout = Layout::default()
		.direction(Direction::Vertical)
		.constraints([Constraint::Length(3), Constraint::Length(1)])
		.split(layout[0]);

	let filter_layout = Layout::default()
		.direction(Direction::Horizontal)
		.constraints([
			Constraint::Length(20),
			Constraint::Length(15),
			Constraint::Length(15),
			Constraint::Min(1),
		])
		.split(input_layout[1]);

	let field_style = |field: SearchField| {
		if app.search_field == field {
			Style::default().fg(theme.text_accent)
		} else {
			Style::default().fg(theme.text_muted)
		}
	};

	let query_block = Block::default()
		.title(Span::styled(
			format!(" Search [{}] ({}) [`: toggle mode] ", mode_str, app.total_search_chunks),
			Style::default().fg(theme.title),
		))
		.borders(Borders::ALL)
		.border_style(field_style(SearchField::Query));
	let query_input = Paragraph::new(Span::styled(
		app.search_query.as_str(),
		Style::default().fg(theme.text),
	)).block(query_block);
	frame.render_widget(query_input, input_layout[0]);

	let author_text = format!(" Author: {} ", app.author_filter);
	frame.render_widget(Paragraph::new(author_text).style(field_style(SearchField::Author)), filter_layout[0]);

	let date_from_text = format!(" From: {} ", app.date_from);
	frame.render_widget(Paragraph::new(date_from_text).style(field_style(SearchField::DateFrom)), filter_layout[1]);

	let date_to_text = format!(" To: {} ", app.date_to);
	frame.render_widget(Paragraph::new(date_to_text).style(field_style(SearchField::DateTo)), filter_layout[2]);

	let mut lines: Vec<Line> = Vec::new();
	let mut chunk_counter = 0usize;

	for result in &app.search_results {
		let score_str = format!("{:.2}", -result.best_rank);
		let date_str = &result.clip_date[..10.min(result.clip_date.len())];
		lines.push(Line::from(vec![
			Span::styled(
				format!("▸ {} ", truncate_str(&result.source_title, 50)),
				Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
			),
			Span::styled(
				format!("  score: {}  date: {}  ", score_str, date_str),
				Style::default().fg(theme.text_muted),
			),
			Span::styled(
				format!("[{} chunks]", result.chunks.len()),
				Style::default().fg(theme.text_muted),
			),
		]));

		for chunk in &result.chunks {
			let is_current = chunk_counter == app.search_chunk_index;
			let base_style = if is_current {
				Style::default().bg(theme.highlight_bg).fg(theme.highlight_fg)
			} else {
				Style::default().fg(theme.text)
			};
			let prefix = if is_current { "> " } else { "  " };

			let mut spans = vec![Span::styled(format!("{}│ ", prefix), base_style)];
			spans.extend(parse_snippet(&chunk.snippet, base_style));

			lines.push(Line::from(spans));
			chunk_counter += 1;
		}
		lines.push(Line::from(""));
	}

	let current_line = {
		let mut counter = 0usize;
		let mut line_index = 0usize;
		for result in &app.search_results {
			line_index += 1;
			for _ in &result.chunks {
				if counter == app.search_chunk_index {
					break;
				}
				counter += 1;
				line_index += 1;
			}
			if counter == app.search_chunk_index {
				break;
			}
			line_index += 1;
		}
		line_index
	};

	let view_height = layout[1].height.saturating_sub(2) as usize;
	let scroll = current_line.saturating_sub(view_height / 3);

	let visible_lines: Vec<Line> = lines
		.into_iter()
		.skip(scroll)
		.take(view_height)
		.collect();

	let paragraph = Paragraph::new(Text::from(visible_lines))
		.block(Block::default()
			.borders(Borders::ALL)
			.border_style(Style::default().fg(theme.border)));

	frame.render_widget(paragraph, layout[1]);
}
