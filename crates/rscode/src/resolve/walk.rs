//! Resolving paths segment by segment.

use super::PathKind;
use super::build::Determinacy;
use super::names;
use super::names::Fallback;
use super::scope::Origin;
use super::scope::Slot;
use super::scope::Tables;
use super::vis::Vis;
use super::vis::declared_vis;
use super::vis::parent_module;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::PathRef;
use crate::model::Workspace;
use crate::resolve::Namespace;
use crate::resolve::Res;
use rscode_fmt::Edition;
use smol_str::SmolStr;

/// Paths into things outside of the loaded crates are not extended beyond this many segments, so that malformed
/// self-referential imports (`use self::a::b as a;`) cannot grow them forever.
const MAX_EXTERNAL_SEGMENTS: usize = 32;

/// A resolution of a segment: what it names, in which namespace, and how visible the binding is.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct Found {
	pub(super) namespace: Namespace,
	pub(super) res: Res,
	pub(super) vis: Vis,

	/// Whether the namespace is a guess (see [`Entry::guessed`](super::scope::Entry::guessed)).
	pub(super) guessed: bool,
}

impl Found {
	pub(super) fn module(module: ItemId) -> Self {
		Self::public(Namespace::Type, Res::Item(module))
	}

	fn public(namespace: Namespace, res: Res) -> Self {
		Self {
			namespace,
			res,
			vis: Vis::Public,
			guessed: false,
		}
	}
}

/// The result of looking up a name in a module's scope.
struct Lookup {
	/// Whether a named import may still bind the name (glob bindings of the name were then ignored).
	pending: bool,

	/// The imports that could still add bindings of the name (none when the lookup is final).
	waits: Vec<usize>,
}

/// What a resolution made while imports are being resolved may still change on.
#[derive(Debug, Default)]
pub(super) struct Unsettled {
	/// A segment before the last one may still resolve to more (which affects every namespace of the result).
	prefix: bool,

	/// The last segment may still resolve to more, by namespace.
	last: [bool; 3],

	/// The imports that could change the resolution: named imports that may still bind a name that was looked up,
	/// and glob imports whose path is not resolved yet.
	pub(super) waits: Vec<usize>,
}

impl Unsettled {
	/// Whether the resolution may still change in a namespace.
	pub(super) fn affects(&self, namespace: Namespace) -> bool {
		self.prefix || self.last[namespace.index()]
	}

	/// Whether the resolution is final.
	pub(super) fn is_settled(&self) -> bool {
		!self.prefix && !self.last.contains(&true)
	}
}

/// Resolves paths written in (or relative to) a module.
pub(super) struct Walker<'a> {
	ws: &'a Workspace,
	tables: &'a Tables,

	/// What is known about imports, while they are being resolved.
	imports: Option<&'a Determinacy>,

	/// The module the path is written in.
	module: ItemId,

	kind: PathKind,

	/// Associated items are reachable through types and traits (not in `use` paths).
	assoc: bool,

	/// Only bindings visible from `module` are found (import resolution).
	enforce_vis: bool,

	/// The import being resolved (item and index): its own bindings are invisible to it, and it never waits on itself.
	own: Option<(ItemId, usize)>,

	/// Whether the segment being looked up is the last one of the path.
	last_segment: bool,

	/// What the resolution may still change on (while imports are being resolved).
	pub(super) unsettled: Unsettled,
}

impl<'a> Walker<'a> {
	/// A walker for paths written in `module` after imports are resolved.
	pub(super) fn new(ws: &'a Workspace, tables: &'a Tables, module: ItemId, kind: PathKind) -> Self {
		Self {
			ws,
			tables,
			imports: None,
			module,
			kind,
			assoc: kind == PathKind::Code,
			enforce_vis: false,
			own: None,
			last_segment: false,
			unsettled: Unsettled::default(),
		}
	}

	/// A walker for the path of import number `index` (item `import`) of `module`, during import resolution.
	pub(super) fn for_import(ws: &'a Workspace, tables: &'a Tables, imports: &'a Determinacy, module: ItemId, import: ItemId, index: usize) -> Self {
		Self {
			imports: Some(imports),
			enforce_vis: true,
			own: Some((import, index)),
			..Self::new(ws, tables, module, PathKind::Use)
		}
	}

	fn assoc_items(&self, owner: ItemId, name: &str, want: Want, out: &mut Vec<Found>) {
		let Some(items) = self.tables.assoc.get(&owner) else {
			return;
		};

		for &item in items {
			let data = self.ws.item(item);

			if data.name.as_deref() != Some(name) {
				continue;
			}

			for &namespace in names::namespaces(data) {
				if want.admits(namespace) {
					out.push(Found {
						vis: declared_vis(self.ws, item),
						..Found::public(namespace, Res::Item(item))
					});
				}
			}
		}
	}

