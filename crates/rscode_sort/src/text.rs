//! Text-based sorting: items are moved as whole chunks of source text, preserving comments and formatting.
//!
//! A container's items are cut into chunks: an item, the comments attached above it, and the comments trailing it
//! on its last line. Chunks are reordered according to [`cryotheum`] and joined with normalized separators, while the
//! text before the first chunk (the container's header) and after the last chunk stays in place.

use crate::SortError;
use crate::SortOptions;
use crate::SortTarget;
use crate::cryotheum;
use crate::cryotheum::Plan;
use crate::cryotheum::PlanEntry;
use crate::cryotheum::Spacing;
use crate::tokens;
use crate::tokens::token_texts;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use quote::ToTokens;
use std::collections::BTreeSet;
use std::ops::Range;
use syn::spanned::Spanned;

#[derive(Clone, Copy)]
enum Children<'a> {
	Items(&'a [syn::Item]),
	Impl(&'a [syn::ImplItem]),
	Trait(&'a [syn::TraitItem]),
	Foreign(&'a [syn::ForeignItem]),
}

/// An item with its attached comments.
#[derive(Debug, Clone)]
struct Chunk<'a> {
	/// Where the chunk's text starts: at its first attached comment, or at the item.
	start: usize,

	/// Where the chunk would start as the first chunk of its container, whose comments separated from it by a blank
	/// line belong to the container's header instead.
	start_as_first: usize,

	/// The end of the item and of the comments trailing it on its last line.
	end: usize,

	/// The item, including its outer attributes and doc comments.
	item: Range<usize>,

	/// The whitespace before `start` on its line, when nothing else precedes the chunk on that line.
	indent: Option<&'a str>,

	/// Whether the chunk ends with a `//` comment, which must be followed by a line break.
	line_comment: bool,
}

impl Chunk<'_> {
	/// Whether the item has attributes or doc comments, or comments above it when the chunk starts at `start`.
	fn decorated(&self, source: &str, start: usize) -> bool {
		// items start with a keyword or identifier, unless an attribute or a doc comment precedes it
		start < self.item.start || source[self.item.start..].starts_with(['#', '/'])
	}
}

/// A container whose items can be sorted.
#[derive(Clone, Copy)]
enum Container<'a> {
	File(&'a syn::File),
	Module(&'a syn::ItemMod, &'a syn::token::Brace, &'a [syn::Item]),
	Impl(&'a syn::ItemImpl),
	Trait(&'a syn::ItemTrait),
	Foreign(&'a syn::ItemForeignMod),
}

impl<'a> Container<'a> {
	fn of_item(item: &'a syn::Item) -> Option<Self> {
		match item {
			syn::Item::Mod(module) => module.content.as_ref().map(|(brace, items)| Self::Module(module, brace, items)),
			syn::Item::Impl(block) => Some(Self::Impl(block)),
			syn::Item::Trait(block) => Some(Self::Trait(block)),
			syn::Item::ForeignMod(block) => Some(Self::Foreign(block)),
			_ => None,
		}
	}

	/// The attributes of the container item, including inner attributes.
	fn attrs(self) -> &'a [syn::Attribute] {
		match self {
			Self::File(file) => &file.attrs,
			Self::Module(module, ..) => &module.attrs,
			Self::Impl(block) => &block.attrs,
			Self::Trait(block) => &block.attrs,
			Self::Foreign(block) => &block.attrs,
		}
	}

	fn brace(self) -> Option<&'a syn::token::Brace> {
		match self {
			Self::File(_) => None,
			Self::Module(_, brace, _) => Some(brace),
			Self::Impl(block) => Some(&block.brace_token),
			Self::Trait(block) => Some(&block.brace_token),
			Self::Foreign(block) => Some(&block.brace_token),
		}
	}

	fn children(self) -> Children<'a> {
		match self {
			Self::File(file) => Children::Items(&file.items),
			Self::Module(_, _, items) => Children::Items(items),
			Self::Impl(block) => Children::Impl(&block.items),
			Self::Trait(block) => Children::Trait(&block.items),
			Self::Foreign(block) => Children::Foreign(&block.items),
		}
	}

	/// Whether the options allow sorting this kind of container.
	fn enabled(self, options: &SortOptions) -> bool {
		match self {
			Self::File(_) => true,
			Self::Module(..) => options.inline_modules,
			Self::Impl(_) => options.impl_items,
			Self::Trait(_) => options.trait_items,
			Self::Foreign(_) => options.foreign_items,
		}
	}
}

/// What a container body looks like around its chunks.
struct Frame<'s, 'a> {
	body: Range<usize>,

	/// The container's own chunks in source order, which determine its header and trailer.
	chunks: &'s [Chunk<'a>],

	/// The indentation of chunks that did not start a line.
	item_indent: &'s str,

	/// The indentation of the container item itself.
	container_indent: &'s str,

	/// The file root has no closing `}`.
	is_file: bool,
}

/// A chunk of text in its new position.
#[derive(Debug)]
struct Placed<'a> {
	text: String,

	/// The chunk's original indentation, if it started a line.
	indent: Option<&'a str>,

	/// Comments of a merged-away `extern` block, placed above the chunk.
	carried: Vec<&'a str>,

	/// The index of the group in the plan.
	group: usize,

	spacing: Spacing,

	/// Whether the chunk has attributes, doc comments, or comments above its item.
	decorated: bool,

	/// Like `decorated`, for the chunk placed first: comments separated from its item by a blank line then belong to
	/// the container's header, so sorting again must lay it out the same way.
	decorated_as_first: bool,

	/// Whether the chunk's own text spans several lines: the item with its attributes and doc comments, the comments
	/// trailing it on its last line, and the sorted text of a nested container, but not the comments above it (a
	/// section header above the chunk placed first becomes the container's header, so counting it would make sorting
	/// again lay the chunk out differently).
	multi_line: bool,

	/// Where the chunk was in the source.
	span: Range<usize>,

	line_comment: bool,
}

struct TextSorter<'a> {
	source: &'a str,

	/// Added to span byte ranges: `syn::parse_file` strips a byte order mark and a shebang line before parsing.
	offset: usize,

	/// The start of every line.
	line_starts: Vec<usize>,

	options: &'a SortOptions,

	/// Whether the file root is a target.
	file_target: bool,

	/// Start offsets of targeted items.
	item_targets: BTreeSet<usize>,

	/// `\n` or `\r\n`, for inserted line breaks.
	newline: &'static str,

	/// Indentation added to items that are moved out of a single-line container.
	indent_unit: &'static str,
}

impl<'a> TextSorter<'a> {
	fn new(source: &'a str, file: &syn::File, targets: &[SortTarget], options: &'a SortOptions) -> Self {
		let bom = if source.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
		let newline = match source.find('\n') {
			Some(index) if source[..index].ends_with('\r') => "\r\n",
			_ => "\n",
		};
		let indent_unit = if source.lines().any(|line| line.starts_with('\t')) {
			"\t"
		} else {
			"    "
		};

		Self {
			source,
			offset: bom + file.shebang.as_ref().map_or(0, String::len),
			line_starts: std::iter::once(0).chain(source.match_indices('\n').map(|(index, _)| index + 1)).collect(),
			options,
			file_target: targets.contains(&SortTarget::File),
			item_targets: targets
				.iter()
				.filter_map(|target| match target {
					SortTarget::File => None,
					SortTarget::Item(start) => Some(*start),
				})
				.collect(),
			newline,
			indent_unit,
		}
	}

	/// Where the chunk of an item starts (at the first comment attached above it), and where it would start as the
	/// first item of its container.
	///
	/// Comments on the line where the gap starts trail the previous item (or the `{`) and stay put. Every other comment
	/// is attached, except that comments separated from the first item of a container by a blank line stay in place
	/// as the container's header.
	fn attached_start(&self, gap: Range<usize>, trivia: &[Trivia]) -> (usize, usize) {
		// the region of whole lines above the item
		let mut region_start = self.line_indent(gap.start).map(|_| gap.start);
		let mut after_blank_line = None;
		let mut line_has_comment = false;

		for piece in trivia {
			if piece.comment {
				line_has_comment |= region_start.is_some();
				continue;
			}

			for (offset, _) in self.source[piece.range.clone()].match_indices('\n') {
				let next_line = piece.range.start + offset + 1;

				match region_start {
					None => region_start = Some(next_line),
					Some(_) if !line_has_comment => after_blank_line = Some(next_line),
					Some(_) => {}
				}

				line_has_comment = false;
			}
		}

		let Some(region_start) = region_start else {
			return (gap.end, gap.end);
		};
		let first_comment_from = |position: usize| {
			trivia
				.iter()
				.find(|piece| piece.comment && piece.range.start >= position)
				.map_or(gap.end, |piece| piece.range.start)
		};

		(
			first_comment_from(region_start),
			first_comment_from(after_blank_line.unwrap_or(region_start)),
		)
	}

	/// Whether a blank line separates two placed chunks (or just a line break).
	///
	/// In a compact group, one-line items follow each other directly, and an item spanning several lines gets a blank
	/// line on both sides, like an item with attributes, doc comments, or comments above it. rustfmt may wrap a
	/// one-liner or join a multi-line item, so sorting again after rustfmt changes blank lines (and nothing else).
	fn blank_line_between(&self, previous: &Placed<'_>, next: &Placed<'_>, previous_is_first: bool) -> bool {
		let decorated = next.decorated
			|| if previous_is_first {
				previous.decorated_as_first
			} else {
				previous.decorated
			};

		if previous.spacing == Spacing::Barrier && next.spacing == Spacing::Barrier {
			// consecutive barriers were next to each other in the source (unless they come from merged blocks), and
			// keep whether a blank line separated them, unless one has attributes, doc comments, or comments
			return decorated
				|| match self.source.get(previous.span.end..next.span.start) {
					Some(gap) if gap.chars().all(is_whitespace) => gap.matches('\n').count() > 1,
					_ => true,
				};
		}

		if previous.group != next.group {
			return true;
		}

		match next.spacing {
			Spacing::Compact => decorated || previous.multi_line || next.multi_line,
			Spacing::Loose | Spacing::Barrier => true,
		}
	}

