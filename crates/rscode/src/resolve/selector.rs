//! [`Selector`]s: picking one of several `impl` blocks (or macro invocations) that a path names, and choosing the
//! selector that the canonical path of such a block (or invocation) shows.

use super::Resolver;
use super::fxhash::FxHashMap;
use super::fxhash::FxHashSet;
use super::impls::named_assoc_items;
use super::text::attribute_paths;
use super::vis::home_module;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::Workspace;
use crate::path::CanonicalPath;
use crate::path::ItemPath;
use crate::path::Selector;
use crate::pattern::PathPattern;
use crate::source::TextRange;
use smol_str::SmolStr;
use std::path::Path;
use std::sync::Mutex;
use std::sync::PoisonError;

/// Where an item is: its file and range. Items of a file that several crates load are at the same place.
type Place<'ws> = (&'ws Path, TextRange);

/// The selectors that the canonical paths of `impl` blocks and macro invocations show, by block or invocation (see
/// [`Resolver::impl_selector`]).
pub(super) type SelectorCache = Mutex<FxHashMap<ItemId, Option<Selector>>>;

impl Resolver<'_> {
	/// The selector of an `impl` block or macro invocation that the cache has, else the one among those that `compute`
	/// gives (which the cache keeps).
	fn cached_selector(&self, item: ItemId, compute: impl FnOnce() -> Vec<(ItemId, Option<Selector>)>) -> Option<Selector> {
		if let Some(selector) = self.selectors.lock().unwrap_or_else(PoisonError::into_inner).get(&item) {
			return selector.clone();
		}

		// (not locked while computing, which resolves paths)
		let computed = compute();
		let mut cache = self.selectors.lock().unwrap_or_else(PoisonError::into_inner);

		cache.extend(computed);
		cache.entry(item).or_default().clone()
	}

	/// The indexes that the canonical paths of the invocations with the path of `call` show (`base` is its canonical
	/// path without a selector), `call` among them (see [`Resolver::macro_selector`]).
	fn call_selectors(&self, call: ItemId, base: &CanonicalPath) -> Vec<(ItemId, Option<Selector>)> {
		let ws = self.ws;
		let Some(named) = self.macro_calls(call, base) else {
			return vec![(call, None)];
		};
		let places = places(ws, &named);
		let at: FxHashMap<Place<'_>, usize> = places.iter().enumerate().map(|(index, &place)| (place, index)).collect();

		// (those in modules with the same path have the same path)
		(named.into_iter())
			.filter(|&other| other == call || self.module_segments(home_module(ws, other)) == base.segments)
			.map(|other| {
				let index = (places.len() > 1).then(|| u32::try_from(at[&place(ws, other)] + 1).ok()).flatten();

				(other, index.map(Selector::Index))
			})
			.collect()
	}

	/// The `impl` blocks that `header` (the header of `impl_block` as a user path, see [`header_text`]) names, the
	/// block among them. `None` when the header does not parse, or does not name the block.
	fn header_blocks(&self, impl_block: ItemId, header: &str) -> Option<Vec<ItemId>> {
		let named = self.compute_item_path(&ItemPath::parse(header).ok()?);

		named.contains(&impl_block).then_some(named)
	}

	/// The selectors that the canonical paths of the `impl` blocks with the header of `impl_block` show (`base` is its
	/// canonical path without a selector), `impl_block` among them (see [`Resolver::impl_selector`]).
	fn header_selectors(&self, impl_block: ItemId, base: &CanonicalPath) -> Vec<(ItemId, Option<Selector>)> {
		let ws = self.ws;
		let Some(header) = self.may_share_header(impl_block).then(|| header_text(ws, impl_block, base)).flatten() else {
			return vec![(impl_block, None)];
		};
		let Some(named) = self.header_blocks(impl_block, &header) else {
			return vec![(impl_block, None)];
		};

		// (blocks with the same header text name the same blocks, so their selectors are computed from those too)
		let same: Vec<ItemId> = (named.iter().copied())
			.filter(|&block| block == impl_block || header_text(ws, block, &self.impl_base(block)).as_ref() == Some(&header))
			.collect();
		let places = places(ws, &named);

		if places.len() < 2 {
			return same.into_iter().map(|block| (block, None)).collect();
		}

		// the places with an associated item of each name, and with an attribute of each path (or ending with it)
		let at: FxHashMap<Place<'_>, usize> = places.iter().enumerate().map(|(index, &place)| (place, index)).collect();
		let mut names: FxHashMap<SmolStr, FxHashSet<usize>> = FxHashMap::default();
		let mut attributes: FxHashMap<String, FxHashSet<usize>> = FxHashMap::default();

		for &block in &named {
			let index = at[&place(ws, block)];

			for name in named_assoc_items(ws, block).filter_map(|item| ws.item(item).name.clone()) {
				names.entry(name).or_default().insert(index);
			}

			for attribute in attribute_paths(ws.item_text(block)) {
				for path in path_suffixes(&attribute) {
					attributes.entry(path.to_owned()).or_default().insert(index);
				}
			}
		}

		let only_at = |places: Option<&FxHashSet<usize>>, index: usize| {
			places.is_some_and(|places| places.len() == 1 && places.contains(&index))
		};

		(same.into_iter())
			.map(|block| {
				let index = at[&place(ws, block)];
				let item = (named_assoc_items(ws, block).filter_map(|item| ws.item(item).name.clone()))
					.find(|name| only_at(names.get(name), index))
					.map(Selector::Item);
				let attribute = || {
					(attribute_paths(ws.item_text(block)).into_iter())
						.find(|attribute| only_at(attributes.get(attribute), index))
						.map(Selector::Attribute)
				};

				(block, item.or_else(attribute).or_else(|| u32::try_from(index + 1).ok().map(Selector::Index)))
			})
			.collect()
	}

	/// The selector that the canonical path of an `impl` block shows (`base` is that path without it), when the path
	/// names other blocks too: the first associated item name that no other block has, else the first attribute path
	/// that no other block has, else its index (see [`Selector`]). `None` when the path names the block alone (or does
	/// not name it, or cannot be written as a user path).
	///
	/// (The selectors of the blocks with the same header are computed together, once: computing each on its own would
	/// take time quadratic in the number of blocks.)
	pub(super) fn impl_selector(&self, impl_block: ItemId, base: &CanonicalPath) -> Option<Selector> {
		self.cached_selector(impl_block, || self.header_selectors(impl_block, base))
	}

	/// The invocations that the path of a macro invocation names (`base` is its canonical path without a selector):
	/// those of the same macro in its module (and the module's `cfg` variants), the invocation among them. `None` when
	/// the path does not name it.
	fn macro_calls(&self, call: ItemId, base: &CanonicalPath) -> Option<Vec<ItemId>> {
		let named = self.compute_item_path(&ItemPath::parse(&base.to_string()).ok()?);

		named.contains(&call).then_some(named)
	}

	/// The index that the canonical path of a macro invocation shows (`base` is that path without it), when the path
	/// names other invocations too (of the same macro, in the module or its `cfg` variants).
	///
	/// (Like [`Resolver::impl_selector`], the indexes of invocations with the same path are computed together, once.)
	pub(super) fn macro_selector(&self, call: ItemId, base: &CanonicalPath) -> Option<Selector> {
		self.cached_selector(call, || self.call_selectors(call, base))
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
	/// the blocks with the same header, or a macro invocation among those of the same macro in its module, as a path
	/// with that selector does (`impl Foo[2]`, `<Foo>[#attr]::name`, `m::name![2]`): any selector that picks a block,
	/// not only the one its canonical path shows (items of inherent `impl`s show none). Picks no other items.
	pub fn selects(&self, selector: &Selector, item: ItemId) -> bool {
		let ws = self.ws;
		let target = match ws.item(item).kind {
			ItemKind::Impl | ItemKind::MacroCall => item,

			_ => match ws.parent(item) {
				Some(parent) if ws.item(parent).kind == ItemKind::Impl => parent,
				_ => return false,
			},
		};
		let named = match ws.item(target).kind {
			ItemKind::Impl if self.may_share_header(target) => {
				header_text(ws, target, &self.impl_base(target)).and_then(|header| self.header_blocks(target, &header))
			}

			ItemKind::Impl => None,

			_ => self.macro_calls(target, &CanonicalPath {
				selector: None,
				..self.compute_canonical_path(target)
			}),
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

/// The header of an `impl` block as a user path (`base` is its canonical path without a selector): `impl a::Foo`,
/// `impl Tr for a::Foo<T>`, or for a self type that is not loaded, `impl Tr for Vec<u8>`.
fn header_text(ws: &Workspace, impl_block: ItemId, base: &CanonicalPath) -> Option<String> {
	let info = ws.item(impl_block).impl_info()?;

	Some(match &base.unresolved_self_ty {
		None => base.to_string(),

		Some(self_ty) => match &info.trait_text {
			Some(trait_text) => format!("impl {trait_text} for {self_ty}"),
			None => format!("impl {self_ty}"),
		},
	})
}

/// An attribute path and its ends after each `::` (`a::b`, `b`): the paths that [`has_attribute`] finds it by.
fn path_suffixes(path: &str) -> impl Iterator<Item = &str> {
	std::iter::once(path).chain(path.match_indices("::").map(move |(at, _)| &path[at + 2..]))
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
