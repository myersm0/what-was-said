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
	text.split(|c: char| c.is_whitespace() || c == '\n')
		.map(|word| {
			word.chars()
				.filter(|c| c.is_alphanumeric())
				.collect::<String>()
				.to_lowercase()
		})
		.filter(|word| !word.is_empty())
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

pub fn jaccard(a: &MinHashSignature, b: &MinHashSignature) -> f64 {
	let matches = a.iter().zip(b.iter()).filter(|(x, y)| x == y).count();
	matches as f64 / MINHASH_SIZE as f64
}

pub fn is_short_entry(text: &str) -> bool {
	text.len() < 50
}
