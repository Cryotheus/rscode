//! Comments and whitespace around items, which `syn` spans do not cover.
//!
//! These helpers find the text that belongs to an item beyond its syntax (the comments attached to it and the
//! whitespace around it) for removals, and lay out new text for insertions and replacements. They work on source
//! text with a small lexer that tells code, string literals, comments, and whitespace apart, so the contents of
//! strings and comments are never mistaken for anything else. Doc comments count as code: they are attributes of the
//! item that follows them, and part of its range.
//!
//! - [`removal_ranges`], [`list_item_removal_range`]: what to delete to remove items.
//! - [`insertion`]: the edit inserting new items before or after a sibling, or at the end of a container.
//! - [`reindent`], [`line_indent`], [`body_indent`], [`indent_unit`], [`line_ending`]: layout of new text.
//!
//! Offsets are expected at token boundaries (item ranges of the model are). Offsets past the end of the text or
//! inside of characters are clamped, so no input panics. Line breaks are `\n` or `\r\n`, and new text uses the line
//! break style of the text it goes into.

use crate::edit::TextEdit;
use crate::path::is_whitespace;
use crate::path::word_len;
use crate::source::TextRange;
use std::borrow::Cow;

/// The trivia after an offset, up to the code after it.
struct After<'a> {
	/// The start of the code after (the end of the text when there is none).
	ceiling: usize,

	/// The first character of the code after.
	ceiling_char: Option<char>,

	pieces: &'a [Piece],
}

/// The trivia before an offset, back to the code before it.
struct Before<'a> {
	/// The end of the code before (the start of the text when there is none).
	floor: usize,

	/// The last character of the code before.
	floor_char: Option<char>,

	pieces: &'a [Piece],
}

/// What borders a run of lines.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Edge {
	/// The start or end of the text, or the opening or closing delimiter of the container.
	Container,

	/// Anything else: code or comments.
	Content,
}

/// The kind of a [`Piece`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Kind {
	/// Whitespace other than line breaks.
	Space,

	/// `\n` or `\r\n`.
	LineBreak,

	/// A comment that is not a doc comment.
	Comment,

	/// A string literal, which may span lines.
	Str,

	/// Any other token, a doc comment, or a shebang line.
	Code,
}

impl Kind {
	fn is_trivia(self) -> bool {
		matches!(self, Self::Space | Self::LineBreak | Self::Comment)
	}
}

/// The lines above an item, as far as they belong to it.
#[derive(Debug, Clone, Copy)]
struct Leading {
	/// Whether only whitespace and comments precede the item on its line.
	starts_line: bool,

	/// The start of the comment lines attached above the item, or of the item's line (the item itself when it does
	/// not start its line).
	block_start: usize,

	/// The start of the blank lines directly above `block_start` (equal to it when there are none).
	blank_start: usize,

	/// What is above those blank lines.
	above: Edge,

	/// The start of the whitespace right before the item.
	space_start: usize,

	/// The end of the code before the item (the start of the text when there is none).
	floor: usize,

	/// The last character of the code before the item.
	floor_char: Option<char>,
}

impl Leading {
	fn has_blank_above(&self) -> bool {
		self.blank_start < self.block_start
	}
}

/// A lexed text. Adjacent code pieces (including string literals) are merged, and so are adjacent spaces.
struct Lexed<'a> {
	text: &'a str,

	/// Where the text starts, after a byte order mark.
	start: usize,

	pieces: Vec<Piece>,
}

impl<'a> Lexed<'a> {
	fn new(text: &'a str) -> Self {
		let mut pieces: Vec<Piece> = Vec::new();

		scan_file(text, |mut piece| {
			if piece.kind == Kind::Str {
				piece.kind = Kind::Code;
			}

			match pieces.last_mut() {
				Some(last) if last.kind == piece.kind && matches!(piece.kind, Kind::Code | Kind::Space) => last.end = piece.end,
				_ => pieces.push(piece),
			}

			true
		});

		Self {
			text,
			start: bom_len(text),
			pieces,
		}
	}

	fn after(&self, offset: usize) -> After<'_> {
		let start = self.pieces.partition_point(|piece| piece.start < offset);

		// inside of a piece: the text after the offset counts as code
		if start > 0 && self.pieces[start - 1].end > offset {
			return After {
				ceiling: offset,
				ceiling_char: self.text[offset..].chars().next(),
				pieces: &[],
			};
		}

		let mut end = start;

		while end < self.pieces.len() && self.pieces[end].kind.is_trivia() {
			end += 1;
		}

		let (ceiling, ceiling_char) = match self.pieces.get(end) {
			Some(code) => (code.start, self.text[code.start..].chars().next()),
			None => (self.text.len(), None),
		};

		After {
			ceiling,
			ceiling_char,
			pieces: &self.pieces[start..end],
		}
	}

	fn before(&self, offset: usize) -> Before<'_> {
		let end = self.pieces.partition_point(|piece| piece.end <= offset);

		// inside of a piece: the text before the offset counts as code
		if self.pieces.get(end).is_some_and(|piece| piece.start < offset) {
			return Before {
				floor: offset,
				floor_char: self.text[..offset].chars().next_back(),
				pieces: &[],
			};
		}

		let mut first = end;

		while first > 0 && self.pieces[first - 1].kind.is_trivia() {
			first -= 1;
		}

		let (floor, floor_char) = match first.checked_sub(1) {
			Some(code) => {
				let floor = self.pieces[code].end;

				(floor, self.text[..floor].chars().next_back())
			}
			None => (self.start, None),
		};

		Before {
			floor,
			floor_char,
			pieces: &self.pieces[first..end],
		}
	}

	/// Pieces overlapping a range.
	fn pieces_in(&self, range: TextRange) -> &[Piece] {
		let first = self.pieces.partition_point(|piece| piece.end <= range.start);
		let last = self.pieces.partition_point(|piece| piece.start < range.end);

		&self.pieces[first..last.max(first)]
	}
}

/// A line of trivia.
#[derive(Debug, Clone, Copy)]
struct Line {
	start: usize,

	/// Where the line's line break starts (or the end of the trivia).
	end: usize,

	/// The end of the line's line break, if it has one.
	next: Option<usize>,

	has_comment: bool,
}

/// A piece of source text.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct Piece {
	kind: Kind,
	start: usize,
	end: usize,
}

/// Where [`insertion`] puts new items.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum Placement {
	/// Before an item (the range of a sibling), above the comments attached to it.
	Before(TextRange),

	/// After an item (the range of a sibling), below the comment trailing it on its line.
	After(TextRange),

	/// At the end of a container body (the range inside its braces, or the whole text of a file), after everything in
	/// it. For a body without items, this is also the start.
	End(TextRange),
}

/// A line of source text, without its line break.
#[derive(Debug, Clone, Copy)]
struct SourceLine<'a> {
	text: &'a str,

	/// Whether the line starts inside of a string literal.
	verbatim: bool,
}

/// The lines below an item, as far as they belong to it.
#[derive(Debug, Clone, Copy)]
struct Trailing {
	/// Whether only whitespace and comments follow the item on its line.
	ends_line: bool,

	/// Where the line break ending the item's line starts (the end of the text when there is none).
	break_start: usize,

	/// The end of the item's line, after its line break.
	line_end: usize,

	/// The end of the blank lines directly below the item's line (equal to `line_end` when there are none).
	blank_end: usize,

	/// What is below those blank lines.
	below: Edge,

	/// The end of the whitespace right after the item.
	space_end: usize,
}

impl Trailing {
	fn has_blank_below(&self) -> bool {
		self.blank_end > self.line_end
	}
}

/// The length of the run of ASCII whitespace other than line breaks at the start of `bytes`.
fn ascii_space_len(bytes: &[u8]) -> usize {
	let mut length = 0;

	while let Some(&byte) = bytes.get(length) {
		match byte {
			b' ' | b'\t' | 0x0b | 0x0c => length += 1,
			b'\r' if bytes.get(length + 1) != Some(&b'\n') => length += 1,
			_ => break,
		}
	}

	length
}

/// The length of a (possibly nested) `/* */` comment; unterminated comments extend to the end.
fn block_comment_len(rest: &str) -> usize {
	let bytes = rest.as_bytes();
	let mut depth = 0_usize;
	let mut index = 0;

	while index + 1 < bytes.len() {
		match (bytes[index], bytes[index + 1]) {
			(b'/', b'*') => {
				depth += 1;
				index += 2;
			}
			(b'*', b'/') => {
				depth -= 1;
				index += 2;

				if depth == 0 {
					return index;
				}
			}
			_ => index += 1,
		}
	}

	rest.len()
}

