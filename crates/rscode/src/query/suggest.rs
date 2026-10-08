//! What a path that names no item may have meant: the one item whose path ends like it, and items named like its
//! last segment.

use crate::model::ItemData;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::Workspace;
use crate::path::Anchor;
use crate::path::ItemPath;
use crate::resolve::Resolver;
use serde::Serialize;
use std::collections::BTreeMap;

/// The most items [`Suggestions::similar`] lists.
const MAX_SIMILAR: usize = 5;

/// What a path that names no item may have meant (see [`suggest`]).
#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize)]
pub struct Suggestions {
	/// For a plain path without an anchor, selector, or `!` (`Type::method`, `module::Item`, `name`): the canonical
	/// path of the only item whose path ends with the path's segments, if exactly one does (its `cfg` variants count
	/// as one).
	pub unique_suffix: Option<String>,

	/// The canonical paths of up to five items named like the path's last segment (compared without case when no
	/// name is equal), in every loaded crate: the items whose path ends like the path first, then the shortest paths.
	pub similar: Vec<String>,

	/// How many items (with distinct paths) are named like the path's last segment, of which
	/// [`Suggestions::similar`] lists the first.
	pub named: usize,
}

/// The name of an item that the last segment of a path is compared with: its name, or for a path with `!`
/// (`macro_call`), the name of the macro of an invocation in a module (not that of a `macro_rules!` definition).
fn compared_name<'ws>(ws: &Workspace, item: ItemId, data: &'ws ItemData, macro_call: bool) -> Option<&'ws str> {
	match macro_call {
		true => ws.parent(item).filter(|&parent| ws.item(parent).kind == ItemKind::Module).and_then(|_| data.macro_name()),
		false => data.name.as_deref().filter(|_| data.kind.is_nameable() && data.kind != ItemKind::Import),
	}
}

/// What `path`, which names no item, may have meant: items of every loaded crate (selected or not) named like its
/// last segment, and, for a plain path without an anchor, the only item whose path ends like it. For a path with `!`,
/// the items are the invocations of macros of that name in modules (`m::name![2]`), not the macros' definitions.
/// Nothing is suggested for `use` paths, `impl` blocks, and fields.
pub fn suggest(resolver: &Resolver<'_>, path: &ItemPath) -> Suggestions {
	let Some(last) = path.segments.last().filter(|_| !path.import && path.field.is_none()) else {
		return Suggestions::default();
	};

	let ws = resolver.workspace();
	let named = |exact: bool| -> Vec<ItemId> {
		(ws.crates().iter())
			.flat_map(|krate| krate.items())
			.filter(|&(item, data)| {
				compared_name(ws, item, data, path.macro_call).is_some_and(|name| match exact {
					true => name == last.as_str(),
					false => name.eq_ignore_ascii_case(last.as_str()),
				})
			})
			.map(|(item, _)| item)
			.collect()
	};
	let mut items = named(true);

	if items.is_empty() {
		items = named(false);
	}

	// by canonical path (`cfg` variants share theirs), whether it ends like the path
	let plain = path.qualifier.is_none() && path.anchor == Anchor::None && path.selector.is_none() && !path.macro_call;
	let mut paths: BTreeMap<String, bool> = BTreeMap::new();

	for item in items {
		let canonical = resolver.canonical_path(item);
		let flat = canonical.flat_segments();
		let ends_like = plain && flat.len() >= path.segments.len() && {
			let tail = &flat[flat.len() - path.segments.len()..];

			tail.iter().zip(&path.segments).all(|(segment, wanted)| segment == wanted.as_str())
		};

		*paths.entry(canonical.to_string()).or_default() |= ends_like;
	}

	let suffixed: Vec<&String> = paths.iter().filter(|(_, ends_like)| **ends_like).map(|(path, _)| path).collect();
	let unique_suffix = match suffixed.as_slice() {
		[path] => Some((*path).clone()),
		_ => None,
	};
	let named = paths.len();
	let mut similar: Vec<(bool, usize, String)> = (paths.into_iter())
		.map(|(path, ends_like)| (!ends_like, path.matches("::").count(), path))
		.collect();

	similar.sort();

	Suggestions {
		unique_suffix,
		similar: similar.into_iter().take(MAX_SIMILAR).map(|(_, _, path)| path).collect(),
		named,
	}
}
