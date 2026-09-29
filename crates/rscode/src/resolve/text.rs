//! Scanning source text for the few things name resolution needs that the model does not record.

/// The position of the `]` that closes an attribute whose `[` was consumed, past nested delimiters and strings.
fn closing_bracket(text: &str) -> Option<usize> {
	let mut depth = 0usize;
	let mut chars = text.char_indices();

	while let Some((index, char)) = chars.next() {
		match char {
			'"' => skip_string(&mut chars),
			'[' | '(' | '{' => depth += 1,
			']' if depth == 0 => return Some(index),
			']' | ')' | '}' => depth = depth.saturating_sub(1),
			_ => {}
		}
	}

	None
}

/// The contents of the inner attributes at the start of a source file (`no_std` for `#![no_std]`).
pub(super) fn inner_attributes(source: &str) -> Vec<&str> {
	let mut rest = source.strip_prefix('\u{feff}').unwrap_or(source);

	// a shebang line (`#!/usr/bin/env ...`), which is not an attribute
	if let Some(after) = rest.strip_prefix("#!")
		&& !skip_trivia(after).starts_with('[')
	{
		rest = after.find('\n').map_or("", |end| &after[end..]);
	}

	leading_attributes(rest, "#!")
}

pub(super) fn is_ident_char(char: char) -> bool {
	char.is_alphanumeric() || char == '_'
}

/// The contents of the attributes starting with `marker` (`#` or `#!`) at the start of some text.
fn leading_attributes<'a>(mut rest: &'a str, marker: &str) -> Vec<&'a str> {
	let mut attributes = Vec::new();

	while let Some(after) = skip_trivia(rest).strip_prefix(marker)
		&& let Some(body) = skip_trivia(after).strip_prefix('[')
		&& let Some(end) = closing_bracket(body)
	{
		attributes.push(&body[..end]);
		rest = &body[end + 1..];
	}

	attributes
}

/// The contents of the outer attributes at the start of an item's text (`derive(Debug)` for `#[derive(Debug)]`).
pub(super) fn outer_attributes(item: &str) -> Vec<&str> {
	leading_attributes(item, "#")
}

/// Skips the rest of a (possibly nested) block comment whose `/*` was already consumed.
fn skip_block_comment(text: &str) -> &str {
	let mut depth = 1;
	let mut index = 0;
	let bytes = text.as_bytes();

	while index < bytes.len() {
		match (bytes[index], bytes.get(index + 1)) {
			(b'/', Some(b'*')) => {
				depth += 1;
				index += 2;
			}

			(b'*', Some(b'/')) => {
				depth -= 1;
				index += 2;

				if depth == 0 {
					return &text[index..];
				}
			}

			_ => index += 1,
		}
	}

	""
}

/// Skips the rest of a string literal whose opening quote was consumed.
fn skip_string(chars: &mut std::str::CharIndices<'_>) {
	while let Some((_, char)) = chars.next() {
		match char {
			'\\' => {
				chars.next();
			}

			'"' => return,
			_ => {}
		}
	}
}

/// Skips whitespace and comments.
pub(super) fn skip_trivia(mut text: &str) -> &str {
	loop {
		text = text.trim_start();

		if let Some(comment) = text.strip_prefix("//") {
			text = comment.find('\n').map_or("", |end| &comment[end..]);
		} else if let Some(comment) = text.strip_prefix("/*") {
			text = skip_block_comment(comment);
		} else {
			return text;
		}
	}
}

pub(super) fn strip_keyword<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
	text.strip_prefix(keyword).filter(|rest| !rest.starts_with(is_ident_char))
}

/// The identifiers in some tokens, outside of string literals.
pub(super) fn words(text: &str) -> Vec<&str> {
	let mut words = Vec::new();
	let mut chars = text.char_indices();
	let mut start = None;

	while let Some((index, char)) = chars.next() {
		if is_ident_char(char) {
			start.get_or_insert(index);
			continue;
		}

		if let Some(start) = start.take() {
			words.push(&text[start..index]);
		}

		if char == '"' {
			skip_string(&mut chars);
		}
	}

	words.extend(start.map(|start| &text[start..]));
	words
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn finds_inner_attributes() {
		let source = "#!/usr/bin/env run\n//! Docs.\n#![no_std]\n/* c */ #! [cfg_attr(x, doc = \"]\")]\n#![a[b]] mod m; #![late]";

		assert_eq!(inner_attributes(source), ["no_std", "cfg_attr(x, doc = \"]\")", "a[b]"]);
		assert_eq!(inner_attributes("\u{feff}#![no_std]"), ["no_std"]);
		assert_eq!(inner_attributes("#![unclosed"), Vec::<&str>::new());
		assert_eq!(inner_attributes(""), Vec::<&str>::new());
	}

	#[test]
	fn finds_outer_attributes() {
		let item = "/// Docs.\n#[proc_macro_derive(Thing, attributes(thing))]\n#[doc = \"]\"] pub fn derive(input: T) -> T { #[a] input }";

		assert_eq!(outer_attributes(item), ["proc_macro_derive(Thing, attributes(thing))", "doc = \"]\""]);
		assert_eq!(outer_attributes("#![inner] fn f() {}"), Vec::<&str>::new());
	}

	#[test]
	fn finds_words_outside_of_strings() {
		assert_eq!(
			words("cfg_attr(not(feature = \"no_std\"), no_std)"),
			["cfg_attr", "not", "feature", "no_std"]
		);
		assert_eq!(words("a\"\\\"b\"c d"), ["a", "c", "d"]);
		assert_eq!(words(""), Vec::<&str>::new());
	}

	#[test]
	fn skips_comments() {
		assert_eq!(skip_trivia(" // a\n /* b /* c */ */ x"), "x");
		assert_eq!(skip_trivia("/* unclosed"), "");
	}
}
