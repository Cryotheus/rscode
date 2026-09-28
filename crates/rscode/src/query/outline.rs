//! Outlines: item source with bodies elided.

use super::snippet::Edit;
use super::snippet::Snippet;
use super::snippet::Syntax;
use crate::model::ItemDetail;
use crate::model::ItemId;
use crate::model::Workspace;
use crate::source::FileId;
use crate::source::SourceFile;
use crate::source::TextRange;

/// The outline of an item: its source text with bodies elided, and everything else (comments included) as written.
///
/// - Function and method bodies become `{ ... }`; the bodies of `macro_rules!` definitions and of macro invocations
///   become `{ ... }`, `( ... )`, or `[ ... ]`, keeping the kind of delimiter. Empty bodies are kept.
/// - Initializers of `const`s and `static`s spanning several lines become `= ...;`.
/// - A module is outlined with its items, and the inline modules in it become `mod name { ... }`. Out-of-line modules
///   (and crate roots) are outlined from their file, without their `mod name;` declaration.
/// - `impl` blocks and traits keep their headers, and their items are outlined.
/// - Everything else (`use` items, structs, enums, type aliases, ...) is kept as it is.
/// - Without `docs`, doc comments and `#[doc = ...]` attributes are removed, with the lines they occupy.
///
/// Runs of blank lines become one blank line, and line breaks become `\n`.
///
/// Indentation of the first line is taken from the item's line in the file, so outlines of nested items line up.
pub fn outline_text(workspace: &Workspace, item: ItemId, docs: bool) -> String {
	let source = Source::of(workspace, item);
	let syntax = Syntax::of(source.file.text());

	item_snippet(workspace, item, &source, &syntax, true, docs).render(false)
}

/// Where the text of an item's view comes from: the item's own text, or the whole file of a module that has one (an
/// out-of-line module whose file was loaded, or a crate root).
pub(super) struct Source<'ws> {
	pub(super) file_id: FileId,
	pub(super) file: &'ws SourceFile,
	pub(super) region: TextRange,

	/// Whether the region is the file of a module, rather than the item's own text.
	pub(super) module_file: bool,
}

impl<'ws> Source<'ws> {
	pub(super) fn of(workspace: &'ws Workspace, item: ItemId) -> Self {
		let data = workspace.item(item);
		let krate = workspace.krate(item.krate());

		if let Some(file_id) = data.module_info().filter(|info| !info.inline).and_then(|info| info.file) {
			let file = krate.file(file_id);
			let bom = if file.text().starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };

			return Self { file_id, file, region: TextRange::new(bom, file.text().len()), module_file: true };
		}

		Self { file_id: data.file, file: krate.file(data.file), region: data.range, module_file: false }
	}
}

/// The lines of an item's [`Source`], outlined if `outline`, and without doc comments unless `docs`.
///
/// `syntax` must be that of the source's file.
pub(super) fn item_snippet(
	workspace: &Workspace,
	item: ItemId,
	source: &Source<'_>,
	syntax: &Syntax,
	outline: bool,
	docs: bool,
) -> Snippet {
	let mut edits = Vec::new();

	if outline {
		collect_elisions(workspace, item, true, source.file.text(), &mut edits);
		edits.extend(within(&syntax.initializers, source.region).iter().map(|&range| Edit::Replace(range, "= ...;")));
	}

	if !docs {
		edits.extend(within(&syntax.docs, source.region).iter().map(|&range| Edit::Delete(range)));
	}

	let mut snippet = Snippet::new(source.file, source.region, &edits, within(&syntax.strings, source.region));

	if outline {
		snippet.collapse_blank_lines();
	}

	snippet
}

/// The ranges (sorted by start) that start inside of `region`.
fn within(ranges: &[TextRange], region: TextRange) -> &[TextRange] {
	let start = ranges.partition_point(|range| range.start < region.start);
	let end = ranges.partition_point(|range| range.start < region.end);

	&ranges[start..end.max(start)]
}

/// Collects the elisions of an item and the items inside of it: bodies of functions and macros, and nested inline
/// modules. `root`: whether the item is the one being outlined, whose own body is kept when it is a module.
///
/// Out-of-line modules are not entered (except for the root): their items are in another file.
fn collect_elisions(workspace: &Workspace, item: ItemId, root: bool, text: &str, edits: &mut Vec<Edit>) {
	match &workspace.item(item).detail {
		ItemDetail::Fn(info) => {
			let inside =
				info.body.filter(|body| body.len() >= 2).map(|body| TextRange::new(body.start + 1, body.end - 1));

			edits.extend(inside.and_then(|inside| elide_group(text, inside)));
		}

		// macro-like syntax that was not understood has no path
		ItemDetail::Macro { path, body } if !path.is_empty() => edits.extend(elide_group(text, *body)),

		ItemDetail::Module(info) if !root => {
			if info.inline
				&& let Some(body) = info.body
			{
				edits.extend(elide_group(text, body));
			}
		}

		_ => {
			for child in workspace.children(item) {
				collect_elisions(workspace, child, false, text, edits);
			}
		}
	}
}

/// Replaces a delimited group with `{ ... }`, `( ... )`, or `[ ... ]`, given the range inside of its delimiters, unless
/// it contains nothing but whitespace.
fn elide_group(text: &str, inside: TextRange) -> Option<Edit> {
	let open = inside.start.checked_sub(1)?;
	let bytes = text.as_bytes();

	let replacement = match (bytes.get(open)?, bytes.get(inside.end)?) {
		(b'{', b'}') => "{ ... }",
		(b'(', b')') => "( ... )",
		(b'[', b']') => "[ ... ]",
		_ => return None,
	};

	let contents = text.get(inside.as_range())?;

	(!contents.trim().is_empty()).then(|| Edit::Replace(TextRange::new(open, inside.end + 1), replacement))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn inside(text: &str, open: char) -> TextRange {
		let start = text.find(open).unwrap() + 1;
		let end = text.rfind(['}', ')', ']']).unwrap();

		TextRange::new(start, end)
	}

	#[test]
	fn elides_groups_by_delimiter() {
		let replacement = |text: &str, open: char| match elide_group(text, inside(text, open)) {
			Some(Edit::Replace(range, replacement)) => {
				assert_eq!(range, TextRange::new(text.find(open).unwrap(), text.len() - 1));
				Some(replacement)
			}
			_ => None,
		};

		assert_eq!(replacement("m! { a }.", '{'), Some("{ ... }"));
		assert_eq!(replacement("m!(\n\ta,\n);", '('), Some("( ... )"));
		assert_eq!(replacement("m![/* c */];", '['), Some("[ ... ]"));
		assert_eq!(replacement("m! { \n\t }.", '{'), None);
		assert_eq!(replacement("m!(a];", '('), None);
		assert_eq!(elide_group("{}", TextRange::new(0, 1)), None);
		assert_eq!(elide_group("x", TextRange::new(1, 1)), None);
	}

	#[test]
	fn selects_ranges_within_regions() {
		let ranges = [TextRange::new(0, 2), TextRange::new(3, 5), TextRange::new(5, 9), TextRange::new(12, 13)];

		assert_eq!(within(&ranges, TextRange::new(3, 12)), &ranges[1..3]);
		assert_eq!(within(&ranges, TextRange::new(1, 4)), &ranges[1..2]);
		assert_eq!(within(&ranges, TextRange::new(0, 13)), &ranges);
		assert!(within(&ranges, TextRange::new(9, 12)).is_empty());
		assert!(within(&[], TextRange::new(0, 1)).is_empty());
	}
}
