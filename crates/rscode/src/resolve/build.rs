//! Building module scopes: definitions first, then `use` imports.
//!
//! Imports are resolved one at a time, in any order, keeping track of what is determined, like rustc does:
//! - A named import binds its name in a namespace once its path resolves to something there, and is known not to
//!   bind it once its path definitely resolves to nothing there. Until then, it may still shadow glob bindings of its
//!   name in its module, so those are not read (by other imports, or by glob imports of the module).
//! - A glob import leaves its module open until its path resolves: names the module lacks may still come.
//! - A lookup is final when nothing can add bindings to what it looked at anymore: no named import may still bind the
//!   name there or in the modules it glob-imports from (transitively), and none of these modules is open.
//!
//! Glob imports copy bindings as soon as they appear. An import whose resolution may still change waits on the imports
//! that could change it, and is resolved again when one of them makes progress. Imports left waiting on each other
//! (cycles) are given up on, innermost cycles first; rustc reports them as unresolved.
//!
//! Scopes only grow (see [`Scope::insert`](super::scope::Scope::insert)), and so does what is known about imports, so
//! resolution terminates.

use super::fxhash::FxHashMap;
use super::fxhash::FxHashSet;
use super::names;
use super::names::SYSROOT_CRATES;
use super::scope::Entry;
use super::scope::Origin;
use super::scope::Tables;
use super::text;
use super::vis::ModuleTree;
use super::vis::Vis;
use super::vis::declared_vis;
use super::vis::home_module;
use super::vis::parent_module;
use super::walk::Found;
use super::walk::Unsettled;
use super::walk::Walker;
use super::walk::Want;
use crate::model::Crate;
use crate::model::Dependency;
use crate::model::ItemDetail;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::TargetKind;
use crate::model::Workspace;
use crate::resolve::Namespace;
use crate::resolve::Res;
use rscode_fmt::Edition;
use smol_str::SmolStr;
use std::collections::BTreeSet;
use std::collections::VecDeque;

/// A binding to add to a scope: name, namespace, and binding.
type NewBinding = (SmolStr, Namespace, Entry);

/// Whether a named import binds its name in a namespace.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Binds {
	/// Not known yet: glob bindings of the name in the import's module may still be shadowed by it.
	Maybe,

	Yes,
	No,
}

/// What is known about imports while they are being resolved.
#[derive(Debug, Default)]
pub(super) struct Determinacy {
	/// Named imports (indices) by module and bound name.
	by_name: FxHashMap<ItemId, FxHashMap<SmolStr, Vec<usize>>>,

	/// Whether each import binds its name, by namespace (always `No` for glob imports and imports that bind no name).
	binds: Vec<[Binds; 3]>,

	/// Glob imports whose path is not resolved yet, by module.
	open: FxHashMap<ItemId, Vec<usize>>,

	/// The modules each module glob-imports from.
	sources: FxHashMap<ItemId, Vec<ItemId>>,
}

impl Determinacy {
	/// Marks a glob import's path as resolved. Returns whether it was not yet.
	fn close(&mut self, module: ItemId, glob: usize) -> bool {
		let Some(globs) = self.open.get_mut(&module) else {
			return false;
		};

		let before = globs.len();

		globs.retain(|&index| index != glob);
		globs.len() != before
	}

	fn is_open(&self, module: ItemId, glob: usize) -> bool {
		self.open.get(&module).is_some_and(|globs| globs.contains(&glob))
	}

