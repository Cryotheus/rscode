//! Resolving paths given by users (`crate::a::Foo`, `::dep::Bar`, `Foo::new`, `<Foo as Display>::fmt`).

use super::Namespace;
use super::PathKind;
use super::Resolver;
use super::vis::Vis;
use super::vis::declared_vis;
use super::vis::home_module;
use super::walk::Found;
use super::walk::Walker;
use super::walk::Want;
use crate::model::CrateId;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::PathRef;
use crate::path::Anchor;
use crate::path::ItemPath;
use crate::path::Qualifier;
use crate::path::last_segment_arguments;
use crate::resolve::Res;
use smol_str::SmolStr;

/// See [`Resolver::private_imports_of`]. Each module the path's other segments name (in every crate, every `cfg`
/// variant) binds the name on its own.
#[derive(Debug, Default)]
pub(crate) struct PrivateImports {
	/// The private imports of the modules that bind the name to some of the items only through them.
	pub(crate) imports: Vec<ItemId>,

	/// The items that private imports bind the name to in modules that also bind it otherwise (they define it under
	/// other `cfg`s or in another namespace, or import it by a glob import or a re-export), and that nothing else binds
	/// it to: the path names what those modules bind otherwise, not these.
	pub(crate) shadowed: Vec<ItemId>,
}

