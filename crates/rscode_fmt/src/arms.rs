//! Layout of `match` arms after formatting: blank lines around arms spanning several lines.
//!
//! Formatters put every arm on its own line; rustfmt keeps the blank lines between arms as they are (prettyplease drops
//! them). This pass lays out the arms of every `match` like `rscode_sort` lays out the items of a group: one-line arms
//! follow each other directly, and an arm spanning several lines, or with attributes or comments above it, is separated
//! from its neighbours by a blank line. rustfmt keeps that layout, so formatting the result again changes nothing.
//!
//! Like rustfmt, the pass leaves code under `#[rustfmt::skip]` (on an item, a `let` statement, or the `match` itself,
//! or `#![rustfmt::skip]` in a file) as it is. It also leaves the line break after an arm whose trailing `/* */`
//! comment spans lines: rustfmt removes a blank line there.

use crate::FormatError;
use crate::source::Parsed;
use crate::source::parse_output;
use crate::trivia::block_comment_len;
use crate::trivia::is_whitespace;
use crate::trivia::line_comment_len;
use std::ops::Range;
use syn::spanned::Spanned;
use syn::visit;
use syn::visit::Visit;

/// Separates the arms of every `match` in the formatted `text` (see the [module docs](self)): two consecutive arms are
/// separated by a blank line iff one of them spans several lines, has attributes, or has comments above it. Only the
/// whitespace between two arms changes (comments trailing an arm and blank lines among the comments above an arm stay
/// as they are); the text between `{` and the first arm and between the last arm and `}` never does. Matches inside
/// macro invocations and under `#[rustfmt::skip]` are left alone.
///
/// `produced_by` names the formatter in the error when `text` does not parse.
pub(crate) fn separate_match_arms(text: &str, produced_by: &str) -> Result<String, FormatError> {
	let parsed = parse_output(text, produced_by)?;
	let mut layout = Layout { text, parsed: &parsed, edits: Vec::new() };

	layout.visit_file(&parsed.file);

	let mut edits = layout.edits;

	if edits.is_empty() {
		return Ok(text.to_owned());
	}

	// the edits are disjoint (the arms of a nested match lie inside an arm of the outer one), but not in order
	edits.sort_by_key(|(range, _)| range.start);

	let mut separated = String::with_capacity(text.len() + edits.len());
	let mut copied = 0;

	for (range, replacement) in edits {
		separated.push_str(&text[copied..range.start]);
		separated.push_str(&replacement);
		copied = range.end;
	}

	separated.push_str(&text[copied..]);

	Ok(separated)
}

/// Collects the edits laying out the arms of every `match` of a file.
struct Layout<'a> {
	text: &'a str,
	parsed: &'a Parsed,

	/// Replacements of the whitespace between two arms.
	edits: Vec<(Range<usize>, String)>,
}

