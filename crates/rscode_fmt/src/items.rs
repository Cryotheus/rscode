//! Formatting of selected items within a file.
//!
//! Items are formatted by formatting the whole file (so the formatter knows their context and indentation), finding
//! each targeted item in the output by its structural position, and splicing its formatted text into the original
//! text. Text outside of the targeted items is never changed.

use crate::FormatError;
use crate::FormatOptions;
use crate::FormatTarget;
use crate::RsFormatter;
use crate::contains_comments;
use crate::prettyplease_fmt;
use crate::rustfmt;
use crate::source::Parsed;
use crate::source::ensure_parses;
use crate::source::indentation_start;
use crate::source::uses_crlf;
use crate::source::with_line_breaks;
use crate::tree;
use crate::tree::Node;
use rscode_sort::SortOptions;
use rscode_sort::SortTarget;
use rscode_sort::Sorter;
use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;

/// An item targeted for formatting.
#[derive(Debug, Clone)]
struct Target {
	/// The structural index path: `[4, 2]` is the 3rd item inside the 5th top-level item.
	path: Vec<usize>,

	/// The byte range in the original source.
	range: Range<usize>,

	kind: &'static str,
	name: Option<String>,

	/// Whether the item's items can be sorted.
	container: bool,
}

impl Target {
	/// Whether this target is nested inside `outer`.
	fn is_inside(&self, outer: &Target) -> bool {
		outer.path.len() < self.path.len() && self.path.starts_with(&outer.path)
	}

	fn describe(&self) -> String {
		match &self.name {
			Some(name) => format!("{} `{name}`", self.kind),
			None => self.kind.to_owned(),
		}
	}
}

pub(crate) fn format_items(source: &str, targets: &[FormatTarget], options: &FormatOptions) -> Result<String, FormatError> {
	if targets.is_empty() {
		return Ok(source.to_owned());
	}

	// parsed even when only the whole file is targeted, so every mode rejects invalid source alike
	let parsed = Parsed::parse(source)?;
	let whole_file = targets.contains(&FormatTarget::File);
	let items = locate(&parsed, targets)?;
	let sorted = match &options.sort {
		Some(sort) => sort_containers(source, whole_file, &items, sort)?,
		None => None,
	};
	let text = sorted.as_deref().unwrap_or(source);

	if whole_file {
		return format_file(text, sorted.is_some(), options);
	}

	let items = outermost(items);

	if options.formatter == RsFormatter::None {
		if sorted.is_some() {
			ensure_parses(text, "sorting")?;
		}

		return Ok(text.to_owned());
	}

	match &sorted {
		Some(sorted) => {
			let parsed = parse_output(sorted, "sorting")?;

			check_unmoved(&parsed, &items)?;
			format_targets(sorted, &parsed, &items, options)
		}
		None => format_targets(source, &parsed, &items, options),
	}
}

/// Finds the targeted items in the parsed source, in document order and without duplicates.
fn locate(parsed: &Parsed, targets: &[FormatTarget]) -> Result<Vec<Target>, FormatError> {
	let mut starts: Vec<usize> = targets
		.iter()
		.filter_map(|target| match target {
			FormatTarget::Item(start) => Some(*start),
			FormatTarget::File => None,
		})
		.collect();

	if starts.is_empty() {
		return Ok(Vec::new());
	}

	starts.sort_unstable();
	starts.dedup();

	let items = tree::index(parsed);
	let by_start: HashMap<usize, &tree::Indexed<'_>> = items.iter().map(|item| (item.range.start, item)).collect();

	starts
		.into_iter()
		.map(|start| {
			let item = by_start.get(&start).ok_or(FormatError::NoItem(start))?;

			Ok(Target {
				path: item.path.clone(),
				range: item.range.clone(),
				kind: item.node.kind(),
				name: item.node.name(),
				container: item.node.is_container(),
			})
		})
		.collect()
}