	/// The part of a container that holds its items: after its `{` and inner attributes, before its `}`.
	fn body(&self, container: Container<'_>) -> Range<usize> {
		let Some(brace) = container.brace() else {
			if let Container::File(file) = container {
				return self.file_body(file);
			}

			return 0..0;
		};
		let open = self.range(brace.span.open()).end;
		let close = self.range(brace.span.close()).start;
		let start = container
			.attrs()
			.iter()
			.filter(|attr| matches!(attr.style, syn::AttrStyle::Inner(_)))
			.map(|attr| self.range_of(attr).end)
			.fold(open, usize::max);

		start..close.max(start)
	}

	/// Returns [`SortError::NoContainer`] for an item target that does not start a container.
	fn check_targets(&self, file: &syn::File) -> Result<(), SortError> {
		let mut containers = BTreeSet::new();

		self.collect_containers(&file.items, &mut containers);

		match self.item_targets.difference(&containers).next() {
			Some(&start) => Err(SortError::NoContainer(start)),
			None => Ok(()),
		}
	}

	fn child_ranges(&self, children: Children<'_>) -> Vec<Range<usize>> {
		match children {
			Children::Items(items) => items.iter().map(|item| self.range_of(item)).collect(),
			Children::Impl(items) => items.iter().map(|item| self.range_of(item)).collect(),
			Children::Trait(items) => items.iter().map(|item| self.range_of(item)).collect(),
			Children::Foreign(items) => items.iter().map(|item| self.range_of(item)).collect(),
		}
	}

	/// Cuts a container body into chunks, one per item.
	fn chunks(&self, body: Range<usize>, items: &[Range<usize>]) -> Result<Vec<Chunk<'a>>, SortError> {
		let mut chunks: Vec<Chunk<'a>> = Vec::with_capacity(items.len());
		let mut previous_end = body.start;

		for (index, item) in items.iter().enumerate() {
			let limit = items.get(index + 1).map_or(body.end, |next| next.start);

			if item.start < previous_end || item.end > limit || !self.source.is_char_boundary(item.start) {
				return Err(self.internal_error(item.start, "overlapping or out-of-order item spans"));
			}

			let gap = previous_end..item.start;
			let trivia = self.trivia(gap.clone())?;
			let (start, start_as_first) = self.attached_start(gap, &trivia);
			let start = if index == 0 { start_as_first } else { start };

			// every comment between items belongs to a chunk, or to the container's header (before the first item)
			if index > 0 && trivia.iter().any(|piece| piece.comment && piece.range.start < start) {
				return Err(self.internal_error(previous_end, "a comment between items belongs to no item"));
			}

			let (end, line_comment) = self.trailing_end(item.end, limit)?;

			chunks.push(Chunk {
				start,
				start_as_first,
				end,
				item: item.clone(),
				indent: self.line_indent(start),
				line_comment,
			});
			previous_end = end;
		}

		self.trivia(previous_end..body.end)?;

