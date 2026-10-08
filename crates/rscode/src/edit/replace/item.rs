//! Editing an item in place: exact text inside of it, and its attributes, doc comment, and visibility.
//!
//! What a caller sends scales with the change, not with the item. Text to replace is looked for in the item as views
//! print it (see [`PrintedText`]: dedented, with `\n` line breaks, and possibly with the line-number gutters of
//! numbered views), so that text copied from the view of a method matches, and as it is written in its file. Where it
//! is found as printed, the new text gets the indentation that the view removed; it always gets the file's line
//! breaks, and nothing else is re-indented.
//!
//! The text of an out-of-line module (or of a crate root) is its file, as views show it, so its items and inner doc
//! comments can be edited; its visibility and outer attributes are those of its `mod` declaration. Attributes, doc
//! comments, and visibilities are found by parsing the item's text on a thread of its own, as plain ranges.
//!
//! The edited item must still be one item of its kind (unless kind changes are allowed), which is checked by parsing
//! it as an item of its container; what that parser cannot take (such as the contents of macro invocations) is left to
//! the check that every edited file still parses.

use super::ItemSpan;
use super::Replacement;
use super::article;
use super::parse::Container;
use super::parse::parse_source;
use super::replacement_warnings;
use crate::Error;
use crate::edit::EditSet;
use crate::edit::trivia;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::Visibility;
use crate::model::Workspace;
use crate::path::ItemPath;
use crate::query::PrintedText;
use crate::query::multiline_strings;
use crate::resolve::Resolver;
use crate::source::SourceFile;
use crate::source::TextRange;
use crate::source::isolated;
use proc_macro2::TokenStream;
use quote::ToTokens;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use syn::Attribute;
use syn::Meta;
use syn::parse::ParseStream;
use syn::parse::Parser;
use syn::spanned::Spanned;

/// How [`closest`] tells what comes closest to a text that was not found, from the closest.
const CLOSEST: [&str; 3] = ["if indentation is ignored", "its first line is at line", "the closest line is"];

/// An attribute of an item (doc comments included), as a range of the text of its [`Region`].
#[derive(Debug, Clone, Eq, PartialEq)]
struct Attr {
	/// Where it is, from its `#` (or the start of its doc comment) to its `]` (or the end of its doc comment).
	range: TextRange,

	/// Whether it is an inner attribute (`#![...]`, `//!`) of a module's file.
	inner: bool,

	/// Whether it is a doc comment (or `#[doc = "..."]`).
	doc: bool,

	/// Its path, as token streams print it.
	path: String,

	/// What is inside of its brackets, as token streams print it.
	meta: String,
}

/// An attribute to remove, from [`ItemEdit::remove_attributes`].
#[derive(Debug, Clone, Eq, PartialEq)]
enum AttributePattern {
	/// Every attribute with this path (as token streams print it), of which there must be one.
	Path(String),

	/// Attributes with these contents (as token streams print them): outer or inner ones as given, or either when the
	/// attribute was given without brackets.
	Exact { meta: String, inner: Option<bool> },
}

impl AttributePattern {
	/// Whether the pattern names an attribute.
	fn matches(&self, attribute: &Attr) -> bool {
		match self {
			Self::Path(path) => attribute.path == *path,
			Self::Exact { meta, inner } => attribute.meta == *meta && inner.is_none_or(|inner| inner == attribute.inner),
		}
	}
}

/// An [`ItemEdit`] whose parts were checked (see [`ItemEdit::check`]).
#[derive(Debug, Clone, Default, Eq, PartialEq)]
struct Checked {
	/// The text replacements, as given.
	replacements: Vec<TextReplacement>,

	/// The attributes to remove: as given, and what they match.
	remove: Vec<(String, AttributePattern)>,

	/// The attributes to add.
	add: Vec<NewAttribute>,

	/// The lines of the new doc comment (none to remove it).
	doc: Option<Vec<String>>,

	/// The new visibility as written in items (`None` for private).
	visibility: Option<Option<String>>,
}

impl Checked {
	/// Whether the edit changes attributes, doc comments, or the visibility (which are those of every import of a
	/// `use` item).
	fn is_structural(&self) -> bool {
		!self.remove.is_empty() || !self.add.is_empty() || self.doc.is_some() || self.visibility.is_some()
	}
}

/// Options for [`edit_item`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct EditItemOptions {
	/// Edit every `cfg` variant rather than failing when the path names several (see
	/// [`replaces_all_variants`](super::replaces_all_variants)).
	pub all_variants: bool,

	/// Allow the edited item to become a different kind of item, or several items.
	pub allow_kind_change: bool,
}

/// What [`edit_item`] changes in an item. Every part is optional (but one must be given); they are applied in the
/// order of the fields.
#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ItemEdit {
	/// Exact text to replace, in order: the [`TextReplacement::old`] text of each must occur exactly once in the item
	/// as the replacements before it left it.
	pub replacements: Vec<TextReplacement>,

	/// Attributes to remove: an attribute path (`derive`, `allow`, `serde`), of which the item must have one attribute
	/// (doc comments do not count; see [`ItemEdit::doc`]), or the exact text of attributes (`#[allow(dead_code)]`,
	/// compared without whitespace).
	pub remove_attributes: Vec<String>,

	/// Attributes to add (`#[derive(Debug)]`, or without brackets, `derive(Debug)`), after the existing ones (just
	/// before the visibility or the keyword). Inner attributes (`#![...]`) go into the file of a module (or crate root)
	/// after its inner attributes; a crate root only has inner attributes, so every attribute added to it is one.
	pub add_attributes: Vec<String>,

	/// The text of the new doc comment, without `///`, replacing the existing doc comments (`///`, `/** */`, and
	/// `#[doc = "..."]`); an empty text removes them. The docs of a module with a file of its own are its inner docs
	/// (`//!`), unless its declaration has docs.
	pub doc: Option<String>,

	/// The new visibility: `pub`, `pub(crate)`, `pub(super)`, `pub(self)`, `pub(in path)`, `crate` (for `pub(crate)`),
	/// or `private` (or an empty text) to remove it; the visibility the item has already changes nothing (with a
	/// note). An import's `use` item is changed, which must have no other imports.
	pub visibility: Option<String>,
}

impl ItemEdit {
	/// Checks the parts of the edit that do not depend on the item: that there is something to change, and that the
	/// visibility and the attributes are valid.
	pub fn check(&self) -> Result<(), Error> {
		self.checked().map(drop)
	}

	/// The parts of the edit, checked and parsed (on a thread of its own: see [`isolated`]).
	fn checked(&self) -> Result<Checked, Error> {
		if self.is_empty() {
			return Err(Error::InvalidSource(
				"nothing to change: give text to replace, attributes to add or remove, a doc comment, or a visibility"
					.to_owned(),
			));
		}

		if let Some(index) = self.replacements.iter().position(|replacement| replacement.old.is_empty()) {
			return Err(Error::InvalidSource(format!("{} is empty", old_name(index, self.replacements.len(), ""))));
		}

		isolated(|| {
			Ok(Checked {
				replacements: self.replacements.clone(),
				remove: (self.remove_attributes.iter())
					.map(|text| Ok((text.trim().to_owned(), attribute_pattern(text)?)))
					.collect::<Result<_, Error>>()?,
				add: self.add_attributes.iter().map(|text| new_attribute(text)).collect::<Result<_, _>>()?,
				doc: self.doc.as_deref().map(doc_lines),
				visibility: self.visibility.as_deref().map(parse_visibility).transpose()?,
			})
		})
	}

	/// Whether the edit changes nothing.
	pub fn is_empty(&self) -> bool {
		self.replacements.is_empty()
			&& self.remove_attributes.is_empty()
			&& self.add_attributes.is_empty()
			&& self.doc.is_none()
			&& self.visibility.is_none()
	}
}

/// The planned edit of one item.
#[derive(Debug, Clone)]
struct ItemPlan<'ws> {
	/// The canonical path of the item (of the import, for an import's `use` item).
	path: String,

	/// The edited regions.
	regions: Regions<'ws>,

	/// Things to know about the edit (see [`Replacement::warnings`]).
	warnings: Vec<String>,

	/// Parts of the edit that changed nothing (see [`Replacement::notes`]).
	notes: Vec<String>,
}

/// An attribute to add, from [`ItemEdit::add_attributes`].
#[derive(Debug, Clone, Eq, PartialEq)]
struct NewAttribute {
	/// The attribute as given, in brackets (`#[...]` or `#![...]`).
	text: String,

	/// Whether it is an inner attribute (`#![...]`).
	inner: bool,

	/// What is inside of its brackets, as token streams print it.
	meta: String,
}

/// Where a text to replace occurs in the text of a region (see [`try_replace`]).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct Occurrence {
	/// Where it is in the text of the region.
	range: TextRange,

	/// Where it is in the text as views print it (see [`PrintedText`]), when it occurs there; `None` when it only
	/// occurs as written in the file.
	printed: Option<(usize, usize)>,
}

