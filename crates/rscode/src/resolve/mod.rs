//! Name resolution over loaded crates.
//!
//! Every module gets a scope of names in three namespaces (types, values, macros), populated by the items it
//! defines and by its `use` imports (resolved to a fixpoint, including globs). Items under any `cfg` are
//! included, so a name may have several bindings (one per `cfg` variant).
//!
//! Things outside of the loaded crates (`std`, dependencies that were not loaded) resolve to
//! [`Res::External`].
//!
//! Resolution follows rustc where it matters for finding items, with some approximations:
//! - Definitions and named imports shadow glob imports; bindings of the same name that are not shadowed are all
//!   kept (`cfg` variants, and glob ambiguities). Like in rustc, a glob binding is only used once no named import of
//!   the same name can shadow it anymore, and imports that wait on each other are given up on.
//! - `macro_rules!` macros are in scope (by bare name) in their module and its descendants, ignoring textual order;
//!   in the parent module too when their module is `#[macro_use]`. `#[macro_export]` macros are also bound at the
//!   crate root.
//! - Imports of external paths bind their name in every namespace, since what they name is unknown; these bindings
//!   neither shadow glob imports nor are shadowed by them.
//! - Enum variants shadow associated items of the same name (in the variants' namespaces).
//! - Generic parameters, local items, and `Self` are not known here (callers resolving code handle them).
//! - In edition 2015, `use` paths and `::` paths start at the crate root, where `extern crate` items (including the
//!   injected `std`, or `core` for `#![no_std]` crates) name crates; other paths also start from the extern prelude.

mod build;
mod canonical;
mod fxhash;
mod impls;
mod names;
mod refs;
mod removal;
mod scope;
mod selector;
mod text;
mod usable;
mod user_path;
mod vis;
mod walk;

#[cfg(test)]
mod test_model;

#[cfg(test)]
mod tests;

use crate::model::CrateId;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::PathRef;
use crate::model::Workspace;
use crate::path::CanonicalPath;
use crate::path::ItemPath;
use crate::path::Qualifier;
use build::ImportIndex;
use fxhash::FxHashSet;
use impls::ImplIndex;
use scope::Tables;
use serde::Serialize;
use smol_str::SmolStr;
use usable::UsableCache;
use walk::Walker;
use walk::Want;

pub use refs::Reference;
pub use refs::ReferenceKind;
pub use refs::ReferenceOptions;
pub use refs::References;
pub(crate) use removal::DeadName;
pub(crate) use removal::DeadNames;
pub(crate) use removal::LostBinding;

/// One binding of a name in a module scope.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
pub struct Binding {
	/// What the name refers to (imports are followed to their final target).
	pub res: Res,

	/// The [`ItemKind::Import`] that introduced the binding, if not a definition.
	/// For crates bound by `extern crate`, the [`ItemKind::ExternCrate`] item.
	pub import: Option<ItemId>,

	/// Whether the binding comes from a glob import.
	pub glob: bool,
}

/// A namespace of names.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Namespace {
	/// Modules, types, traits, crates.
	Type,

	/// Functions, constants, statics, tuple/unit struct and variant constructors.
	Value,

	/// Macros.
	Macro,
}

impl Namespace {
	/// Every namespace.
	pub const ALL: [Self; 3] = [Self::Type, Self::Value, Self::Macro];

	fn index(self) -> usize {
		self as usize
	}
}

/// How a path is written, which decides where its first segment is looked up.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathKind {
	/// A path in code (types, expressions, patterns, macro invocations). Associated items are reachable through
	/// types and traits (`Type::new`, `Trait::method`).
	#[default]
	Code,

	/// A path in a `use` item. In edition 2015, it is relative to the crate root.
	Use,
}

/// What a name resolves to.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "target")]
pub enum Res {
	/// A loaded item (a crate root module for crates).
	Item(ItemId),

	/// Something outside of the loaded crates, as a path (`std::fmt::Display`, `serde::Serialize`).
	External(SmolStr),

	/// A primitive type (`u8`, `str`, ...) or built-in attribute/macro.
	Builtin(SmolStr),
}

/// Name resolution for a [`Workspace`]. Construction resolves every import; queries are cheap.
#[derive(Debug)]
pub struct Resolver<'ws> {
	ws: &'ws Workspace,
	tables: Tables,
	impls: ImplIndex,
	imports: ImportIndex,

	/// Usable paths, computed on demand per viewpoint.
	usable: UsableCache,
}

impl<'ws> Resolver<'ws> {
	/// Builds the scopes of every module of every loaded crate, resolving all imports, and indexes `impl` blocks.
	pub fn new(ws: &'ws Workspace) -> Self {
		Self::build(ws, &FxHashSet::default())
	}

