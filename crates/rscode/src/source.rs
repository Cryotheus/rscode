//! Source files, byte ranges, and line-column locations.

use serde::Deserialize;
use serde::Serialize;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

/// Index of a [`SourceFile`] within its [`Crate`](crate::Crate).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct FileId(pub(crate) u32);

impl FileId {
	/// The index of the file in [`Crate::files`](crate::Crate::files).
	pub fn index(self) -> usize {
		self.0 as usize
	}
}

/// A half-open range of byte offsets into a source file.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct TextRange {
	/// The offset of the first byte.
	pub start: usize,

	/// The offset just past the last byte.
	pub end: usize,
}

impl TextRange {
	/// The range `start..end` (`start` must not be after `end`).
	pub fn new(start: usize, end: usize) -> Self {
		debug_assert!(start <= end, "inverted text range {start}..{end}");

		Self { start, end }
	}

	/// The number of bytes.
	pub fn len(self) -> usize {
		self.end - self.start
	}

	/// Whether the range has no bytes.
	pub fn is_empty(self) -> bool {
		self.start == self.end
	}

	/// Whether the byte at `offset` is in the range.
	pub fn contains(self, offset: usize) -> bool {
		self.start <= offset && offset < self.end
	}

	/// Whether `other` lies within this range.
	pub fn contains_range(self, other: TextRange) -> bool {
		self.start <= other.start && other.end <= self.end
	}

	/// Whether the ranges have bytes in common.
	pub fn overlaps(self, other: TextRange) -> bool {
		self.start < other.end && other.start < self.end
	}

	/// The smallest range covering both ranges.
	pub fn cover(self, other: TextRange) -> TextRange {
		TextRange::new(self.start.min(other.start), self.end.max(other.end))
	}

	/// The range as a [`Range`] (to slice text with).
	pub fn as_range(self) -> Range<usize> {
		self.start..self.end
	}
}

impl From<Range<usize>> for TextRange {
	fn from(range: Range<usize>) -> Self {
		Self::new(range.start, range.end)
	}
}

impl From<TextRange> for Range<usize> {
	fn from(range: TextRange) -> Self {
		range.as_range()
	}
}

/// A 1-based line and 1-based column (counted in `char`s), like rustc diagnostics.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct LineCol {
	/// The line, from 1.
	pub line: usize,

	/// The column in `char`s, from 1.
	pub column: usize,
}

impl std::fmt::Display for LineCol {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}:{}", self.line, self.column)
	}
}

/// Maps between byte offsets and line-column locations of a text.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LineIndex {
	/// Byte offset of the start of every line.
	line_starts: Vec<usize>,
	len: usize,
}

impl LineIndex {
	/// The index of a text's lines.
	pub fn new(text: &str) -> Self {
		let mut line_starts = vec![0];

		line_starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));

		Self {
			line_starts,
			len: text.len(),
		}
	}

	/// Number of lines (a trailing newline starts an empty last line).
	pub fn line_count(&self) -> usize {
		self.line_starts.len()
	}

	/// Byte offset of the start of a 1-based line, clamped to the end of the text.
	pub fn line_start(&self, line: usize) -> usize {
		match line.checked_sub(1) {
			Some(index) => self.line_starts.get(index).copied().unwrap_or(self.len),
			None => 0,
		}
	}

	/// Byte offset of the end of a 1-based line, excluding the line break (`\n` or `\r\n`).
	pub fn line_end(&self, text: &str, line: usize) -> usize {
		let start = self.line_start(line);
		let bytes = text.as_bytes();
		let mut end = self.line_start(line + 1);

		if end > start && bytes[end - 1] == b'\n' {
			end -= 1;

			if end > start && bytes[end - 1] == b'\r' {
				end -= 1;
			}
		}

		end
	}

	/// The line-column location of a byte offset. Offsets inside a multi-byte `char` round down.
	pub fn line_col(&self, text: &str, offset: usize) -> LineCol {
		let offset = offset.min(self.len);
		let line_index = self.line_starts.partition_point(|&start| start <= offset) - 1;
		let line_start = self.line_starts[line_index];
		let mut column_end = offset;

		while !text.is_char_boundary(column_end) {
			column_end -= 1;
		}

		LineCol {
			line: line_index + 1,
			column: text[line_start..column_end].chars().count() + 1,
		}
	}

	/// The byte offset of a 1-based line and 0-based `char` column,
	/// the convention of [`proc_macro2::LineColumn`].
	///
	/// Columns past the end of the line are clamped to the end of the line (before its line break, like
	/// [`line_end`](Self::line_end)).
	pub fn offset_of_line_column0(&self, text: &str, line: usize, column: usize) -> usize {
		let start = self.line_start(line);
		let end = self.line_end(text, line);

		match text[start..end].char_indices().nth(column) {
			Some((index, _)) => start + index,
			None => end,
		}
	}

	/// The byte offset of a [`LineCol`] (1-based line and 1-based column).
	pub fn offset(&self, text: &str, location: LineCol) -> usize {
		self.offset_of_line_column0(text, location.line, location.column.saturating_sub(1))
	}
}

