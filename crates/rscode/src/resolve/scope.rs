//! Module scopes: the bindings of names in the three namespaces, and the other name tables.

use super::fxhash::FxHashMap;
use super::vis::ModuleTree;
use super::vis::Vis;
use crate::model::ItemId;
use crate::model::Workspace;
use crate::resolve::Binding;
use crate::resolve::Namespace;
use crate::resolve::Res;
use smol_str::SmolStr;

/// A name keeps at most this many bindings per namespace from imports; more are dropped (only malformed code gets
/// near it, e.g. imports feeding each other ever longer external paths).
const MAX_IMPORTED_BINDINGS: usize = 64;

/// How a binding came into a scope.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub(super) enum Origin {
	/// An item defined in the module, an `extern crate`, or a `#[macro_export]` macro at the crate root.
	Def,

	/// A named import (`use a::B;`, `use a::B as C;`, `use a::{self};`).
	Import,

	/// A glob import (`use a::*;`). Shadowed by definitions and named imports.
	Glob,

	/// A `macro_rules!` macro defined in the module. Reachable by its bare name in the module and (through
	/// [`Tables::textual`]) its descendants, but never re-exported by glob imports.
	Textual,
}

impl Origin {
	/// Definitions and named imports shadow glob imports.
	pub(super) fn is_explicit(self) -> bool {
		matches!(self, Self::Def | Self::Import)
	}
}

/// One binding of a name in a module scope.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct Entry {
	pub(super) res: Res,
	pub(super) import: Option<ItemId>,
	pub(super) origin: Origin,
	pub(super) vis: Vis,

	/// Whether the namespace is a guess: a path outside of the loaded crates that is imported by name is bound in
	/// every namespace, since which ones it is in is unknown. Such bindings are not shadowed by glob imports, and only
	/// shadow glob bindings of the same target.
	pub(super) guessed: bool,
}

impl Entry {
	pub(super) fn to_binding(&self) -> Binding {
		Binding {
			res: self.res.clone(),
			import: self.import,
			glob: self.origin == Origin::Glob,
		}
	}

	/// Definitions and named imports shadow glob imports, unless their namespace is a guess.
	pub(super) fn shadows_globs(&self) -> bool {
		self.origin.is_explicit() && !self.guessed
	}

	/// Whether the binding makes a glob binding unnecessary: it shadows glob imports, or it is a named import of the
	/// same target (with a guessed namespace).
	fn supersedes(&self, glob: &Self) -> bool {
		self.shadows_globs() || self.guessed && self.origin == Origin::Import && self.res == glob.res
	}

	/// Whether two entries are the same binding (then only the wider visibility is kept).
	fn is_same_binding(&self, other: &Self) -> bool {
		self.res == other.res && self.origin == other.origin && (self.origin == Origin::Glob || self.import == other.import)
	}
}

/// The bindings of one name in one namespace.
///
/// Invariant: glob bindings only exist while no other binding supersedes them.
#[derive(Debug, Default)]
pub(super) struct Slot {
	entries: Vec<Entry>,
}

impl Slot {
	pub(super) fn entries(&self) -> &[Entry] {
		&self.entries
	}

	/// Whether the slot holds bindings that shadow glob imports: then only named imports can still add to it.
	pub(super) fn shadows_globs(&self) -> bool {
		self.entries.iter().any(Entry::shadows_globs)
	}