	/// `name` as a member of the crate root: 2015 `use` paths and `::name` paths. Crates are only found there through
	/// `extern crate` items (including the injected `extern crate std;`).
	fn crate_relative(&mut self, name: &str, want: Want) -> Vec<Found> {
		let mut out = Vec::new();

		self.member(&Found::module(self.root()), name, want, &mut out);
		out
	}

	/// Sorts and removes duplicate resolutions, keeping the widest visibility of each (and certainty of namespace).
	fn dedup(&self, mut found: Vec<Found>) -> Vec<Found> {
		found.sort_by(|a, b| (a.namespace, &a.res).cmp(&(b.namespace, &b.res)));
		found.dedup_by(|later, kept| {
			if later.namespace != kept.namespace || later.res != kept.res {
				return false;
			}

			if later.vis.is_wider_than(&self.tables.tree, kept.vis) {
				kept.vis = later.vis;
			}

			kept.guessed &= later.guessed;
			true
		});

		found
	}

	fn edition(&self) -> Edition {
		self.ws.krate(self.module.krate()).edition()
	}

	/// `::name` in 2018+: a crate of the extern prelude, or an external crate that was not declared.
	fn extern_crate(&self, name: &str, want: Want) -> Vec<Found> {
		if !want.admits(Namespace::Type) {
			return Vec::new();
		}

		let krate = self.module.krate().index();

		match self.tables.extern_preludes.get(krate).and_then(|prelude| prelude.get(name)) {
			Some(resolutions) => resolutions.iter().map(|res| Found::public(Namespace::Type, res.clone())).collect(),
			None => vec![Found::public(Namespace::Type, Res::External(name.into()))],
		}
	}

	/// The modules and enums (or external paths) a glob import `path::*` imports from.
	pub(super) fn glob_sources(&mut self, path: &PathRef) -> Vec<Found> {
		if path.segments.is_empty() {
			// `use *;` / `use ::*;` import from the crate root in 2015 and are errors otherwise
			return match self.edition() {
				Edition::E2015 => vec![Found::module(self.root())],
				_ => Vec::new(),
			};
		}

		self.resolve(path, Want::One(Namespace::Type))
	}

	/// A name written in the module itself: its scope (including textually scoped macros), then the preludes.
	fn lexical(&mut self, name: &str, want: Want) -> Vec<Found> {
		let mut out = Vec::new();

		for &namespace in want.namespaces() {
			let before = out.len();
			let lookup = self.scope_lookup(self.module, name, namespace, true, &mut out);

			if namespace == Namespace::Macro {
				for &macro_item in self.tables.textual_macros(self.ws, self.module, name) {
					out.push(Found {
						vis: declared_vis(self.ws, macro_item),
						..Found::public(namespace, Res::Item(macro_item))
					});
				}
			}

			// the preludes apply when no named import may still bind the name in the module (a glob import cannot: a
			// name both glob-imported and in a prelude is ambiguous at the start of an import path)
			if out.len() == before && !lookup.pending && self.prelude(name, namespace, &mut out) {
				continue;
			}

			self.unsettle(namespace, lookup.waits);
		}

		self.dedup(out)
	}

	/// Looks up `name` as a member of what `container` resolved to.
	fn member(&mut self, container: &Found, name: &str, want: Want, out: &mut Vec<Found>) {
		match &container.res {
			Res::Item(id) => match self.ws.item(*id).kind {
				ItemKind::Module => {
					for &namespace in want.namespaces() {
						let lookup = self.scope_lookup(*id, name, namespace, false, out);

						self.unsettle(namespace, lookup.waits);
					}
				}

				ItemKind::Enum => {
					let before = out.len();

					self.variants(*id, name, want, out);

					// variants shadow associated items of the same name in their namespaces
					if self.assoc {
						let shadowed: Vec<Namespace> = out[before..].iter().map(|found| found.namespace).collect();

						for &namespace in want.namespaces().iter().filter(|namespace| !shadowed.contains(namespace)) {
							self.assoc_items(*id, name, Want::One(namespace), out);
						}
					}
				}

				ItemKind::Struct | ItemKind::Union | ItemKind::TypeAlias | ItemKind::ForeignType | ItemKind::Trait if self.assoc => {
					self.assoc_items(*id, name, want, out);
				}

				_ => {}
			},

			Res::External(path) => extend_external(path, name, want, out),

			// `u8::MAX`, `str::from_utf8`
			Res::Builtin(ty) if self.kind == PathKind::Code => extend_external(ty, name, want, out),
			Res::Builtin(_) => {}
		}
	}