/// The indentation of items inside of a container `body` (the text inside its braces, or a whole file): that of the
/// first line in the body that starts with code or a comment, else the indentation of the line where the body starts
/// plus [`indent_unit`]. Items of a whole file are not indented.
pub(crate) fn body_indent(text: &str, body: TextRange) -> String {
	let lexed = Lexed::new(text);
	let body = clamp(text, body);

	if body.end == text.len() {
		return String::new();
	}

	// the start of the current line, while only whitespace has been seen on it
	let mut line_start = None;

	for piece in lexed.pieces_in(body) {
		match piece.kind {
			Kind::LineBreak => line_start = Some(piece.end),
			Kind::Space => {}
			_ => match line_start {
				Some(start) if piece.start >= body.start => return text[start..piece.start].to_owned(),
				_ => line_start = None,
			},
		}
	}

	format!("{}{}", line_indent(text, body.start), indent_unit_in(&lexed))
}

fn bom_len(text: &str) -> usize {
	if text.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 }
}

/// Clamps a range to the text (after a byte order mark) and to character boundaries.
fn clamp(text: &str, range: TextRange) -> TextRange {
	let start = floor_boundary(text, range.start).max(bom_len(text));
	let end = floor_boundary(text, range.end).max(start);

	TextRange { start, end }
}

/// Classifies a comment of `length` at the start of `rest`: doc comments count as code.
fn comment(rest: &str, length: usize) -> (Kind, usize) {
	let text = &rest[..length];
	let doc = (text.starts_with("///") && !text.starts_with("////"))
		|| text.starts_with("//!")
		|| (text.starts_with("/**") && !text.starts_with("/***") && text != "/**/")
		|| text.starts_with("/*!");

	(if doc { Kind::Code } else { Kind::Comment }, length)
}

/// The indentation to remove from every (non-blank, non-verbatim) line.
fn common_indent<'a>(lines: &[SourceLine<'a>]) -> &'a str {
	fn indent_of(text: &str) -> &str {
		&text[..text.len() - text.trim_start_matches([' ', '\t']).len()]
	}

	let Some((first, others)) = lines.split_first() else {
		return "";
	};
	let first_indent = indent_of(first.text);
	let others_indent = others
		.iter()
		.filter(|line| !line.verbatim && !line.text.trim().is_empty())
		.map(|line| indent_of(line.text))
		.reduce(common_prefix);

	match others_indent {
		None => first_indent,
		Some(others_indent) if first_indent.is_empty() => others_indent,
		Some(others_indent) => common_prefix(first_indent, others_indent),
	}
}

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
	let length = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();

	&a[..length]
}

/// Converts whole levels of indentation from one unit to another, keeping any remainder.
fn convert_indent<'a>(indentation: &'a str, from: &str, to: &str) -> Cow<'a, str> {
	if from == to || from.is_empty() {
		return Cow::Borrowed(indentation);
	}

	let mut rest = indentation;
	let mut levels = 0;

	while let Some(after) = rest.strip_prefix(from) {
		rest = after;
		levels += 1;
	}

	Cow::Owned(format!("{}{rest}", to.repeat(levels)))
}

/// The largest character boundary at or before `offset`, within the text.
fn floor_boundary(text: &str, offset: usize) -> usize {
	let mut offset = offset.min(text.len());

	while !text.is_char_boundary(offset) {
		offset -= 1;
	}

	offset
}

/// One level of indentation in the style of a text: a tab if more lines are indented with tabs than with spaces,
/// else the smallest indentation in spaces (at most 8), and 4 spaces when nothing is indented.
///
/// Only lines that start with code or a comment count (not lines inside of strings or block comments).
pub(crate) fn indent_unit(text: &str) -> String {
	indent_unit_in(&Lexed::new(text))
}

fn indent_unit_in(lexed: &Lexed<'_>) -> String {
	let mut tab_lines = 0_usize;
	let mut space_lines = 0_usize;
	let mut min_spaces: Option<usize> = None;

	// the whitespace at the start of the current line, while nothing else has been seen on it
	let mut line_start = true;
	let mut indentation = "";

	for piece in &lexed.pieces {
		match piece.kind {
			Kind::LineBreak => {
				line_start = true;
				indentation = "";
			}
			Kind::Space if line_start => indentation = &lexed.text[piece.start..piece.end],
			Kind::Code | Kind::Comment if line_start => {
				if indentation.starts_with('\t') {
					tab_lines += 1;
				} else if indentation.starts_with(' ') {
					let spaces = indentation.bytes().take_while(|&byte| byte == b' ').count();

					space_lines += 1;
					min_spaces = Some(min_spaces.map_or(spaces, |min| min.min(spaces)));
				}

				line_start = false;
			}
			_ => line_start = false,
		}
	}

	match min_spaces {
		Some(spaces) if space_lines >= tab_lines => " ".repeat(spaces.clamp(1, 8)),
		_ if tab_lines > 0 => "\t".to_owned(),
		_ => "    ".to_owned(),
	}
}

fn insert_after(lexed: &Lexed<'_>, sibling: TextRange, item: &str, indent: &str, line_ending: &str) -> (TextRange, String) {
	let trailing = trailing(lexed, sibling.end);

	if !trailing.ends_line {
		// code follows the sibling on its line: move that code to a line of its own
		let replacement = format!("{line_ending}{line_ending}{item}{line_ending}{line_ending}{indent}");

		return (TextRange::new(sibling.end, trailing.space_end), replacement);
	}

	let separator = if trailing.below == Edge::Content && !trailing.has_blank_below() {
		line_ending
	} else {
		""
	};
	let at = trailing.break_start;

	(TextRange::new(at, at), format!("{line_ending}{line_ending}{item}{separator}"))
}

fn insert_at_end(lexed: &Lexed<'_>, body: TextRange, item: &str, line_ending: &str) -> (TextRange, String) {
	let text = lexed.text;

	// a braced body ends before its `}`
	let is_file = body.end == text.len();
	let closing_indent = line_indent(text, body.start);
	// the last content inside of the body (a merged piece may start with the `{` or end with the `}`)
	let content = lexed
		.pieces_in(body)
		.iter()
		.rev()
		.find(|piece| !matches!(piece.kind, Kind::Space | Kind::LineBreak) && piece.end.min(body.end) > piece.start.max(body.start));

	let Some(content) = content else {
		let replacement = if is_file {
			format!("{item}{line_ending}")
		} else {
			format!("{line_ending}{item}{line_ending}{closing_indent}")
		};

		return (body, replacement);
	};

	let content_end = content.end.min(body.end);
	let rest = TextRange::new(content_end, body.end);

	if lexed.pieces_in(rest).iter().any(|piece| piece.kind == Kind::LineBreak) {
		return (TextRange::new(content_end, content_end), format!("{line_ending}{line_ending}{item}"));
	}

	let replacement = if is_file {
		format!("{line_ending}{line_ending}{item}{line_ending}")
	} else {
		format!("{line_ending}{line_ending}{item}{line_ending}{closing_indent}")
	};

	(rest, replacement)
}

fn insert_before(lexed: &Lexed<'_>, sibling: TextRange, item: &str, indent: &str, line_ending: &str) -> (TextRange, String) {
	let leading = leading(lexed, sibling.start);

	if !leading.starts_line {
		// code precedes the sibling on its line: move the sibling to a line of its own
		let replacement = format!("{line_ending}{line_ending}{item}{line_ending}{line_ending}{indent}");

		return (TextRange::new(leading.space_start, sibling.start), replacement);
	}

	let separator = if leading.above == Edge::Content && !leading.has_blank_above() {
		line_ending
	} else {
		""
	};
	let at = leading.block_start;

	(TextRange::new(at, at), format!("{separator}{item}{line_ending}{line_ending}"))
}

/// The edit inserting the items of `source` into `text` at `placement`, indented with `indent` (see [`reindent`]
/// and [`body_indent`]).
///
/// The new items are separated from the item they are placed next to by a blank line, and from their other
/// neighbor by a blank line unless it is the start or end of the container, keeping any blank lines already there.
/// An empty braced body gets the items on their own lines, with the closing brace on its own line.
pub(crate) fn insertion(text: &str, placement: Placement, source: &str, indent: &str) -> TextEdit {
	let lexed = Lexed::new(text);
	let line_ending = line_ending(text);
	let item = format!("{indent}{}", reindent_with(source, indent, line_ending, &indent_unit_in(&lexed)));

	let (range, replacement) = match placement {
		Placement::Before(sibling) => insert_before(&lexed, clamp(text, sibling), &item, indent, line_ending),
		Placement::After(sibling) => insert_after(&lexed, clamp(text, sibling), &item, indent, line_ending),
		Placement::End(body) => insert_at_end(&lexed, clamp(text, body), &item, line_ending),
	};

	TextEdit { range, replacement }
}

