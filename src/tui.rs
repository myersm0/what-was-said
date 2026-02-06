use anyhow::Result;
use crossterm::{
	event::{self, Event, KeyCode, KeyEventKind},
	execute,
	terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
	backend::CrosstermBackend,
	layout::{Constraint, Direction, Layout, Rect},
	style::{Color, Modifier, Style},
	text::{Line, Span, Text},
	widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
	Frame, Terminal,
};
use rusqlite::Connection;
use std::io;

use crate::storage::{
	self, DocumentContent, DocumentSummary,
	GroupedSearchResult, SearchSortColumn, SortColumn, SortDirection,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
	Browse,
	Read,
	Search,
}

struct App {
	mode: Mode,
	previous_mode: Mode,
	documents: Vec<DocumentSummary>,
	browse_state: ListState,
	sort_column: SortColumn,
	sort_direction: SortDirection,
	current_document: Option<DocumentContent>,
	scroll_offset: usize,
	current_chunk_index: usize,
	total_chunks: usize,
	search_query: String,
	search_results: Vec<GroupedSearchResult>,
	search_sort_column: SearchSortColumn,
	search_chunk_index: usize,
	total_search_chunks: usize,
	should_quit: bool,
	status_message: Option<String>,
}

impl App {
	fn new() -> Self {
		let mut browse_state = ListState::default();
		browse_state.select(Some(0));
		App {
			mode: Mode::Browse,
			previous_mode: Mode::Browse,
			documents: Vec::new(),
			browse_state,
			sort_column: SortColumn::Date,
			sort_direction: SortDirection::Descending,
			current_document: None,
			scroll_offset: 0,
			current_chunk_index: 0,
			total_chunks: 0,
			search_query: String::new(),
			search_results: Vec::new(),
			search_sort_column: SearchSortColumn::Score,
			search_chunk_index: 0,
			total_search_chunks: 0,
			should_quit: false,
			status_message: None,
		}
	}

	fn load_documents(&mut self, connection: &Connection) -> Result<()> {
		self.documents = storage::list_documents(connection, self.sort_column, self.sort_direction)?;
		if self.documents.is_empty() {
			self.browse_state.select(None);
		} else if self.browse_state.selected().is_none() {
			self.browse_state.select(Some(0));
		}
		Ok(())
	}

	fn open_selected_document(&mut self, connection: &Connection) -> Result<()> {
		if let Some(selected) = self.browse_state.selected() {
			if let Some(doc) = self.documents.get(selected) {
				self.current_document = storage::get_document(connection, doc.id)?;
				self.scroll_offset = 0;
				self.current_chunk_index = 0;
				self.total_chunks = self.current_document.as_ref()
					.map(|d| d.entries.iter().map(|e| e.chunks.len().max(1)).sum())
					.unwrap_or(0);
				self.previous_mode = Mode::Browse;
				self.mode = Mode::Read;
			}
		}
		Ok(())
	}

	fn run_search(&mut self, connection: &Connection) -> Result<()> {
		if self.search_query.is_empty() {
			self.search_results.clear();
			self.total_search_chunks = 0;
			return Ok(());
		}
		self.search_results = storage::search(connection, &self.search_query, self.search_sort_column)?;
		self.total_search_chunks = self.search_results.iter().map(|r| r.chunks.len()).sum();
		self.search_chunk_index = 0;
		Ok(())
	}

	fn open_search_result(&mut self, connection: &Connection) -> Result<()> {
		let chunk_info = self.get_selected_search_chunk();
		if let Some((document_id, entry_position, chunk_index)) = chunk_info {
			self.current_document = storage::get_document(connection, document_id)?;
			self.scroll_offset = 0;
			self.total_chunks = self.current_document.as_ref()
				.map(|d| d.entries.iter().map(|e| e.chunks.len().max(1)).sum())
				.unwrap_or(0);

			self.current_chunk_index = self.current_document.as_ref()
				.map(|doc| {
					let mut flat_index = 0usize;
					for entry in &doc.entries {
						if entry.position < entry_position {
							flat_index += entry.chunks.len().max(1);
						} else if entry.position == entry_position {
							flat_index += chunk_index as usize;
							break;
						}
					}
					flat_index
				})
				.unwrap_or(0);

			self.previous_mode = Mode::Search;
			self.mode = Mode::Read;
		}
		Ok(())
	}

