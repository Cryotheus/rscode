//! "Version sorting" as specified by the Rust style guide.

use std::cmp::Ordering;

/// Compares two strings using the Rust style guide's version sorting.
///
/// Chunks of ASCII digits compare numerically (`x8` < `x16`), `_` sorts immediately after ` `,
/// and non-lowercase characters sort before lowercase characters.
///
/// Numbers with equal values but a different count of leading zeros compare equal, except that when the strings are
/// otherwise equal, the string that had more leading zeros at the first such difference sorts first (`x08` < `x8`).
/// Strings that are still equal after that compare with [`str::cmp`], so the order is total.
///
/// ```
/// use rscode_sort::version_cmp;
///
/// let mut names = ["x16", "x_1", "alpha", "x8", "Zeta", "_x", "Alpha", "x08"];
///
/// names.sort_by(|a, b| version_cmp(a, b));
///
/// assert_eq!(names, ["_x", "Alpha", "Zeta", "alpha", "x_1", "x08", "x8", "x16"]);
/// ```
pub fn version_cmp(a: &str, b: &str) -> Ordering {
	let mut left = a;
	let mut right = b;
	let mut leading_zeros = Ordering::Equal;

	loop {
		let (Some(left_char), Some(right_char)) = (left.chars().next(), right.chars().next()) else {
			// the shorter string sorts first
			match (left.is_empty(), right.is_empty()) {
				(true, true) => break,
				(true, false) => return Ordering::Less,
				_ => return Ordering::Greater,
			}
		};

		if left_char.is_ascii_digit() && right_char.is_ascii_digit() {
			let (left_digits, left_rest) = split_digits(left);
			let (right_digits, right_rest) = split_digits(right);
			let ordering = numeric_cmp(left_digits, right_digits);

			if ordering.is_ne() {
				return ordering;
			}

			if leading_zeros.is_eq() {
				// more leading zeros sorts first
				leading_zeros = count_zeros(right_digits).cmp(&count_zeros(left_digits));
			}

			left = left_rest;
			right = right_rest;
		} else {
			let ordering = char_key(left_char).cmp(&char_key(right_char));

			if ordering.is_ne() {
				return ordering;
			}

			left = &left[left_char.len_utf8()..];
			right = &right[right_char.len_utf8()..];
		}
	}

	leading_zeros.then_with(|| a.cmp(b))
}

/// rustfmt's implementation of version sorting (`version_sort` in rustfmt's `src/sort.rs`, MIT OR Apache-2.0), which
/// orders `use` items in style edition 2024.
///
/// On ASCII identifiers it agrees with [`version_cmp`], except that it considers more strings equal. Non-ASCII
/// characters compare by their UTF-8 bytes, so `É` sorts after `a`. Numbers compare by value without overflowing
/// (where rustfmt gives up on numbers that overflow a `usize`).
pub(crate) fn rustfmt_version_cmp(a: &str, b: &str) -> Ordering {
	let mut left = VersionChunks(a);
	let mut right = VersionChunks(b);
	let mut more_zeros = Ordering::Equal;

	loop {
		let ordering = match (left.next(), right.next()) {
			(None, None) => return more_zeros,
			(Some(_), None) => return Ordering::Greater,
			(None, Some(_)) => return Ordering::Less,
			(Some(VersionChunk::Underscore), Some(VersionChunk::Underscore)) => Ordering::Equal,
			(Some(VersionChunk::Underscore), _) => Ordering::Less,
			(_, Some(VersionChunk::Underscore)) => Ordering::Greater,
			(Some(VersionChunk::Number(x)), Some(VersionChunk::Number(y))) => {
				let ordering = numeric_cmp(x, y);

				if ordering.is_eq() && more_zeros.is_eq() {
					more_zeros = count_zeros(y).cmp(&count_zeros(x));
				}

				ordering
			}
			(
				Some(VersionChunk::Str(x) | VersionChunk::Number(x)),
				Some(VersionChunk::Str(y) | VersionChunk::Number(y)),
			) => x.cmp(y),
		};

		if ordering.is_ne() {
			return ordering;
		}
	}
}

/// A chunk of an identifier for [`rustfmt_version_cmp`].
enum VersionChunk<'a> {
	Underscore,
	Str(&'a str),
	Number(&'a str),
}

struct VersionChunks<'a>(&'a str);

impl<'a> Iterator for VersionChunks<'a> {
	type Item = VersionChunk<'a>;

	fn next(&mut self) -> Option<Self::Item> {
		let first = self.0.chars().next()?;

		if first == '_' {
			self.0 = &self.0[1..];
			return Some(VersionChunk::Underscore);
		}

		if first.is_ascii_digit() {
			let (digits, rest) = split_digits(self.0);

			self.0 = rest;
			return Some(VersionChunk::Number(digits));
		}

		let end = self.0.find(|c: char| c == '_' || c.is_ascii_digit()).unwrap_or(self.0.len());
		let (chunk, rest) = self.0.split_at(end);

		self.0 = rest;
		Some(VersionChunk::Str(chunk))
	}
}

