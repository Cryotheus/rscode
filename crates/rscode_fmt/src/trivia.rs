//! Detection of comments, which are invisible to `syn`.

use crate::source::bom_len;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use std::ops::Range;
use std::str::FromStr;

/// The length of a (nested) block comment at the start of `text`, including `*/`. `None` if unterminated.
fn block_comment_len(text: &str) -> Option<usize> {
	let bytes = text.as_bytes();
	let mut depth = 0usize;
	let mut index = 0;

	while index + 1 < bytes.len() {
		match &bytes[index..index + 2] {
			b"/*" => {
				depth += 1;
				index += 2;
			}
			b"*/" => {
				depth = depth.saturating_sub(1);
				index += 2;

				if depth == 0 {
					return Some(index);
				}
			}
			_ => index += 1,
		}
	}

	None
}

/// The length of a character literal at the start of `text` (starting with `'`), or 1 for the quote of a lifetime or
/// label.
fn char_or_lifetime_len(text: &str) -> usize {
	let mut characters = text[1..].chars();

	match (characters.next(), characters.next()) {
		// an escape: the closing quote follows within the longest escape, `\u{10FFFF}`
		(Some('\\'), Some(_)) => {
			let closing_quote = text[1..].char_indices().skip(2).take(10).find(|&(_, character)| character == '\'');

			closing_quote.map_or(1, |(index, _)| index + 2)
		}
		(Some('\\'), None) => text.len(),
		(Some(character), Some('\'')) => 1 + character.len_utf8() + 1,
		_ => 1,
	}
}

/// Whether the source contains any non-doc comments (`//`, `/* */`), which prettyplease would discard.
///
/// Doc comments (`///`, `//!`, `/** */`, `/*! */`) are attributes and are not counted.
/// A leading byte order mark and shebang line are skipped, like `syn::parse_file` does.
pub fn contains_comments(source: &str) -> bool {
	let text = &source[prefix_len(source)..];

	lexed_comments(text).unwrap_or_else(|| scanned_comments(text))
}

/// Whether the text consists only of whitespace, as defined by the Rust lexer.
fn is_blank(text: &str) -> bool {
	text.chars().all(is_whitespace)
}

/// Whether text starting with `/*` is a doc comment: `/**` (but not `/***` or the empty comment `/**/`) or `/*!`.
fn is_doc_block_comment(text: &str) -> bool {
	text.starts_with("/**") && !text.starts_with("/***") && !text.starts_with("/**/") || text.starts_with("/*!")
}

/// Whether text starting with `//` is a doc comment: `///` (but not `////`) or `//!`.
fn is_doc_line_comment(text: &str) -> bool {
	text.starts_with("///") && !text.starts_with("////") || text.starts_with("//!")
}

fn is_whitespace(character: char) -> bool {
	// Rust treats the left-to-right and right-to-left marks as whitespace
	character.is_whitespace() || character == '\u{200e}' || character == '\u{200f}'
}

/// Finds comments by lexing: every token is covered by its span (the tokens of a doc comment span the whole comment),
/// so any text between tokens that is not whitespace is a comment. `None` if the text cannot be lexed.
fn lexed_comments(text: &str) -> Option<bool> {
	let tokens = TokenStream::from_str(text).ok()?;
	let mut ranges = token_ranges(tokens);
	let mut covered = 0;

	ranges.sort_unstable_by_key(|range| range.start);

	for range in ranges {
		if range.start > covered && !is_blank(&text[covered..range.start]) {
			return Some(true);
		}

		covered = covered.max(range.end);
	}

	Some(!is_blank(&text[covered.min(text.len())..]))
}

/// The length of the text `syn::parse_file` skips before parsing: a byte order mark, and a shebang line (without its
/// line break).
pub(crate) fn prefix_len(text: &str) -> usize {
	let bom = bom_len(text);
	let rest = &text[bom..];

	// `#![attribute]` is an inner attribute, not a shebang
	if rest.starts_with("#!") && !skip_trivia(&rest[2..]).starts_with('[') {
		bom + rest.find('\n').unwrap_or(rest.len())
	} else {
		bom
	}
}

/// The length of a string literal at the start of `text` (starting with `"`), including the closing quote.
fn quoted_len(text: &str) -> usize {
	let bytes = text.as_bytes();
	let mut index = 1;

	while index < bytes.len() {
		match bytes[index] {
			b'\\' => index += 2,
			b'"' => return index + 1,
			_ => index += 1,
		}
	}

	text.len()
}

/// The length of a raw string after its prefix: `#`s, then a quoted string, then the same number of `#`s.
/// `None` if `text` does not start a raw string (as in the raw identifier `r#match`).
fn raw_string_len(text: &str) -> Option<usize> {
	let hashes = text.len() - text.trim_start_matches('#').len();
	let body = text[hashes..].strip_prefix('"')?;
	let terminator = format!("\"{}", "#".repeat(hashes));

	Some(match body.find(&terminator) {
		Some(index) => hashes + 1 + index + terminator.len(),
		None => text.len(),
	})
}