		Ok(chunks)
	}

	fn collect_containers(&self, items: &[syn::Item], containers: &mut BTreeSet<usize>) {
		for item in items {
			if let Some(container) = Container::of_item(item) {
				containers.insert(self.range_of(item).start);

				if let Container::Module(_, _, items) = container {
					self.collect_containers(items, containers);
				}
			}
		}
	}

	/// The comments within a range of trivia.
	fn comments(&self, range: Range<usize>) -> Result<Vec<&'a str>, SortError> {
		Ok(self
			.trivia(range)?
			.into_iter()
			.filter(|piece| piece.comment)
			.map(|piece| &self.source[piece.range])
			.collect())
	}

	/// The part of the file that holds items: after the shebang, the inner attributes, and `//!` docs.
	fn file_body(&self, file: &syn::File) -> Range<usize> {
		let start = file.attrs.iter().map(|attr| self.range_of(attr).end).max().unwrap_or(0).max(self.offset);

		start..self.source.len()
	}

	fn has_target_within(&self, range: &Range<usize>) -> bool {
		self.item_targets.range(range.clone()).next().is_some()
	}

	/// The comments between the tokens of an `extern` block's header (its outer attributes, `unsafe extern "abi"`,
	/// and `{`), or `None` if they cannot be told apart from the tokens (such a block never merges).
	fn header_comments(&self, block: &syn::ItemForeignMod) -> Option<Vec<&'a str>> {
		let mut spans = Vec::new();

		for attr in &block.attrs {
			token_spans(attr.to_token_stream(), &mut spans);
		}

		spans.extend(block.unsafety.map(|unsafety| unsafety.span));
		spans.push(block.abi.extern_token.span);
		spans.extend(block.abi.name.as_ref().map(syn::LitStr::span));

		let start = self.range_of(block).start;
		let open = self.range(block.brace_token.span.open());
		let mut ranges: Vec<Range<usize>> = spans.into_iter().map(|span| self.range(span)).collect();
		let mut position = start;
		let mut comments = Vec::new();

		ranges.push(open.clone());
		ranges.sort_by_key(|range| range.start);

		for range in ranges {
			if range.start < start || range.end > open.end || !self.source.is_char_boundary(range.start) {
				return None;
			}

			if range.start > position {
				comments.extend(self.comments(position..range.start).ok()?);
			}

			position = position.max(range.end);
		}

		(position == open.end).then_some(comments)
	}

	fn internal_error(&self, offset: usize, message: &str) -> SortError {
		let mut offset = offset.min(self.source.len());

		while !self.source.is_char_boundary(offset) {
			offset -= 1;
		}

		let line_start = self.line_start(offset);

		SortError::Internal {
			message: message.to_owned(),
			line: self.line_starts.partition_point(|&start| start <= offset),
			column: self.source[line_start..offset].chars().count() + 1,
		}
	}

	/// The indentation of a container's items: that of the first item starting a line, or, when no item starts a
	/// line, one level deeper than the container.
	fn item_indent(&self, chunks: &[Chunk<'a>], container: Container<'_>, container_indent: &str) -> String {
		match chunks.iter().find_map(|chunk| chunk.indent) {
			Some(indent) => indent.to_owned(),
			None if matches!(container, Container::File(_)) => String::new(),
			None => format!("{container_indent}{}", self.indent_unit),
		}
	}

	/// Joins placed chunks into a new body: the original header, the chunks with normalized separators, each on its
	/// own line, extra trailing comments, and the original trailer.
	fn layout(&self, frame: &Frame<'_, '_>, placed: Vec<Placed<'_>>, extra: &[&str]) -> String {
		let body = frame.body.clone();
		let body_text = &self.source[body.clone()];

		if placed.is_empty() && extra.is_empty() {
			return body_text.to_owned();
		}

		let (header, trailer) = match (frame.chunks.first(), frame.chunks.last()) {
			(Some(first), Some(last)) => {
				let header_end = first.start - first.indent.map_or(0, str::len);

				(&self.source[body.start..header_end.max(body.start)], &self.source[last.end..body.end])
			}

			_ => ("", body_text),
		};
		let single_line = !body_text.contains('\n');
		let mut output = String::with_capacity(body_text.len() + 16);

		// the first item starts a line
		if frame.chunks.first().is_some_and(|first| first.indent.is_some()) {
			output.push_str(header);
		} else {
			output.push_str(header.trim_end_matches([' ', '\t']));
			output.push_str(self.newline);
		}

		for (index, chunk) in placed.iter().enumerate() {
			if index > 0 {
				output.push_str(self.newline);

				if self.blank_line_between(&placed[index - 1], chunk, index == 1) {
					output.push_str(self.newline);
				}
			}

			let indent = chunk.indent.unwrap_or(frame.item_indent);

			for comment in &chunk.carried {
				output.push_str(indent);
				output.push_str(comment);
				output.push_str(self.newline);
			}

			output.push_str(indent);
			output.push_str(&chunk.text);
		}

		for comment in extra {
			output.push_str(self.newline);
			output.push_str(frame.item_indent);
			output.push_str(comment);
		}

		let ends_with_line_comment = match extra.last() {
			Some(comment) => comment.starts_with("//"),
			None => placed.last().is_some_and(|chunk| chunk.line_comment),
		};

		// the closing `}` goes on its own line when the body was single-line, and never after a line comment
		if !frame.is_file && !trailer.contains('\n') && (single_line || ends_with_line_comment) {
			// a single-line trailer can only hold block comments (of an empty container)
			let comments = trailer.trim();

			if !comments.is_empty() {
				output.push_str(self.newline);
				output.push_str(frame.item_indent);
				output.push_str(comments);
			}

			output.push_str(self.newline);
			output.push_str(frame.container_indent);
		} else {
			output.push_str(trailer);
		}

		output
	}

	/// The whitespace between the start of the line and `position`, if there is nothing else (other than a byte order
	/// mark at the start of the file).
	fn line_indent(&self, position: usize) -> Option<&'a str> {
		indent_before(self.source, self.line_start(position), position)
	}

	/// The start of the line holding `position`.
	fn line_start(&self, position: usize) -> usize {
		let line = self.line_starts.partition_point(|&start| start <= position);

		self.line_starts[line.saturating_sub(1)]
	}

	/// The text of an `extern` block chunk with the items of the blocks merging into it.
	///
	/// Comments attached to or inside a merged-away block (including comments in its header) move above its first
	/// item (or to the end of the merged block when it has no items).
	fn merge_extern_blocks(
		&self,
		items: &[syn::Item],
		chunks: &[Chunk<'a>],
		entry: &PlanEntry,
		indent: &str,
		sort_items: bool,
	) -> Result<String, SortError> {
		let mut foreign_items: Vec<&syn::ForeignItem> = Vec::new();
		let mut foreign_chunks: Vec<Chunk<'a>> = Vec::new();
		let mut carried: Vec<Vec<&'a str>> = Vec::new();
		let mut extra: Vec<&'a str> = Vec::new();
		let mut survivor = None;

		for (position, &index) in std::iter::once(&entry.index).chain(&entry.merged).enumerate() {
			let syn::Item::ForeignMod(block) = &items[index] else {
				return Err(self.internal_error(chunks[index].item.start, "merged item is not an extern block"));
			};
			let outer = &chunks[index];
			let body = self.body(Container::Foreign(block));
			let block_chunks = self.chunks(body.clone(), &self.child_ranges(Children::Foreign(&block.items)))?;

			if position == 0 {
				carried.extend(block_chunks.iter().map(|_| Vec::new()));
				survivor = Some((block, body, block_chunks.clone()));
			} else {
				// everything that is not an item: attached comments, the header, the trailer, and trailing comments
				let Some(header) = self.header_comments(block) else {
					return Err(self.internal_error(outer.item.start, "merged extern block with unexpected header"));
				};
				let mut comments = self.comments(outer.start..outer.item.start)?;
				let inner_start = block_chunks.first().map_or(body.end, |first| first.start);
				let inner_end = block_chunks.last().map_or(body.end, |last| last.end);

				comments.extend(header);
				comments.extend(self.comments(body.start..inner_start)?);
				comments.extend(self.comments(inner_end..body.end)?);
				comments.extend(self.comments(outer.item.end..outer.end)?);

				if block_chunks.is_empty() {
					extra.extend(comments);
				} else {
					carried.push(comments);
					carried.extend(block_chunks.iter().skip(1).map(|_| Vec::new()));
				}
			}

			foreign_items.extend(&block.items);
			foreign_chunks.extend(block_chunks);
		}

		let Some((block, body, survivor_chunks)) = survivor else {
			return Err(self.internal_error(0, "empty extern block merge"));
		};
		let plan = if sort_items {
			cryotheum::plan_foreign_items(&foreign_items, &token_texts(&foreign_items))
		} else {
			Plan {
				groups: vec![cryotheum::PlanGroup {
					spacing: Spacing::Compact,
					entries: (0..foreign_items.len()).map(PlanEntry::new).collect(),
				}],
			}
		};
		let mut placed = Vec::with_capacity(foreign_chunks.len());

		for (group_index, group) in plan.groups.iter().enumerate() {
			for entry in &group.entries {
				let chunk = &foreign_chunks[entry.index];
				let mut chunk_placed = self.place(chunk, self.source[chunk.start..chunk.end].to_owned(), group_index, group.spacing);

				chunk_placed.carried = std::mem::take(&mut carried[entry.index]);

				if !chunk_placed.carried.is_empty() {
					chunk_placed.decorated = true;
					chunk_placed.decorated_as_first = true;
				}

				placed.push(chunk_placed);
			}
		}

		let item_indent = match foreign_chunks.iter().find_map(|chunk| chunk.indent) {
			Some(indent) => indent.to_owned(),
			None => format!("{indent}{}", self.indent_unit),
		};
		let frame = Frame {
			body: body.clone(),
			chunks: &survivor_chunks,
			item_indent: &item_indent,
			container_indent: indent,
			is_file: false,
		};
		let body_text = self.layout(&frame, placed, &extra);
		let outer = &chunks[entry.index];

		debug_assert_eq!(self.range_of(block).start, outer.item.start);

		Ok(format!(
			"{}{body_text}{}",
			&self.source[outer.start..body.start],
			&self.source[body.end..outer.end]
		))
	}

	/// A chunk in its new position, with `text` as its new text (which starts at `chunk.start`).
	fn place(&self, chunk: &Chunk<'a>, text: String, group: usize, spacing: Spacing) -> Placed<'a> {
		// the comments above the item are never touched by nested replacements, so the item starts at the same offset
		let multi_line = text[chunk.item.start - chunk.start..].contains('\n');

		Placed {
			text,
			indent: chunk.indent,
			carried: Vec::new(),
			group,
			spacing,
			decorated: chunk.decorated(self.source, chunk.start),
			decorated_as_first: chunk.decorated(self.source, chunk.start_as_first),
			multi_line,
			span: chunk.start..chunk.end,
			line_comment: chunk.line_comment,
		}
	}

	/// The new order of a container's children.
	fn plan(&self, children: Children<'_>) -> Plan {
		match children {
			Children::Items(items) => {
				let ties: Vec<String> = items.iter().map(tokens::tie_text).collect();
				// a block whose header comments cannot be moved does not merge
				let mergeable = |index: usize| {
					self.options.merge_extern_blocks && matches!(&items[index], syn::Item::ForeignMod(block) if self.header_comments(block).is_some())
				};

				cryotheum::plan_items(&items.iter().collect::<Vec<_>>(), &ties, &mergeable, self.options.style_edition)
			}

			Children::Impl(items) => cryotheum::plan_impl_items(&items.iter().collect::<Vec<_>>(), &token_texts(items)),
			Children::Trait(items) => cryotheum::plan_trait_items(&items.iter().collect::<Vec<_>>(), &token_texts(items)),
			Children::Foreign(items) => cryotheum::plan_foreign_items(&items.iter().collect::<Vec<_>>(), &token_texts(items)),
		}
	}

	/// The byte range of a span in the original source.
	fn range(&self, span: proc_macro2::Span) -> Range<usize> {
		let range = span.byte_range();

		range.start + self.offset..range.end + self.offset
	}

	fn range_of(&self, node: &impl Spanned) -> Range<usize> {
		self.range(node.span())
	}

	/// The new text of a container's body.
	///
	/// `indent` is the indentation of the container item itself, and `inherited` is set when an ancestor is being
	/// sorted recursively.
	fn sort_body(&self, container: Container<'_>, indent: &str, inherited: bool) -> Result<String, SortError> {
		let body = self.body(container);
		let targeted = match container {
			Container::File(_) => self.file_target,
			_ => self.item_targets.contains(&body_owner_start(self, container)),
		};
		let sort_this = (targeted || inherited) && container.enabled(self.options);
		let nested_inherited = self.options.recursive && (targeted || inherited);
		let children = container.children();
		let ranges = self.child_ranges(children);
		let chunks = self.chunks(body.clone(), &ranges)?;
		let item_indent = self.item_indent(&chunks, container, indent);
		let mut replacements: Vec<Option<(Range<usize>, String)>> = vec![None; chunks.len()];

		if let Children::Items(items) = children {
			for (index, item) in items.iter().enumerate() {
				let Some(nested) = Container::of_item(item) else {
					continue;
				};

				if !nested_inherited && !self.has_target_within(&ranges[index]) {
					continue;
				}

				let nested_indent = chunks[index].indent.unwrap_or(item_indent.as_str());
				let text = self.sort_body(nested, nested_indent, nested_inherited)?;

				replacements[index] = Some((self.body(nested), text));
			}
		}

		// a single item stays as it is
		if !sort_this || chunks.len() < 2 {
			let replacements: Vec<_> = replacements
				.iter()
				.flatten()
				.map(|(range, text)| (range.clone(), text.as_str()))
				.collect();

			return Ok(splice(self.source, body, &replacements));
		}

		let plan = self.plan(children);
		let mut placed = Vec::with_capacity(chunks.len());

		for (group_index, group) in plan.groups.iter().enumerate() {
			for entry in &group.entries {
				let chunk = &chunks[entry.index];
				let text = match children {
					Children::Items(items) if !entry.merged.is_empty() => {
						let indent = chunk.indent.unwrap_or(item_indent.as_str());
						// the merged items are sorted when any of the merged blocks is sorted
						let targeted = std::iter::once(&entry.index)
							.chain(&entry.merged)
							.any(|&index| self.item_targets.contains(&chunks[index].item.start));
						let sort_items = self.options.foreign_items && (nested_inherited || targeted);

						self.merge_extern_blocks(items, &chunks, entry, indent, sort_items)?
					}

					_ => {
						let replacement = replacements[entry.index].as_ref().map(|(range, text)| (range.clone(), text.as_str()));

						splice(self.source, chunk.start..chunk.end, replacement.as_slice())
					}
				};

				placed.push(self.place(chunk, text, group_index, group.spacing));
			}
		}

		let frame = Frame {
			body,
			chunks: &chunks,
			item_indent: &item_indent,
			container_indent: indent,
			is_file: matches!(container, Container::File(_)),
		};

		Ok(self.layout(&frame, placed, &[]))
	}

	/// The end of an item including the comments that follow it on the same line.
	fn trailing_end(&self, item_end: usize, limit: usize) -> Result<(usize, bool), SortError> {
		let mut end = item_end;

		loop {
			let rest = &self.source[end..limit];
			let start = end + (rest.len() - rest.trim_start_matches(is_horizontal_whitespace).len());
			let rest = &self.source[start..limit];

			if rest.starts_with("//") {
				return Ok((start + line_comment_len(rest), true));
			}

			if !rest.starts_with("/*") {
				return Ok((end, false));
			}

			let length = block_comment_len(rest).ok_or_else(|| self.internal_error(start, "unterminated block comment"))?;

			end = start + length;
		}
	}

	/// Splits the text between items into whitespace and comments.
	fn trivia(&self, range: Range<usize>) -> Result<Vec<Trivia>, SortError> {
		let mut pieces = Vec::new();
		let mut position = range.start;

		while position < range.end {
			let rest = &self.source[position..range.end];
			let (length, comment) = if rest.starts_with("//") {
				(line_comment_len(rest), true)
			} else if rest.starts_with("/*") {
				let length = block_comment_len(rest).ok_or_else(|| self.internal_error(position, "unterminated block comment"))?;

				(length, true)
			} else {
				let length = rest.len() - rest.trim_start_matches(is_whitespace).len();

				if length == 0 {
					return Err(self.internal_error(position, "unexpected text between items"));
				}

				(length, false)
			};

			pieces.push(Trivia {
				range: position..position + length,
				comment,
			});
			position += length;
		}

		Ok(pieces)
	}
}

/// A piece of trivia (the text between items).
#[derive(Debug, Clone)]
struct Trivia {
	range: Range<usize>,
	comment: bool,
}

/// The length of a (possibly nested) `/* */` comment at the start of `text`.
fn block_comment_len(text: &str) -> Option<usize> {
	let bytes = text.as_bytes();
	let mut depth = 0_usize;
	let mut index = 0;

	while index + 1 < bytes.len() {
		match (bytes[index], bytes[index + 1]) {
			(b'/', b'*') => {
				depth += 1;
				index += 2;
			}

			(b'*', b'/') => {
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

/// The start of the item owning a container body, which is how [`SortTarget::Item`] names containers.
fn body_owner_start(sorter: &TextSorter<'_>, container: Container<'_>) -> usize {
	match container {
		Container::File(_) => 0,
		Container::Module(module, ..) => sorter.range_of(module).start,
		Container::Impl(block) => sorter.range_of(block).start,
		Container::Trait(block) => sorter.range_of(block).start,
		Container::Foreign(block) => sorter.range_of(block).start,
	}
}

/// The text between `line_start` and `position` if it is only spaces and tabs (after a byte order mark at the start
/// of the file).
fn indent_before(source: &str, line_start: usize, position: usize) -> Option<&str> {
	let mut prefix = &source[line_start..position];

	if line_start == 0 {
		prefix = prefix.strip_prefix('\u{feff}').unwrap_or(prefix);
	}

	prefix.bytes().all(|byte| byte == b' ' || byte == b'\t').then_some(prefix)
}

/// Whitespace that does not end a line (which only `\n` does, for `//` comments).
fn is_horizontal_whitespace(c: char) -> bool {
	c != '\n' && is_whitespace(c)
}

/// Whitespace as the parser (proc-macro2, like rustc) knows it: Unicode whitespace and the directional marks.
fn is_whitespace(c: char) -> bool {
	c.is_whitespace() || c == '\u{200e}' || c == '\u{200f}'
}

/// The length of a `//` comment at the start of `text`, excluding the line break.
fn line_comment_len(text: &str) -> usize {
	let length = text.find('\n').unwrap_or(text.len());

	if text[..length].ends_with('\r') && length < text.len() {
		length - 1
	} else {
		length
	}
}

pub(crate) fn sort(source: &str, targets: &[SortTarget], options: &SortOptions) -> Result<String, SortError> {
	let file = syn::parse_file(source).map_err(|error| SortError::from_syn_in(&error, source))?;
	let sorter = TextSorter::new(source, &file, targets, options);

	sorter.check_targets(&file)?;

	let output = sorter.sort_body(Container::File(&file), "", false)?;
	let output = format!("{}{output}", &source[..sorter.file_body(&file).start]);

	if output != source
		&& let Err(error) = syn::parse_file(&output)
	{
		let start = error.span().start();

		return Err(SortError::Internal {
			message: format!("sorting produced unparsable output: {error}"),
			line: start.line,
			column: start.column + 1,
		});
	}

	Ok(output)
}

/// Copies a range of `source`, replacing sub-ranges (sorted, non-overlapping).
fn splice(source: &str, range: Range<usize>, replacements: &[(Range<usize>, &str)]) -> String {
	let mut output = String::with_capacity(range.len());
	let mut position = range.start;

	for (replaced, text) in replacements {
		output.push_str(&source[position..replaced.start]);
		output.push_str(text);
		position = replaced.end;
	}

	output.push_str(&source[position..range.end]);
	output
}

/// Collects the spans of every token, and of the delimiters of every group.
fn token_spans(tokens: TokenStream, spans: &mut Vec<proc_macro2::Span>) {
	for token in tokens {
		match token {
			TokenTree::Group(group) => {
				spans.push(group.span_open());
				token_spans(group.stream(), spans);
				spans.push(group.span_close());
			}

			other => spans.push(other.span()),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Asserts the sorted output of a file, that it parses, and that sorting it again changes nothing.
	#[track_caller]
	fn assert_sorts_to(source: &str, expected: &str) {
		assert_sorts_to_with(source, expected, &SortOptions::new());
	}

	#[track_caller]
	fn assert_sorts_to_with(source: &str, expected: &str, options: &SortOptions) {
		let output = sort_file(source, options);

		assert_eq!(output, expected, "\n--- output:\n{output}\n--- expected:\n{expected}");
		assert_valid_and_idempotent(&output, options);
	}

	#[track_caller]
	fn assert_unchanged(source: &str) {
		assert_sorts_to(source, source);
	}

	#[track_caller]
	fn assert_valid_and_idempotent(output: &str, options: &SortOptions) {
		if let Err(error) = syn::parse_file(output) {
			panic!("output does not parse: {error}\n{output}");
		}

		let again = sort_file(output, options);

		assert_eq!(again, output, "not idempotent\n--- first:\n{output}\n--- second:\n{again}");
	}

	#[test]
	fn associated_items() {
		let source = "\
struct S;

impl S {
    fn m(&self) {}

    // helper
    fn _new() -> Self {
        S
    }
    const C: u8 = 0;
    fn new() -> Self {
        S
    }
    type T = u8;
}
";
		let expected = "\
struct S;

impl S {
    type T = u8;

    const C: u8 = 0;

    fn new() -> Self {
        S
    }

    // helper
    fn _new() -> Self {
        S
    }

    fn m(&self) {}
}
";

		assert_sorts_to(source, expected);
	}

	#[test]
	fn block_comments() {
		assert_sorts_to(
			"fn b() {} /* trailing\n   block */\n/* nested /* comment */ about a */\nfn a() {}\n",
			"/* nested /* comment */ about a */\nfn a() {}\n\nfn b() {} /* trailing\n   block */\n",
		);
		assert_sorts_to(
			"fn b() {} /* x */ /* y */ // z\nfn a() {}\n",
			"fn a() {}\n\nfn b() {} /* x */ /* y */ // z\n",
		);
	}

	#[test]
	fn byte_order_mark_and_shebang() {
		assert_sorts_to("\u{feff}fn b() {}\nfn a() {}\n", "\u{feff}fn a() {}\n\nfn b() {}\n");
		assert_sorts_to(
			"\u{feff}#!/usr/bin/env run-cargo-script\n//! docs\nfn b() {}\nfn a() {}\n",
			"\u{feff}#!/usr/bin/env run-cargo-script\n//! docs\nfn a() {}\n\nfn b() {}\n",
		);
		assert_sorts_to(
			"#!/bin/x\n// about b\nfn b() {}\nfn a() {}\n",
			"#!/bin/x\nfn a() {}\n\n// about b\nfn b() {}\n",
		);
		assert_sorts_to(
			"\u{feff}// about b\nfn b() {}\nfn a() {}\n",
			"\u{feff}fn a() {}\n\n// about b\nfn b() {}\n",
		);
	}

	#[test]
	fn cfg_variants_are_ordered_by_token_text() {
		let source = "#[cfg(windows)]\nfn imp() {}\n\n#[cfg(unix)]\nfn imp() {}\n";
		let expected = "#[cfg(unix)]\nfn imp() {}\n\n#[cfg(windows)]\nfn imp() {}\n";

		assert_sorts_to(source, expected);
		assert_sorts_to(expected, expected);
	}

	#[test]
	fn comments_move_with_their_items() {
		let source = "\
// License header
// second line

// about b
fn b() {} // trailing b

/* block about a */
/// Docs of a
fn a() {}

// ===== section =====

fn c() {}
// dangling at the end
";
		let expected = "\
// License header
// second line

/* block about a */
/// Docs of a
fn a() {}

// about b
fn b() {} // trailing b

// ===== section =====

fn c() {}
// dangling at the end
";

		assert_sorts_to(source, expected);
	}

	#[test]
	fn comments_on_the_opening_line_stay() {
		assert_sorts_to(
			"impl X { // about X\n    fn b() {}\n    fn a() {}\n}\n",
			"impl X { // about X\n    fn a() {}\n\n    fn b() {}\n}\n",
		);
		assert_sorts_to(
			"#![allow(dead_code)] // why\nfn b() {}\nfn a() {}\n",
			"#![allow(dead_code)] // why\nfn a() {}\n\nfn b() {}\n",
		);
	}

	#[test]
	fn crlf_line_endings() {
		assert_sorts_to("fn b() {}\r\nfn a() {}\r\n", "fn a() {}\r\n\r\nfn b() {}\r\n");
		assert_sorts_to("// c\r\nfn b() {} // t\r\nfn a() {}\r\n", "fn a() {}\r\n\r\n// c\r\nfn b() {} // t\r\n");
		assert_sorts_to(
			"mod m { fn b() {}\r\n    fn a() {}\r\n}\r\n",
			"mod m {\r\n    fn a() {}\r\n\r\n    fn b() {}\r\n}\r\n",
		);
	}

	#[test]
	fn dangling_comments_stay_at_the_end() {
		assert_sorts_to(
			"mod m {\n    fn b() {}\n    fn a() {}\n    // end of m\n}\n",
			"mod m {\n    fn a() {}\n\n    fn b() {}\n    // end of m\n}\n",
		);
	}

	#[test]
	fn disabled_container_kinds() {
		let source = "impl X {\n\tfn b() {}\n\tfn a() {}\n}\n\nmod m {\n\tfn d() {}\n\tfn c() {}\n\timpl Y {\n\t\tfn f() {}\n\t\tfn e() {}\n\t}\n}\n";

		// the items of an unsorted module are not reordered, but its nested containers are still sorted
		assert_sorts_to_with(
			source,
			"impl X {\n\tfn a() {}\n\n\tfn b() {}\n}\n\nmod m {\n\tfn d() {}\n\tfn c() {}\n\timpl Y {\n\t\tfn e() {}\n\n\t\tfn f() {}\n\t}\n}\n",
			&SortOptions::new().inline_modules(false),
		);
		assert_sorts_to_with(
			source,
			"impl X {\n\tfn b() {}\n\tfn a() {}\n}\n\nmod m {\n\timpl Y {\n\t\tfn f() {}\n\t\tfn e() {}\n\t}\n\n\tfn c() {}\n\n\tfn d() {}\n}\n",
			&SortOptions::new().impl_items(false),
		);
	}

	#[test]
	fn extern_without_abi_merges_with_extern_c() {
		// rustfmt rewrites `extern {}` to `extern "C" {}`, which must not change how the blocks sort or merge
		assert_sorts_to(
			"extern {\n    fn b();\n}\n\nextern \"C\" {\n    fn a();\n}\n",
			"extern \"C\" {\n    fn a();\n    fn b();\n}\n",
		);
		assert_sorts_to(
			"extern {\n    fn a();\n}\n\nextern \"C\" {\n    fn b();\n}\n",
			"extern {\n    fn a();\n    fn b();\n}\n",
		);
		assert_sorts_to_with(
			"extern \"C\" {\n    fn b();\n}\n\nextern {\n    fn a();\n}\n",
			"extern {\n    fn a();\n}\n\nextern \"C\" {\n    fn b();\n}\n",
			&SortOptions::new().merge_extern_blocks(false),
		);
	}

	#[test]
	fn header_comments_stay_in_place() {
		assert_sorts_to(
			"// header comment\n\nfn b() {}\n\nfn a() {}\n",
			"// header comment\n\nfn a() {}\n\nfn b() {}\n",
		);

		// without a blank line, the comment belongs to the first item
		assert_sorts_to("// about b\nfn b() {}\nfn a() {}\n", "fn a() {}\n\n// about b\nfn b() {}\n");
		assert_sorts_to("\n// about b\nfn b() {}\nfn a() {}\n", "\nfn a() {}\n\n// about b\nfn b() {}\n");
	}

	#[test]
	fn helpers() {
		assert_eq!(line_indent("  a\n\tb", 2), Some("  "));
		assert_eq!(line_indent("  a\n\tb", 5), Some("\t"));
		assert_eq!(line_indent("  a\n\tb", 3), None);
		assert_eq!(line_indent("\u{feff}a", 3), Some(""));
		assert_eq!(line_comment_len("// a\r\nb"), 4);
		assert_eq!(line_comment_len("// a\rb"), 6);
		assert_eq!(line_comment_len("// a"), 4);
		assert_eq!(block_comment_len("/* a /* b */ c */ d"), Some(17));
		assert_eq!(block_comment_len("/**/"), Some(4));
		assert_eq!(block_comment_len("/***/"), Some(5));
		assert_eq!(block_comment_len("/* a"), None);
		assert_eq!(splice("abcdef", 1..5, &[(2..3, "X"), (4..4, "Y")]), "bXdYe");
	}

	#[test]
	fn inner_attributes_and_docs_of_inline_modules() {
		assert_sorts_to(
			"mod m {\n    #![allow(dead_code)]\n\n    fn b() {}\n    fn a() {}\n}\n",
			"mod m {\n    #![allow(dead_code)]\n\n    fn a() {}\n\n    fn b() {}\n}\n",
		);
		assert_sorts_to(
			"mod m {\n    //! Module docs.\n    fn b() {}\n    fn a() {}\n}\n",
			"mod m {\n    //! Module docs.\n    fn a() {}\n\n    fn b() {}\n}\n",
		);
	}

	#[test]
	fn item_targets() {
		let source = "fn b() {}\nfn a() {}\nimpl X {\n\tfn d() {}\n\tfn c() {}\n}\nmod m {\n\tfn f() {}\n\timpl Y {\n\t\tfn h() {}\n\t\tfn g() {}\n\t}\n\tfn e() {}\n}\n";
		let options = SortOptions::new();
		let sorter = crate::Sorter::new(options.clone());
		let impl_x = source.find("impl X").unwrap();
		let module = source.find("mod m").unwrap();
		let impl_y = source.find("impl Y").unwrap();

		assert_eq!(
			sorter.sort_str_within(source, &[SortTarget::Item(impl_x)]).unwrap(),
			"fn b() {}\nfn a() {}\nimpl X {\n\tfn c() {}\n\n\tfn d() {}\n}\nmod m {\n\tfn f() {}\n\timpl Y {\n\t\tfn h() {}\n\t\tfn g() {}\n\t}\n\tfn e() {}\n}\n"
		);

		// a nested target in a container that is not sorted itself
		assert_eq!(
			sorter.sort_str_within(source, &[SortTarget::Item(impl_y)]).unwrap(),
			"fn b() {}\nfn a() {}\nimpl X {\n\tfn d() {}\n\tfn c() {}\n}\nmod m {\n\tfn f() {}\n\timpl Y {\n\t\tfn g() {}\n\n\t\tfn h() {}\n\t}\n\tfn e() {}\n}\n"
		);

		// recursive into the targeted module
		assert_eq!(
			sorter.sort_str_within(source, &[SortTarget::Item(module)]).unwrap(),
			"fn b() {}\nfn a() {}\nimpl X {\n\tfn d() {}\n\tfn c() {}\n}\nmod m {\n\timpl Y {\n\t\tfn g() {}\n\n\t\tfn h() {}\n\t}\n\n\tfn e() {}\n\n\tfn f() {}\n}\n"
		);

		// not recursive
		assert_eq!(
			crate::Sorter::new(options.clone().recursive(false))
				.sort_str_within(source, &[SortTarget::Item(module)])
				.unwrap(),
			"fn b() {}\nfn a() {}\nimpl X {\n\tfn d() {}\n\tfn c() {}\n}\nmod m {\n\timpl Y {\n\t\tfn h() {}\n\t\tfn g() {}\n\t}\n\n\tfn e() {}\n\n\tfn f() {}\n}\n"
		);

		// several targets
		assert_eq!(
			sorter
				.sort_str_within(source, &[SortTarget::Item(impl_y), SortTarget::Item(impl_x)])
				.unwrap(),
			"fn b() {}\nfn a() {}\nimpl X {\n\tfn c() {}\n\n\tfn d() {}\n}\nmod m {\n\tfn f() {}\n\timpl Y {\n\t\tfn g() {}\n\n\t\tfn h() {}\n\t}\n\tfn e() {}\n}\n"
		);

		// no targets: nothing changes
		assert_eq!(sorter.sort_str_within(source, &[]).unwrap(), source);
	}

	#[test]
	fn items_sharing_lines_are_split() {
		assert_sorts_to("struct B; struct A;\nfn c() {}\n", "struct A;\n\nstruct B;\n\nfn c() {}\n");
		assert_sorts_to("mod m { fn b() {}\n    fn a() {}\n}\n", "mod m {\n    fn a() {}\n\n    fn b() {}\n}\n");
		assert_sorts_to("mod m { fn b() {} fn a() {}\n}\n", "mod m {\n    fn a() {}\n\n    fn b() {}\n}\n");
		assert_sorts_to("\tmod m { fn b() {} fn a() {}\n\t}\n", "\tmod m {\n\t\tfn a() {}\n\n\t\tfn b() {}\n\t}\n");
		assert_sorts_to("mod m {\n    fn b() {}\n    fn a() {} }\n", "mod m {\n    fn a() {}\n\n    fn b() {} }\n");

		// a closing brace never ends up after a line comment
		assert_sorts_to(
			"mod m {\n    fn b() {} // c\n    fn a() {} }\n",
			"mod m {\n    fn a() {}\n\n    fn b() {} // c\n}\n",
		);
	}

	#[test]
	fn keeps_the_end_of_the_file() {
		assert_sorts_to("fn b() {}\nfn a() {}", "fn a() {}\n\nfn b() {}");
		assert_sorts_to("fn b() {}\nfn a() {}\n\n\n", "fn a() {}\n\nfn b() {}\n\n\n");
	}

	fn line_indent(source: &str, position: usize) -> Option<&str> {
		indent_before(source, source[..position].rfind('\n').map_or(0, |index| index + 1), position)
	}

	#[test]
	fn long_lines() {
		// many items on a single line (like generated code) are sorted in linear time
		let source: String = (0..20_000).rev().map(|index| format!("const C{index}: u8 = 0; ")).collect();
		let output = sort_file(&source, &SortOptions::new());

		assert!(output.starts_with("const C0: u8 = 0;\nconst C1: u8 = 0;\n"));
		assert_eq!(output.lines().count(), 20_000);
	}

	#[test]
	fn macros_are_barriers() {
		// macros at the top of a file stay there, and everything after them is sorted
		assert_sorts_to(
			"macro_rules! m {\n    () => {};\n}\n\nfn uses() { m!(); }\nuse x;\nmod a;\n",
			"macro_rules! m {\n    () => {};\n}\n\nmod a;\n\nuse x;\n\nfn uses() { m!(); }\n",
		);

		// nothing moves across a barrier; consecutive barriers keep their blank lines (or the lack of them)
		assert_sorts_to(
			"fn uses() { m!(); }\nm!();\nmacro_rules! m { () => {} }\n\n#[macro_use]\nmod macros;\nuse x;\nmod a;\n",
			"fn uses() { m!(); }\n\nm!();\nmacro_rules! m { () => {} }\n\n#[macro_use]\nmod macros;\n\nmod a;\n\nuse x;\n",
		);
		assert_sorts_to("a!(); b!();\n\n\nc!();\n", "a!();\nb!();\n\nc!();\n");

		// ... unless one of them has attributes, doc comments, or comments
		assert_sorts_to(
			"#[macro_use]\nextern crate a;\n#[macro_use]\nextern crate b;\nm!();\n#[cfg(x)]\nthread_local!(static A: u8 = 0);\nn!();\n",
			"#[macro_use]\nextern crate a;\n\n#[macro_use]\nextern crate b;\n\nm!();\n\n#[cfg(x)]\nthread_local!(static A: u8 = 0);\n\nn!();\n",
		);

		// tokio's `cfg_x! { macro_rules! .. }` pattern: the macro stays above its uses
		assert_sorts_to(
			"cfg_trace! {\n    macro_rules! trace { () => {} }\n}\n\nfn z() { trace!(); }\n\nfn a() { trace!(); }\n",
			"cfg_trace! {\n    macro_rules! trace { () => {} }\n}\n\nfn a() { trace!(); }\n\nfn z() { trace!(); }\n",
		);

		// os_str_bytes's redefined helper macros: every invocation keeps the definition it had
		let redefined = "\
macro_rules! r#impl {
    ( $a:ident ) => {};
}

r#impl!(B);
r#impl!(A);

macro_rules! r#impl {
    ( $a:ident, $b:ident ) => {};
}

r#impl!(D, E);

const C: u8 = 0;
";

		assert_sorts_to(redefined, redefined);

		// a use of a redefined macro outside of an invocation keeps its value
		assert_sorts_to(
			"macro_rules! value { () => { 1 } }\npub const B: i32 = value!();\npub const A: i32 = 0;\nmacro_rules! value { () => { 2 } }\npub const C: i32 = value!();\n",
			"macro_rules! value { () => { 1 } }\n\npub const A: i32 = 0;\npub const B: i32 = value!();\n\nmacro_rules! value { () => { 2 } }\n\npub const C: i32 = value!();\n",
		);

		// `#[macro_use] extern crate` and `include!` are barriers too
		assert_sorts_to(
			"extern crate b;\n#[macro_use]\nextern crate log;\nextern crate z;\nextern crate a;\ninclude!(\"x.rs\");\nfn b() {}\nfn a() {}\n",
			"extern crate b;\n\n#[macro_use]\nextern crate log;\n\nextern crate a;\nextern crate z;\n\ninclude!(\"x.rs\");\n\nfn a() {}\n\nfn b() {}\n",
		);

		// macro invocations in `impl` blocks, traits, and `extern` blocks are barriers
		assert_sorts_to(
			"impl X {\n    fn b() {}\n    fn a() {}\n    m!();\n    fn d() {}\n    fn c() {}\n}\n",
			"impl X {\n    fn a() {}\n\n    fn b() {}\n\n    m!();\n\n    fn c() {}\n\n    fn d() {}\n}\n",
		);
	}

	#[test]
	fn merged_targets_are_sorted_without_recursion() {
		let source = "extern \"C\" {\n    fn b();\n    fn a();\n}\n\nextern \"C\" {\n    fn c();\n}\n";
		let sorter = crate::Sorter::new(SortOptions::new().recursive(false));
		let targets = [SortTarget::File, SortTarget::Item(0)];
		let output = sorter.sort_str_within(source, &targets).unwrap();

		assert_eq!(output, "extern \"C\" {\n    fn a();\n    fn b();\n    fn c();\n}\n");
		assert_eq!(sorter.sort_str_within(&output, &targets).unwrap(), output);

		// the merged-away block is the target
		let targets = [SortTarget::File, SortTarget::Item(source.rfind("extern").unwrap())];

		assert_eq!(sorter.sort_str_within(source, &targets).unwrap(), output);

		// without targets in the block, its items are merged in order
		let output = sorter.sort_str(source).unwrap();

		assert_eq!(output, "extern \"C\" {\n    fn b();\n    fn a();\n    fn c();\n}\n");
		assert_eq!(sorter.sort_str(&output).unwrap(), output);
	}

	#[test]
	fn merges_bindgen_output() {
		let source = "\
unsafe extern \"C\" {
    pub fn zeta(x: i32) -> i32;
}
unsafe extern \"C\" {
    pub fn alpha();
}
unsafe extern \"C\" {
    pub static mut beta: u8;
}
";
		let expected = "\
unsafe extern \"C\" {
    pub static mut beta: u8;

    pub fn alpha();
    pub fn zeta(x: i32) -> i32;
}
";

		assert_sorts_to(source, expected);
		// unmerged blocks are ordered by their token text
		assert_sorts_to_with(
			source,
			"\
unsafe extern \"C\" {
    pub fn alpha();
}

unsafe extern \"C\" {
    pub fn zeta(x: i32) -> i32;
}

unsafe extern \"C\" {
    pub static mut beta: u8;
}
",
			&SortOptions::new().merge_extern_blocks(false),
		);
	}

	#[test]
	fn merges_block_headers_and_trailers() {
		let source = "\
unsafe extern \"C\" {
    pub fn a();
}