/// Sorts the whole file (if targeted) and the targeted containers. `None` if there is nothing to sort.
fn sort_containers(
	source: &str,
	whole_file: bool,
	targets: &[Target],
	options: &SortOptions,
) -> Result<Option<String>, FormatError> {
	let mut sort_targets = Vec::new();

	if whole_file {
		sort_targets.push(SortTarget::File);
	}

	for target in targets.iter().filter(|target| target.container) {
		// when sorting recursively, containers inside other targets are sorted with them
		let inside_container = targets.iter().any(|outer| outer.container && target.is_inside(outer));
		let covered = options.recursive && (whole_file || inside_container);

		if !covered {
			sort_targets.push(SortTarget::Item(target.range.start));
		}
	}

	if sort_targets.is_empty() {
		return Ok(None);
	}

	Ok(Some(Sorter::new(options.clone()).sort_str_within(source, &sort_targets)?))
}

/// Drops targets nested inside other targets: formatting the outer item formats them too.
fn outermost(targets: Vec<Target>) -> Vec<Target> {
	let mut kept: Vec<Target> = Vec::with_capacity(targets.len());

	// in document order, a target can only be nested inside the last kept target
	for target in targets {
		if kept.last().is_none_or(|outer| !target.is_inside(outer)) {
			kept.push(target);
		}
	}

	kept
}

/// Checks that the (outermost) targets are where they were: sorting only rearranges the insides of the targets.
fn check_unmoved(sorted: &Parsed, targets: &[Target]) -> Result<(), FormatError> {
	for target in targets {
		match Node::at(&sorted.file, &target.path) {
			Some(node) if node.kind() == target.kind && node.name() == target.name => {}
			_ => {
				return Err(FormatError::StructureMismatch(format!(
					"{} (item path {:?}) moved in the output of sorting",
					target.describe(),
					target.path,
				)));
			}
		}
	}

	Ok(())
}

fn format_file(text: &str, sorted: bool, options: &FormatOptions) -> Result<String, FormatError> {
	let formatted = match options.formatter {
		RsFormatter::RustFmt => rustfmt::format(text, &options.rustfmt)?,
		RsFormatter::PrettyPlease => prettyplease_fmt::format_str(text, options.allow_comment_loss)?,
		RsFormatter::None if sorted => text.to_owned(),
		RsFormatter::None => return Ok(text.to_owned()),
	};
	let produced_by = match options.formatter {
		RsFormatter::None => "sorting",
		formatter => formatter.name(),
	};

	ensure_parses(&formatted, produced_by)?;

	Ok(formatted)
}

/// Formats the whole text, then splices the formatted text of each target into the text.
fn format_targets(text: &str, parsed: &Parsed, targets: &[Target], options: &FormatOptions) -> Result<String, FormatError> {
	let before: Vec<Range<usize>> = targets
		.iter()
		.map(|target| Node::at(&parsed.file, &target.path).map(|node| parsed.range(node.span())))
		.collect::<Option<_>>()
		.ok_or_else(|| FormatError::StructureMismatch("a target is missing from the source".to_owned()))?;

	let formatted = match options.formatter {
		RsFormatter::RustFmt => rustfmt::format_preserving_items(text, &options.rustfmt)?,
		RsFormatter::PrettyPlease => {
			// comments elsewhere are lost too, but only the targets' text is used
			if !options.allow_comment_loss && before.iter().any(|range| contains_comments(&text[range.clone()])) {
				return Err(FormatError::CommentsWouldBeLost);
			}

			prettyplease_fmt::format_str(text, true)?
		}
		RsFormatter::None => return Ok(text.to_owned()),
	};

	let produced_by = options.formatter.name();
	let formatted_parsed = parse_output(&formatted, produced_by)?;
	let mut matcher = Matcher::new(parsed, &formatted_parsed, produced_by);
	let after: Vec<Option<Range<usize>>> = targets.iter().map(|target| matcher.find(&target.path)).collect::<Result<_, _>>()?;
	let spliced = splice(text, &before, &formatted, &after);

	ensure_parses(&spliced, produced_by)?;

	Ok(spliced)
}

