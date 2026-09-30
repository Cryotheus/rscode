//! Snippets: a region of a source file with edits applied (elided bodies, deleted doc comments), as lines that
//! remember the line of the file they come from.
//!
//! The first line of a region usually starts mid-line, after its indentation; it is given the indentation of its
//! line, so that all lines of a snippet are indented as in the file (and [`Snippet::dedent`] can remove the common
//! indentation). Line breaks (`\n` or `\r\n`) become `\n`.

use crate::edit::trivia::line_indent;
use crate::load::thread_local;
use crate::source::ParsedFile;
use crate::source::SourceFile;
use crate::source::TextRange;
use crate::source::isolated;
use proc_macro2::Delimiter;
use proc_macro2::Span;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use quote::ToTokens;
use std::cmp::Reverse;
use syn::visit::Visit;

/// A change of a snippet's text, in offsets of its file.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum Edit {
	/// Replaces a range (an elided body) with text, which must not contain line breaks.
	Replace(TextRange, &'static str),

	/// Deletes a range (a doc comment), along with the line it is on (indentation and line break) when nothing else
	/// is on it, or else with the spaces between it and the code next to it.
	Delete(TextRange),
}

impl Edit {
	fn range(self) -> TextRange {
		match self {
			Self::Replace(range, _) | Self::Delete(range) => range,
		}
	}

	fn replacement(self) -> &'static str {
		match self {
			Self::Replace(_, replacement) => replacement,
			Self::Delete(_) => "",
		}
	}

	fn shifted(self, shift: impl Fn(usize) -> usize) -> Self {
		let shift_range = |range: TextRange| TextRange::new(shift(range.start), shift(range.end));

		match self {
			Self::Replace(range, replacement) => Self::Replace(shift_range(range), replacement),
			Self::Delete(range) => Self::Delete(shift_range(range)),
		}
	}
}

/// Finds the initializers of `const`s and `static`s that span lines.
struct Initializers<'p, 'a> {
	parsed: &'p ParsedFile<'a>,
	ranges: Vec<TextRange>,
}

impl Initializers<'_, '_> {
	/// Adds `= initializer;` (without a `;` for the last declaration of a `thread_local!`, which may have none).
	fn add(&mut self, eq: Span, expr: &syn::Expr, semi: Option<Span>) {
		let expr = self.parsed.range_of(expr);

		if self.parsed.text.get(expr.as_range()).is_some_and(|text| text.contains('\n')) {
			let end = semi.map_or(expr, |semi| self.parsed.range(semi));

			self.ranges.push(self.parsed.range(eq).cover(end));
		}
	}
}

impl<'ast> Visit<'ast> for Initializers<'_, '_> {
	// items in blocks (function bodies) are never outlined
	fn visit_block(&mut self, _: &'ast syn::Block) {}

	fn visit_impl_item_const(&mut self, item: &'ast syn::ImplItemConst) {
		self.add(item.eq_token.span, &item.expr, Some(item.semi_token.span));
	}

	fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
		self.add(item.eq_token.span, &item.expr, Some(item.semi_token.span));
	}

	// the statics of a `thread_local!` (in modules: blocks are skipped)
	fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
		for declaration in thread_local::declarations(&item.mac).into_iter().flatten() {
			self.add(declaration.eq_token.span, &declaration.expr, declaration.semi_token.map(|semi| semi.span));
		}
	}

	fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
		self.add(item.eq_token.span, &item.expr, Some(item.semi_token.span));
	}

	fn visit_trait_item_const(&mut self, item: &'ast syn::TraitItemConst) {
		if let Some((eq, expr)) = &item.default {
			self.add(eq.span, expr, Some(item.semi_token.span));
		}
	}
}

/// A line of a [`Snippet`].
#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct Line {
	/// The (1-based) line of the file that the line's first character comes from, or `None` for a line that does
	/// not come from the file (a header).
	pub(super) number: Option<usize>,

	/// The text, without a line break.
	pub(super) text: String,

	/// Whether the line starts inside of a string literal, so that its leading whitespace belongs to the string.
	pub(super) verbatim: bool,
}