fn is_closing_delimiter(c: char) -> bool {
	matches!(c, '}' | ')' | ']')
}

fn is_opening_delimiter(c: char) -> bool {
	matches!(c, '{' | '(' | '[')
}

fn leading(lexed: &Lexed<'_>, offset: usize) -> Leading {
	let before = lexed.before(offset);
	let space_start = match before.pieces.last() {
		Some(piece) if piece.kind == Kind::Space => piece.start,
		_ => offset,
	};
	let lines = lines(before.pieces, before.floor, offset);

	// the first line is the rest of the line with the code before, unless there is no code before
	let whole_first = before.floor_char.is_none();
	let is_whole = |index: usize| index > 0 || whole_first;
	let last = lines.len() - 1;
	let mut leading = Leading {
		starts_line: is_whole(last),
		block_start: offset,
		blank_start: offset,
		above: Edge::Content,
		space_start,
		floor: before.floor,
		floor_char: before.floor_char,
	};

	if !leading.starts_line {
		return leading;
	}

	let mut top = last;

	while top > 0 && is_whole(top - 1) && lines[top - 1].has_comment {
		top -= 1;
	}

	let mut blank_top = top;

	while blank_top > 0 && is_whole(blank_top - 1) && !lines[blank_top - 1].has_comment {
		blank_top -= 1;
	}

	leading.block_start = lines[top].start;
	leading.blank_start = lines[blank_top].start;
	leading.above = match blank_top {
		0 => Edge::Container,
		1 if !whole_first && before.floor_char.is_some_and(is_opening_delimiter) => Edge::Container,
		_ => Edge::Content,
	};

	leading
}

/// The length of a `//` comment, without its line break.
fn line_comment_len(rest: &str) -> usize {
	match rest.find('\n') {
		Some(index) if rest[..index].ends_with('\r') => index - 1,
		Some(index) => index,
		None => rest.len(),
	}
}

/// The line break used by a text: `"\r\n"` if its first line break is one, else `"\n"`.
pub(crate) fn line_ending(text: &str) -> &'static str {
	match text.find('\n') {
		Some(index) if text[..index].ends_with('\r') => "\r\n",
		_ => "\n",
	}
}