	/// Continues from `start` through member segments `names` (the last one looked up in `want`).
	pub(super) fn members(&mut self, start: Vec<Found>, names: &[SmolStr], want: Want) -> Vec<Found> {
		let mut current = start;

		for (index, name) in names.iter().enumerate() {
			self.last_segment = index + 1 == names.len();

			let want_here = if self.last_segment { want } else { Want::One(Namespace::Type) };
			let mut next = Vec::new();

			for container in &current {
				self.member(container, name, want_here, &mut next);
			}

			current = self.dedup(next);

			if current.is_empty() {
				break;
			}
		}

		current
	}

	/// The resolution of every prefix of `path`: element `i` resolves `path.segments[..=i]`, in the type namespace
	/// except for the last segment, which is looked up in `want`.
	pub(super) fn prefixes(&mut self, path: &PathRef, want: Want) -> Vec<Vec<Found>> {
		let count = path.segments.len();
		let mut out: Vec<Vec<Found>> = Vec::with_capacity(count);

		if let Some(mut index) = self.start(path, want, &mut out) {
			while index < count && out.last().is_some_and(|previous| !previous.is_empty()) {
				self.last_segment = index + 1 == count;

				let want_here = if self.last_segment { want } else { Want::One(Namespace::Type) };
				let name = &path.segments[index].name;
				let mut next = Vec::new();

				if let Some(previous) = out.last() {
					for container in previous {
						self.member(container, name, want_here, &mut next);
					}
				}

				out.push(self.dedup(next));
				index += 1;
			}
		}

		out.resize_with(count, Vec::new);
		out
	}

	/// The preludes, in rustc's order: `#[macro_use]` macros, the extern prelude, the standard library prelude, and
	/// built-in types and macros. Returns whether one of them has the name.
	fn prelude(&self, name: &str, namespace: Namespace, out: &mut Vec<Found>) -> bool {
		let krate = self.module.krate().index();

		let from_table = match namespace {
			Namespace::Type => self.tables.extern_preludes.get(krate).and_then(|prelude| prelude.get(name)),
			Namespace::Macro => self.tables.macro_preludes.get(krate).and_then(|prelude| prelude.get(name)),
			Namespace::Value => None,
		};

		if let Some(resolutions) = from_table {
			out.extend(resolutions.iter().map(|res| Found::public(namespace, res.clone())));
			return true;
		}

		let res = match names::fallback(name, namespace) {
			Some(Fallback::External) => Res::External(name.into()),
			Some(Fallback::Builtin) => Res::Builtin(name.into()),
			None => return false,
		};

		out.push(Found::public(namespace, res));
		true
	}

	/// What `path` names (its last segment looked up in `want`).
	pub(super) fn resolve(&mut self, path: &PathRef, want: Want) -> Vec<Found> {
		self.prefixes(path, want).pop().unwrap_or_default()
	}

	fn root(&self) -> ItemId {
		ItemId::crate_root(self.module.krate())
	}

	/// Looks up a name in a module's scope, adding the bindings found to `out`.
	///
	/// While imports are being resolved, glob bindings that a named import may still shadow are ignored, and the
	/// imports that could still add bindings are returned.
	fn scope_lookup(&self, module: ItemId, name: &str, namespace: Namespace, lexical: bool, out: &mut Vec<Found>) -> Lookup {
		let own = self.own.map(|(_, index)| index);
		let slot = self.tables.scopes.get(&module).and_then(|scope| scope.slot(name, namespace));
		let pending = self
			.imports
			.is_some_and(|imports| imports.pending(module, name, namespace, own).next().is_some());

		for entry in slot.map(Slot::entries).unwrap_or_default() {
			// an import cannot see its own bindings
			if self.own.is_some_and(|(import, _)| entry.import == Some(import)) {
				continue;
			}

			// everything in a module's own scope is visible inside of it
			if self.enforce_vis && !lexical && !entry.vis.is_visible_from(&self.tables.tree, self.module) {
				continue;
			}

			if pending && entry.origin == Origin::Glob {
				continue;
			}

			out.push(Found {
				namespace,
				res: entry.res.clone(),
				vis: entry.vis,
				guessed: entry.guessed,
			});
		}

		let waits = match self.imports {
			None => Vec::new(),

			// only named imports of the module can add to bindings that shadow glob imports
			Some(imports) if slot.is_some_and(Slot::shadows_globs) => imports.pending(module, name, namespace, own).collect(),
			Some(imports) => imports.unsettling(module, name, namespace, own),
		};

		Lookup { pending, waits }
	}

