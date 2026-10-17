//! Moving sibling items within their existing container.

use super::EditSet;
use super::InsertPosition;
use super::describe;
use super::trivia;
use super::trivia::Placement;
use super::trivia::Spacing;
use crate::CfgExpr;
use crate::Error;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::Workspace;
use crate::path::ItemPath;
use crate::resolve::Resolver;
use crate::source::SourceFile;
use crate::source::TextRange;
use serde::Serialize;
use std::path::PathBuf;

/// A planned move of one item path within its existing container.
#[derive(Debug, Clone, Serialize)]
pub struct Movement {
	/// The single-file edit, to preview or apply atomically.
	#[serde(skip)]
	pub edits: EditSet,

	/// Canonical paths of the moved item and its `cfg` variants.
	pub moved: Vec<String>,

	/// The canonical path of their existing container.
	pub parent: String,

	/// The edited file.
	pub file: PathBuf,
}

/// The physical container whose siblings are being reordered.
#[derive(Debug, Clone, Copy)]
struct Container<'ws> {
	item: ItemId,
	file: &'ws SourceFile,
	body: TextRange,
}

/// A moved source block and whether its first non-blank line began a line in the original text.
struct ContentBlock {
	text: String,
	starts_line: bool,
}

impl Container<'_> {
	fn same_place(&self, other: &Self) -> bool {
		self.file.path() == other.file.path() && self.body == other.body
	}
}

/// Plans moving the item named by `path` within its existing syntactic container.
///
/// [`InsertPosition::Before`] and [`InsertPosition::After`] name a sibling in that same container; `start` and `end`
/// mean the first and last sibling positions. Variants with directly complementary `cfg` predicates, such as `X` and
/// `not(X)`, move as one group. The item's complete source moves with it, including outer docs and attributes,
/// attached comments above it, and a trailing comment that ends its line.
///
/// The move is one whole-file replacement in an [`EditSet`], so it previews and applies atomically. Imports (`use`
/// items and their leaves), fields, variants, macro definitions/invocations and their entries, `#[macro_use]` modules
/// and `extern crate` items, and out-of-line module declarations are refused because their surrounding syntax, lexical
/// scope, or file-resolution semantics need specialized handling. No item may cross a direct-sibling macro-scope
/// barrier (`macro_rules!`, an item-position macro invocation, or a `#[macro_use]` item). The destination must be in the
/// existing container; this operation never changes paths or references.
pub fn move_item(resolver: &Resolver<'_>, path: &ItemPath, position: &InsertPosition) -> Result<Movement, Error> {
	let ws = resolver.workspace();
	let items = moved_items(resolver, path)?;
	let first = *items.first().expect("moved_items returns at least one item");
	let container = container_of(ws, first, path)?;

	for &item in &items[1..] {
		let other = container_of(ws, item, path)?;

		if !container.same_place(&other) {
			return Err(Error::Unsupported(format!(
				"the `cfg` variants of `{path}` are not siblings of the same container"
			)));
		}
	}

	let mut item_ranges: Vec<TextRange> = items.iter().map(|&item| ws.item(item).range).collect();

	item_ranges.sort();
	item_ranges.dedup();

	let text = container.file.text();
	let anchors = match position {
		InsertPosition::Before(anchor) | InsertPosition::After(anchor) => {
			let anchor = ItemPath::parse(anchor)?;

			Some(anchor_ranges(resolver, &container, &items, &anchor)?)
		}
		InsertPosition::Start | InsertPosition::End => None,
	};

	if already_positioned(ws, &container, &item_ranges, position, anchors.as_deref()) {
		return Ok(movement(resolver, &container, &items, EditSet::new()));
	}

	let removed = trivia::movement_ranges(text, &item_ranges);
	let line_ending = trivia::line_ending(text);
	let separator = format!("{line_ending}{line_ending}");
	let blocks: Vec<ContentBlock> = removed
		.iter()
		.filter_map(|&range| content_block(text, range))
		.filter(|block| !block.text.is_empty())
		.collect();
	let without = without_ranges(text, &removed);
	let body = shifted_range(container.body, &removed);
	let placement = destination(ws, &container, &items, &removed, position, body, anchors.as_deref());

	check_macro_barriers(ws, &container, &items, &removed, placement, path)?;

	let indent = trivia::body_indent(&without, body);
	let source = content_source(&blocks, &separator, &indent);
	let starts_line = blocks.first().is_none_or(|block| block.starts_line);
	let insertion = trivia::verbatim_insertion(&without, placement, &source, starts_line, &indent, line_ending, Spacing::default());
	let mut final_text = without;

	final_text.replace_range(insertion.range.start..insertion.range.end, &insertion.replacement);

	let mut edits = EditSet::new();

	edits.replace_file(container.file, final_text);

	Ok(movement(resolver, &container, &items, edits))
}

