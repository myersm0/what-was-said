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
	widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
	Frame, Terminal,
};
use rusqlite::Connection;
use std::io;

use crate::storage::{
	self, DocumentContent, DocumentSummary, EntryContent,
	GroupedSearchResult, SortColumn, SortDirection,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
	Browse,
	Read,
	Search,
}

struct App {
	mode: Mode,
	documents: Vec<DocumentSummary>,
	browse_state: ListState,
	sort_column: SortColumn,
	sort_direction: SortDirection,
	current_document: Option<DocumentContent>,
	scroll_offset: usize,
	search_query: String,
	search_results: Vec<GroupedSearchResult>,
	search_state: ListState,
	should_quit: bool,
	status_message: Option<String>,
}

impl App {
	fn new() -> Self {
		let mut browse_state = ListState::default();
		browse_state.select(Some(0));
		let mut search_state = ListState::default();
		search_state.select(Some(0));
		App {
			mode: Mode::Browse,
			documents: Vec::new(),
			browse_state,
			sort_column: SortColumn::Date,
			sort_direction: SortDirection::Descending,
			current_document: None,
			scroll_offset: 0,
			search_query: String::new(),
			search_results: Vec::new(),
			search_state,
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
				self.mode = Mode::Read;
			}
		}
		Ok(())
	}

	fn run_search(&mut self, connection: &Connection) -> Result<()> {
		if self.search_query.is_empty() {
			self.search_results.clear();
			return Ok(());
		}
		self.search_results = storage::search(connection, &self.search_query)?;
		if self.search_results.is_empty() {
			self.search_state.select(None);
		} else {
			self.search_state.select(Some(0));
		}
		Ok(())
	}

	fn open_search_result(&mut self, connection: &Connection) -> Result<()> {
		if let Some(selected) = self.search_state.selected() {
			if let Some(result) = self.search_results.get(selected) {
				self.current_document = storage::get_document(connection, result.document_id)?;
				self.scroll_offset = 0;
				self.mode = Mode::Read;
			}
		}
		Ok(())
	}

	fn yank_current_chunk(&mut self) {
		if let Some(ref doc) = self.current_document {
			let all_text: String = doc.entries.iter()
				.map(|e| format_entry_for_display(e))
				.collect::<Vec<_>>()
				.join("\n\n");

			if let Ok(mut clipboard) = arboard::Clipboard::new() {
				if clipboard.set_text(&all_text).is_ok() {
					self.status_message = Some("Copied to clipboard".to_string());
				} else {
					self.status_message = Some("Failed to copy".to_string());
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

fn format_entry_for_display(entry: &EntryContent) -> String {
	let mut lines = Vec::new();
	if let Some(ref title) = entry.heading_title {
		let prefix = "#".repeat(entry.heading_level.unwrap_or(1) as usize);
		lines.push(format!("{} {}", prefix, title));
		lines.push(String::new());
	}
	if let Some(ref author) = entry.author {
		lines.push(format!("[{}]", author));
	}
	lines.push(entry.body.clone());
	lines.join("\n")
}

pub fn run(connection: &Connection) -> Result<()> {
	enable_raw_mode()?;
	let mut stdout = io::stdout();
	execute!(stdout, EnterAlternateScreen)?;
	let backend = CrosstermBackend::new(stdout);
	let mut terminal = Terminal::new(backend)?;

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
						app.mode = Mode::Browse;
						app.current_document = None;
					}
					KeyCode::Up => {
						if app.scroll_offset > 0 {
							app.scroll_offset -= 1;
						}
					}
					KeyCode::Down => {
						app.scroll_offset += 1;
					}
					KeyCode::PageUp => {
						app.scroll_offset = app.scroll_offset.saturating_sub(20);
					}
					KeyCode::PageDown => {
						app.scroll_offset += 20;
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
						if !app.search_results.is_empty() {
							app.open_search_result(connection)?;
						}
					}
					KeyCode::Up => {
						if let Some(selected) = app.search_state.selected() {
							if selected > 0 {
								app.search_state.select(Some(selected - 1));
							}
						}
					}
					KeyCode::Down => {
						if let Some(selected) = app.search_state.selected() {
							if selected < app.search_results.len().saturating_sub(1) {
								app.search_state.select(Some(selected + 1));
							}
						}
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
	let sort_indicator = |col: SortColumn| {
		if app.sort_column == col {
			match app.sort_direction {
				SortDirection::Ascending => " ▲",
				SortDirection::Descending => " ▼",
			}
		} else {
			""
		}
	};

	let _header = Line::from(vec![
		Span::styled(
			format!("Title{}", sort_indicator(SortColumn::Title)),
			Style::default().add_modifier(Modifier::BOLD),
		),
		Span::raw("  │  "),
		Span::styled(
			format!("Source{}", sort_indicator(SortColumn::Source)),
			Style::default().add_modifier(Modifier::BOLD),
		),
		Span::raw("  │  "),
		Span::styled(
			format!("Type{}", sort_indicator(SortColumn::Doctype)),
			Style::default().add_modifier(Modifier::BOLD),
		),
		Span::raw("  │  "),
		Span::styled(
			format!("Date{}", sort_indicator(SortColumn::Date)),
			Style::default().add_modifier(Modifier::BOLD),
		),
	]);

	let items: Vec<ListItem> = app
		.documents
		.iter()
		.map(|doc| {
			let title = doc.title.as_deref()
				.unwrap_or_else(|| truncate_str(&doc.source_title, 30));
			let source = truncate_str(&doc.source_title, 25);
			let doctype = doc.doctype_name.as_deref().unwrap_or("-");
			let date = &doc.clip_date[..10.min(doc.clip_date.len())];
			ListItem::new(Line::from(vec![
				Span::raw(format!("{:<32}", truncate_str(title, 30))),
				Span::raw("│ "),
				Span::styled(
					format!("{:<27}", source),
					Style::default().fg(Color::Cyan),
				),
				Span::raw("│ "),
				Span::styled(
					format!("{:<10}", doctype),
					Style::default().fg(Color::Yellow),
				),
				Span::raw("│ "),
				Span::styled(date.to_string(), Style::default().fg(Color::Green)),
			]))
		})
		.collect();

	let list = List::new(items)
		.block(
			Block::default()
				.title(Line::from(vec![
					Span::raw(" BROWSE "),
					Span::styled(
						format!("({} documents)", app.documents.len()),
						Style::default().fg(Color::DarkGray),
					),
				]))
				.borders(Borders::ALL),
		)
		.highlight_style(
			Style::default()
				.bg(Color::DarkGray)
				.add_modifier(Modifier::BOLD),
		)
		.highlight_symbol("> ");

	frame.render_stateful_widget(list, area, &mut app.browse_state.clone());
}

fn draw_read(frame: &mut Frame, app: &App, area: Rect) {
	let Some(ref doc) = app.current_document else {
		return;
	};

	let title = doc.title.as_deref().unwrap_or(&doc.source_title);

	let mut lines: Vec<Line> = Vec::new();
	for entry in &doc.entries {
		if let Some(ref heading) = entry.heading_title {
			let level = entry.heading_level.unwrap_or(1);
			let style = match level {
				1 => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
				2 => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
				_ => Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
			};
			let prefix = "#".repeat(level as usize);
			lines.push(Line::from(Span::styled(
				format!("{} {}", prefix, heading),
				style,
			)));
			lines.push(Line::from(""));
		}

		if let Some(ref author) = entry.author {
			lines.push(Line::from(Span::styled(
				format!("[{}]", author),
				Style::default().fg(Color::Magenta),
			)));
		}

		for body_line in entry.body.lines() {
			let styled = if body_line.starts_with("```") || body_line.starts_with("    ") {
				Line::from(Span::styled(body_line, Style::default().fg(Color::Green)))
			} else if body_line.starts_with("- ") || body_line.starts_with("* ") {
				Line::from(Span::styled(body_line, Style::default().fg(Color::White)))
			} else {
				Line::from(body_line)
			};
			lines.push(styled);
		}
		lines.push(Line::from(""));
	}

	let visible_lines: Vec<Line> = lines
		.into_iter()
		.skip(app.scroll_offset)
		.collect();

	let paragraph = Paragraph::new(Text::from(visible_lines))
		.block(
			Block::default()
				.title(format!(
					" {} ({} entries) ",
					truncate_str(title, 50),
					doc.entries.len()
				))
				.borders(Borders::ALL),
		)
		.wrap(Wrap { trim: false });

	frame.render_widget(paragraph, area);
}

fn draw_search(frame: &mut Frame, app: &App, area: Rect) {
	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([Constraint::Length(3), Constraint::Min(1)])
		.split(area);

	let input = Paragraph::new(app.search_query.as_str())
		.block(
			Block::default()
				.title(" Search ")
				.borders(Borders::ALL),
		);
	frame.render_widget(input, chunks[0]);

	let items: Vec<ListItem> = app
		.search_results
		.iter()
		.map(|result| {
			let mut lines = vec![Line::from(vec![
				Span::styled(
					truncate_str(&result.source_title, 50),
					Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
				),
				Span::raw("  "),
				Span::styled(
					format!("[{} matches]", result.chunks.len()),
					Style::default().fg(Color::Yellow),
				),
			])];
			for chunk in result.chunks.iter().take(2) {
				let preview = truncate_string(chunk.chunk_body.replace('\n', " "), 60);
				lines.push(Line::from(vec![
					Span::raw("  │ "),
					Span::styled(preview, Style::default().fg(Color::DarkGray)),
				]));
			}
			if result.chunks.len() > 2 {
				lines.push(Line::from(Span::styled(
					format!("  │ ... and {} more", result.chunks.len() - 2),
					Style::default().fg(Color::DarkGray),
				)));
			}
			ListItem::new(lines)
		})
		.collect();

	let results_title = if app.search_results.is_empty() && !app.search_query.is_empty() {
		" No results ".to_string()
	} else {
		format!(" Results ({}) ", app.search_results.len())
	};

	let list = List::new(items)
		.block(Block::default().title(results_title).borders(Borders::ALL))
		.highlight_style(
			Style::default()
				.bg(Color::DarkGray)
				.add_modifier(Modifier::BOLD),
		)
		.highlight_symbol("> ");

	frame.render_stateful_widget(list, chunks[1], &mut app.search_state.clone());
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
	let help_text = match app.mode {
		Mode::Browse => "[↑↓] move  [Enter] open  [/] search  [s] sort  [S] direction  [q] quit",
		Mode::Read => "[↑↓] scroll  [PgUp/PgDn] page  [y] yank  [b] back  [/] search  [q] quit",
		Mode::Search => "[↑↓] move  [Enter] open  [Esc] cancel",
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