unsafe extern \"C\" {
    // header of the second block

    pub fn b();
    // end of the second block
}
";
		let expected = "\
unsafe extern \"C\" {
    pub fn a();

    // header of the second block
    // end of the second block
    pub fn b();
}
";

		assert_sorts_to(source, expected);
	}

	#[test]
	fn merges_empty_blocks() {
		// `{ }` sorts before `{ fn a (); }`, so the empty block survives
		assert_sorts_to(
			"extern \"C\" {\n    fn a();\n}\n\nextern \"C\" {\n    // nothing here\n}\n",
			"extern \"C\" {\n    fn a();\n    // nothing here\n}\n",
		);
		assert_sorts_to("extern \"C\" {}\nextern \"C\" {}\n", "extern \"C\" {}\n");
		assert_sorts_to(
			"extern \"C\" { fn a(); }\nextern \"C\" { /* c */ }\n",
			"extern \"C\" {\n    fn a();\n    /* c */\n}\n",
		);
		assert_sorts_to(
			"extern \"C\" { fn a(); }\n// about the empty block\nextern \"C\" {} // trailing\n",
			"// about the empty block\nextern \"C\" {\n    fn a();\n} // trailing\n",
		);

		// `# [doc ..]` sorts before `}`, so the empty block merges away and its comments move to the end
		assert_sorts_to(
			"extern \"C\" {\n    /// Doc\n    fn a();\n}\n\n// about the empty block\nextern \"C\" {} // trailing\n",
			"extern \"C\" {\n    /// Doc\n    fn a();\n    // about the empty block\n    // trailing\n}\n",
		);
	}

	#[test]
	fn merges_extern_blocks_with_their_comments() {
		let source = "\
// block one
unsafe extern \"C\" {
    pub fn c();
}