/// Parses text produced by sorting or formatting; failing to parse is a [`FormatError::StructureMismatch`].
fn parse_output(text: &str, produced_by: &str) -> Result<Parsed, FormatError> {
	Parsed::parse(text)
		.map_err(|error| FormatError::StructureMismatch(format!("the output of {produced_by} does not parse: {error}")))
}

/// Finds the items of a source in the formatted source.
///
/// Items are found by their structural position, after checking that the items of every container on the way
/// correspond (see [`tree::align`]): rustfmt may reorder `use` and `extern crate` items among their neighbors (those
/// are found by what they import), and remove `use` items that import nothing.
struct Matcher<'a> {
	before: &'a Parsed,
	after: &'a Parsed,
	produced_by: &'a str,

	/// The alignment of the items of each container visited so far (by structural index path), computed once per
	/// container: with many targets in one container, computing it for each target would be quadratic.
	alignments: HashMap<Vec<usize>, Vec<Option<usize>>>,
}

impl<'a> Matcher<'a> {
	fn new(before: &'a Parsed, after: &'a Parsed, produced_by: &'a str) -> Self {
		Self {
			before,
			after,
			produced_by,
			alignments: HashMap::new(),
		}
	}

	/// The byte range, in the formatted source, of the item at `path` in the source. `None` if the formatter removed
	/// the item (only a `use` item that imports nothing can be removed).
	fn find(&mut self, path: &[usize]) -> Result<Option<Range<usize>>, FormatError> {
		// the containers on the way; `None` is the file
		let mut before: Option<Node<'a>> = None;
		let mut after: Option<Node<'a>> = None;

		for (depth, &index) in path.iter().enumerate() {
			let container = &path[..depth];

			if !self.alignments.contains_key(container) {
				let alignment = tree::align(&children(&self.before.file, before), &children(&self.after.file, after))
					.map_err(|misalignment| self.misaligned(before, misalignment))?;

				self.alignments.insert(container.to_vec(), alignment);
			}

			let Some(node) = child(&self.before.file, before, index) else {
				return Err(self.mismatch(format!("item path {path:?} does not exist")));
			};

			let Some(found) = self.alignments[container][index] else {
				// only `use` items can be removed, and they have no items inside
				return match depth + 1 == path.len() {
					true => Ok(None),
					false => Err(self.mismatch(format!("{} is missing", node.describe()))),
				};
			};

			let Some(found) = child(&self.after.file, after, found) else {
				return Err(self.mismatch(format!("{} is missing", node.describe())));
			};

			if depth + 1 == path.len() {
				if !tree::same_subtree(node, found) {
					return Err(self.mismatch(format!("the items of {} changed", node.describe())));
				}

				return Ok(Some(self.after.range(found.span())));
			}

			before = Some(node);
			after = Some(found);
		}

		Err(self.mismatch("an empty item path was given".to_owned()))
	}

	fn misaligned(&self, container: Option<Node<'_>>, misalignment: tree::Misalignment<'_>) -> FormatError {
		match misalignment {
			tree::Misalignment::Changed => {
				let container = container.map_or_else(|| "the file".to_owned(), Node::describe);

				self.mismatch(format!("the items of {container} changed"))
			}
			tree::Misalignment::Missing(node) => self.mismatch(format!("{} is missing", node.describe())),
		}
	}

	fn mismatch(&self, what: String) -> FormatError {
		FormatError::StructureMismatch(format!("{what} in the output of {}", self.produced_by))
	}
}

/// The items of a container (`None` for the file).
fn children<'a>(file: &'a syn::File, container: Option<Node<'a>>) -> Vec<Node<'a>> {
	match container {
		Some(node) => node.children(),
		None => Node::roots(file),
	}
}

