//! Intra-doc links in doc comments.
//!
//! The doc comments of an item are read as Markdown lines (from the source text of sugared comments, or from
//! `#[doc = "..."]` strings without escapes, so that offsets map to the source). Outside of code blocks and code spans,
//! links whose destination looks like a path are resolved from the item's module (with `Self` as for the item's code):
//! ``[`Name`]``, `[Name]`, `[text](path)`, `[text][path]`, `[path][]`, and reference definitions `[label]: path`.
//! Disambiguators (`struct@`, `fn@`, `macro@`, ...) and suffixes (`()`, `!`) select a namespace.

use super::ReferenceKind;
use super::format_str;
use super::paths::Locals;
use super::walker::FileWalker;
use crate::model::PathRef;
use crate::model::PathSegmentRef;
use crate::resolve::Namespace;
use crate::source::TextRange;
use smol_str::SmolStr;
use std::ops::Range;
use syn::AttrStyle;
use syn::Attribute;
use syn::Expr;
use syn::ExprLit;
use syn::Lit;
use syn::Meta;

/// A path in an intra-doc link.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct DocPath {
	pub(super) leading_colon: bool,

	/// The segments, with their ranges in the link text (including any `r#`).
	pub(super) segments: Vec<DocSegment>,

	/// The namespace a disambiguator selects (`struct@`, `fn@`, `macro@`, `name()`, `name!`).
	pub(super) namespace: Option<Namespace>,
}

impl DocPath {
	/// Parses a link destination (or the text of a shortcut link) as a path: `path`, `` `path` ``, `kind@path`,
	/// `path()`, `path!`. `None` for anything else (URLs, prose, generic arguments, ...).
	pub(super) fn parse(text: &str) -> Option<Self> {
		let mut start = text.len() - text.trim_start().len();
		let mut end = text.trim_end().len();

		if start >= end {
			return None;
		}

		if end > start + 1 && text[start..].starts_with('`') && text[..end].ends_with('`') {
			start += 1;
			end -= 1;
		}

		let mut namespace = None;

		if let Some(at) = text[start..end].find('@') {
			namespace = Some(disambiguator(&text[start..start + at])?);
			start += at + 1;
		}

		let body = &text[start..end];

		if let Some(stripped) = body.strip_suffix("()") {
			namespace = Some(Namespace::Value);
			end = start + stripped.len();
		} else if let Some(stripped) = ["!()", "![]", "!{}", "!"].iter().find_map(|suffix| body.strip_suffix(suffix)) {
			namespace = Some(Namespace::Macro);
			end = start + stripped.len();
		}

		let mut path = parse_path(&text[start..end])?;

		for segment in &mut path.segments {
			segment.range = segment.range.start + start..segment.range.end + start;
		}

		path.namespace = namespace;
		Some(path)
	}
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct DocSegment {
	/// The name, without `r#`.
	pub(super) name: SmolStr,

	pub(super) range: Range<usize>,
}

/// Which doc comments of an item to read.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum DocStyle {
	/// `///` and `/** */` (and `#[doc = ...]`).
	Outer,

	/// `//!` and `/*! */` (and `#![doc = ...]`).
	Inner,
}

/// A link in a line of Markdown.
#[derive(Debug, Clone, Eq, PartialEq)]
enum Link {
	/// `[text](destination)`: the destination.
	Inline(Range<usize>),

	/// `[text][label]` or `[text][]` (empty label).
	Reference { text: Range<usize>, label: Range<usize> },

	/// `[text]`
	Shortcut(Range<usize>),
}