impl Layout<'_> {
	/// Lays out the arms of one `match`.
	fn lay_out(&mut self, expr: &syn::ExprMatch) {
		let Some(first) = expr.arms.first() else {
			return;
		};

		// the spans of arms cover their attributes, pattern, guard, body, and comma; the gap above the first arm starts
		// after the `{` and the inner attributes of the match
		let open = (expr.attrs.iter())
			.filter(|attr| matches!(attr.style, syn::AttrStyle::Inner(_)))
			.map(|attr| self.parsed.range(attr.span()).end)
			.fold(self.parsed.range(expr.brace_token.span.open()).end, usize::max);
		let mut range = self.parsed.range(first.span());
		let mut multi_line = self.spans_lines(&range);
		let mut decorated = !first.attrs.is_empty() || self.first_arm_has_comments(open..range.start);

		for arm in &expr.arms[1..] {
			let next_range = self.parsed.range(arm.span());
			let next_multi_line = self.spans_lines(&next_range);
			let gap = range.end..next_range.start;

			// the gap holds only whitespace and comments, unless an arm's span is incomplete: then it is left alone
			let pieces = trivia(self.text, gap).unwrap_or_default();
			let line_break = pieces.iter().position(|piece| piece.breaks_line(self.text));
			let next_decorated = !arm.attrs.is_empty()
				|| line_break.is_some_and(|index| pieces[index + 1..].iter().any(|piece| piece.comment));

			// only the first whitespace with a line break changes: everything before it trails the previous arm, and
			// blank lines among the comments above the next arm (a section header) stay; after a trailing comment
			// spanning lines, nothing changes, as rustfmt removes a blank line there
			if let Some(index) = line_break
				&& !pieces[..index].iter().any(|piece| piece.comment && self.spans_lines(&piece.range))
			{
				let piece = pieces[index].range.clone();
				let blank_line = multi_line || next_multi_line || decorated || next_decorated;

				if let Some(replacement) = separation(&self.text[piece.clone()], blank_line) {
					self.edits.push((piece, replacement));
				}
			}

			range = next_range;
			multi_line = next_multi_line;
			decorated = next_decorated;
		}
	}

	/// Whether the text in `range` spans several lines.
	fn spans_lines(&self, range: &Range<usize>) -> bool {
		self.text.get(range.clone()).is_some_and(|text| text.contains('\n'))
	}

	/// Whether comments directly above the first arm decorate it, given the gap from the `{` to the arm: comments on
	/// the line of the `{` trail it, and comments separated from the arm by a blank line are the header of the match,
	/// like the header of a container in `rscode_sort`.
	fn first_arm_has_comments(&self, gap: Range<usize>) -> bool {
		let Some(pieces) = trivia(self.text, gap) else {
			return false;
		};
		let Some(line_break) = pieces.iter().position(|piece| piece.breaks_line(self.text)) else {
			return false;
		};
		let above = &pieces[line_break + 1..];
		let after_blank_line =
			above.iter().rposition(|piece| piece.has_blank_line(self.text)).map_or(0, |index| index + 1);

		above[after_blank_line..].iter().any(|piece| piece.comment)
	}
}

impl<'ast> Visit<'ast> for Layout<'_> {
	fn visit_file(&mut self, file: &'ast syn::File) {
		if !skipped(&file.attrs) {
			visit::visit_file(self, file);
		}
	}

	fn visit_item(&mut self, item: &'ast syn::Item) {
		if !skipped(item_attrs(item)) {
			visit::visit_item(self, item);
		}
	}

	fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
		if !skipped(impl_item_attrs(item)) {
			visit::visit_impl_item(self, item);
		}
	}

	fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
		if !skipped(trait_item_attrs(item)) {
			visit::visit_trait_item(self, item);
		}
	}

	fn visit_local(&mut self, local: &'ast syn::Local) {
		if !skipped(&local.attrs) {
			visit::visit_local(self, local);
		}
	}

	fn visit_expr_match(&mut self, expr: &'ast syn::ExprMatch) {
		if !skipped(&expr.attrs) {
			self.lay_out(expr);
			visit::visit_expr_match(self, expr);
		}
	}
}

/// Whether the attributes tell rustfmt to leave the code as it is: `#[rustfmt::skip]`, the older `#[rustfmt_skip]`,
/// or either behind `#[cfg_attr(rustfmt, ..)]`.
fn skipped(attrs: &[syn::Attribute]) -> bool {
	attrs.iter().any(|attr| {
		if is_skip(attr.path()) {
			return true;
		}

		if !attr.path().is_ident("cfg_attr") {
			return false;
		}

		let Ok(metas) =
			attr.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
		else {
			return false;
		};
		let mut metas = metas.iter();

		metas.next().is_some_and(|predicate| predicate.path().is_ident("rustfmt"))
			&& metas.any(|meta| is_skip(meta.path()))
	})
}

/// Whether a path is `rustfmt::skip` or `rustfmt_skip`.
fn is_skip(path: &syn::Path) -> bool {
	path.is_ident("rustfmt_skip")
		|| (path.segments.len() == 2 && path.segments[0].ident == "rustfmt" && path.segments[1].ident == "skip")
}

/// The attributes of an item (an item `syn` does not model has none).
fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
	match item {
		syn::Item::Const(item) => &item.attrs,
		syn::Item::Enum(item) => &item.attrs,
		syn::Item::ExternCrate(item) => &item.attrs,
		syn::Item::Fn(item) => &item.attrs,
		syn::Item::ForeignMod(item) => &item.attrs,
		syn::Item::Impl(item) => &item.attrs,
		syn::Item::Macro(item) => &item.attrs,
		syn::Item::Mod(item) => &item.attrs,
		syn::Item::Static(item) => &item.attrs,
		syn::Item::Struct(item) => &item.attrs,
		syn::Item::Trait(item) => &item.attrs,
		syn::Item::TraitAlias(item) => &item.attrs,
		syn::Item::Type(item) => &item.attrs,
		syn::Item::Union(item) => &item.attrs,
		syn::Item::Use(item) => &item.attrs,
		_ => &[],
	}
}