fn movement(resolver: &Resolver<'_>, container: &Container<'_>, items: &[ItemId], edits: EditSet) -> Movement {
	let mut moved = Vec::new();

	for &item in items {
		let canonical = resolver.canonical_path(item).to_string();

		if !moved.contains(&canonical) {
			moved.push(canonical);
		}
	}

	Movement {
		edits,
		moved,
		parent: resolver.canonical_path(container.item).to_string(),
		file: container.file.path().to_path_buf(),
	}
}

/// Resolves and validates the items that move together.
fn moved_items(resolver: &Resolver<'_>, path: &ItemPath) -> Result<Vec<ItemId>, Error> {
	let ws = resolver.workspace();
	let mut items = resolver.resolve_item_path(path);

	if items.is_empty() {
		return Err(Error::NotFound(path.to_string()));
	}

	super::check_private_imports(resolver, path, &mut items)?;
	distinct_places(ws, &mut items);

	if items.len() > 1 {
		let exclusive = items.iter().enumerate().all(|(index, &left)| {
			items[index + 1..]
				.iter()
				.all(|&right| cfgs_are_exclusive(ws.effective_cfg(left).as_ref(), ws.effective_cfg(right).as_ref()))
		});

		if !exclusive || super::impl_headers_differ(ws, &items) {
			return Err(Error::Ambiguous {
				path: path.to_string(),
				candidates: items.iter().map(|&item| describe(resolver, item)).collect(),
			});
		}
	}

	for &item in &items {
		check_movable(ws, item, path)?;
	}

	Ok(items)
}

/// The container in whose body `item` is a direct child.
fn container_of<'ws>(ws: &'ws Workspace, item: ItemId, path: &ItemPath) -> Result<Container<'ws>, Error> {
	let Some(parent) = ws.parent(item) else {
		return Err(Error::Unsupported(format!("`{path}` is a crate root, which cannot be moved")));
	};

	container(ws, parent).ok_or_else(|| Error::Unsupported(format!("`{path}` is not a sibling item of a module, `impl` block, trait, or `extern` block")))
}

/// The editable body of `item`, when it is a supported sibling container.
fn container(ws: &Workspace, item: ItemId) -> Option<Container<'_>> {
	let data = ws.item(item);

	if data.kind == ItemKind::Module {
		let info = data.module_info()?;

		return match info.inline {
			true => Some(Container {
				item,
				file: ws.file_of(item),
				body: info.body?,
			}),

			false => {
				let file = ws.krate(item.krate()).file(info.file?);

				Some(Container {
					item,
					file,
					body: TextRange::new(0, file.text().len()),
				})
			}
		};
	}

	if !matches!(data.kind, ItemKind::Impl | ItemKind::Trait | ItemKind::ExternBlock) {
		return None;
	}

	Some(Container {
		item,
		file: ws.file_of(item),
		body: data.body()?,
	})
}

fn check_movable(ws: &Workspace, item: ItemId, path: &ItemPath) -> Result<(), Error> {
	let data = ws.item(item);
	let why = if has_macro_use(ws, item) {
		Some("items with `#[macro_use]` are macro-scope barriers")
	} else {
		match data.kind {
			ItemKind::Use | ItemKind::Import => Some("imports need use-tree-aware movement"),
			ItemKind::Field => Some("fields need comma-aware movement"),
			ItemKind::Variant => Some("enum variants need comma-aware movement"),
			ItemKind::MacroRules | ItemKind::MacroCall | ItemKind::AssocMacro | ItemKind::ForeignMacro => Some("macros can depend on their lexical position"),
			ItemKind::Module if data.module_info().is_some_and(|info| !info.inline) => {
				Some("moving an out-of-line module declaration can change how its file is resolved")
			}
			_ if ws.parent(item).is_some_and(|parent| ws.item(parent).kind == ItemKind::MacroCall) => {
				Some("entries of macro invocations need macro-specific movement")
			}
			_ => None,
		}
	};

	match why {
		Some(why) => Err(Error::Unsupported(format!("cannot move `{path}`: {why}"))),
		None => Ok(()),
	}
}

