//! String literals and the inline arguments of format strings (`"{name}"`, `"{:>width$}"`).

/// The `name$` parameters (width and precision) of a format spec.
fn dollar_parameters(spec: &str) -> impl Iterator<Item = (usize, &str)> {
	spec.match_indices('$').filter_map(|(end, _)| {
		let start = (spec[..end].char_indices().rev())
			.take_while(|&(_, char)| char.is_alphanumeric() || char == '_')
			.last()
			.map_or(end, |(start, _)| start);

		let name = &spec[start..end];

		is_identifier(name).then_some((start, name))
	})
}

/// The identifiers used by the placeholders of a format string (`{name}`, `{name:?}`, `{:width$}`, `{:.prec$}`), with
/// their offsets in `content` (the source text of the literal between its quotes, where `raw` tells whether escapes
/// are possible). Positional arguments (`{}`, `{0}`) are skipped.
pub(super) fn inline_arguments(content: &str, raw: bool) -> Vec<(usize, &str)> {
	let bytes = content.as_bytes();
	let mut found = Vec::new();
	let mut index = 0;

	while index < bytes.len() {
		match bytes[index] {
			b'{' if bytes.get(index + 1) == Some(&b'{') => index += 2,

			// `\u{..}` is not a placeholder
			b'\\' if !raw => {
				index += match content[index + 1..].strip_prefix("u{").and_then(|rest| rest.find('}')) {
					Some(end) => end + 4,
					None => 2,
				};
			}

			b'{' => {
				let start = index + 1;

				let Some(length) = content[start..].find('}') else {
					break;
				};

				let placeholder = &content[start..start + length];
				let (argument, spec) = placeholder.split_once(':').unwrap_or((placeholder, ""));

				if is_identifier(argument) {
					found.push((start, argument));
				}

				let spec_start = start + argument.len() + 1;

				found.extend(dollar_parameters(spec).map(|(offset, name)| (spec_start + offset, name)));
				index = start + length + 1;
			}

			_ => index += 1,
		}
	}

	found
}

/// Whether a text is a (non-raw) identifier other than `_`.
fn is_identifier(text: &str) -> bool {
	let mut chars = text.chars();

	chars.next().is_some_and(|first| first.is_alphabetic() || first == '_') && chars.all(|char| char.is_alphanumeric() || char == '_') && text != "_"
}

/// The content of a string literal's source text (between its quotes) and the content's offset in the text, and
/// whether the literal is raw. `None` for other literals (byte strings, C strings, ...).
pub(super) fn literal_content(text: &str) -> Option<(&str, usize, bool)> {
	if let Some(rest) = text.strip_prefix('r') {
		let hashes = rest.len() - rest.trim_start_matches('#').len();
		let open = 1 + hashes + 1;
		let close = 1 + hashes;

		if text.as_bytes().get(open - 1) != Some(&b'"') || !text[open..].ends_with(&format!("\"{}", "#".repeat(hashes))) {
			return None;
		}

		return Some((text.get(open..text.len().checked_sub(close)?)?, open, true));
	}

	let content = text.strip_prefix('"')?.strip_suffix('"')?;

	Some((content, 1, false))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn finds_inline_arguments() {
		fn found(content: &str) -> Vec<(&str, usize)> {
			inline_arguments(content, false)
				.into_iter()
				.map(|(offset, name)| (name, offset))
				.collect()
		}

		assert_eq!(found("{a} and {b:?} and {}"), [("a", 1), ("b", 9)]);
		assert_eq!(found("{0} {x:>width$.prec$}"), [("x", 5), ("width", 8), ("prec", 15)]);
		assert_eq!(found("{{escaped}} {{{real}}}"), [("real", 15)]);
		assert_eq!(found("{:1$} {_} {9x} {ünï}"), [("ünï", 16)]);
		assert_eq!(found("\\u{abc} {d} \\n{e}"), [("d", 9), ("e", 15)]);
		assert_eq!(found("{unclosed"), []);
		assert_eq!(inline_arguments("\\{x}", true), [(2, "x")]);
	}

	#[test]
	fn finds_the_content_of_string_literals() {
		assert_eq!(literal_content("\"a{b}\""), Some(("a{b}", 1, false)));
		assert_eq!(literal_content("r\"a\""), Some(("a", 2, true)));
		assert_eq!(literal_content("r##\"a\"#b\"##"), Some(("a\"#b", 4, true)));
		assert_eq!(literal_content("\"\""), Some(("", 1, false)));
		assert_eq!(literal_content("b\"a\""), None);
		assert_eq!(literal_content("c\"a\""), None);
		assert_eq!(literal_content("r#\"a\""), None);
		assert_eq!(literal_content("'a'"), None);
		assert_eq!(literal_content("r"), None);
	}
}
