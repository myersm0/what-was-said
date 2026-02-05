use anyhow::Result;
use chrono::NaiveDateTime;
use std::path::Path;

use crate::types::{MediaId, MediaItem, MediaType};

#[derive(Debug, Clone)]
pub struct WhisperSegment {
	pub start_seconds: f64,
	pub end_seconds: f64,
	pub text: String,
}

pub fn parse_whisper_json(json_text: &str) -> Result<Vec<WhisperSegment>> {
	let parsed: serde_json::Value = serde_json::from_str(json_text)?;
	let segments = parsed["segments"]
		.as_array()
		.ok_or_else(|| anyhow::anyhow!("missing segments array"))?;

	let result = segments
		.iter()
		.filter_map(|segment| {
			Some(WhisperSegment {
				start_seconds: segment["start"].as_f64()?,
				end_seconds: segment["end"].as_f64()?,
				text: segment["text"].as_str()?.trim().to_string(),
			})
		})
		.filter(|segment| !segment.text.is_empty())
		.collect();

	Ok(result)
}

pub fn segments_to_media_items(
	segments: &[WhisperSegment],
	recording_start: NaiveDateTime,
) -> Vec<MediaItem> {
	segments
		.iter()
		.enumerate()
		.map(|(index, segment)| {
			let offset = chrono::Duration::milliseconds((segment.start_seconds * 1000.0) as i64);
			MediaItem {
				id: MediaId(index as i64),
				file_path: std::path::PathBuf::new(),
				media_type: MediaType::TranscriptSegment,
				timestamp: recording_start + offset,
				duration: Some(segment.end_seconds - segment.start_seconds),
				document_id: None,
			}
		})
		.collect()
}
