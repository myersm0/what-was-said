//! SipHash-1-3 with a zero key: what std's `DefaultHasher::new()` computes
//! today, pinned here because std may change that algorithm in any release
//! and these hashes are stored in the database (MinHash signatures, prompt
//! and source hashes). Callers feed bytes the way std's `Hash` impls did, so
//! existing stored values stay valid: a `str` is its bytes plus a 0xFF
//! terminator, a `usize` its 8 little-endian bytes.

const STR_TERMINATOR: u8 = 0xff;

#[inline]
fn sip_round(v: &mut [u64; 4]) {
	v[0] = v[0].wrapping_add(v[1]);
	v[1] = v[1].rotate_left(13);
	v[1] ^= v[0];
	v[0] = v[0].rotate_left(32);
	v[2] = v[2].wrapping_add(v[3]);
	v[3] = v[3].rotate_left(16);
	v[3] ^= v[2];
	v[0] = v[0].wrapping_add(v[3]);
	v[3] = v[3].rotate_left(21);
	v[3] ^= v[0];
	v[2] = v[2].wrapping_add(v[1]);
	v[1] = v[1].rotate_left(17);
	v[1] ^= v[2];
	v[2] = v[2].rotate_left(32);
}

/// SipHash-1-3 of `data` with keys (0, 0).
pub fn sip13(data: &[u8]) -> u64 {
	let mut v = [
		0x736f_6d65_7073_6575,
		0x646f_7261_6e64_6f6d,
		0x6c79_6765_6e65_7261,
		0x7465_6462_7974_6573,
	];
	let mut blocks = data.chunks_exact(8);
	for block in &mut blocks {
		let m = u64::from_le_bytes(block.try_into().expect("8-byte block"));
		v[3] ^= m;
		sip_round(&mut v);
		v[0] ^= m;
	}
	let mut tail = [0u8; 8];
	let remainder = blocks.remainder();
	tail[..remainder.len()].copy_from_slice(remainder);
	let b = ((data.len() as u64 & 0xff) << 56) | u64::from_le_bytes(tail);
	v[3] ^= b;
	sip_round(&mut v);
	v[0] ^= b;
	v[2] ^= 0xff;
	for _ in 0..3 {
		sip_round(&mut v);
	}
	v[0] ^ v[1] ^ v[2] ^ v[3]
}

/// Hash of a sequence of strings, as hashing each into one std hasher did.
pub fn hash_strs<'a>(items: impl IntoIterator<Item = &'a str>) -> u64 {
	let mut bytes = Vec::new();
	for item in items {
		bytes.extend_from_slice(item.as_bytes());
		bytes.push(STR_TERMINATOR);
	}
	sip13(&bytes)
}

pub fn hash_str(s: &str) -> u64 {
	hash_strs([s])
}

/// Hashes one token under many seeds, as `seed.hash(); token.hash()` did,
/// reusing one buffer and rewriting only the seed bytes.
pub struct SeededStrHasher {
	buffer: Vec<u8>,
}

impl SeededStrHasher {
	pub fn new(token: &str) -> Self {
		let mut buffer = Vec::with_capacity(8 + token.len() + 1);
		buffer.extend_from_slice(&[0u8; 8]);
		buffer.extend_from_slice(token.as_bytes());
		buffer.push(STR_TERMINATOR);
		Self { buffer }
	}

	pub fn hash(&mut self, seed: usize) -> u64 {
		self.buffer[..8].copy_from_slice(&(seed as u64).to_le_bytes());
		sip13(&self.buffer)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Computed independently with Python 3.12's built-in SipHash-1-3
	/// (`hash(bytes)` under `PYTHONHASHSEED=0`, which zeroes the key).
	#[test]
	fn matches_reference_values() {
		assert_eq!(sip13(b""), 0xd1fb_a762_150c_532c);
		assert_eq!(hash_str(""), 0x3040_6ea5_23c5_3def);
		assert_eq!(hash_str("abc"), 0xef09_e0f4_895a_251d);
		let mut seeded = SeededStrHasher::new("the quick brown");
		assert_eq!(seeded.hash(0), 0x9377_98b4_d928_b9f0);
		assert_eq!(seeded.hash(127), 0x4877_7744_a7b8_fc74);
	}

	/// Holds while std's DefaultHasher is still SipHash-1-3 with a zero key.
	/// If a future Rust release changes that, delete this test: the stored
	/// values are defined by this module, not by std.
	#[test]
	fn matches_todays_default_hasher() {
		use std::collections::hash_map::DefaultHasher;
		use std::hash::{Hash, Hasher};

		for text in ["", "abc", "seven c", "exactly8", "nine char", "a longer string spanning blocks"] {
			let mut hasher = DefaultHasher::new();
			text.hash(&mut hasher);
			assert_eq!(hash_str(text), hasher.finish(), "{:?}", text);
		}
		let bodies = ["first entry", "", "third entry body"];
		let mut hasher = DefaultHasher::new();
		for body in bodies {
			body.to_string().hash(&mut hasher);
		}
		assert_eq!(hash_strs(bodies), hasher.finish());
		let mut seeded = SeededStrHasher::new("the quick brown");
		for seed in [0usize, 1, 127] {
			let mut hasher = DefaultHasher::new();
			seed.hash(&mut hasher);
			"the quick brown".hash(&mut hasher);
			assert_eq!(seeded.hash(seed), hasher.finish(), "seed {}", seed);
		}
	}
}
