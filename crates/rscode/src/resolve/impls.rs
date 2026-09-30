//! `impl` blocks: what they implement, for which types, and the associated items reachable through types and traits.

use super::PathKind;
use super::fxhash::FxHashMap;
use super::scope::Tables;
use super::text::is_ident_char;
use super::text::skip_trivia;
use super::text::strip_keyword;
use super::walk::Walker;
use super::walk::Want;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::PathRef;
use crate::model::Workspace;
use crate::resolve::Namespace;
use crate::resolve::Res;

/// Kinds of items an `impl` block can be for (a trait for `impl dyn Trait`).
const SELF_TYPE_KINDS: &[ItemKind] = &[
	ItemKind::Struct,
	ItemKind::Enum,
	ItemKind::Union,
	ItemKind::TypeAlias,
	ItemKind::ForeignType,
	ItemKind::Trait,
];

/// Resolved `impl` blocks.
#[derive(Debug, Default)]
pub(super) struct ImplIndex {
	/// Every `impl` block, in item order.
	pub(super) all: Vec<ItemId>,

	/// The loaded items each `impl`'s self type resolves to.
	pub(super) self_types: FxHashMap<ItemId, Vec<ItemId>>,

	/// Everything each `impl`'s self type path resolves to (including external paths and primitives).
	pub(super) self_res: FxHashMap<ItemId, Vec<Res>>,

	/// The loaded traits each trait `impl` implements.
	pub(super) traits: FxHashMap<ItemId, Vec<ItemId>>,

	/// Everything each trait `impl`'s trait path resolves to.
	pub(super) trait_res: FxHashMap<ItemId, Vec<Res>>,

	/// `impl` blocks by self type, in item order.
	pub(super) by_self_type: FxHashMap<ItemId, Vec<ItemId>>,

	/// Trait `impl` blocks by trait, in item order.
	pub(super) by_trait: FxHashMap<ItemId, Vec<ItemId>>,
}

impl ImplIndex {
	pub(super) fn self_types(&self, impl_block: ItemId) -> &[ItemId] {
		self.self_types.get(&impl_block).map_or(&[], Vec::as_slice)
	}

	pub(super) fn traits(&self, impl_block: ItemId) -> &[ItemId] {
		self.traits.get(&impl_block).map_or(&[], Vec::as_slice)
	}
}

/// Associated items reachable as `Owner::name`: for types, the items of their inherent `impl`s, then of their trait
/// `impl`s (each in item order); for traits, their own items.
pub(super) fn assoc_index(ws: &Workspace, impls: &ImplIndex) -> FxHashMap<ItemId, Vec<ItemId>> {
	let mut inherent: FxHashMap<ItemId, Vec<ItemId>> = FxHashMap::default();
	let mut from_traits: FxHashMap<ItemId, Vec<ItemId>> = FxHashMap::default();

	for &impl_block in &impls.all {
		let is_trait_impl = ws.item(impl_block).impl_info().is_some_and(|info| info.trait_path.is_some());

		for &owner in impls.self_types(impl_block) {
			// `impl dyn Trait` items are not reachable as `Trait::name`
			if ws.item(owner).kind == ItemKind::Trait {
				continue;
			}

			let items = if is_trait_impl { &mut from_traits } else { &mut inherent };

			items.entry(owner).or_default().extend(named_assoc_items(ws, impl_block));
		}
	}

	for (owner, items) in from_traits {
		inherent.entry(owner).or_default().extend(items);
	}

	for krate in ws.crates() {
		for (id, data) in krate.items() {
			if data.kind == ItemKind::Trait {
				inherent.insert(id, named_assoc_items(ws, id).collect());
			}
		}
	}

	inherent
}

/// Resolves the self type and trait of every `impl` block (after imports are resolved).
pub(super) fn build(ws: &Workspace, tables: &Tables) -> ImplIndex {
	let mut index = ImplIndex::default();

	for krate in ws.crates() {
		for (id, data) in krate.items() {
			let Some(info) = data.impl_info() else {
				continue;
			};

			index.all.push(id);

			let generics = generic_params(ws, id);
			let mut walker = Walker::new(ws, tables, ws.module_of(id), PathKind::Code);

			// a generic parameter (`impl<T> Trait for T`) shadows items of the same name
			if let Some(path) = info.self_ty.base_path().filter(|path| !starts_with_generic(path, &generics)) {
				let res = resolve_type(&mut walker, path);
				let items = loaded(ws, &res, SELF_TYPE_KINDS);

				index.self_res.insert(id, res);
				index.self_types.insert(id, items);
			}

			if let Some(path) = &info.trait_path {
				let res = resolve_type(&mut walker, path);
				let items = loaded(ws, &res, &[ItemKind::Trait]);

				index.trait_res.insert(id, res);
				index.traits.insert(id, items);
			}
		}
	}

	for &impl_block in &index.all {
		for &self_type in index.self_types.get(&impl_block).into_iter().flatten() {
			index.by_self_type.entry(self_type).or_default().push(impl_block);
		}

		for &trait_item in index.traits.get(&impl_block).into_iter().flatten() {
			index.by_trait.entry(trait_item).or_default().push(impl_block);
		}
	}

	index
}