/// Why [`plan_item`] failed for an item: the error, and the index of the text replacement that failed, if one did.
#[derive(Debug)]
struct PlanFailure {
	error: Error,
	replacement: Option<usize>,
}

impl PlanFailure {
	/// How far the edit of the item got (of `replacements` text replacements): the text replacements that fit it, and
	/// then whether more than text that the item does not have failed (text that occurs several times in it, or what
	/// follows the text replacements).
	fn progress(&self, replacements: usize) -> (usize, bool) {
		match (self.replacement, &self.error) {
			(Some(index), Error::TextMismatch { lines, .. }) => (index, !lines.is_empty()),
			(Some(index), _) => (index, true),
			(None, _) => (replacements, true),
		}
	}
}

impl From<Error> for PlanFailure {
	fn from(error: Error) -> Self {
		Self {
			error,
			replacement: None,
		}
	}
}

/// The attributes and the visibility of an item, as ranges of the text of its [`Region`].
#[derive(Debug, Clone, Default, Eq, PartialEq)]
struct Prefix {
	attributes: Vec<Attr>,

	/// The visibility; for items without one, an empty range at the item's first token after the attributes.
	visibility: TextRange,

	/// The visibility as written in items (`pub(crate)`), `None` when there is none.
	visibility_text: Option<String>,
}

/// Text of a file that an edit changes, as edited so far.
#[derive(Debug, Clone)]
struct Region<'ws> {
	file: &'ws SourceFile,

	/// The range of the file that the text replaces.
	range: TextRange,

	/// The text, as edited so far.
	text: String,

	/// The indentation of the item's line when code precedes the item on it, so that the text starts at the item
	/// rather than at the start of its line; views put it before the text. Empty otherwise.
	prefix: String,

	/// Where the item starts in the text: after the indentation of its line (at 0 in a module's file).
	item: usize,

	/// The indentation of the item's line, for new lines (none in a module's file).
	indent: String,

	/// The line break of the file.
	line_ending: &'static str,

	/// Whether the text is the file of a module (or crate root), whose attributes are inner attributes.
	module_file: bool,
}

impl<'ws> Region<'ws> {
	/// The text of an item at `range` of a file: from the start of its line, unless code precedes it there.
	fn item(file: &'ws SourceFile, range: TextRange) -> Self {
		let text = file.text();
		let line_start = text[..range.start].rfind('\n').map_or(bom_len(text).min(range.start), |index| index + 1);
		let indent = trivia::line_indent(text, range.start).to_owned();

		let (start, prefix) = match text[line_start..range.start].bytes().all(|byte| matches!(byte, b' ' | b'\t')) {
			true => (line_start, String::new()),
			false => (range.start, indent.clone()),
		};

		Self {
			file,
			range: TextRange::new(start, range.end),
			text: text[start..range.end].to_owned(),
			prefix,
			item: range.start - start,
			indent,
			line_ending: trivia::line_ending(text),
			module_file: false,
		}
	}

	/// The file of a module, but for a byte order mark.
	fn module_file(file: &'ws SourceFile) -> Self {
		let text = file.text();
		let start = bom_len(text);

		Self {
			file,
			range: TextRange::new(start, text.len()),
			text: text[start..].to_owned(),
			prefix: String::new(),
			item: 0,
			indent: String::new(),
			line_ending: trivia::line_ending(text),
			module_file: true,
		}
	}

	/// Applies edits, which must not overlap, to the text.
	fn apply(&mut self, mut edits: Vec<(TextRange, String)>) {
		edits.sort_by_key(|(range, _)| std::cmp::Reverse((range.start, range.end)));

		for (range, replacement) in edits {
			self.text.replace_range(range.as_range(), &replacement);
		}
	}

	/// The attributes and visibility of the item (inner attributes of a module's file), as the text has them now.
	fn attributes(&self, path: &str) -> Result<Prefix, Error> {
		let (text, start, module_file) = (self.text.as_str(), self.item, self.module_file);

		isolated(|| parse_prefix(&text[start..], start, module_file))
			.map_err(|error| Error::Unsupported(format!("cannot read the attributes of `{path}`: {error}")))
	}

	/// The edit of the file that turns its text into the edited text: only the part that changed.
	fn edit(&self) -> Option<(TextRange, String)> {
		let old = self.original();
		let new = self.text.as_str();
		let mut prefix = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();

		while !old.is_char_boundary(prefix) {
			prefix -= 1;
		}

		let longest = old.len().min(new.len()) - prefix;
		let mut suffix = (old.bytes().rev().zip(new.bytes().rev())).take(longest).take_while(|(a, b)| a == b).count();

		while !old.is_char_boundary(old.len() - suffix) {
			suffix -= 1;
		}

		(old != new).then(|| {
			(
				TextRange::new(self.range.start + prefix, self.range.end - suffix),
				new[prefix..new.len() - suffix].to_owned(),
			)
		})
	}

	/// Whether the text was edited.
	fn is_changed(&self) -> bool {
		self.text != self.original()
	}

	/// The line (1-based) of an offset of the text, in the file as edited so far.
	fn line_of(&self, offset: usize) -> usize {
		let before = self.text.get(..offset).unwrap_or(&self.text);

		self.file.line_col(self.range.start).line + before.matches('\n').count()
	}

	/// The text before the edit.
	fn original(&self) -> &'ws str {
		&self.file.text()[self.range.as_range()]
	}
}

/// The parts of the item's text that the edit changes.
#[derive(Debug, Clone)]
struct Regions<'ws> {
	/// The item's own text (for an out-of-line module, its declaration), with its outer attributes and doc comments
	/// and its visibility; none for crate roots.
	outer: Option<Region<'ws>>,

	/// The file of an out-of-line module or crate root, with its inner attributes and doc comments.
	file: Option<Region<'ws>>,
}

impl<'ws> Regions<'ws> {
	/// The region with the inner attributes of the item (the file of a module or crate root) or else with its outer
	/// attributes, if it has one.
	fn attributes(&mut self, inner: bool) -> Option<&mut Region<'ws>> {
		match inner {
			true => self.file.as_mut(),
			false => self.outer.as_mut(),
		}
	}

	/// The edited regions.
	fn changed(&self) -> impl Iterator<Item = &Region<'ws>> {
		self.outer.iter().chain(&self.file).filter(|region| region.is_changed())
	}

	/// The region whose text views show, and text replacements change.
	fn text(&mut self) -> &mut Region<'ws> {
		match (&mut self.file, &mut self.outer) {
			(Some(file), _) => file,
			(None, Some(outer)) => outer,
			(None, None) => unreachable!("every item has text"),
		}
	}
}

/// How a range to delete ([`deletions`]) relates to its line.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Shape {
	/// Whole lines, with their line breaks.
	Lines,

	/// Code follows it on its line: it ends where that code starts.
	BeforeCode,

	/// Code precedes it on its line and nothing follows it: it starts where that code ends, and ends at the end of the
	/// line (before the line break).
	AfterCode,
}

/// One exact-text replacement inside of an item (see [`ItemEdit::replacements`]).
#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct TextReplacement {
	/// The text to replace: as views print it (dedented, with `\n` line breaks, and also with their line numbers when
	/// every line has one, which tell which occurrence is meant when there are several), or as written in the file.
	/// Text that occurs in both forms at different places occurs several times.
	pub old: String,

	/// The text to put in its place, in the form of `old`: when `old` is found as views print it, the lines of `new`
	/// after the first get the indentation that the view removed. An `old` without line breaks that is found as
	/// written is found as printed too (unless it starts in that indentation), so its `new` is indented as in views.
	pub new: String,
}

/// Adds an attribute to an item (see [`ItemEdit::add_attributes`]).
fn add_attribute(regions: &mut Regions<'_>, attribute: &NewAttribute, path: &str) -> Result<(), Error> {
	// (a crate root only has inner attributes: whatever is added to it is one)
	let inner = attribute.inner || regions.outer.is_none();

	let Some(region) = regions.attributes(inner) else {
		return Err(Error::Unsupported(format!(
			"`{}` is an inner attribute, which only modules with a file of their own (and crate roots) take",
			attribute.text
		)));
	};

	let prefix = region.attributes(path)?;

	if let Some(existing) = (prefix.attributes.iter()).find(|existing| existing.inner == inner && existing.meta == attribute.meta) {
		return Err(Error::InvalidSource(format!(
			"`{path}` already has the attribute `{}`",
			&region.text[existing.range.as_range()]
		)));
	}

	let text = match (inner, attribute.inner) {
		(true, false) => format!("#!{}", attribute.text.strip_prefix('#').unwrap_or(&attribute.text)),
		_ => attribute.text.clone(),
	};
	let line_ending = region.line_ending;

	let edit = match inner {
		// after the last inner attribute (or inner doc comment), on a line of its own
		true => match prefix.attributes.last() {
			Some(last) => match line_end(&region.text, last.range.end) {
				end if end == region.text.len() => (TextRange::new(end, end), format!("{line_ending}{text}")),
				end => (TextRange::new(end + 1, end + 1), format!("{text}{line_ending}")),
			},

			None => {
				let start = shebang_len(&region.text);
				let blank_after = region.text[start..].trim_start_matches([' ', '\t', '\r']).starts_with('\n');
				let separator = if blank_after || region.text[start..].trim().is_empty() { "" } else { line_ending };

				(TextRange::new(start, start), format!("{text}{line_ending}{separator}"))
			}
		},

		// before the visibility (or keyword), on the line of the attributes when they share it with the item
		false => {
			let at = prefix.visibility.start;
			let shares_line = (prefix.attributes.last()).is_some_and(|last| !region.text[last.range.end..at].contains('\n'));

			match shares_line {
				true => (TextRange::new(at, at), format!("{text} ")),
				false => (TextRange::new(at, at), format!("{text}{line_ending}{}", region.indent)),
			}
		}
	};

	region.apply(vec![edit]);
	Ok(())
}

