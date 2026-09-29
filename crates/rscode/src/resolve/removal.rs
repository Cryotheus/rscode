//! What removing imports breaks: the bindings that come through them, and the imports and names that go through
//! them.
//!
//! Resolving the workspace again without the imports ([`Resolver::without_imports`]) tells which bindings of module
//! scopes are lost ([`Resolver::lost_bindings`]) and which imports no longer resolve ([`Resolver::broken_in`]).
//! Imports that resolve to nothing (of items that are not loaded, like items made by macros) change no binding, so
//! the names they bound are followed instead ([`Resolver::dead_names`]).

use super::Namespace;
use super::PathKind;
use super::Res;
use super::Resolver;
use super::fxhash::FxHashSet;
use super::names;
use super::vis::declared_vis;
use crate::model::ItemId;
use crate::model::ItemKind;
use rscode_fmt::Edition;
use smol_str::SmolStr;

/// A binding of a module scope that comes through removed imports (see [`Resolver::lost_bindings`]).
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct LostBinding {
	pub(crate) module: ItemId,
	pub(crate) name: SmolStr,
	pub(crate) namespace: Namespace,
	pub(crate) res: Res,

	/// The import (of `module`) that bound it.
	pub(crate) import: ItemId,

	/// What the name is bound to without the removed imports, in the namespace: when something is (a glob import that
	/// the removed import shadowed), code that used the binding still compiles, but names something else.
	pub(crate) now: Vec<Res>,
}

/// A name that a removed import bound in a module, or that an import through it binds (see
/// [`Resolver::dead_names`]).
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct DeadName {
	pub(crate) module: ItemId,
	pub(crate) name: SmolStr,

	/// The import that bound it: the removed import, a named import through it, or a glob import of the module of
	/// another dead name.
	pub(crate) import: ItemId,
}

/// See [`Resolver::dead_names`].
#[derive(Debug, Clone, Default)]
pub(crate) struct DeadNames {
	pub(crate) names: Vec<DeadName>,

	/// The (named) imports whose paths go through the names, which no longer resolve.
	pub(crate) broken: Vec<ItemId>,
}