impl<'ws> FileWalker<'_, 'ws> {
	/// Reports the intra-doc links to targets in the doc comments among `attrs` (the attributes of one item).
	pub(super) fn doc_comments(&mut self, attrs: &[Attribute], style: DocStyle) {
		if !self.options.doc_links || self.body_depth > 0 {
			return;
		}

		let mut lines = Vec::new();

		for attr in attrs {
			if matches!(attr.style, AttrStyle::Inner(_)) == (style == DocStyle::Inner) {
				self.doc_lines(attr, &mut lines);
			}
		}

		if !lines.iter().any(|(_, line)| self.targets.mentioned_in(line)) {
			return;
		}

		for (offset, destination) in links(&lines) {
			if let Some(path) = DocPath::parse(destination) {
				self.doc_link(offset, &path);
			}
		}
	}

	/// The lines of a doc attribute with their offsets: of a sugared doc comment's source text, or of the content of a
	/// `#[doc = "..."]` string without escapes (other doc attributes cannot be mapped to the source).
	fn doc_lines(&self, attr: &Attribute, lines: &mut Vec<(usize, &'ws str)>) {
		let Meta::NameValue(meta) = &attr.meta else {
			return;
		};

		let Expr::Lit(ExprLit { lit: Lit::Str(value), .. }) = &meta.value else {
			return;
		};

		if !meta.path.is_ident("doc") {
			return;
		}

		let text: &'ws str = self.parsed.text;

		// every token of a sugared doc comment has the span of the whole comment
		let pound = self.parsed.range(attr.pound_token.span);
		let comment = text.get(pound.as_range()).unwrap_or_default().trim_end_matches('\r');

		let (content, offset) = if let Some(line) = comment.strip_prefix("///").or_else(|| comment.strip_prefix("//!")) {
			(line, pound.start + 3)
		} else if let Some(block) = comment.strip_prefix("/**").or_else(|| comment.strip_prefix("/*!")) {
			(block.strip_suffix("*/").unwrap_or(block), pound.start + 3)
		} else {
			let range = self.parsed.range(value.span());

			let Some((content, start, raw)) = text.get(range.as_range()).and_then(format_str::literal_content) else {
				return;
			};

			if !raw && content.contains('\\') {
				return;
			}

			(content, range.start + start)
		};

		let mut offset = offset;

		for line in content.split('\n') {
			lines.push((offset, line.trim_end_matches('\r')));
			offset += line.len() + 1;
		}
	}

	/// Resolves the path of a link (at `offset`) from the current module, and reports the segments naming targets.
	fn doc_link(&mut self, offset: usize, link: &DocPath) {
		let targets = self.targets;

		if !link.segments.iter().any(|segment| targets.named_str(&segment.name).is_some()) {
			return;
		}

		let path = PathRef {
			leading_colon: link.leading_colon,
			segments: (link.segments.iter())
				.map(|segment| PathSegmentRef {
					name: segment.name.clone(),
					range: TextRange::new(offset + segment.range.start, offset + segment.range.end),
					has_arguments: false,
				})
				.collect(),
		};

		let namespaces = match &link.namespace {
			Some(namespace) => std::slice::from_ref(namespace),
			None => &Namespace::ALL[..],
		};

		for &namespace in namespaces {
			let res = self.resolve_path(&path, namespace, Locals::None);

			self.report_path(&path, &res, ReferenceKind::DocLink);
		}
	}
}

/// The index of the bracket closing the one at `open`, skipping code spans and escaped characters.
fn closing(line: &str, open: usize, opening: u8, closing: u8) -> Option<usize> {
	let bytes = line.as_bytes();
	let mut depth = 0;
	let mut index = open;

	while index < bytes.len() {
		match bytes[index] {
			b'\\' => {
				index += 2;
				continue;
			}

			b'`' => {
				index = skip_code_span(line, index);
				continue;
			}

			byte if byte == opening => depth += 1,

			byte if byte == closing => {
				depth -= 1;

				if depth == 0 {
					return Some(index);
				}
			}

			_ => {}
		}

		index += 1;
	}

	None
}

/// Which lines are in fenced code blocks (the fences included).
fn code_lines(lines: &[(usize, &str)]) -> Vec<bool> {
	let mut fence: Option<(u8, usize)> = None;

	(lines.iter())
		.map(|(_, line)| {
			let trimmed = line.trim_start();

			// the leading `*` of lines in block comments
			let trimmed = trimmed.strip_prefix('*').map_or(trimmed, str::trim_start);
			let marker = fence_marker(trimmed);

			match (fence, marker) {
				(None, Some(open)) => fence = Some(open),
				(Some((char, length)), Some((closing, closing_length)))
					if closing == char && closing_length >= length && trimmed[closing_length..].trim().is_empty() =>
				{
					fence = None;
				}
				(None, None) => return false,
				(Some(_), _) => {}
			}

			true
		})
		.collect()
}

/// The destination of an inline link, without surrounding whitespace, angle brackets, and a title (`path "title"`).
fn destination(line: &str, range: Range<usize>) -> Range<usize> {
	let text = &line[range.clone()];
	let start = range.start + (text.len() - text.trim_start().len());
	let text = text.trim();
	let text = text.split_whitespace().next().unwrap_or_default();

	match text.strip_prefix('<').and_then(|inner| inner.strip_suffix('>')) {
		Some(inner) => start + 1..start + 1 + inner.len(),
		None => start..start + text.len(),
	}
}