impl Line {
	/// A line that does not come from the file.
	pub(super) fn synthetic(text: String) -> Self {
		Self {
			number: None,
			text,
			verbatim: false,
		}
	}

	fn is_blank(&self) -> bool {
		!self.verbatim && self.text.trim().is_empty()
	}
}

/// Lines of a region of a source file, with edits applied.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub(super) struct Snippet {
	pub(super) lines: Vec<Line>,
}

impl Snippet {
	/// The lines of `region` of `file` with `edits` applied (edits that are not inside of the region are ignored),
	/// the first line prefixed with the indentation of its line. `strings`: string literals spanning lines, sorted.
	///
	/// Edits inside of other edits are ignored, as are replacements overlapping others; overlapping deletions are
	/// merged. Blank lines at the start and end are removed.
	pub(super) fn new(file: &SourceFile, region: TextRange, edits: &[Edit], strings: &[TextRange]) -> Self {
		let text = file.text();
		let region = clamp(text, region);
		let prefix = first_line_indent(text, region.start);
		let working = format!("{prefix}{}", &text[region.as_range()]);
		let edits: Vec<Edit> = edits
			.iter()
			.filter(|edit| region.contains_range(edit.range()))
			.map(|edit| edit.shifted(|offset| offset - region.start + prefix.len()))
			.collect();

		let lines = split_lines(&working, &normalize(&working, edits))
			.into_iter()
			.map(|(anchor, text)| {
				// the indentation put before the first line counts as the start of the region
				let offset = region.start + anchor.saturating_sub(prefix.len());

				Line {
					number: Some(file.line_col(offset).line),
					text,
					verbatim: is_inside(strings, offset),
				}
			})
			.collect();

		let mut snippet = Self { lines };

		snippet.trim_blank_lines();
		snippet
	}

	/// Replaces runs of blank lines with one blank line.
	pub(super) fn collapse_blank_lines(&mut self) {
		let mut previous_blank = false;

		self.lines.retain(|line| {
			let blank = line.is_blank();
			let keep = !(blank && previous_blank);

			previous_blank = blank;
			keep
		});
	}

	/// Removes the indentation common to all lines (but those starting inside of string literals), and empties blank
	/// lines.
	pub(super) fn dedent(&mut self) {
		let common = (self.lines.iter())
			.filter(|line| !line.is_blank() && !line.verbatim)
			.map(|line| indentation(&line.text))
			.reduce(common_prefix)
			.map_or(0, str::len);

		for line in &mut self.lines {
			if line.is_blank() {
				line.text.clear();
			} else if !line.verbatim {
				line.text.drain(..common);
			}
		}
	}

	/// The lines joined with `\n` (no line break at the end), each prefixed with its line number (right-aligned, then
	/// ` │ `) if `line_numbers` is set.
	pub(super) fn render(&self, line_numbers: bool) -> String {
		if !line_numbers {
			return self.lines.iter().map(|line| line.text.as_str()).collect::<Vec<_>>().join("\n");
		}

		let width = (self.lines.iter().filter_map(|line| line.number).max())
			.map_or(1, |number| number.to_string().len())
			.max(4);

		(self.lines.iter())
			.map(|line| {
				let number = line.number.map(|number| number.to_string()).unwrap_or_default();

				match line.text.is_empty() {
					true => format!("{number:>width$} │"),
					false => format!("{number:>width$} │ {}", line.text),
				}
			})
			.collect::<Vec<_>>()
			.join("\n")
	}

	/// Removes blank lines at the start and the end.
	fn trim_blank_lines(&mut self) {
		let end = self.lines.iter().rposition(|line| !line.is_blank()).map_or(0, |index| index + 1);

		self.lines.truncate(end);

		let start = self.lines.iter().position(|line| !line.is_blank()).unwrap_or(self.lines.len());

		self.lines.drain(..start);
	}
}

/// Collects lines of text for [`split_lines`].
#[derive(Default)]
struct Splitter {
	lines: Vec<(usize, String)>,
	text: String,