/// Whether an item is a lexical macro-scope barrier. The model flag covers active and unknown `cfg_attr`s; inspecting
/// module and `extern crate` syntax also catches `cfg_attr(..., macro_use)` when its predicate is currently false.
fn is_macro_barrier(ws: &Workspace, item: ItemId) -> bool {
	let data = ws.item(item);

	matches!(
		data.kind,
		ItemKind::MacroRules | ItemKind::MacroCall | ItemKind::AssocMacro | ItemKind::ForeignMacro
	) || has_macro_use(ws, item)
}

fn has_macro_use(ws: &Workspace, item: ItemId) -> bool {
	let data = ws.item(item);

	if data.attrs.macro_use {
		return true;
	}

	fn attributes_have_macro_use(attrs: &[syn::Attribute]) -> bool {
		fn meta_has_macro_use(meta: &syn::Meta) -> bool {
			if meta.path().is_ident("macro_use") {
				return true;
			}

			let syn::Meta::List(list) = meta else {
				return false;
			};

			if !list.path.is_ident("cfg_attr") {
				return false;
			}

			CfgExpr::parse_cfg_attr(list.tokens.clone()).is_ok_and(|(_, attrs)| {
				attrs
					.into_iter()
					.filter_map(|tokens| syn::parse2::<syn::Meta>(tokens).ok())
					.any(|meta| meta_has_macro_use(&meta))
			})
		}

		attrs.iter().any(|attr| meta_has_macro_use(&attr.meta))
	}

	match (data.kind, syn::parse_str::<syn::Item>(ws.item_text(item))) {
		(ItemKind::Module, Ok(syn::Item::Mod(item))) => attributes_have_macro_use(&item.attrs),
		(ItemKind::ExternCrate, Ok(syn::Item::ExternCrate(item))) => attributes_have_macro_use(&item.attrs),
		_ => false,
	}
}

/// Refuses a move across a direct-sibling macro definition, invocation, or `#[macro_use]` item. These barriers split
/// a container into lexical macro-scope segments, like `rscode_sort` does: the moved variants and their destination
/// must all be in the same segment.
fn check_macro_barriers(
	ws: &Workspace,
	container: &Container<'_>,
	moved: &[ItemId],
	removed: &[TextRange],
	placement: Placement,
	path: &ItemPath,
) -> Result<(), Error> {
	let mut barriers: Vec<TextRange> = ws
		.children(container.item)
		.filter(|&item| is_macro_barrier(ws, item))
		.map(|item| ws.item(item).range)
		.collect();

	barriers.sort();
	barriers.dedup();

	let segment_of = |offset: usize| barriers.partition_point(|barrier| barrier.start < offset);
	let Some(source_segment) = moved.first().map(|&item| segment_of(ws.item(item).range.start)) else {
		return Ok(());
	};

	if moved.iter().any(|&item| segment_of(ws.item(item).range.start) != source_segment) {
		return Err(macro_barrier(path));
	}

	let shifted: Vec<TextRange> = barriers.iter().map(|&barrier| shifted_range(barrier, removed)).collect();
	let destination_segment = match placement {
		Placement::Before(sibling) => shifted.partition_point(|barrier| barrier.start < sibling.start),
		Placement::After(sibling) => shifted.partition_point(|barrier| barrier.start <= sibling.start),
		Placement::End(_) => shifted.len(),
	};

	match destination_segment == source_segment {
		true => Ok(()),
		false => Err(macro_barrier(path)),
	}
}

fn macro_barrier(path: &ItemPath) -> Error {
	Error::Unsupported(format!(
		"cannot move `{path}` across a macro-scope barrier (`macro_rules!`, a macro invocation, or a `#[macro_use]` item)"
	))
}

/// Whether removing the moved sibling ranges and inserting them at `position` would leave their sibling order
/// unchanged. Such a move is a true no-op: returning before source extraction preserves comments and whitespace byte
/// for byte.
fn already_positioned(ws: &Workspace, container: &Container<'_>, moved: &[TextRange], position: &InsertPosition, anchors: Option<&[TextRange]>) -> bool {
	let mut children: Vec<TextRange> = ws.children(container.item).map(|child| ws.item(child).range).collect();

	children.sort();
	children.dedup();

	let positions: Vec<usize> = children
		.iter()
		.enumerate()
		.filter_map(|(index, range)| moved.contains(range).then_some(index))
		.collect();
	let Some((&first, rest)) = positions.split_first() else {
		return false;
	};
	let last = *positions.last().unwrap_or(&first);

	if positions.len() != moved.len() || rest.iter().enumerate().any(|(index, &position)| position != first + index + 1) {
		return false;
	}

	match position {
		InsertPosition::Start => first == 0,
		InsertPosition::End => last + 1 == children.len(),

		InsertPosition::Before(_) => anchors
			.and_then(|anchors| anchors.iter().filter_map(|anchor| children.iter().position(|child| child == anchor)).min())
			.is_some_and(|anchor| last + 1 == anchor),

		InsertPosition::After(_) => anchors
			.and_then(|anchors| anchors.iter().filter_map(|anchor| children.iter().position(|child| child == anchor)).max())
			.is_some_and(|anchor| anchor + 1 == first),
	}
}