	fn get_selected_search_chunk(&self) -> Option<(i64, u32, u32)> {
		let mut chunk_counter = 0usize;
		for result in &self.search_results {
			for chunk in &result.chunks {
				if chunk_counter == self.search_chunk_index {
					return Some((result.document_id, chunk.entry_position, chunk.chunk_index));
				}
				chunk_counter += 1;
			}
		}
		None
	}

	fn toggle_search_sort(&mut self, connection: &Connection) -> Result<()> {
		self.search_sort_column = match self.search_sort_column {
			SearchSortColumn::Score => SearchSortColumn::Date,
			SearchSortColumn::Date => SearchSortColumn::Score,
		};
		self.run_search(connection)
	}

	fn yank_current_chunk(&mut self) {
		if let Some(ref doc) = self.current_document {
			let mut chunk_counter = 0usize;
			let mut chunk_text: Option<String> = None;

			'outer: for entry in &doc.entries {
				if entry.chunks.is_empty() {
					if chunk_counter == self.current_chunk_index {
						chunk_text = Some(entry.body.clone());
						break 'outer;
					}
					chunk_counter += 1;
				} else {
					for chunk in &entry.chunks {
						if chunk_counter == self.current_chunk_index {
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
						self.status_message = Some("Copied chunk to clipboard".to_string());
					} else {
						self.status_message = Some("Failed to copy".to_string());
					}
				}
			}
		}
	}

	fn cycle_sort(&mut self) {
		self.sort_column = match self.sort_column {
			SortColumn::Title => SortColumn::Source,
			SortColumn::Source => SortColumn::Doctype,
			SortColumn::Doctype => SortColumn::Date,
			SortColumn::Date => SortColumn::Title,
		};
	}

	fn toggle_sort_direction(&mut self) {
		self.sort_direction = match self.sort_direction {
			SortDirection::Ascending => SortDirection::Descending,
			SortDirection::Descending => SortDirection::Ascending,
		};
	}
}

pub fn run(connection: &Connection) -> Result<()> {
	enable_raw_mode()?;
	let mut stdout = io::stdout();
	execute!(stdout, EnterAlternateScreen)?;
	let backend = CrosstermBackend::new(stdout);
	let mut terminal = Terminal::new(backend)?;
	terminal.clear()?;

	let mut app = App::new();
	app.load_documents(connection)?;

	let result = run_app(&mut terminal, &mut app, connection);

	disable_raw_mode()?;
	execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
	terminal.show_cursor()?;

	result
}