	fn build(ws: &'ws Workspace, excluded: &FxHashSet<ItemId>) -> Self {
		let (mut tables, imports) = build::build(ws, excluded);
		let impls = impls::build(ws, &tables);

		tables.assoc = impls::assoc_index(ws, &impls);

		Self {
			ws,
			tables,
			impls,
			imports,
			usable: UsableCache::default(),
		}
	}

	/// A resolver of the workspace in which `imports` bind nothing, as if they were removed: comparing it with one of the
	/// whole workspace (see [`Resolver::lost_bindings`]) tells what the removal breaks.
	pub(crate) fn without_imports(ws: &'ws Workspace, imports: &[ItemId]) -> Self {
		Self::build(ws, &imports.iter().copied().collect())
	}

	/// Associated items reachable as `Type::name` (from inherent and trait `impl`s) or `Trait::name`.
	///
	/// Items of inherent `impl`s come first, each group in source order. Enum variants are not included. For a type
	/// alias, only the items of `impl`s written for the alias itself are known (what it aliases is not resolved).
	pub fn associated_items(&self, item: ItemId) -> Vec<ItemId> {
		self.tables.assoc.get(&item).cloned().unwrap_or_default()
	}

	/// The bindings of `name` in a module's scope (for a non-module item: the scope of its module), including
	/// `macro_rules!` macros in textual scope.
	pub fn bindings(&self, module: ItemId, name: &str, namespace: Namespace) -> Vec<Binding> {
		let module = self.ws.module_of(module);

		let mut bindings: Vec<Binding> = (self.tables.scopes.get(&module).and_then(|scope| scope.slot(name, namespace)))
			.map(|slot| slot.entries().iter().map(|entry| entry.to_binding()).collect())
			.unwrap_or_default();

		if namespace == Namespace::Macro {
			for &macro_item in self.tables.textual_macros(self.ws, module, name) {
				let binding = Binding {
					res: Res::Item(macro_item),
					import: None,
					glob: false,
				};

				if !bindings.contains(&binding) {
					bindings.push(binding);
				}
			}
		}

		bindings
	}

	/// The definition path of an item.
	///
	/// Items of `impl` blocks are owned by the first loaded type the self type resolves to; `#[macro_export]` macros
	/// live at the crate root; imports are named by the name they bind (`*` for globs, `_` for underscore imports).
	pub fn canonical_path(&self, item: ItemId) -> CanonicalPath {
		self.compute_canonical_path(item)
	}

	/// The crate root module named `name` from inside of `from` (extern prelude, including renames), if loaded.
	pub fn crate_by_name(&self, from: CrateId, name: &str) -> Option<CrateId> {
		let prelude = self.tables.extern_preludes.get(from.index())?;

		prelude.get(name)?.iter().find_map(|res| match res {
			Res::Item(root) => Some(root.krate()),
			_ => None,
		})
	}

	/// For a plain path `Type::name` that names an associated item (or variant) while `Type` has a field of that name, a
	/// hint for messages about a replacement that does not fit: the field is `Type.name`.
	pub fn field_hint(&self, path: &ItemPath) -> Option<String> {
		if path.field.is_some() || path.import || path.qualifier.is_some() {
			return None;
		}

		let (name, owner) = path.segments.split_last().filter(|(_, owner)| !owner.is_empty())?;
		let field = ItemPath {
			segments: owner.to_vec(),
			field: Some(name.clone()),
			..path.clone()
		};

		(!self.compute_item_path(&field).is_empty() && !self.compute_item_path(path).is_empty())
			.then(|| format!("`{path}` names an associated item or variant; the field is `{field}`"))
	}

	/// References to the targets across all loaded crates.
	pub fn find_references(&self, targets: &[ItemId], options: &ReferenceOptions) -> References {
		refs::find_references(self, targets, options)
	}

	/// References to the targets, references through `lost` bindings (see [`Resolver::lost_bindings`]), and unresolved
	/// references to `dead` names (see [`Resolver::dead_names`]), whose target is the import that bound them.
	pub(crate) fn find_references_through(
		&self,
		targets: &[ItemId],
		lost: &[LostBinding],
		dead: &[DeadName],
		options: &ReferenceOptions,
	) -> References {
		refs::find_references_through(self, targets, lost, dead, options)
	}

	/// The loaded item(s) an `impl` block implements (its resolved self type), one per `cfg` variant.
	///
	/// The self type is resolved after stripping references, pointers, slices, and arrays (`impl Tr for &Foo` is for
	/// `Foo`). An `impl` for a type alias is for the alias item. `impl<T> Tr for T` is for nothing.
	pub fn impl_self_types(&self, impl_block: ItemId) -> Vec<ItemId> {
		self.impls.self_types(impl_block).to_vec()
	}

