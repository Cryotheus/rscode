//! Renaming items and updating references.
//!
//! A rename changes every `cfg` variant of the named item (and, for items of traits and trait `impl`s, the trait's item
//! and the items of every `impl` of the trait; for files that several crates load, the other crates' copies of the item)
//! together with every reference found by [`Resolver::find_references`] in all loaded crates. `Type::name` names the
//! item of an inherent `impl` when there is one, not the trait `impl` items of the same name. Uncertain references
//! (method calls, paths through generic parameters, and identifiers in macro bodies that do not parse as code) and doc
//! links are only changed when asked for; otherwise they are reported. Out-of-line modules are renamed with their files,
//! and so are the directories of their child modules (unless a module elsewhere loads one of the moved files with
//! `#[path]`, which refuses the rename).
//!
//! Before anything is planned, the new name is checked against the names it could clash with: bindings in the
//! defining module and in the modules importing the item (by name, or through a glob import of its module), other
//! associated items of the same type or trait, other variants of the enum, and existing files for modules.

use crate::Error;
use crate::edit::EditSet;
use crate::model::ItemData;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::PathRef;
use crate::model::PathSegmentRef;
use crate::model::Workspace;
use crate::path::ItemPath;
use crate::path::is_keyword;
use crate::path::is_valid_ident;
use crate::resolve::Binding;
use crate::resolve::Namespace;
use crate::resolve::Reference;
use crate::resolve::ReferenceKind;
use crate::resolve::ReferenceOptions;
use crate::resolve::References;
use crate::resolve::Res;
use crate::resolve::Resolver;
use crate::source::LineCol;
use crate::source::TextRange;
use serde::Deserialize;
use serde::Serialize;
use smol_str::SmolStr;
use std::path::Path;
use std::path::PathBuf;

/// An existing binding the new name would clash with.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct Collision {
	/// Where the clash happens (a module or type path).
	pub scope: String,

	/// The existing item (or import, or file) with the new name, or a local binding with the new name that would
	/// capture a reference (`local variable name`; its location is the reference's).
	pub existing: String,

	/// The file of the existing binding.
	pub file: PathBuf,

	/// Where the existing binding starts.
	pub start: LineCol,
}

/// Collects collisions.
struct Collisions<'a, 'ws> {
	resolver: &'a Resolver<'ws>,
	targets: &'a [ItemId],
	name: &'a str,
	collisions: Vec<Collision>,
}

impl Collisions<'_, '_> {
	fn add(&mut self, collision: Collision) {
		if !self
			.collisions
			.iter()
			.any(|known| known.scope == collision.scope && known.existing == collision.existing)
		{
			self.collisions.push(collision);
		}
	}

	/// Associated items: other associated items of the same types (in any `impl`) or trait, and variants of enums.
	fn associated(&mut self, target: ItemId) {
		let ws = self.resolver.workspace();

		let Some(parent) = ws.parent(target) else {
			return;
		};

		let owners = match ws.item(parent).kind {
			ItemKind::Impl => self.resolver.impl_self_types(parent),
			_ => vec![parent],
		};

		let namespaces = namespaces(ws.item(target));

		// siblings in the same `impl` block (its self type may be unknown)
		let siblings: Vec<ItemId> = ws.children(parent).collect();

		self.members(parent, &siblings, namespaces);

		for owner in owners {
			let mut members = self.resolver.associated_items(owner);

			if ws.item(owner).kind == ItemKind::Enum {
				members.extend(ws.children(owner));
			}

			self.members(owner, &members, namespaces);
		}
	}

	fn binding(&self, module: ItemId, binding: &Binding) -> Collision {
		let ws = self.resolver.workspace();
		let scope = self.resolver.canonical_path(module).to_string();

		let (existing, at) = match (binding.import, &binding.res) {
			(Some(import), _) => (import_text(self.resolver, import), Some(import)),
			(None, Res::Item(item)) => (self.resolver.canonical_path(*item).to_string(), Some(*item)),
			(None, Res::External(path) | Res::Builtin(path)) => (path.to_string(), None),
		};

		let (file, start) = at.map(|item| location(ws, item)).unwrap_or_default();

		Collision {
			scope,
			existing,
			file,
			start,
		}
	}

