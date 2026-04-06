mod browse;
mod read;
mod render;
mod search;
mod summary;
mod tags;
pub mod theme;

use anyhow::Result;
use crossterm::{
	event::{self, Event, KeyEventKind},
	execute,
	terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
	backend::CrosstermBackend,
	layout::{Constraint, Direction, Layout},
	style::Style,
	widgets::{Block, Clear},
	widgets::ListState,
	Frame, Terminal,
};
use rusqlite::Connection;
use std::io;

use crate::config::TagConfig;
use crate::storage::{
	self, DocumentContent, DocumentSummary,
	GroupedSearchResult, SearchSortColumn, SortColumn, SortDirection,
};

use render::extract_group_key;

pub(super) fn entry_chunk_count(entry: &storage::EntryContent) -> usize {
	if entry.chunks.is_empty() {
		if entry.body.trim().is_empty() { 0 } else { 1 }
	} else {
		entry.chunks.len()
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
	Browse,
	Read,
	Search,
	TagEdit,
	TagFilter,
	SummaryView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchMode {
	Fts5,
	Semantic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchField {
	Query,
	Author,
	DateFrom,
	DateTo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SummaryType {
	Brief,
	Detailed,
}

pub(super) struct App {
	pub(super) mode: Mode,
	pub(super) previous_mode: Mode,
	pub(super) documents: Vec<DocumentSummary>,
	pub(super) browse_state: ListState,
	pub(super) sort_column: SortColumn,
	pub(super) sort_direction: SortDirection,
	pub(super) current_document: Option<DocumentContent>,
	pub(super) scroll_offset: usize,
	pub(super) current_chunk_index: usize,
	pub(super) total_chunks: usize,
	pub(super) search_query: String,
	pub(super) search_results: Vec<GroupedSearchResult>,
	pub(super) search_sort_column: SearchSortColumn,
	pub(super) search_chunk_index: usize,
	pub(super) total_search_chunks: usize,
	pub(super) search_mode: SearchMode,
	pub(super) search_field: SearchField,
	pub(super) author_filter: String,
	pub(super) date_from: String,
	pub(super) date_to: String,
	pub(super) should_quit: bool,
	pub(super) status_message: Option<String>,
	pub(super) group_docs: Vec<i64>,
	pub(super) group_index: usize,
	pub(super) marked_docs: Vec<i64>,
	pub(super) current_doc_tags: Vec<String>,
	pub(super) tag_input: String,
	pub(super) all_tags: Vec<(String, i64)>,
	pub(super) tag_filter: Option<String>,
	pub(super) tag_filter_index: usize,
	pub(super) tag_config: TagConfig,
	pub(super) global_include: Option<Vec<String>>,
	pub(super) global_exclude: Vec<String>,
	pub(super) summary_content: Option<String>,
	pub(super) summary_type: SummaryType,
	pub(super) summary_doc_id: Option<i64>,
	pub(super) summary_scroll: usize,
	pub(super) summary_return_mode: Mode,
	pub(super) theme: theme::Theme,
}

impl App {
	fn new(theme: theme::Theme) -> Self {
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
			search_mode: SearchMode::Semantic,
			search_field: SearchField::Query,
			author_filter: String::new(),
			date_from: String::new(),
			date_to: String::new(),
			should_quit: false,
			status_message: None,
			group_docs: Vec::new(),
			group_index: 0,
			marked_docs: Vec::new(),
			current_doc_tags: Vec::new(),
			tag_input: String::new(),
			all_tags: Vec::new(),
			tag_filter: None,
			tag_filter_index: 0,
			tag_config: TagConfig::default(),
			global_include: None,
			global_exclude: Vec::new(),
			summary_content: None,
			summary_type: SummaryType::Brief,
			summary_doc_id: None,
			summary_scroll: 0,
			summary_return_mode: Mode::Browse,
			theme,
		}
	}

	pub(super) fn load_documents(&mut self, connection: &Connection) -> Result<()> {
		self.documents = storage::list_documents(connection, self.sort_column, self.sort_direction)?;
		if self.documents.is_empty() {
			self.browse_state.select(None);
		} else if self.browse_state.selected().is_none() {
			self.browse_state.select(Some(0));
		}
		Ok(())
	}

	pub(super) fn open_selected_document(&mut self, connection: &Connection) -> Result<()> {
		if let Some(selected) = self.browse_state.selected() {
			let filtered = self.filtered_documents();
			if let Some(doc) = filtered.get(selected) {
				let doc_id = doc.id;
				self.open_document_by_id(connection, doc_id)?;
			}
		}
		Ok(())
	}

	fn open_document_by_id(&mut self, connection: &Connection, doc_id: i64) -> Result<()> {
		self.current_document = storage::get_document(connection, doc_id)?;
		self.current_doc_tags = storage::get_tags_for_document(connection, doc_id)?;
		self.scroll_offset = 0;
		self.current_chunk_index = 0;
		self.total_chunks = self.current_document.as_ref()
			.map(|d| d.entries.iter().map(|e| entry_chunk_count(e)).sum())
			.unwrap_or(0);

		if self.marked_docs.len() > 1 && self.marked_docs.contains(&doc_id) {
			self.group_docs = self.marked_docs.clone();
			self.group_index = self.group_docs.iter()
				.position(|&id| id == doc_id)
				.unwrap_or(0);
		} else if let Some(ref doc) = self.current_document {
			let group_key = extract_group_key(&doc.source_title);
			if let Some(ref key) = group_key {
				self.group_docs = self.documents.iter()
					.filter(|d| extract_group_key(&d.source_title).as_ref() == Some(key))
					.map(|d| d.id)
					.collect();
				self.group_index = self.group_docs.iter()
					.position(|&id| id == doc_id)
					.unwrap_or(0);
			} else {
				self.group_docs.clear();
				self.group_index = 0;
			}
		}

		self.previous_mode = Mode::Browse;
		self.mode = Mode::Read;
		Ok(())
	}

	pub(super) fn navigate_group(&mut self, connection: &Connection, delta: i32) -> Result<()> {
		if self.group_docs.len() <= 1 {
			return Ok(());
		}
		let new_index = if delta > 0 {
			(self.group_index + 1) % self.group_docs.len()
		} else {
			(self.group_index + self.group_docs.len() - 1) % self.group_docs.len()
		};
		if let Some(&doc_id) = self.group_docs.get(new_index) {
			self.group_index = new_index;
			self.current_document = storage::get_document(connection, doc_id)?;
			self.current_doc_tags = storage::get_tags_for_document(connection, doc_id)?;
			self.scroll_offset = 0;
			self.current_chunk_index = 0;
			self.total_chunks = self.current_document.as_ref()
				.map(|d| d.entries.iter().map(|e| entry_chunk_count(e)).sum())
				.unwrap_or(0);
		}
		Ok(())
	}

	pub(super) fn toggle_mark(&mut self, doc_id: i64) {
		if let Some(pos) = self.marked_docs.iter().position(|&id| id == doc_id) {
			self.marked_docs.remove(pos);
		} else {
			self.marked_docs.push(doc_id);
		}
	}

	pub(super) fn clear_marks(&mut self) {
		self.marked_docs.clear();
	}

	pub(super) fn cycle_sort(&mut self) {
		self.sort_column = match self.sort_column {
			SortColumn::Source => SortColumn::Doctype,
			SortColumn::Doctype => SortColumn::Date,
			SortColumn::Date => SortColumn::Source,
		};
	}

	pub(super) fn toggle_sort_direction(&mut self) {
		self.sort_direction = match self.sort_direction {
			SortDirection::Ascending => SortDirection::Descending,
			SortDirection::Descending => SortDirection::Ascending,
		};
	}

	pub(super) fn load_all_tags(&mut self, connection: &Connection) -> Result<()> {
		self.all_tags = storage::list_all_tags(connection)?;
		Ok(())
	}

	pub(super) fn clear_tag_filter(&mut self) {
		self.tag_filter = None;
		self.tag_filter_index = 0;
	}

	pub(super) fn passes_global_filter(&self, doc: &DocumentSummary) -> bool {
		if let Some(ref includes) = self.global_include {
			if !includes.iter().any(|t| self.tag_config.doc_matches_filter(&doc.tags, t)) {
				return false;
			}
		}
		for exclude in &self.global_exclude {
			if self.tag_config.doc_matches_filter(&doc.tags, exclude) {
				return false;
			}
		}
		true
	}

	pub(super) fn filtered_documents(&self) -> Vec<&DocumentSummary> {
		self.documents.iter()
			.filter(|d| self.passes_global_filter(d))
			.filter(|d| {
				match &self.tag_filter {
					Some(tag) => self.tag_config.doc_matches_filter(&d.tags, tag),
					None => true,
				}
			})
			.collect()
	}
}

#[derive(Debug, Clone, Default)]
pub struct GlobalFilter {
	pub include: Option<Vec<String>>,
	pub exclude: Vec<String>,
	pub include_all: bool,
}

pub struct SearchConfig<'a> {
	pub embed_model: String,
	pub backend: &'a dyn crate::llm::LlmBackend,
}

impl Default for SearchConfig<'_> {
	fn default() -> Self {
		panic!("SearchConfig requires an explicit backend")
	}
}

pub fn run(
	connection: &Connection,
	filter: GlobalFilter,
	search_config: SearchConfig,
	theme_name: Option<&str>,
) -> Result<()> {
	enable_raw_mode()?;
	let mut stdout = io::stdout();
	execute!(stdout, EnterAlternateScreen)?;
	let backend = CrosstermBackend::new(stdout);
	let mut terminal = Terminal::new(backend)?;
	terminal.clear()?;

	let mut app = App::new(theme::load_theme(theme_name));
	app.tag_config = crate::config::load_tag_config(None);

	app.global_include = filter.include;
	app.global_exclude = if filter.include_all {
		Vec::new()
	} else if filter.exclude.is_empty() {
		app.tag_config.default_exclude.clone()
	} else {
		filter.exclude
	};

	app.load_documents(connection)?;
	app.load_all_tags(connection)?;

	let result = run_app(&mut terminal, &mut app, connection, &search_config);

	disable_raw_mode()?;
	execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
	terminal.show_cursor()?;

	result
}

fn run_app(
	terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
	app: &mut App,
	connection: &Connection,
	search_config: &SearchConfig,
) -> Result<()> {
	loop {
		terminal.draw(|frame| draw(frame, app))?;

		if let Event::Key(key) = event::read()? {
			if key.kind != KeyEventKind::Press {
				continue;
			}

			app.status_message = None;

			match app.mode {
				Mode::Browse => browse::handle_keys(app, key.code, connection)?,
				Mode::Read => read::handle_keys(app, key.code, connection)?,
				Mode::Search => search::handle_keys(app, key.code, connection, search_config)?,
				Mode::TagEdit => tags::handle_edit_keys(app, key.code, connection)?,
				Mode::TagFilter => tags::handle_filter_keys(app, key.code)?,
				Mode::SummaryView => summary::handle_keys(app, key.code, connection)?,
			}
		}

		if app.should_quit {
			return Ok(());
		}
	}
}

fn draw(frame: &mut Frame, app: &App) {
	frame.render_widget(Clear, frame.area());
	frame.render_widget(
		Block::default().style(Style::default().bg(app.theme.background).fg(app.theme.text)),
		frame.area(),
	);

	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Min(3),
			Constraint::Length(1),
		])
		.split(frame.area());

	match app.mode {
		Mode::Browse | Mode::TagFilter => browse::draw(frame, app, chunks[0]),
		Mode::Read | Mode::TagEdit => read::draw(frame, app, chunks[0]),
		Mode::Search => search::draw(frame, app, chunks[0]),
		Mode::SummaryView => match app.summary_return_mode {
			Mode::Read => read::draw(frame, app, chunks[0]),
			Mode::Search => search::draw(frame, app, chunks[0]),
			_ => browse::draw(frame, app, chunks[0]),
		},
	}

	if app.mode == Mode::TagFilter {
		tags::draw_filter_popup(frame, app);
	}

	if app.mode == Mode::TagEdit {
		tags::draw_edit_popup(frame, app);
	}

	if app.mode == Mode::SummaryView {
		summary::draw_popup(frame, app);
	}

	render::draw_status_bar(frame, app, chunks[1]);
}