/// Finds the destination in the text after the moved ranges have been removed.
fn destination(
	ws: &Workspace,
	container: &Container<'_>,
	moved: &[ItemId],
	removed: &[TextRange],
	position: &InsertPosition,
	body: TextRange,
	anchors: Option<&[TextRange]>,
) -> Placement {
	let moved_ranges: Vec<TextRange> = moved.iter().map(|&item| ws.item(item).range).collect();
	let remaining = || {
		ws.children(container.item)
			.filter(|&child| !moved_ranges.contains(&ws.item(child).range))
			.map(|child| shifted_range(ws.item(child).range, removed))
	};

	match position {
		InsertPosition::Start => remaining().min_by_key(|range| range.start).map_or(Placement::End(body), Placement::Before),
		InsertPosition::End => remaining().max_by_key(|range| range.start).map_or(Placement::End(body), Placement::After),

		InsertPosition::Before(_) | InsertPosition::After(_) => {
			let anchors = anchors.expect("relative positions have resolved anchor ranges");
			let shifted = anchors.iter().map(|&range| shifted_range(range, removed));
			let range = match position {
				InsertPosition::Before(_) => shifted.min_by_key(|range| range.start).map(Placement::Before),
				InsertPosition::After(_) => shifted.max_by_key(|range| range.start).map(Placement::After),
				_ => unreachable!(),
			};

			range.expect("anchor_ranges returns at least one range")
		}
	}
}

/// The ranges of the direct siblings named by `anchor`.
fn anchor_ranges(resolver: &Resolver<'_>, container: &Container<'_>, moved: &[ItemId], anchor: &ItemPath) -> Result<Vec<TextRange>, Error> {
	let ws = resolver.workspace();
	let mut resolved = resolver.resolve_item_path(anchor);

	if resolved.is_empty() {
		return Err(Error::NotFound(anchor.to_string()));
	}

	super::check_private_imports(resolver, anchor, &mut resolved)?;
	let mut siblings = Vec::new();

	for item in resolved {
		if let Some(sibling) = sibling_in(ws, item, container)
			&& !siblings.iter().any(|&known| same_place(ws, known, sibling))
		{
			siblings.push(sibling);
		}
	}

	if siblings.is_empty() {
		return Err(Error::Unsupported(format!(
			"`{anchor}` is not a sibling in `{}`",
			resolver.canonical_path(container.item)
		)));
	}

	if siblings.iter().any(|&anchor| moved.iter().any(|&item| same_place(ws, item, anchor))) {
		return Err(Error::Unsupported("an item cannot be moved relative to itself".to_owned()));
	}

	let mut ranges: Vec<TextRange> = siblings.into_iter().map(|item| ws.item(item).range).collect();

	ranges.sort();
	ranges.dedup();

	Ok(ranges)
}

/// The direct child of `container` that contains `item`.
fn sibling_in(ws: &Workspace, mut item: ItemId, expected: &Container<'_>) -> Option<ItemId> {
	loop {
		let parent = ws.parent(item)?;

		if container(ws, parent).is_some_and(|container| container.same_place(expected)) {
			return Some(item);
		}

		if !matches!(ws.item(parent).kind, ItemKind::Use | ItemKind::ExternBlock | ItemKind::MacroCall) {
			return None;
		}

		item = parent;
	}
}

fn same_place(ws: &Workspace, left: ItemId, right: ItemId) -> bool {
	left == right || (ws.file_of(left).path() == ws.file_of(right).path() && ws.item(left).range == ws.item(right).range)
}

fn distinct_places(ws: &Workspace, items: &mut Vec<ItemId>) {
	let mut distinct = Vec::new();

	for item in items.drain(..) {
		if !distinct.iter().any(|&known| same_place(ws, known, item)) {
			distinct.push(item);
		}
	}

	*items = distinct;
}