	/// Modules with a glob import of `source` (a module or enum) that sees the target.
	fn glob_importers(&mut self, source: ItemId, target: ItemId, namespaces: &[Namespace]) {
		let ws = self.resolver.workspace();

		for import in self.resolver.imports_of(source) {
			let is_glob = ws.item(import).import_info().is_some_and(|info| info.glob);
			let module = home_module(ws, import);

			if is_glob && module != source && self.resolver.is_visible_from(target, module) {
				self.scope(module, namespaces, &[]);
			}
		}
	}

	/// Modules that import the target by its name (the import will bind the new name).
	fn importers(&mut self, target: ItemId, namespaces: &[Namespace]) {
		let ws = self.resolver.workspace();
		let old = ws.item(target).name.as_deref();

		for import in self.resolver.imports_of(target) {
			let Some(info) = ws.item(import).import_info() else {
				continue;
			};

			// `use a::Old as Other;` keeps binding `Other`
			if info.glob || info.alias.as_deref().is_some_and(|alias| Some(alias) != old) {
				continue;
			}

			self.scope(home_module(ws, import), namespaces, &[import]);
		}
	}

	fn is_target(&self, item: ItemId) -> bool {
		self.targets.contains(&item)
	}

	/// Members of a container named like the new name, in one of the namespaces.
	fn members(&mut self, container: ItemId, members: &[ItemId], wanted: &[Namespace]) {
		let ws = self.resolver.workspace();

		for &member in members {
			let data = ws.item(member);

			let clashes = data.name.as_deref() == Some(self.name) && namespaces(data).iter().any(|namespace| wanted.contains(namespace));

			if !clashes || self.is_target(member) {
				continue;
			}

			let (file, start) = location(ws, member);

			self.add(Collision {
				scope: self.resolver.canonical_path(container).to_string(),
				existing: self.resolver.canonical_path(member).to_string(),
				file,
				start,
			});
		}
	}

	/// Items of modules: bindings in the defining module, in modules importing the item by name, and in modules
	/// glob-importing its module.
	fn module_level(&mut self, target: ItemId) {
		let ws = self.resolver.workspace();
		let data = ws.item(target);
		let home = home_module(ws, target);
		let namespaces = namespaces(data);

		self.scope(home, namespaces, &[]);

		if data.kind == ItemKind::MacroRules && data.attrs.macro_export {
			self.scope(ItemId::crate_root(target.krate()), namespaces, &[]);
		}

		self.importers(target, namespaces);
		self.glob_importers(home, target, namespaces);
	}

	/// The bindings of the new name in a module's scope (except the targets' definitions, and `exclude`d imports).
	fn scope(&mut self, module: ItemId, namespaces: &[Namespace], exclude: &[ItemId]) {
		for &namespace in namespaces {
			for binding in self.resolver.bindings(module, self.name, namespace) {
				if binding.import.is_some_and(|import| exclude.contains(&import)) {
					continue;
				}

				if binding.import.is_none() && matches!(binding.res, Res::Item(item) if self.is_target(item)) {
					continue;
				}

				let collision = self.binding(module, &binding);

				self.add(collision);
			}
		}
	}

	/// Variants: other variants and associated items of the enum, and the modules importing the variant.
	fn variant(&mut self, target: ItemId) {
		let ws = self.resolver.workspace();

		let Some(enum_item) = ws.parent(target) else {
			return;
		};

		let namespaces = namespaces(ws.item(target));
		let members: Vec<ItemId> = ws.children(enum_item).chain(self.resolver.associated_items(enum_item)).collect();

		self.members(enum_item, &members, namespaces);
		self.importers(target, namespaces);
		self.glob_importers(enum_item, target, namespaces);
	}
}

/// A validated new name.
#[derive(Debug, Clone)]
struct NewName {
	/// The name, without `r#`.
	bare: SmolStr,

	/// The identifier to write (`r#name` for keywords).
	written: String,
}

impl NewName {
	fn parse(text: &str) -> Result<Self, Error> {
		let bare = text.strip_prefix("r#").unwrap_or(text);
		let written = if is_keyword(bare) { format!("r#{bare}") } else { bare.to_owned() };

		if matches!(bare, "crate" | "self" | "super" | "Self" | "_") || !is_valid_ident(&written) {
			return Err(Error::InvalidIdent(text.to_owned()));
		}

		Ok(Self { bare: bare.into(), written })
	}
}

/// The planned rename.
#[derive(Debug, Clone, Serialize)]
pub struct Rename {
	/// The edits, to preview or apply.
	#[serde(skip)]
	pub edits: EditSet,