	/// Where the first character of the current line comes from.
	anchor: Option<usize>,

	/// Where the current line starts.
	line_start: usize,
}

impl Splitter {
	fn append(&mut self, at: usize, text: &str) {
		if !text.is_empty() {
			self.anchor.get_or_insert(at);
			self.text.push_str(text);
		}
	}

	/// Ends the current line; the next one starts at `next`.
	fn end_line(&mut self, next: usize) {
		let mut text = std::mem::take(&mut self.text);

		if text.ends_with('\r') {
			text.pop();
		}

		self.lines.push((self.anchor.take().unwrap_or(self.line_start), text));
		self.line_start = next;
	}

	/// The lines; the text after the last line break is a line unless it is empty.
	fn finish(mut self) -> Vec<(usize, String)> {
		if self.anchor.is_some() {
			self.end_line(self.line_start);
		}

		self.lines
	}

	/// Adds `working[start..end]`.
	fn original(&mut self, working: &str, start: usize, end: usize) {
		let mut position = start;

		for (index, _) in working[start..end].match_indices('\n') {
			let line_break = start + index;

			self.append(position, &working[position..line_break]);
			self.end_line(line_break + 1);
			position = line_break + 1;
		}

		self.append(position, &working[position..end]);
	}
}

/// What rendering needs to know about the syntax of a file, as plain data (`syn` spans are only valid on the thread
/// that parsed the file). All ranges are sorted.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub(super) struct Syntax {
	/// Doc attributes, outer and inner (`///`, `//!`, `/** */`, `/*! */`, `#[doc = ...]`, `#![doc = ...]`), including
	/// those inside of macro invocations.
	pub(super) docs: Vec<TextRange>,

	/// String literals (including byte and C strings, raw or not) that span lines.
	pub(super) strings: Vec<TextRange>,

	/// The `= initializer;` of `const`s and `static`s whose initializer spans lines (outside of blocks).
	pub(super) initializers: Vec<TextRange>,
}

impl Syntax {
	/// Parses a file's text (on a thread of its own, see [`isolated`]). A text that does not parse has no known
	/// syntax.
	pub(super) fn of(text: &str) -> Self {
		isolated(|| Self::parse(text))
	}

	fn parse(text: &str) -> Self {
		let Ok(parsed) = ParsedFile::parse(text) else {
			return Self::default();
		};

		let mut syntax = Self::default();
		let mut initializers = Initializers {
			parsed: &parsed,
			ranges: Vec::new(),
		};

		syntax.scan_tokens(&parsed, parsed.file.to_token_stream());
		initializers.visit_file(&parsed.file);
		syntax.initializers = initializers.ranges;

		syntax.docs.sort();
		syntax.strings.sort();
		syntax.initializers.sort();
		syntax
	}

	/// Finds doc attributes and multi-line string literals in tokens.
	fn scan_tokens(&mut self, parsed: &ParsedFile<'_>, tokens: TokenStream) {
		let tokens: Vec<TokenTree> = tokens.into_iter().collect();

		for (index, token) in tokens.iter().enumerate() {
			match token {
				TokenTree::Group(group) => {
					if let Some(pound) = doc_attribute_start(&tokens[..index], group) {
						self.docs.push(parsed.range(pound).cover(parsed.range(group.span())));
					}

					self.scan_tokens(parsed, group.stream());
				}

				TokenTree::Literal(literal) => {
					let range = parsed.range(literal.span());
					let text = parsed.text.get(range.as_range()).unwrap_or_default();

					if is_string_literal(text) && text.contains('\n') {
						self.strings.push(range);
					}
				}

				TokenTree::Ident(_) | TokenTree::Punct(_) => {}
			}
		}
	}
}

/// A range clamped to the text and to character boundaries.
fn clamp(text: &str, range: TextRange) -> TextRange {
	let floor = |mut offset: usize| {
		offset = offset.min(text.len());

		while !text.is_char_boundary(offset) {
			offset -= 1;
		}

		offset
	};
	let start = floor(range.start);

	TextRange::new(start, floor(range.end).max(start))
}

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
	let length = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();

	&a[..length]
}