/// Parses an attribute to remove (see [`ItemEdit::remove_attributes`]); must run on a parsing thread.
fn attribute_pattern(text: &str) -> Result<AttributePattern, Error> {
	let text = text.trim();

	if !text.starts_with('#')
		&& let Ok(path) = syn::parse_str::<syn::Path>(text)
	{
		return Ok(AttributePattern::Path(path.to_token_stream().to_string()));
	}

	let attribute = new_attribute(text)?;

	Ok(AttributePattern::Exact {
		meta: attribute.meta,
		inner: text.starts_with('#').then_some(attribute.inner),
	})
}

/// The length of a byte order mark at the start of a text.
fn bom_len(text: &str) -> usize {
	if text.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 }
}

/// Checks that the item still is one item of its kind in its container (unless `allow_kind_change`), and tells what
/// the edit leaves as it is (see [`replacement_warnings`]).
///
/// Items that the container's parser does not take as they were (such as the contents of macro invocations) are left
/// to the check that the whole file still parses.
fn check_item(ws: &Workspace, item: ItemId, path: &str, region: &Region<'_>, allow_kind_change: bool) -> Result<Vec<String>, Error> {
	let data = ws.item(item);
	let Some(container) = ws.parent(item).and_then(|parent| Container::of_item(ws, parent)) else {
		return Ok(Vec::new());
	};

	let is_the_item = |items: &[super::parse::NewItem]| matches!(items, [new] if new.kind == data.kind);

	if !parse_source(&region.original()[region.item..], container).is_ok_and(|parsed| is_the_item(&parsed.items)) {
		return Ok(Vec::new());
	}

	let after = |problem: String| {
		Error::InvalidSource(format!(
			"after the edit, `{path}` (at line {} of {}) {problem}",
			region.line_of(region.item),
			ws.display_path(region.file.path()).display()
		))
	};

	let parsed = parse_source(&region.text[region.item..], container).map_err(|error| match error {
		Error::InvalidSource(message) => after(message.strip_prefix("the source ").unwrap_or(&message).to_owned()),
		error => error,
	})?;

	match parsed.items.as_slice() {
		_ if is_the_item(&parsed.items) => {}
		[] => return Err(after("is no item: remove items to remove them".to_owned())),
		_ if allow_kind_change => {}

		[new] => {
			return Err(after(format!(
				"is {} rather than {} (allow a kind change to change it)",
				article(new.kind),
				article(data.kind)
			)));
		}

		items => return Err(after(format!("is {} items (allow a kind change to split it)", items.len()))),
	}

	// (structural edits of an out-of-line module change its declaration, which keeps its name)
	match data.module_info().is_some_and(|info| !info.inline) {
		true => Ok(Vec::new()),
		false => Ok(replacement_warnings(ws, item, path, &parsed.items)),
	}
}

/// The line that comes closest to `old` when it does not occur in the printed text of a region, as the end of a
/// message: one that matches if indentation is ignored, or the first line of `old`, or a similar line.
fn closest(region: &Region<'_>, printed: &PrintedText, old: &str) -> String {
	let lines: Vec<(&str, usize)> = printed.lines().collect();
	let line_number = |index: usize| region.line_of(lines[index].1);

	// without indentation (and trailing spaces)
	let flat_text = flatten(lines.iter().map(|(line, _)| *line));
	let flat_old = flatten(old.split('\n'));

	if let [at] = occurrences(&flat_text, &flat_old)[..] {
		let index = flat_text[..at].matches('\n').count();
		let style = |text: &str, unit: &str| {
			let indented = |c: char| text.split('\n').any(|line| line.starts_with(c));

			match (unit, indented('\t'), indented(' ')) {
				("\t", false, true) => " (the file indents with tabs, `old` with spaces)",
				(unit, true, false) if unit.starts_with(' ') => " (the file indents with spaces, `old` with tabs)",
				_ => " (copy the indentation of every line from the view)",
			}
		};

		return format!(
			"; it matches at line {} if indentation is ignored{}",
			line_number(index),
			style(old, &trivia::indent_unit(region.file.text()))
		);
	}

	let Some(first) = old.split('\n').map(str::trim).find(|line| !line.is_empty()) else {
		return String::new();
	};

	let starts: Vec<usize> = (0..lines.len()).filter(|&index| lines[index].0.contains(first)).collect();

	if !starts.is_empty() {
		let numbers: Vec<String> = starts.iter().take(3).map(|&index| line_number(index).to_string()).collect();

		return format!("; its first line is at line {}, but what follows differs", numbers.join(", "));
	}

	let best = (0..lines.len())
		.map(|index| (similarity(lines[index].0.trim(), first), index))
		.max_by(|a, b| a.0.total_cmp(&b.0));

	match best {
		Some((score, index)) if score >= 0.5 => {
			let text: String = lines[index].0.trim().chars().take(100).collect();

			format!("; the closest line is {}: `{text}`", line_number(index))
		}

		_ => String::new(),
	}
}

/// The ranges to delete for removing `ranges` (sorted, disjoint) from the text of a region, with their shapes: ranges
/// separated by nothing but spaces are merged, then extended to whole lines when nothing else is on their lines, or
/// else to the spaces between them and the code after them (or before them, when nothing follows them).
fn deletions(region: &Region<'_>, ranges: &[TextRange]) -> Vec<(TextRange, Shape)> {
	let text = region.text.as_str();
	let is_blank = |text: &str| text.bytes().all(|byte| matches!(byte, b' ' | b'\t' | b'\r'));
	let mut merged: Vec<TextRange> = Vec::new();

	for &range in ranges {
		match merged.last_mut() {
			Some(last) if spaces(&text[last.end..range.start]) == range.start - last.end => last.end = range.end,

			_ => merged.push(range),
		}
	}

	(merged.into_iter())
		.map(|range| {
			let line_start = text[..range.start].rfind('\n').map_or(0, |index| index + 1);
			let line_end = line_end(text, range.end);
			let before = &text[line_start..range.start];
			let after = &text[range.end..line_end];

			// (the text of a region whose item follows code on its line does not start a line)
			let starts_line = line_start > 0 || region.prefix.is_empty();

			match (starts_line && is_blank(before), is_blank(after)) {
				(true, true) => (TextRange::new(line_start, (line_end + 1).min(text.len())), Shape::Lines),
				(_, false) => (TextRange::new(range.start, range.end + spaces(after)), Shape::BeforeCode),

				(false, true) => {
					let code_end = before.trim_end_matches([' ', '\t']).len();
					let end = line_end - usize::from(after.ends_with('\r'));

					(TextRange::new(line_start + code_end, end), Shape::AfterCode)
				}
			}
		})
		.collect()
}

/// The lines of a new doc comment (see [`ItemEdit::doc`]): none for an empty (or blank) text. Lines that all start
/// with `///` (or `//!`) lose it.
fn doc_lines(doc: &str) -> Vec<String> {
	let doc = doc.replace("\r\n", "\n");
	let doc = doc.strip_suffix('\n').unwrap_or(&doc);

	if doc.trim().is_empty() {
		return Vec::new();
	}

	let lines: Vec<&str> = doc.split('\n').collect();

	let marked = |marker: &str| {
		(lines.iter())
			.filter(|line| !line.trim().is_empty())
			.all(|line| line.trim_start().starts_with(marker) && !line.trim_start().starts_with("////"))
	};
	let marker = ["///", "//!"].into_iter().find(|marker| marked(marker));

	(lines.into_iter())
		.map(|line| {
			let line = match marker {
				Some(marker) => {
					let rest = line.trim_start().strip_prefix(marker).unwrap_or_default();

					rest.strip_prefix(' ').unwrap_or(rest)
				}

				None => line,
			};

			line.trim_end().to_owned()
		})
		.collect()
}