	/// Canonical paths of the renamed items (every `cfg` variant, and trait item counterparts).
	pub renamed: Vec<String>,

	/// Every occurrence that is edited (definitions included).
	pub references: Vec<Reference>,

	/// Occurrences that might refer to the target but are left untouched (including doc links, unless they are
	/// renamed).
	pub uncertain: Vec<Reference>,

	/// Clashes with existing names (only non-empty when forced).
	pub collisions: Vec<Collision>,

	/// Things to know about the rename.
	pub warnings: Vec<String>,
}

/// Options for [`rename`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct RenameOptions {
	/// Rename even when the new name collides with an existing name.
	pub force: bool,

	/// Which uncertain occurrences (and doc links) to also rename.
	pub references: ReferenceOptions,
}

/// Adds what modules that import a target (by its own name) bind to the name under other `cfg`s, such as `struct Chain`
/// in `#[cfg(a)] use crate::Chain; #[cfg(not(a))] struct Chain;`: the paths through the import name those too, and are
/// renamed, so those configurations only still compile with them renamed. What cannot be renamed along (items that
/// are not loaded, and items imported under another name) is noted in `notes`. Returns whether any was added.
fn add_cfg_variants(resolver: &Resolver<'_>, targets: &mut Vec<ItemId>, notes: &mut Vec<String>) -> bool {
	let ws = resolver.workspace();
	let mut pending = targets.clone();
	let mut added = false;

	while let Some(target) = pending.pop() {
		let data = ws.item(target);

		let Some(name) = data.name.clone() else {
			continue;
		};

		let imports = (resolver.imports_of(target).into_iter()).filter(|&import| {
			ws.item(import)
				.import_info()
				.is_some_and(|info| !info.glob && info.binding_name() == Some(&name))
		});

		for import in imports {
			let module = ws.module_of(import);
			let bindings = namespaces(data).iter().flat_map(|&namespace| resolver.bindings(module, &name, namespace));

			for binding in bindings {
				if binding.glob || binding.import == Some(import) {
					continue;
				}

				match &binding.res {
					Res::Item(item) if targets.contains(item) => {}

					// defined (or imported) there with the same name
					Res::Item(item) if ws.item(*item).name.as_deref() == Some(name.as_str()) => {
						notes.push(format!(
							"also renaming `{}`: `{}` binds `{name}` to it under other `cfg`s, instead of importing `{}`",
							resolver.canonical_path(*item),
							resolver.canonical_path(module),
							resolver.canonical_path(target),
						));
						targets.push(*item);
						pending.push(*item);
						added = true;
					}

					res => {
						let other = match res {
							Res::Item(item) => resolver.canonical_path(*item).to_string(),
							Res::External(path) | Res::Builtin(path) => path.to_string(),
						};

						notes.push(format!(
							"`{}` binds `{name}` to `{other}` under other `cfg`s, which is not renamed: the renamed paths there do \
							 not compile with those `cfg`s",
							resolver.canonical_path(module),
						));
					}
				}
			}
		}
	}

	notes.dedup();
	targets.sort();
	targets.dedup();
	added
}

/// Adds the counterparts of trait items and trait `impl` items (the trait's item, and the items of all `impl`s of the
/// trait), which must keep the same name.
fn add_counterparts(resolver: &Resolver<'_>, targets: &mut Vec<ItemId>) {
	let mut pending = targets.clone();

	while let Some(item) = pending.pop() {
		for counterpart in resolver.trait_counterparts(item) {
			if !targets.contains(&counterpart) {
				targets.push(counterpart);
				pending.push(counterpart);
			}
		}
	}

	targets.sort();
	targets.dedup();
}

