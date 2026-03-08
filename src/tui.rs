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

use crate::config::TagConfig;
use crate::ingest::OllamaClient;
use crate::storage::{
	self, DocumentContent, DocumentSummary,
	GroupedSearchResult, SearchSortColumn, SortColumn, SortDirection,
};
use crate::util::truncate_str;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
	Browse,
	Read,
	Search,
	TagEdit,
	TagFilter,
	SummaryView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchMode {
	Fts5,
	Semantic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchField {
	Query,
	Author,
	DateFrom,
	DateTo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SummaryType {
	Brief,
	Detailed,
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
	search_mode: SearchMode,
	search_field: SearchField,
	author_filter: String,
	date_from: String,
	date_to: String,
	should_quit: bool,
	status_message: Option<String>,
	group_docs: Vec<i64>,
	group_index: usize,
	marked_docs: Vec<i64>,
	current_doc_tags: Vec<String>,
	tag_input: String,
	all_tags: Vec<(String, i64)>,
	tag_filter: Option<String>,
	tag_filter_index: usize,
	tag_config: TagConfig,
	global_include: Option<Vec<String>>,
	global_exclude: Vec<String>,
	summary_content: Option<String>,
	summary_type: SummaryType,
	summary_doc_id: Option<i64>,
	summary_scroll: usize,
	summary_return_mode: Mode,
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
			search_mode: SearchMode::Fts5,
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
			let filtered = self.filtered_documents();
			if let Some(doc) = filtered.get(selected) {
				self.open_document_by_id(connection, doc.id)?;
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
			.map(|d| d.entries.iter().map(|e| e.chunks.len().max(1)).sum())
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

	fn navigate_group(&mut self, connection: &Connection, delta: i32) -> Result<()> {
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
				.map(|d| d.entries.iter().map(|e| e.chunks.len().max(1)).sum())
				.unwrap_or(0);
		}
		Ok(())
	}

	fn toggle_mark(&mut self, doc_id: i64) {
		if let Some(pos) = self.marked_docs.iter().position(|&id| id == doc_id) {
			self.marked_docs.remove(pos);
		} else {
			self.marked_docs.push(doc_id);
		}
	}

	fn clear_marks(&mut self) {
		self.marked_docs.clear();
	}

	fn run_search(&mut self, connection: &Connection, search_config: &SearchConfig) -> Result<()> {
		if self.search_query.is_empty() {
			self.search_results.clear();
			self.total_search_chunks = 0;
			return Ok(());
		}

		let author_filter = if self.author_filter.is_empty() { None } else { Some(self.author_filter.as_str()) };
		let date_from = if self.date_from.is_empty() { None } else { Some(self.date_from.as_str()) };
		let date_to = if self.date_to.is_empty() { None } else { Some(self.date_to.as_str()) };

		let allowed_doc_ids: std::collections::HashSet<i64> = self.documents.iter()
			.filter(|d| self.passes_global_filter(d))
			.map(|d| d.id)
			.collect();

		match self.search_mode {
			SearchMode::Fts5 => {
				let all_results = storage::search_filtered(
					connection,
					&self.search_query,
					self.search_sort_column,
					author_filter,
					date_from,
					date_to,
				)?;

				self.search_results = all_results.into_iter()
					.filter(|r| allowed_doc_ids.contains(&r.document_id))
					.collect();

				self.total_search_chunks = self.search_results.iter().map(|r| r.chunks.len()).sum();
			}
			SearchMode::Semantic => {
				let embeddings_exist = storage::count_chunks_with_embeddings(connection)? > 0;
				if !embeddings_exist {
					self.status_message = Some("No embeddings - run 'cathedrals embed' first".to_string());
					return Ok(());
				}

				let client = OllamaClient::new(&search_config.ollama_url, "");
				let query_embedding = match client.embed(&self.search_query, &search_config.embed_model) {
					Ok(emb) => emb,
					Err(e) => {
						self.status_message = Some(format!("Embed error: {}", e));
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

				self.search_results = grouped;
				self.total_search_chunks = self.search_results.iter().map(|r| r.chunks.len()).sum();
			}
		}

		self.search_chunk_index = 0;
		Ok(())
	}

	fn open_search_result(&mut self, connection: &Connection) -> Result<()> {
		let chunk_info = self.get_selected_fts5_chunk();
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

	fn get_selected_fts5_chunk(&self) -> Option<(i64, u32, u32)> {
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

	fn yank_full_document(&mut self) {
		if let Some(ref doc) = self.current_document {
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
					self.status_message = Some("Copied full document to clipboard".to_string());
				} else {
					self.status_message = Some("Failed to copy".to_string());
				}
			}
		}
	}

	fn cycle_sort(&mut self) {
		self.sort_column = match self.sort_column {
			SortColumn::Source => SortColumn::Doctype,
			SortColumn::Doctype => SortColumn::Date,
			SortColumn::Date => SortColumn::Source,
		};
	}

	fn toggle_sort_direction(&mut self) {
		self.sort_direction = match self.sort_direction {
			SortDirection::Ascending => SortDirection::Descending,
			SortDirection::Descending => SortDirection::Ascending,
		};
	}

	fn load_all_tags(&mut self, connection: &Connection) -> Result<()> {
		self.all_tags = storage::list_all_tags(connection)?;
		Ok(())
	}

	fn add_current_tag(&mut self, connection: &Connection) -> Result<()> {
		let input = self.tag_input.trim().to_lowercase().replace(' ', "-");
		if input.is_empty() {
			return Ok(());
		}

		if let Some(ref doc) = self.current_document {
			if let Ok(idx) = input.parse::<usize>() {
				if idx > 0 && idx <= self.current_doc_tags.len() {
					let tag = self.current_doc_tags[idx - 1].clone();
					storage::remove_tag(connection, doc.id, &tag)?;
					self.current_doc_tags.remove(idx - 1);
					self.tag_input.clear();
					self.load_documents(connection)?;
					self.status_message = Some(format!("Removed tag: {}", tag));
					return Ok(());
				}
			}

			if !self.current_doc_tags.contains(&input) {
				storage::add_tag(connection, doc.id, &input)?;
				self.current_doc_tags.push(input.clone());
				self.current_doc_tags.sort();
				self.status_message = Some(format!("Added tag: {}", input));
			}
			self.tag_input.clear();
			self.load_documents(connection)?;
		}
		Ok(())
	}

	fn apply_tag_filter(&mut self) {
		if self.all_tags.is_empty() {
			self.tag_filter = None;
		} else if self.tag_filter_index < self.all_tags.len() {
			self.tag_filter = Some(self.all_tags[self.tag_filter_index].0.clone());
		}
	}

	fn clear_tag_filter(&mut self) {
		self.tag_filter = None;
		self.tag_filter_index = 0;
	}

	fn passes_global_filter(&self, doc: &DocumentSummary) -> bool {
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

	fn filtered_documents(&self) -> Vec<&DocumentSummary> {
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

pub struct SearchConfig {
	pub ollama_url: String,
	pub embed_model: String,
}

impl Default for SearchConfig {
	fn default() -> Self {
		SearchConfig {
			ollama_url: "http://localhost:11434".to_string(),
			embed_model: "nomic-embed-text".to_string(),
		}
	}
}

pub fn run(connection: &Connection, filter: GlobalFilter, search_config: SearchConfig) -> Result<()> {
	enable_raw_mode()?;
	let mut stdout = io::stdout();
	execute!(stdout, EnterAlternateScreen)?;
	let backend = CrosstermBackend::new(stdout);
	let mut terminal = Terminal::new(backend)?;
	terminal.clear()?;

	let mut app = App::new();
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
									app.status_message = Some("No summary - run 'cathedrals derive'".to_string());
								}
							}
						}
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
					KeyCode::Char('Y') => {
						app.yank_full_document();
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
								app.status_message = Some("No summary - run 'cathedrals derive'".to_string());
							}
						}
					}
					_ => {}
				},
				Mode::Search => {
					match key.code {
						KeyCode::F(2) => {
							app.search_mode = match app.search_mode {
								SearchMode::Fts5 => SearchMode::Semantic,
								SearchMode::Semantic => SearchMode::Fts5,
							};
							app.run_search(connection, search_config)?;
						}
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
							app.run_search(connection, search_config)?;
						}
						KeyCode::Backspace => {
							match app.search_field {
								SearchField::Query => { app.search_query.pop(); }
								SearchField::Author => { app.author_filter.pop(); }
								SearchField::DateFrom => { app.date_from.pop(); }
								SearchField::DateTo => { app.date_to.pop(); }
							}
							app.run_search(connection, search_config)?;
						}
						_ => {}
					}
				},
				Mode::TagEdit => match key.code {
					KeyCode::Esc => {
						app.mode = Mode::Read;
						app.tag_input.clear();
					}
					KeyCode::Enter => {
						app.add_current_tag(connection)?;
						app.load_all_tags(connection)?;
					}
					KeyCode::Backspace => {
						app.tag_input.pop();
					}
					KeyCode::Char(c) => {
						app.tag_input.push(c);
					}
					_ => {}
				},
				Mode::TagFilter => match key.code {
					KeyCode::Esc => {
						app.mode = Mode::Browse;
					}
					KeyCode::Enter => {
						app.apply_tag_filter();
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
				},
				Mode::SummaryView => match key.code {
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
		Mode::Browse | Mode::TagFilter => draw_browse(frame, app, chunks[0]),
		Mode::Read | Mode::TagEdit => draw_read(frame, app, chunks[0]),
		Mode::Search => draw_search(frame, app, chunks[0]),
		Mode::SummaryView => match app.summary_return_mode {
			Mode::Read => draw_read(frame, app, chunks[0]),
			Mode::Search => draw_search(frame, app, chunks[0]),
			_ => draw_browse(frame, app, chunks[0]),
		},
	}

	if app.mode == Mode::TagFilter {
		draw_tag_filter_popup(frame, app);
	}

	if app.mode == Mode::TagEdit {
		draw_tag_edit_popup(frame, app);
	}

	if app.mode == Mode::SummaryView {
		draw_summary_popup(frame, app);
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

	const max_dots: usize = 4;

	let header = Line::from(vec![
		Span::raw("    "),
		Span::raw(format!("{:<width$}", "", width = max_dots)),
		Span::styled(
			format!("{:<43}", format!("Source{}", sort_indicator(SortColumn::Source))),
			Style::default().add_modifier(Modifier::BOLD),
		),
		Span::raw("│ "),
		Span::styled(
			format!("{:<10}", format!("Type{}", sort_indicator(SortColumn::Doctype))),
			Style::default().add_modifier(Modifier::BOLD),
		),
		Span::raw("│ "),
		Span::styled(
			format!("{:<12}", format!("Date{}", sort_indicator(SortColumn::Date))),
			Style::default().add_modifier(Modifier::BOLD),
		),
		Span::raw("│ "),
		Span::styled(
			"Preview",
			Style::default().add_modifier(Modifier::BOLD),
		),
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
			.title(title)
			.borders(Borders::TOP | Borders::LEFT | Borders::RIGHT));
	frame.render_widget(header_para, chunks[0]);

	let items: Vec<ListItem> = filtered
		.iter()
		.map(|doc| {
			let mark_indicator = if app.marked_docs.contains(&doc.id) { "* " } else { "  " };
			let doctype = doc.doctype_name.as_deref().unwrap_or("-");
			let date = &doc.clip_date[..10.min(doc.clip_date.len())];

			let mark_style = if app.marked_docs.contains(&doc.id) {
				Style::default().fg(Color::Yellow)
			} else {
				Style::default()
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
			spans.push(Span::raw(format!("{:<43}", source)));
			spans.push(Span::raw("│ "));
			spans.push(Span::raw(format!("{:<10}", truncate_str(doctype, 8))));
			spans.push(Span::raw("│ "));
			spans.push(Span::raw(format!("{:<12}", date)));
			spans.push(Span::raw("│ "));
			spans.push(Span::styled(
				truncate_str(preview, 60).to_string(),
				Style::default().fg(Color::DarkGray),
			));

			ListItem::new(Line::from(spans))
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
				.title(format!(
					" {} | {} | chunk {}/{}{}{}",
					truncate_str(&display_title, 35),
					date_str,
					app.current_chunk_index + 1,
					app.total_chunks.max(1),
					tags_info,
					group_info,
				))
				.borders(Borders::ALL),
		)
		.wrap(Wrap { trim: false });

	frame.render_widget(paragraph, area);
}

fn draw_search(frame: &mut Frame, app: &App, area: Rect) {
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

	let query_style = if app.search_field == SearchField::Query {
		Style::default().fg(Color::Yellow)
	} else {
		Style::default()
	};
	let query_block = Block::default()
		.title(format!(" Search [{}] ({}) [F2: toggle mode] ", mode_str, app.total_search_chunks))
		.borders(Borders::ALL)
		.border_style(query_style);
	let query_input = Paragraph::new(app.search_query.as_str()).block(query_block);
	frame.render_widget(query_input, input_layout[0]);

	let author_style = if app.search_field == SearchField::Author {
		Style::default().fg(Color::Yellow)
	} else {
		Style::default().fg(Color::DarkGray)
	};
	let author_text = format!(" Author: {} ", app.author_filter);
	frame.render_widget(Paragraph::new(author_text).style(author_style), filter_layout[0]);

	let date_from_style = if app.search_field == SearchField::DateFrom {
		Style::default().fg(Color::Yellow)
	} else {
		Style::default().fg(Color::DarkGray)
	};
	let date_from_text = format!(" From: {} ", app.date_from);
	frame.render_widget(Paragraph::new(date_from_text).style(date_from_style), filter_layout[1]);

	let date_to_style = if app.search_field == SearchField::DateTo {
		Style::default().fg(Color::Yellow)
	} else {
		Style::default().fg(Color::DarkGray)
	};
	let date_to_text = format!(" To: {} ", app.date_to);
	frame.render_widget(Paragraph::new(date_to_text).style(date_to_style), filter_layout[2]);

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
	let read_help = if app.group_docs.len() > 1 {
		"[↑↓/gG] chunk  [◀▶] group  [d] summary  [y/Y] yank  [t] tags  [b] back  [q] quit"
	} else {
		"[↑↓/gG] chunk  [PgUp/Dn] jump  [d] summary  [y/Y] yank  [t] tags  [b] back  [q] quit"
	};

	let help_text = match app.mode {
		Mode::Browse => "[↑↓/gG] move  [m] mark  [d] summary  [Enter] open  [/] search  [f] filter  [s] sort  [q] quit",
		Mode::Read => read_help,
		Mode::Search => "[↑↓] move  [Tab] field  [F2] mode  [Enter] open  [Esc] back",
		Mode::TagEdit => "[type] add tag  [Enter] save  [Esc] cancel",
		Mode::TagFilter => "[↑↓/gG] select  [Enter] apply  [c] clear  [Esc] cancel",
		Mode::SummaryView => "[↑↓] scroll  [d] toggle  [y] copy  [x] mark bad  [Esc] close",
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

fn draw_tag_edit_popup(frame: &mut Frame, app: &App) {
	let area = centered_rect(50, 12, frame.area());
	frame.render_widget(Clear, area);

	let mut lines: Vec<Line> = vec![
		Line::from("Current tags:"),
		Line::from(""),
	];

	if app.current_doc_tags.is_empty() {
		lines.push(Line::from(Span::styled("  (none)", Style::default().fg(Color::DarkGray))));
	} else {
		for (i, tag) in app.current_doc_tags.iter().enumerate() {
			lines.push(Line::from(format!("  {}. {}", i + 1, tag)));
		}
	}

	lines.push(Line::from(""));
	lines.push(Line::from("Type tag name + Enter to add"));
	lines.push(Line::from("Type number + Enter to remove"));
	lines.push(Line::from(format!("> {}_", app.tag_input)));

	let popup = Paragraph::new(Text::from(lines))
		.block(
			Block::default()
				.title(" Edit Tags ")
				.borders(Borders::ALL)
				.border_style(Style::default().fg(Color::Cyan)),
		);

	frame.render_widget(popup, area);
}

fn draw_tag_filter_popup(frame: &mut Frame, app: &App) {
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
		Line::from(format!(
			"Select tag ({}/{}):",
			app.tag_filter_index + 1,
			app.all_tags.len()
		)),
		Line::from(""),
	];

	for (i, (tag, count)) in app.all_tags.iter().enumerate().skip(scroll_offset).take(inner_height) {
		let style = if i == app.tag_filter_index {
			Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
		} else {
			Style::default()
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
				.title(" Filter by Tag ")
				.borders(Borders::ALL)
				.border_style(Style::default().fg(Color::Cyan)),
		);

	frame.render_widget(popup, area);
}

fn draw_summary_popup(frame: &mut Frame, app: &App) {
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
		.map(|line| Line::from(line))
		.collect();

	let popup = Paragraph::new(Text::from(lines))
		.block(
			Block::default()
				.title(format!(" {} Summary [d: toggle] [y: copy] [x: mark bad] ", type_str))
				.borders(Borders::ALL)
				.border_style(Style::default().fg(Color::Cyan)),
		)
		.wrap(Wrap { trim: false });

	frame.render_widget(popup, area);
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
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

fn expand_tabs(s: &str) -> String {
	s.replace('\t', "    ")
}

fn parse_color(name: &str) -> Option<Color> {
	match name.to_lowercase().as_str() {
		"red" => Some(Color::Red),
		"green" => Some(Color::Green),
		"blue" => Some(Color::Blue),
		"cyan" => Some(Color::Cyan),
		"magenta" => Some(Color::Magenta),
		"yellow" => Some(Color::Yellow),
		"white" => Some(Color::White),
		"gray" | "grey" => Some(Color::Gray),
		"light_red" => Some(Color::LightRed),
		"light_green" => Some(Color::LightGreen),
		"light_blue" => Some(Color::LightBlue),
		"light_cyan" => Some(Color::LightCyan),
		"light_magenta" => Some(Color::LightMagenta),
		"light_yellow" => Some(Color::LightYellow),
		_ => None,
	}
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

fn extract_group_key(source_title: &str) -> Option<String> {
	let key = crate::util::strip_source_suffix(source_title);

	if key.is_empty() || key.len() < 3 {
		return None;
	}

	let lower = key.to_lowercase();
	let generic = ["untitled", "new tab", "bash", "zsh", "terminal", "new document"];
	if generic.iter().any(|pattern| lower.contains(pattern)) {
		return None;
	}

	Some(key)
}