/// The sort key of a character outside of a numeric chunk:
/// ` ` < `_` < other non-lowercase characters < lowercase characters, then by code point.
fn char_key(c: char) -> (u8, char) {
	match c {
		' ' => (0, c),
		'_' => (1, c),
		c if c.is_lowercase() => (3, c),
		c => (2, c),
	}
}

/// Splits a leading chunk of ASCII digits off of `s`.
fn split_digits(s: &str) -> (&str, &str) {
	let end = s.bytes().position(|byte| !byte.is_ascii_digit()).unwrap_or(s.len());

	s.split_at(end)
}

fn count_zeros(digits: &str) -> usize {
	digits.bytes().take_while(|&byte| byte == b'0').count()
}

/// Compares chunks of ASCII digits by value, without overflowing.
fn numeric_cmp(a: &str, b: &str) -> Ordering {
	let a = a.trim_start_matches('0');
	let b = b.trim_start_matches('0');

	a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn sorted(input: &[&'static str]) -> Vec<&'static str> {
		let mut output = input.to_vec();

		output.sort_by(|a, b| version_cmp(a, b));
		output
	}

	/// Asserts that `expected` is sorted, whatever order it is given in.
	fn assert_sorts(expected: &[&'static str]) {
		assert_eq!(sorted(expected), expected);

		let mut reversed = expected.to_vec();

		reversed.reverse();
		assert_eq!(sorted(&reversed), expected);

		for window in expected.windows(2) {
			assert_ne!(version_cmp(window[0], window[1]), Ordering::Greater, "{window:?}");
			assert_ne!(version_cmp(window[1], window[0]), Ordering::Less, "{window:?}");
		}
	}

	#[test]
	fn style_guide_example() {
		// the example list of the Rust style guide, in its order
		assert_sorts(&[
			"_ZYXW", "_abcd", "A2", "ABCD", "Z_YXW", "ZY_XW", "ZY_XW", "ZYXW", "ZYXW_", "a1", "abcd", "u_zzz", "u8",
			"u16", "u32", "u64", "u128", "u256", "ua", "usize", "uz", "v000", "v00", "v0", "v0s", "v00t", "v0u",
			"v001", "v01", "v1", "v009", "v09", "v9", "v010", "v10", "w005s09t", "w5s009t", "x64", "x86", "x86_32",
			"x86_64", "x86_128", "x87", "zyxw",
		]);
	}

	#[test]
	fn numbers_compare_by_value() {
		assert_eq!(version_cmp("x8", "x16"), Ordering::Less);
		assert_eq!(version_cmp("x16", "x8"), Ordering::Greater);
		assert_eq!(version_cmp("x9", "x10"), Ordering::Less);
		assert_eq!(version_cmp("1_000_000", "1_010_001"), Ordering::Less);
		assert_sorts(&["5", "5_000", "5_005", "5_050", "5_500", "50", "50_000", "50_005", "50_050", "50_500", "500"]);
		assert_sorts(&["X86_64", "X86_128", "x86_64", "x86_128"]);
	}

	#[test]
	fn huge_numbers_do_not_overflow() {
		let small = "v99999999999999999999999999999999999999";
		let big = "v100000000000000000000000000000000000000";

		assert_eq!(version_cmp(small, big), Ordering::Less);
		assert_eq!(version_cmp(big, small), Ordering::Greater);
		assert_eq!(version_cmp(big, big), Ordering::Equal);
	}

	#[test]
	fn leading_zeros() {
		// equal values: the first difference in leading zeros decides, more zeros first
		assert_eq!(version_cmp("x08", "x8"), Ordering::Less);
		assert_eq!(version_cmp("x8", "x08"), Ordering::Greater);
		assert_eq!(version_cmp("x08", "x008"), Ordering::Greater);
		assert_eq!(version_cmp("w005s09t", "w5s009t"), Ordering::Less);

		// a later difference of anything else wins over leading zeros
		assert_eq!(version_cmp("x08a", "x8b"), Ordering::Less);
		assert_eq!(version_cmp("x08b", "x8a"), Ordering::Greater);

		// zero itself
		assert_sorts(&["v000", "v00", "v0", "v0s", "v00t", "v0u", "v001", "v01", "v1"]);
	}

	#[test]
	fn uppercase_sorts_before_lowercase() {
		assert_eq!(version_cmp("A", "a"), Ordering::Less);
		assert_eq!(version_cmp("Zeta", "alpha"), Ordering::Less);
		assert_eq!(version_cmp("aB", "ab"), Ordering::Less);
		assert_sorts(&["A", "AA", "B", "a", "aA", "aa", "b"]);
		assert_sorts(&["AAA1A", "AAAAA", "BB_BB", "BBBBB", "C3CCC"]);
	}

	#[test]
	fn underscore_placement() {
		// `_` sorts after ` ` and before everything else
		assert_eq!(version_cmp(" ", "_"), Ordering::Less);
		assert_eq!(version_cmp("_", "0"), Ordering::Less);
		assert_eq!(version_cmp("_", "A"), Ordering::Less);
		assert_eq!(version_cmp("_", "a"), Ordering::Less);
		assert_eq!(version_cmp("a_b", "ab"), Ordering::Less);
		assert_eq!(version_cmp("aaa_a", "aaaaa"), Ordering::Less);
		assert_sorts(&["_", "__"]);
		assert_sorts(&["foo", "foo_"]);
		assert_sorts(&["x_", "x__1", "x_1", "x08", "x8", "x8a", "x16", "xa"]);
	}

	#[test]
	fn digits_sort_before_letters() {
		// outside of a numeric comparison, a digit is an ordinary non-lowercase character
		assert_eq!(version_cmp("a1", "ab"), Ordering::Less);
		assert_eq!(version_cmp("a1", "aB"), Ordering::Less);
		assert_eq!(version_cmp("x7x", "xxx"), Ordering::Less);
		assert_eq!(version_cmp("u256", "ua"), Ordering::Less);
	}

	#[test]
	fn rustfmt_use_list() {
		// `rustfmt --edition 2024` sorts `use a::{Zeta, alpha, Alpha, _x, x8, x16, x_1, ...};` in this order
		assert_sorts(&[
			"_x", "Alpha", "X_1", "Zeta", "a1", "aB", "ab", "alpha", "x_", "x__1", "x_1", "x08", "x8", "x8a", "x16",
			"xa",
		]);
	}

	#[test]
	fn empty_strings() {
		assert_eq!(version_cmp("", ""), Ordering::Equal);
		assert_eq!(version_cmp("", "a"), Ordering::Less);
		assert_eq!(version_cmp("a", ""), Ordering::Greater);
		assert_eq!(version_cmp("", "0"), Ordering::Less);
		assert_sorts(&["", "a", "applesauce", "b"]);
		assert_sorts(&["apple", "applesauce"]);
	}

	#[test]
	fn unicode() {
		// non-lowercase characters (including uppercase letters and scripts without case) sort before lowercase ones
		assert_eq!(version_cmp("Éclair", "apple"), Ordering::Less);
		assert_eq!(version_cmp("éclair", "zebra"), Ordering::Greater);
		assert_eq!(version_cmp("x๙x", "xéx"), Ordering::Less);
		assert_eq!(version_cmp("日本", "abc"), Ordering::Less);

		// non-ASCII digits are not numeric
		assert_eq!(version_cmp("x๙", "x10"), Ordering::Greater);
		assert_sorts(&["x0x", "x๙x", "xéx"]);
	}

	#[test]
	fn total_and_consistent() {
		let words = [
			"", "_", "__", "a", "A", "a_", "_a", "a0", "a00", "a1", "a01", "a10", "ab", "aB", "Ab", "AB", "b", "é",
			"É", "x8", "x08", "x008", "x80", "x_8", "x 8", "0", "00", "1", "01",
		];

		for a in words {
			assert_eq!(version_cmp(a, a), Ordering::Equal, "{a:?}");

			for b in words {
				let forward = version_cmp(a, b);

				assert_eq!(forward, version_cmp(b, a).reverse(), "{a:?} {b:?}");
				assert_eq!(forward.is_eq(), a == b, "{a:?} {b:?}");

				for c in words {
					if forward.is_le() && version_cmp(b, c).is_le() {
						assert!(version_cmp(a, c).is_le(), "{a:?} <= {b:?} <= {c:?}");
					}
				}
			}
		}
	}

	#[test]
	fn agrees_with_rustfmt_on_ascii_identifiers() {
		// deterministic pseudo-random identifiers over a small alphabet, so collisions and shared prefixes are common
		let alphabet = ['a', 'b', 'A', 'B', '_', '0', '1', '9'];
		let mut state = 0x2545_f491_4f6c_dd1d_u64;
		let mut next = move || {
			state ^= state << 13;
			state ^= state >> 7;
			state ^= state << 17;
			state
		};
		let idents: Vec<String> = (0..600)
			.map(|_| {
				let len = (next() % 6) as usize;

				(0..len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect()
			})
			.collect();

		for a in &idents {
			for b in &idents {
				let ours = version_cmp(a, b);
				let theirs = rustfmt_version_cmp(a, b);

				// rustfmt considers strings equal where we fall back to `str::cmp`
				if theirs.is_ne() {
					assert_eq!(ours, theirs, "{a:?} vs {b:?}");
				}
			}
		}
	}
}