/// The namespace of a disambiguator (`kind@`), or `None` for unknown ones (and fields, which are not items).
fn disambiguator(kind: &str) -> Option<Namespace> {
	match kind {
		"struct" | "enum" | "trait" | "union" | "mod" | "module" | "type" | "tyalias" | "typealias" | "prim" | "primitive" | "variant" => {
			Some(Namespace::Type)
		}

		"const" | "constant" | "fn" | "function" | "method" | "tymethod" | "static" | "value" => Some(Namespace::Value),
		"macro" | "derive" | "attr" => Some(Namespace::Macro),
		_ => None,
	}
}

/// A code fence: three or more backticks or tildes.
fn fence_marker(line: &str) -> Option<(u8, usize)> {
	let char = *line.as_bytes().first().filter(|&&char| char == b'`' || char == b'~')?;
	let length = line.bytes().take_while(|&byte| byte == char).count();

	(length >= 3).then_some((char, length))
}

fn is_identifier(text: &str) -> bool {
	let mut chars = text.chars();

	chars.next().is_some_and(|first| first.is_alphabetic() || first == '_') && chars.all(|char| char.is_alphanumeric() || char == '_') && text != "_"
}

/// The links in a line, skipping code spans and escaped characters.
fn line_links(line: &str) -> Vec<Link> {
	let bytes = line.as_bytes();
	let mut links = Vec::new();
	let mut index = 0;

	while index < bytes.len() {
		match bytes[index] {
			b'\\' => index += 2,
			b'`' => index = skip_code_span(line, index),

			b'[' => {
				let Some(close) = closing(line, index, b'[', b']') else {
					index += 1;
					continue;
				};

				let text = index + 1..close;
				let after = close + 1;

				match bytes.get(after) {
					Some(b'(') if let Some(end) = closing(line, after, b'(', b')') => {
						links.push(Link::Inline(destination(line, after + 1..end)));
						index = end + 1;
					}

					Some(b'[') if let Some(end) = line[after + 1..].find(']').map(|end| after + 1 + end) => {
						links.push(Link::Reference { text, label: after + 1..end });
						index = end + 1;
					}

					_ => {
						links.push(Link::Shortcut(text));
						index = after;
					}
				}
			}

			_ => index += 1,
		}
	}

	links
}

/// The link destinations (and shortcut link texts) in doc comment lines (with their offsets), outside of code blocks,
/// with the offset of each.
pub(super) fn links<'t>(lines: &[(usize, &'t str)]) -> Vec<(usize, &'t str)> {
	let code = code_lines(lines);

	// labels of reference definitions: links using them are not intra-doc links (their definitions may be)
	let labels: Vec<String> = (lines.iter().zip(&code))
		.filter(|&(_, &in_code)| !in_code)
		.filter_map(|((_, line), _)| reference_definition(line).map(|(label, _)| normalize_label(&line[label])))
		.collect();

	let is_label = |text: &str| labels.contains(&normalize_label(text));
	let mut found = Vec::new();

	for (&(offset, line), in_code) in lines.iter().zip(code) {
		if in_code {
			continue;
		}

		if let Some((_, destination)) = reference_definition(line) {
			found.push((offset + destination.start, &line[destination]));
			continue;
		}

		for link in line_links(line) {
			let destination = match link {
				Link::Inline(destination) => destination,
				Link::Reference { text, label } if label.is_empty() && !is_label(&line[text.clone()]) => text,
				Link::Reference { label, .. } if !label.is_empty() && !is_label(&line[label.clone()]) => label,
				Link::Shortcut(text) if !is_label(&line[text.clone()]) => text,
				_ => continue,
			};

			found.push((offset + destination.start, &line[destination]));
		}
	}

	found
}