/// Finds comments by scanning, skipping string, byte string, raw string, and character literals.
/// Used for text that cannot be lexed, such as text with unterminated literals.
fn scanned_comments(text: &str) -> bool {
	let mut index = 0;

	while let Some(first) = text[index..].chars().next() {
		let rest = &text[index..];

		index += match first {
			'/' if rest.starts_with("//") => {
				if !is_doc_line_comment(rest) {
					return true;
				}

				rest.find('\n').unwrap_or(rest.len())
			}
			'/' if rest.starts_with("/*") => {
				if !is_doc_block_comment(rest) {
					return true;
				}

				block_comment_len(rest).unwrap_or(rest.len())
			}
			'"' => quoted_len(rest),
			'\'' => char_or_lifetime_len(rest),
			character if character == '_' || character.is_alphabetic() => word_or_literal_len(rest),
			character => character.len_utf8(),
		};
	}

	false
}

/// Skips whitespace and non-doc comments, like `syn` does when looking for a shebang.
fn skip_trivia(mut text: &str) -> &str {
	loop {
		if text.starts_with("//") && !is_doc_line_comment(text) {
			match text.find('\n') {
				Some(index) => text = &text[index + 1..],
				None => return "",
			}
		} else if text.starts_with("/*") && !is_doc_block_comment(text) {
			match block_comment_len(text) {
				Some(len) => text = &text[len..],
				None => return text,
			}
		} else {
			match text.chars().next() {
				Some(character) if is_whitespace(character) => text = &text[character.len_utf8()..],
				_ => return text,
			}
		}
	}
}

/// The byte ranges of every token, including the delimiters of groups.
fn token_ranges(tokens: TokenStream) -> Vec<Range<usize>> {
	let mut ranges = Vec::new();
	let mut stack = vec![tokens.into_iter()];

	// iterative, as deeply nested groups must not overflow the stack
	while let Some(tokens) = stack.last_mut() {
		match tokens.next() {
			Some(TokenTree::Group(group)) => {
				ranges.push(group.span_open().byte_range());
				ranges.push(group.span_close().byte_range());
				stack.push(group.stream().into_iter());
			}
			Some(token) => ranges.push(token.span().byte_range()),
			None => {
				stack.pop();
			}
		}
	}

	ranges
}