	/// Named imports of `module` other than `own` that may still bind `name` in `namespace`.
	pub(super) fn pending(&self, module: ItemId, name: &str, namespace: Namespace, own: Option<usize>) -> impl Iterator<Item = usize> + '_ {
		(self.by_name.get(&module).and_then(|names| names.get(name)).into_iter().flatten())
			.copied()
			.filter(move |&index| Some(index) != own && self.binds[index][namespace.index()] == Binds::Maybe)
	}

	/// The imports other than `own` that may still add bindings of `name` in `namespace` to `module`: named imports that
	/// may still bind it, and glob imports whose path is not resolved yet, in the module and in the modules it
	/// glob-imports from (transitively).
	pub(super) fn unsettling(&self, module: ItemId, name: &str, namespace: Namespace, own: Option<usize>) -> Vec<usize> {
		let mut waits = Vec::new();
		let mut visited = FxHashSet::default();
		let mut stack = vec![module];

		while let Some(current) = stack.pop() {
			waits.extend(self.pending(current, name, namespace, own));
			waits.extend(self.open.get(&current).into_iter().flatten().copied().filter(|&index| Some(index) != own));

			for &source in self.sources.get(&current).into_iter().flatten() {
				if source != module && visited.insert(source) {
					stack.push(source);
				}
			}
		}

		waits
	}
}

#[derive(Debug)]
struct Fixpoint {
	imports: Vec<Import>,
	det: Determinacy,

	/// Glob imports (indices) by the modules they import from.
	importers: FxHashMap<ItemId, Vec<usize>>,

	/// The sources each glob import (index) was linked to.
	linked: FxHashSet<(usize, ItemId)>,

	/// What each import refers to.
	targets: FxHashMap<ItemId, BTreeSet<(Namespace, Res)>>,

	/// Imports whose resolution is final, or that were given up on.
	done: Vec<bool>,

	/// Imports to resolve (again).
	queue: VecDeque<usize>,
	queued: Vec<bool>,

	/// The imports each import waits on, as of its last resolution.
	waits: Vec<Vec<usize>>,

	/// The imports waiting on each import (possibly more than once).
	waiters: Vec<Vec<usize>>,

	/// Slots whose bindings changed or became readable, to copy to the modules that glob-import them.
	changed: Vec<(ItemId, SmolStr, Namespace)>,
}

impl Fixpoint {
	fn new(ws: &Workspace, excluded: &FxHashSet<ItemId>) -> Self {
		let mut imports = Vec::new();
		let mut det = Determinacy::default();

		for krate in ws.crates() {
			for (id, data) in krate.items() {
				let Some(info) = data.import_info().filter(|_| !excluded.contains(&id)) else {
					continue;
				};

				let index = imports.len();
				let module = home_module(ws, id);
				let name = info.binding_name().filter(|name| !names::is_path_keyword(name)).cloned();

				let shape = match (info.glob, info.is_self) {
					(true, _) => Shape::Glob,
					(false, true) => Shape::SelfImport,
					(false, false) => Shape::Single,
				};

				let binds = match (shape, &name) {
					(Shape::Single, Some(_)) => [Binds::Maybe; 3],
					(Shape::SelfImport, Some(_)) => [Binds::Maybe, Binds::No, Binds::No],
					_ => [Binds::No; 3],
				};

				if shape == Shape::Glob {
					det.open.entry(module).or_default().push(index);
				} else if let Some(name) = &name {
					det.by_name.entry(module).or_default().entry(name.clone()).or_default().push(index);
				}

				det.binds.push(binds);

				imports.push(Import {
					id,
					module,
					vis: declared_vis(ws, id),
					name,
					shape,
				});
			}
		}

		let count = imports.len();

		Self {
			imports,
			det,
			importers: FxHashMap::default(),
			linked: FxHashSet::default(),
			targets: FxHashMap::default(),
			done: vec![false; count],
			queue: (0..count).collect(),
			queued: vec![true; count],
			waits: vec![Vec::new(); count],
			waiters: vec![Vec::new(); count],
			changed: Vec::new(),
		}
	}