fn run_app(
	terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
	app: &mut App,
	connection: &Connection,
) -> Result<()> {
	loop {
		terminal.draw(|frame| draw(frame, app))?;

		if let Event::Key(key) = event::read()? {
			if key.kind != KeyEventKind::Press {
				continue;
			}

			app.status_message = None;

			match app.mode {
				Mode::Browse => match key.code {
					KeyCode::Char('q') => app.should_quit = true,
					KeyCode::Up => {
						if let Some(selected) = app.browse_state.selected() {
							if selected > 0 {
								app.browse_state.select(Some(selected - 1));
							}
						}
					}
					KeyCode::Down => {
						if let Some(selected) = app.browse_state.selected() {
							if selected < app.documents.len().saturating_sub(1) {
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
					_ => {}
				},
				Mode::Read => match key.code {
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
						app.yank_current_chunk();
					}
					KeyCode::Char('/') => {
						app.mode = Mode::Search;
					}
					_ => {}
				},
				Mode::Search => match key.code {
					KeyCode::Esc => {
						app.mode = Mode::Browse;
					}
					KeyCode::Enter => {
						if app.total_search_chunks > 0 {
							app.open_search_result(connection)?;
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
						app.toggle_search_sort(connection)?;
					}
					KeyCode::Char(c) => {
						app.search_query.push(c);
						app.run_search(connection)?;
					}
					KeyCode::Backspace => {
						app.search_query.pop();
						app.run_search(connection)?;
					}
					_ => {}
				},
			}
		}

		if app.should_quit {
			return Ok(());
		}
	}
}

fn draw(frame: &mut Frame, app: &App) {
	frame.render_widget(Clear, frame.area());

	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Min(3),
			Constraint::Length(1),
		])
		.split(frame.area());

	match app.mode {
		Mode::Browse => draw_browse(frame, app, chunks[0]),
		Mode::Read => draw_read(frame, app, chunks[0]),
		Mode::Search => draw_search(frame, app, chunks[0]),
	}

	draw_status_bar(frame, app, chunks[1]);
}

fn draw_browse(frame: &mut Frame, app: &App, area: Rect) {
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

	let header = Line::from(vec![
		Span::raw("  "),
		Span::styled(
			format!("{:<48}", format!("Title{}", sort_indicator(SortColumn::Title))),
			Style::default().add_modifier(Modifier::BOLD),
		),
		Span::raw("│ "),
		Span::styled(
			format!("{:<40}", format!("Source{}", sort_indicator(SortColumn::Source))),
			Style::default().add_modifier(Modifier::BOLD),
		),
		Span::raw("│ "),
		Span::styled(
			format!("{:<12}", format!("Type{}", sort_indicator(SortColumn::Doctype))),
			Style::default().add_modifier(Modifier::BOLD),
		),
		Span::raw("│ "),
		Span::styled(
			format!("Date{}", sort_indicator(SortColumn::Date)),
			Style::default().add_modifier(Modifier::BOLD),
		),
	]);

	let header_para = Paragraph::new(header)
		.block(Block::default()
			.title(format!(" BROWSE ({} documents) ", app.documents.len()))
			.borders(Borders::TOP | Borders::LEFT | Borders::RIGHT));
	frame.render_widget(header_para, chunks[0]);

	let items: Vec<ListItem> = app
		.documents
		.iter()
		.map(|doc| {
			let title = doc.title.as_deref().unwrap_or("-");
			let source = truncate_str(&doc.source_title, 38);
			let doctype = doc.doctype_name.as_deref().unwrap_or("-");
			let date = &doc.clip_date[..10.min(doc.clip_date.len())];
			ListItem::new(Line::from(vec![
				Span::raw(format!("{:<48}", truncate_str(title, 46))),
				Span::raw("│ "),
				Span::raw(format!("{:<40}", source)),
				Span::raw("│ "),
				Span::raw(format!("{:<12}", doctype)),
				Span::raw("│ "),
				Span::raw(date.to_string()),
			]))
		})
		.collect();

	let list = List::new(items)
		.block(Block::default().borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT))
		.highlight_style(
			Style::default()
				.bg(Color::DarkGray)
				.add_modifier(Modifier::BOLD),
		)
		.highlight_symbol("> ");

	frame.render_stateful_widget(list, chunks[1], &mut app.browse_state.clone());
}

fn draw_read(frame: &mut Frame, app: &App, area: Rect) {
	let Some(ref doc) = app.current_document else {
		return;
	};

	let title = doc.title.as_deref().unwrap_or(&doc.source_title);

	let mut all_lines: Vec<(Line, bool)> = Vec::new();
	let mut chunk_counter = 0usize;
	let mut in_code_block = false;

	for entry in &doc.entries {
		if let Some(ref heading) = entry.heading_title {
			let level = entry.heading_level.unwrap_or(1);
			let prefix = "#".repeat(level as usize);
			let heading_line = Line::from(Span::styled(
				expand_tabs(&format!("{} {}", prefix, heading)),
				Style::default().add_modifier(Modifier::BOLD),
			));
			all_lines.push((heading_line, false));
			all_lines.push((Line::from(""), false));
		}

		if let Some(ref author) = entry.author {
			all_lines.push((Line::from(format!("[{}]", author)), false));
		}

		if entry.chunks.is_empty() {
			let is_current = chunk_counter == app.current_chunk_index;
			let aligned_body = align_markdown_tables(&entry.body);
			for body_line in aligned_body.lines() {
				let (line, new_in_code_block) = render_markdown_line(body_line, in_code_block);
				in_code_block = new_in_code_block;
				all_lines.push((line, is_current));
			}
			chunk_counter += 1;
		} else {
			for chunk in &entry.chunks {
				let is_current = chunk_counter == app.current_chunk_index;
				let aligned_body = align_markdown_tables(&chunk.body);
				for body_line in aligned_body.lines() {
					let (line, new_in_code_block) = render_markdown_line(body_line, in_code_block);
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
						span.style.bg(Color::DarkGray),
					)
				}).collect();
				Line::from(spans)
			} else {
				line
			}
		})
		.collect();

	let paragraph = Paragraph::new(Text::from(visible_lines))
		.block(
			Block::default()
				.title(format!(
					" {} (chunk {}/{}) ",
					truncate_str(title, 50),
					app.current_chunk_index + 1,
					app.total_chunks.max(1),
				))
				.borders(Borders::ALL),
		)
		.wrap(Wrap { trim: false });

	frame.render_widget(paragraph, area);
}