/// A loaded Rust source file.
#[derive(Debug, Clone)]
pub struct SourceFile {
	pub(crate) path: PathBuf,
	pub(crate) text: Arc<str>,
	pub(crate) line_index: Arc<LineIndex>,
}

impl SourceFile {
	/// A file loaded from `path`, with its text.
	pub fn new(path: PathBuf, text: impl Into<Arc<str>>) -> Self {
		let text = text.into();
		let line_index = Arc::new(LineIndex::new(&text));

		Self { path, text, line_index }
	}

	/// The path the file was loaded from.
	pub fn path(&self) -> &Path {
		&self.path
	}

	/// The text of the file, as loaded.
	pub fn text(&self) -> &str {
		&self.text
	}

	/// The text of the file, to share without copying it.
	pub fn shared_text(&self) -> &Arc<str> {
		&self.text
	}

	/// The index of the file's lines.
	pub fn line_index(&self) -> &LineIndex {
		&self.line_index
	}

	/// The line-column location of a byte offset (see [`LineIndex::line_col`]).
	pub fn line_col(&self, offset: usize) -> LineCol {
		self.line_index.line_col(&self.text, offset)
	}

	/// Start and end locations of a range. The end location is exclusive (points just past the last `char`).
	pub fn locate(&self, range: TextRange) -> (LineCol, LineCol) {
		(self.line_col(range.start), self.line_col(range.end))
	}

	/// The text of a range (which must be within the text, at character boundaries).
	pub fn slice(&self, range: TextRange) -> &str {
		&self.text[range.as_range()]
	}

	/// Parses the file with `syn`.
	///
	/// Must be called on the thread that uses the result: spans live in a thread-local source map.
	pub(crate) fn parse(&self) -> Result<ParsedFile<'_>, syn::Error> {
		ParsedFile::parse(&self.text)
	}

	/// The location of a `syn` parse error of this file's text (must be called on the parsing thread).
	pub(crate) fn error_location(&self, error: &syn::Error) -> LineCol {
		let start = error.span().start();

		LineCol {
			line: start.line,
			column: start.column + 1 + usize::from(start.line == 1 && self.text.starts_with('\u{feff}')),
		}
	}
}

/// Runs `job`, which parses, on a short-lived thread with a large stack, and returns its result.
///
/// `proc_macro2` (which `syn` parses with) keeps the text of everything parsed on a thread in a thread-local source
/// map that only grows until the thread exits: parsing on the caller's thread would make a long-running caller grow
/// with every file it loads. Parsing also recurses as deeply as the code is nested, which can exhaust a small stack.
/// When no thread can be started, the job runs on the calling thread.
pub(crate) fn isolated<T: Send>(job: impl FnOnce() -> T + Send) -> T {
	let job = Mutex::new(Some(job));
	let run = || job.lock().unwrap_or_else(PoisonError::into_inner).take().map(|job| job());

	let ran = std::thread::scope(|scope| {
		let thread = std::thread::Builder::new().stack_size(rscode_fmt::RECOMMENDED_STACK_SIZE);

		match thread.spawn_scoped(scope, run) {
			Ok(handle) => handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
			Err(_) => None,
		}
	});

	ran.or_else(run).expect("the job runs exactly once, on the new thread or else here")
}

