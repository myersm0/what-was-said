use chrono::NaiveDateTime;
use std::path::PathBuf;

pub const MINHASH_SIZE: usize = 32;
pub type MinHashSignature = [u64; MINHASH_SIZE];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DocumentId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MediaId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStrategy {
	None,
	Positional,
	Timestamped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
	Screenshot,
	Audio,
	TranscriptSegment,
}

#[derive(Debug, Clone)]
pub struct MediaItem {
	pub id: MediaId,
	pub file_path: PathBuf,
	pub media_type: MediaType,
	pub timestamp: NaiveDateTime,
	pub duration: Option<f64>,
	pub document_id: Option<DocumentId>,
}

#[derive(Debug, Clone)]
pub struct SegmentedEntry {
	pub start_line: usize,
	pub end_line: usize,
	pub author: Option<String>,
	pub timestamp: Option<String>,
	pub body: String,
	pub is_quote: bool,
	pub heading_level: Option<u8>,
	pub heading_title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SegmentationResult {
	pub entries: Vec<SegmentedEntry>,
}