/// For a `[...]` group that is the brackets of a doc attribute (`#[doc = ...]` or `#![doc = ...]`, as doc comments
/// are lexed), the span of its `#`; `preceding` are the tokens before the group.
fn doc_attribute_start(preceding: &[TokenTree], group: &proc_macro2::Group) -> Option<Span> {
	if group.delimiter() != Delimiter::Bracket {
		return None;
	}

	let mut inside = group.stream().into_iter();

	match (inside.next(), inside.next()) {
		(Some(TokenTree::Ident(ident)), Some(TokenTree::Punct(eq))) if ident == "doc" && eq.as_char() == '=' => {}
		_ => return None,
	}

	match preceding {
		[.., pound, bang] if is_punct(pound, '#') && is_punct(bang, '!') => Some(pound.span()),
		[.., pound] if is_punct(pound, '#') => Some(pound.span()),
		_ => None,
	}
}

/// A deletion extended to the whole lines it is on (with their line break) when nothing else is on them; else to the
/// spaces separating it from the code after it, or (with only spaces after it) from the code before it.
fn extend_deletion(working: &str, range: TextRange) -> TextRange {
	let line_start = working[..range.start].rfind('\n').map_or(0, |index| index + 1);
	let line_end = working[range.end..].find('\n').map_or(working.len(), |index| range.end + index);
	let before = &working[line_start..range.start];
	let after = &working[range.end..line_end];
	let blank = |text: &str| text.bytes().all(|byte| is_space(byte) || byte == b'\r');

	match (blank(before), blank(after)) {
		(true, true) => TextRange::new(line_start, (line_end + 1).min(working.len())),
		(_, false) => TextRange::new(range.start, range.end + after.len() - after.trim_start_matches([' ', '\t']).len()),
		(false, true) => TextRange::new(range.start - (before.len() - before.trim_end_matches([' ', '\t']).len()), line_end),
	}
}

/// The text to put before a region: the text between the start of its line and its start when that is only
/// indentation, else the indentation of the line.
fn first_line_indent(text: &str, start: usize) -> &str {
	let bom = if text.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
	let line_start = text[..start].rfind('\n').map_or(bom.min(start), |index| index + 1);
	let before = &text[line_start..start];

	match before.bytes().all(|byte| matches!(byte, b' ' | b'\t')) {
		true => before,
		false => line_indent(text, start),
	}
}