/// Adds the items that other crates load from the same source as a target (from a file several crates load as a
/// module, or modules loaded from the same file), which a rename changes too. Returns whether any was added.
fn add_twins(ws: &Workspace, targets: &mut Vec<ItemId>) -> bool {
	let mut twins = Vec::new();

	for &target in targets.iter() {
		let data = ws.item(target);
		let path = ws.file_of(target).path();
		let module_file = data.module_info().filter(|info| !info.inline).and_then(|info| info.file_path.as_deref());

		for krate in ws.crates().iter().filter(|krate| krate.id() != target.krate()) {
			let file = krate.file_id(path);

			if file.is_none() && module_file.is_none_or(|module_file| krate.file_id(module_file).is_none()) {
				continue;
			}

			for (item, other) in krate.items() {
				let same_definition = Some(other.file) == file && other.name_range == data.name_range && other.kind == data.kind;

				// (a module loaded with `#[path]` keeps its file, so it cannot be renamed with one that does not)
				let same_module_file = module_file.is_some()
					&& data.attrs.path.is_none()
					&& other.attrs.path.is_none()
					&& (other.module_info()).is_some_and(|info| !info.inline && info.file_path.as_deref() == module_file);

				if other.name == data.name && (same_definition || same_module_file) && !item.is_crate_root() {
					twins.push(item);
				}
			}
		}
	}

	twins.retain(|twin| !targets.contains(twin));

	let added = !twins.is_empty();

	targets.extend(twins);
	targets.sort();
	targets.dedup();
	added
}

/// Refuses to move files that modules (other than descendants of the renamed ones) load with `#[path]`, since their
/// attributes would point to nothing.
fn check_moves(resolver: &Resolver<'_>, targets: &[ItemId], moves: &[(PathBuf, PathBuf)]) -> Result<(), Error> {
	let ws = resolver.workspace();

	if moves.is_empty() {
		return Ok(());
	}

	for krate in ws.crates() {
		for (item, data) in krate.items() {
			let Some(file) = data
				.module_info()
				.filter(|info| !info.inline && data.attrs.path.is_some())
				.and_then(|info| info.file_path.as_deref())
			else {
				continue;
			};

			let moved = moves.iter().any(|(from, _)| file.starts_with(from));

			// (the files of descendants move with the directory their paths are relative to)
			if moved && !ws.ancestors(item).any(|ancestor| targets.contains(&ancestor)) {
				let path = resolver.canonical_path(item);
				let file = ws.display_path(file).display();

				return Err(Error::Unsupported(format!(
					"`{file}` cannot be moved: module `{path}` loads it with `#[path]`"
				)));
			}
		}
	}

	Ok(())
}

/// Refuses items that cannot be renamed.
fn check_supported(resolver: &Resolver<'_>, target: ItemId) -> Result<(), Error> {
	let data = resolver.workspace().item(target);

	let what = match data.kind {
		_ if target.is_crate_root() => "a crate root",
		ItemKind::Impl => "an `impl` block",
		ItemKind::Use | ItemKind::Import => "an import",
		ItemKind::MacroCall | ItemKind::AssocMacro | ItemKind::ForeignMacro => "a macro invocation",
		ItemKind::ExternBlock => "an `extern` block",
		ItemKind::ExternCrate => "an `extern crate` item",
		_ if data.name.as_ref().is_none_or(|name| name == "_") => "an unnamed item",
		_ => return Ok(()),
	};

	let hint = match data.kind {
		ItemKind::Import => ": rename what it imports instead, or replace its `use` item (for example with `use a::Name as NewName;`)",
		_ => "",
	};

	Err(Error::Unsupported(format!(
		"`{}` is {what}, which cannot be renamed{hint}",
		resolver.canonical_path(target)
	)))
}

/// The directory of the files of a module's child modules declared without `#[path]` (like the loader finds them).
fn child_dir(ws: &Workspace, module: ItemId) -> Option<PathBuf> {
	let data = ws.item(module);
	let info = data.module_info()?;

	if !info.inline {
		let dir = info.file_path.as_deref()?.parent()?;

		return Some(match info.dir_owner {
			true => dir.to_path_buf(),
			false => dir.join(data.name.as_deref()?),
		});
	}

	let parent = ws.module_of(ws.parent(module)?);

	match &data.attrs.path {
		Some(path) => Some(own_dir(ws, parent)?.join(path)),
		None => Some(child_dir(ws, parent)?.join(data.name.as_deref()?)),
	}
}

/// The error refusing a rename because of collisions.
fn collision_error(ws: &Workspace, name: NewName, collisions: &[Collision]) -> Error {
	Error::Collision {
		name: name.written,
		collisions: collisions.iter().map(|collision| describe(ws, collision)).collect(),
	}
}

