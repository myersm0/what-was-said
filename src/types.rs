use chrono::NaiveDateTime;
use std::path::PathBuf;

pub const minhash_size: usize = 32;
pub type MinHashSignature = [u64; minhash_size];

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
pub enum Collection {
	Personal,
	Work,
}


#[derive(Debug, Clone)]
pub struct SourceInfo {
	pub title: String,
	pub clip_date: NaiveDateTime,
	pub file_path: PathBuf,
}


#[derive(Debug, Clone)]
pub struct Entry {
	pub id: EntryId,
	pub document_id: DocumentId,
	pub body: String,
	pub author: Option<String>,
	pub timestamp: Option<String>,
	pub position: u32,
	pub heading_level: Option<u8>,
	pub heading_title: Option<String>,
	pub source: SourceInfo,
	pub is_quote: bool,
	pub is_contaminated: bool,
	pub minhash: MinHashSignature,
}

#[derive(Debug, Clone)]
pub struct Document {
	pub id: DocumentId,
	pub collection: Collection,
	pub source_title: String,
	pub merge_strategy: MergeStrategy,
	pub origin_path: Option<PathBuf>,
	pub entries: Vec<Entry>,
}

impl Document {
	pub fn should_attempt_merge(&self) -> bool {
		!matches!(self.merge_strategy, MergeStrategy::None)
	}
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
pub enum TimelineEvent {
	Screenshot {
		media: MediaItem,
	},
	Subtitle {
		text: String,
		start: NaiveDateTime,
		end: NaiveDateTime,
	},
}

#[derive(Debug, Clone)]
pub struct Timeline {
	pub events: Vec<TimelineEvent>,
}

impl Timeline {
	pub fn from_time_window(
		screenshots: &[MediaItem],
		transcript_segments: &[MediaItem],
		entries: &[Entry],
	) -> Self {
		let mut events = Vec::new();

		for screenshot in screenshots {
			events.push(TimelineEvent::Screenshot {
				media: screenshot.clone(),
			});
		}

		for (segment, entry) in transcript_segments.iter().zip(entries.iter()) {
			events.push(TimelineEvent::Subtitle {
				text: entry.body.clone(),
				start: segment.timestamp,
				end: segment
					.duration
					.map(|duration| {
						segment.timestamp
							+ chrono::Duration::milliseconds((duration * 1000.0) as i64)
					})
					.unwrap_or(segment.timestamp),
			});
		}

		events.sort_by_key(|event| match event {
			TimelineEvent::Screenshot { media } => media.timestamp,
			TimelineEvent::Subtitle { start, .. } => *start,
		});

		Timeline { events }
	}
}


#[derive(Debug, Clone)]
pub struct SegmentedEntry {
	pub start_line: usize,
	pub end_line: usize,
	pub author: Option<String>,
	pub timestamp: Option<String>,
	pub body: String,
	pub is_quote: bool,
	pub is_contaminated: bool,
	pub heading_level: Option<u8>,
	pub heading_title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SegmentationResult {
	pub entries: Vec<SegmentedEntry>,
}


#[derive(Debug, Clone)]
pub struct CandidateMatch {
	pub entry_a: EntryId,
	pub entry_b: EntryId,
	pub similarity: f64,
	pub neighbor_corroborated: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum MergeDecision {
	KeepExisting,
	ReplaceWithNew,
	New,
}