/// The indentation (leading spaces and tabs) of the line containing `offset`.
pub(crate) fn line_indent(text: &str, offset: usize) -> &str {
	let offset = floor_boundary(text, offset);
	let line_start = text[..offset].rfind('\n').map_or(bom_len(text), |index| index + 1);
	let line = &text[line_start..];

	&line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

/// Splits trivia from `start` to `end` into lines. Line breaks inside of block comments do not split lines. The last
/// line has no line break.
fn lines(pieces: &[Piece], start: usize, end: usize) -> Vec<Line> {
	let mut lines = Vec::new();
	let mut line = Line {
		start,
		end,
		next: None,
		has_comment: false,
	};

	for piece in pieces {
		match piece.kind {
			Kind::LineBreak => {
				lines.push(Line {
					end: piece.start,
					next: Some(piece.end),
					..line
				});
				line = Line {
					start: piece.end,
					end,
					next: None,
					has_comment: false,
				};
			}
			Kind::Comment => line.has_comment = true,
			_ => {}
		}
	}

	lines.push(line);
	lines
}

/// The range to delete to remove an element of a comma-separated list, such as an enum variant or a leaf of a `use`
/// group, including its separating comma.
///
/// - With a comma after the element, the element and that comma are removed like an item ([`removal_ranges`]):
///   `{A, B, C}` → `{A, C}`, and an element on its own lines goes with those lines.
/// - Otherwise (the last element without a trailing comma), an element on the same line as the preceding comma is
///   removed with that comma (`{A, B}` → `{A}`), and an element starting its own line is removed like an item,
///   leaving the preceding comma as a trailing comma.
pub(crate) fn list_item_removal_range(text: &str, item: TextRange) -> TextRange {
	let lexed = Lexed::new(text);
	let item = clamp(text, item);
	let after = lexed.after(item.end);

	if after.ceiling_char == Some(',') {
		return removal_range_in(&lexed, TextRange::new(item.start, after.ceiling + 1));
	}

	let leading = leading(&lexed, item.start);

	if !leading.starts_line && leading.floor_char == Some(',') {
		return TextRange::new(leading.floor - 1, item.end);
	}

	removal_range_in(&lexed, item)
}

/// The kind and length of the piece at the start of `rest`. The length is 0 only for an empty `rest`.
fn next_piece(rest: &str) -> (Kind, usize) {
	let bytes = rest.as_bytes();

	match bytes {
		[] => (Kind::Space, 0),
		[b'\n', ..] => (Kind::LineBreak, 1),
		[b'\r', b'\n', ..] => (Kind::LineBreak, 2),
		[b' ' | b'\t' | b'\r' | 0x0b | 0x0c, ..] => (Kind::Space, ascii_space_len(bytes)),
		[b'/', b'/', ..] => comment(rest, line_comment_len(rest)),
		[b'/', b'*', ..] => comment(rest, block_comment_len(rest)),
		[b'"', ..] => (Kind::Str, quoted_len(bytes, 0)),
		[b'\'', ..] => (Kind::Code, quote_len(rest)),
		[b'b' | b'c' | b'r', ..] => prefixed_literal(rest).unwrap_or((Kind::Code, word_len(rest))),
		[byte, ..] if byte.is_ascii_alphanumeric() || *byte == b'_' => (Kind::Code, word_len(rest)),
		[byte, ..] if byte.is_ascii() => (Kind::Code, 1),
		_ => {
			let c = rest.chars().next().unwrap_or(' ');

			if is_whitespace(c) {
				(Kind::Space, c.len_utf8())
			} else {
				(Kind::Code, word_len(rest).max(c.len_utf8()))
			}
		}
	}
}

/// A literal with a prefix: `b"..."`, `c"..."`, `b'x'`, and raw strings (`r"..."`, `r#"..."#`, `br`, `cr`).
fn prefixed_literal(rest: &str) -> Option<(Kind, usize)> {
	let bytes = rest.as_bytes();
	let (prefix, raw) = match bytes {
		[b'b' | b'c', b'r', ..] => (2, true),
		[b'r', ..] => (1, true),
		[b'b' | b'c', ..] => (1, false),
		_ => return None,
	};

	if !raw {
		return match bytes.get(prefix) {
			Some(b'"') => Some((Kind::Str, quoted_len(bytes, prefix))),
			Some(b'\'') if bytes[0] == b'b' => Some((Kind::Code, prefix + quote_len(&rest[prefix..]))),
			_ => None,
		};
	}

	let hashes = bytes[prefix..].iter().take_while(|&&byte| byte == b'#').count();

	if bytes.get(prefix + hashes) != Some(&b'"') {
		return None;
	}

	let mut search = prefix + hashes + 1;

	while let Some(index) = rest[search..].find('"') {
		let quote = search + index;

		if bytes
			.get(quote + 1..quote + 1 + hashes)
			.is_some_and(|closing| closing.iter().all(|&byte| byte == b'#'))
		{
			return Some((Kind::Str, quote + 1 + hashes));
		}

		search = quote + 1;
	}

	Some((Kind::Str, rest.len()))
}

/// The length of a character literal or lifetime at the start of `rest` (which starts with `'`).
fn quote_len(rest: &str) -> usize {
	let mut chars = rest[1..].chars();

	match chars.next() {
		None => 1,
		Some('\\') => {
			// an escaped character: the next quote on the line closes the literal
			let body = 2 + rest[2..].chars().next().map_or(0, char::len_utf8);

			match rest[body..].find(['\'', '\n']) {
				Some(index) if rest.as_bytes()[body + index] == b'\'' => body + index + 1,
				_ => body,
			}
		}
		Some(c) if chars.next() == Some('\'') => 1 + c.len_utf8() + 1,
		Some(_) => 1 + word_len(&rest[1..]),
	}
}

/// The end of a quoted string whose opening quote is at `open`; unterminated strings extend to the end.
fn quoted_len(bytes: &[u8], open: usize) -> usize {
	let mut index = open + 1;

	while index < bytes.len() {
		match bytes[index] {
			b'\\' => index += 2,
			b'"' => return index + 1,
			_ => index += 1,
		}
	}

	bytes.len()
}

/// Lays out the source of items for the indentation `indent` in `text`:
/// - blank lines at the start and end are removed, and other blank lines are emptied;
/// - the common indentation of the lines is removed (if the first line has none while all others do, as when the
///   first line was copied from the middle of a line, the others' common indentation is removed);
/// - every line but the first is indented with `indent` (the first line goes where the caller puts it);
/// - indentation within the source is converted to the [`indent_unit`] of `text` (e.g. 4 spaces to a tab);
/// - line breaks become those of `text` ([`line_ending`]).
///
/// Lines that start inside of a string literal are kept exactly as they are, since their whitespace is part of the
/// string.
pub(crate) fn reindent(source: &str, indent: &str, text: &str) -> String {
	reindent_with(source, indent, line_ending(text), &indent_unit(text))
}

fn reindent_with(source: &str, indent: &str, line_ending: &str, unit: &str) -> String {
	let source = source.strip_prefix('\u{feff}').unwrap_or(source);
	let lines = source_lines(source);
	let is_blank = |line: &SourceLine<'_>| !line.verbatim && line.text.trim().is_empty();
	let (Some(first), Some(last)) = (
		lines.iter().position(|line| !is_blank(line)),
		lines.iter().rposition(|line| !is_blank(line)),
	) else {
		return String::new();
	};
	let lines = &lines[first..=last];
	let common = common_indent(lines);

	// (text, verbatim) with the common indentation removed
	let dedented: Vec<(&str, bool)> = lines
		.iter()
		.map(|line| match line.verbatim {
			true => (line.text, true),
			false if line.text.trim().is_empty() => ("", false),
			false => (
				line.text
					.strip_prefix(common)
					.unwrap_or_else(|| line.text.trim_start_matches([' ', '\t'])),
				false,
			),
		})
		.collect();

	// the source's own indentation unit, measured without its common indentation
	let source_unit = indent_unit(&dedented.iter().map(|&(line, _)| line).collect::<Vec<_>>().join("\n"));
	let mut text = String::with_capacity(source.len() + lines.len() * (indent.len() + line_ending.len()));

	for (index, &(line, verbatim)) in dedented.iter().enumerate() {
		if index > 0 {
			text.push_str(line_ending);
		}

		if verbatim || line.is_empty() {
			text.push_str(line);
			continue;
		}

		let content = line.trim_start_matches([' ', '\t']);

		if index > 0 {
			text.push_str(indent);
		}

		text.push_str(&convert_indent(&line[..line.len() - content.len()], &source_unit, unit));
		text.push_str(content);
	}

	text
}

fn removal_range_in(lexed: &Lexed<'_>, item: TextRange) -> TextRange {
	let leading = leading(lexed, item.start);
	let trailing = trailing(lexed, item.end);

	if !trailing.ends_line {
		return TextRange::new(item.start, trailing.space_end);
	}

	if !leading.starts_line {
		return TextRange::new(leading.space_start, item.end);
	}

	let blank_above = leading.has_blank_above();
	let blank_below = trailing.has_blank_below();

	match (leading.above, trailing.below) {
		(Edge::Container, Edge::Container) => TextRange::new(leading.blank_start, trailing.blank_end),
		(Edge::Container, Edge::Content) => TextRange::new(leading.block_start, trailing.blank_end),
		(Edge::Content, Edge::Container) => TextRange::new(leading.blank_start, trailing.line_end),
		(Edge::Content, Edge::Content) if blank_above && blank_below => TextRange::new(leading.block_start, trailing.blank_end),
		(Edge::Content, Edge::Content) => TextRange::new(leading.block_start, trailing.line_end),
	}
}

/// The ranges to delete to remove items of one text, each extended by its attached trivia:
/// - comment lines directly above it (no blank line in between; `//`, `/* */`, and multi-line block comments, but
///   not the trailing comment of the code before them) and the indentation of its first line;
/// - trailing spaces, a comment on the same line as its end, and one line break (`\n` or `\r\n`);
/// - one of the runs of blank lines around it, so that no two blank lines (nor a blank line at the start or end of
///   the container) are left where it was: the run below it if there are blank lines on both sides or it is the first
///   thing in its container, the run above it if it is the last. If it is the only thing in its container, both.
///
/// An item that shares its line with other code keeps that code: with code before it on the line, only the spaces
/// between that code and the item are removed (and not the trailing comment and line break); with code after it,
/// the item and the spaces after it are removed (and not the indentation and comments above).
///
/// Items separated only by trivia are removed as one block (so the blank lines around the block are handled once, and
/// the comments between them go too), and items inside of other items are ignored. Returns disjoint ranges sorted by
/// position. Removing them removes the items' lines without leaving an empty line behind.
pub(crate) fn removal_ranges(text: &str, items: &[TextRange]) -> Vec<TextRange> {
	let lexed = Lexed::new(text);
	let mut items: Vec<TextRange> = items.iter().map(|&item| clamp(text, item)).collect();

	items.sort_by_key(|item| (item.start, std::cmp::Reverse(item.end)));

	let mut blocks: Vec<TextRange> = Vec::with_capacity(items.len());

	for item in items {
		match blocks.last_mut() {
			Some(block) if item.end <= block.end => {}
			Some(block) if item.start <= block.end || lexed.after(block.end).ceiling >= item.start => block.end = item.end,
			_ => blocks.push(item),
		}
	}

	blocks.into_iter().map(|block| removal_range_in(&lexed, block)).collect()
}

/// Lexes a whole text (after a byte order mark), calling `visit` with every piece in order until it returns `false`.
fn scan_file(text: &str, mut visit: impl FnMut(Piece) -> bool) {
	let mut position = bom_len(text);
	let shebang = shebang_len(&text[position..]);

	if shebang > 0 {
		if !visit(Piece {
			kind: Kind::Code,
			start: position,
			end: position + shebang,
		}) {
			return;
		}

		position += shebang;
	}

	while position < text.len() {
		let (kind, length) = next_piece(&text[position..]);

		if length == 0 {
			break;
		}

		let piece = Piece {
			kind,
			start: position,
			end: position + length,
		};

		if !visit(piece) {
			return;
		}

		position = piece.end;
	}
}

/// The length of a shebang line (`#!` not followed by `[`, which would start an inner attribute), without its line
/// break.
fn shebang_len(text: &str) -> usize {
	let Some(after) = text.strip_prefix("#!") else {
		return 0;
	};
	let mut position = 0;

	while position < after.len() {
		let (kind, length) = next_piece(&after[position..]);

		if !kind.is_trivia() || length == 0 {
			break;
		}

		position += length;
	}

	if after[position..].starts_with('[') {
		return 0;
	}

	match text.find('\n') {
		Some(index) if text[..index].ends_with('\r') => index - 1,
		Some(index) => index,
		None => text.len(),
	}
}

fn source_lines(source: &str) -> Vec<SourceLine<'_>> {
	let mut verbatim_starts = Vec::new();

	scan_file(source, |piece| {
		if piece.kind == Kind::Str {
			let string = &source[piece.start..piece.end];

			verbatim_starts.extend(string.match_indices('\n').map(|(index, _)| piece.start + index + 1));
		}

		true
	});

	let mut lines = Vec::new();
	let mut offset = 0;

	for raw in source.split('\n') {
		lines.push(SourceLine {
			text: raw.strip_suffix('\r').unwrap_or(raw),
			verbatim: verbatim_starts.binary_search(&offset).is_ok(),
		});
		offset += raw.len() + 1;
	}

	lines
}