// block two
unsafe extern \"C\" {
    /// Doc of a
    pub fn a();
    pub static B: u8;
}

#[link(name = \"z\")]
unsafe extern \"C\" {
    pub fn z();
}

unsafe extern \"C\" { pub fn b(); } // trailing b
";
		// the second block sorts first (by its items' token text) and survives, keeping its comment above it
		let expected = "\
// block two
unsafe extern \"C\" {
    pub static B: u8;

    /// Doc of a
    pub fn a();

    // trailing b
    pub fn b();

    // block one
    pub fn c();
}

#[link(name = \"z\")]
unsafe extern \"C\" {
    pub fn z();
}
";

		assert_sorts_to(source, expected);
	}

	#[test]
	fn merges_single_line_blocks() {
		assert_sorts_to(
			"extern \"C\" { fn b(); }\nextern \"C\" { fn a(); }\n",
			"extern \"C\" {\n    fn a();\n    fn b();\n}\n",
		);
		assert_sorts_to(
			"mod m {\n\textern \"C\" { fn b(); }\n\textern \"C\" { fn a(); }\n}\n",
			"mod m {\n\textern \"C\" {\n\t\tfn a();\n\t\tfn b();\n\t}\n}\n",
		);
		assert_sorts_to(
			"mod m { extern \"C\" { fn b(); } extern \"C\" { fn a(); } }\n",
			"mod m {\n    extern \"C\" {\n        fn a();\n        fn b();\n    }\n}\n",
		);
	}

	#[test]
	fn merging_keeps_header_comments() {
		// comments between and after attributes, and around `unsafe`, `extern`, and the ABI
		let source = "\
unsafe extern \"C\" {
    pub fn b();
}