	/// The loaded trait(s) an `impl` block implements.
	pub fn impl_traits(&self, impl_block: ItemId) -> Vec<ItemId> {
		self.impls.traits(impl_block).to_vec()
	}

	/// `impl` blocks whose self type is `item` (a struct, enum, union, trait, type alias, or foreign type),
	/// or, for a trait, `impl` blocks implementing it.
	pub fn impls_of(&self, item: ItemId) -> Vec<ItemId> {
		let mut impls: Vec<ItemId> = self.impls.by_self_type.get(&item).into_iter().flatten().copied().collect();

		if self.ws.item(item).kind == ItemKind::Trait {
			impls.extend(self.impls.by_trait.get(&item).into_iter().flatten());
		}

		impls.sort();
		impls.dedup();
		impls
	}

	/// For a path that names nothing, a hint about imports, for messages: what a `use` path names, or the `use` path
	/// of the imports that a plain path names (when they import items that are not loaded).
	pub fn import_hint(&self, path: &ItemPath) -> Option<String> {
		if path.import {
			let name = path.name().map_or("Name", SmolStr::as_str);

			return Some(format!(
				"a `use` path names the imports of the module its other segments name (`use crate::m::{name}` names \
				 `use a::{name};` in `m`), not an import written with that path; the pattern `use *{name}*` finds the \
				 imports of `{name}`"
			));
		}

		let imports = ItemPath {
			import: true,
			..path.clone()
		};

		(path.qualifier.is_none() && !self.compute_item_path(&imports).is_empty())
			.then(|| format!("`{path}` names imports of items that are not loaded: `{imports}` names the imports themselves"))
	}

	/// What an import refers to (for a glob import: the modules and enums it imports from), with imports followed to
	/// their final targets. Empty for unresolved imports and non-imports.
	pub fn import_targets(&self, import: ItemId) -> Vec<Res> {
		let mut targets: Vec<Res> = (self.imports.targets.get(&import).into_iter().flatten())
			.map(|(_, res)| res.clone())
			.collect();

		targets.sort();
		targets.dedup();
		targets
	}

	/// Imports (in all loaded crates) that refer to `target`: named imports of it, and glob imports of it (a module
	/// or enum).
	pub fn imports_of(&self, target: ItemId) -> Vec<ItemId> {
		self.imports.by_target.get(&target).cloned().unwrap_or_default()
	}

	/// Whether `item` is visible from `module` according to its declared visibility.
	///
	/// This only considers the item's own visibility, not whether the modules on the way to it are visible.
	pub fn is_visible_from(&self, item: ItemId, module: ItemId) -> bool {
		vis::declared_vis(self.ws, item).is_visible_from(&self.tables.tree, self.ws.module_of(module))
	}

	/// Every name bound in a module's scope, sorted.
	pub fn names(&self, module: ItemId, namespace: Namespace) -> Vec<SmolStr> {
		let module = self.ws.module_of(module);

		let mut names: Vec<SmolStr> = (self.tables.scopes.get(&module).into_iter())
			.flat_map(|scope| scope.iter())
			.filter(|&(_, slot_namespace, _)| slot_namespace == namespace)
			.map(|(name, _, _)| name.clone())
			.collect();

		if namespace == Namespace::Macro {
			let mut current = Some(module);

			while let Some(module) = current {
				names.extend(self.tables.textual.get(&module).into_iter().flat_map(|macros| macros.keys().cloned()));
				current = vis::parent_module(self.ws, module);
			}
		}

		names.sort();
		names.dedup();
		names
	}

	/// Items named by a user-given path, in selected crates (and, for `::name` or crate-name-first paths,
	/// in the named crate). Imports are followed to their definitions, except by `use` paths
	/// ([`ItemPath::import`]), which name the imports themselves; every `cfg` variant is returned.
	///
	/// Visibility is not enforced. Segments after a type or trait name its associated items (`Type::new`), and
	/// segments after an enum its variants (or its associated items, in namespaces without a variant of that name).
	/// In `<Type as Trait>::name`, a type or trait path that does not name a loaded item matches the `impl`s whose type
	/// or trait path ends with its segments (`<Circle as Shape>` for `impl shapes::Shape for Circle`). Results are sorted
	/// and free of duplicates.
	pub fn resolve_item_path(&self, path: &ItemPath) -> Vec<ItemId> {
		self.compute_item_path(path)
	}