/// Plans editing the item named by `path`: replacing exact text inside of it, and changing its attributes, doc comment,
/// and visibility (see [`ItemEdit`]). Only what changes is written.
///
/// The item is named like for [`replace`](super::replace): imports stand for their `use` items (the text of a `use`
/// item with other imports can be edited, but not its attributes or visibility), and when the path names several
/// items (`cfg` variants), the one that every text replacement fits is edited, or with
/// [`EditItemOptions::all_variants`] every one of them. Without text replacements, or when they fit several, the path
/// is [`Error::Ambiguous`]; when they fit none, the error is that of the item they got furthest in (or that the text of
/// a replacement is in none of the items that they got as far in).
///
/// Text replacements fail with [`Error::TextMismatch`] when their text does not occur in the item exactly once (the
/// message tells where it occurs, or what comes closest). The item must still be one item of its kind afterwards
/// ([`EditItemOptions::allow_kind_change`]), and its file must still parse when the edit is previewed or applied.
/// [`Replacement::spans`] tells where the edited items are after the edit, and [`Replacement::notes`] which parts
/// changed nothing.
pub fn edit_item(resolver: &Resolver<'_>, path: &ItemPath, edit: &ItemEdit, options: &EditItemOptions) -> Result<Replacement, Error> {
	let checked = edit.checked()?;
	let ws = resolver.workspace();
	let mut resolved = resolver.resolve_item_path(path);

	crate::edit::check_private_imports(resolver, path, &mut resolved)?;

	let (items, imports) = use_items(ws, resolved);
	let items = super::distinct_places(ws, items);
	let plan = |item: ItemId| plan_item(resolver, item, &imports, &checked, options);
	let mut notes = Vec::new();

	let plans: Vec<ItemPlan<'_>> = match items.as_slice() {
		[] => return Err(Error::NotFound(path.to_string())),
		[item] => vec![plan(*item).map_err(|failure| failure.error)?],
		_ if options.all_variants && super::replaces_all_variants(resolver, path) => {
			items.iter().map(|&item| plan(item).map_err(|failure| failure.error)).collect::<Result<_, _>>()?
		}

		_ if checked.replacements.is_empty() => return Err(super::ambiguous(resolver, path, &items, &imports)),

		// the text to replace may tell them apart
		_ => {
			let (fitting, failed): (Vec<_>, Vec<_>) = items.iter().map(|&item| (item, plan(item))).partition(|(_, plan)| plan.is_ok());
			let mut fitting: Vec<_> = fitting.into_iter().filter_map(|(item, plan)| Some((item, plan.ok()?))).collect();

			match fitting.len() {
				1 => {
					let (item, plan) = fitting.remove(0);

					notes.push(format!(
						"of the {} items that `{path}` names, only {} has the text to replace",
						items.len(),
						crate::edit::describe(resolver, item)
					));
					vec![plan]
				}

				0 => {
					let failures = failed.into_iter().filter_map(|(_, plan)| plan.err()).collect();

					return Err(unfit(failures, &checked.replacements, path).unwrap_or_else(|| {
						super::ambiguous(resolver, path, &items, &imports)
					}));
				}

				_ => return Err(super::ambiguous(resolver, path, &items, &imports)),
			}
		}
	};

	let mut edits = EditSet::new();
	let mut replacement = Replacement {
		edits: EditSet::new(),
		replaced: Vec::new(),
		files: Vec::new(),
		spans: Vec::new(),
		warnings: Vec::new(),
		notes,
	};

	// the file edits, to tell how many lines the edits before an item add
	let changes: Vec<(&Path, TextRange, isize)> = (plans.iter())
		.flat_map(|plan| plan.regions.changed())
		.filter_map(|region| {
			let (range, text) = region.edit()?;
			let added = text.matches('\n').count() as isize - region.file.text()[range.as_range()].matches('\n').count() as isize;

			edits.replace(region.file, range, text);
			Some((region.file.path(), range, added))
		})
		.collect();

	for plan in &plans {
		replacement.notes.extend(plan.notes.iter().cloned());
		replacement.warnings.extend(plan.warnings.iter().cloned());

		// the file of a module when it changed, else its declaration (or the item)
		let Some(region) = plan.regions.file.iter().chain(&plan.regions.outer).find(|region| region.is_changed()) else {
			continue;
		};

		let shift: isize = (changes.iter())
			.filter(|(file, range, _)| *file == region.file.path() && range.end <= region.range.start)
			.map(|(_, _, added)| added)
			.sum();
		let start = region.line_of(region.item).saturating_add_signed(shift);
		let lines = region.text[region.item..].trim_end_matches(['\n', '\r']).matches('\n').count();

		replacement.replaced.push(plan.path.clone());
		replacement.spans.push(ItemSpan {
			path: plan.path.clone(),
			file: region.file.path().to_path_buf(),
			start,
			end: start + lines,
		});

		for region in plan.regions.changed() {
			if !replacement.files.iter().any(|file| file == region.file.path()) {
				replacement.files.push(region.file.path().to_path_buf());
			}
		}
	}

	if plans.len() > 1 {
		replacement.warnings.push(format!("edited {} `cfg` variants of `{path}`", plans.len()));
	}

	replacement.edits = edits;
	Ok(replacement)
}

/// Lines without their leading and trailing whitespace, joined with `\n`.
fn flatten<'a>(lines: impl Iterator<Item = &'a str>) -> String {
	lines.map(str::trim).collect::<Vec<_>>().join("\n")
}

/// The length of a line-number gutter of a numbered view at the start of a line (`  12 │ `; `     │ ` for lines that
/// are not from the file, and `  12 │` for empty lines), if it has one.
pub(super) fn gutter_len(line: &str) -> Option<usize> {
	let bar = line.find('│')?;
	let number = line[..bar].strip_suffix(' ')?.trim_start_matches(' ');

	if !number.bytes().all(|byte| byte.is_ascii_digit()) {
		return None;
	}

	let after = bar + '│'.len_utf8();

	Some(after + usize::from(line[after..].starts_with(' ')))
}

/// Whether `offset` is strictly inside of one of the (sorted, disjoint) `ranges`.
fn is_inside(ranges: &[TextRange], offset: usize) -> bool {
	let index = ranges.partition_point(|range| range.start < offset);

	index > 0 && offset < ranges[index - 1].end
}

/// Where the line containing `offset` ends: at its `\n` (or the end of the text).
fn line_end(text: &str, offset: usize) -> usize {
	text[offset..].find('\n').map_or(text.len(), |index| offset + index)
}

/// Parses an attribute to add (see [`ItemEdit::add_attributes`]); must run on a parsing thread.
fn new_attribute(text: &str) -> Result<NewAttribute, Error> {
	let text = text.trim();
	let bracketed = match text.starts_with('#') {
		true => text.to_owned(),
		false => format!("#[{text}]"),
	};
	let inner = bracketed.starts_with("#!");
	let invalid = |why: &str| Error::InvalidSource(format!("`{text}` is not an attribute: {why}"));

	let parsed = match inner {
		true => Attribute::parse_inner.parse_str(&bracketed),
		false => Attribute::parse_outer.parse_str(&bracketed),
	};
	let attribute = match parsed.map_err(|error| invalid(&error.to_string()))? {
		attributes if attributes.len() == 1 => attributes.into_iter().next().unwrap_or_else(|| unreachable!()),
		_ => return Err(invalid("expected one attribute")),
	};

	if attribute.path().is_ident("doc") && matches!(attribute.meta, Meta::NameValue(_)) {
		return Err(Error::InvalidSource(format!(
			"`{text}` is a doc comment: set doc comments as such instead"
		)));
	}

	Ok(NewAttribute {
		text: bracketed,
		inner,
		meta: attribute.meta.to_token_stream().to_string(),
	})
}

/// The offsets where `needle` occurs in `text`, overlapping occurrences included (none for an empty needle).
fn occurrences(text: &str, needle: &str) -> Vec<usize> {
	let mut found = Vec::new();
	let mut from = 0;

	if needle.is_empty() {
		return found;
	}

	while let Some(index) = text[from..].find(needle) {
		let at = from + index;

		found.push(at);
		from = at + text[at..].chars().next().map_or(1, char::len_utf8);
	}

	found
}

/// How to name the `old` text of a replacement in messages: by its number and first line when there are several.
fn old_name(index: usize, count: usize, old: &str) -> String {
	match count {
		1 => "`old`".to_owned(),

		_ => {
			let first = old.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default();
			let first: String = first.chars().take(40).collect();

			match first.is_empty() {
				true => format!("`old` number {}", index + 1),
				false => format!("`old` number {} (`{first}`)", index + 1),
			}
		}
	}
}