impl Resolver<'_> {
	/// The bindings of module scopes that `other`, a resolver of the same workspace without some imports (see
	/// [`Resolver::without_imports`]), does not have: those that came through the missing imports, directly or through
	/// other imports (and glob imports) of what they bound.
	pub(crate) fn lost_bindings(&self, other: &Resolver<'_>) -> Vec<LostBinding> {
		let mut lost = Vec::new();

		for (&module, scope) in &self.tables.scopes {
			let other_scope = other.tables.scopes.get(&module);

			for (name, namespace, slot) in scope.iter() {
				let kept = other_scope.and_then(|scope| scope.slot(name, namespace)).map(|slot| slot.entries()).unwrap_or_default();

				for entry in slot.entries() {
					if let Some(import) = entry.import.filter(|_| !kept.iter().any(|kept| kept.res == entry.res)) {
						lost.push(LostBinding {
							module,
							name: name.clone(),
							namespace,
							res: entry.res.clone(),
							import,
							now: kept.iter().map(|kept| kept.res.clone()).collect(),
						});
					}
				}
			}
		}

		lost.sort_by(|a, b| (a.module, &a.name, a.namespace.index(), a.import).cmp(&(b.module, &b.name, b.namespace.index(), b.import)));
		lost
	}

	/// The imports that resolve to something here, but to nothing in `other` (a resolver of the same workspace without
	/// some imports, see [`Resolver::without_imports`]): they go through the missing imports.
	pub(crate) fn broken_in(&self, other: &Resolver<'_>) -> Vec<ItemId> {
		let unresolved: FxHashSet<ItemId> = self.imports.unresolved.iter().copied().collect();

		(other.imports.unresolved.iter().copied())
			.filter(|import| !unresolved.contains(import) && self.imports.targets.contains_key(import))
			.collect()
	}

	/// The names that `removed` imports bound, the names that imports through them (named imports that resolve to
	/// nothing, and glob imports of their modules) bind, and so on: what removing the imports breaks even when they
	/// resolve to nothing (they import items that are not loaded, like items made by macros), which comparing
	/// resolvers (see [`Resolver::lost_bindings`]) cannot tell.
	///
	/// A name that a glob import of the module might still bind to items that are not loaded (its module has macro
	/// invocations, or imports that resolve to nothing) is not dead.
	pub(crate) fn dead_names(&self, removed: &[ItemId]) -> DeadNames {
		let mut dead = DeadNames::default();

		for &import in removed {
			self.add_dead(&mut dead.names, import, self.ws.module_of(import), None);
		}

		let mut pending: Vec<ItemId> = (self.imports.unresolved.iter().copied())
			.filter(|&import| !removed.contains(&import) && self.ws.item(import).import_info().is_some_and(|info| !info.glob))
			.collect();
		let globs: Vec<(ItemId, ItemId, Vec<ItemId>)> = (self.imports.targets.iter())
			.filter(|(import, _)| !removed.contains(import))
			.filter(|(import, _)| self.ws.item(**import).import_info().is_some_and(|info| info.glob))
			.map(|(&import, targets)| {
				let sources = targets.iter().filter_map(|(_, res)| match res {
					Res::Item(item) => Some(*item),
					Res::External(_) | Res::Builtin(_) => None,
				});

				(import, self.ws.module_of(import), sources.collect())
			})
			.collect();

		loop {
			let before = (dead.names.len(), dead.broken.len());

			pending.retain(|&import| {
				if !self.path_through(import, &dead.names) {
					return true;
				}

				dead.broken.push(import);
				self.add_dead(&mut dead.names, import, self.ws.module_of(import), None);
				false
			});

			// glob imports of the modules of dead names bind them too (when they see them, and nothing else binds them)
			for (glob, module, sources) in &globs {
				let found: Vec<DeadName> = (dead.names.iter())
					.filter(|name| sources.contains(&name.module))
					.filter(|name| declared_vis(self.ws, name.import).is_visible_from(&self.tables.tree, *module))
					.cloned()
					.collect();

				for name in found {
					self.add_dead(&mut dead.names, *glob, *module, Some(name.name));
				}
			}

			if (dead.names.len(), dead.broken.len()) == before {
				dead.broken.sort();
				dead.broken.dedup();
				return dead;
			}
		}
	}

	/// Adds the name that `import` binds in `module` (the name of a named import, or `name` for a glob import) to the
	/// dead names, unless it is there, or `module` binds it otherwise, or a glob import might (see
	/// [`Resolver::dead_names`]).
	fn add_dead(&self, dead: &mut Vec<DeadName>, import: ItemId, module: ItemId, name: Option<SmolStr>) {
		let name = match name {
			Some(name) => name,
			None => {
				let info = self.ws.item(import).import_info();

				match info.and_then(|info| info.binding_name()).filter(|name| !names::is_path_keyword(name)) {
					Some(name) => name.clone(),
					None => return,
				}
			}
		};

		let known = dead.iter().any(|dead| dead.module == module && dead.name == name);
		let bound = Namespace::ALL.into_iter().any(|namespace| !self.bindings(module, &name, namespace).is_empty());

		if !known && !bound && !self.globs_may_bind(module, &name, dead, &mut Vec::new()) {
			dead.push(DeadName { module, name, import });
		}
	}

	/// Whether a glob import of `module` might bind `name` to items that are not loaded.
	fn globs_may_bind(&self, module: ItemId, name: &str, dead: &[DeadName], visited: &mut Vec<ItemId>) -> bool {
		if visited.contains(&module) {
			return false;
		}

		visited.push(module);

		let uses = self.ws.children(module).filter(|&child| self.ws.item(child).kind == ItemKind::Use);

		for glob in uses.flat_map(|use_item| self.ws.children(use_item)) {
			if !self.ws.item(glob).import_info().is_some_and(|info| info.glob) {
				continue;
			}

			let Some(targets) = self.imports.targets.get(&glob) else {
				return true;
			};

			for (_, res) in targets {
				let Res::Item(source) = res else {
					return true;
				};

				if self.may_bind_unloaded(*source, name, dead, visited) {
					return true;
				}
			}
		}

		false
	}

	/// Whether a module might bind `name` to items that are not loaded: it has macro invocations (other than
	/// `thread_local!`, whose statics are loaded), or imports that resolve to nothing and are not dead, or glob
	/// imports that might. Not when the name is dead in it: its import of the name would have clashed with an item of
	/// the same name, and its glob imports were checked.
	fn may_bind_unloaded(&self, module: ItemId, name: &str, dead: &[DeadName], visited: &mut Vec<ItemId>) -> bool {
		if self.ws.item(module).kind != ItemKind::Module || dead.iter().any(|dead| dead.module == module && dead.name == name) {
			return false;
		}

		let macro_calls = (self.ws.children(module))
			.any(|child| self.ws.item(child).kind == ItemKind::MacroCall && self.ws.children(child).next().is_none());

		let uses = self.ws.children(module).filter(|&child| self.ws.item(child).kind == ItemKind::Use);
		let unresolved = uses.flat_map(|use_item| self.ws.children(use_item)).any(|import| {
			let Some(info) = self.ws.item(import).import_info() else {
				return false;
			};
			let is_dead = dead.iter().any(|dead| dead.import == import);
			let binds_name = info.glob || info.binding_name().is_some_and(|bound| bound == name);

			binds_name && !is_dead && self.imports.unresolved.binary_search(&import).is_ok()
		});

		macro_calls || unresolved || self.globs_may_bind(module, name, dead, visited)
	}

	/// Whether the path of an import fails to resolve at a segment that names one of `dead` in its module.
	fn path_through(&self, import: ItemId, dead: &[DeadName]) -> bool {
		let Some(info) = self.ws.item(import).import_info() else {
			return false;
		};

		let module = self.ws.module_of(import);
		let edition_2015 = self.ws.krate(import.krate()).edition() == Edition::E2015;
		let prefixes = self.resolve_prefixes(module, &info.path, None, PathKind::Use);

		(info.path.segments.iter().zip(&prefixes)).enumerate().any(|(index, (segment, resolutions))| {
			let scopes: Vec<ItemId> = match index {
				0 if names::is_path_keyword(&segment.name) => return false,
				0 if edition_2015 => vec![ItemId::crate_root(import.krate())],
				0 if info.path.leading_colon => return false,
				0 => vec![module],
				_ => (prefixes[index - 1].iter())
					.filter_map(|res| match res {
						Res::Item(item) if self.ws.item(*item).kind == ItemKind::Module => Some(*item),
						_ => None,
					})
					.collect(),
			};

			// (the first segment falls back to preludes when the scope does not bind it)
			let unresolved = resolutions.is_empty() || (index == 0 && scopes.iter().all(|&scope| !self.binds(scope, &segment.name)));

			unresolved && dead.iter().any(|dead| dead.name == segment.name && scopes.contains(&dead.module))
		})
	}

	/// Whether a module's scope binds a name, in any namespace.
	pub(crate) fn binds(&self, module: ItemId, name: &str) -> bool {
		Namespace::ALL.into_iter().any(|namespace| !self.bindings(module, name, namespace).is_empty())
	}
}