/// The item at `index` in a container (`None` for the file).
fn child<'a>(file: &'a syn::File, container: Option<Node<'a>>, index: usize) -> Option<Node<'a>> {
	match container {
		Some(node) => node.child(index),
		None => file.items.get(index).map(Node::Item),
	}
}

/// Replaces each `before` range of `text` with the corresponding `after` range of `formatted`, or removes it if the
/// formatter removed the item (`None`).
///
/// When only indentation precedes both ranges on their lines, the ranges are extended to the start of their lines so
/// that the indentation comes from the formatter. A removed item that fills its lines is removed with them. The
/// formatted text gets the line endings of `text`.
fn splice(text: &str, before: &[Range<usize>], formatted: &str, after: &[Option<Range<usize>>]) -> String {
	let crlf = uses_crlf(text);
	let mut replacements: Vec<(Range<usize>, Option<Range<usize>>)> = before.iter().cloned().zip(after.iter().cloned()).collect();
	let mut spliced = String::with_capacity(text.len());
	let mut copied = 0;

	replacements.sort_by_key(|(range, _)| range.start);

	// built from first to last: replacing ranges in place would copy the rest of the text for every target
	for (mut range, replacement) in replacements {
		let line_start = indentation_start(text, range.start);
		let replacement = match replacement {
			Some(mut replacement) => {
				if let (Some(line_start), Some(replacement_line_start)) = (line_start, indentation_start(formatted, replacement.start)) {
					range.start = line_start;
					replacement.start = replacement_line_start;
				}

				with_line_breaks(&formatted[replacement], crlf)
			}
			None => {
				if let (Some(line_start), Some(line_end)) = (line_start, trailing_line_end(text, range.end)) {
					range = line_start..line_end;
				}

				Cow::Borrowed("")
			}
		};

		// targets do not overlap: they are outermost, and a range is only extended over indentation
		let start = range.start.max(copied);

		spliced.push_str(&text[copied..start]);
		spliced.push_str(&replacement);
		copied = range.end.max(start);
	}

	spliced.push_str(&text[copied..]);
	spliced
}