/// Names of the type and const parameters in the generics list at the start of `text` (after `impl`).
fn generic_param_names(text: &str) -> Vec<&str> {
	let Some(list) = skip_trivia(text).strip_prefix('<') else {
		return Vec::new();
	};

	let bytes = list.as_bytes();
	let mut names = Vec::new();
	let mut depth = 0usize;
	let mut param_start = 0;
	let mut index = 0;

	while index < bytes.len() {
		match bytes[index] {
			// `Fn() -> T` bounds
			b'-' if bytes.get(index + 1) == Some(&b'>') => index += 1,

			b'<' | b'(' | b'[' | b'{' => depth += 1,

			b'>' | b')' | b']' | b'}' if depth == 0 => {
				names.extend(param_name(&list[param_start..index]));
				break;
			}

			b'>' | b')' | b']' | b'}' => depth -= 1,

			b',' if depth == 0 => {
				names.extend(param_name(&list[param_start..index]));
				param_start = index + 1;
			}

			_ => {}
		}

		index += 1;
	}

	names
}

/// Names of the type and const generic parameters of an `impl` block (`impl<'a, T: Tr, const N: usize>` → `T`, `N`).
fn generic_params(ws: &Workspace, impl_block: ItemId) -> Vec<&str> {
	let data = ws.item(impl_block);
	let range = data.range;

	// the header starts after the outer attributes (and doc comments, which could mention `impl<...>`)
	let start = if (range.start..=range.end).contains(&data.attrs.after_attrs) {
		data.attrs.after_attrs
	} else {
		range.start
	};

	ws.file_of(impl_block)
		.text()
		.get(start..range.end)
		.and_then(impl_header)
		.map(generic_param_names)
		.unwrap_or_default()
}

/// The text after the `impl` keyword of an `impl` block (skipping `default` and `unsafe`).
fn impl_header(text: &str) -> Option<&str> {
	let mut rest = skip_trivia(text);

	while let Some(after) = strip_keyword(rest, "default").or_else(|| strip_keyword(rest, "unsafe")) {
		rest = skip_trivia(after);
	}

	strip_keyword(rest, "impl")
}

fn loaded(ws: &Workspace, res: &[Res], kinds: &[ItemKind]) -> Vec<ItemId> {
	res.iter()
		.filter_map(|res| match res {
			Res::Item(id) if kinds.contains(&ws.item(*id).kind) => Some(*id),
			_ => None,
		})
		.collect()
}

/// The named associated items of an `impl` block or trait, in source order.
pub(super) fn named_assoc_items(ws: &Workspace, container: ItemId) -> impl Iterator<Item = ItemId> + '_ {
	ws.children(container).filter(|&child| {
		let data = ws.item(child);

		data.kind.is_associated() && data.name.is_some()
	})
}

/// The name of one generic parameter (`T: Bound`, `const N: usize`), or `None` for lifetimes.
fn param_name(param: &str) -> Option<&str> {
	let mut param = skip_trivia(param);

	// attributes on parameters
	while let Some(attr) = param.strip_prefix("#[") {
		let end = attr.find(']')?;

		param = skip_trivia(&attr[end + 1..]);
	}

	if param.starts_with('\'') {
		return None;
	}

	if let Some(rest) = strip_keyword(param, "const") {
		param = skip_trivia(rest);
	}

	let param = param.strip_prefix("r#").unwrap_or(param);
	let end = param.find(|char| !is_ident_char(char)).unwrap_or(param.len());

	(end > 0).then(|| &param[..end])
}

fn resolve_type(walker: &mut Walker<'_>, path: &PathRef) -> Vec<Res> {
	walker
		.resolve(path, Want::One(Namespace::Type))
		.into_iter()
		.map(|found| found.res)
		.collect()
}

fn starts_with_generic(path: &PathRef, generics: &[&str]) -> bool {
	!path.leading_colon && path.segments.first().is_some_and(|segment| generics.contains(&segment.name.as_str()))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn finds_generic_params() {
		fn names(text: &str) -> Vec<&str> {
			impl_header(text).map(generic_param_names).unwrap_or_default()
		}

		assert_eq!(names("impl<T> Tr for T {}"), ["T"]);
		assert_eq!(
			names("unsafe impl<'a, T: Fn(u8) -> Vec<u8>, const N: usize> Tr for [T; N] {}"),
			["T", "N"]
		);
		assert_eq!(names("impl < /* c */ #[cfg(x)] U , > Tr for U {}"), ["U"]);
		assert_eq!(names("impl<T: Iterator<Item = (A, B)>, r#S> X {}"), ["T", "S"]);
		assert_eq!(names("default impl<T> Tr for T {}"), ["T"]);
		assert_eq!(names("impl Tr for Foo {}"), Vec::<&str>::new());
		assert_eq!(names("implementation"), Vec::<&str>::new());
		assert_eq!(names("impl<"), Vec::<&str>::new());
		assert_eq!(names("impl<T"), Vec::<&str>::new());
	}
}
