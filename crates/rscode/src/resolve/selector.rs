//! [`Selector`]s: picking one of several `impl` blocks (or macro invocations) that a path names, and choosing the
//! selector that the canonical path of such a block (or invocation) shows.

use super::Resolver;
use super::impls::named_assoc_items;
use super::text::attribute_paths;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::Workspace;
use crate::path::CanonicalPath;
use crate::path::ItemPath;
use crate::path::Selector;
use crate::pattern::PathPattern;
use crate::source::TextRange;
use std::path::Path;

/// Where an item is: its file and range. Items of a file that several crates load are at the same place.
type Place<'ws> = (&'ws Path, TextRange);

impl Resolver<'_> {
	/// The selector that the canonical path of an `impl` block shows (`base` is that path without it), when the path
	/// names other blocks too: the first associated item name that no other block has, else the first attribute path
	/// that no other block has, else its index (see [`Selector`]). `None` when the path names the block alone (or does
	/// not name it, or cannot be written as a user path).
	pub(super) fn impl_selector(&self, impl_block: ItemId, base: &CanonicalPath) -> Option<Selector> {
		if !self.may_share_header(impl_block) {
			return None;
		}

		let named = self.header_blocks(impl_block, base)?;
		let places = places(self.ws, &named);
		let own = place(self.ws, impl_block);

		if places.len() < 2 {
			return None;
		}

		let others: Vec<ItemId> = named.iter().copied().filter(|&item| place(self.ws, item) != own).collect();
		let has_item = |block: ItemId, name: &str| named_assoc_items(self.ws, block).any(|item| self.ws.item(item).name() == Some(name));

		if let Some(name) = named_assoc_items(self.ws, impl_block)
			.filter_map(|item| self.ws.item(item).name.clone())
			.find(|name| !others.iter().any(|&other| has_item(other, name)))
		{
			return Some(Selector::Item(name));
		}

		if let Some(attribute) = attribute_paths(self.ws.item_text(impl_block))
			.into_iter()
			.find(|attribute| !others.iter().any(|&other| has_attribute(self.ws, other, attribute)))
		{
			return Some(Selector::Attribute(attribute));
		}

		let index = places.iter().position(|&known| known == own)?;

		Some(Selector::Index(u32::try_from(index + 1).ok()?))
	}

	/// The `impl` blocks that the header of `impl_block` names (`base` is the block's canonical path without a
	/// selector), the block among them. `None` when the header cannot be written as a user path, or does not name the
	/// block.
	fn header_blocks(&self, impl_block: ItemId, base: &CanonicalPath) -> Option<Vec<ItemId>> {
		let info = self.ws.item(impl_block).impl_info()?;
		let text = match &base.unresolved_self_ty {
			None => base.to_string(),
			Some(self_ty) => match &info.trait_text {
				Some(trait_text) => format!("impl {trait_text} for {self_ty}"),
				None => format!("impl {self_ty}"),
			},
		};
		let named = self.compute_item_path(&ItemPath::parse(&text).ok()?);

		named.contains(&impl_block).then_some(named)
	}

	/// The index that the canonical path of a macro invocation shows (`base` is that path without it), when the path
	/// names other invocations too (of the same macro, in the module or its `cfg` variants).
	pub(super) fn macro_selector(&self, call: ItemId, base: &CanonicalPath) -> Option<Selector> {
		let named = self.macro_calls(call, base)?;
		let places = places(self.ws, &named);

		if places.len() < 2 {
			return None;
		}

		let index = places.iter().position(|&known| known == place(self.ws, call))?;

		Some(Selector::Index(u32::try_from(index + 1).ok()?))
	}

	/// The invocations that the path of a macro invocation names (`base` is its canonical path without a selector): those
	/// of the same macro in its module (and the module's `cfg` variants), the invocation among them. `None` when the path
	/// does not name it.
	fn macro_calls(&self, call: ItemId, base: &CanonicalPath) -> Option<Vec<ItemId>> {
		let named = self.compute_item_path(&ItemPath::parse(&base.to_string()).ok()?);

		named.contains(&call).then_some(named)
	}

	/// Whether an item with this canonical path matches a pattern, with the pattern's selector tested by what it picks
	/// ([`Resolver::selects`]) rather than compared with the path's (see [`PathPattern::matches_any_selector`]).
	pub(crate) fn matches_pattern(&self, pattern: &PathPattern, item: ItemId, path: &CanonicalPath, is_selected: bool) -> bool {
		match &pattern.selector {
			Some(selector) => pattern.matches_any_selector(path, is_selected) && self.selects(selector, item),
			None => pattern.matches(path, is_selected),
		}
	}

	/// Whether another `impl` block may have the same header as `impl_block`: one for a type of the same name, and
	/// either inherent too or for a trait of the same name. A quick test before resolving the header.
	fn may_share_header(&self, impl_block: ItemId) -> bool {
		let trait_name = |block: ItemId| {
			let info = self.ws.item(block).impl_info()?;

			Some(info.trait_path.as_ref().map(|path| path.last().map(|segment| segment.name.clone())))
		};
		let own = trait_name(impl_block);
		let same_type = self.impls.same_self_name(self.ws, impl_block);

		same_type.iter().any(|&block| block != impl_block && trait_name(block) == own)
	}

	/// The items among `items` (`impl` blocks, or macro invocations) that `selector` picks.
	pub(super) fn select(&self, mut items: Vec<ItemId>, selector: &Selector) -> Vec<ItemId> {
		let ws = self.ws;

		match selector {
			Selector::Item(name) => {
				items.retain(|&block| named_assoc_items(ws, block).any(|item| ws.item(item).name.as_ref() == Some(name)));
			}

			Selector::Attribute(path) => items.retain(|&block| has_attribute(ws, block, path)),

			Selector::Index(index) => {
				let places = places(ws, &items);
				let Some(&wanted) = usize::try_from(*index).ok().and_then(|index| places.get(index.checked_sub(1)?)) else {
					return Vec::new();
				};

				items.retain(|&item| place(ws, item) == wanted);
			}
		}

		items
	}

	/// Whether `selector` picks the `impl` block of an item (the item itself, or the block of an associated item) among
	/// the blocks with the same header, or a macro invocation among those of the same macro in its module, as a path with
	/// that selector does (`impl Foo[2]`, `<Foo>[#attr]::name`, `m::name![2]`). Every selector that picks a block counts,
	/// not only the one its canonical path shows (items of inherent `impl`s show none). `false` for other items.
	pub fn selects(&self, selector: &Selector, item: ItemId) -> bool {
		let ws = self.ws;
		let target = match ws.item(item).kind {
			ItemKind::Impl | ItemKind::MacroCall => item,

			_ => match ws.parent(item) {
				Some(parent) if ws.item(parent).kind == ItemKind::Impl => parent,
				_ => return false,
			},
		};
		let base = CanonicalPath {
			selector: None,
			..self.compute_canonical_path(target)
		};
		let named = match ws.item(target).kind {
			ItemKind::Impl if self.may_share_header(target) => self.header_blocks(target, &base),
			ItemKind::Impl => None,
			_ => self.macro_calls(target, &base),
		};

		self.select(named.unwrap_or_else(|| vec![target]), selector).contains(&target)
	}
}

/// Whether an item has an outer attribute whose path is `path` or ends with it (see [`Selector::Attribute`]).
fn has_attribute(ws: &Workspace, item: ItemId, path: &str) -> bool {
	attribute_paths(ws.item_text(item))
		.iter()
		.any(|attribute| attribute == path || attribute.strip_suffix(path).is_some_and(|prefix| prefix.ends_with("::")))
}

/// The place of an item.
fn place(ws: &Workspace, item: ItemId) -> Place<'_> {
	(ws.file_of(item).path(), ws.item(item).range)
}

/// The distinct places of items, ordered by file path, then by position.
fn places<'ws>(ws: &'ws Workspace, items: &[ItemId]) -> Vec<Place<'ws>> {
	let mut places: Vec<Place<'ws>> = items.iter().map(|&item| place(ws, item)).collect();

	places.sort_by_key(|&(path, range)| (path, range.start, range.end));
	places.dedup();
	places
}