/// The clashes of the new name.
fn collisions(resolver: &Resolver<'_>, targets: &[ItemId], name: &NewName, moves: &[(PathBuf, PathBuf)]) -> Vec<Collision> {
	let ws = resolver.workspace();
	let mut found = Collisions {
		resolver,
		targets,
		name: &name.bare,
		collisions: Vec::new(),
	};

	for &target in targets {
		let data = ws.item(target);

		match data.kind {
			ItemKind::Variant => found.variant(target),
			kind if kind.is_associated() => found.associated(target),
			_ => found.module_level(target),
		}
	}

	for (from, to) in moves {
		// (on file systems that ignore case, the path of a module's file in another case names the file itself)
		if super::destination_exists(from, to) {
			// the module whose file or directory moves
			let moves_from = |target: ItemId| {
				let file = ws.item(target).module_info().and_then(|info| info.file_path.as_deref());

				file.is_some_and(|file| file.starts_with(from))
			};

			let module = (targets.iter().copied())
				.find(|&target| moves_from(target))
				.or_else(|| targets.iter().copied().find(|&target| ws.item(target).kind == ItemKind::Module));

			found.add(Collision {
				scope: module.map(|module| resolver.canonical_path(module).to_string()).unwrap_or_default(),
				existing: ws.display_path(to).display().to_string(),
				file: to.clone(),
				start: LineCol { line: 1, column: 1 },
			});
		}
	}

	found.collisions
}

/// A collision as a line of an error message.
fn describe(ws: &Workspace, collision: &Collision) -> String {
	let place = match collision.file.as_os_str().is_empty() {
		true => String::new(),
		false => format!(" ({}:{})", ws.display_path(&collision.file).display(), collision.start),
	};

	format!("`{}` in `{}`{place}", collision.existing, collision.scope)
}

/// Drops the items of trait `impl`s shadowed by targets of inherent `impl`s of the same type with the same name
/// (`Type::name` names the inherent item).
fn drop_shadowed(resolver: &Resolver<'_>, targets: &mut Vec<ItemId>) {
	let ws = resolver.workspace();

	// the self types of the `impl` block of an associated item, if it is an inherent (or trait) `impl`
	let owners = |item: ItemId, inherent: bool| -> Option<Vec<ItemId>> {
		let data = ws.item(item);
		let parent = ws.parent(item).filter(|_| data.kind.is_associated())?;
		let info = ws.item(parent).impl_info()?;

		(info.trait_path.is_none() == inherent).then(|| resolver.impl_self_types(parent))
	};

	let shadowing: Vec<(Vec<ItemId>, &ItemData)> = (targets.iter())
		.filter_map(|&target| Some((owners(target, true)?, ws.item(target))))
		.collect();

	targets.retain(|&target| {
		let data = ws.item(target);

		let Some(types) = owners(target, false) else {
			return true;
		};

		!shadowing.iter().any(|(inherent_types, inherent)| {
			inherent.name == data.name
				&& namespaces(inherent).iter().any(|namespace| namespaces(data).contains(namespace))
				&& inherent_types.iter().any(|ty| types.contains(ty))
		})
	});
}

/// The module an item is declared in (extern blocks are transparent; items of `impl` blocks, traits, and enums belong
/// to the enclosing module).
fn home_module(ws: &Workspace, item: ItemId) -> ItemId {
	ws.parent(item).map_or(item, |parent| ws.module_of(parent))
}

/// How an import reads: `use a::b::Name`, `use a::b::Old as Name`, `use a::*`, or `extern crate name`.
fn import_text(resolver: &Resolver<'_>, import: ItemId) -> String {
	let data = resolver.workspace().item(import);

	let Some(info) = data.import_info() else {
		return format!("{} ({})", resolver.canonical_path(import), data.kind);
	};

	let mut text = format!("use {}", info.path);

	if info.glob {
		text.push_str(if info.path.segments.is_empty() { "*" } else { "::*" });
	}

	if let Some(alias) = &info.alias {
		text.push_str(&format!(" as {alias}"));
	}

	text
}

/// Whether a reference is renamed (rather than reported as uncertain).
fn is_renamed(reference: &Reference, options: &ReferenceOptions) -> bool {
	match reference.kind {
		ReferenceKind::DocLink => options.doc_links,
		ReferenceKind::MethodCall => options.method_calls,
		ReferenceKind::MacroToken if !reference.certain => options.macro_tokens,

		// paths through generic parameters (`T::method`) depend on types, like method calls
		_ if !reference.certain => options.method_calls,
		_ => true,
	}
}