fn draw_search(frame: &mut Frame, app: &App, area: Rect) {
	let layout = Layout::default()
		.direction(Direction::Vertical)
		.constraints([Constraint::Length(3), Constraint::Min(1)])
		.split(area);

	let sort_indicator = match app.search_sort_column {
		SearchSortColumn::Score => "sort: Score ▼",
		SearchSortColumn::Date => "sort: Date ▼",
	};

	let input = Paragraph::new(app.search_query.as_str())
		.block(
			Block::default()
				.title(format!(" Search ({}) [Tab] {} ", app.total_search_chunks, sort_indicator))
				.borders(Borders::ALL),
		);
	frame.render_widget(input, layout[0]);

	let mut lines: Vec<Line> = Vec::new();
	let mut chunk_counter = 0usize;

	for result in &app.search_results {
		let score_str = format!("{:.2}", -result.best_rank);
		let date_str = &result.clip_date[..10.min(result.clip_date.len())];
		lines.push(Line::from(vec![
			Span::styled(
				format!("▸ {} ", truncate_str(&result.source_title, 50)),
				Style::default().add_modifier(Modifier::BOLD),
			),
			Span::raw(format!("  score: {}  date: {}  ", score_str, date_str)),
			Span::styled(
				format!("[{} chunks]", result.chunks.len()),
				Style::default().fg(Color::DarkGray),
			),
		]));

		for chunk in &result.chunks {
			let is_current = chunk_counter == app.search_chunk_index;
			let base_style = if is_current {
				Style::default().bg(Color::DarkGray)
			} else {
				Style::default()
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
		.block(Block::default().borders(Borders::ALL));

	frame.render_widget(paragraph, layout[1]);
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
	let help_text = match app.mode {
		Mode::Browse => "[↑↓] move  [Enter] open  [/] search  [s] sort  [S] direction  [q] quit",
		Mode::Read => "[↑↓] chunk  [PgUp/PgDn] jump  [y] yank  [b] back  [/] search  [q] quit",
		Mode::Search => "[↑↓] chunk  [Enter] open  [Tab] sort  [Esc] back",
	};

	let status = if let Some(ref msg) = app.status_message {
		Line::from(vec![
			Span::styled(msg, Style::default().fg(Color::Green)),
			Span::raw("  │  "),
			Span::raw(help_text),
		])
	} else {
		Line::from(help_text)
	};

	let paragraph = Paragraph::new(status)
		.style(Style::default().bg(Color::DarkGray));
	frame.render_widget(paragraph, area);
}

fn truncate_str(s: &str, max_len: usize) -> &str {
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

fn truncate_string(s: String, max_len: usize) -> String {
	if s.len() <= max_len {
		s
	} else {
		let mut end = max_len;
		while end > 0 && !s.is_char_boundary(end) {
			end -= 1;
		}
		s[..end].to_string()
	}
}

fn expand_tabs(s: &str) -> String {
	s.replace('\t', "    ")
}

fn align_markdown_tables(text: &str) -> String {
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

fn render_markdown_line(line: &str, in_code_block: bool) -> (Line<'static>, bool) {
	let expanded = expand_tabs(line);

	if expanded.trim().starts_with("```") {
		let style = Style::default().fg(Color::DarkGray);
		return (Line::from(Span::styled(expanded, style)), !in_code_block);
	}

	if in_code_block {
		let style = Style::default().fg(Color::DarkGray);
		return (Line::from(Span::styled(expanded, style)), true);
	}

	if expanded.trim().starts_with("    ") || expanded.trim().starts_with("\t") {
		let style = Style::default().fg(Color::DarkGray);
		return (Line::from(Span::styled(expanded, style)), false);
	}

	let spans = parse_inline_markdown(&expanded);
	(Line::from(spans), false)
}

fn parse_inline_markdown(text: &str) -> Vec<Span<'static>> {
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
				spans.push(Span::styled(code_text, Style::default().fg(Color::DarkGray)));
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

fn parse_snippet<'a>(snippet: &str, base_style: Style) -> Vec<Span<'a>> {
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
