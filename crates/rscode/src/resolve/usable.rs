//! Paths through which items can be named from a viewpoint.
//!
//! A breadth-first search over the modules reachable from the roots of a viewpoint finds the shortest paths to every
//! module (several per module, since modules can be re-exported); the paths of the items bound in the modules' scopes
//! follow, then those of variants and associated items. The result is cached per viewpoint, so that asking for many
//! items is cheap.

use super::Resolver;
use super::Viewpoint;
use super::fxhash::FxHashMap;
use super::fxhash::FxHashSet;
use super::names;
use super::names::ident_text;
use super::scope::Entry;
use super::scope::Origin;
use super::vis::Vis;
use super::vis::declared_vis;
use super::vis::parent_module;
use crate::model::CrateId;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::resolve::Res;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::PoisonError;

/// At most this many paths are kept per item (and per module, for the paths through it).
const MAX_PATHS: usize = 16;

/// Paths longer than this many segments are not searched.
const MAX_DEPTH: usize = 16;

/// Usable paths of every item reachable from a viewpoint.
pub(super) type UsableIndex = FxHashMap<ItemId, Vec<String>>;

/// Usable path indices by viewpoint.
pub(super) type UsableCache = std::sync::Mutex<FxHashMap<Viewpoint, Arc<UsableIndex>>>;

/// A path, ordered shortest first: by segments, then characters, then text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Path {
	segments: usize,
	chars: usize,
	text: String,

	/// The path only names items of this crate (it starts at the root of an unselected crate, from another crate).
	only: Option<CrateId>,
}

impl Path {
	fn root(text: String, only: Option<CrateId>) -> Self {
		Self {
			segments: 1,
			chars: text.len(),
			text,
			only,
		}
	}

	fn join(&self, name: &str) -> Self {
		let text = format!("{}::{name}", self.text);

		Self {
			segments: self.segments + 1,
			chars: text.len(),
			text,
			only: self.only,
		}
	}

	fn names(&self, item: ItemId) -> bool {
		self.only.is_none_or(|krate| item.krate() == krate)
	}
}

/// The best paths found so far per item.
#[derive(Default)]
struct Paths {
	by_item: FxHashMap<ItemId, BTreeSet<Path>>,
}

impl Paths {
	/// Whether `prefix::name` could be one of the kept paths of `item` (then `prefix` followed by any longer name, or
	/// any longer prefix, might not be).
	fn admits(&self, item: ItemId, prefix: &Path, name: &str) -> bool {
		let length = (prefix.segments + 1, prefix.chars + 2 + name.len());

		self.by_item.get(&item).is_none_or(|paths| paths.len() < MAX_PATHS || paths.last().is_some_and(|worst| length <= (worst.segments, worst.chars)))
	}

	/// Records `prefix::name` as a path of `item` for each of `prefixes` (sorted), as long as they can be kept.
	fn record_all(&mut self, item: ItemId, prefixes: &[Path], name: &str) {
		for prefix in prefixes {
			if !self.admits(item, prefix, name) {
				break;
			}

			if prefix.names(item) {
				self.insert(item, prefix.join(name));
			}
		}
	}

	fn insert(&mut self, item: ItemId, path: Path) {
		let paths = self.by_item.entry(item).or_default();

		paths.insert(path);

		if paths.len() > MAX_PATHS {
			paths.pop_last();
		}
	}

	fn finish(self) -> UsableIndex {
		(self.by_item.into_iter())
			.map(|(item, paths)| {
				let mut texts: Vec<String> = Vec::with_capacity(paths.len());

				for path in paths {
					if !texts.contains(&path.text) {
						texts.push(path.text);
					}
				}

				(item, texts)
			})
			.collect()
	}
}

/// Where searches start: a root module, and the path naming it.
#[derive(Debug)]
struct Root {
	module: ItemId,
	path: Path,
}

/// The shortest paths to modules (at most [`MAX_PATHS`] each), by module and crate restriction.
type ModulePaths = FxHashMap<(ItemId, Option<CrateId>), Vec<Path>>;