	/// Resolves the first segment(s), pushing their results, and returns the index of the next segment
	/// (`None` when the path cannot start this way, e.g. `Self::x` or too many `super`s).
	fn start(&mut self, path: &PathRef, want: Want, out: &mut Vec<Vec<Found>>) -> Option<usize> {
		let first = path.segments.first()?;
		let edition_2015 = self.edition() == Edition::E2015;

		self.last_segment = path.segments.len() == 1;

		let want_first = if self.last_segment { want } else { Want::One(Namespace::Type) };

		if path.leading_colon {
			let found = if edition_2015 {
				self.crate_relative(&first.name, want_first)
			} else {
				self.extern_crate(&first.name, want_first)
			};

			out.push(found);
			return Some(1);
		}

		let index = match first.name.as_str() {
			"crate" | "$crate" => {
				out.push(vec![Found::module(self.root())]);
				1
			}

			// `self`, then any number of `super`s (`self::super::x` is `super::x`)
			"self" | "super" => {
				let mut current = self.module;
				let mut index = 0;

				if first.name == "self" {
					out.push(vec![Found::module(current)]);
					index = 1;
				}

				while path.segments.get(index).is_some_and(|segment| segment.name == "super") {
					current = parent_module(self.ws, current)?;
					out.push(vec![Found::module(current)]);
					index += 1;
				}

				index
			}

			// resolved by the caller, who knows the enclosing `impl` or trait
			"Self" => return None,

			name if self.kind == PathKind::Use && edition_2015 => {
				let found = self.crate_relative(name, want_first);

				out.push(found);
				return Some(1);
			}

			name => {
				let found = self.lexical(name, want_first);

				out.push(found);
				return Some(1);
			}
		};

		// a path of only `crate`/`self`/`super` names a module, which is not a value or macro
		if index == path.segments.len()
			&& !want.admits(Namespace::Type)
			&& let Some(last) = out.last_mut()
		{
			last.clear();
		}

		Some(index)
	}

	/// Records that the resolution may still change in `namespace`, until one of `waits` makes progress.
	fn unsettle(&mut self, namespace: Namespace, waits: Vec<usize>) {
		if waits.is_empty() {
			return;
		}

		if self.last_segment {
			self.unsettled.last[namespace.index()] = true;
		} else {
			self.unsettled.prefix = true;
		}

		self.unsettled.waits.extend(waits);
	}

	fn variants(&self, enum_item: ItemId, name: &str, want: Want, out: &mut Vec<Found>) {
		for variant in self.ws.children(enum_item) {
			let data = self.ws.item(variant);

			if data.kind != ItemKind::Variant || data.name.as_deref() != Some(name) {
				continue;
			}

			let vis = declared_vis(self.ws, variant);

			if self.enforce_vis && !vis.is_visible_from(&self.tables.tree, self.module) {
				continue;
			}

			for &namespace in names::namespaces(data) {
				if want.admits(namespace) {
					out.push(Found {
						vis,
						..Found::public(namespace, Res::Item(variant))
					});
				}
			}
		}
	}
}

/// The namespaces a segment is looked up in.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum Want {
	One(Namespace),

	/// Every namespace (the last segment of an import, or of a user path).
	All,
}

impl Want {
	fn admits(self, namespace: Namespace) -> bool {
		match self {
			Self::One(wanted) => wanted == namespace,
			Self::All => true,
		}
	}

	fn namespaces(self) -> &'static [Namespace] {
		match self {
			Self::One(Namespace::Type) => &[Namespace::Type],
			Self::One(Namespace::Value) => &[Namespace::Value],
			Self::One(Namespace::Macro) => &[Namespace::Macro],
			Self::All => &Namespace::ALL,
		}
	}
}

/// `path::name` for a path outside of the loaded crates, in every wanted namespace (a guess when that is all of them).
fn extend_external(path: &str, name: &str, want: Want, out: &mut Vec<Found>) {
	if path.matches("::").count() + 1 >= MAX_EXTERNAL_SEGMENTS {
		return;
	}

	let joined = SmolStr::from(format!("{path}::{name}"));

	for &namespace in want.namespaces() {
		out.push(Found {
			guessed: want == Want::All,
			..Found::public(namespace, Res::External(joined.clone()))
		});
	}
}
