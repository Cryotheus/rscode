//! Parsing source text, and mapping spans back to byte offsets of the original text.

use crate::FormatError;
use proc_macro2::Span;
use std::borrow::Cow;
use std::ops::Range;

/// The byte order mark, which `syn` and `proc_macro2` skip.
pub(crate) const BOM: &str = "\u{feff}";

/// A parsed source text, able to map `proc_macro2` spans back to byte ranges of the original text.
///
/// `syn::parse_file` strips a byte order mark and a shebang line before parsing, and span byte ranges are relative
/// to the stripped text; [`Parsed::range`] compensates.
///
/// Must be used on the thread that parsed it: spans live in a thread-local source map.
pub(crate) struct Parsed {
	pub(crate) file: syn::File,
	offset: usize,
}

impl Parsed {
	pub(crate) fn parse(text: &str) -> Result<Self, FormatError> {
		let file = syn::parse_file(text).map_err(|error| FormatError::from_syn_in(&error, text))?;
		let offset = bom_len(text) + file.shebang.as_ref().map_or(0, String::len);

		Ok(Self { file, offset })
	}

	/// The byte range of a span produced by this parse.
	pub(crate) fn range(&self, span: Span) -> Range<usize> {
		let range = span.byte_range();

		range.start + self.offset..range.end + self.offset
	}
}

/// The length of the byte order mark at the start of `text`, if any.
pub(crate) fn bom_len(text: &str) -> usize {
	if text.starts_with(BOM) { BOM.len() } else { 0 }
}

/// Fails with [`FormatError::StructureMismatch`] if `text` is not a valid Rust file.
pub(crate) fn ensure_parses(text: &str, produced_by: &str) -> Result<(), FormatError> {
	match syn::parse_file(text) {
		Ok(_) => Ok(()),
		Err(error) => {
			let location = FormatError::from_syn_in(&error, text);

			Err(FormatError::StructureMismatch(format!(
				"the output of {produced_by} does not parse: {location}"
			)))
		}
	}
}

/// The start of the line containing `offset`, if only indentation (spaces and tabs) precedes `offset` on that line.
pub(crate) fn indentation_start(text: &str, offset: usize) -> Option<usize> {
	let start = line_start(text, offset);

	text[start..offset].bytes().all(|byte| byte == b' ' || byte == b'\t').then_some(start)
}

/// The byte offset of the start of the line containing `offset`.
pub(crate) fn line_start(text: &str, offset: usize) -> usize {
	text[..offset].rfind('\n').map_or(0, |index| index + 1)
}

/// Converts every `\n` that is not already part of a `\r\n` to `\r\n`.
fn to_crlf(text: &str) -> Cow<'_, str> {
	let bare_line_feeds = text.match_indices('\n').filter(|&(index, _)| !text[..index].ends_with('\r')).count();

	if bare_line_feeds == 0 {
		return Cow::Borrowed(text);
	}

	let mut converted = String::with_capacity(text.len() + bare_line_feeds);
	let mut previous = '\0';

	for character in text.chars() {
		if character == '\n' && previous != '\r' {
			converted.push('\r');
		}

		converted.push(character);
		previous = character;
	}

	Cow::Owned(converted)
}

/// Whether the first line break of `text` is `\r\n`.
pub(crate) fn uses_crlf(text: &str) -> bool {
	text.find('\n').is_some_and(|index| text[..index].ends_with('\r'))
}

/// Converts the line breaks of `text` to `\r\n` if `crlf`, else to `\n`.
///
/// This never changes the meaning of Rust source: `\r\n` is normalized to `\n` before tokenization.
pub(crate) fn with_line_breaks(text: &str, crlf: bool) -> Cow<'_, str> {
	if crlf {
		to_crlf(text)
	} else if text.contains("\r\n") {
		Cow::Owned(text.replace("\r\n", "\n"))
	} else {
		Cow::Borrowed(text)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use syn::spanned::Spanned;

	#[test]
	fn ensure_parses_reports_structure_mismatch() {
		assert!(ensure_parses("fn a() {}", "test").is_ok());
		assert!(matches!(ensure_parses("fn a( {}", "test"), Err(FormatError::StructureMismatch(_))));
	}

	#[test]
	fn line_endings() {
		assert!(!uses_crlf("a\nb\r\n"));
		assert!(uses_crlf("a\r\nb\n"));
		assert!(!uses_crlf("no line break"));
		assert_eq!(to_crlf("a\nb\r\nc\n"), "a\r\nb\r\nc\r\n");
		assert!(matches!(to_crlf("a\r\nb"), Cow::Borrowed(_)));
		assert_eq!(to_crlf("\n\n"), "\r\n\r\n");
		assert_eq!(to_crlf("é\nü"), "é\r\nü");
		assert_eq!(with_line_breaks("a\nb\r\n", true), "a\r\nb\r\n");
		assert_eq!(with_line_breaks("a\nb\r\n", false), "a\nb\n");
		assert!(matches!(with_line_breaks("a\nb", false), Cow::Borrowed(_)));
	}

	#[test]
	fn line_helpers() {
		let text = "a\n\t  b c\r\nd";

		assert_eq!(line_start(text, 0), 0);
		assert_eq!(line_start(text, 5), 2);
		assert_eq!(line_start(text, text.len()), 10);
		assert_eq!(indentation_start(text, 5), Some(2));
		assert_eq!(indentation_start(text, 7), None);
		assert_eq!(indentation_start(text, 10), Some(10));
	}

	#[test]
	fn parse_errors_report_editor_locations() {
		let error = Parsed::parse("fn a() {}\nfn b( {}\n").err().unwrap();

		assert!(matches!(error, FormatError::Parse { line: 2, .. }), "{error:?}");

		// the BOM counts as a character of the first line
		let with_bom = Parsed::parse("\u{feff}fn match() {}").err().unwrap();
		let without_bom = Parsed::parse("fn match() {}").err().unwrap();

		match (with_bom, without_bom) {
			(FormatError::Parse { column: with, .. }, FormatError::Parse { column: without, .. }) => {
				assert_eq!(without, 4);
				assert_eq!(with, without + 1);
			}
			other => panic!("unexpected errors: {other:?}"),
		}
	}

	#[test]
	fn ranges_compensate_for_bom_and_shebang() {
		let text = "\u{feff}#!/usr/bin/env run-cargo-script\nfn a() {}\n";
		let parsed = Parsed::parse(text).unwrap();
		let range = parsed.range(parsed.file.items[0].span());

		assert_eq!(&text[range], "fn a() {}");
	}

	#[test]
	fn ranges_without_prefix() {
		let text = "/// doc\nstruct S;\n";
		let parsed = Parsed::parse(text).unwrap();
		let range = parsed.range(parsed.file.items[0].span());

		assert_eq!(&text[range], "/// doc\nstruct S;");
	}
}