impl Resolver<'_> {
	pub(super) fn compute_usable_paths(&self, target: ItemId, viewpoint: Viewpoint) -> Vec<String> {
		let viewpoint = match viewpoint {
			Viewpoint::Module(module) => Viewpoint::Module(self.ws.module_of(module)),
			Viewpoint::Foreign => Viewpoint::Foreign,
		};

		let cached = self.usable.lock().unwrap_or_else(PoisonError::into_inner).get(&viewpoint).cloned();

		let index = match cached {
			Some(index) => index,

			None => {
				// computed without holding the lock; a concurrent computation of the same index is harmless
				let index = Arc::new(self.usable_index(viewpoint));

				self.usable.lock().unwrap_or_else(PoisonError::into_inner).entry(viewpoint).or_insert(index).clone()
			}
		};

		index.get(&target).cloned().unwrap_or_default()
	}

	fn usable_index(&self, viewpoint: Viewpoint) -> UsableIndex {
		let mut paths = Paths::default();

		if let Viewpoint::Module(module) = viewpoint {
			self.local_paths(module, &mut paths);
		}

		let roots = self.roots(viewpoint);

		for root in &roots {
			if root.path.names(root.module) {
				paths.insert(root.module, root.path.clone());
			}
		}

		let modules = self.module_paths(roots, viewpoint);

		for (&(module, _), prefixes) in &modules {
			self.record_bindings(module, prefixes, viewpoint, &mut paths);
		}

		self.record_members(viewpoint, &mut paths);
		paths.finish()
	}

	fn is_visible(&self, vis: Vis, viewpoint: Viewpoint) -> bool {
		match viewpoint {
			Viewpoint::Module(module) => vis.is_visible_from(&self.tables.tree, module),
			Viewpoint::Foreign => vis == Vis::Public,
		}
	}

	/// Where searches start: `crate` and the extern prelude inside of a crate; every crate root as `::name` from
	/// another crate (the roots of unselected crates only lead to their own items).
	fn roots(&self, viewpoint: Viewpoint) -> Vec<Root> {
		let mut roots = Vec::new();

		match viewpoint {
			Viewpoint::Module(module) => {
				let krate = self.ws.krate(module.krate());

				roots.push(Root {
					module: krate.root_module(),
					path: Path::root("crate".to_owned(), None),
				});

				// in every edition: 2015 paths outside of `use` items start from the extern prelude too
				for (name, resolutions) in self.tables.extern_preludes.get(krate.id().index()).into_iter().flatten() {
					for res in resolutions {
						if let Res::Item(root) = res {
							roots.push(Root {
								module: *root,
								path: Path::root(ident_text(name).into_owned(), None),
							});
						}
					}
				}
			}

			Viewpoint::Foreign => {
				for krate in self.ws.crates() {
					roots.push(Root {
						module: krate.root_module(),
						path: Path::root(format!("::{}", ident_text(krate.name())), (!krate.is_selected()).then_some(krate.id())),
					});
				}
			}
		}

		roots
	}

	/// The shortest paths to every module reachable from the roots, through bindings visible from the viewpoint.
	fn module_paths(&self, roots: Vec<Root>, viewpoint: Viewpoint) -> ModulePaths {
		let mut modules = ModulePaths::default();
		let mut children: FxHashMap<ItemId, Vec<(String, ItemId)>> = FxHashMap::default();
		let mut level: Vec<(Path, ItemId)> = roots.into_iter().map(|root| (root.path, root.module)).collect();

		// breadth-first: every path of a level is shorter than those of the next one
		for depth in 1..=MAX_DEPTH {
			level.sort();

			let mut next = Vec::new();

			for (path, module) in level {
				let kept = modules.entry((module, path.only)).or_default();

				if kept.len() >= MAX_PATHS || kept.contains(&path) {
					continue;
				}

				kept.push(path.clone());

				if depth == MAX_DEPTH {
					continue;
				}

				for (name, child) in children.entry(module).or_insert_with(|| self.child_modules(module, viewpoint)) {
					// modules with enough (shorter) paths already
					if modules.get(&(*child, path.only)).is_none_or(|kept| kept.len() < MAX_PATHS) {
						next.push((path.join(name), *child));
					}
				}
			}

			if next.is_empty() {
				break;
			}

			level = next;
		}

		modules
	}

	/// The modules a module's scope names, through bindings visible from the viewpoint, with the names naming them.
	fn child_modules(&self, module: ItemId, viewpoint: Viewpoint) -> Vec<(String, ItemId)> {
		(self.visible_bindings(module, viewpoint))
			.filter_map(|(name, entry)| match entry.res {
				Res::Item(item) if self.ws.item(item).kind == ItemKind::Module => Some((ident_text(name).into_owned(), item)),
				_ => None,
			})
			.collect()
	}

	/// The bindings of a module's scope that paths can go through from the viewpoint (textually scoped macros cannot be
	/// named by paths).
	fn visible_bindings(&self, module: ItemId, viewpoint: Viewpoint) -> impl Iterator<Item = (&str, &Entry)> + '_ {
		(self.tables.scopes.get(&module).into_iter().flat_map(|scope| scope.iter()))
			.flat_map(|(name, _, slot)| slot.entries().iter().map(move |entry| (name.as_str(), entry)))
			.filter(move |(_, entry)| entry.origin != Origin::Textual && self.is_visible(entry.vis, viewpoint))
	}

	/// Records the paths of what a module's bindings name, through each of the module's paths.
	fn record_bindings(&self, module: ItemId, prefixes: &[Path], viewpoint: Viewpoint, paths: &mut Paths) {
		for (name, entry) in self.visible_bindings(module, viewpoint) {
			let name = ident_text(name);

			for item in named_items(entry) {
				paths.record_all(item, prefixes, &name);
			}
		}
	}

	/// Bare names in the viewpoint module's own scope, including macros in textual scope.
	fn local_paths(&self, module: ItemId, paths: &mut Paths) {
		for (name, _, slot) in self.tables.scopes.get(&module).into_iter().flat_map(|scope| scope.iter()) {
			for entry in slot.entries() {
				for item in named_items(entry) {
					paths.insert(item, Path::root(ident_text(name).into_owned(), None));
				}
			}
		}

		// the nearest textual scope of each name shadows those further out
		let mut seen = FxHashSet::default();
		let mut current = Some(module);

		while let Some(module) = current {
			for (name, macros) in self.tables.textual.get(&module).into_iter().flatten() {
				if seen.insert(name.clone()) {
					for &macro_item in macros {
						paths.insert(macro_item, Path::root(ident_text(name).into_owned(), None));
					}
				}
			}

			current = parent_module(self.ws, module);
		}
	}

	/// Records the paths of variants and associated items visible from the viewpoint, through the paths of their
	/// enums, types, and traits.
	fn record_members(&self, viewpoint: Viewpoint, paths: &mut Paths) {
		let owners: Vec<(ItemId, Vec<Path>)> = (paths.by_item.iter())
			.filter(|&(&item, _)| {
				matches!(
					self.ws.item(item).kind,
					ItemKind::Enum | ItemKind::Struct | ItemKind::Union | ItemKind::TypeAlias | ItemKind::ForeignType | ItemKind::Trait
				)
			})
			.map(|(&item, owner_paths)| (item, owner_paths.iter().cloned().collect()))
			.collect();

		for (owner, owner_paths) in owners {
			for member in self.members(owner) {
				let Some(name) = self.ws.item(member).name.as_deref().map(ident_text) else {
					continue;
				};

				if !self.is_visible(declared_vis(self.ws, member), viewpoint) {
					continue;
				}

				paths.record_all(member, &owner_paths, &name);
			}
		}
	}

	/// The variants of an enum and the associated items reachable as `Owner::name` (not those a variant of the same
	/// name shadows).
	fn members(&self, owner: ItemId) -> Vec<ItemId> {
		let variants: Vec<ItemId> = self.ws.children(owner).filter(|&child| self.ws.item(child).kind == ItemKind::Variant).collect();

		let shadowed = |item: ItemId| {
			let data = self.ws.item(item);

			variants.iter().any(|&variant| {
				let variant = self.ws.item(variant);

				variant.name == data.name && names::namespaces(variant).iter().any(|namespace| names::namespaces(data).contains(namespace))
			})
		};

		let assoc = self.tables.assoc.get(&owner).into_iter().flatten().copied().filter(|&item| !shadowed(item));

		variants.iter().copied().chain(assoc).collect()
	}
}

/// The items a binding gives a path to: what it names, and, for a named import or an `extern crate`, the import
/// itself (named by the name it binds).
fn named_items(entry: &Entry) -> impl Iterator<Item = ItemId> {
	let import = entry.import.filter(|_| entry.origin != Origin::Glob);

	let item = match entry.res {
		Res::Item(item) => Some(item),
		_ => None,
	};

	import.into_iter().chain(item)
}