/// Case-insensitive, with runs of whitespace as one space (like Markdown compares labels).
fn normalize_label(label: &str) -> String {
	label.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Parses `[::]segment(::segment)*` where a segment is an identifier (possibly raw).
fn parse_path(text: &str) -> Option<DocPath> {
	let (leading_colon, body, offset) = match text.strip_prefix("::") {
		Some(rest) => (true, rest, 2),
		None => (false, text, 0),
	};

	let mut segments = Vec::new();
	let mut position = offset;

	for part in body.split("::") {
		let bare = part.strip_prefix("r#").unwrap_or(part);

		if !is_identifier(bare) || (bare.len() != part.len() && matches!(bare, "crate" | "self" | "super" | "Self")) {
			return None;
		}

		segments.push(DocSegment {
			name: SmolStr::new(bare),
			range: position..position + part.len(),
		});

		position += part.len() + 2;
	}

	Some(DocPath {
		leading_colon,
		segments,
		namespace: None,
	})
}

/// `[label]: destination`: the ranges of the label and the destination.
fn reference_definition(line: &str) -> Option<(Range<usize>, Range<usize>)> {
	let start = line.len() - line.trim_start().len();

	if !line[start..].starts_with('[') {
		return None;
	}

	let close = closing(line, start, b'[', b']')?;

	if line.as_bytes().get(close + 1) != Some(&b':') {
		return None;
	}

	let destination = destination(line, close + 2..line.len());

	(!destination.is_empty()).then_some((start + 1..close, destination))
}

/// The index after the code span starting at `start` (a run of backticks closed by a run of the same length), or after
/// the backticks when they are not closed.
fn skip_code_span(line: &str, start: usize) -> usize {
	let bytes = line.as_bytes();
	let length = bytes[start..].iter().take_while(|&&byte| byte == b'`').count();
	let mut index = start + length;

	while index < bytes.len() {
		if bytes[index] == b'`' {
			let run = bytes[index..].iter().take_while(|&&byte| byte == b'`').count();

			if run == length {
				return index + run;
			}

			index += run;
		} else {
			index += 1;
		}
	}

	start + length
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The candidates found in lines (all at offset 0 + 100 * line number).
	fn candidates(lines: &[&str]) -> Vec<String> {
		let lines: Vec<(usize, &str)> = lines.iter().enumerate().map(|(index, line)| (index * 100, *line)).collect();

		links(&lines).into_iter().map(|(offset, text)| format!("{offset}:{text}")).collect()
	}

	#[test]
	fn finds_links() {
		assert_eq!(
			candidates(&[" See [`Foo`] and [Bar](crate::Bar), [text][baz::Qux]."]),
			["6:`Foo`", "23:crate::Bar", "43:baz::Qux"]
		);
		assert_eq!(candidates(&[" [Foo][] and [a `[b]` c] `[not]`"]), ["2:Foo", "14:a `[b]` c"]);
		assert_eq!(candidates(&[" [x]( <a::B> \"title\" ) \\[escaped]"]), ["7:a::B"]);
		assert_eq!(candidates(&[" [nested [inner]] text"]), ["2:nested [inner]"]);
	}

	#[test]
	fn parses_link_paths() {
		let parse = |text: &str| {
			DocPath::parse(text).map(|path| {
				let segments: Vec<String> = path
					.segments
					.iter()
					.map(|segment| format!("{}@{:?}", segment.name, segment.range))
					.collect();

				(path.leading_colon, segments.join(" "), path.namespace)
			})
		};

		assert_eq!(parse("Foo"), Some((false, "Foo@0..3".to_owned(), None)));
		assert_eq!(
			parse(" `crate::a::Foo` "),
			Some((false, "crate@2..7 a@9..10 Foo@12..15".to_owned(), None))
		);
		assert_eq!(parse("struct@Foo"), Some((false, "Foo@7..10".to_owned(), Some(Namespace::Type))));
		assert_eq!(parse("`fn@foo()`"), Some((false, "foo@4..7".to_owned(), Some(Namespace::Value))));
		assert_eq!(parse("mac!"), Some((false, "mac@0..3".to_owned(), Some(Namespace::Macro))));
		assert_eq!(parse("::dep::r#type"), Some((true, "dep@2..5 type@7..13".to_owned(), None)));
		assert_eq!(parse("Self::new"), Some((false, "Self@0..4 new@6..9".to_owned(), None)));

		let invalid = [
			"https://example.com",
			"a b",
			"Vec<T>",
			"field@x",
			"x@y",
			"a::",
			"::",
			"1a",
			"r#crate",
			"a.b",
			"#anchor",
		];
		let empty = ["", " ", "  ", "`", "``", " ` ", "@"];

		for invalid in invalid.into_iter().chain(empty) {
			assert_eq!(parse(invalid), None, "{invalid}");
		}
	}

	#[test]
	fn reference_definitions_replace_their_links() {
		let found = candidates(&[
			" See [the docs] and [Foo].",
			"",
			" [the docs]: crate::Docs",
			"[other]: https://example.com",
		]);

		assert_eq!(found, ["21:Foo", "213:crate::Docs", "309:https://example.com"]);
	}

	#[test]
	fn skips_code_blocks() {
		// a fence closes with at least as many of its characters
		let lines = [
			" [A]", " ````", " [B]", " ```", " [C]", " ````", " [D]", "~~~text", "[E]", "~~~", " * ```", " * [F]", " * ```", "[G]",
		];
		let found = candidates(&lines);

		assert_eq!(found, ["2:A", "602:D", "1301:G"]);
	}
}