	/// Settles which namespaces a named import binds its name in, and binds it. Returns whether anything changed.
	fn bind(&mut self, tables: &mut Tables, index: usize, found: Vec<Found>, unsettled: &Unsettled) -> bool {
		let (id, module, vis) = (self.imports[index].id, self.imports[index].module, self.imports[index].vis);

		let Some(name) = self.imports[index].name.clone() else {
			return false;
		};

		let mut progress = false;

		// settled before bindings are added, so that glob imports of the module copy them
		for namespace in Namespace::ALL {
			let binds = &mut self.det.binds[index][namespace.index()];

			let now = if found.iter().any(|found| found.namespace == namespace) {
				Binds::Yes
			} else if *binds == Binds::Maybe && !unsettled.affects(namespace) {
				Binds::No
			} else {
				continue;
			};

			if *binds == now || *binds == Binds::Yes {
				continue;
			}

			// glob bindings of the name are readable again once no named import may shadow them
			if *binds == Binds::Maybe {
				self.changed.push((module, name.clone(), namespace));
			}

			*binds = now;
			progress = true;
		}

		for found in found {
			let entry = Entry {
				res: found.res,
				import: Some(id),
				origin: Origin::Import,
				vis: vis.narrow(&tables.tree, found.vis),
				guessed: found.guessed,
			};

			if tables.insert(module, &name, found.namespace, entry) {
				self.changed.push((module, name.clone(), found.namespace));
				progress = true;
			}
		}

		progress
	}

	fn finish(self) -> ImportIndex {
		let mut index = ImportIndex::default();

		for import in &self.imports {
			let Some(targets) = self.targets.get(&import.id).filter(|targets| !targets.is_empty()) else {
				index.unresolved.push(import.id);
				continue;
			};

			for (_, res) in targets {
				if let Res::Item(target) = res {
					let importers = index.by_target.entry(*target).or_default();

					if importers.last() != Some(&import.id) {
						importers.push(import.id);
					}
				}
			}

			index.targets.insert(import.id, targets.iter().cloned().collect());
		}

		index.unresolved.sort();

		for importers in index.by_target.values_mut() {
			importers.sort();
			importers.dedup();
		}

		index
	}

	/// Gives up on an import: it binds nothing more, and no longer leaves its module open.
	fn give_up(&mut self, tables: &mut Tables, index: usize) {
		let module = self.imports[index].module;

		self.done[index] = true;

		if self.imports[index].shape == Shape::Glob {
			self.det.close(module, index);
		} else if let Some(name) = self.imports[index].name.clone() {
			for namespace in Namespace::ALL {
				let binds = &mut self.det.binds[index][namespace.index()];

				if *binds == Binds::Maybe {
					*binds = Binds::No;
					self.changed.push((module, name.clone(), namespace));
				}
			}
		}

		self.propagate(tables);
		self.wake(index);
	}

	fn glob_import(&self, index: usize) -> GlobImport {
		let import = &self.imports[index];

		GlobImport {
			id: import.id,
			module: import.module,
			vis: import.vis,
		}
	}

	/// Once nothing makes progress: the undetermined imports that only wait on each other, in cycles that no other
	/// import can break (in the graph of undetermined imports waiting on each other, the strongly connected components
	/// without edges to other components).
	fn innermost_cycles(&self) -> Vec<usize> {
		let stuck: Vec<usize> = (0..self.imports.len())
			.filter(|&index| !self.done[index] && self.is_undetermined(index))
			.collect();
		let mut position = vec![usize::MAX; self.imports.len()];

		for (at, &index) in stuck.iter().enumerate() {
			position[index] = at;
		}

		let edges: Vec<Vec<usize>> = (stuck.iter())
			.map(|&index| {
				self.waits[index]
					.iter()
					.map(|&other| position[other])
					.filter(|&at| at != usize::MAX)
					.collect()
			})
			.collect();

		let component = strongly_connected_components(&edges);
		let mut innermost = vec![true; stuck.len()];

		for (at, next) in edges.iter().enumerate() {
			if next.iter().any(|&next| component[next] != component[at]) {
				innermost[component[at]] = false;
			}
		}

		(0..stuck.len()).filter(|&at| innermost[component[at]]).map(|at| stuck[at]).collect()
	}

