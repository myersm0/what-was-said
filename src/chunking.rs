const target_words: usize = 300;
const min_split_threshold: usize = 400;
const stride_fraction: f64 = 1.0 / 3.0;
const max_snap_chars: usize = 500;

pub struct Chunk {
	pub chunk_index: u32,
	pub start_char: usize,
	pub end_char: usize,
	pub body: String,
}

pub fn chunk_text(text: &str) -> Vec<Chunk> {
	let text = text.trim();
	if text.is_empty() {
		return Vec::new();
	}

	let word_count = text.split_whitespace().count();
	if word_count <= min_split_threshold {
		return vec![Chunk {
			chunk_index: 0,
			start_char: 0,
			end_char: text.len(),
			body: text.to_string(),
		}];
	}

	let sentence_boundaries = find_sentence_boundaries(text);
	let word_positions = find_word_positions(text);

	if word_positions.is_empty() {
		return vec![Chunk {
			chunk_index: 0,
			start_char: 0,
			end_char: text.len(),
			body: text.to_string(),
		}];
	}

	let stride_words = ((target_words as f64) * stride_fraction).round() as usize;
	let stride_words = stride_words.max(1);

	let mut chunks = Vec::new();
	let mut focal_start_word = 0usize;

	while focal_start_word < word_positions.len() {
		let chunk_start_word = focal_start_word.saturating_sub(target_words / 3);
		let chunk_end_word = (focal_start_word + target_words).min(word_positions.len());

		let remaining_words = word_positions.len() - focal_start_word;
		if remaining_words <= min_split_threshold - target_words / 3 && !chunks.is_empty() {
			let last: &mut Chunk = chunks.last_mut().unwrap();
			last.end_char = text.len();
			last.body = text[last.start_char..last.end_char].to_string();
			break;
		}

		let raw_start_char = word_positions[chunk_start_word].0;
		let raw_end_char = if chunk_end_word >= word_positions.len() {
			text.len()
		} else {
			word_positions[chunk_end_word].0
		};

		let start_char = snap_to_sentence_boundary(&sentence_boundaries, raw_start_char, true, max_snap_chars);
		let end_char = snap_to_sentence_boundary(&sentence_boundaries, raw_end_char, false, max_snap_chars);

		let start_char = start_char.min(raw_start_char);
		let end_char = end_char.max(raw_end_char).min(text.len());

		let body = text[start_char..end_char].trim().to_string();

		if !body.is_empty() {
			chunks.push(Chunk {
				chunk_index: chunks.len() as u32,
				start_char,
				end_char,
				body,
			});
		}

		focal_start_word += stride_words;
	}

	if chunks.is_empty() {
		chunks.push(Chunk {
			chunk_index: 0,
			start_char: 0,
			end_char: text.len(),
			body: text.to_string(),
		});
	}

	chunks
}

fn find_word_positions(text: &str) -> Vec<(usize, usize)> {
	let mut positions = Vec::new();
	let mut in_word = false;
	let mut word_start = 0;

	for (index, character) in text.char_indices() {
		if character.is_whitespace() {
			if in_word {
				positions.push((word_start, index));
				in_word = false;
			}
		} else {
			if !in_word {
				word_start = index;
				in_word = true;
			}
		}
	}

	if in_word {
		positions.push((word_start, text.len()));
	}

	positions
}

fn find_sentence_boundaries(text: &str) -> Vec<usize> {
	let mut boundaries = vec![0];
	let chars: Vec<char> = text.chars().collect();

	for (index, &character) in chars.iter().enumerate() {
		if character == '.' || character == '!' || character == '?' {
			let next_is_space_or_end = chars.get(index + 1)
				.map(|c| c.is_whitespace())
				.unwrap_or(true);
			if next_is_space_or_end {
				let boundary = text.char_indices()
					.nth(index + 1)
					.map(|(i, _)| i)
					.unwrap_or(text.len());
				boundaries.push(boundary);
			}
		}
	}

	boundaries.push(text.len());
	boundaries.sort();
	boundaries.dedup();
	boundaries
}

fn snap_to_sentence_boundary(boundaries: &[usize], position: usize, prefer_earlier: bool, max_distance: usize) -> usize {
	if boundaries.is_empty() {
		return position;
	}

	let mut best = position;
	let mut best_distance = usize::MAX;

	for &boundary in boundaries {
		let distance = position.abs_diff(boundary);
		if distance > max_distance {
			continue;
		}
		let dominated = if prefer_earlier {
			distance < best_distance || (distance == best_distance && boundary < best)
		} else {
			distance < best_distance || (distance == best_distance && boundary > best)
		};
		if dominated {
			best = boundary;
			best_distance = distance;
		}
	}

	best
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn short_text_single_chunk() {
		let text = "This is a short piece of text that should not be split.";
		let chunks = chunk_text(text);
		assert_eq!(chunks.len(), 1);
		assert_eq!(chunks[0].body, text);
	}

	#[test]
	fn empty_text() {
		let chunks = chunk_text("");
		assert!(chunks.is_empty());
	}

	#[test]
	fn whitespace_only() {
		let chunks = chunk_text("   \n\t  ");
		assert!(chunks.is_empty());
	}

	#[test]
	fn long_text_multiple_chunks() {
		let sentences: Vec<String> = (0..50)
			.map(|i| format!("This is sentence number {} with some extra words to pad it out.", i))
			.collect();
		let text = sentences.join(" ");
		let chunks = chunk_text(&text);
		assert!(chunks.len() > 1);
		for chunk in &chunks {
			assert!(!chunk.body.is_empty());
		}
	}

	#[test]
	fn chunks_have_sequential_indices() {
		let text = "Word ".repeat(200);
		let chunks = chunk_text(&text);
		for (index, chunk) in chunks.iter().enumerate() {
			assert_eq!(chunk.chunk_index, index as u32);
		}
	}

	#[test]
	fn near_threshold_no_tiny_final_chunk() {
		let text = "Word ".repeat(145);
		let chunks = chunk_text(&text);
		if chunks.len() > 1 {
			let last_word_count = chunks.last().unwrap().body.split_whitespace().count();
			assert!(last_word_count > 20, "final chunk too small: {} words", last_word_count);
		}
	}
}