/// Where an item (or import) is: its name, or its start.
fn location(ws: &Workspace, item: ItemId) -> (PathBuf, LineCol) {
	let data = ws.item(item);
	let file = ws.file_of(item);

	(file.path().to_path_buf(), file.line_col(data.name_range.unwrap_or(data.range).start))
}

/// The file and directory moves of renamed modules: `old.rs` to `new.rs` (with the directory of its child modules,
/// `old/`), or `old/mod.rs` to `new/mod.rs`, or the child module directory of an inline module. Modules with a
/// `#[path]` attribute keep their files.
fn module_moves(resolver: &Resolver<'_>, targets: &[ItemId], name: &NewName, warnings: &mut Vec<String>) -> Vec<(PathBuf, PathBuf)> {
	let ws = resolver.workspace();
	let mut moves = Vec::new();

	for &target in targets {
		let data = ws.item(target);

		let (Some(info), Some(old)) = (data.module_info(), data.name.as_deref()) else {
			continue;
		};

		if data.attrs.path.is_some() {
			if !info.inline {
				let path = resolver.canonical_path(target);

				warnings.push(format!("module `{path}` is loaded from a file set with `#[path]`, which is not renamed"));
			}

			continue;
		}

		if info.inline {
			// the directory of the files of the inline module's child modules
			if let Some(children) = child_dir(ws, target).filter(|dir| dir.is_dir()) {
				moves.push((children.clone(), children.with_file_name(name.bare.as_str())));
			}

			continue;
		}

		let Some(file) = info.file_path.as_deref().filter(|_| info.file.is_some()) else {
			let path = resolver.canonical_path(target);

			warnings.push(format!("the file of module `{path}` was not loaded, so it is not renamed"));
			continue;
		};

		let Some(dir) = file.parent() else {
			continue;
		};

		if info.dir_owner {
			// `old/mod.rs`
			if dir.file_name().is_some_and(|dir_name| dir_name == old) {
				moves.push((dir.to_path_buf(), dir.with_file_name(name.bare.as_str())));
			}
		} else if file.file_name().is_some_and(|file_name| *file_name == *format!("{old}.rs")) {
			moves.push((file.to_path_buf(), dir.join(format!("{}.rs", name.bare))));

			let children = dir.join(old);

			if children.is_dir() {
				moves.push((children, dir.join(name.bare.as_str())));
			}
		}
	}

	moves.sort();
	moves.dedup();
	moves
}

/// The namespaces an item is bound in (in its module, or as a member of its enum, type, or trait).
fn namespaces(data: &ItemData) -> &'static [Namespace] {
	use crate::model::DataShape;
	use crate::model::ItemDetail;

	const TYPE: &[Namespace] = &[Namespace::Type];
	const VALUE: &[Namespace] = &[Namespace::Value];
	const TYPE_AND_VALUE: &[Namespace] = &[Namespace::Type, Namespace::Value];
	const MACRO: &[Namespace] = &[Namespace::Macro];

	match data.kind {
		ItemKind::Struct | ItemKind::Variant => match data.detail {
			ItemDetail::Data {
				shape: DataShape::Tuple | DataShape::Unit,
				..
			} => TYPE_AND_VALUE,
			_ => TYPE,
		},

		ItemKind::Fn
		| ItemKind::Const
		| ItemKind::Static
		| ItemKind::ForeignFn
		| ItemKind::ForeignStatic
		| ItemKind::AssocFn
		| ItemKind::AssocConst => VALUE,

		ItemKind::MacroRules => MACRO,
		_ => TYPE,
	}
}

/// The directory `#[path]` attributes of a module's children are relative to.
fn own_dir(ws: &Workspace, module: ItemId) -> Option<PathBuf> {
	let info = ws.item(module).module_info()?;

	match info.inline {
		true => child_dir(ws, module),
		false => info.file_path.as_deref().and_then(Path::parent).map(Path::to_path_buf),
	}
}

