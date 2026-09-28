//! The Fx hash function (as used by rustc) for the resolver's maps, which are keyed by short names and item ids.
//!
//! It is much faster than the default SipHash on such keys; its lack of flooding resistance does not matter for
//! resolving names of source files a user chose to load.

use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::BuildHasherDefault;
use std::hash::Hasher;

/// A hash map with [`FxHasher`].
pub(super) type FxHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// A hash set with [`FxHasher`].
pub(super) type FxHashSet<T> = HashSet<T, BuildHasherDefault<FxHasher>>;

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct FxHasher {
	hash: u64,
}

impl FxHasher {
	fn add(&mut self, word: u64) {
		self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
	}
}

impl Hasher for FxHasher {
	fn write(&mut self, bytes: &[u8]) {
		let (words, rest) = bytes.as_chunks::<8>();

		for word in words {
			self.add(u64::from_le_bytes(*word));
		}

		if !rest.is_empty() {
			let mut word = [0; 8];

			word[..rest.len()].copy_from_slice(rest);
			self.add(u64::from_le_bytes(word));
		}
	}

	fn write_u8(&mut self, value: u8) {
		self.add(u64::from(value));
	}

	fn write_u32(&mut self, value: u32) {
		self.add(u64::from(value));
	}

	fn write_u64(&mut self, value: u64) {
		self.add(value);
	}

	fn write_usize(&mut self, value: usize) {
		self.add(value as u64);
	}

	fn finish(&self) -> u64 {
		self.hash
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::hash::BuildHasher;

	#[test]
	fn hashes_depend_on_every_byte() {
		let build = BuildHasherDefault::<FxHasher>::default();
		let hashes: FxHashSet<u64> = ["", "a", "b", "ab", "ba", "abcdefgh", "abcdefghi", "abcdefgj"].iter().map(|text| build.hash_one(text)).collect();

		assert_eq!(hashes.len(), 8);
	}
}