/// The leading spaces and tabs of a line.
fn indentation(line: &str) -> &str {
	&line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

/// Whether `offset` is strictly inside of one of the (sorted, disjoint) `ranges`.
fn is_inside(ranges: &[TextRange], offset: usize) -> bool {
	let index = ranges.partition_point(|range| range.start < offset);

	index > 0 && offset < ranges[index - 1].end
}

fn is_punct(token: &TokenTree, char: char) -> bool {
	matches!(token, TokenTree::Punct(punct) if punct.as_char() == char)
}

fn is_space(byte: u8) -> bool {
	matches!(byte, b' ' | b'\t')
}

/// Whether the source text of a literal is a string (`"..."`, `r#"..."#`, `b"..."`, `br"..."`, `c"..."`, `cr"..."`).
fn is_string_literal(text: &str) -> bool {
	let rest = text.strip_prefix(['b', 'c']).unwrap_or(text);
	let rest = rest.strip_prefix('r').unwrap_or(rest);

	rest.trim_start_matches('#').starts_with('"')
}

/// Sorts edits and resolves overlaps: deletions separated by nothing but spaces are merged and then extended to their
/// lines ([`extend_deletion`]), edits inside of others are dropped, and so are replacements overlapping earlier edits.
fn normalize(working: &str, edits: Vec<Edit>) -> Vec<Edit> {
	let mut deletions: Vec<TextRange> = (edits.iter())
		.filter_map(|edit| match edit {
			Edit::Delete(range) => Some(*range),
			Edit::Replace(..) => None,
		})
		.collect();

	deletions.sort();

	let mut merged: Vec<TextRange> = Vec::with_capacity(deletions.len());

	for range in deletions {
		match merged.last_mut() {
			Some(last) if range.start <= last.end || working[last.end..range.start].bytes().all(is_space) => {
				last.end = last.end.max(range.end);
			}

			_ => merged.push(range),
		}
	}

	let mut edits: Vec<Edit> = (edits.into_iter())
		.filter(|edit| matches!(edit, Edit::Replace(..)))
		.chain(merged.into_iter().map(|range| Edit::Delete(extend_deletion(working, range))))
		.collect();

	edits.sort_by_key(|edit| (edit.range().start, Reverse(edit.range().end)));

	let mut disjoint: Vec<Edit> = Vec::with_capacity(edits.len());

	for edit in edits {
		match disjoint.last_mut() {
			Some(Edit::Delete(last)) if edit.range().start < last.end => {
				if let Edit::Delete(range) = edit {
					last.end = last.end.max(range.end);
				}
			}

			Some(last) if edit.range().start < last.range().end => {}
			_ => disjoint.push(edit),
		}
	}

	disjoint
}

/// Applies sorted, disjoint edits to `working` and splits the result into lines, each with the offset in `working` of
/// its first character (for text from an edit: of the edit), or of its start when it is empty.
fn split_lines(working: &str, edits: &[Edit]) -> Vec<(usize, String)> {
	let mut splitter = Splitter::default();
	let mut position = 0;

	for edit in edits {
		let range = edit.range();

		splitter.original(working, position, range.start);
		splitter.append(range.start, edit.replacement());
		position = range.end;
	}

	splitter.original(working, position, working.len());
	splitter.finish()
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::PathBuf;

	#[test]
	fn clamps_regions() {
		let text = "é\nb";
		let source = file(text);

		assert_eq!(clamp(text, TextRange::new(1, 99)), TextRange::new(0, 4));
		assert_eq!(Snippet::new(&source, TextRange::new(3, 99), &[], &[]).render(true), "   2 │ b");
		assert_eq!(Snippet::new(&source, TextRange::new(9, 99), &[], &[]).render(false), "");
	}

	#[test]
	fn collapses_blank_lines() {
		let mut snippet = Snippet {
			lines: ["a", "", " ", "b", "", "c", "\t", ""]
				.iter()
				.map(|text| Line::synthetic((*text).to_owned()))
				.collect(),
		};

		snippet.collapse_blank_lines();
		snippet.trim_blank_lines();
		assert_eq!(snippet.render(false), "a\n\nb\n\nc");
	}

	#[test]
	fn dedents_blank_and_mixed_lines() {
		let mut snippet = Snippet {
			lines: ["\t\ta", "\t  ", "", "\t\t\tb", "\t c", "   d"]
				.iter()
				.map(|text| Line::synthetic((*text).to_owned()))
				.collect(),
		};

		snippet.dedent();
		assert_eq!(snippet.render(false), "\t\ta\n\n\n\t\t\tb\n\t c\n   d");

		snippet.lines.remove(5);
		snippet.dedent();
		assert_eq!(snippet.render(false), "\ta\n\n\n\t\tb\n c");
	}

	#[test]
	fn deletes_doc_attributes_sharing_lines() {
		let text = concat!(
			"#[doc = \"a\"] #[doc = \"b\"]\n",
			"fn a() {}\n",
			"\n",
			"#[doc = \"c\"] #[cfg(x)] fn b() {}\n",
			"#[cfg(y)] #[doc = \"d\"]  \n",
			"fn c() {}\r\n",
			"//! inner\r\n",
			"fn d() {}",
		);
		let source = file(text);

		// not a valid file (`//!` after items), so the ranges are given
		let deletions: Vec<Edit> = ["#[doc = \"a\"]", "#[doc = \"b\"]", "#[doc = \"c\"]", "#[doc = \"d\"]", "//! inner"]
			.iter()
			.map(|needle| Edit::Delete(range_of(text, needle)))
			.collect();

		let snippet = Snippet::new(&source, TextRange::new(0, text.len()), &deletions, &[]);

		assert_eq!(snippet.render(false), "fn a() {}\n\n#[cfg(x)] fn b() {}\n#[cfg(y)]\nfn c() {}\nfn d() {}");
		assert_eq!(
			snippet.lines.iter().map(|line| line.number.unwrap()).collect::<Vec<_>>(),
			[2, 3, 4, 5, 6, 8]
		);
	}

	#[test]
	fn deletes_doc_comments_with_their_lines() {
		let text = "mod m {\n\t/// Docs.\n\t/// More.\n\t#[inline] /** a */ fn a() {} /// b\n\tfn b() {}\n}";
		let source = file(text);
		let syntax = Syntax::of(text);

		assert_eq!(syntax.docs.len(), 4);

		let deletions: Vec<Edit> = syntax.docs.iter().map(|&range| Edit::Delete(range)).collect();
		let snippet = Snippet::new(&source, region(text, "/// Docs.", "fn a() {}"), &deletions, &[]);

		assert_eq!(lines(&snippet), [(Some(4), "\t#[inline] fn a() {}", false)]);

		let snippet = Snippet::new(&source, region(text, "/// b", "fn b() {}"), &deletions, &[]);

		assert_eq!(lines(&snippet), [(Some(5), "\tfn b() {}", false)]);

		let snippet = Snippet::new(&source, TextRange::new(0, text.len()), &deletions, &[]);

		assert_eq!(snippet.render(false), "mod m {\n\t#[inline] fn a() {}\n\tfn b() {}\n}");
	}

	fn file(text: &str) -> SourceFile {
		SourceFile::new(PathBuf::from("test.rs"), text)
	}

	#[test]
	fn finds_doc_attributes_everywhere() {
		let text = concat!(
			"//! Inner.\n",
			"#![doc = \"more\"]\n",
			"\n",
			"/** Block\n",
			" */\n",
			"struct S {\n",
			"\t/// Field.\n",
			"\tf: u8,\n",
			"}\n",
			"m! {\n",
			"\t/// In a macro.\n",
			"\tfn x() {}\n",
			"}\n",
			"#[doc(hidden)]\n",
			"#[cfg_attr(x, doc = \"no\")]\n",
			"fn f() {\n",
			"\t/// Local.\n",
			"\tfn g() {}\n",
			"\tlet s = r\"/// not\n",
			"\";\n",
			"}\n",
		);
		let syntax = Syntax::of(text);
		let found: Vec<&str> = syntax.docs.iter().map(|range| &text[range.as_range()]).collect();

		assert_eq!(
			found,
			[
				"//! Inner.",
				"#![doc = \"more\"]",
				"/** Block\n */",
				"/// Field.",
				"/// In a macro.",
				"/// Local."
			]
		);
		assert_eq!(syntax.strings.len(), 1);
		assert_eq!(Syntax::of("fn {"), Syntax::default());
	}

	#[test]
	fn finds_initializers() {
		let text = concat!(
			"const A: u8 = 1;\n",
			"const B: [u8; 2] = [\n",
			"\t1,\n",
			"\t2,\n",
			"];\n",
			"static C: &str = \"a\n",
			"b\";\n",
			"impl X {\n",
			"\tconst D: u8 = {\n",
			"\t\t1\n",
			"\t};\n",
			"}\n",
			"trait T {\n",
			"\tconst E: u8 = 1\n",
			"\t\t+ 1;\n",
			"\tconst F: u8;\n",
			"}\n",
			"fn f() {\n",
			"\tconst G: u8 = {\n",
			"\t\t1\n",
			"\t};\n",
			"}\n",
			"thread_local! {\n",
			"\tstatic H: u8 = {\n",
			"\t\t1\n",
			"\t};\n",
			"\tstatic I: u8 = 1;\n",
			"\tstatic J: u8 = {\n",
			"\t\t1\n",
			"\t}\n",
			"}\n",
		);
		let syntax = Syntax::of(text);
		let found: Vec<&str> = syntax.initializers.iter().map(|range| &text[range.as_range()]).collect();

		assert_eq!(
			found,
			[
				"= [\n\t1,\n\t2,\n];",
				"= \"a\nb\";",
				"= {\n\t\t1\n\t};",
				"= 1\n\t\t+ 1;",
				"= {\n\t\t1\n\t};",
				"= {\n\t\t1\n\t}",
			]
		);
		assert_eq!(syntax.strings.len(), 1);
	}

	#[test]
	fn indents_the_first_line_and_numbers_lines() {
		let text = "mod m {\n\tfn a() {\n\t\tx();\n\t}\n}\n";
		let source = file(text);
		let snippet = Snippet::new(&source, region(text, "fn a", "\t}"), &[], &[]);

		assert_eq!(
			lines(&snippet),
			[(Some(2), "\tfn a() {", false), (Some(3), "\t\tx();", false), (Some(4), "\t}", false)]
		);

		let mut dedented = snippet.clone();

		dedented.dedent();
		assert_eq!(dedented.render(false), "fn a() {\n\tx();\n}");
		assert_eq!(dedented.render(true), "   2 │ fn a() {\n   3 │ \tx();\n   4 │ }");
	}

	#[test]
	fn items_after_code_get_the_indentation_of_their_line() {
		let text = "  struct A; struct B {\n    b: u8,\n  }\n";
		let source = file(text);
		let mut snippet = Snippet::new(&source, region(text, "struct B", "  }"), &[], &[]);

		snippet.dedent();
		assert_eq!(snippet.render(false), "struct B {\n  b: u8,\n}");
	}

	#[test]
	fn keeps_string_continuation_lines() {
		let text = "impl A {\n\tfn a() {\n\t\tlet s = \"x\n  y\n\";\n\t\tlet r = r#\"\n\t\t\"#;\n\t}\n}";
		let source = file(text);
		let syntax = Syntax::of(text);

		assert_eq!(syntax.strings.len(), 2);

		let mut snippet = Snippet::new(&source, region(text, "fn a", "\t}"), &[], &syntax.strings);

		snippet.dedent();
		assert_eq!(
			lines(&snippet),
			[
				(Some(2), "fn a() {", false),
				(Some(3), "\tlet s = \"x", false),
				(Some(4), "  y", true),
				(Some(5), "\";", true),
				(Some(6), "\tlet r = r#\"", false),
				(Some(7), "\t\t\"#;", true),
				(Some(8), "}", false),
			]
		);

		// without knowing about the strings, the continuation lines limit the dedent
		let mut snippet = Snippet::new(&source, region(text, "fn a", "\t}"), &[], &[]);

		snippet.dedent();
		assert_eq!(snippet.lines[0].text, "\tfn a() {");
	}

	fn lines(snippet: &Snippet) -> Vec<(Option<usize>, &str, bool)> {
		snippet
			.lines
			.iter()
			.map(|line| (line.number, line.text.as_str(), line.verbatim))
			.collect()
	}

	#[test]
	fn nested_and_overlapping_edits() {
		let text = "0123456789";
		let working = text;
		let edits = vec![
			Edit::Replace(TextRange::new(2, 8), "a"),
			Edit::Replace(TextRange::new(3, 4), "b"),
			Edit::Replace(TextRange::new(7, 9), "c"),
			Edit::Delete(TextRange::new(8, 9)),
			Edit::Delete(TextRange::new(0, 1)),
			Edit::Delete(TextRange::new(1, 2)),
		];

		assert_eq!(
			normalize(working, edits),
			[
				Edit::Delete(TextRange::new(0, 2)),
				Edit::Replace(TextRange::new(2, 8), "a"),
				Edit::Delete(TextRange::new(8, 9))
			]
		);
	}

	fn range_of(text: &str, needle: &str) -> TextRange {
		let start = text.find(needle).unwrap_or_else(|| panic!("`{needle}` not in `{text}`"));

		TextRange::new(start, start + needle.len())
	}

	#[test]
	fn ranges_account_for_byte_order_marks_and_shebangs() {
		for text in [
			"#!/usr/bin/env run-cargo-script\n//! Doc.\nfn main() {\n\tlet s = \"a\nb\";\n}\n",
			"\u{feff}//! Doc.\nfn main() {\n\tlet s = \"a\nb\";\n}\n",
			"\u{feff}#!/usr/bin/env run-cargo-script\r\n//! Doc.\r\nfn main() {\r\n\tlet s = \"a\r\nb\";\r\n}\r\n",
		] {
			let syntax = Syntax::of(text);
			let slice = |range: &TextRange| text[range.as_range()].trim_end_matches('\r');

			assert_eq!(syntax.docs.iter().map(slice).collect::<Vec<_>>(), ["//! Doc."], "{text:?}");
			assert_eq!(syntax.strings.len(), 1, "{text:?}");
			assert!(slice(&syntax.strings[0]).starts_with("\"a") && slice(&syntax.strings[0]).ends_with("b\""));
		}
	}

	#[test]
	fn recognizes_string_literals() {
		for text in ["\"a\"", "r\"a\"", "r#\"a\"#", "b\"a\"", "br##\"a\"##", "c\"a\"", "cr\"a\""] {
			assert!(is_string_literal(text), "{text}");
		}

		for text in ["'a'", "b'a'", "1", "/// doc", "/** doc */", "1u8"] {
			assert!(!is_string_literal(text), "{text}");
		}
	}

	/// The region from the start of `from` to the end of `to`.
	fn region(text: &str, from: &str, to: &str) -> TextRange {
		range_of(text, from).cover(range_of(text, to))
	}

	#[test]
	fn renders_line_numbers() {
		let snippet = Snippet {
			lines: vec![
				Line::synthetic("// header".to_owned()),
				Line {
					number: Some(9),
					text: "a".to_owned(),
					verbatim: false,
				},
				Line {
					number: Some(10),
					text: String::new(),
					verbatim: false,
				},
				Line {
					number: Some(12345),
					text: "b".to_owned(),
					verbatim: false,
				},
			],
		};

		assert_eq!(snippet.render(true), "      │ // header\n    9 │ a\n   10 │\n12345 │ b");
		assert_eq!(Snippet::default().render(true), "");
	}

	#[test]
	fn replaces_ranges() {
		let text = "impl A {\n\tfn a() {\n\t\tx();\n\t} // after\n\n\tfn b()\n\t{\n\t}\n}";
		let source = file(text);
		let body_a = Edit::Replace(region(text, "{\n\t\tx", "\t}"), "{ ... }");
		let body_b = Edit::Replace(range_of(text, "{\n\t}"), "{ ... }");
		let snippet = Snippet::new(&source, TextRange::new(0, text.len()), &[body_a], &[]);

		// the line after an elision is numbered by where it starts
		assert_eq!(
			snippet.render(true),
			concat!(
				"   1 │ impl A {\n",
				"   2 │ \tfn a() { ... } // after\n",
				"   5 │\n",
				"   6 │ \tfn b()\n",
				"   7 │ \t{\n",
				"   8 │ \t}\n",
				"   9 │ }",
			)
		);

		// the lines of an elided range become one
		let snippet = Snippet::new(&source, region(text, "fn b", "\t}\n}"), &[body_a, body_b], &[]);

		assert_eq!(
			lines(&snippet),
			[(Some(6), "\tfn b()", false), (Some(7), "\t{ ... }", false), (Some(9), "}", false)]
		);
	}

	#[test]
	fn whole_files_keep_their_first_line() {
		let text = "\u{feff}  fn a() {}\r\n\r\nfn b() {}\r\n\r\n";
		let source = file(text);
		let snippet = Snippet::new(&source, TextRange::new(3, text.len()), &[], &[]);

		// blank lines at the end are dropped, and line breaks become `\n`
		assert_eq!(
			lines(&snippet),
			[(Some(1), "  fn a() {}", false), (Some(2), "", false), (Some(3), "fn b() {}", false)]
		);
	}
}