/// Whether two understood `cfg` predicates cannot both be true.
fn cfgs_are_exclusive(left: Option<&CfgExpr>, right: Option<&CfgExpr>) -> bool {
	fn false_predicate(expression: &CfgExpr) -> bool {
		match expression {
			CfgExpr::Bool(value) => !value,
			CfgExpr::All(expressions) => expressions.iter().any(false_predicate),
			CfgExpr::Any(expressions) => expressions.iter().all(false_predicate),
			CfgExpr::Not(expression) => matches!(expression.as_ref(), CfgExpr::Bool(true)),
			CfgExpr::Name(_) | CfgExpr::KeyValue(_, _) | CfgExpr::Other(_) => false,
		}
	}

	fn opposites(left: &CfgExpr, right: &CfgExpr) -> bool {
		match (left, right) {
			(CfgExpr::All(expressions), right) => expressions.iter().any(|left| opposites(left, right)),
			(left, CfgExpr::All(expressions)) => expressions.iter().any(|right| opposites(left, right)),
			(CfgExpr::Not(left), right) => left.as_ref() == right,
			(left, CfgExpr::Not(right)) => left == right.as_ref(),
			_ => false,
		}
	}

	let (Some(left), Some(right)) = (left, right) else {
		return false;
	};

	false_predicate(left) || false_predicate(right) || opposites(left, right)
}

/// The non-blank lines of a removal range, retaining every byte on those lines (including indentation and trailing
/// spaces). Spaces removed only to separate the item from following code on the same line are not part of its source.
/// The insertion supplies only the surrounding blank lines.
fn content_block(text: &str, range: TextRange) -> Option<ContentBlock> {
	let source = text.get(range.start..range.end)?;
	let mut lines = Vec::new();
	let mut start = 0;

	for line in source.split_inclusive('\n') {
		let mut end = start + line.len();

		if line.ends_with('\n') {
			end -= 1;

			if end > start && source.as_bytes()[end - 1] == b'\r' {
				end -= 1;
			}
		}

		lines.push((start, end));
		start += line.len();
	}

	if start < source.len() {
		lines.push((start, source.len()));
	}

	let first = lines.iter().position(|&(start, end)| !source[start..end].trim().is_empty())?;
	let last = lines.iter().rposition(|&(start, end)| !source[start..end].trim().is_empty())?;
	let absolute_start = range.start + lines[first].0;
	let block = &source[lines[first].0..lines[last].1];
	let block = match text.as_bytes().get(range.end) {
		Some(b'\r' | b'\n') | None => block,
		Some(_) => block.trim_end_matches([' ', '\t']),
	};
	let starts_line = absolute_start == 0 || text.as_bytes().get(absolute_start - 1) == Some(&b'\n');

	let text = match starts_line {
		true => block.to_owned(),
		false => block.trim_start_matches([' ', '\t']).to_owned(),
	};

	Some(ContentBlock { text, starts_line })
}

/// Joins moved blocks, adding the container's indentation only to blocks that originally followed code on their
/// first line. The first block is indented by [`trivia::verbatim_insertion`] from the same explicit metadata.
fn content_source(blocks: &[ContentBlock], separator: &str, indent: &str) -> String {
	let mut source = String::new();

	for (index, block) in blocks.iter().enumerate() {
		if index > 0 {
			source.push_str(separator);

			if !block.starts_line {
				source.push_str(indent);
			}
		}

		source.push_str(&block.text);
	}

	source
}

/// Removes sorted, disjoint ranges from a text.
fn without_ranges(text: &str, ranges: &[TextRange]) -> String {
	let mut result = String::with_capacity(text.len());
	let mut at = 0;

	for range in ranges {
		result.push_str(&text[at..range.start]);
		at = range.end;
	}

	result.push_str(&text[at..]);
	result
}

/// Adjusts a range to its offsets after `removed` ranges have been deleted.
fn shifted_range(range: TextRange, removed: &[TextRange]) -> TextRange {
	TextRange::new(shifted_offset(range.start, removed), shifted_offset(range.end, removed))
}

fn shifted_offset(offset: usize, removed: &[TextRange]) -> usize {
	let mut deleted = 0;

	for range in removed {
		if offset >= range.end {
			deleted += range.end - range.start;
		} else if offset > range.start {
			return range.start - deleted;
		} else {
			break;
		}
	}

	offset - deleted
}