/// A parsed source text, able to map `proc_macro2` spans back to byte ranges of the original text.
///
/// `syn::parse_file` strips a byte order mark and a shebang line before parsing, and span byte ranges are relative
/// to the stripped text; [`ParsedFile::range`] compensates.
pub(crate) struct ParsedFile<'a> {
	pub(crate) text: &'a str,
	pub(crate) file: syn::File,
	offset: usize,
}

impl<'a> ParsedFile<'a> {
	pub(crate) fn parse(text: &'a str) -> Result<Self, syn::Error> {
		let file = syn::parse_file(text)?;
		let bom = if text.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
		let offset = bom + file.shebang.as_ref().map_or(0, String::len);

		Ok(Self { text, file, offset })
	}

	/// The byte range of a span produced by this parse.
	pub(crate) fn range(&self, span: proc_macro2::Span) -> TextRange {
		let range = span.byte_range();

		TextRange::new(range.start + self.offset, range.end + self.offset)
	}

	/// The byte range of a syntax node (spanning its first to last token).
	pub(crate) fn range_of(&self, node: &impl syn::spanned::Spanned) -> TextRange {
		self.range(node.span())
	}

	pub(crate) fn slice(&self, range: TextRange) -> &'a str {
		&self.text[range.as_range()]
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn line_index_round_trips() {
		let text = "fn a() {}\r\nstruct Ü;\n\nmod x;";
		let index = LineIndex::new(text);

		assert_eq!(index.line_count(), 4);
		assert_eq!(index.line_col(text, 0), LineCol { line: 1, column: 1 });
		assert_eq!(index.line_col(text, 11), LineCol { line: 2, column: 1 });

		// `;` after the two-byte `Ü`
		let semi = text.find(';').unwrap();

		assert_eq!(index.line_col(text, semi), LineCol { line: 2, column: 9 });
		assert_eq!(index.offset_of_line_column0(text, 2, 8), semi);
		assert_eq!(index.offset(text, LineCol { line: 4, column: 1 }), text.rfind("mod").unwrap());
		assert_eq!(index.line_end(text, 1), 9);
		assert_eq!(index.line_end(text, 3), index.line_start(3));
		assert_eq!(index.line_end(text, 4), text.len());
	}

	#[test]
	fn isolates_jobs_on_threads_of_their_own() {
		let caller = std::thread::current().id();
		let text = "fn a() {}";
		let (thread, parsed) = isolated(|| (std::thread::current().id(), ParsedFile::parse(text).is_ok()));

		assert_ne!(thread, caller);
		assert!(parsed);

		let panicked = std::panic::catch_unwind(|| isolated(|| panic!("boom")));

		assert!(panicked.is_err());
	}

	#[test]
	fn clamps_columns_to_the_end_of_the_line() {
		let text = "ab\r\ncd\n";
		let index = LineIndex::new(text);

		assert_eq!(index.offset(text, LineCol { line: 1, column: 10 }), 2);
		assert_eq!(index.offset(text, LineCol { line: 1, column: 3 }), 2);
		assert_eq!(index.offset(text, LineCol { line: 1, column: 4 }), 2);
		assert_eq!(index.offset(text, LineCol { line: 2, column: 2 }), 5);
		assert_eq!(index.offset_of_line_column0(text, 2, 7), 6);
		assert_eq!(index.offset_of_line_column0(text, 3, 1), text.len());
		assert_eq!(index.offset_of_line_column0(text, 9, 0), text.len());
	}
}