/// The attributes of an item of an `impl` block.
fn impl_item_attrs(item: &syn::ImplItem) -> &[syn::Attribute] {
	match item {
		syn::ImplItem::Const(item) => &item.attrs,
		syn::ImplItem::Fn(item) => &item.attrs,
		syn::ImplItem::Type(item) => &item.attrs,
		syn::ImplItem::Macro(item) => &item.attrs,
		_ => &[],
	}
}

/// The attributes of an item of a trait.
fn trait_item_attrs(item: &syn::TraitItem) -> &[syn::Attribute] {
	match item {
		syn::TraitItem::Const(item) => &item.attrs,
		syn::TraitItem::Fn(item) => &item.attrs,
		syn::TraitItem::Type(item) => &item.attrs,
		syn::TraitItem::Macro(item) => &item.attrs,
		_ => &[],
	}
}

/// The whitespace separating two arms by a blank line, or not, in place of a whitespace `piece` containing a line
/// break: the piece's text up to and including its first line break, one more line break of the same kind (`\n` or
/// `\r\n`) for a blank line, and the piece's text after its last line break (the indentation of the next arm). `None`
/// if the piece has no line break or already separates the arms that way.
fn separation(piece: &str, blank_line: bool) -> Option<String> {
	let first = piece.find('\n')?;
	let head = &piece[..=first];
	let line_break = if head.ends_with("\r\n") { "\r\n" } else { "\n" };
	let tail = &piece[piece.rfind('\n').unwrap_or(first) + 1..];
	let mut separation = String::with_capacity(head.len() + line_break.len() + tail.len());

	separation.push_str(head);

	if blank_line {
		separation.push_str(line_break);
	}

	separation.push_str(tail);

	(separation != piece).then_some(separation)
}

/// A run of whitespace, or one comment, between two arms.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Piece {
	range: Range<usize>,
	comment: bool,
}

impl Piece {
	/// Whether the piece is whitespace containing a line break.
	fn breaks_line(&self, text: &str) -> bool {
		!self.comment && text[self.range.clone()].contains('\n')
	}

	/// Whether the piece is whitespace containing a blank line (two line breaks).
	fn has_blank_line(&self, text: &str) -> bool {
		!self.comment && text[self.range.clone()].matches('\n').nth(1).is_some()
	}
}