/// The length of an identifier, keyword, or prefixed literal (`r"…"`, `br#"…"#`, `b"…"`, `c"…"`, `b'…'`) at the start
/// of `text`.
fn word_or_literal_len(text: &str) -> usize {
	let is_word_character = |character: char| character == '_' || character.is_alphanumeric();
	let word_len = text.find(|character: char| !is_word_character(character)).unwrap_or(text.len());
	let rest = &text[word_len..];

	match &text[..word_len] {
		"r" | "br" | "cr" => word_len + raw_string_len(rest).unwrap_or(0),
		"b" | "c" if rest.starts_with('"') => word_len + quoted_len(rest),
		"b" if rest.starts_with('\'') => word_len + char_or_lifetime_len(rest),
		_ => word_len,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Lexable sources and whether they contain non-doc comments.
	const CASES: &[(&str, bool)] = &[
		("", false),
		("   \n\t\r\n", false),
		("fn a() {}", false),
		("// comment", true),
		("fn a() {} // trailing", true),
		("/* block */ fn a() {}", true),
		("fn a() { /* inside */ }", true),
		("/// doc\nfn a() {}", false),
		("//! inner doc\nfn a() {}", false),
		("/** block doc */ fn a() {}", false),
		("/*! inner block doc */", false),
		("//// four slashes is not a doc comment", true),
		("/*** three stars is not a doc comment */", true),
		("/**/", true),
		("/***/", true),
		("/*!*/", false),
		("/** doc /* nested */ still doc */ fn a() {}", false),
		("/* comment /* nested */ still comment */", true),
		("/// doc\n/// more\n#[derive(Debug)]\nstruct S;", false),
		("/// doc\r\nfn a() {}\r\n", false),
		("fn a() { let s = \"// not a comment\"; }", false),
		("fn a() { let s = \"/* not a comment */\"; }", false),
		("fn a() { let s = \"escaped \\\" // still a string\"; }", false),
		("fn a() { let s = r##\"// \"# still raw\"##; }", false),
		("fn a() { let s = r#\"x\"#; } // after a raw string", true),
		("fn a() { let s = br##\"/* \"# \"##; }", false),
		("fn a() { let s = b\"//\"; let t = c\"/*\"; }", false),
		("fn a() { let s = r\"\\\"; } // after a raw string ending in a backslash", true),
		("fn a() { let c = '\"'; } // comment", true),
		("fn a() { let c = '/'; let d = '/'; }", false),
		("fn a() { let c = '\\''; let s = \"// x\"; }", false),
		("fn a() { let c = b'\\''; let s = \"//\"; }", false),
		("fn a() { let c = '\\u{1F980}'; let s = \"//\"; }", false),
		("fn a() { let c = '🦀'; let s = \"//\"; }", false),
		("fn a<'a>(x: &'a str) -> &'a str { x }", false),
		("fn a<'a>(x: &'a str) -> &'a str { x } // c", true),
		("fn a() { 'outer: loop { break 'outer; } }", false),
		("fn a() { let x = 1 / 2; }", false),
		("fn a() { let x = 1 /* c */ / 2; }", true),
		("fn r#match() { r#match() }", false),
		("fn a() { let r#type = 1; } // c", true),
		("#![allow(dead_code)]", false),
		("#![allow(dead_code)] // c", true),
		("mac! { // inside a macro\n }", true),
		("mac! { /// doc inside a macro\n x }", false),
		("#!/usr/bin/env run-cargo-script\nfn a() {}", false),
		("#!/usr/bin/env foo // part of the shebang\nfn a() {}", false),
		("#!/usr/bin/env foo\n// c\nfn a() {}", true),
		("#! [allow(dead_code)] // c", true),
		("#!/*c*/[allow(dead_code)]", true),
		("\u{feff}fn a() {}", false),
		("\u{feff}// c\nfn a() {}", true),
		("\u{feff}#!/bin/sh\nfn a() {}", false),
		("fn a() {\u{a0}}", false),
		("fn a() {}\u{200e}", false),
	];

	/// Sources that cannot be lexed and whether they contain non-doc comments.
	const UNLEXABLE: &[(&str, bool)] = &[
		("fn a() { \"unterminated // string", false),
		("fn a( // unbalanced", true),
		("fn a() { let s = \"x\"; // c\n", true),
		("fn a() } /* c */", true),
		("fn a() { r#\"unterminated raw // string", false),
		("fn a() { '\\u{1F980 } // c", true),
		("€ // c", true),
		("€ /// doc", false),
	];

	#[test]
	fn block_comments() {
		assert_eq!(block_comment_len("/**/"), Some(4));
		assert_eq!(block_comment_len("/* a /* b */ c */ d"), Some(17));
		assert_eq!(block_comment_len("/*/"), None);
		assert_eq!(block_comment_len("/* /* */"), None);
	}

	#[test]
	fn lexer_detects_comments() {
		for &(source, expected) in CASES {
			let text = &source[prefix_len(source)..];

			assert_eq!(lexed_comments(text), Some(expected), "{source:?}");
			assert_eq!(contains_comments(source), expected, "{source:?}");
		}
	}

	#[test]
	fn literals() {
		assert_eq!(quoted_len("\"a\\\"b\" c"), 6);
		assert_eq!(quoted_len("\"unterminated"), 13);
		assert_eq!(char_or_lifetime_len("'a' b"), 3);
		assert_eq!(char_or_lifetime_len("'a b"), 1);
		assert_eq!(char_or_lifetime_len("'\\'' b"), 4);
		assert_eq!(char_or_lifetime_len("'\\u{10FFFF}' b"), 12);
		assert_eq!(char_or_lifetime_len("'é' b"), 4);
		assert_eq!(word_or_literal_len("r#\"a\"# b"), 6);
		assert_eq!(word_or_literal_len("r#match b"), 1);
		assert_eq!(word_or_literal_len("br\"a\" b"), 5);
		assert_eq!(word_or_literal_len("b'x' b"), 4);
		assert_eq!(word_or_literal_len("bar\"x\""), 3);
		assert_eq!(raw_string_len("##\"a\"# \"##"), Some(10));
		assert_eq!(raw_string_len("#x"), None);
	}

	#[test]
	fn prefix() {
		assert_eq!(prefix_len(""), 0);
		assert_eq!(prefix_len("fn a() {}"), 0);
		assert_eq!(prefix_len("\u{feff}fn a() {}"), 3);
		assert_eq!(prefix_len("#!/bin/sh\nfn a() {}"), 9);
		assert_eq!(prefix_len("\u{feff}#!/bin/sh\nfn a() {}"), 12);
		assert_eq!(prefix_len("#!/bin/sh"), 9);
		assert_eq!(prefix_len("#![allow(x)]"), 0);
		assert_eq!(prefix_len("#!\n[allow(x)]"), 0);
		assert_eq!(prefix_len("#! // c\n /* c */ [allow(x)]"), 0);
		assert_eq!(prefix_len("#! /// doc\n[allow(x)]"), 10);
	}

	#[test]
	fn scanner_agrees_with_lexer() {
		for &(source, expected) in CASES {
			let text = &source[prefix_len(source)..];

			assert_eq!(scanned_comments(text), expected, "{source:?}");
		}
	}

	#[test]
	fn scanner_handles_unlexable_text() {
		for &(source, expected) in UNLEXABLE {
			let text = &source[prefix_len(source)..];

			assert_eq!(lexed_comments(text), None, "{source:?} should not be lexable");
			assert_eq!(contains_comments(source), expected, "{source:?}");
		}
	}
}