	/// Whether an import may still bind its name somewhere, or (for a glob import) resolve its path.
	fn is_undetermined(&self, index: usize) -> bool {
		let import = &self.imports[index];

		match import.shape {
			Shape::Glob => self.det.is_open(import.module, index),
			Shape::Single | Shape::SelfImport => self.det.binds[index].contains(&Binds::Maybe),
		}
	}

	/// Links a glob import to the modules and enums its path resolved to, copying their bindings. Returns whether
	/// anything changed.
	fn link(&mut self, ws: &Workspace, tables: &mut Tables, index: usize, sources: Vec<Found>, unsettled: &Unsettled) -> bool {
		let glob = self.glob_import(index);

		// once the path resolves, the module is no longer open (even if more `cfg` variants of the path may come)
		let mut progress = (!sources.is_empty() || !unsettled.affects(Namespace::Type)) && self.det.close(glob.module, index);

		for source in sources {
			let Res::Item(source) = source.res else {
				continue;
			};

			let kind = ws.item(source).kind;

			if source == glob.module || !matches!(kind, ItemKind::Module | ItemKind::Enum) || !self.linked.insert((index, source)) {
				continue;
			}

			let new = if kind == ItemKind::Module {
				self.importers.entry(source).or_default().push(index);

				let sources = self.det.sources.entry(glob.module).or_default();

				if !sources.contains(&source) {
					sources.push(source);
				}

				module_glob(tables, &self.det, source, glob)
			} else {
				enum_glob(ws, &tables.tree, source, glob)
			};

			for (name, namespace, entry) in new {
				if tables.insert(glob.module, &name, namespace, entry) {
					self.changed.push((glob.module, name, namespace));
				}
			}

			progress = true;
		}

		progress
	}

	/// Copies changed slots to the modules that glob-import them, until no slot changes.
	fn propagate(&mut self, tables: &mut Tables) {
		while let Some((source, name, namespace)) = self.changed.pop() {
			let Some(globs) = self.importers.get(&source) else {
				continue;
			};

			let Some(slot) = tables.scopes.get(&source).and_then(|scope| scope.slot(&name, namespace)) else {
				continue;
			};

			let pending = self.det.pending(source, &name, namespace, None).next().is_some();
			let mut new = Vec::new();

			for &glob in globs {
				let glob = self.glob_import(glob);

				for entry in slot.entries() {
					if glob.copies(&tables.tree, entry, pending) {
						new.push((glob.module, glob.entry(&tables.tree, entry.res.clone(), entry.vis, entry.guessed)));
					}
				}
			}

			for (module, entry) in new {
				if tables.insert(module, &name, namespace, entry) {
					self.changed.push((module, name.clone(), namespace));
				}
			}
		}
	}

	/// Resolves an import (again), binds what it names, and wakes the imports waiting on it if it made progress.
	fn resolve(&mut self, ws: &Workspace, tables: &mut Tables, index: usize) {
		let (id, module, shape) = (self.imports[index].id, self.imports[index].module, self.imports[index].shape);

		let Some(info) = ws.item(id).import_info() else {
			self.done[index] = true;
			return;
		};

		let (found, unsettled) = {
			let mut walker = Walker::for_import(ws, tables, &self.det, module, id, index);

			let found = match shape {
				Shape::Glob => walker.glob_sources(&info.path),
				Shape::SelfImport => walker.resolve(&info.path, Want::One(Namespace::Type)),
				Shape::Single => walker.resolve(&info.path, Want::All),
			};

			(found, walker.unsettled)
		};

		let targets = self.targets.entry(id).or_default();

		for found in &found {
			targets.insert((found.namespace, found.res.clone()));
		}

		let progress = match shape {
			Shape::Glob => self.link(ws, tables, index, found, &unsettled),
			Shape::SelfImport | Shape::Single => self.bind(tables, index, found, &unsettled),
		};

		self.propagate(tables);
		self.done[index] = unsettled.is_settled();
		self.wait(index, unsettled.waits);

		if progress {
			self.wake(index);
		}
	}