/// Parses the attributes and the visibility at the start of `text`, an item (or with `module_file`, the inner
/// attributes of a file), which starts at `offset` of its region's text; must run on a parsing thread.
fn parse_prefix(text: &str, offset: usize, module_file: bool) -> Result<Prefix, String> {
	let skipped = if module_file { shebang_len(text) } else { 0 };

	let parser = |input: ParseStream<'_>| {
		let attributes = match module_file {
			true => input.call(Attribute::parse_inner)?,
			false => input.call(Attribute::parse_outer)?,
		};
		let next = input.span().byte_range().start;
		let visibility = match module_file {
			true => syn::Visibility::Inherited,
			false => input.parse()?,
		};

		input.parse::<TokenStream>()?;
		Ok((attributes, next, visibility))
	};

	let (attributes, next, visibility) = parser.parse_str(&text[skipped..]).map_err(|error| error.to_string())?;
	let shift = |range: std::ops::Range<usize>| TextRange::new(range.start + offset + skipped, range.end + offset + skipped);

	Ok(Prefix {
		attributes: (attributes.iter())
			.map(|attribute| Attr {
				range: shift(attribute.span().byte_range()),
				inner: module_file,
				doc: attribute.path().is_ident("doc") && matches!(attribute.meta, Meta::NameValue(_)),
				path: attribute.path().to_token_stream().to_string(),
				meta: attribute.meta.to_token_stream().to_string(),
			})
			.collect(),
		visibility: match visibility {
			syn::Visibility::Inherited => shift(next..next),
			ref visibility => shift(visibility.span().byte_range()),
		},
		visibility_text: visibility_text(&visibility),
	})
}

/// Parses a new visibility (see [`ItemEdit::visibility`]): `None` for private; must run on a parsing thread.
fn parse_visibility(text: &str) -> Result<Option<String>, Error> {
	let text = text.trim();

	match text {
		"" | "private" => return Ok(None),
		"crate" => return Ok(Some("pub(crate)".to_owned())),
		_ => {}
	}

	match syn::parse_str::<syn::Visibility>(text) {
		Ok(visibility) if visibility_text(&visibility).is_some() => Ok(visibility_text(&visibility)),

		_ => Err(Error::InvalidSource(format!(
			"`{text}` is not a visibility: expected `pub`, `pub(crate)`, `pub(super)`, `pub(self)`, `pub(in path)`, or \
			 `private`"
		))),
	}
}

/// Plans the edit of one item (an import's `use` item).
fn plan_item<'ws>(
	resolver: &Resolver<'ws>,
	item: ItemId,
	imports: &[(ItemId, ItemId)],
	checked: &Checked,
	options: &EditItemOptions,
) -> Result<ItemPlan<'ws>, PlanFailure> {
	let ws = resolver.workspace();
	let data = ws.item(item);
	let named = super::import_of(item, imports);
	let path = resolver.canonical_path(named).to_string();

	let mut plan = ItemPlan {
		regions: Regions {
			outer: (!item.is_crate_root()).then(|| Region::item(ws.file_of(item), data.range)),
			file: (data.module_info().filter(|info| !info.inline).and_then(|info| info.file))
				.map(|file| Region::module_file(ws.krate(item.krate()).file(file))),
		},
		path,
		warnings: Vec::new(),
		notes: Vec::new(),
	};
	let path = plan.path.as_str();

	// where the text is, for messages
	let place = {
		let region = plan.regions.text();
		let lines = region.text[region.item..].trim_end().matches('\n').count();
		let start = region.line_of(region.item);

		format!("`{path}` ({}:{start}-{})", ws.display_path(region.file.path()).display(), start + lines)
	};

	for (index, replacement) in checked.replacements.iter().enumerate() {
		let name = old_name(index, checked.replacements.len(), &replacement.old);

		replace_text(plan.regions.text(), replacement, &name, &place).map_err(|error| PlanFailure {
			error,
			replacement: Some(index),
		})?;
	}

	if checked.is_structural() && named != item && ws.children(item).count() > 1 {
		return Err(Error::Unsupported(format!(
			"`{path}` is one of the {} imports of the `use` item at {}:{}, whose attributes, doc comment, and visibility \
			 are those of all of them: split the `use` item first",
			ws.children(item).count(),
			ws.display_path(ws.file_of(item).path()).display(),
			ws.file_of(item).line_col(data.range.start),
		))
		.into());
	}

	for (text, pattern) in &checked.remove {
		remove_attribute(&mut plan.regions, text, pattern, path)?;
	}

	for attribute in &checked.add {
		add_attribute(&mut plan.regions, attribute, path)?;
	}

	if let Some(lines) = &checked.doc {
		plan.notes.extend(set_doc(&mut plan.regions, lines, path)?);
	}

	if let Some(visibility) = &checked.visibility {
		if let Some(why) = without_visibility(ws, item) {
			return Err(Error::Unsupported(format!("cannot change the visibility of `{path}`: it is {why}")).into());
		}

		plan.notes.extend(set_visibility(&mut plan.regions, visibility.as_deref(), path)?);
	}

	if let Some(outer) = plan.regions.outer.as_ref().filter(|outer| outer.is_changed()) {
		plan.warnings = check_item(ws, item, path, outer, options.allow_kind_change)?;
	}

	Ok(plan)
}

/// `new`, for the printed text `start..end` of `printed`: its lines after the first get the indentation that the view
/// removed (but empty lines, unless text follows the last one on its line, and lines that start inside of string
/// literals), and line breaks become `line_ending`.
fn reindented(printed: &PrintedText, start: usize, end: usize, new: &str, line_ending: &str) -> String {
	let new = new.replace("\r\n", "\n");

	if !new.contains('\n') {
		return new;
	}

	// which new lines start inside of string literals, in the item as it would print
	let spliced = format!("{}{new}{}", &printed.text[..start], &printed.text[end..]);
	let strings = multiline_strings(&spliced);
	let followed = !printed.text[end..].is_empty() && !printed.text[end..].starts_with('\n');
	let lines: Vec<&str> = new.split('\n').collect();
	let mut text = String::with_capacity(new.len() + lines.len() * (printed.indentation.len() + 1));
	let mut offset = start;

	for (index, line) in lines.iter().enumerate() {
		if index > 0 {
			let last = index == lines.len() - 1;

			text.push_str(line_ending);

			if (!line.is_empty() || (last && followed)) && !is_inside(&strings, offset) {
				text.push_str(&printed.indentation);
			}
		}

		text.push_str(line);
		offset += line.len() + 1;
	}

	text
}

/// Removes the attributes that a pattern names (see [`ItemEdit::remove_attributes`]).
fn remove_attribute(regions: &mut Regions<'_>, text: &str, pattern: &AttributePattern, path: &str) -> Result<(), Error> {
	// (a crate root only has inner attributes, whichever way they are given)
	let pattern = &match pattern {
		AttributePattern::Exact { meta, .. } if regions.outer.is_none() => AttributePattern::Exact {
			meta: meta.clone(),
			inner: None,
		},

		pattern => pattern.clone(),
	};
	let mut found: Vec<(bool, Attr)> = Vec::new();
	let mut existing: Vec<String> = Vec::new();

	for inner in [false, true] {
		let Some(region) = regions.attributes(inner) else {
			continue;
		};

		let attributes = region.attributes(path)?.attributes;

		for attribute in attributes.into_iter().filter(|attribute| !attribute.doc && attribute.inner == inner) {
			existing.push(format!("`{}`", &region.text[attribute.range.as_range()]));

			if pattern.matches(&attribute) {
				found.push((inner, attribute));
			}
		}
	}

	match (found.len(), pattern) {
		(0, _) => {
			let has = match existing.is_empty() {
				true => "it has none".to_owned(),
				false => format!("it has {}", existing.join(", ")),
			};

			return Err(Error::Unsupported(format!("`{path}` has no attribute `{text}` ({has})")));
		}

		(1, _) | (_, AttributePattern::Exact { .. }) => {}

		(count, AttributePattern::Path(_)) => {
			let matches: Vec<String> = (found.iter())
				.map(|(inner, attribute)| match regions.attributes(*inner) {
					Some(region) => format!("`{}`", &region.text[attribute.range.as_range()]),
					None => String::new(),
				})
				.collect();

			return Err(Error::Unsupported(format!(
				"`{text}` is the path of {count} attributes of `{path}` ({}): give the exact text of the one to remove",
				matches.join(", ")
			)));
		}
	}

	for inner in [false, true] {
		let ranges: Vec<TextRange> = (found.iter())
			.filter(|(found, _)| *found == inner)
			.map(|(_, attribute)| attribute.range)
			.collect();

		if let Some(region) = regions.attributes(inner).filter(|_| !ranges.is_empty()) {
			let edits = (deletions(region, &ranges).into_iter())
				.map(|(range, shape)| match shape {
					Shape::Lines => (with_blank_line(region, range), String::new()),
					_ => (range, String::new()),
				})
				.collect();

			region.apply(edits);
		}
	}

	Ok(())
}