	/// Resolves a path as if written inside of `module` (a module item).
	///
	/// Single-segment names that are bound nowhere fall back to the standard library prelude ([`Res::External`]) and to
	/// primitive types and built-in macros ([`Res::Builtin`]). Paths starting with `Self` resolve to nothing.
	pub fn resolve_path(&self, module: ItemId, path: &PathRef, namespace: Namespace) -> Vec<Res> {
		self.resolve_prefixes(module, path, Some(namespace), PathKind::Code)
			.pop()
			.unwrap_or_default()
	}

	/// Resolves every prefix of a path written inside of `module`: element `i` is what `path.segments[..=i]` names,
	/// looked up in the type namespace, except for the last segment, which is looked up in `namespace` (every
	/// namespace for `None`, like a `use` does). The result has one element per segment.
	pub fn resolve_prefixes(&self, module: ItemId, path: &PathRef, namespace: Option<Namespace>, kind: PathKind) -> Vec<Vec<Res>> {
		let want = namespace.map_or(Want::All, Want::One);
		let mut walker = Walker::new(self.ws, &self.tables, self.ws.module_of(module), kind);

		walker
			.prefixes(path, want)
			.into_iter()
			.map(|found| {
				let mut res: Vec<Res> = found.into_iter().map(|found| found.res).collect();

				res.sort();
				res.dedup();
				res
			})
			.collect()
	}

	/// For a path with a selector that names nothing, a hint for messages: the paths of the `impl` blocks (or macro
	/// invocations) that the path names without its selector (each with the selector that names it, when it needs one).
	pub fn selector_hint(&self, path: &ItemPath) -> Option<String> {
		let header = match &path.qualifier {
			Some(qualifier) if qualifier.selector.is_some() => ItemPath {
				qualifier: Some(Qualifier {
					selector: None,
					..qualifier.clone()
				}),
				segments: Vec::new(),
				..path.clone()
			},

			None if path.selector.is_some() => ItemPath {
				selector: None,
				..path.clone()
			},

			_ => return None,
		};
		let mut blocks: Vec<String> = (self.compute_item_path(&header).into_iter())
			.map(|block| format!("`{}`", self.compute_canonical_path(block)))
			.collect();

		blocks.dedup();

		(!blocks.is_empty()).then(|| format!("`{header}` names {}", blocks.join(", ")))
	}

	/// For an item of a trait: the corresponding items in every `impl` of the trait.
	/// For an item of a trait `impl`: the trait's item.
	pub fn trait_counterparts(&self, item: ItemId) -> Vec<ItemId> {
		let data = self.ws.item(item);

		let (Some(name), Some(parent)) = (&data.name, self.ws.parent(item)) else {
			return Vec::new();
		};

		let containers: Vec<ItemId> = match self.ws.item(parent).kind {
			ItemKind::Trait => self.impls.by_trait.get(&parent).cloned().unwrap_or_default(),
			ItemKind::Impl => self.impls.traits(parent).to_vec(),
			_ => return Vec::new(),
		};

		let mut counterparts: Vec<ItemId> = (containers.into_iter())
			.flat_map(|container| impls::named_assoc_items(self.ws, container))
			.filter(|&candidate| {
				let candidate_data = self.ws.item(candidate);

				candidate != item && candidate_data.kind == data.kind && candidate_data.name.as_ref() == Some(name)
			})
			.collect();

		counterparts.sort();
		counterparts.dedup();
		counterparts
	}

	/// Imports that resolved to nothing (typically paths into items produced by macros).
	pub fn unresolved_imports(&self) -> &[ItemId] {
		&self.imports.unresolved
	}

	/// Paths through which `target` can be named from `viewpoint`, shortest first
	/// (`crate::...` inside of the target's crate, `::crate_name::...` from another crate).
	///
	/// From a module, a name bound directly in its scope is usable bare, and paths through dependencies start with
	/// their extern prelude name (in every edition: 2015 paths outside of `use` items start from the extern prelude
	/// too). Paths go through every re-export of a module; at most 16 (the shortest) are returned. Paths of every item
	/// are computed together, once per viewpoint.
	pub fn usable_paths(&self, target: ItemId, viewpoint: Viewpoint) -> Vec<String> {
		self.compute_usable_paths(target, viewpoint)
	}

	/// The module whose descendants may see `item` according to its declared visibility (`None` when public).
	pub fn visibility_scope(&self, item: ItemId) -> Option<ItemId> {
		vis::declared_vis(self.ws, item).scope()
	}

	/// The workspace names are resolved in.
	pub fn workspace(&self) -> &'ws Workspace {
		self.ws
	}
}

/// A point of view for computing usable paths.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum Viewpoint {
	/// Inside of a module (use the crate root module for `--from crate`).
	Module(ItemId),

	/// From another crate (`--from ::`): only public items through public modules and re-exports.
	Foreign,
}