impl Resolver<'_> {
	pub(super) fn compute_item_path(&self, path: &ItemPath) -> Vec<ItemId> {
		let mut items = match &path.qualifier {
			_ if path.import => self.resolve_import(path),
			Some(qualifier) => self.resolve_qualified(qualifier, &path.segments),
			None => self.resolve_unqualified(path),
		};

		items.sort();
		items.dedup();
		items
	}

	/// Crates named `name`: loaded crates of that name, and dependencies of selected crates known by that name.
	pub(super) fn crates_named(&self, name: &str) -> Vec<CrateId> {
		let mut crates: Vec<CrateId> = self
			.ws
			.crates()
			.iter()
			.filter(|krate| krate.name() == name)
			.map(|krate| krate.id())
			.collect();

		for krate in self.ws.selected_crates() {
			crates.extend(
				krate
					.dependencies()
					.iter()
					.filter(|dependency| dependency.name == name)
					.filter_map(|dependency| dependency.krate),
			);
		}

		crates.sort();
		crates.dedup();
		crates
	}

	/// Whether the names of a path as written, or of anything it resolved to, end with the segments of a user path
	/// without an anchor (or starting with `::`).
	fn ends_with(&self, written: &PathRef, resolved: &[Res], path: &ItemPath) -> bool {
		if path.qualifier.is_some() || path.segments.is_empty() || matches!(path.anchor, Anchor::Crate | Anchor::SelfModule | Anchor::Super(_)) {
			return false;
		}

		let wanted: Vec<&str> = path.segments.iter().map(SmolStr::as_str).collect();
		let written: Vec<&str> = written.names().collect();

		ends_with(&written, &wanted)
			|| resolved.iter().any(|res| match res {
				Res::External(external) | Res::Builtin(external) => ends_with(&external.split("::").collect::<Vec<_>>(), &wanted),
				Res::Item(item) => ends_with(&self.flat_path(*item).iter().map(SmolStr::as_str).collect::<Vec<_>>(), &wanted),
			})
	}

	/// Whether a trait `impl` implements one of `traits` (the loaded traits the user's trait path names), or, when
	/// either trait is not a loaded one, whether the `impl`'s trait ends with the segments of `trait_path`.
	fn implements(&self, impl_block: ItemId, traits: &[ItemId], trait_path: &ItemPath) -> bool {
		let Some(written) = self.ws.item(impl_block).impl_info().and_then(|info| info.trait_path.as_ref()) else {
			return false;
		};

		let loaded = self.impls.traits(impl_block);

		if loaded.iter().any(|trait_item| traits.contains(trait_item)) {
			return true;
		}

		if !loaded.is_empty() && !traits.is_empty() {
			return false;
		}

		let resolved = self.impls.trait_res.get(&impl_block).map_or(&[][..], Vec::as_slice);

		self.ends_with(written, resolved, trait_path)
	}

	/// Whether an item is a named import no more visible than its module.
	fn is_private_import(&self, import: ItemId) -> bool {
		self.ws.item(import).kind == ItemKind::Import && declared_vis(self.ws, import) == Vis::Module(home_module(self.ws, import))
	}

	/// How the last segment of a plain path names some of `items` (what the path resolves to) in the module(s) its other
	/// segments name, through named imports no more visible than their module (`use a::B;`, `pub(crate) use` in a crate
	/// root, but not re-exports): the path names both what they import and, as `use m::name`, the imports themselves.
	pub(crate) fn private_imports_of(&self, path: &ItemPath, items: &[ItemId]) -> PrivateImports {
		let mut found = PrivateImports::default();

		if path.import || path.qualifier.is_some() {
			return found;
		}

		let Some((name, modules)) = self.split_name(path) else {
			return found;
		};

		// each module (every crate's, every `cfg` variant) binds the name on its own
		let mut otherwise = Vec::new();

		for module in modules {
			let (mut imports, mut imported, mut own) = (Vec::new(), Vec::new(), Vec::new());
			let bindings = Namespace::ALL.into_iter().flat_map(|namespace| self.bindings(module, name, namespace));

			for binding in bindings {
				let Res::Item(item) = binding.res else {
					continue;
				};

				if !items.contains(&item) {
					continue;
				}

				match binding.import.filter(|&import| !binding.glob && self.is_private_import(import)) {
					Some(import) => {
						imports.push(import);
						imported.push(item);
					}

					None => own.push(item),
				}
			}

			match (imports.is_empty(), own.is_empty()) {
				(true, _) => {}
				(false, true) => found.imports.append(&mut imports),
				(false, false) => found.shadowed.append(&mut imported),
			}

			otherwise.append(&mut own);
		}

		found.imports.sort();
		found.imports.dedup();
		found.shadowed.retain(|item| !otherwise.contains(item));
		found.shadowed.sort();
		found.shadowed.dedup();
		found
	}

	/// `use m::name`: the imports of the module(s) `m` names (every `cfg` variant) that bind `name` (`*`: glob imports,
	/// `_`: underscore imports), found in the item tree, so that imports that resolve to nothing are named too.
	fn resolve_import(&self, path: &ItemPath) -> Vec<ItemId> {
		let Some((name, modules)) = self.split_name(path) else {
			return Vec::new();
		};

		(modules.into_iter())
			.flat_map(|module| self.ws.children(module))
			.filter(|&child| self.ws.item(child).kind == ItemKind::Use)
			.flat_map(|use_item| self.ws.children(use_item))
			.filter(|&import| self.ws.item(import).import_info().is_some_and(|info| info.path_name() == *name))
			.collect()
	}

	/// `<Type as Trait>::name`, `<Type>::name`, or the `impl` blocks themselves when there are no segments.
	///
	/// When the type or the trait is not a loaded item (as a presumed absolute path: `<Circle as Shape>` with a trait
	/// `crate::shapes::Shape`, or `<u8 as Display>`), `impl` blocks whose type or trait ends with its segments match,
	/// as written or as resolved. Generic arguments of the type or trait keep the `impl` blocks with the same ones, as
	/// written (`<Wrapper as From<u8>>`).
	fn resolve_qualified(&self, qualifier: &Qualifier, segments: &[SmolStr]) -> Vec<ItemId> {
		let types = self.compute_item_path(&qualifier.self_ty);

		let mut impls: Vec<ItemId> = if types.is_empty() {
			(self.impls.all.iter().copied())
				.filter(|&impl_block| self.self_type_ends_with(impl_block, &qualifier.self_ty))
				.collect()
		} else {
			types.iter().flat_map(|ty| self.impls.by_self_type.get(ty)).flatten().copied().collect()
		};

		match &qualifier.trait_path {
			None => impls.retain(|&impl_block| self.ws.item(impl_block).impl_info().is_some_and(|info| info.trait_path.is_none())),

			Some(trait_path) => {
				let mut traits = self.compute_item_path(trait_path);

				traits.retain(|&item| self.ws.item(item).kind == ItemKind::Trait);

				impls.retain(|&impl_block| self.implements(impl_block, &traits, trait_path));
			}
		}

		// generic arguments select `impl` blocks by their header, as written
		let arguments_match = |wanted: Option<&String>, written: Option<&str>| {
			wanted.is_none_or(|wanted| written.and_then(last_segment_arguments).as_ref() == Some(wanted))
		};

		impls.retain(|&impl_block| {
			let Some(info) = self.ws.item(impl_block).impl_info() else {
				return false;
			};

			arguments_match(qualifier.self_ty.arguments.as_ref(), Some(&info.self_ty_text))
				&& arguments_match(
					qualifier.trait_path.as_ref().and_then(|path| path.arguments.as_ref()),
					info.trait_text.as_deref(),
				)
		});

		impls.sort();
		impls.dedup();

		match segments {
			[] => impls,

			[name] => impls
				.iter()
				.flat_map(|&impl_block| super::impls::named_assoc_items(self.ws, impl_block))
				.filter(|&item| self.ws.item(item).name.as_ref() == Some(name))
				.collect(),

			_ => Vec::new(),
		}
	}

	fn resolve_unqualified(&self, path: &ItemPath) -> Vec<ItemId> {
		let selected = || self.ws.selected_crates().map(|krate| krate.id()).collect::<Vec<_>>();
		let mut items = Vec::new();

		match path.anchor {
			Anchor::Crate => {
				for krate in selected() {
					items.extend(self.walk_from_root(krate, &path.segments));
				}
			}

			Anchor::Global => {
				if let Some((first, rest)) = path.segments.split_first() {
					for krate in self.crates_named(first) {
						items.extend(self.walk_from_root(krate, rest));
					}
				}
			}

			// presumed absolute: `crate::...` in every selected crate, and `::...` when it names a crate
			Anchor::None => {
				for krate in selected() {
					items.extend(self.walk_from_root(krate, &path.segments));
				}

				if let Some((first, rest)) = path.segments.split_first() {
					for krate in self.crates_named(first) {
						items.extend(self.walk_from_root(krate, rest));
					}
				}
			}

			// there is no current module for user paths
			Anchor::SelfModule | Anchor::Super(_) => {}
		}

		items
	}

	fn self_type_ends_with(&self, impl_block: ItemId, self_ty: &ItemPath) -> bool {
		let Some(written) = self.ws.item(impl_block).impl_info().and_then(|info| info.self_ty.base_path()) else {
			return false;
		};

		let resolved = self.impls.self_res.get(&impl_block).map_or(&[][..], Vec::as_slice);

		self.ends_with(written, resolved, self_ty)
	}

	/// The last segment of an unqualified path, and the modules its other segments name.
	fn split_name<'p>(&self, path: &'p ItemPath) -> Option<(&'p SmolStr, Vec<ItemId>)> {
		let (name, prefix) = path.segments.split_last()?;
		let prefix = ItemPath {
			anchor: path.anchor,
			segments: prefix.to_vec(),
			..ItemPath::default()
		};
		let modules = self
			.compute_item_path(&prefix)
			.into_iter()
			.filter(|&item| self.ws.item(item).kind == ItemKind::Module);

		Some((name, modules.collect()))
	}

	/// Items named by `segments` starting at a crate root (all namespaces for the last segment). Visibility is not
	/// enforced: users may name private items.
	fn walk_from_root(&self, krate: CrateId, segments: &[SmolStr]) -> Vec<ItemId> {
		let root = ItemId::crate_root(krate);

		if segments.is_empty() {
			return vec![root];
		}

		let mut walker = Walker::new(self.ws, &self.tables, root, PathKind::Code);

		walker
			.members(vec![Found::module(root)], segments, Want::All)
			.into_iter()
			.filter_map(|found| match found.res {
				Res::Item(id) => Some(id),
				_ => None,
			})
			.collect()
	}
}

fn ends_with(names: &[&str], suffix: &[&str]) -> bool {
	names.len() >= suffix.len() && names[names.len() - suffix.len()..] == *suffix
}
