//! The line diff rustfmt uses for its `json` and `checkstyle` output: the `lines` function of the `diff` crate.
//!
//! Reproduced exactly (including how it aligns ambiguous lines) so that line numbers and blocks match rustfmt's.

use similar::Algorithm;
use similar::DiffOp;

/// The largest table (in cells, 4 bytes each) built to align lines exactly like rustfmt.
/// Larger changed regions are aligned with the Myers algorithm instead, which may split blocks differently.
const MAX_TABLE_CELLS: usize = 1 << 24;

/// A line of a line diff.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum Line<'a> {
	/// A line only in the original text.
	Original(&'a str),

	/// A line in both texts.
	Both(&'a str),

	/// A line only in the formatted text.
	Formatted(&'a str),
}

/// The `diff` crate's alignment: a table of longest common subsequence lengths of prefixes, walked back from the end,
/// preferring lines only in the formatted text, then lines only in the original text.
fn align_lcs<'a>(old: &[&'a str], new: &[&'a str], lines: &mut Vec<Line<'a>>) {
	let width = new.len() + 1;
	let mut table = vec![0u32; (old.len() + 1) * width];

	for (i, old_line) in old.iter().enumerate() {
		for (j, new_line) in new.iter().enumerate() {
			table[(i + 1) * width + j + 1] = if old_line == new_line {
				table[i * width + j] + 1
			} else {
				table[i * width + j + 1].max(table[(i + 1) * width + j])
			};
		}
	}

	let start = lines.len();
	let mut i = old.len();
	let mut j = new.len();

	loop {
		if j > 0 && (i == 0 || table[i * width + j] == table[i * width + j - 1]) {
			j -= 1;
			lines.push(Line::Formatted(new[j]));
		} else if i > 0 && (j == 0 || table[i * width + j] == table[(i - 1) * width + j]) {
			i -= 1;
			lines.push(Line::Original(old[i]));
		} else if i > 0 && j > 0 {
			i -= 1;
			j -= 1;
			lines.push(Line::Both(old[i]));
		} else {
			break;
		}
	}

	lines[start..].reverse();
}

/// Aligns large regions in linear memory. Within a change, original lines come before formatted lines, like the
/// `diff` crate's.
fn align_myers<'a>(old: &[&'a str], new: &[&'a str], lines: &mut Vec<Line<'a>>) {
	for operation in similar::capture_diff_slices(Algorithm::Myers, old, new) {
		match operation {
			DiffOp::Equal { .. } => lines.extend(old[operation.old_range()].iter().map(|&line| Line::Both(line))),
			DiffOp::Delete { .. } | DiffOp::Insert { .. } | DiffOp::Replace { .. } => {
				lines.extend(old[operation.old_range()].iter().map(|&line| Line::Original(line)));
				lines.extend(new[operation.new_range()].iter().map(|&line| Line::Formatted(line)));
			}
		}
	}
}

/// Diffs the lines (as split by [`str::lines`]) of two texts. A text ending with a line break has a final empty line.
pub(super) fn diff<'a>(original: &'a str, formatted: &'a str) -> Vec<Line<'a>> {
	let old: Vec<&str> = original.lines().collect();
	let new: Vec<&str> = formatted.lines().collect();
	let mut lines = diff_slices(&old, &new);

	match (original.ends_with('\n'), formatted.ends_with('\n')) {
		(true, true) => lines.push(Line::Both("")),
		(true, false) => lines.push(Line::Original("")),
		(false, true) => lines.push(Line::Formatted("")),
		(false, false) => {}
	}

	lines
}

fn diff_slices<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Line<'a>> {
	let prefix = old.iter().zip(new).take_while(|(old, new)| old == new).count();
	let suffix = old[prefix..]
		.iter()
		.rev()
		.zip(new[prefix..].iter().rev())
		.take_while(|(old, new)| old == new)
		.count();
	let old_middle = &old[prefix..old.len() - suffix];
	let new_middle = &new[prefix..new.len() - suffix];
	let mut lines = Vec::with_capacity(old.len().max(new.len()));

	lines.extend(old[..prefix].iter().map(|&line| Line::Both(line)));

	if (old_middle.len() + 1).saturating_mul(new_middle.len() + 1) <= MAX_TABLE_CELLS {
		align_lcs(old_middle, new_middle, &mut lines);
	} else {
		align_myers(old_middle, new_middle, &mut lines);
	}

	lines.extend(old[old.len() - suffix..].iter().map(|&line| Line::Both(line)));
	lines
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn large_regions_fall_back_to_myers() {
		let old: Vec<String> = (0..5_000).map(|index| format!("old {index}")).collect();
		let new: Vec<String> = (0..5_000).map(|index| format!("new {index}")).collect();
		let original = old.join("\n");
		let formatted = new.join("\n");
		let lines = diff(&original, &formatted);

		assert_eq!(lines.len(), 10_000);
		assert!(lines[..5_000].iter().all(|line| matches!(line, Line::Original(_))));
		assert!(lines[5_000..].iter().all(|line| matches!(line, Line::Formatted(_))));
	}

	/// Expected outputs were produced by `diff::lines` of the `diff` crate (0.1.13), which rustfmt uses.
	#[test]
	fn matches_the_diff_crate() {
		let cases = [
			("a\nb\nc\n", "a\nb\nc\n", " a\n b\n c\n \n"),
			("a\nb\nc\n", "a\nx\nc\n", " a\n-b\n+x\n c\n \n"),
			("a\nb", "a\nb\n", " a\n b\n+\n"),
			("a\nb\n", "a\nb", " a\n b\n-\n"),
			("", "", ""),
			("", "\n", "+\n+\n"),
			("a\n\n\n\nb\n", "a\n\nb\n", " a\n \n-\n-\n b\n \n"),
			("x\na\nb\n", "a\nb\nx\n", "-x\n a\n b\n+x\n \n"),
			("a\nb\n", "b\na\n", "-a\n b\n+a\n \n"),
			("}\n}\n}\n", "}\nx\n}\n", " }\n-}\n+x\n }\n \n"),
			("a\nx\nb\nx\nc\n", "a\nb\nx\nx\nc\n", " a\n-x\n b\n+x\n x\n c\n \n"),
			("a\r\nb\r\n", "a\nb\n", " a\n b\n \n"),
			("p\nq\nr\ns\n", "s\nr\nq\np\n", "-p\n-q\n-r\n s\n+r\n+q\n+p\n \n"),
		];

		for (original, formatted, expected) in cases {
			assert_eq!(render(&diff(original, formatted)), expected, "{original:?} -> {formatted:?}");
		}
	}

	#[test]
	fn myers_alignment_keeps_every_line() {
		let old = ["a", "b", "c", "d"];
		let new = ["a", "x", "c", "d", "e"];
		let mut lines = Vec::new();

		align_myers(&old, &new, &mut lines);

		assert_eq!(render(&lines), " a\n-b\n+x\n c\n d\n+e\n");
	}

	fn render(lines: &[Line<'_>]) -> String {
		lines
			.iter()
			.map(|line| match line {
				Line::Original(text) => format!("-{text}\n"),
				Line::Both(text) => format!(" {text}\n"),
				Line::Formatted(text) => format!("+{text}\n"),
			})
			.collect()
	}
}
