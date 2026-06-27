use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::types::{MINHASH_SIZE, MinHashSignature};

fn hash_with_seed(seed: usize, token: &str) -> u64 {
	let mut hasher = DefaultHasher::new();
	seed.hash(&mut hasher);
	token.hash(&mut hasher);
	hasher.finish()
}

fn tokenize(text: &str) -> Vec<String> {
	let words: Vec<String> = text
		.split(|c: char| c.is_whitespace() || c == '\n')
		.map(|word| {
			word.chars()
				.filter(|c| c.is_alphanumeric())
				.collect::<String>()
				.to_lowercase()
		})
		.filter(|word| !word.is_empty())
		.collect();
	if words.len() < 3 {
		return words;
	}
	words.windows(3)
		.map(|w| format!("{} {} {}", w[0], w[1], w[2]))
		.collect()
}

pub fn minhash(text: &str) -> MinHashSignature {
	let tokens = tokenize(text);
	let mut signature = [u64::MAX; MINHASH_SIZE];
	for token in &tokens {
		for i in 0..MINHASH_SIZE {
			let hash = hash_with_seed(i, token);
			if hash < signature[i] {
				signature[i] = hash;
			}
		}
	}
	signature
}

pub fn minhash_with_context(
	entry_text: &str,
	prev_text: Option<&str>,
	next_text: Option<&str>,
) -> MinHashSignature {
	let mut combined = String::new();
	if let Some(prev) = prev_text {
		combined.push_str(prev);
		combined.push('\n');
	}
	combined.push_str(entry_text);
	if let Some(next) = next_text {
		combined.push('\n');
		combined.push_str(next);
	}
	minhash(&combined)
}

pub fn longest_shared_block_words(a: &str, b: &str) -> usize {
	let shingles_a = tokenize(a);
	let shingles_b = tokenize(b);
	let run = longest_common_shingle_run(&shingles_a, &shingles_b);
	if run == 0 {
		0
	} else {
		run + 2
	}
}

fn longest_common_shingle_run(a: &[String], b: &[String]) -> usize {
	if a.is_empty() || b.is_empty() {
		return 0;
	}

	let mut ids: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
	for shingle in a.iter().chain(b.iter()) {
		let next = ids.len() as u32;
		ids.entry(shingle.as_str()).or_insert(next);
	}
	let encoded_a: Vec<u32> = a.iter().map(|s| ids[s.as_str()]).collect();
	let encoded_b: Vec<u32> = b.iter().map(|s| ids[s.as_str()]).collect();

	let mut previous = vec![0u32; encoded_b.len()];
	let mut current = vec![0u32; encoded_b.len()];
	let mut best = 0u32;
	for &token_a in &encoded_a {
		for (j, &token_b) in encoded_b.iter().enumerate() {
			current[j] = if token_a == token_b {
				if j == 0 {
					1
				} else {
					previous[j - 1] + 1
				}
			} else {
				0
			};
			if current[j] > best {
				best = current[j];
			}
		}
		std::mem::swap(&mut previous, &mut current);
	}
	best as usize
}

pub fn jaccard(a: &MinHashSignature, b: &MinHashSignature) -> f64 {
	let matches = a.iter().zip(b.iter()).filter(|(x, y)| x == y).count();
	matches as f64 / MINHASH_SIZE as f64
}

pub fn is_short_entry(text: &str) -> bool {
	text.len() < 50
}

pub fn minhash_document(entries: &[crate::types::SegmentedEntry]) -> MinHashSignature {
	let combined: String = entries.iter()
		.map(|e| e.body.as_str())
		.collect::<Vec<_>>()
		.join("\n");
	minhash(&combined)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn identical_texts_have_jaccard_one() {
		let a = minhash("the quick brown fox jumps over the lazy dog");
		let b = minhash("the quick brown fox jumps over the lazy dog");
		assert_eq!(jaccard(&a, &b), 1.0);
	}

	#[test]
	fn disjoint_texts_have_low_jaccard() {
		let a = minhash("the quick brown fox jumps over the lazy dog");
		let b = minhash("quantum mechanics describes subatomic particles");
		assert!(jaccard(&a, &b) < 0.3);
	}

	#[test]
	fn similar_texts_have_moderate_jaccard() {
		let a = minhash("the quick brown fox jumps over the lazy dog");
		let b = minhash("the quick brown fox leaps over the lazy dog");
		assert!(jaccard(&a, &b) > 0.2);
	}

	#[test]
	fn empty_text_produces_max_signature() {
		let sig = minhash("");
		assert!(sig.iter().all(|&v| v == u64::MAX));
	}

	#[test]
	fn short_entry_detection() {
		assert!(is_short_entry("hi"));
		assert!(!is_short_entry(&"word ".repeat(20)));
	}
}