/// Splits the text of `range` (between two arms) into runs of whitespace and comments: `//` comments to the end of
/// their line, and `/* */` comments, possibly nested and spanning lines. `None` if the range is not a valid range of
/// `text` or holds anything else, as when an arm's span is incomplete.
fn trivia(text: &str, range: Range<usize>) -> Option<Vec<Piece>> {
	let gap = text.get(range.clone())?;
	let mut pieces = Vec::new();
	let mut position = 0;

	while position < gap.len() {
		let rest = &gap[position..];
		let (length, comment) = if rest.starts_with("//") {
			(line_comment_len(rest), true)
		} else if rest.starts_with("/*") {
			(block_comment_len(rest)?, true)
		} else {
			let length = rest.len() - rest.trim_start_matches(is_whitespace).len();

			if length == 0 {
				return None;
			}

			(length, false)
		};

		pieces.push(Piece { range: range.start + position..range.start + position + length, comment });
		position += length;
	}

	Some(pieces)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A function whose body is a `match` with these arms (lines indented by two tabs).
	fn in_match(arms: &str) -> String {
		format!("fn f() {{\n\tmatch x {{\n{arms}\t}}\n}}\n")
	}

	/// Separates the arms of `text`, checking that separating the result again changes nothing.
	#[track_caller]
	fn separate(text: &str) -> String {
		let once = separate_match_arms(text, "test").unwrap();
		let twice = separate_match_arms(&once, "test").unwrap();

		assert_eq!(twice, once, "not idempotent:\n{once}");
		once
	}

	/// Asserts that the `arms` of a match are laid out as `expected`.
	#[track_caller]
	fn assert_arms(arms: &str, expected: &str) {
		let separated = separate(&in_match(arms));

		assert_eq!(separated, in_match(expected), "\n--- output:\n{separated}");
	}

	/// Asserts that the `arms` of a match are laid out as they are.
	#[track_caller]
	fn assert_unchanged(arms: &str) {
		assert_arms(arms, arms);
	}

	#[test]
	fn separates_arms_spanning_lines() {
		// first
		assert_arms(
			"\t\tA => {\n\t\t\t1\n\t\t}\n\t\tB => 2,\n\t\tC => 3,\n",
			"\t\tA => {\n\t\t\t1\n\t\t}\n\n\t\tB => 2,\n\t\tC => 3,\n",
		);

		// middle, with a pattern spanning lines and a trailing comma
		assert_arms(
			"\t\tA => 1,\n\t\tB {\n\t\t\tx,\n\t\t} => 2,\n\t\tC => 3,\n",
			"\t\tA => 1,\n\n\t\tB {\n\t\t\tx,\n\t\t} => 2,\n\n\t\tC => 3,\n",
		);

		// last
		assert_arms(
			"\t\tA => 1,\n\t\tB => 2,\n\t\tC => {\n\t\t\t3\n\t\t}\n",
			"\t\tA => 1,\n\t\tB => 2,\n\n\t\tC => {\n\t\t\t3\n\t\t}\n",
		);

		// consecutive: one blank line
		assert_arms(
			"\t\tA => {\n\t\t\t1\n\t\t}\n\t\tB => {\n\t\t\t2\n\t\t}\n\n\n\t\tC => {\n\t\t\t3\n\t\t}\n",
			"\t\tA => {\n\t\t\t1\n\t\t}\n\n\t\tB => {\n\t\t\t2\n\t\t}\n\n\t\tC => {\n\t\t\t3\n\t\t}\n",
		);

		// a guard spanning lines
		assert_arms(
			"\t\tA => 1,\n\t\tB if b\n\t\t\t&& c =>\n\t\t{\n\t\t\t2\n\t\t}\n\t\tC => 3,\n",
			"\t\tA => 1,\n\n\t\tB if b\n\t\t\t&& c =>\n\t\t{\n\t\t\t2\n\t\t}\n\n\t\tC => 3,\n",
		);
		assert_arms(
			"\t\tA => 1,\n\t\tB if b\n\t\t\t&& c => 2,\n\t\tC => 3,\n",
			"\t\tA => 1,\n\n\t\tB if b\n\t\t\t&& c => 2,\n\n\t\tC => 3,\n",
		);
	}

	#[test]
	fn one_liners_follow_each_other() {
		let compact = "\t\tA => 1,\n\t\tB => 2,\n\t\tC => {}\n\t\tD => 4,\n";

		assert_unchanged(compact);

		// existing blank lines are removed
		assert_arms("\t\tA => 1,\n\n\t\tB => 2,\n\n\n\t\tC => {}\n\t\tD => 4,\n", compact);
		assert_arms("\t\tA => 1,\n\t\n\t\tB => 2,\n\t\tC => {}\n\n\t\tD => 4,\n", compact);
	}

	#[test]
	fn never_touches_the_text_around_the_arms() {
		// blank lines after `{` and before `}` stay (rustfmt removes them), as do comments there
		let text = in_match("\n\t\tA => {\n\t\t\t1\n\t\t}\n\t\tB => 2,\n\t\t// trailing\n\n");

		assert_eq!(separate(&text), in_match("\n\t\tA => {\n\t\t\t1\n\t\t}\n\n\t\tB => 2,\n\t\t// trailing\n\n"));

		// nothing is added before `}` after an arm spanning lines
		assert_unchanged("\t\tA => 1,\n\t\tB => 2,\n");
		assert_arms("\t\tA => 1,\n\n\t\tB => {\n\t\t\t2\n\t\t}\n", "\t\tA => 1,\n\n\t\tB => {\n\t\t\t2\n\t\t}\n");
	}

	#[test]
	fn attributes_decorate_arms() {
		assert_arms(
			"\t\tA => 1,\n\t\t#[cfg(x)]\n\t\tB => 2,\n\t\tC => 3,\n",
			"\t\tA => 1,\n\n\t\t#[cfg(x)]\n\t\tB => 2,\n\n\t\tC => 3,\n",
		);
		assert_arms("\t\t#[cfg(x)]\n\t\tA => 1,\n\t\tB => 2,\n", "\t\t#[cfg(x)]\n\t\tA => 1,\n\n\t\tB => 2,\n");

		// attributes on an arm spanning lines: one blank line, never two
		assert_arms(
			"\t\tA => 1,\n\t\t#[cfg(x)]\n\t\tB => {\n\t\t\t2\n\t\t}\n\t\tC => 3,\n",
			"\t\tA => 1,\n\n\t\t#[cfg(x)]\n\t\tB => {\n\t\t\t2\n\t\t}\n\n\t\tC => 3,\n",
		);
	}

	#[test]
	fn comments_above_arms_decorate_them() {
		assert_arms(
			"\t\tA => 1,\n\t\t// about b\n\t\tB => 2,\n\t\tC => 3,\n",
			"\t\tA => 1,\n\n\t\t// about b\n\t\tB => 2,\n\n\t\tC => 3,\n",
		);
		assert_arms(
			"\t\tA => 1,\n\t\t/* about b */\n\t\tB => 2,\n\t\tC => 3,\n",
			"\t\tA => 1,\n\n\t\t/* about b */\n\t\tB => 2,\n\n\t\tC => 3,\n",
		);

		// a comment on the arm's line before it
		assert_arms(
			"\t\tA => 1,\n\t\t/* b */ B => 2,\n\t\tC => 3,\n",
			"\t\tA => 1,\n\n\t\t/* b */ B => 2,\n\n\t\tC => 3,\n",
		);

		// a section header separated from the arm by a blank line stays as it is
		assert_arms(
			"\t\tA => 1,\n\t\t// section\n\n\t\tB => 2,\n\t\tC => 3,\n",
			"\t\tA => 1,\n\n\t\t// section\n\n\t\tB => 2,\n\n\t\tC => 3,\n",
		);
		assert_arms(
			"\t\tA => 1,\n\n\n\t\t// section\n\n\n\t\t// about b\n\t\tB => 2,\n",
			"\t\tA => 1,\n\n\t\t// section\n\n\n\t\t// about b\n\t\tB => 2,\n",
		);
	}

	#[test]
	fn comments_above_the_first_arm() {
		// directly above the arm: decorated
		assert_arms("\t\t// about a\n\t\tA => 1,\n\t\tB => 2,\n", "\t\t// about a\n\t\tA => 1,\n\n\t\tB => 2,\n");
		assert_arms(
			"\t\t// header\n\n\t\t// about a\n\t\tA => 1,\n\t\tB => 2,\n",
			"\t\t// header\n\n\t\t// about a\n\t\tA => 1,\n\n\t\tB => 2,\n",
		);
		assert_arms("\n\t\t// about a\n\t\tA => 1,\n\t\tB => 2,\n", "\n\t\t// about a\n\t\tA => 1,\n\n\t\tB => 2,\n");

		// separated from the arm by a blank line: the header of the match
		assert_unchanged("\t\t// header\n\n\t\tA => 1,\n\t\tB => 2,\n");
		assert_unchanged("\t\t// header\n\t\t// more\n\n\n\t\tA => 1,\n\t\tB => 2,\n");

		// after the inner attributes of the match
		assert_arms(
			"\t\t#![allow(x)]\n\t\t// about a\n\t\tA => 1,\n\t\tB => 2,\n",
			"\t\t#![allow(x)]\n\t\t// about a\n\t\tA => 1,\n\n\t\tB => 2,\n",
		);
		assert_unchanged("\t\t#![allow(x)]\n\t\t// header\n\n\t\tA => 1,\n\t\tB => 2,\n");
		assert_unchanged("\t\t#![allow(x)]\n\t\tA => 1,\n\t\tB => 2,\n");

		// on the line of the `{`: trailing it
		let text = "fn f() {\n\tmatch x { // on the brace line\n\t\tA => 1,\n\t\tB => 2,\n\t}\n}\n";

		assert_eq!(separate(text), text);

		let text =
			"fn f() {\n\tmatch x { /* on the brace line */\n\n\t\t// about a\n\t\tA => 1,\n\t\tB => 2,\n\t}\n}\n";

		assert_eq!(separate(text), text.replace("A => 1,\n", "A => 1,\n\n"));
	}

	#[test]
	fn trailing_comments_stay_on_their_line() {
		assert_unchanged("\t\tA => 1, // one\n\t\tB => 2, /* two */\n\t\tC => 3, /* three */ // and more\n");
		assert_arms(
			"\t\tA => 1, // one\n\t\tB => {\n\t\t\t2\n\t\t} // two\n\t\tC => 3, /* three */\n",
			"\t\tA => 1, // one\n\n\t\tB => {\n\t\t\t2\n\t\t} // two\n\n\t\tC => 3, /* three */\n",
		);

		// a trailing block comment spanning lines is not part of the arm, and the line break after it is left alone
		// either way, as rustfmt removes a blank line there
		assert_unchanged("\t\tA => 1, /* one\n\t\tand more */\n\t\tB => 2,\n");
		assert_unchanged("\t\tA => 1, /* one /* nested\n\t\t*/ and more */ // and a line comment\n\t\tB => 2,\n");
		assert_unchanged("\t\tA => 1, /* one\n\t\tand more */\n\t\tB => {\n\t\t\t2\n\t\t}\n");
		assert_unchanged("\t\tA => 1, /* one\n\t\tand more */\n\n\t\tB => 2,\n");
	}

	#[test]
	fn leaves_skipped_code_alone() {
		let arms = "\t\tA => 1,\n\n\t\tB => 2,\n\t\tC => {\n\t\t\t3\n\t\t}\n";
		let laid_out = "\t\tA => 1,\n\t\tB => 2,\n\n\t\tC => {\n\t\t\t3\n\t\t}\n";
		let skips = [
			"#[rustfmt::skip]",
			"#[rustfmt_skip]",
			"#[cfg_attr(rustfmt, rustfmt::skip)]",
			"#[cfg_attr(rustfmt, rustfmt_skip)]",
		];

		for skip in skips {
			// on the function, the `match` itself, a `let`, an `impl` item, a trait item, and a module
			for text in [
				format!("{skip}\nfn f() {{\n\tmatch x {{\n{arms}\t}}\n}}\n"),
				format!("fn f() {{\n\t{skip}\n\tmatch x {{\n{arms}\t}}\n}}\n"),
				format!("fn f() {{\n\t{skip}\n\tlet v = match x {{\n{arms}\t}};\n}}\n"),
				format!("impl S {{\n\t{skip}\n\tfn f() {{\n\t\tmatch x {{\n{arms}\t\t}}\n\t}}\n}}\n"),
				format!("trait T {{\n\t{skip}\n\tfn f() {{\n\t\tmatch x {{\n{arms}\t\t}}\n\t}}\n}}\n"),
				format!("{skip}\nmod m {{\n\tfn f() {{\n\t\tmatch x {{\n{arms}\t\t}}\n\t}}\n}}\n"),
			] {
				assert_eq!(separate(&text), text, "{text}");
			}
		}

		// the whole file
		let text = format!("#![rustfmt::skip]\n\nfn f() {{\n\tmatch x {{\n{arms}\t}}\n}}\n");

		assert_eq!(separate(&text), text);

		// other code is still laid out
		let text = format!(
			"#[rustfmt::skip]\nfn f() {{\n\tmatch x {{\n{arms}\t}}\n}}\n\nfn g() {{\n\tmatch x {{\n{arms}\t}}\n}}\n"
		);
		let expected = format!(
			"#[rustfmt::skip]\nfn f() {{\n\tmatch x {{\n{arms}\t}}\n}}\n\nfn g() {{\n\tmatch x {{\n{laid_out}\t}}\n}}\n"
		);

		assert_eq!(separate(&text), expected);

		// other attributes do not skip
		for attribute in ["#[allow(x)]", "#[cfg_attr(test, rustfmt::skip)]", "#[rustfmt::skip::macros(m)]"] {
			let text = format!("{attribute}\nfn f() {{\n\tmatch x {{\n{arms}\t}}\n}}\n");
			let expected = format!("{attribute}\nfn f() {{\n\tmatch x {{\n{laid_out}\t}}\n}}\n");

			assert_eq!(separate(&text), expected, "{text}");
		}
	}

	#[test]
	fn matches_with_fewer_than_two_arms_are_left_alone() {
		for arms in ["", "\n", "\t\tA => 1,\n", "\n\t\tA => {\n\t\t\t1\n\t\t}\n\n", "\t\t// about a\n\t\tA => 1,\n"] {
			assert_unchanged(arms);
		}

		let text = "fn f() {\n\tmatch x {}\n}\n";

		assert_eq!(separate(text), text);
	}

	#[test]
	fn lays_out_nested_matches() {
		// in the body of an arm
		assert_arms(
			"\t\tA => match y {\n\t\t\tC => 1,\n\t\t\tD => {\n\t\t\t\t2\n\t\t\t}\n\t\t},\n\t\tB => 3,\n",
			"\t\tA => match y {\n\t\t\tC => 1,\n\n\t\t\tD => {\n\t\t\t\t2\n\t\t\t}\n\t\t},\n\n\t\tB => 3,\n",
		);

		// in the scrutinee
		let text = "fn f() {\n\tmatch match y {\n\t\tC => {\n\t\t\t1\n\t\t}\n\t\tD => 2,\n\t} {\n\
			\t\tA => 1,\n\n\t\tB => 2,\n\t}\n}\n";
		let expected = text.replace("}\n\t\tD => 2,", "}\n\n\t\tD => 2,").replace("A => 1,\n\n", "A => 1,\n");

		assert_eq!(separate(text), expected);
	}

	#[test]
	fn lays_out_matches_everywhere() {
		let arms = ["A => 1,", "B => {", "\t2", "}"];
		let separated = ["A => 1,", "", "B => {", "\t2", "}"];
		let indented = |lines: &[&str], depth: usize| -> String {
			let indent = "\t".repeat(depth);
			let body: String = lines
				.iter()
				.map(|line| if line.is_empty() { "\n".to_owned() } else { format!("{indent}\t{line}\n") })
				.collect();

			format!("match y {{\n{body}{indent}}}")
		};

		// (text before the match, its depth, text after it)
		let places = [
			("const X: u8 = ", 0, ";\n"),
			("static X: u8 = ", 0, ";\n"),
			("fn f() {\n\tlet c = |y| ", 1, ";\n}\n"),
			("fn f() {\n\tlet _ = [", 1, "];\n}\n"),
			("fn f() {\n\tif ", 1, " == 1 {}\n}\n"),
			("trait T {\n\tfn f(y: Y) -> u8 {\n\t\t", 2, "\n\t}\n}\n"),
			("impl S {\n\tfn f(y: Y) -> u8 {\n\t\t", 2, "\n\t}\n}\n"),
			("mod m {\n\tfn f(y: Y) -> u8 {\n\t\t", 2, "\n\t}\n}\n"),
			("mod m {\n\tmod n {\n\t\tconst X: u8 = ", 2, ";\n\t}\n}\n"),
			("fn f() {\n\tfn g(y: Y) -> u8 {\n\t\t", 2, "\n\t}\n}\n"),
			("fn f() -> [u8; ", 0, "] {\n\ttodo!()\n}\n"),
		];

		for (before, depth, after) in places {
			let text = format!("{before}{}{after}", indented(&arms, depth));
			let expected = format!("{before}{}{after}", indented(&separated, depth));

			assert_eq!(separate(&text), expected, "{text}");
		}
	}

	#[test]
	fn leaves_matches_in_macro_invocations_alone() {
		let arms = "\t\tA => 1,\n\t\tB => {\n\t\t\t2\n\t\t}\n";

		for text in [
			format!("fn f() {{\n\tm!(match x {{\n{arms}\t}});\n}}\n"),
			format!("fn f() {{\n\tlet _ = m![match x {{\n{arms}\t}}];\n}}\n"),
			format!("m! {{\n\tmatch x {{\n{arms}\t}}\n}}\n"),
			format!("macro_rules! m {{\n\t() => {{\n\t\tmatch x {{\n{arms}\t\t}}\n\t}};\n}}\n"),
		] {
			assert_eq!(separate(&text), text);
		}
	}

	#[test]
	fn keeps_crlf_line_breaks() {
		let text =
			in_match("\t\tA => 1,\n\n\t\tB => 2,\n\t\tC => {\n\t\t\t3\n\t\t}\n\t\tD => 4,\n").replace('\n', "\r\n");
		let expected =
			in_match("\t\tA => 1,\n\t\tB => 2,\n\n\t\tC => {\n\t\t\t3\n\t\t}\n\n\t\tD => 4,\n").replace('\n', "\r\n");

		assert_eq!(separate(&text), expected);
	}

	#[test]
	fn compensates_for_bom_and_shebang() {
		for prefix in ["\u{feff}", "#!/usr/bin/env run-cargo-script\n", "\u{feff}#!/usr/bin/env run-cargo-script\n"] {
			let text = format!("{prefix}{}", in_match("\t\tA => 1,\n\t\tB => {\n\t\t\t2\n\t\t}\n"));
			let expected = format!("{prefix}{}", in_match("\t\tA => 1,\n\n\t\tB => {\n\t\t\t2\n\t\t}\n"));

			assert_eq!(separate(&text), expected);
		}
	}

	#[test]
	fn reports_text_that_does_not_parse() {
		let error = separate_match_arms("fn f( {}", "rustfmt").unwrap_err();

		assert!(matches!(error, FormatError::StructureMismatch(_)), "{error}");
		assert!(error.to_string().contains("the output of rustfmt does not parse"), "{error}");
	}

	#[test]
	fn separations() {
		assert_eq!(separation("\n\t", true).as_deref(), Some("\n\n\t"));
		assert_eq!(separation("\n\t", false), None);
		assert_eq!(separation("\n\n\t", true), None);
		assert_eq!(separation("\n\n\t", false).as_deref(), Some("\n\t"));
		assert_eq!(separation("\n\n\n\t", true).as_deref(), Some("\n\n\t"));
		assert_eq!(separation(" \r\n\r\n    ", false).as_deref(), Some(" \r\n    "));
		assert_eq!(separation(" \r\n    ", true).as_deref(), Some(" \r\n\r\n    "));

		// whitespace between the first and the last line break goes
		assert_eq!(separation("\n \t \n\t", true).as_deref(), Some("\n\n\t"));
		assert_eq!(separation("\n \t \n\t", false).as_deref(), Some("\n\t"));
		assert_eq!(separation("\n\r\n\t", true).as_deref(), Some("\n\n\t"));

		// no line break
		assert_eq!(separation(" \t ", true), None);
		assert_eq!(separation("", false), None);
	}

	#[test]
	fn splits_trivia() {
		let pieces = |text: &str| -> Option<Vec<(String, bool)>> {
			trivia(text, 0..text.len())
				.map(|pieces| pieces.into_iter().map(|piece| (text[piece.range].to_owned(), piece.comment)).collect())
		};
		let owned = |pieces: &[(&str, bool)]| -> Vec<(String, bool)> {
			pieces.iter().map(|(text, comment)| ((*text).to_owned(), *comment)).collect()
		};

		assert_eq!(pieces(""), Some(Vec::new()));
		assert_eq!(pieces(" \n\t"), Some(owned(&[(" \n\t", false)])));
		assert_eq!(
			pieces(" // c\r\n\t/* b /* nested\n */ */\n"),
			Some(owned(&[
				(" ", false),
				("// c", true),
				("\r\n\t", false),
				("/* b /* nested\n */ */", true),
				("\n", false)
			])),
		);
		assert_eq!(pieces("// c"), Some(owned(&[("// c", true)])));
		assert_eq!(pieces("/* c */x"), None);
		assert_eq!(pieces("/* unterminated"), None);
		assert_eq!(trivia("ab", Range { start: 1, end: 0 }), None);
		assert_eq!(trivia("ab", 1..3), None);
		assert_eq!(trivia("é", 0..1), None);

		let text = "a /* b */ \n c";
		let pieces = trivia(text, 1..text.len() - 1).unwrap();

		assert!(!pieces[0].breaks_line(text));
		assert!(!pieces[1].breaks_line(text));
		assert!(pieces[2].breaks_line(text));
		assert!(!pieces[2].has_blank_line(text));
		assert!(trivia("\n\n", 0..2).unwrap()[0].has_blank_line("\n\n"));
		assert!(!trivia("/*\n\n*/", 0..6).unwrap()[0].has_blank_line("/*\n\n*/"));
	}
}