	fn run(&mut self, ws: &Workspace, tables: &mut Tables) {
		loop {
			while let Some(index) = self.queue.pop_front() {
				self.queued[index] = false;

				if !self.done[index] {
					self.resolve(ws, tables, index);
				}
			}

			let cycles = self.innermost_cycles();

			if cycles.is_empty() {
				break;
			}

			for index in cycles {
				self.give_up(tables, index);
			}
		}
	}

	/// Records what an import waits on.
	fn wait(&mut self, index: usize, mut waits: Vec<usize>) {
		waits.sort_unstable();
		waits.dedup();

		for &other in &waits {
			self.waiters[other].push(index);
		}

		self.waits[index] = waits;
	}

	/// Queues the imports waiting on an import that made progress.
	fn wake(&mut self, index: usize) {
		for waiter in std::mem::take(&mut self.waiters[index]) {
			if !self.done[waiter] && !self.queued[waiter] {
				self.queued[waiter] = true;
				self.queue.push_back(waiter);
			}
		}
	}
}

/// A glob import copying bindings into its module.
#[derive(Debug, Clone, Copy)]
struct GlobImport {
	id: ItemId,
	module: ItemId,
	vis: Vis,
}

impl GlobImport {
	/// Whether the glob import copies a binding of its source: not textually scoped macros, not bindings the importing
	/// module cannot see, and not glob bindings that a named import may still shadow (when `pending`).
	fn copies(self, tree: &ModuleTree, entry: &Entry, pending: bool) -> bool {
		entry.origin != Origin::Textual && !(pending && entry.origin == Origin::Glob) && entry.vis.is_visible_from(tree, self.module)
	}

	/// The binding the glob import makes for a binding of its source.
	fn entry(self, tree: &ModuleTree, res: Res, vis: Vis, guessed: bool) -> Entry {
		Entry {
			res,
			import: Some(self.id),
			origin: Origin::Glob,
			vis: self.vis.narrow(tree, vis),
			guessed,
		}
	}
}

#[derive(Debug)]
struct Import {
	id: ItemId,
	module: ItemId,
	vis: Vis,

	/// The bound name (`None` for globs and `_` imports).
	name: Option<SmolStr>,

	shape: Shape,
}

/// What imports resolved to.
#[derive(Debug, Default)]
pub(super) struct ImportIndex {
	/// What every import refers to, by namespace. For glob imports: the modules and enums they import from.
	pub(super) targets: FxHashMap<ItemId, Vec<(Namespace, Res)>>,

	/// Imports by the loaded items they refer to.
	pub(super) by_target: FxHashMap<ItemId, Vec<ItemId>>,