/// Replaces text in the region of an item (see [`TextReplacement`]). `name`: how to call `old` in messages; `place`:
/// the item and its lines.
fn replace_text(region: &mut Region<'_>, replacement: &TextReplacement, name: &str, place: &str) -> Result<(), Error> {
	// without line numbers first, when it has them (and as given if that fails, in case they were text)
	let mut attempts: Vec<(String, String, Option<usize>)> = (strip_gutters(&replacement.old, &replacement.new).into_iter())
		.chain([(replacement.old.clone(), replacement.new.clone(), None)])
		.collect();

	// a whole view copied with a line break at its end, which the item's text does not have
	if let Some((old, new, line)) = attempts.first().filter(|(old, _, _)| old.ends_with('\n')) {
		let trim = |text: &str| text.strip_suffix('\n').map_or(text, |text| text.strip_suffix('\r').unwrap_or(text)).to_owned();

		attempts.push((trim(old), trim(new), *line));
	}

	let mut failure = None;

	for (old, new, line) in &attempts {
		match try_replace(region, old, new, *line, name, place) {
			Ok(()) => return Ok(()),

			// (the first failure tells about the text as it was meant)
			Err(error @ Error::TextMismatch { .. }) => {
				failure.get_or_insert(error);
			}

			Err(error) => return Err(error),
		}
	}

	Err(failure.unwrap_or_else(|| unreachable!("there is at least one attempt")))
}

/// Sets the doc comment of an item (see [`ItemEdit::doc`]); returns a note when nothing changes.
fn set_doc(regions: &mut Regions<'_>, lines: &[String], path: &str) -> Result<Option<String>, Error> {
	// a module's inner docs, unless its declaration has docs (only those are removed)
	let outer_docs = match &regions.outer {
		Some(outer) => outer.attributes(path)?.attributes.into_iter().any(|attribute| attribute.doc),
		None => false,
	};
	let inner = regions.file.is_some() && !outer_docs;
	let mut changed = false;

	// removing the docs removes those of both places
	let places = match lines.is_empty() {
		true => vec![inner, !inner],
		false => vec![inner],
	};

	for inner in places {
		let Some(region) = regions.attributes(inner) else {
			continue;
		};
		let before = region.text.clone();

		set_docs_of(region, lines, path)?;
		changed |= region.text != before;
	}

	Ok((!changed).then(|| match lines.is_empty() {
		true => format!("`{path}` has no doc comment to remove"),
		false => format!("`{path}` already has this doc comment"),
	}))
}

/// Replaces the doc comments of a region (outer ones, or the inner ones of a module's file) with `lines`.
fn set_docs_of(region: &mut Region<'_>, lines: &[String], path: &str) -> Result<(), Error> {
	let prefix = region.attributes(path)?;
	let marker = if region.module_file { "//!" } else { "///" };
	let docs: Vec<TextRange> = (prefix.attributes.iter()).filter(|attribute| attribute.doc).map(|attribute| attribute.range).collect();
	let (line_ending, indent) = (region.line_ending, region.indent.clone());
	let comment = |line: &String| match line.is_empty() {
		true => marker.to_owned(),
		false => format!("{marker} {line}"),
	};

	// the block of comments for each shape of the range it replaces
	let block = |shape: Shape| -> String {
		match shape {
			Shape::Lines => lines.iter().map(|line| format!("{indent}{}{line_ending}", comment(line))).collect(),
			Shape::BeforeCode => lines.iter().map(|line| format!("{}{line_ending}{indent}", comment(line))).collect(),
			Shape::AfterCode => format!(" {}", lines.iter().map(comment).collect::<Vec<_>>().join(&format!("{line_ending}{indent}"))),
		}
	};

	let mut edits: Vec<(TextRange, String)> = Vec::new();

	match docs.is_empty() {
		// before the attributes (in a module's file, at its start)
		true if lines.is_empty() => {}

		true if region.module_file => {
			let start = shebang_len(&region.text);
			let rest = &region.text[start..];
			let separator = match rest.trim_start_matches([' ', '\t', '\r']).starts_with('\n') || rest.trim().is_empty() {
				true => "",
				false => line_ending,
			};

			edits.push((TextRange::new(start, start), format!("{}{separator}", block(Shape::Lines))));
		}

		true => edits.push((TextRange::new(region.item, region.item), block(Shape::BeforeCode))),

		// in place of the first one
		false => {
			for (index, (mut range, shape)) in deletions(region, &docs).into_iter().enumerate() {
				let replacement = match index == 0 && !lines.is_empty() {
					true => block(shape),
					false => String::new(),
				};

				if replacement.is_empty() && shape == Shape::Lines {
					range = with_blank_line(region, range);
				}

				edits.push((range, replacement));
			}
		}
	}

	region.apply(edits);
	Ok(())
}

/// Sets the visibility of an item (`None` for private); returns a note when it is unchanged.
fn set_visibility(regions: &mut Regions<'_>, visibility: Option<&str>, path: &str) -> Result<Option<String>, Error> {
	let Some(region) = regions.outer.as_mut() else {
		return Ok(None);
	};
	let prefix = region.attributes(path)?;

	if prefix.visibility_text.as_deref() == visibility {
		return Ok(Some(format!("`{path}` is {} already", visibility.unwrap_or("private"))));
	}

	let range = prefix.visibility;

	let edit = match (prefix.visibility_text, visibility) {
		(None, Some(visibility)) => (range, format!("{visibility} ")),
		(Some(_), Some(visibility)) => (range, visibility.to_owned()),
		(_, None) => (TextRange::new(range.start, range.end + spaces(&region.text[range.end..])), String::new()),
	};

	region.apply(vec![edit]);
	Ok(None)
}

/// The error for a text (named `name` in messages) that occurs several times in the text of a region (of the item at
/// `place`): where it occurs, and in which form when it occurs both as views print it and, elsewhere, as written.
fn several_occurrences(region: &Region<'_>, found: &[Occurrence], name: &str, place: &str) -> Error {
	let lines_of = |printed: Option<bool>| -> Vec<usize> {
		let mut lines: Vec<usize> = (found.iter())
			.filter(|occurrence| printed.is_none_or(|printed| occurrence.printed.is_some() == printed))
			.map(|occurrence| region.line_of(occurrence.range.start))
			.collect();

		lines.dedup();
		lines
	};
	let numbers = |lines: &[usize]| {
		let numbers: Vec<String> = lines.iter().map(ToString::to_string).collect();

		format!("line{} {}", if lines.len() == 1 { "" } else { "s" }, numbers.join(", "))
	};

	let lines = lines_of(None);
	let (printed, written) = (lines_of(Some(true)), lines_of(Some(false)));

	// (text copied from a view that also occurs as written, which gives its lines another indentation)
	let forms = match printed.is_empty() || written.is_empty() {
		true => String::new(),
		false => format!(
			": as views print it at {}, and as written in the file at {}",
			numbers(&printed),
			numbers(&written)
		),
	};

	Error::TextMismatch {
		message: format!("{name} occurs {} times in {place}, at {}{forms}", found.len(), numbers(&lines)),
		lines,
	}
}

/// The length of a shebang line (with its line break) at the start of a module's file: `#!` not followed by `[`.
fn shebang_len(text: &str) -> usize {
	match text.strip_prefix("#!") {
		Some(rest) if !rest.trim_start().starts_with('[') => (line_end(text, 0) + 1).min(text.len()),
		_ => 0,
	}
}

/// How similar two texts are, from 0 to 1 (the Dice coefficient of their pairs of adjacent characters).
fn similarity(a: &str, b: &str) -> f64 {
	let pairs = |text: &str| {
		let chars: Vec<char> = text.chars().collect();

		chars.windows(2).map(|pair| (pair[0], pair[1])).collect::<Vec<_>>()
	};
	let (a, mut b) = (pairs(a), pairs(b));
	let total = a.len() + b.len();
	let mut common = 0;

	for pair in a {
		if let Some(index) = b.iter().position(|other| *other == pair) {
			b.swap_remove(index);
			common += 1;
		}
	}

	match total {
		0 => 0.0,
		_ => 2.0 * common as f64 / total as f64,
	}
}

/// The length of the spaces and tabs at the start of a text.
fn spaces(text: &str) -> usize {
	text.len() - text.trim_start_matches([' ', '\t']).len()
}

/// `old` and `new` without the line-number gutters of a numbered view, when every line of `old` (but empty ones) has
/// one, and the line of the file where `old` starts, by its line numbers.
fn strip_gutters(old: &str, new: &str) -> Option<(String, String, Option<usize>)> {
	/// The lines of a text, without their line breaks.
	fn lines(text: &str) -> Vec<&str> {
		text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).collect()
	}

	let old_lines = lines(old);

	// (lines with nothing but a gutter are no text to look for)
	if !old_lines.iter().any(|line| gutter_len(line).is_some_and(|gutter| !line[gutter..].trim().is_empty()))
		|| !old_lines.iter().all(|line| line.is_empty() || gutter_len(line).is_some())
	{
		return None;
	}

	let strip = |text: &str| {
		(lines(text).into_iter())
			.map(|line| &line[gutter_len(line).unwrap_or(0)..])
			.collect::<Vec<_>>()
			.join("\n")
	};

	// the line of the first line, from the first line number
	let line = (old_lines.iter().enumerate()).find_map(|(index, line)| {
		let number = line.split_once(" │")?.0.trim();

		number.parse::<usize>().ok()?.checked_sub(index)
	});

	Some((strip(old), strip(new), line))
}