/// Warnings for new names of module items that a prelude provides in the modules binding them (`drop`, `Vec`, `u8`, or
/// the name of a dependency): the renamed item shadows the prelude's where it is in scope.
fn prelude_warnings(resolver: &Resolver<'_>, targets: &[ItemId], name: &NewName) -> Vec<String> {
	let ws = resolver.workspace();
	let mut warnings = Vec::new();

	let path = PathRef {
		leading_colon: false,
		segments: vec![PathSegmentRef {
			name: name.bare.clone(),
			range: TextRange::new(0, 0),
			has_arguments: false,
		}],
	};

	for &target in targets {
		let data = ws.item(target);

		if data.kind == ItemKind::Variant || data.kind.is_associated() {
			continue;
		}

		// the modules binding the new name: the defining one, and those importing the item (by name, or with a glob)
		let home = home_module(ws, target);
		let named = resolver.imports_of(target).into_iter().map(|import| home_module(ws, import));

		let globs = (resolver.imports_of(home).into_iter())
			.filter(|&import| ws.item(import).import_info().is_some_and(|info| info.glob))
			.map(|import| home_module(ws, import))
			.filter(|&module| resolver.is_visible_from(target, module));

		let mut modules: Vec<ItemId> = Vec::new();

		for module in std::iter::once(home).chain(named).chain(globs) {
			if !modules.contains(&module) {
				modules.push(module);
			}
		}

		// what the prelude provides, and the modules where the renamed item shadows it
		let mut shadowed: Vec<(String, Vec<String>)> = Vec::new();

		for module in modules {
			for &namespace in namespaces(data) {
				if !resolver.bindings(module, &name.bare, namespace).is_empty() {
					continue;
				}

				for res in resolver.resolve_path(module, &path, namespace) {
					let what = match res {
						Res::External(_) => "a name of the standard library's prelude".to_owned(),
						Res::Builtin(_) => "a built-in name".to_owned(),
						Res::Item(item) if item.is_crate_root() => format!("the name of the crate `{}`", ws.krate(item.krate()).name()),
						Res::Item(_) => continue,
					};

					let module = format!("`{}`", resolver.canonical_path(module));

					match shadowed.iter_mut().find(|(known, _)| *known == what) {
						Some((_, modules)) if !modules.contains(&module) => modules.push(module),
						Some(_) => {}
						None => shadowed.push((what, vec![module])),
					}
				}
			}
		}

		for (what, modules) in shadowed {
			let target = resolver.canonical_path(target);

			warnings.push(format!(
				"`{}` is {what}, which the renamed `{target}` shadows in {}",
				name.written,
				modules.join(", ")
			));
		}
	}

	warnings.dedup();
	warnings
}