/// The end of the line containing `offset` (after its line break), if only spaces and tabs follow `offset` on that
/// line.
fn trailing_line_end(text: &str, offset: usize) -> Option<usize> {
	let rest = &text[offset..];
	let line_break = rest.trim_start_matches([' ', '\t']);
	let end = offset + rest.len() - line_break.len();

	if line_break.is_empty() {
		Some(end)
	} else if line_break.starts_with("\r\n") {
		Some(end + 2)
	} else if line_break.starts_with('\n') {
		Some(end + 1)
	} else {
		None
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn target(path: &[usize], range: Range<usize>) -> Target {
		Target {
			path: path.to_vec(),
			range,
			kind: "fn",
			name: None,
			container: false,
		}
	}

	#[test]
	fn outermost_targets() {
		let targets = vec![
			target(&[0], 0..10),
			target(&[0, 1], 2..4),
			target(&[0, 1, 0], 2..3),
			target(&[1], 11..20),
			target(&[2, 0], 25..30),
			target(&[2, 1], 31..35),
		];
		let kept: Vec<Vec<usize>> = outermost(targets).into_iter().map(|target| target.path).collect();

		assert_eq!(kept, [vec![0], vec![1], vec![2, 0], vec![2, 1]]);
	}

	#[test]
	fn nesting() {
		assert!(target(&[1, 2], 0..0).is_inside(&target(&[1], 0..0)));
		assert!(!target(&[1], 0..0).is_inside(&target(&[1], 0..0)));
		assert!(!target(&[1], 0..0).is_inside(&target(&[1, 2], 0..0)));
		assert!(!target(&[2, 1], 0..0).is_inside(&target(&[1], 0..0)));
	}

	fn find(before: &str, after: &str, path: &[usize]) -> Result<String, FormatError> {
		let before_parsed = Parsed::parse(before).unwrap();
		let after_parsed = Parsed::parse(after).unwrap();
		let range = Matcher::new(&before_parsed, &after_parsed, "test").find(path)?;

		Ok(range.map_or_else(|| "<removed>".to_owned(), |range| after[range].to_owned()))
	}

	#[test]
	fn matches_items_by_position() {
		let before = "use b;\nfn a() {}\nmod m {\n    struct S;\n    fn f() {}\n}\n";
		let after = "use b;\nfn a() {}\nmod m {\n    struct S;\n    fn f() {\n    }\n}\n";

		assert_eq!(find(before, after, &[1]).unwrap(), "fn a() {}");
		assert_eq!(find(before, after, &[2, 1]).unwrap(), "fn f() {\n    }");
	}

	#[test]
	fn matches_reordered_imports_by_identity() {
		let before = "use b::{y, x};\nuse a;\nextern crate q as r;\nextern crate p;\nfn f() {}\n";
		let after = "use a;\nuse b::{x, y};\nextern crate p;\nextern crate q as r;\nfn f() {}\n";

		assert_eq!(find(before, after, &[0]).unwrap(), "use b::{x, y};");
		assert_eq!(find(before, after, &[1]).unwrap(), "use a;");
		assert_eq!(find(before, after, &[2]).unwrap(), "extern crate q as r;");
		assert_eq!(find(before, after, &[4]).unwrap(), "fn f() {}");
	}

	#[test]
	fn matches_duplicate_imports_in_order() {
		let before = "use c;\nuse a;\nuse c;\n";
		let after = "use a;\nuse c;\nuse c ;\n";

		assert_eq!(find(before, after, &[0]).unwrap(), "use c;");
		assert_eq!(find(before, after, &[2]).unwrap(), "use c ;");
	}

	#[test]
	fn matches_items_next_to_removed_imports() {
		// rustfmt removes `use` items that import nothing, unless they have attributes or a visibility
		let before = "use a::{};\nfn f() {}\nuse {};\n#[cfg(x)]\nuse b::{c::{}};\nmod m {\n    use d::{};\n    fn g() {}\n}\n";
		let after = "fn f() {}\n#[cfg(x)]\nuse b::c::{};\nmod m {\n    fn g() {}\n}\n";

		assert_eq!(find(before, after, &[0]).unwrap(), "<removed>");
		assert_eq!(find(before, after, &[1]).unwrap(), "fn f() {}");
		assert_eq!(find(before, after, &[2]).unwrap(), "<removed>");
		assert_eq!(find(before, after, &[3]).unwrap(), "#[cfg(x)]\nuse b::c::{};");
		assert_eq!(find(before, after, &[4]).unwrap(), "mod m {\n    fn g() {}\n}");
		assert_eq!(find(before, after, &[4, 1]).unwrap(), "fn g() {}");

		// other items cannot disappear
		assert!(matches!(find("use a;\nfn f() {}\n", "fn f() {}\n", &[1]), Err(FormatError::StructureMismatch(_))));
	}

	#[test]
	fn matches_imports_with_normalized_visibilities() {
		let before = "pub(in crate) use a;\npub(in self) use  b;\npub(in super) use c;\npub(in crate::m) use d;\n";
		let after = "pub(crate) use a;\npub(self) use b;\npub(super) use c;\npub(in crate::m) use d;\n";

		assert_eq!(find(before, after, &[0]).unwrap(), "pub(crate) use a;");
		assert_eq!(find(before, after, &[1]).unwrap(), "pub(self) use b;");
		assert_eq!(find(before, after, &[2]).unwrap(), "pub(super) use c;");
		assert_eq!(find(before, after, &[3]).unwrap(), "pub(in crate::m) use d;");
	}

	#[test]
	fn detects_structural_changes() {
		let cases = [
			// an item was removed
			("fn a() {}\nfn b() {}\n", "fn b() {}\n", vec![1]),
			// items were reordered
			("fn a() {}\nfn b() {}\n", "fn b() {}\nfn a() {}\n", vec![0]),
			// an item was renamed
			("mod m {\n    fn a() {}\n}\n", "mod m {\n    fn b() {}\n}\n", vec![0, 0]),
			// the items of a container changed, even when the target itself did not
			("mod m {\n    fn a() {}\n    fn b() {}\n}\n", "mod m {\n    fn a() {}\n}\n", vec![0, 0]),
			// imports were merged
			("use a::b;\nuse a::c;\n", "use a::{b, c};\n", vec![0]),
			// an import changed
			("use a::b;\n", "use a::c;\n", vec![0]),
		];

		for (before, after, path) in cases {
			match find(before, after, &path) {
				Err(FormatError::StructureMismatch(message)) => assert!(message.ends_with("in the output of test"), "{message}"),
				other => panic!("{before:?} -> {after:?}: unexpected result {other:?}"),
			}
		}
	}

	/// Splices one range.
	fn splice_one(text: &str, before: Range<usize>, formatted: &str, after: Range<usize>) -> String {
		splice(text, std::slice::from_ref(&before), formatted, &[Some(after)])
	}

	#[test]
	fn splices_with_indentation_from_the_formatter() {
		let text = "mod m {\n  fn a( ) {}\n}\n";
		let formatted = "mod m {\n    fn a() {}\n}\n";
		let before = text.find("fn").unwrap()..text.find("}\n}").unwrap() + 1;
		let after = formatted.find("fn").unwrap()..formatted.find("}\n}").unwrap() + 1;

		assert_eq!(splice_one(text, before, formatted, after), "mod m {\n    fn a() {}\n}\n");
	}

	#[test]
	fn splices_mid_line_items_without_indentation() {
		let text = "fn a() {} fn  b( ) {}\n";
		let formatted = "fn a() {}\nfn b() {}\n";

		assert_eq!(splice_one(text, 10..text.len() - 1, formatted, 10..formatted.len() - 1), "fn a() {} fn b() {}\n");
	}

	#[test]
	fn splices_with_the_line_endings_of_the_text() {
		let text = "fn  a( ) {let x=1;}\r\n";
		let formatted = "fn a() {\n    let x = 1;\n}\n";

		assert_eq!(splice_one(text, 0..text.len() - 2, formatted, 0..formatted.len() - 1), "fn a() {\r\n    let x = 1;\r\n}\r\n");

		let text = "fn  a( ) {let x=1;}\n";
		let formatted = "fn a() {\r\n    let x = 1;\r\n}\r\n";

		assert_eq!(splice_one(text, 0..text.len() - 1, formatted, 0..formatted.len() - 2), "fn a() {\n    let x = 1;\n}\n");
	}

	#[test]
	fn splices_removed_items_with_their_lines() {
		let text = "fn a() {}\n    use a::{};  \r\nfn  b( ) {} use c::{};\nuse d::{};";
		let formatted = "fn a() {}\nfn b() {}\n";
		let before = [offset(text, "use a")..offset(text, "  \r\n"), offset(text, "use c")..offset(text, "\nuse d"), offset(text, "use d")..text.len()];

		assert_eq!(splice(text, &before, formatted, &[None, None, None]), "fn a() {}\nfn  b( ) {} \n");
	}

	fn offset(text: &str, needle: &str) -> usize {
		text.find(needle).unwrap()
	}

	#[test]
	fn splices_multiple_ranges() {
		let text = "fn  a( ) {}\nfn  b( ) {}\nfn  c( ) {}\n";
		let formatted = "fn a() {}\nfn b() {}\nfn c() {}\n";
		let before = [24..35, 0..11];
		let after = [Some(20..29), Some(0..9)];

		assert_eq!(splice(text, &before, formatted, &after), "fn a() {}\nfn  b( ) {}\nfn c() {}\n");
	}
}