fn trailing(lexed: &Lexed<'_>, offset: usize) -> Trailing {
	let after = lexed.after(offset);
	let space_end = match after.pieces.first() {
		Some(piece) if piece.kind == Kind::Space => piece.end,
		_ => offset,
	};
	let lines = lines(after.pieces, offset, after.ceiling);
	let at_end = after.ceiling_char.is_none();
	let first = lines[0];
	let mut trailing = Trailing {
		ends_line: first.next.is_some() || at_end,
		break_start: first.end,
		line_end: first.next.unwrap_or(first.end),
		blank_end: first.next.unwrap_or(first.end),
		below: Edge::Container,
		space_end,
	};

	if first.next.is_none() {
		// either code follows on the line, or the item's line is the last one
		return trailing;
	}

	// the last line is the one with the code after (or the end of the text), up to that code
	let last = lines.len() - 1;
	let mut index = 1;

	while index < last && !lines[index].has_comment {
		index += 1;
	}

	trailing.blank_end = lines[index].start;
	let closes = at_end || after.ceiling_char.is_some_and(is_closing_delimiter);

	trailing.below = if index == last && !lines[last].has_comment && closes {
		Edge::Container
	} else {
		Edge::Content
	};

	trailing
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Source files of this crate, which have doc comments, attributes, strings with comment-like contents, and
	/// raw strings.
	const REAL_FILES: &[(&str, &str)] = &[
		("model.rs", include_str!("../model.rs")),
		("source.rs", include_str!("../source.rs")),
		("path.rs", include_str!("../path.rs")),
		("pattern.rs", include_str!("../pattern.rs")),
		("edit/mod.rs", include_str!("mod.rs")),
		("edit/trivia.rs", include_str!("trivia.rs")),
	];

	fn apply(text: &str, edit: &TextEdit) -> String {
		format!("{}{}{}", &text[..edit.range.start], edit.replacement, &text[edit.range.end..])
	}

	fn assert_parses(text: &str) {
		if let Err(error) = syn::parse_file(text) {
			panic!("does not parse: {error}\n{text}");
		}
	}

	/// The range after `open` (which ends with `{`) up to the end of the first `close` after it (which ends with
	/// `}`), excluding that `}`.
	fn body(text: &str, open: &str, close: &str) -> TextRange {
		let start = text.find(open).unwrap() + open.len();

		TextRange::new(start, start + text[start..].find(close).unwrap() + close.len() - 1)
	}

	#[test]
	fn clamps_invalid_ranges() {
		let text = "struct Ü;\n";

		assert_eq!(removal_range(text, TextRange::new(0, 100)), TextRange::new(0, text.len()));
		assert_eq!(removal_range(text, TextRange::new(8, 8)).start, 7);
		assert_eq!(removal_range("", TextRange::new(3, 5)), TextRange::new(0, 0));
		assert_eq!(line_indent(text, 100), "");
		assert_eq!(
			list_item_removal_range(text, TextRange::new(50, 60)),
			TextRange::new(text.len(), text.len())
		);
	}

	/// The code of a text (pieces that are not trivia), unmerged.
	fn code(text: &str) -> Vec<&str> {
		pieces(text)
			.into_iter()
			.filter(|(kind, _)| !kind.is_trivia())
			.map(|(_, text)| text)
			.collect()
	}

	#[test]
	fn collapses_blank_lines() {
		// blank lines on both sides: one run goes
		assert_eq!(remove("fn a() {}\n\nfn b() {}\n\nfn c() {}\n", "fn b() {}"), "fn a() {}\n\nfn c() {}\n");
		assert_eq!(
			remove("fn a() {}\n\n\nfn b() {}\n\nfn c() {}\n", "fn b() {}"),
			"fn a() {}\n\n\nfn c() {}\n"
		);

		// a blank line on one side only stays
		assert_eq!(remove("fn a() {}\nfn b() {}\n\nfn c() {}\n", "fn b() {}"), "fn a() {}\n\nfn c() {}\n");
		assert_eq!(remove("fn a() {}\n\nfn b() {}\nfn c() {}\n", "fn b() {}"), "fn a() {}\n\nfn c() {}\n");
		assert_eq!(remove("fn a() {}\nfn b() {}\nfn c() {}\n", "fn b() {}"), "fn a() {}\nfn c() {}\n");

		// no blank line at the start or end of a container
		assert_eq!(remove("fn a() {}\n\nfn b() {}\n", "fn a() {}"), "fn b() {}\n");
		assert_eq!(remove("fn a() {}\n\nfn b() {}\n", "fn b() {}"), "fn a() {}\n");
		assert_eq!(remove("fn a() {}\n\nfn b() {}", "fn b() {}"), "fn a() {}\n");
		assert_eq!(
			remove("mod m {\n    fn a() {}\n\n    fn b() {}\n}\n", "fn a() {}"),
			"mod m {\n    fn b() {}\n}\n"
		);
		assert_eq!(
			remove("mod m {\n    fn a() {}\n\n    fn b() {}\n}\n", "fn b() {}"),
			"mod m {\n    fn a() {}\n}\n"
		);

		// existing blank lines at the edges stay
		assert_eq!(
			remove("impl X {\n\n    fn a() {}\n\n    fn b() {}\n}\n", "fn a() {}"),
			"impl X {\n\n    fn b() {}\n}\n"
		);

		// the only item leaves an empty container
		assert_eq!(remove("mod m {\n\n    fn a() {}\n\n}\n", "fn a() {}"), "mod m {\n}\n");
		assert_eq!(remove("fn a() {}\n", "fn a() {}"), "");
		assert_eq!(remove("\n\nfn a() {} // x\n\n\n", "fn a() {}"), "");
	}

	fn count(text: &str, needle: &str) -> usize {
		text.matches(needle).count()
	}

	#[test]
	fn detects_line_endings_and_indentation() {
		assert_eq!(line_ending("a\r\nb\n"), "\r\n");
		assert_eq!(line_ending("a\nb\r\n"), "\n");
		assert_eq!(line_ending("a"), "\n");

		assert_eq!(indent_unit("mod m {\n\tfn a() {\n\t\tx();\n\t}\n}"), "\t");
		assert_eq!(indent_unit("mod m {\n  fn a() {\n    x();\n  }\n}"), "  ");
		assert_eq!(indent_unit("mod m {\n    fn a() {}\n}"), "    ");
		assert_eq!(indent_unit("fn a() {}"), "    ");
		assert_eq!(indent_unit("const S: &str = \"\n  x\";\n/*\n *\n */\nmod m {\n\tfn a() {}\n}"), "\t");
		assert_eq!(indent_unit("mod m {\n        // deep\n    fn a() {}\n}"), "    ");
	}

	/// The range of the unique occurrence of `needle`.
	fn find(text: &str, needle: &str) -> TextRange {
		let start = text.find(needle).unwrap_or_else(|| panic!("`{needle}` not found"));

		assert_eq!(text.rfind(needle), Some(start), "`{needle}` is ambiguous");

		TextRange::new(start, start + needle.len())
	}

	#[test]
	fn finds_body_indentation() {
		let text = "mod m {\n\n    // c\n    fn a() {}\n}\n";

		assert_eq!(body_indent(text, body(text, "mod m {", "\n}")), "    ");
		assert_eq!(body_indent(text, TextRange::new(0, text.len())), "");

		let text = "mod m {\n    fn a() {}\n}\nmod n {}\n";

		assert_eq!(body_indent(text, body(text, "mod n {", "}")), "    ");

		let text = "mod m {\n    fn a() {}\n    impl X { fn b() {} }\n}\n";

		assert_eq!(body_indent(text, body(text, "impl X {", " }")), "        ");

		let text = "mod n {}\n\tmod o {\n\t}";

		assert_eq!(body_indent(text, body(text, "mod o {", "}")), "\t\t");
		assert_eq!(body_indent("mod o {fn a() {}\n  fn b() {}\n}", TextRange::new(7, 29)), "  ");
	}

	#[test]
	fn finds_line_indentation() {
		let text = "mod m {\n\t  fn a() {}\n    fn b() {}\n}";

		assert_eq!(line_indent(text, text.find("fn a").unwrap()), "\t  ");
		assert_eq!(line_indent(text, text.find("a()").unwrap()), "\t  ");
		assert_eq!(line_indent(text, text.find("fn b").unwrap()), "    ");
		assert_eq!(line_indent(text, 0), "");
		assert_eq!(line_indent(text, text.len()), "");
		assert_eq!(line_indent("\u{feff}  x", 5), "  ");
		assert_eq!(line_indent("\u{feff}  x", 1), "  ");
	}

	#[test]
	fn insertions_follow_the_style_of_the_text() {
		let text = "mod m {\r\n\tfn a() {}\r\n}\r\n";
		let edit = insertion(
			text,
			Placement::After(find(text, "fn a() {}")),
			"fn x() {\n    if y {\n        z();\n    }\n}",
			"\t",
		);

		assert_eq!(
			apply(text, &edit),
			"mod m {\r\n\tfn a() {}\r\n\r\n\tfn x() {\r\n\t\tif y {\r\n\t\t\tz();\r\n\t\t}\r\n\t}\r\n}\r\n"
		);
	}

	#[test]
	fn insertions_keep_files_parsing() {
		let text = "//! docs\n\nuse a::b;\n\nmod m {\n    fn f() {}\n}\n\nimpl X {}\n";
		let module_body = body(text, "mod m {", "\n}");
		let impl_body = body(text, "impl X {", "}");
		let placements = [
			(Placement::Before(find(text, "use a::b;")), "", "struct S;"),
			(Placement::After(find(text, "use a::b;")), "", "struct S;"),
			(Placement::End(TextRange::new(0, text.len())), "", "struct S;"),
			(Placement::End(module_body), "    ", "struct S;"),
			(Placement::Before(find(text, "fn f() {}")), "    ", "struct S;"),
			(Placement::End(impl_body), "    ", "fn g() {}"),
		];

		for (placement, indent, source) in placements {
			let inserted = apply(text, &insertion(text, placement, source, indent));

			assert_parses(&inserted);
			assert!(inserted.contains(source), "{inserted}");
		}
	}

	#[test]
	fn inserts_after_items() {
		let text = "fn a() {} // a\nfn b() {}\n";
		let edit = insertion(text, Placement::After(find(text, "fn a() {}")), "fn x() {}", "");

		assert_eq!(apply(text, &edit), "fn a() {} // a\n\nfn x() {}\n\nfn b() {}\n");

		let text = "fn a() {}\n\nfn b() {}\n";
		let edit = insertion(text, Placement::After(find(text, "fn a() {}")), "fn x() {}\n", "");

		assert_eq!(apply(text, &edit), "fn a() {}\n\nfn x() {}\n\nfn b() {}\n");

		let edit = insertion(text, Placement::After(find(text, "fn b() {}")), "fn x() {}", "");

		assert_eq!(apply(text, &edit), "fn a() {}\n\nfn b() {}\n\nfn x() {}\n");

		let text = "impl X {\n    fn a() {}\n}\n";
		let edit = insertion(text, Placement::After(find(text, "fn a() {}")), "fn x() {\n    y();\n}", "    ");

		assert_eq!(apply(text, &edit), "impl X {\n    fn a() {}\n\n    fn x() {\n        y();\n    }\n}\n");

		let text = "fn a() {}";
		let edit = insertion(text, Placement::After(find(text, "fn a() {}")), "fn x() {}", "");

		assert_eq!(apply(text, &edit), "fn a() {}\n\nfn x() {}");

		let text = "struct A; struct B;\n";
		let edit = insertion(text, Placement::After(find(text, "struct A;")), "struct X;", "");

		assert_eq!(apply(text, &edit), "struct A;\n\nstruct X;\n\nstruct B;\n");
	}

	#[test]
	fn inserts_at_the_end_of_bodies() {
		let body = |text: &str, open: &str| {
			let start = text.find(open).unwrap() + open.len();

			TextRange::new(start, start + text[start..].rfind('}').unwrap())
		};

		let text = "mod m {}\n";
		let edit = insertion(text, Placement::End(body(text, "mod m {")), "fn x() {}", "    ");

		assert_eq!(apply(text, &edit), "mod m {\n    fn x() {}\n}\n");

		let text = "    impl X { }\n";
		let edit = insertion(text, Placement::End(body(text, "impl X {")), "fn x() {\n    y();\n}", "        ");

		assert_eq!(apply(text, &edit), "    impl X {\n        fn x() {\n            y();\n        }\n    }\n");

		let text = "mod m {\n    #![allow(x)]\n    // dangling\n}\n";
		let edit = insertion(text, Placement::End(body(text, "mod m {")), "fn x() {}", "    ");

		assert_eq!(apply(text, &edit), "mod m {\n    #![allow(x)]\n    // dangling\n\n    fn x() {}\n}\n");

		let text = "mod m { #![allow(x)] }";
		let edit = insertion(text, Placement::End(body(text, "mod m {")), "fn x() {}", "    ");

		assert_eq!(apply(text, &edit), "mod m { #![allow(x)]\n\n    fn x() {}\n}");

		let whole = |text: &str| TextRange::new(0, text.len());

		for (text, expected) in [
			("", "fn x() {}\n"),
			("\n\n", "fn x() {}\n"),
			("\u{feff}", "\u{feff}fn x() {}\n"),
			("//! docs\n", "//! docs\n\nfn x() {}\n"),
			("//! docs", "//! docs\n\nfn x() {}\n"),
			("fn a() {}\n\n\n", "fn a() {}\n\nfn x() {}\n\n\n"),
			("#!/bin/x\r\n", "#!/bin/x\r\n\r\nfn x() {}\r\n"),
		] {
			let edit = insertion(text, Placement::End(whole(text)), "fn x() {}", "");

			assert_eq!(apply(text, &edit), expected, "{text:?}");
		}
	}

	#[test]
	fn inserts_before_items() {
		let text = "fn a() {}\n// about b\nfn b() {}\n";
		let edit = insertion(text, Placement::Before(find(text, "fn b() {}")), "fn x() {}", "");

		assert_eq!(apply(text, &edit), "fn a() {}\n\nfn x() {}\n\n// about b\nfn b() {}\n");

		let text = "fn a() {}\n\nfn b() {}\n";
		let edit = insertion(text, Placement::Before(find(text, "fn b() {}")), "fn x() {}", "");

		assert_eq!(apply(text, &edit), "fn a() {}\n\nfn x() {}\n\nfn b() {}\n");

		let text = "mod m {\n    fn a() {}\n}\n";
		let edit = insertion(text, Placement::Before(find(text, "fn a() {}")), "fn x() {}", "    ");

		assert_eq!(apply(text, &edit), "mod m {\n    fn x() {}\n\n    fn a() {}\n}\n");

		let text = "//! docs\n\n// license\n\nuse a;\n";
		let edit = insertion(text, Placement::Before(find(text, "use a;")), "use b;", "");

		assert_eq!(apply(text, &edit), "//! docs\n\n// license\n\nuse b;\n\nuse a;\n");

		let text = "#![allow(x)]\nuse a;\n";
		let edit = insertion(text, Placement::Before(find(text, "use a;")), "use b;", "");

		assert_eq!(apply(text, &edit), "#![allow(x)]\n\nuse b;\n\nuse a;\n");

		let text = "struct A; struct B;\n";
		let edit = insertion(text, Placement::Before(find(text, "struct B;")), "struct X;", "");

		assert_eq!(apply(text, &edit), "struct A;\n\nstruct X;\n\nstruct B;\n");
	}

	#[test]
	fn keeps_comment_like_text_of_strings() {
		let text = "const S: &str = \"x\n// not a comment\";\nfn f() {}\nfn g() {}\n";

		assert_eq!(remove(text, "fn f() {}"), "const S: &str = \"x\n// not a comment\";\nfn g() {}\n");

		let text = "const S: &str = r#\"\n/* \"# ;\n// about f\nfn f() {}\n";

		assert_eq!(remove(text, "fn f() {}"), "const S: &str = r#\"\n/* \"# ;\n");
	}

	#[test]
	fn keeps_inner_docs_and_next_items_docs() {
		let text = "//! crate docs\n// about f\nfn f() {}\nfn g() {}\n";

		assert_eq!(remove(text, "fn f() {}"), "//! crate docs\nfn g() {}\n");

		let text = "fn f() {} /// docs of g\nfn g() {}\n";

		assert_eq!(remove(text, "fn f() {}"), "/// docs of g\nfn g() {}\n");
	}

	#[test]
	fn keeps_other_code_on_the_same_line() {
		assert_eq!(remove("struct A; struct B;\n", "struct A;"), "struct B;\n");
		assert_eq!(remove("struct A; struct B;\n", "struct B;"), "struct A;\n");
		assert_eq!(remove("struct A; struct B; struct C;\n", "struct B;"), "struct A; struct C;\n");
		assert_eq!(remove("    struct A;  struct B;\n", "struct A;"), "    struct B;\n");
		assert_eq!(remove("struct A; struct B; // c\n", "struct B;"), "struct A; // c\n");

		// the comment above belongs to the whole line
		assert_eq!(remove("// c\nstruct A; struct B;\n", "struct A;"), "// c\nstruct B;\n");
		assert_eq!(remove("mod m { fn a() {} }\n", "fn a() {}"), "mod m { }\n");
	}

	#[test]
	fn lexes_byte_order_marks_and_shebangs() {
		use Kind::*;

		assert_eq!(pieces("\u{feff}fn"), [(Code, "fn")]);
		assert_eq!(
			pieces("#!/usr/bin/env run-cargo-script\r\nfn"),
			[(Code, "#!/usr/bin/env run-cargo-script"), (LineBreak, "\r\n"), (Code, "fn")]
		);
		assert_eq!(code("#![allow(x)]"), ["#", "!", "[", "allow", "(", "x", ")", "]"]);
		assert_eq!(code("#! /* c */ [allow(x)]")[..3], ["#", "!", "["]);
		assert_eq!(code("\u{feff}#!shebang 'x\nfn"), ["#!shebang 'x", "fn"]);
	}

	#[test]
	fn lexes_code_strings_and_comments() {
		use Kind::*;

		assert_eq!(
			pieces("a // c\r\nb"),
			[(Code, "a"), (Space, " "), (Comment, "// c"), (LineBreak, "\r\n"), (Code, "b")]
		);
		assert_eq!(pieces("/* a /* b */ c */x"), [(Comment, "/* a /* b */ c */"), (Code, "x")]);
		assert_eq!(pieces("\"// no\\\" /*\" x"), [(Str, "\"// no\\\" /*\""), (Space, " "), (Code, "x")]);
		assert_eq!(pieces("r#\"a\"b\"#c"), [(Str, "r#\"a\"b\"#"), (Code, "c")]);
		assert_eq!(pieces("r\"x\" br##\"\"#\"##"), [(Str, "r\"x\""), (Space, " "), (Str, "br##\"\"#\"##")]);
		assert_eq!(pieces("b\"x\" c\"y\""), [(Str, "b\"x\""), (Space, " "), (Str, "c\"y\"")]);
		assert_eq!(
			pieces("'a' '\\'' b'\"' '\\u{1F600}'"),
			[
				(Code, "'a'"),
				(Space, " "),
				(Code, "'\\''"),
				(Space, " "),
				(Code, "b'\"'"),
				(Space, " "),
				(Code, "'\\u{1F600}'"),
			]
		);
		assert_eq!(
			code("fn f<'a>(x: &'a str) -> char { '\"' }"),
			[
				"fn", "f", "<", "'a", ">", "(", "x", ":", "&", "'a", "str", ")", "-", ">", "char", "{", "'\"'", "}"
			]
		);
		assert_eq!(code("r#type break crate r#\"s\"#"), ["r", "#", "type", "break", "crate", "r#\"s\"#"]);
		assert_eq!(
			pieces("/// doc\n//! inner\n//// plain\n/** doc */ /*! inner */ /**/ /*** plain */"),
			[
				(Code, "/// doc"),
				(LineBreak, "\n"),
				(Code, "//! inner"),
				(LineBreak, "\n"),
				(Comment, "//// plain"),
				(LineBreak, "\n"),
				(Code, "/** doc */"),
				(Space, " "),
				(Code, "/*! inner */"),
				(Space, " "),
				(Comment, "/**/"),
				(Space, " "),
				(Comment, "/*** plain */"),
			]
		);
		assert_eq!(pieces("é\u{a0}x"), [(Code, "é"), (Space, "\u{a0}"), (Code, "x")]);
		assert_eq!(pieces("a\rb"), [(Code, "a"), (Space, "\r"), (Code, "b")]);
	}

	#[test]
	fn lexes_unterminated_input_to_the_end() {
		for text in ["\"abc", "r#\"abc\"", "/* a /* b */", "'\\", "'", "b'", "r#", "br", "\r"] {
			let total: usize = pieces(text).iter().map(|(_, piece)| piece.len()).sum();

			assert_eq!(total, text.len(), "{text:?}");
		}
	}

	/// The pieces of a text as (kind, text) pairs, unmerged.
	fn pieces(text: &str) -> Vec<(Kind, &str)> {
		let mut pieces = Vec::new();

		scan_file(text, |piece| {
			pieces.push((piece.kind, &text[piece.start..piece.end]));
			true
		});

		pieces
	}

	/// Random texts and ranges never panic, and give ranges within the text on character boundaries.
	#[test]
	fn random_input_never_panics() {
		const PIECES: &[&str] = &[
			"/", "*", "'", "\"", "\\", "#", "r", "b", "c", "{", "}", "(", ")", "<", ">", ",", ";", "\n", "\r\n", "\r", "\t", " ", "  ", "a", "é",
			"\u{feff}", "fn", "//", "/*", "*/", "///", "//!", "r#\"", "\"#", "#!", "x", "1", "\u{a0}",
		];

		// xorshift, for reproducible inputs without dependencies
		let mut state = 0xd1b5_4a32_d192_ed03_u64;
		let mut random = |bound: usize| {
			state ^= state << 13;
			state ^= state >> 7;
			state ^= state << 17;
			(state % bound as u64) as usize
		};
		let in_text = |text: &str, range: TextRange| {
			assert!(range.start <= range.end && range.end <= text.len(), "{range:?} in {text:?}");
			assert!(
				text.is_char_boundary(range.start) && text.is_char_boundary(range.end),
				"{range:?} in {text:?}"
			);
		};

		for _ in 0..5_000 {
			let length = random(30);
			let text: String = (0..length).map(|_| PIECES[random(PIECES.len())]).collect();
			let mut offset = || random(text.len() + 3);
			let range = TextRange {
				start: offset(),
				end: offset(),
			};
			let other = TextRange {
				start: offset(),
				end: offset(),
			};

			in_text(&text, removal_range(&text, range));
			in_text(&text, list_item_removal_range(&text, range));

			for removal in removal_ranges(&text, &[range, other]) {
				in_text(&text, removal);
			}

			let _ = (line_indent(&text, range.start), line_ending(&text), indent_unit(&text));
			let _ = (body_indent(&text, range), reindent(&text, "\t", &text));

			for placement in [Placement::Before(range), Placement::After(range), Placement::End(range)] {
				let edit = insertion(&text, placement, &text, "  ");

				in_text(&text, edit.range);
			}
		}
	}

	#[test]
	fn reindents_source() {
		assert_eq!(reindent("fn a() {\n    x();\n}", "    ", "impl X {}"), "fn a() {\n        x();\n    }");
		assert_eq!(
			reindent("\n\n  fn a() {\n      x();\n  }\n\n", "\t", "mod m {\n\tfn b() {}\n}"),
			"fn a() {\n\t\tx();\n\t}"
		);

		// the first line was copied without its indentation
		assert_eq!(reindent("fn a() {\n        x();\n    }", "", "mod m {}"), "fn a() {\n    x();\n}");

		// blank lines are emptied, and line breaks follow the text
		assert_eq!(
			reindent("fn a() {\n\n    x();   \n}\n", "  ", "a\r\nb"),
			"fn a() {\r\n\r\n      x();   \r\n  }"
		);

		// string contents are kept
		let source = "const S: &str = \"one\n  two\n\";\nconst R: &str = r#\"\n    raw\"#;";

		assert_eq!(
			reindent(source, "    ", "x"),
			"const S: &str = \"one\n  two\n\";\n    const R: &str = r#\"\n    raw\"#;"
		);

		// doc and block comments are re-indented
		let source = "    /**\n     * docs\n     */\n    fn a() {}";

		assert_eq!(reindent(source, "\t", "mod m {\n\tfn a() {}\n}"), "/**\n\t * docs\n\t */\n\tfn a() {}");

		assert_eq!(reindent("", "    ", "x"), "");
		assert_eq!(reindent(" \n \n", "    ", "x"), "");
		assert_eq!(reindent("\u{feff}fn a() {}", "    ", "x"), "fn a() {}");
	}

	#[test]
	fn removal_keeps_files_parsing() {
		let text = concat!(
			"//! docs\n\nuse std::fmt;\n\n/// A\n#[derive(Debug)]\nstruct A { x: u8 } // a\n\n",
			"// about B\nenum B {\n    X,\n}\n\n",
			"impl fmt::Display for A {\n    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {\n",
			"        write!(f, \"// {}\", self.x)\n    }\n}\n\n",
			"const C: char = '\"';\nfn main() {}\n",
		);

		assert_parses(text);

		for item in [
			"use std::fmt;",
			"/// A\n#[derive(Debug)]\nstruct A { x: u8 }",
			"enum B {\n    X,\n}",
			"const C: char = '\"';",
			"fn main() {}",
		] {
			let removed = remove(text, item);

			assert_parses(&removed);
			assert!(!removed.contains(item), "{item}");
			assert!(!removed.contains("\n\n\n"), "{removed}");
		}
	}

	/// [`removal_ranges`] for one item.
	fn removal_range(text: &str, item: TextRange) -> TextRange {
		removal_range_in(&Lexed::new(text), clamp(text, item))
	}

	/// Removes `item` (a unique substring) from `text` with [`removal_range`].
	fn remove(text: &str, item: &str) -> String {
		let range = removal_range(text, find(text, item));

		format!("{}{}", &text[..range.start], &text[range.end..])
	}

	fn remove_list_item(text: &str, item: &str) -> String {
		let range = list_item_removal_range(text, find(text, item));

		format!("{}{}", &text[..range.start], &text[range.end..])
	}

	#[test]
	fn removes_every_item_of_real_files() {
		use syn::spanned::Spanned;

		// the tokens (other than the item) in a removed range, per proc-macro2's lexer: trivia has none
		let extra_tokens = |text: &str, removal: TextRange, item: TextRange| {
			let extra = format!("{} {}", &text[removal.start..item.start], &text[item.end..removal.end]);

			extra.parse::<proc_macro2::TokenStream>().ok().map(|tokens| tokens.to_string())
		};

		for &(name, text) in REAL_FILES {
			let file = syn::parse_file(text).unwrap();
			let mut items: Vec<TextRange> = Vec::new();
			let mut variants: Vec<TextRange> = Vec::new();
			let range = |node: &dyn Spanned| TextRange::from(node.span().byte_range());

			for item in &file.items {
				items.push(range(item));

				match item {
					syn::Item::Impl(block) => items.extend(block.items.iter().map(|item| range(item))),
					syn::Item::Trait(block) => items.extend(block.items.iter().map(|item| range(item))),
					syn::Item::Mod(module) => {
						items.extend(module.content.iter().flat_map(|(_, items)| items).map(|item| range(item)));
					}
					syn::Item::Enum(enumeration) => variants.extend(enumeration.variants.iter().map(|v| range(v))),
					_ => {}
				}
			}

			for (index, &item) in items.iter().enumerate() {
				let removal = removal_range(text, item);
				let removed = format!("{}{}", &text[..removal.start], &text[removal.end..]);
				let shown = &text[item.as_range()];

				assert!(removal.contains_range(item), "{name}: {item:?} not in {removal:?}");
				assert_eq!(extra_tokens(text, removal, item).as_deref(), Some(""), "{name}: removing {shown:?}");
				assert!(count(&removed, "\n\n\n") <= count(text, "\n\n\n"), "{name}: removing {shown:?}");

				// parsing is slow in debug builds: fully check a sample
				if index % 8 != 0 {
					continue;
				}

				assert!(syn::parse_file(&removed).is_ok(), "{name}: removing {shown:?} breaks the file");

				for placement in [Placement::Before(item), Placement::After(item)] {
					let edit = insertion(text, placement, "fn inserted() {}", line_indent(text, item.start));

					assert!(syn::parse_file(&apply(text, &edit)).is_ok(), "{name}: inserting at {placement:?}");
				}
			}

			for variant in variants {
				let removal = list_item_removal_range(text, variant);
				let shown = &text[variant.as_range()];

				assert_eq!(extra_tokens(text, removal, variant).as_deref(), Some(","), "{name}: removing {shown:?}");
			}
		}
	}

	#[test]
	fn removes_items_with_attached_comments() {
		let text = "use a;\n\n// about f\n/* more */\n/// docs\n#[inline]\nfn f() {} // trailing\n\nfn g() {}\n";

		assert_eq!(remove(text, "/// docs\n#[inline]\nfn f() {}"), "use a;\n\nfn g() {}\n");

		// a comment separated by a blank line stays
		let text = "use a;\n\n// section\n\nfn f() {}\n\nfn g() {}\n";

		assert_eq!(remove(text, "fn f() {}"), "use a;\n\n// section\n\nfn g() {}\n");

		// the trailing comment of the code before stays with it
		let text = "use a; // about a\n// about f\nfn f() {}\nfn g() {}\n";

		assert_eq!(remove(text, "fn f() {}"), "use a; // about a\nfn g() {}\n");

		// comments on the same line before the item go with it
		let text = "use a;\n/* note */ fn f() {}\nfn g() {}\n";

		assert_eq!(remove(text, "fn f() {}"), "use a;\nfn g() {}\n");
	}

	#[test]
	fn removes_list_items() {
		let text = "use a::{B, C, D};";

		assert_eq!(remove_list_item(text, "B"), "use a::{C, D};");
		assert_eq!(remove_list_item(text, "C"), "use a::{B, D};");
		assert_eq!(remove_list_item(text, "D"), "use a::{B, C};");
		assert_eq!(remove_list_item("use a::{B};", "B"), "use a::{};");
		assert_eq!(remove_list_item("enum E { A, B, }", "B"), "enum E { A, }");

		let text = "enum E {\n    A,\n    // about B\n    B = 2, // two\n    C\n}\n";

		assert_eq!(remove_list_item(text, "A"), "enum E {\n    // about B\n    B = 2, // two\n    C\n}\n");
		assert_eq!(remove_list_item(text, "B = 2"), "enum E {\n    A,\n    C\n}\n");
		assert_eq!(remove_list_item(text, "C"), "enum E {\n    A,\n    // about B\n    B = 2, // two\n}\n");

		let text = "use a::{\n    b::{X, Y},\n    Z,\n};\n";

		assert_eq!(remove_list_item(text, "Y"), "use a::{\n    b::{X},\n    Z,\n};\n");
		assert_eq!(remove_list_item(text, "b::{X, Y}"), "use a::{\n    Z,\n};\n");
	}

	#[test]
	fn removes_multi_line_block_comments() {
		let text = "use a;\n/*\n * about f\n */\nfn f() {}\nfn g() {}\n";

		assert_eq!(remove(text, "fn f() {}"), "use a;\nfn g() {}\n");

		// a block comment starting after the code before belongs to that code
		let text = "use a; /* about a\n   still a */\n// about f\nfn f() {}\nfn g() {}\n";

		assert_eq!(remove(text, "fn f() {}"), "use a; /* about a\n   still a */\nfn g() {}\n");

		// a blank line inside of a block comment does not separate it
		let text = "use a;\n/* one\n\n   two */\nfn f() {}\nfn g() {}\n";

		assert_eq!(remove(text, "fn f() {}"), "use a;\nfn g() {}\n");
	}

	#[test]
	fn removes_nested_items() {
		let text = "impl X {\n    // about a\n    fn a() {}\n\n    fn b() {}\n\n    fn c() {}\n}\n";

		assert_eq!(remove(text, "fn a() {}"), "impl X {\n    fn b() {}\n\n    fn c() {}\n}\n");
		assert_eq!(remove(text, "fn b() {}"), "impl X {\n    // about a\n    fn a() {}\n\n    fn c() {}\n}\n");
		assert_eq!(remove(text, "fn c() {}"), "impl X {\n    // about a\n    fn a() {}\n\n    fn b() {}\n}\n");

		let text = "mod m { // header\n\n\tfn a() {}\n\tfn b() {}\n}";

		assert_eq!(remove(text, "fn a() {}"), "mod m { // header\n\n\tfn b() {}\n}");
	}

	#[test]
	fn removes_several_items() {
		let text = "fn a() {}\n\nfn b() {}\n\n// about c\nfn c() {}\n\nfn d() {}\n";
		let ranges = removal_ranges(text, &[find(text, "fn c() {}"), find(text, "fn b() {}")]);

		assert_eq!(ranges.len(), 1);

		let range = ranges[0];

		assert_eq!(format!("{}{}", &text[..range.start], &text[range.end..]), "fn a() {}\n\nfn d() {}\n");

		let text = "mod m {\n    fn a() {}\n\n    fn b() {}\n}\n\nfn c() {}\n";
		let ranges = removal_ranges(
			text,
			&[
				find(text, "fn b() {}"),
				find(text, "fn c() {}"),
				find(text, "mod m {\n    fn a() {}\n\n    fn b() {}\n}"),
			],
		);

		assert_eq!(ranges, [TextRange::new(0, text.len())]);

		let text = "fn a() {}\nfn b() {}\nfn c() {}\n";
		let ranges = removal_ranges(text, &[find(text, "fn a() {}"), find(text, "fn c() {}")]);

		assert_eq!(ranges, [TextRange::new(0, 10), TextRange::new(20, 30)]);
	}

	#[test]
	fn removes_with_crlf_and_byte_order_marks() {
		let text = "use a;\r\n\r\n// about f\r\nfn f() {} // x\r\n\r\nfn g() {}\r\n";

		assert_eq!(remove(text, "fn f() {}"), "use a;\r\n\r\nfn g() {}\r\n");
		assert_eq!(remove(text, "fn g() {}"), "use a;\r\n\r\n// about f\r\nfn f() {} // x\r\n");

		let text = "\u{feff}// about f\nfn f() {}\n\nfn g() {}\n";

		assert_eq!(remove(text, "fn f() {}"), "\u{feff}fn g() {}\n");

		let text = "#!/usr/bin/env x\n\nfn f() {}\n";

		assert_eq!(remove(text, "fn f() {}"), "#!/usr/bin/env x\n");
	}
}