/// Replaces `old` with `new` in a region, where `old` occurs exactly once, as views print the region or as written in
/// the file (with its line breaks). What occurs in both forms at the same place occurs once, as printed (so that the
/// lines of `new` after the first get the indentation that the view removed); what occurs in each form at a different
/// place occurs several times. Where it occurs several times, `line` (from the line numbers of a numbered view) may
/// tell which one is meant.
fn try_replace(region: &mut Region<'_>, old: &str, new: &str, line: Option<usize>, name: &str, place: &str) -> Result<(), Error> {
	let line_ending = region.line_ending;
	let printed = PrintedText::new(&region.prefix, &region.text, &multiline_strings(&region.text));
	let old = old.replace("\r\n", "\n");
	let raw = old.replace('\n', line_ending);

	let mut found: Vec<Occurrence> = (occurrences(&printed.text, &old).into_iter())
		.map(|start| Occurrence {
			range: TextRange::new(printed.source_offset(start), printed.source_offset(start + old.len())),
			printed: Some((start, start + old.len())),
		})
		.collect();

	for start in occurrences(&region.text, &raw) {
		if !found.iter().any(|occurrence| occurrence.range.start == start) {
			found.push(Occurrence {
				range: TextRange::new(start, start + raw.len()),
				printed: None,
			});
		}
	}

	found.sort_by_key(|occurrence| occurrence.range.start);

	// the one at the line of the line numbers
	if let Some(line) = line.filter(|_| found.len() > 1) {
		let at: Vec<Occurrence> = (found.iter().copied())
			.filter(|occurrence| region.line_of(occurrence.range.start) == line)
			.collect();

		if at.len() == 1 {
			found = at;
		}
	}

	let (range, new) = match found[..] {
		[Occurrence { range, printed: Some((start, end)) }] => (range, reindented(&printed, start, end, new, line_ending)),
		[Occurrence { range, printed: None }] => (range, new.replace("\r\n", "\n").replace('\n', line_ending)),

		[] => {
			return Err(Error::TextMismatch {
				message: format!("{name} not found in {place}{}", closest(region, &printed, &old)),
				lines: Vec::new(),
			});
		}

		ref found => return Err(several_occurrences(region, found, name, place)),
	};

	region.text.replace_range(range.as_range(), &new);
	Ok(())
}

/// The error for an edit with text replacements that fits none of the items a path names (one per failure), unless
/// it is ambiguous: the failure of the item that the edit got furthest in, or that the text a replacement needs is in
/// none of the items that it got that far in (with what comes closest in any of them).
fn unfit(mut failures: Vec<PlanFailure>, replacements: &[TextReplacement], path: &ItemPath) -> Option<Error> {
	let count = failures.len();
	let furthest = failures.iter().map(|failure| failure.progress(replacements.len())).max()?;

	failures.retain(|failure| failure.progress(replacements.len()) == furthest);

	match (failures.len(), furthest) {
		(1, _) => failures.pop().map(|failure| failure.error),

		// (the same text replacement does not fit any of them)
		(_, (index, false)) => {
			// what comes closest in any of them (as the messages of their failures tell)
			let messages = (failures.iter()).filter_map(|failure| match &failure.error {
				Error::TextMismatch { message, .. } => message.split_once(" not found in ").map(|(_, rest)| rest),
				_ => None,
			});
			let closest = (messages.filter_map(|rest| Some((CLOSEST.iter().position(|kind| rest.contains(kind))?, rest))))
				.min_by_key(|(rank, _)| *rank)
				.map(|(_, rest)| format!("; the closest is in {rest}"))
				.unwrap_or_default();
			let name = old_name(index, replacements.len(), replacements.get(index).map_or("", |replacement| &replacement.old));

			Some(Error::TextMismatch {
				message: match failures.len() == count {
					true => format!("{name} not found in any of the {count} items that `{path}` names{closest}"),
					false => format!(
						"{name} not found in any of the {} items (of {count}) that `{path}` names with the text before \
						 it{closest}",
						failures.len()
					),
				},
				lines: Vec::new(),
			})
		}

		_ => None,
	}
}

/// Imports stand for their `use` items: the items to edit, and the imports named for `use` items.
fn use_items(ws: &Workspace, items: Vec<ItemId>) -> (Vec<ItemId>, Vec<(ItemId, ItemId)>) {
	let mut edited = Vec::with_capacity(items.len());
	let mut imports = Vec::new();

	for item in items {
		match ws.parent(item).filter(|_| ws.item(item).kind == ItemKind::Import) {
			Some(use_item) => {
				imports.push((use_item, item));
				edited.push(use_item);
			}

			None => edited.push(item),
		}
	}

	(edited, imports)
}

/// A visibility as written in items (`pub(in crate::a)`), `None` for none.
fn visibility_text(visibility: &syn::Visibility) -> Option<String> {
	match visibility {
		syn::Visibility::Public(_) => Some("pub".to_owned()),
		syn::Visibility::Inherited => None,

		syn::Visibility::Restricted(restricted) => {
			let segments: Vec<String> = restricted.path.segments.iter().map(|segment| segment.ident.to_string()).collect();
			let path = format!("{}{}", if restricted.path.leading_colon.is_some() { "::" } else { "" }, segments.join("::"));

			match restricted.in_token {
				Some(_) => Some(format!("pub(in {path})")),
				None => Some(format!("pub({path})")),
			}
		}
	}
}

/// A deletion of whole lines of a region's text, extended over the blank line after them when the line before them is
/// blank too (or they start a module's file), so that removing them leaves no run of blank lines.
fn with_blank_line(region: &Region<'_>, range: TextRange) -> TextRange {
	let text = region.text.as_str();

	// (the line before the text of an item is not known)
	let blank_before = match text[..range.start].strip_suffix('\n') {
		None => region.module_file && range.start == 0,
		Some(before) => before[before.rfind('\n').map_or(0, |index| index + 1)..].trim().is_empty(),
	};
	let rest = &text[range.end..];

	match rest.find('\n') {
		Some(end) if blank_before && rest[..end].trim().is_empty() => TextRange::new(range.start, range.end + end + 1),
		_ => range,
	}
}