	/// Adds a binding, applying shadowing. Returns whether the slot changed.
	///
	/// Changes are monotone (bindings are added, visibilities widened, and glob bindings removed at most once, when a
	/// binding superseding them comes), so repeated insertion reaches a fixpoint.
	fn insert(&mut self, tree: &ModuleTree, entry: Entry) -> bool {
		let mut changed = false;

		if entry.origin == Origin::Glob {
			if self.entries.iter().any(|existing| existing.supersedes(&entry)) {
				return false;
			}
		} else {
			let before = self.entries.len();

			self.entries.retain(|existing| existing.origin != Origin::Glob || !entry.supersedes(existing));
			changed = self.entries.len() != before;
		}

		if let Some(existing) = self.entries.iter_mut().find(|existing| existing.is_same_binding(&entry)) {
			if entry.vis.is_wider_than(tree, existing.vis) {
				existing.vis = entry.vis;
				existing.import = entry.import;

				return true;
			}

			return changed;
		}

		// definitions are finite; only imports could grow a slot without bound
		if matches!(entry.origin, Origin::Import | Origin::Glob) && self.entries.len() >= MAX_IMPORTED_BINDINGS {
			return changed;
		}

		self.entries.push(entry);
		true
	}
}

/// The names bound in a module.
#[derive(Debug, Default)]
pub(super) struct Scope {
	names: FxHashMap<SmolStr, [Slot; 3]>,
}

impl Scope {
	pub(super) fn slot(&self, name: &str, namespace: Namespace) -> Option<&Slot> {
		self.names.get(name).map(|slots| &slots[namespace.index()])
	}

	/// Adds a binding, applying shadowing. Returns whether the scope changed.
	pub(super) fn insert(&mut self, tree: &ModuleTree, name: &SmolStr, namespace: Namespace, entry: Entry) -> bool {
		match self.names.get_mut(name) {
			Some(slots) => slots[namespace.index()].insert(tree, entry),

			None => {
				let mut slots: [Slot; 3] = Default::default();

				slots[namespace.index()].insert(tree, entry);
				self.names.insert(name.clone(), slots);
				true
			}
		}
	}

	/// Every non-empty slot, in no particular order.
	pub(super) fn iter(&self) -> impl Iterator<Item = (&SmolStr, Namespace, &Slot)> {
		self.names.iter().flat_map(|(name, slots)| {
			Namespace::ALL
				.into_iter()
				.map(move |namespace| (name, namespace, &slots[namespace.index()]))
				.filter(|(_, _, slot)| !slot.entries.is_empty())
		})
	}
}

/// The name tables of all loaded crates.
#[derive(Debug, Default)]
pub(super) struct Tables {
	pub(super) tree: ModuleTree,

	/// The scope of every module (crate roots included).
	pub(super) scopes: FxHashMap<ItemId, Scope>,

	/// `macro_rules!` macros by the module whose subtree their textual scope covers (the defining module, or an
	/// ancestor when the defining module is `#[macro_use]`).
	pub(super) textual: FxHashMap<ItemId, FxHashMap<SmolStr, Vec<ItemId>>>,

	/// Crates nameable by a bare name (dependencies, root `extern crate`s, and the sysroot crates), per crate.
	pub(super) extern_preludes: Vec<FxHashMap<SmolStr, Vec<Res>>>,

	/// Macros imported with `#[macro_use] extern crate`, per crate.
	pub(super) macro_preludes: Vec<FxHashMap<SmolStr, Vec<Res>>>,

	/// Associated items reachable as `Owner::name`, by owner (types and traits). Filled once `impl`s are resolved.
	pub(super) assoc: FxHashMap<ItemId, Vec<ItemId>>,
}

impl Tables {
	/// Adds a binding to a module's scope, applying shadowing. Returns whether the scope changed.
	pub(super) fn insert(&mut self, module: ItemId, name: &SmolStr, namespace: Namespace, entry: Entry) -> bool {
		self.scopes.entry(module).or_default().insert(&self.tree, name, namespace, entry)
	}

	/// `macro_rules!` macros named `name` whose textual scope covers `module`: those of the nearest module (the
	/// module itself or an ancestor) that has any.
	pub(super) fn textual_macros(&self, ws: &Workspace, module: ItemId, name: &str) -> &[ItemId] {
		let mut current = Some(module);

		while let Some(module) = current {
			if let Some(macros) = self.textual.get(&module).and_then(|names| names.get(name)) {
				return macros;
			}

			current = super::vis::parent_module(ws, module);
		}

		&[]
	}
}