/// Plans renaming the item(s) named by `path` to `new_name`, updating references across all loaded crates.
///
/// Every `cfg` variant is renamed, and so are the items of trait `impl`s implementing a renamed trait item (and the
/// trait item of a renamed implementation), and what a module that imports a renamed item binds to its name under
/// other `cfg`s instead (such as `struct Chain` in `#[cfg(a)] use crate::Chain; #[cfg(not(a))] struct Chain;`),
/// which [`Rename::warnings`] mentions.
///
/// `new_name` may be written with `r#`; keywords are written as raw identifiers (`type` becomes `r#type`).
///
/// Fails with [`Error::InvalidIdent`] for names that cannot name an item (`self`, `_`, `1a`, ...),
/// [`Error::NotFound`] when the path names nothing, [`Error::Unsupported`] for items that cannot be renamed (`impl`
/// blocks, imports, macro invocations, `extern crate` items, crate roots, and modules whose files cannot be moved), and
/// [`Error::Collision`] when the new name clashes with an existing name, unless forced.
pub fn rename(resolver: &Resolver<'_>, path: &ItemPath, new_name: &str, options: &RenameOptions) -> Result<Rename, Error> {
	let ws = resolver.workspace();
	let name = NewName::parse(new_name)?;
	let mut targets = resolver.resolve_item_path(path);

	if targets.is_empty() {
		return Err(Error::NotFound(path.to_string()));
	}

	// a path through private imports renames what they import, unless its module also binds the name otherwise
	super::narrow_private_imports(resolver, path, &mut targets);

	for &target in &targets {
		check_supported(resolver, target)?;
	}

	drop_shadowed(resolver, &mut targets);
	add_counterparts(resolver, &mut targets);

	// (twins of trait items have counterparts of their own)
	if add_twins(ws, &mut targets) {
		add_counterparts(resolver, &mut targets);
	}

	let mut variant_notes = Vec::new();

	if add_cfg_variants(resolver, &mut targets, &mut variant_notes) {
		add_twins(ws, &mut targets);
	}

	let mut renamed: Vec<String> = targets.iter().map(|&target| resolver.canonical_path(target).to_string()).collect();

	renamed.sort();
	renamed.dedup();

	if targets.iter().all(|&target| ws.item(target).name.as_deref() == Some(name.bare.as_str())) {
		return Ok(Rename {
			edits: EditSet::new(),
			renamed,
			references: Vec::new(),
			uncertain: Vec::new(),
			collisions: Vec::new(),
			warnings: vec![format!("`{path}` is already named `{}`; nothing to do", name.written)],
		});
	}

	let mut warnings = prelude_warnings(resolver, &targets, &name);

	warnings.extend(variant_notes);
	let moves = module_moves(resolver, &targets, &name, &mut warnings);

	check_moves(resolver, &targets, &moves)?;

	let mut collisions = collisions(resolver, &targets, &name, &moves);

	// (clashes with bindings of modules are known before searching)
	if !(collisions.is_empty() || options.force) {
		return Err(collision_error(ws, name, &collisions));
	}

	let (found, captures) = References::for_rename(resolver, &targets, &ReferenceOptions::all(), &name.bare);

	for (reference, module, binding) in captures {
		collisions.push(Collision {
			scope: resolver.canonical_path(module).to_string(),
			existing: format!("{binding} {}", name.written),
			file: reference.path,
			start: reference.start,
		});
	}

	if !(collisions.is_empty() || options.force) {
		return Err(collision_error(ws, name, &collisions));
	}

	let mut edits = EditSet::new();
	let mut references = Vec::new();
	let mut uncertain = Vec::new();

	warnings.extend(found.notes);

	for reference in found.references {
		if !is_renamed(&reference, &options.references) {
			uncertain.push(reference);
			continue;
		}

		edits.replace(ws.krate(reference.krate).file(reference.file), reference.range, name.written.clone());
		references.push(reference);
	}

	for (from, to) in moves {
		edits.move_path(from, to);
	}

	Ok(Rename {
		edits,
		renamed,
		references,
		uncertain,
		collisions,
		warnings,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::CrateId;
	use crate::source::FileId;

	#[test]
	fn uncertain_references_are_renamed_on_request() {
		let reference = |kind, certain| Reference {
			target: ItemId::crate_root(CrateId(0)),
			kind,
			krate: CrateId(0),
			file: FileId(0),
			path: PathBuf::new(),
			range: TextRange::new(0, 1),
			start: LineCol::default(),
			certain,
		};

		let none = ReferenceOptions::default();
		let all = ReferenceOptions::all();

		let certain = [
			(ReferenceKind::Definition, true),
			(ReferenceKind::Import, true),
			(ReferenceKind::Path, true),
			(ReferenceKind::MacroToken, true),
		];

		for (kind, certain) in certain {
			assert!(is_renamed(&reference(kind, certain), &none), "{kind:?}");
		}

		for (kind, certain) in [
			(ReferenceKind::MethodCall, false),
			(ReferenceKind::MacroToken, false),
			(ReferenceKind::DocLink, true),
			(ReferenceKind::Path, false),
		] {
			assert!(!is_renamed(&reference(kind, certain), &none), "{kind:?}");
			assert!(is_renamed(&reference(kind, certain), &all), "{kind:?}");
		}

		let method_calls = ReferenceOptions {
			method_calls: true,
			..ReferenceOptions::default()
		};

		assert!(is_renamed(&reference(ReferenceKind::Path, false), &method_calls));
		assert!(!is_renamed(&reference(ReferenceKind::MacroToken, false), &method_calls));
	}

	#[test]
	fn validates_new_names() {
		let written = |text: &str| NewName::parse(text).map(|name| (name.bare.to_string(), name.written));

		assert_eq!(written("Bar").unwrap(), ("Bar".to_owned(), "Bar".to_owned()));
		assert_eq!(written("type").unwrap(), ("type".to_owned(), "r#type".to_owned()));
		assert_eq!(written("r#type").unwrap(), ("type".to_owned(), "r#type".to_owned()));
		assert_eq!(written("r#plain").unwrap(), ("plain".to_owned(), "plain".to_owned()));
		assert_eq!(written("ünïcode").unwrap().1, "ünïcode");

		for invalid in [
			"", "1a", "a b", "a::b", "self", "r#self", "Self", "crate", "super", "_", "r#_", "r#", "-", "a-b",
		] {
			assert!(matches!(NewName::parse(invalid), Err(Error::InvalidIdent(_))), "{invalid:?}");
		}
	}
}