/// Why an item has no visibility of its own to change, if it has none.
fn without_visibility(ws: &Workspace, item: ItemId) -> Option<String> {
	let data = ws.item(item);

	if item.is_crate_root() {
		return Some("a crate root, which has no visibility".to_owned());
	}

	match data.kind {
		ItemKind::Impl
		| ItemKind::MacroCall
		| ItemKind::AssocMacro
		| ItemKind::ForeignMacro
		| ItemKind::MacroRules
		| ItemKind::ExternBlock => Some(format!("{}, which has no visibility", article(data.kind))),

		_ if data.vis == Visibility::Inherited => Some(format!("{}, which has the visibility of its parent", article(data.kind))),
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::PathBuf;

	#[test]
	fn applies_text_replacements_as_written_and_as_printed() {
		let text = "impl S {\r\n\tfn f() {\r\n\t\tlet a = 1;\r\n\r\n\t\tlet b = 2;\r\n\t}\r\n}\r\n";
		let file = SourceFile::new(PathBuf::from("lib.rs"), text);
		let range = TextRange::new(11, file.text().len() - 5);
		let replace = |old: &str, new: &str| {
			let mut region = Region::item(&file, range);

			try_replace(&mut region, old, new, None, "`old`", "`f`").map(|()| region.text)
		};

		// as written: line breaks become the file's
		assert_eq!(
			replace("let a = 1;\n", "let a = 10;\n").unwrap(),
			"\tfn f() {\r\n\t\tlet a = 10;\r\n\r\n\t\tlet b = 2;\r\n\t}"
		);

		// as printed: lines after the first get the indentation the view removed
		assert_eq!(
			replace("\tlet a = 1;\n\n\tlet b = 2;\n}", "\tlet a = 1;\n\tlet b = 3;\n}").unwrap(),
			"\tfn f() {\r\n\t\tlet a = 1;\r\n\t\tlet b = 3;\r\n\t}"
		);
		assert_eq!(
			replace("fn f() {\n\tlet a", "fn g() {\n\n\tlet a").unwrap(),
			"\tfn g() {\r\n\r\n\t\tlet a = 1;\r\n\r\n\t\tlet b = 2;\r\n\t}"
		);

		// (text without line breaks occurs in both forms at the same place: as printed)
		assert_eq!(
			replace("let a = 1;", "let a = 1;\n\tlet c = 3;").unwrap(),
			"\tfn f() {\r\n\t\tlet a = 1;\r\n\t\tlet c = 3;\r\n\r\n\t\tlet b = 2;\r\n\t}"
		);

		// text after the replaced text on its line keeps its indentation (and loses what `old` had of it)
		assert_eq!(
			replace("1;\n\n", "1;\n").unwrap(),
			"\tfn f() {\r\n\t\tlet a = 1;\r\n\t\tlet b = 2;\r\n\t}"
		);
		assert_eq!(
			replace("1;\n\n\t", "1;\n").unwrap(),
			"\tfn f() {\r\n\t\tlet a = 1;\r\n\tlet b = 2;\r\n\t}"
		);

		let mismatch = |old: &str| match replace(old, "") {
			Err(Error::TextMismatch { message, lines }) => (message, lines),
			other => panic!("{other:?}"),
		};

		assert_eq!(mismatch("let"), ("`old` occurs 2 times in `f`, at lines 3, 5".to_owned(), vec![3, 5]));
		assert_eq!(mismatch("let a = 3;").0, "`old` not found in `f`; the closest line is 3: `let a = 1;`");
		assert_eq!(
			mismatch("let a = 1;\n\n    let b = 2;").0,
			"`old` not found in `f`; it matches at line 3 if indentation is ignored (the file indents with tabs, `old` \
			 with spaces)"
		);
		assert_eq!(
			mismatch("let a = 1;\nlet c").0,
			"`old` not found in `f`; its first line is at line 3, but what follows differs"
		);
	}

	#[test]
	fn tells_text_as_printed_from_text_as_written() {
		let text = "impl W {\n\tfn h(&self) {\n\t\tfor x in xs {\n\t\t\tif x {\n\t\t\t\ta();\n\t\t\t}\n\t\t}\n\t}\n}\n";
		let file = SourceFile::new(PathBuf::from("lib.rs"), text);
		let range = TextRange::new(10, text.len() - 3);
		let replace = |old: &str, new: &str| {
			let mut region = Region::item(&file, range);
			let replacement = TextReplacement {
				old: old.to_owned(),
				new: new.to_owned(),
			};

			replace_text(&mut region, &replacement, "`old`", "`h`").map(|()| region.text)
		};

		// the closing braces of the `if` and the `for` as views print them are those of the `for` and `h` as written
		assert_eq!(
			replace("\t\t}\n\t}", "\t\t}\n\t\tb();\n\t}").unwrap_err().to_string(),
			"`old` occurs 2 times in `h`, at lines 6, 7: as views print it at line 6, and as written in the file at line 7"
		);

		// the line numbers of a numbered view tell which one is meant
		assert_eq!(
			replace("   6 │ \t\t}\n   7 │ \t}", "   6 │ \t\t}\n\t\tb();\n   7 │ \t}").unwrap(),
			"\tfn h(&self) {\n\t\tfor x in xs {\n\t\t\tif x {\n\t\t\t\ta();\n\t\t\t}\n\t\t\tb();\n\t\t}\n\t}"
		);

		// (or the one as written, whose `new` is as written too)
		assert_eq!(
			replace("   7 │ \t\t}\n   8 │ \t}", "   7 │ \t\t}\n\t\tb();\n   8 │ \t}").unwrap(),
			"\tfn h(&self) {\n\t\tfor x in xs {\n\t\t\tif x {\n\t\t\t\ta();\n\t\t\t}\n\t\t}\n\t\tb();\n\t}"
		);
	}

	#[test]
	fn checks_the_parts_of_edits() {
		let checked = ItemEdit {
			remove_attributes: vec!["derive".to_owned(), "allow(dead_code)".to_owned(), "#![allow(x)]".to_owned()],
			add_attributes: vec!["derive(Debug)".to_owned(), "#![allow(x)]".to_owned()],
			doc: Some("/// A.\n///\n/// B.\n".to_owned()),
			visibility: Some(" crate ".to_owned()),
			..ItemEdit::default()
		}
		.checked()
		.unwrap();

		assert_eq!(
			checked.remove,
			[
				("derive".to_owned(), AttributePattern::Path("derive".to_owned())),
				(
					"allow(dead_code)".to_owned(),
					AttributePattern::Exact {
						meta: "allow (dead_code)".to_owned(),
						inner: None
					}
				),
				(
					"#![allow(x)]".to_owned(),
					AttributePattern::Exact {
						meta: "allow (x)".to_owned(),
						inner: Some(true)
					}
				),
			]
		);
		assert_eq!(checked.add[0].text, "#[derive(Debug)]");
		assert!(!checked.add[0].inner && checked.add[1].inner);
		assert_eq!(checked.doc.unwrap(), ["A.", "", "B."]);
		assert_eq!(checked.visibility, Some(Some("pub(crate)".to_owned())));

		let error = |edit: ItemEdit| edit.check().unwrap_err().to_string();

		assert!(error(ItemEdit::default()).starts_with("nothing to change"));
		assert!(error(ItemEdit {
			visibility: Some("public".to_owned()),
			..ItemEdit::default()
		})
		.starts_with("`public` is not a visibility"));
		assert!(error(ItemEdit {
			add_attributes: vec!["#[doc = \"a\"]".to_owned()],
			..ItemEdit::default()
		})
		.contains("is a doc comment"));
		assert!(error(ItemEdit {
			add_attributes: vec!["#[a] #[b]".to_owned()],
			..ItemEdit::default()
		})
		.contains("expected one attribute"));
		assert_eq!(
			error(ItemEdit {
				replacements: vec![TextReplacement::default()],
				..ItemEdit::default()
			}),
			"`old` is empty"
		);
	}

	#[test]
	fn deletes_whole_lines_or_up_to_code() {
		let file = SourceFile::new(PathBuf::from("lib.rs"), "\t#[a]\n\t#[b] #[c]  fn f() {}\n\t#[d] /// e\n");
		let region = Region::item(&file, TextRange::new(1, file.text().len() - 1));
		let range = |needle: &str| {
			let start = region.text.find(needle).unwrap();

			TextRange::new(start, start + needle.len())
		};
		let deleted = |needles: &[&str]| -> Vec<(&str, Shape)> {
			let ranges: Vec<TextRange> = needles.iter().map(|needle| range(needle)).collect();

			(deletions(&region, &ranges).into_iter())
				.map(|(range, shape)| (&region.text[range.as_range()], shape))
				.collect()
		};

		assert_eq!(deleted(&["#[a]"]), [("\t#[a]\n", Shape::Lines)]);
		assert_eq!(deleted(&["#[b]", "#[c]"]), [("#[b] #[c]  ", Shape::BeforeCode)]);
		assert_eq!(deleted(&["/// e"]), [(" /// e", Shape::AfterCode)]);
	}

	#[test]
	fn reads_doc_lines() {
		assert!(doc_lines("").is_empty() && doc_lines(" \n").is_empty());
		assert_eq!(doc_lines("A.\r\n\r\nB.  \n"), ["A.", "", "B."]);
		assert_eq!(doc_lines("//! A.\n//!\n//!   B."), ["A.", "", "  B."]);
		assert_eq!(doc_lines("/// A.\nB."), ["/// A.", "B."]);
	}

	#[test]
	fn reindents_new_lines() {
		let printed = PrintedText::new("", "\tfn f() {\n\t\tlet s = \"a\n  b\";\n\t}", &[TextRange::new(21, 28)]);

		assert_eq!(printed.text, "fn f() {\n\tlet s = \"a\n  b\";\n}");
		assert_eq!(printed.indentation, "\t");

		let start = printed.text.find("let").unwrap();
		let end = printed.text.find('}').unwrap();

		// lines inside of the string are kept, empty lines stay empty
		assert_eq!(
			reindented(&printed, start, end, "let s = \"a\n  c\";\n\n\tx();\n", "\r\n"),
			"let s = \"a\r\n  c\";\r\n\r\n\t\tx();\r\n\t"
		);
		assert_eq!(reindented(&printed, start, start + 3, "let\n", "\n"), "let\n\t");
	}

	#[test]
	fn strips_line_number_gutters() {
		// (`│` takes three bytes)
		assert_eq!(gutter_len("  12 │ \tx"), Some(9));
		assert_eq!(gutter_len("     │ // file"), Some(9));
		assert_eq!(gutter_len("12 │"), Some(6));
		assert_eq!(gutter_len("x │ y"), None);
		assert_eq!(
			strip_gutters("  12 │ a\n\n  14 │ \tb", "  12 │ a\n\tc"),
			Some(("a\n\n\tb".to_owned(), "a\n\tc".to_owned(), Some(12)))
		);
		assert_eq!(strip_gutters("\n     │ x\n  14 │ b", "").unwrap().2, Some(12));
		assert_eq!(strip_gutters("a\n  14 │ b", ""), None);
		assert_eq!(strip_gutters("  14 │\n  15 │", ""), None);
	}

	#[test]
	fn tells_similar_lines() {
		assert!(similarity("let a = 1;", "let a = 2;") > 0.7);
		assert!(similarity("let a = 1;", "fn main() {}") < 0.3);
		assert_eq!(similarity("", "a"), 0.0);
		assert_eq!(occurrences("aaa", "aa"), [0, 1]);
		assert_eq!(occurrences("éaé", "é"), [0, 3]);
		assert!(occurrences("a", "").is_empty());
	}
}