#[cfg(x)] // why this cfg
unsafe extern \"C\" {
    pub fn c();
}

#[cfg(x)]
unsafe extern \"C\" /* abi note */ {
    pub fn a();
}
";
		let expected = "\
unsafe extern \"C\" {
    pub fn b();
}

#[cfg(x)]
unsafe extern \"C\" /* abi note */ {
    pub fn a();

    // why this cfg
    pub fn c();
}
";

		assert_sorts_to(source, expected);
		assert_sorts_to(
			"unsafe extern \"C\" { pub fn b(); }\nunsafe /* u */ extern /* e */ \"C\" // after abi\n{ pub fn c(); }\n",
			"unsafe extern \"C\" {\n    pub fn b();\n\n    /* u */\n    /* e */\n    // after abi\n    pub fn c();\n}\n",
		);

		// comments inside attributes
		assert_sorts_to(
			"#[link(name = \"x\" /* why */)]\n#[cfg(all(\n    // unix only\n    unix\n))]\nextern \"C\" {\n    fn b();\n}\n\n#[link(name = \"x\")]\n#[cfg(all(unix))]\nextern \"C\" {\n    fn a();\n}\n",
			"#[link(name = \"x\")]\n#[cfg(all(unix))]\nextern \"C\" {\n    fn a();\n\n    /* why */\n    // unix only\n    fn b();\n}\n",
		);

		// in an empty block's header, whether it survives or not
		assert_sorts_to(
			"extern \"C\" {\n    fn a();\n}\n\nextern /* why */ \"C\" {}\n",
			"extern /* why */ \"C\" {\n    fn a();\n}\n",
		);
		assert_sorts_to(
			"extern \"C\" {\n    /// Doc\n    fn a();\n}\n\nextern /* why */ \"C\" {}\n",
			"extern \"C\" {\n    /// Doc\n    fn a();\n    /* why */\n}\n",
		);

		// doc comments look like comments but are attributes, compared when merging
		assert_sorts_to(
			"/// Docs\nextern \"C\" {\n    fn b();\n}\n\n/// Docs\nextern \"C\" {\n    fn a();\n}\n",
			"/// Docs\nextern \"C\" {\n    fn a();\n    fn b();\n}\n",
		);
	}

	/// The layout a user asked for: a blank line on both sides of a compact item spanning several lines.
	#[test]
	fn multi_line_items_get_blank_lines() {
		let source = "\
pub(crate) static INCOMING: IncomingDebug = IncomingDebug;
static INCOMING_STATE: MainThreadCell<IncomingState> = MainThreadCell::new(IncomingState {
	log: LogMode::Off,
	blocks: Vec::new(),
});
";
		let expected = "\
pub(crate) static INCOMING: IncomingDebug = IncomingDebug;

static INCOMING_STATE: MainThreadCell<IncomingState> = MainThreadCell::new(IncomingState {
	log: LogMode::Off,
	blocks: Vec::new(),
});
";

		assert_sorts_to(source, expected);
		assert_unchanged(expected);

		// first, in the middle, and last, whether the items move or not
		assert_sorts_to(
			"const A: [u8; 2] = [\n\t0,\n\t1,\n];\nconst B: u8 = 0;\nconst C: u8 = 0;\n",
			"const A: [u8; 2] = [\n\t0,\n\t1,\n];\n\nconst B: u8 = 0;\nconst C: u8 = 0;\n",
		);
		assert_sorts_to(
			"static A: u8 = 0;\nstatic B: X = X {\n\tx: 0,\n};\nstatic C: u8 = 0;\n",
			"static A: u8 = 0;\n\nstatic B: X = X {\n\tx: 0,\n};\n\nstatic C: u8 = 0;\n",
		);
		assert_sorts_to(
			"type B = u8;\ntype A = u8;\ntype C = Map<\n\tu8,\n\tu8,\n>;\n",
			"type A = u8;\ntype B = u8;\n\ntype C = Map<\n\tu8,\n\tu8,\n>;\n",
		);
		assert_sorts_to(
			"const B: [u8; 1] = [\n\t0,\n];\nconst A: [u8; 1] = [\n\t0,\n];\n",
			"const A: [u8; 1] = [\n\t0,\n];\n\nconst B: [u8; 1] = [\n\t0,\n];\n",
		);

		// in `impl` blocks, traits, and `extern` blocks
		assert_sorts_to(
			"impl X {\n\tconst B: u8 = 0;\n\tconst A: [u8; 2] = [\n\t\t0,\n\t\t1,\n\t];\n\tconst C: u8 = 0;\n}\n",
			"impl X {\n\tconst A: [u8; 2] = [\n\t\t0,\n\t\t1,\n\t];\n\n\tconst B: u8 = 0;\n\tconst C: u8 = 0;\n}\n",
		);
		assert_sorts_to(
			"trait T {\n\ttype C;\n\ttype B: Iterator<Item = u8>\n\t\t+ Clone;\n\ttype A;\n}\n",
			"trait T {\n\ttype A;\n\n\ttype B: Iterator<Item = u8>\n\t\t+ Clone;\n\n\ttype C;\n}\n",
		);
		assert_sorts_to(
			"extern \"C\" {\n\tfn c();\n\tfn b(\n\t\tx: u8,\n\t);\n\tfn a();\n}\n",
			"extern \"C\" {\n\tfn a();\n\n\tfn b(\n\t\tx: u8,\n\t);\n\n\tfn c();\n}\n",
		);

		// a trailing block comment spanning lines makes a one-liner span several lines; a line comment does not
		assert_sorts_to(
			"const B: u8 = 0; /* about b\n   continued */\nconst A: u8 = 0;\nconst C: u8 = 0;\n",
			"const A: u8 = 0;\n\nconst B: u8 = 0; /* about b\n   continued */\n\nconst C: u8 = 0;\n",
		);
		assert_sorts_to("const B: u8 = 0; // b\nconst A: u8 = 0;\n", "const A: u8 = 0;\nconst B: u8 = 0; // b\n");

		// the comments above an item do not count: a section header becomes the header of the container
		assert_sorts_to(
			"const B: u8 = 0;\n\n// section\n\nconst A: [u8; 2] = [\n\t0,\n];\n",
			"// section\n\nconst A: [u8; 2] = [\n\t0,\n];\n\nconst B: u8 = 0;\n",
		);
		assert_sorts_to(
			"const B: u8 = 0;\n// about a\n// on two lines\nconst A: u8 = 0;\nconst C: u8 = 0;\n",
			"// about a\n// on two lines\nconst A: u8 = 0;\n\nconst B: u8 = 0;\nconst C: u8 = 0;\n",
		);

		// decorated and spanning several lines: one blank line, never two
		assert_sorts_to(
			"const B: u8 = 0;\n/// Doc\nconst A: [u8; 2] = [\n\t0,\n];\nconst C: u8 = 0;\n",
			"/// Doc\nconst A: [u8; 2] = [\n\t0,\n];\n\nconst B: u8 = 0;\nconst C: u8 = 0;\n",
		);
		assert_sorts_to(
			"const A: [u8; 1] = [\n\t0,\n];\n#[cfg(x)]\nconst B: u8 = 0;\n",
			"const A: [u8; 1] = [\n\t0,\n];\n\n#[cfg(x)]\nconst B: u8 = 0;\n",
		);

		// blank lines between one-liners are still removed
		assert_sorts_to(
			"const A: u8 = 0;\n\nconst B: u8 = 0;\n\n\nconst C: u8 = 0;\n",
			"const A: u8 = 0;\nconst B: u8 = 0;\nconst C: u8 = 0;\n",
		);

		// CRLF
		assert_sorts_to(
			"const B: u8 = 0;\r\nconst A: [u8; 2] = [\r\n\t0,\r\n];\r\nconst C: u8 = 0;\r\n",
			"const A: [u8; 2] = [\r\n\t0,\r\n];\r\n\r\nconst B: u8 = 0;\r\nconst C: u8 = 0;\r\n",
		);

		// a merged `extern` block with an item spanning several lines
		assert_sorts_to(
			"extern \"C\" {\n\tfn a();\n}\nextern \"C\" {\n\tfn c(\n\t\tx: u8,\n\t);\n\tfn b();\n}\n",
			"extern \"C\" {\n\tfn a();\n\tfn b();\n\n\tfn c(\n\t\tx: u8,\n\t);\n}\n",
		);
	}

	#[test]
	fn nested_inline_modules() {
		assert_sorts_to(
			"mod outer {\n\tfn b() {}\n\tmod inner {\n\t\tfn d() {}\n\t\tfn c() {}\n\t}\n\tfn a() {}\n}\n",
			"mod outer {\n\tfn a() {}\n\n\tfn b() {}\n\n\tmod inner {\n\t\tfn c() {}\n\n\t\tfn d() {}\n\t}\n}\n",
		);
	}

	#[test]
	fn never_merges_different_blocks() {
		let source = "\
extern \"C\" {
    fn a();
}