	/// Imports that resolved to nothing, in item order.
	pub(super) unresolved: Vec<ItemId>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Shape {
	/// `use a::b;`, `use a::b as c;`, `use a::b as _;`
	Single,

	/// `use a::{self};`: binds only in the type namespace.
	SelfImport,

	/// `use a::*;`
	Glob,
}

/// Adds every module's scope and the items defined in it.
fn add_definitions(ws: &Workspace, krate: &Crate, tables: &mut Tables) {
	for (id, data) in krate.items() {
		if data.kind == ItemKind::Module {
			tables.scopes.entry(id).or_default();
		}
	}

	for (id, data) in krate.items() {
		let Some(parent) = ws.parent(id) else {
			continue;
		};

		// module members; extern blocks and `thread_local!` invocations (the only macro calls with children) are
		// transparent
		if !matches!(ws.item(parent).kind, ItemKind::Module | ItemKind::ExternBlock | ItemKind::MacroCall) {
			continue;
		}

		// `extern crate foo as _;` binds nothing nameable
		let Some(name) = data.name.as_ref().filter(|name| *name != "_") else {
			continue;
		};

		let module = home_module(ws, id);

		match &data.detail {
			ItemDetail::ExternCrate { crate_name, .. } => {
				let entry = Entry {
					res: extern_crate_res(krate, crate_name),
					import: Some(id),
					origin: Origin::Def,
					vis: declared_vis(ws, id),
					guessed: false,
				};

				tables.insert(module, name, Namespace::Type, entry);
			}

			_ if names::is_macro_rules(data) => add_macro_rules(ws, tables, id, name, module),

			_ => {
				for &namespace in names::namespaces(data) {
					let entry = Entry {
						res: Res::Item(id),
						import: None,
						origin: Origin::Def,
						vis: declared_vis(ws, id),
						guessed: false,
					};

					tables.insert(module, name, namespace, entry);
				}
			}
		}
	}
}

/// Binds the crates rustc injects into the root of a 2015 crate (`extern crate std;`), which `use` paths start from.
fn add_injected_crates(krate: &Crate, tables: &mut Tables) {
	if krate.edition() != Edition::E2015 {
		return;
	}

	let root = krate.root_module();

	for &name in names::injected_crates(krate.root_file().text()) {
		let name = SmolStr::new_static(name);

		// an `extern crate` of the same name replaces it
		if tables
			.scopes
			.get(&root)
			.and_then(|scope| scope.slot(&name, Namespace::Type))
			.is_some_and(|slot| !slot.entries().is_empty())
		{
			continue;
		}

		let entry = Entry {
			res: Res::External(name.clone()),
			import: None,
			origin: Origin::Def,
			vis: Vis::Module(root),
			guessed: false,
		};

		tables.insert(root, &name, Namespace::Type, entry);
	}
}

/// Binds a `macro_rules!` macro: path-based at the crate root with `#[macro_export]`, and textually in the defining
/// module and its descendants (and in the parent too, for a `#[macro_use]` module). Textual order is ignored.
fn add_macro_rules(ws: &Workspace, tables: &mut Tables, id: ItemId, name: &SmolStr, module: ItemId) {
	let root = ItemId::crate_root(id.krate());
	let exported = ws.item(id).attrs.macro_export;
	let vis = declared_vis(ws, id);

	if exported {
		let entry = Entry {
			res: Res::Item(id),
			import: None,
			origin: Origin::Def,
			vis,
			guessed: false,
		};

		tables.insert(root, name, Namespace::Macro, entry);
	}

	if !(exported && module == root) {
		let entry = Entry {
			res: Res::Item(id),
			import: None,
			origin: Origin::Textual,
			vis,
			guessed: false,
		};

		tables.insert(module, name, Namespace::Macro, entry);
	}

	let mut textual_root = module;

	while ws.item(textual_root).attrs.macro_use
		&& let Some(parent) = parent_module(ws, textual_root)
	{
		textual_root = parent;
	}

	let macros = tables.textual.entry(textual_root).or_default().entry(name.clone()).or_default();

	if !macros.contains(&id) {
		macros.push(id);
	}
}

/// Binds the macros of a proc-macro crate, which are functions of its root, in the macro namespace: by their names
/// (`#[proc_macro]`, `#[proc_macro_attribute]`), or by the names of the derives they implement
/// (`#[proc_macro_derive(Name)]`).
fn add_proc_macros(ws: &Workspace, krate: &Crate, tables: &mut Tables) {
	if krate.kind() != TargetKind::ProcMacro {
		return;
	}

	let root = krate.root_module();

	for function in ws.children(root) {
		let data = ws.item(function);

		let (ItemKind::Fn, Some(name)) = (data.kind, data.name.as_deref()) else {
			continue;
		};

		let attributes = ws
			.file_of(function)
			.text()
			.get(data.range.start..data.attrs.after_attrs)
			.unwrap_or_default();

		let Some(macro_name) = names::proc_macro_name(&text::outer_attributes(attributes), name) else {
			continue;
		};

		let entry = Entry {
			res: Res::Item(function),
			import: None,
			origin: Origin::Def,
			vis: declared_vis(ws, function),
			guessed: false,
		};

		tables.insert(root, &SmolStr::new(macro_name), Namespace::Macro, entry);
	}
}

/// Builds the scopes of every module of every loaded crate; the `excluded` imports bind nothing, as if removed.
pub(super) fn build(ws: &Workspace, excluded: &FxHashSet<ItemId>) -> (Tables, ImportIndex) {
	let mut tables = Tables {
		tree: ModuleTree::new(ws),
		extern_preludes: ws.crates().iter().map(|krate| extern_prelude(ws, krate)).collect(),
		..Tables::default()
	};

	for krate in ws.crates() {
		add_definitions(ws, krate, &mut tables);
		add_injected_crates(krate, &mut tables);
		add_proc_macros(ws, krate, &mut tables);
	}

	tables.macro_preludes = ws.crates().iter().map(|krate| macro_prelude(ws, krate, &tables)).collect();

	let mut fixpoint = Fixpoint::new(ws, excluded);

	fixpoint.run(ws, &mut tables);
	(tables, fixpoint.finish())
}

/// What `extern crate name` (or a dependency) refers to.
fn dependency_res(dependency: &Dependency) -> Res {
	match dependency.krate {
		Some(krate) => Res::Item(ItemId::crate_root(krate)),
		None => Res::External(dependency.crate_name.clone()),
	}
}

/// The bindings a glob import of an enum makes: its variants visible from the importing module.
fn enum_glob(ws: &Workspace, tree: &ModuleTree, source: ItemId, glob: GlobImport) -> Vec<NewBinding> {
	let mut new = Vec::new();

	for variant in ws.children(source) {
		let data = ws.item(variant);
		let vis = declared_vis(ws, variant);

		let Some(name) = data.name.as_ref().filter(|_| data.kind == ItemKind::Variant) else {
			continue;
		};

		if !vis.is_visible_from(tree, glob.module) {
			continue;
		}

		for &namespace in names::namespaces(data) {
			new.push((name.clone(), namespace, glob.entry(tree, Res::Item(variant), vis, false)));
		}
	}

	new
}

/// What `extern crate crate_name;` in `krate` refers to.
pub(super) fn extern_crate_res(krate: &Crate, crate_name: &str) -> Res {
	if crate_name == "self" {
		return Res::Item(krate.root_module());
	}

	match krate.dependencies().iter().find(|dependency| dependency.name == crate_name) {
		Some(dependency) => dependency_res(dependency),
		None => Res::External(crate_name.into()),
	}
}

/// Dependencies, `extern crate` items of the crate root, and the sysroot crates.
fn extern_prelude(ws: &Workspace, krate: &Crate) -> FxHashMap<SmolStr, Vec<Res>> {
	let mut prelude = FxHashMap::default();

	for dependency in krate.dependencies() {
		push_unique(&mut prelude, dependency.name.clone(), dependency_res(dependency));
	}

	for child in ws.children(krate.root_module()) {
		let data = ws.item(child);

		if let (ItemDetail::ExternCrate { crate_name, .. }, Some(name)) = (&data.detail, &data.name)
			&& name != "_"
		{
			push_unique(&mut prelude, name.clone(), extern_crate_res(krate, crate_name));
		}
	}

	for &name in SYSROOT_CRATES {
		prelude.entry(name.into()).or_insert_with(|| vec![Res::External(name.into())]);
	}

	prelude
}

/// Public macros of the crate roots of `#[macro_use] extern crate`s.
fn macro_prelude(ws: &Workspace, krate: &Crate, tables: &Tables) -> FxHashMap<SmolStr, Vec<Res>> {
	let mut prelude = FxHashMap::default();

	for child in ws.children(krate.root_module()) {
		let data = ws.item(child);

		let ItemDetail::ExternCrate { crate_name, .. } = &data.detail else {
			continue;
		};

		if !data.attrs.macro_use {
			continue;
		}

		let Res::Item(root) = extern_crate_res(krate, crate_name) else {
			continue;
		};

		let Some(scope) = tables.scopes.get(&root) else {
			continue;
		};

		for (name, namespace, slot) in scope.iter() {
			if namespace != Namespace::Macro {
				continue;
			}

			for entry in slot.entries() {
				if entry.origin == Origin::Def && entry.vis == Vis::Public {
					push_unique(&mut prelude, name.clone(), entry.res.clone());
				}
			}
		}
	}

	prelude
}

/// The bindings a glob import makes of the current bindings of a module.
fn module_glob(tables: &Tables, imports: &Determinacy, source: ItemId, glob: GlobImport) -> Vec<NewBinding> {
	let mut new = Vec::new();

	for (name, namespace, slot) in tables.scopes.get(&source).into_iter().flat_map(|scope| scope.iter()) {
		let pending = imports.pending(source, name, namespace, None).next().is_some();

		for entry in slot.entries() {
			if glob.copies(&tables.tree, entry, pending) {
				new.push((
					name.clone(),
					namespace,
					glob.entry(&tables.tree, entry.res.clone(), entry.vis, entry.guessed),
				));
			}
		}
	}

	new
}

fn push_unique(map: &mut FxHashMap<SmolStr, Vec<Res>>, name: SmolStr, res: Res) {
	let resolutions = map.entry(name).or_default();

	if !resolutions.contains(&res) {
		resolutions.push(res);
	}
}

/// The strongly connected component of every node of a graph given by adjacency lists (Tarjan's algorithm, without
/// recursion): nodes get the same number exactly when they are in the same component.
fn strongly_connected_components(edges: &[Vec<usize>]) -> Vec<usize> {
	const UNVISITED: usize = usize::MAX;

	let count = edges.len();
	let mut order = vec![UNVISITED; count];
	let mut low = vec![0; count];
	let mut on_stack = vec![false; count];
	let mut component = vec![UNVISITED; count];
	let mut stack = Vec::new();
	let mut next_order = 0;
	let mut next_component = 0;

	for root in 0..count {
		if order[root] != UNVISITED {
			continue;
		}

		// the nodes being visited, and how many of their edges were followed
		let mut calls = vec![(root, 0)];

		order[root] = next_order;
		low[root] = next_order;
		next_order += 1;
		stack.push(root);
		on_stack[root] = true;

		while let Some(&mut (node, ref mut followed)) = calls.last_mut() {
			if let Some(&next) = edges[node].get(*followed) {
				*followed += 1;

				if order[next] == UNVISITED {
					order[next] = next_order;
					low[next] = next_order;
					next_order += 1;
					stack.push(next);
					on_stack[next] = true;
					calls.push((next, 0));
				} else if on_stack[next] {
					low[node] = low[node].min(order[next]);
				}

				continue;
			}

			calls.pop();

			if let Some(&(caller, _)) = calls.last() {
				low[caller] = low[caller].min(low[node]);
			}

			if low[node] == order[node] {
				while let Some(member) = stack.pop() {
					on_stack[member] = false;
					component[member] = next_component;

					if member == node {
						break;
					}
				}

				next_component += 1;
			}
		}
	}

	component
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn finds_strongly_connected_components() {
		// 0 → 1 → 2 → 0, 2 → 3, 3 → 4 → 3, 5 alone
		let component = strongly_connected_components(&[vec![1], vec![2], vec![0, 3], vec![4], vec![3], vec![]]);

		assert_eq!(component[0], component[1]);
		assert_eq!(component[1], component[2]);
		assert_eq!(component[3], component[4]);
		assert_ne!(component[0], component[3]);
		assert_ne!(component[5], component[0]);
		assert_ne!(component[5], component[3]);
	}
}