unsafe extern \"C\" {
    fn b();
}

extern \"system\" {
    fn c();
}

#[cfg(unix)]
extern \"C\" {
    fn d();
}

extern \"C\" {
    #![allow(dead_code)]
    fn e();
}
";
		// by ABI, attributes, then token text (in which `#` sorts before lowercase letters)
		let expected = "\
extern \"C\" {
    #![allow(dead_code)]
    fn e();
}

extern \"C\" {
    fn a();
}

unsafe extern \"C\" {
    fn b();
}

#[cfg(unix)]
extern \"C\" {
    fn d();
}

extern \"system\" {
    fn c();
}
";

		assert_sorts_to(source, expected);
	}

	#[test]
	fn not_recursive() {
		let options = SortOptions::new().recursive(false);

		assert_sorts_to_with(
			"mod m {\n\tfn b() {}\n\tfn a() {}\n}\nfn z() {}\nuse x;\n",
			"use x;\n\nfn z() {}\n\nmod m {\n\tfn b() {}\n\tfn a() {}\n}\n",
			&options,
		);
	}

	#[test]
	fn parse_errors() {
		let error = sort("fn a() {}\nfn b( {}\n", &[SortTarget::File], &SortOptions::new()).unwrap_err();

		assert!(matches!(error, SortError::Parse { line: 2, .. }), "{error:?}");

		let error = sort("fn a() { let x = ; }", &[SortTarget::File], &SortOptions::new()).unwrap_err();
		let SortError::Parse { line, column, .. } = error else { panic!() };

		assert_eq!((line, column), (1, 18));

		// a byte order mark counts as a column (like `rscode` locations)
		let error = sort("\u{feff}fn a() { let x = ; }", &[SortTarget::File], &SortOptions::new()).unwrap_err();
		let SortError::Parse { line, column, .. } = error else { panic!() };

		assert_eq!((line, column), (1, 19));
	}

	#[test]
	fn section_header_of_the_new_first_item() {
		// the section comment of `a` becomes part of the header once `a` is first, so `a` counts as undecorated
		assert_sorts_to(
			"extern \"C\" {\n    // header\n\n    type b;\n\n    // section\n\n    type a;\n}\n",
			"extern \"C\" {\n    // header\n\n    // section\n\n    type a;\n    type b;\n}\n",
		);
		assert_sorts_to("use b;\n\n// section\n\nuse a;\n", "// section\n\nuse a;\nuse b;\n");
	}

	#[test]
	fn section_headers_move_with_the_next_item() {
		assert_sorts_to(
			"fn c() {}\n\n// ===== section =====\n\nfn b() {}\nfn a() {}\n",
			"fn a() {}\n\n// ===== section =====\n\nfn b() {}\n\nfn c() {}\n",
		);
	}

	/// The layout a user asked for: blank lines around the imports and module declarations with attributes.
	#[test]
	fn separates_attributed_imports() {
		let source = concat!(
			"#![cfg_attr(docsrs, feature(doc_cfg))]\n",
			"\n",
			"mod api;\n",
			"mod context;\n",
			"mod plugin;\n",
			"\n",
			"#[cfg(feature = \"sdk\")]\n",
			"mod commands;\n",
			"#[cfg(feature = \"sdk\")]\n",
			"mod hooks;\n",
			"\n",
			"pub use api::{\n",
			"\tLoaderVersionInfo, MetamodApi, MetamodApiBinding, MetamodFeature, MetamodVersion,\n",
			"\tSourceHookVersions, UnsupportedFeature,\n",
			"};\n",
			"#[cfg(feature = \"sdk\")]\n",
			"#[cfg_attr(docsrs, doc(cfg(feature = \"sdk\")))]\n",
			"pub use commands::MetamodRegistrar;\n",
			"pub use context::{CachedContext, ContextKey, cached_context_key};\n",
			"/// Used by the [`plugin_meta`] macro.\n",
			"#[doc(hidden)]\n",
			"pub use crys_bricks::env_cstr as __private_env_cstr;\n",
			"#[cfg(feature = \"sdk\")]\n",
			"#[cfg_attr(docsrs, doc(cfg(feature = \"sdk\")))]\n",
			"pub use hooks::{GameFrameFn, HookError, LevelEvents, NetMessageHookError};\n",
			"pub use plugin::{ErrorBuffer, PluginCallbacks, PluginDescriptor, PluginMetadata};\n",
			"pub use sys;\n",
		);
		let expected = concat!(
			"#![cfg_attr(docsrs, feature(doc_cfg))]\n",
			"\n",
			"mod api;\n",
			"mod context;\n",
			"mod plugin;\n",
			"\n",
			"#[cfg(feature = \"sdk\")]\n",
			"mod commands;\n",
			"\n",
			"#[cfg(feature = \"sdk\")]\n",
			"mod hooks;\n",
			"\n",
			"pub use api::{\n",
			"\tLoaderVersionInfo, MetamodApi, MetamodApiBinding, MetamodFeature, MetamodVersion,\n",
			"\tSourceHookVersions, UnsupportedFeature,\n",
			"};\n",
			"\n",
			"#[cfg(feature = \"sdk\")]\n",
			"#[cfg_attr(docsrs, doc(cfg(feature = \"sdk\")))]\n",
			"pub use commands::MetamodRegistrar;\n",
			"\n",
			"pub use context::{CachedContext, ContextKey, cached_context_key};\n",
			"\n",
			"/// Used by the [`plugin_meta`] macro.\n",
			"#[doc(hidden)]\n",
			"pub use crys_bricks::env_cstr as __private_env_cstr;\n",
			"\n",
			"#[cfg(feature = \"sdk\")]\n",
			"#[cfg_attr(docsrs, doc(cfg(feature = \"sdk\")))]\n",
			"pub use hooks::{GameFrameFn, HookError, LevelEvents, NetMessageHookError};\n",
			"\n",
			"pub use plugin::{ErrorBuffer, PluginCallbacks, PluginDescriptor, PluginMetadata};\n",
			"pub use sys;\n",
		);

		assert_sorts_to(source, expected);
		assert_unchanged(expected);
	}

	#[test]
	fn single_line_containers_are_split() {
		// like rustfmt splits them, so sorting after rustfmt changes nothing
		assert_sorts_to("impl X { fn b() {} fn a() {} }", "impl X {\n    fn a() {}\n\n    fn b() {}\n}");
		assert_sorts_to("impl X { fn a() {} fn b() {} }", "impl X {\n    fn a() {}\n\n    fn b() {}\n}");
		assert_sorts_to("mod m { fn b() {} fn a() {} }\n", "mod m {\n    fn a() {}\n\n    fn b() {}\n}\n");
		assert_sorts_to(
			"mod m { /* c */ fn b() {} fn a() {} }\n",
			"mod m { /* c */\n    fn a() {}\n\n    fn b() {}\n}\n",
		);
		assert_sorts_to("fn b() {} fn a() {}", "fn a() {}\n\nfn b() {}");
		assert_sorts_to("fn b() {} fn a() {} // c", "fn a() {} // c\n\nfn b() {}");

		// nothing to sort
		assert_unchanged("mod m { fn a() {} }\n");
		assert_unchanged("impl X { fn a() {} }");
		assert_unchanged("mod m { fn a() {}\n}\n");
	}

	fn sort_file(source: &str, options: &SortOptions) -> String {
		sort(source, &[SortTarget::File], options).unwrap_or_else(|error| panic!("{error}\n{source}"))
	}

	#[test]
	fn sorts_and_separates_items() {
		assert_sorts_to("fn b() {}\nfn a() {}\n", "fn a() {}\n\nfn b() {}\n");
		assert_sorts_to("fn b() {}\n\n\n\nfn a() {}\n", "fn a() {}\n\nfn b() {}\n");
		assert_sorts_to("fn b() {}   \nfn a() {}\n", "fn a() {}\n\nfn b() {}\n");
		assert_sorts_to(
			"fn f() {}\nuse b;\nconst C: u8 = 0;\nuse a;\npub use x;\nstruct S;\nconst B: u8 = 0;\n",
			"use a;\nuse b;\n\npub use x;\n\nconst B: u8 = 0;\nconst C: u8 = 0;\n\nstruct S;\n\nfn f() {}\n",
		);
	}

	#[test]
	fn spacing_within_groups() {
		// one-line items follow each other directly, and an item spanning several lines gets a blank line on both
		// sides ...
		assert_sorts_to(
			"const B: u8 = 0;\nconst A: [u8; 2] = [\n\t0, 1,\n];\nconst C: u8 = 0;\n",
			"const A: [u8; 2] = [\n\t0, 1,\n];\n\nconst B: u8 = 0;\nconst C: u8 = 0;\n",
		);

		// ... as do items with attributes, docs, or comments
		assert_sorts_to("use c;\n/// Doc\nuse b;\nuse a;\n", "use a;\n\n/// Doc\nuse b;\n\nuse c;\n");
		assert_sorts_to(
			"use b;\nuse a::{\n    One,\n    Two,\n};\n#[cfg(unix)]\nuse c;\n",
			"use a::{\n    One,\n    Two,\n};\n\nuse b;\n\n#[cfg(unix)]\nuse c;\n",
		);
		assert_sorts_to(
			"mod c;\n#[cfg(x)]\nmod b;\n#[cfg(x)]\nmod a;\nextern crate e;\n/// Docs\nextern crate d;\nextern crate f;\n",
			"/// Docs\nextern crate d;\n\nextern crate e;\nextern crate f;\n\nmod c;\n\n#[cfg(x)]\nmod a;\n\n#[cfg(x)]\nmod b;\n",
		);
		assert_sorts_to(
			"/// B\nconst B: u8 = 0;\nconst C: u8 = 0;\nconst D: u8 = 0;\n#[cfg(x)]\nconst A: u8 = 0;\n",
			"#[cfg(x)]\nconst A: u8 = 0;\n\n/// B\nconst B: u8 = 0;\n\nconst C: u8 = 0;\nconst D: u8 = 0;\n",
		);
		assert_sorts_to(
			"const B: u8 = 0;\n// about a\nconst A: u8 = 0;\nconst C: u8 = 0;\n",
			"// about a\nconst A: u8 = 0;\n\nconst B: u8 = 0;\nconst C: u8 = 0;\n",
		);

		// a section header of the new first item becomes the container's header
		assert_sorts_to(
			"const B: u8 = 0;\n\n// section\n\nconst A: u8 = 0;\n",
			"// section\n\nconst A: u8 = 0;\nconst B: u8 = 0;\n",
		);

		// other groups always get blank lines
		assert_sorts_to("fn b() {}\nfn a() {}\n", "fn a() {}\n\nfn b() {}\n");

		// the same goes for the items of `impl` blocks, traits, and `extern` blocks
		assert_sorts_to(
			"impl X {\n\tconst B: u8 = 0;\n\t#[cfg(x)]\n\tconst A: u8 = 0;\n\tconst C: u8 = 0;\n\tconst D: u8 = 0;\n}\n",
			"impl X {\n\t#[cfg(x)]\n\tconst A: u8 = 0;\n\n\tconst B: u8 = 0;\n\tconst C: u8 = 0;\n\tconst D: u8 = 0;\n}\n",
		);
		assert_sorts_to(
			"trait T {\n\tconst C: u8;\n\t/// Doc\n\tconst B: u8;\n\tconst A: u8;\n}\n",
			"trait T {\n\tconst A: u8;\n\n\t/// Doc\n\tconst B: u8;\n\n\tconst C: u8;\n}\n",
		);
		assert_sorts_to(
			"extern \"C\" {\n\tfn c();\n\t#[link_name = \"x\"]\n\tfn b();\n\tfn a();\n\tfn d();\n}\n",
			"extern \"C\" {\n\tfn a();\n\n\t#[link_name = \"x\"]\n\tfn b();\n\n\tfn c();\n\tfn d();\n}\n",
		);
	}

	#[test]
	fn targets_must_be_containers() {
		let source = "/// Docs\nfn b() {}\n#[cfg(x)]\nimpl X {}\nfn f() {\n\timpl Y {}\n}\n";
		let sorter = crate::Sorter::default();

		assert!(matches!(
			sorter.sort_str_within(source, &[SortTarget::Item(0)]),
			Err(SortError::NoContainer(0))
		));

		// the target is the start of the item's attributes
		let attributes = source.find("#[cfg(x)]").unwrap();
		let impl_token = source.find("impl X").unwrap();

		assert!(sorter.sort_str_within(source, &[SortTarget::Item(attributes)]).is_ok());
		assert!(matches!(
			sorter.sort_str_within(source, &[SortTarget::Item(impl_token)]),
			Err(SortError::NoContainer(start)) if start == impl_token
		));

		// items in function bodies are not sorted
		let nested = source.find("impl Y").unwrap();

		assert!(matches!(
			sorter.sort_str_within(source, &[SortTarget::Item(nested)]),
			Err(SortError::NoContainer(_))
		));
		assert!(matches!(
			sorter.sort_str_within(source, &[SortTarget::Item(10_000)]),
			Err(SortError::NoContainer(_))
		));
	}

	#[test]
	fn ties_do_not_depend_on_nested_order() {
		// the targeted `impl` is sorted, the other one is not: their order stays the same when sorting again
		let source = "impl X {\n    fn d() {}\n    fn a() {}\n}\n\nimpl X {\n    fn c() {}\n    fn b() {}\n}\n";
		let sorter = crate::Sorter::new(SortOptions::new().recursive(false));
		let targets = [SortTarget::File, SortTarget::Item(0)];
		let output = sorter.sort_str_within(source, &targets).unwrap();

		assert_eq!(
			output,
			"impl X {\n    fn a() {}\n\n    fn d() {}\n}\n\nimpl X {\n    fn c() {}\n    fn b() {}\n}\n"
		);
		assert_eq!(sorter.sort_str_within(&output, &targets).unwrap(), output);
	}

	#[test]
	fn traits_and_foreign_items() {
		assert_sorts_to(
			"trait T {\n\tfn b(&self);\n\tfn a();\n\ttype X;\n\tconst C: u8;\n}\n",
			"trait T {\n\ttype X;\n\n\tconst C: u8;\n\n\tfn a();\n\n\tfn b(&self);\n}\n",
		);
		assert_sorts_to(
			"extern \"C\" {\n\tfn b();\n\tstatic S: u8;\n\tfn a();\n\ttype T;\n}\n",
			"extern \"C\" {\n\ttype T;\n\n\tstatic S: u8;\n\n\tfn a();\n\tfn b();\n}\n",
		);
	}

	#[test]
	fn unchanged_when_sorted() {
		assert_unchanged("");
		assert_unchanged("\n");
		assert_unchanged("// only a comment\n");
		assert_unchanged("#![allow(dead_code)]\n");
		assert_unchanged("use a;\nuse b;\n\nfn a() {}\n\nfn b() {}\n");
		assert_unchanged("fn f() {\n\tfn b() {}\n\tfn a() {}\n}\n");
		assert_unchanged("enum E {\n\tB,\n\tA,\n}\n\nstruct S {\n\tb: u8,\n\ta: u8,\n}\n");
	}

	#[test]
	fn unicode() {
		assert_sorts_to("// é comment\nfn é() {}\nfn a() {}\n", "fn a() {}\n\n// é comment\nfn é() {}\n");
		assert_sorts_to(
			"fn r#type() {}\nfn Type() {}\nfn r#try() {}\n",
			"fn Type() {}\n\nfn r#try() {}\n\nfn r#type() {}\n",
		);
	}

	#[test]
	fn unusual_whitespace() {
		// comments after other whitespace than spaces and tabs still trail their item
		assert_sorts_to("fn b() {}\x0c// keep me\nfn a() {}\n", "fn a() {}\n\nfn b() {}\x0c// keep me\n");
		assert_sorts_to("fn b() {}\r/* keep me too */\nfn a() {}\n", "fn a() {}\n\nfn b() {}\r/* keep me too */\n");
		assert_sorts_to("fn b() {}\u{2028}// c\nfn a() {}\n", "fn a() {}\n\nfn b() {}\u{2028}// c\n");
		assert_sorts_to("fn b() {}\u{a0}\u{b}/* c */\nfn a() {}\n", "fn a() {}\n\nfn b() {}\u{a0}\u{b}/* c */\n");

		// directional marks are whitespace
		assert_sorts_to("fn b() {}\n\u{200e}\nfn a() {}\n", "fn a() {}\n\nfn b() {}\n");
		assert_sorts_to("fn b() {}\n\u{200f}// c\nfn a() {}\n", "// c\nfn a() {}\n\nfn b() {}\n");
	}
}
