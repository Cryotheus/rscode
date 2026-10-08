//! Replacing the source of items and inserting new items.
//!
//! The new source is parsed as items of the container it goes into (see [`parse`]) before anything is planned, so
//! errors point into the source as given. It is re-indented to its place, with the indentation style and line breaks
//! of the file.

pub(super) mod item;
mod parse;

use crate::Error;
use crate::edit::EditSet;
use crate::edit::describe;
use crate::edit::trivia;
use crate::edit::trivia::Placement;
use crate::model::ImportInfo;
use crate::model::ItemDetail;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::PathRef;
use crate::model::PathSegmentRef;
use crate::model::Workspace;
use crate::path::Anchor;
use crate::path::ItemPath;
use crate::resolve::Binding;
use crate::resolve::Namespace;
use crate::resolve::PathKind;
use crate::resolve::Res;
use crate::resolve::Resolver;
use crate::source::SourceFile;
use crate::source::TextRange;
use parse::Container;
use parse::NewBinding;
use parse::NewItem;
use parse::ParsedSource;
use parse::cfg_attributes;
use parse::parse_source;
use serde::Deserialize;
use serde::Serialize;
use smol_str::SmolStr;
use std::collections::HashSet;
use std::path::PathBuf;

/// The items to replace, and the imports named by the path by the `use` items replaced for them.
type UseItems = (Vec<ItemId>, Vec<(ItemId, ItemId)>);

/// Options for [`insert`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct InsertOptions {
	/// Where the new items go.
	pub position: InsertPosition,

	/// Insert even when an inserted item's name is already bound in the container.
	pub force: bool,
}

/// Where [`insert`] puts new items.
#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "path")]
pub enum InsertPosition {
	/// After the last item of the container.
	#[default]
	End,

	/// Before the first item of the container (after inner attributes and `//!` docs).
	Start,

	/// Before the named sibling item.
	Before(String),

	/// After the named sibling item.
	After(String),
}

/// The planned insertion.
#[derive(Debug, Clone, Serialize)]
pub struct Insertion {
	/// The edits, to preview or apply.
	#[serde(skip)]
	pub edits: EditSet,

	/// Kinds and names of the inserted items.
	pub inserted: Vec<(ItemKind, Option<String>)>,

	/// The names that inserted `use` items import (not `*` and `_`), with the index of the `use` item in
	/// [`Insertion::inserted`]: their imports are named by `use` paths.
	pub imports: Vec<(usize, String)>,

	/// The canonical path of the container the items go into (the parent, given or found from the anchor).
	pub parent: String,

	/// The edited file.
	pub file: PathBuf,

	/// Things to know about the insertion.
	pub warnings: Vec<String>,
}

/// Where an edited item is after an edit (see [`Replacement::spans`]).
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct ItemSpan {
	/// The canonical path of the item.
	pub path: String,

	/// The file with the item's text (a module's own file for out-of-line modules whose file was edited).
	pub file: PathBuf,

	/// The first line of the item (1-based).
	pub start: usize,

	/// The last line of the item.
	pub end: usize,
}

/// Options for [`replace`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ReplaceOptions {
	/// Allow the replacement to be a different kind of item, or several items.
	pub allow_kind_change: bool,

	/// Replace every `cfg` variant rather than failing when the path is ambiguous (see [`replaces_all_variants`]).
	/// Each keeps its own `cfg` (and `cfg_attr`) attributes, unless the new source has some.
	pub all_variants: bool,
}

/// The planned replacement.
#[derive(Debug, Clone, Serialize)]
pub struct Replacement {
	/// The edits, to preview or apply.
	#[serde(skip)]
	pub edits: EditSet,

	/// Canonical paths of the replaced items.
	pub replaced: Vec<String>,

	/// The edited files.
	pub files: Vec<PathBuf>,

	/// Where the edited items are after the edit, in the order of [`Replacement::replaced`]
	/// ([`edit_item`](crate::edit::edit_item) tells; [`replace`] does not).
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub spans: Vec<ItemSpan>,

	/// Things to know about the replacement.
	pub warnings: Vec<String>,

	/// What needs no attention, such as parts of an edit that change nothing.
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub notes: Vec<String>,
}

/// A container to insert items into.
#[derive(Debug, Clone, Copy)]
struct Target<'ws> {
	item: ItemId,
	container: Container,

	/// The file with the container's items (a module's own file for out-of-line modules).
	file: &'ws SourceFile,

	/// The text inside the container's braces, or the whole file.
	body: TextRange,
}

impl<'ws> Target<'ws> {
	/// The container `item`, which `parent` names (for messages).
	fn new(ws: &'ws Workspace, item: ItemId, parent: &str) -> Result<Self, Error> {
		let data = ws.item(item);
		let unsupported = |why: String| Error::Unsupported(format!("cannot insert into `{parent}`: {why}"));

		let (container, file, body) = match &data.detail {
			ItemDetail::Module(info) if info.inline => {
				let body = info.body.ok_or_else(|| unsupported("the module has no body".to_owned()))?;

				(Container::Module, ws.file_of(item), body)
			}

			ItemDetail::Module(info) => {
				if let Some(error) = &info.load_error {
					return Err(unsupported(format!("its file could not be loaded ({error})")));
				}

				let file = info.file.ok_or_else(|| unsupported("its file is not loaded".to_owned()))?;
				let file = ws.krate(item.krate()).file(file);

				(Container::Module, file, TextRange::new(0, file.text().len()))
			}

			ItemDetail::Impl(info) => (Container::Impl, ws.file_of(item), info.body),
			ItemDetail::Trait { body: Some(body), .. } if data.kind == ItemKind::Trait => (Container::Trait, ws.file_of(item), *body),

			_ => {
				return Err(unsupported(format!(
					"it is {}, and items can only be inserted into modules, `impl` blocks, and traits",
					article(data.kind)
				)));
			}
		};

		Ok(Self { item, container, file, body })
	}

	/// Whether two containers are the same text (of a file loaded by several crates).
	fn same_place(&self, other: &Self) -> bool {
		self.file.path() == other.file.path() && self.body == other.body
	}
}

/// Candidates for [`Error::Ambiguous`]; `use` items replaced for imports are described as the imports (`use` paths).
fn ambiguous(resolver: &Resolver<'_>, path: &ItemPath, items: &[ItemId], imports: &[(ItemId, ItemId)]) -> Error {
	Error::Ambiguous {
		path: path.to_string(),
		candidates: items.iter().map(|&item| describe(resolver, import_of(item, imports))).collect(),
	}
}

/// The container of the sibling that `anchor` names, for an insertion without a parent (see [`insert`]).
fn anchor_target<'ws>(resolver: &Resolver<'ws>, anchor: Option<&ItemPath>) -> Result<Target<'ws>, Error> {
	let Some(anchor) = anchor else {
		return Err(Error::Unsupported(
			"without a parent, the items go before or after a sibling (the anchor), whose container is the parent".to_owned(),
		));
	};
	let ws = resolver.workspace();
	let mut items = resolver.resolve_item_path(anchor);

	// like siblings: a path that names nothing may name imports (of items that are not loaded) as a `use` path
	if items.is_empty() && is_plain(anchor) {
		items = resolver.resolve_item_path(&ItemPath {
			import: true,
			..anchor.clone()
		});
	}

	let mut targets: Vec<Target<'ws>> = Vec::new();
	let mut unsupported = None;

	for &item in &items {
		let Some(container) = ws.parent(item_of_container(ws, item)) else {
			unsupported = Some(Error::Unsupported(format!("`{anchor}` is a crate root, which has no siblings")));
			continue;
		};

		match Target::new(ws, container, &resolver.canonical_path(container).to_string()) {
			Ok(target) if targets.iter().any(|known| known.same_place(&target)) => {}
			Ok(target) => targets.push(target),
			Err(error) => unsupported = unsupported.or(Some(error)),
		}
	}

	match targets.as_slice() {
		[] => Err(unsupported.unwrap_or_else(|| Error::NotFound(anchor.to_string()))),
		[target] => Ok(*target),

		// the containers are the candidates, since the parent is what is missing
		_ => Err(ambiguous(
			resolver,
			anchor,
			&targets.iter().map(|target| target.item).collect::<Vec<_>>(),
			&[],
		)),
	}
}

/// `a fn`, `an enum`, ...
fn article(kind: ItemKind) -> String {
	let name = kind.name();
	// (the kinds that start with `u`, `union` and `use`, take "a")
	let vowel = name.starts_with(['a', 'e', 'i', 'o']);

	format!("{} {name}", if vowel { "an" } else { "a" })
}

/// Descriptions of the associated items named `name` in the namespace that a new associated item would clash with:
/// those of the container, and for inherent `impl` blocks, those of the other inherent `impl`s of the type.
fn assoc_collisions(resolver: &Resolver<'_>, container: ItemId, name: &str, namespace: Namespace) -> Vec<String> {
	let ws = resolver.workspace();
	let mut containers = vec![container];

	if ws.item(container).impl_info().is_some_and(|info| info.trait_path.is_none()) {
		for self_type in resolver.impl_self_types(container) {
			containers.extend(
				resolver
					.impls_of(self_type)
					.into_iter()
					.filter(|&other| other != container && ws.item(other).impl_info().is_some_and(|info| info.trait_path.is_none())),
			);
		}
	}

	(containers.into_iter())
		.flat_map(|container| ws.children(container))
		.filter(|&item| ws.item(item).name.as_deref() == Some(name) && assoc_namespace(ws.item(item).kind) == Some(namespace))
		.map(|item| describe(resolver, item))
		.collect()
}

/// The namespace an associated item is in.
fn assoc_namespace(kind: ItemKind) -> Option<Namespace> {
	match kind {
		ItemKind::AssocFn | ItemKind::AssocConst => Some(Namespace::Value),
		ItemKind::AssocType => Some(Namespace::Type),
		_ => None,
	}
}

/// Whether an item of a container is named `name`, or (a `use` item, an `extern` block, or a `thread_local!`) holds an
/// import or item that binds it.
fn binds_name(ws: &Workspace, item: ItemId, name: &str) -> bool {
	let data = ws.item(item);

	match data.kind {
		ItemKind::Import => data.import_info().and_then(ImportInfo::binding_name).is_some_and(|bound| bound == name),
		ItemKind::Use | ItemKind::ExternBlock | ItemKind::MacroCall => ws.children(item).any(|child| binds_name(ws, child, name)),
		_ => data.name.as_deref() == Some(name),
	}
}

/// Fails unless the new items can replace an item of `kind`: one item of the same kind, or with `allow_kind_change`,
/// any items.
fn check_kinds(kind: ItemKind, path: &str, items: &[NewItem], container: Container, allow_kind_change: bool) -> Result<(), Error> {
	match items {
		[] => Err(Error::InvalidSource(format!("the source contains no {}", container.items()))),
		_ if allow_kind_change => Ok(()),
		[item] if item.kind == kind => Ok(()),

		[item] => Err(Error::InvalidSource(format!(
			"`{path}` is {}, but the source is {} (allow a kind change to replace it anyway)",
			article(kind),
			article(item.kind),
		))),

		items => Err(Error::InvalidSource(format!(
			"the source has {} items, but `{path}` can only be replaced by one {kind} (allow a kind change to replace \
			 it by several items)",
			items.len(),
		))),
	}
}

/// Whether code (not just a line comment) follows `offset` on its line.
fn code_follows(text: &str, offset: usize) -> bool {
	let rest = text.get(offset..).unwrap_or_default();
	let line = rest.split('\n').next().unwrap_or_default().trim();

	!line.is_empty() && !line.starts_with("//")
}

/// Existing bindings the new items would clash with: `(name, description of the existing binding)`.
fn collisions(resolver: &Resolver<'_>, target: &Target<'_>, items: &[NewItem]) -> Vec<(SmolStr, String)> {
	let mut collisions: Vec<(SmolStr, String)> = Vec::new();
	let mut seen: Vec<(&SmolStr, Namespace)> = Vec::new();

	for binding in items.iter().flat_map(|item| &item.bindings) {
		let namespaces = match target.container {
			Container::Module => module_namespaces(resolver, target.item, binding),
			_ => binding.namespaces.to_vec(),
		};

		// `macro_rules!` macros may shadow each other, and other macros are left to the compiler
		for namespace in namespaces.into_iter().filter(|&namespace| namespace != Namespace::Macro) {
			if seen.contains(&(&binding.name, namespace)) {
				push_unique(&mut collisions, (binding.name.clone(), "another item of the source".to_owned()));
			}

			seen.push((&binding.name, namespace));

			let existing = match target.container {
				Container::Module => module_collisions(resolver, target.item, &binding.name, namespace),
				_ => assoc_collisions(resolver, target.item, &binding.name, namespace),
			};

			for existing in existing {
				push_unique(&mut collisions, (binding.name.clone(), existing));
			}
		}
	}

	collisions
}

/// A binding for messages: the item, or the import and what it imports.
fn describe_binding(resolver: &Resolver<'_>, binding: &Binding) -> String {
	let ws = resolver.workspace();

	let target = match &binding.res {
		Res::Item(item) => format!("`{}`", resolver.canonical_path(*item)),
		Res::External(path) | Res::Builtin(path) => format!("`{path}`"),
	};

	match binding.import {
		Some(import) => {
			let file = ws.file_of(import);
			let start = file.line_col(ws.item(import).range.start);

			format!("the import of {target} at {}:{start}", ws.display_path(file.path()).display())
		}

		None => match &binding.res {
			Res::Item(item) => describe(resolver, *item),
			_ => target,
		},
	}
}

/// Items at distinct places: an item of a file that several crates load counts once.
fn distinct_places(ws: &Workspace, items: Vec<ItemId>) -> Vec<ItemId> {
	let mut seen = HashSet::new();

	items
		.into_iter()
		.filter(|&item| seen.insert((ws.file_of(item).path().to_path_buf(), ws.item(item).range)))
		.collect()
}

/// The item that replacing `item` replaces for the path: the import, when `item` is its `use` item.
fn import_of(item: ItemId, imports: &[(ItemId, ItemId)]) -> ItemId {
	imports.iter().find(|(use_item, _)| *use_item == item).map_or(item, |&(_, import)| import)
}

/// Plans inserting the items in `source` into the container named by `parent`: a module (`crate` for the crate
/// root), an `impl` block (`<Type as Trait>` / `<Type>`), a `trait`, or an `extern` block's module.
/// Items must be valid for the container (e.g. associated items for `impl`s).
///
/// The sibling of [`InsertPosition::Before`] and [`InsertPosition::After`] is a path resolved like `parent`, or the
/// name of an item of the container. Without a `parent`, the position must be before or after a sibling, whose path
/// must name items of one container: that container is the parent (an `impl` block or trait for an associated item,
/// the module for other items). New items are separated from their neighbors by blank lines and indented like the
/// container's items. Fails with [`Error::Collision`] when a new name is already bound in the container (in a module:
/// by an item or a named import in the same namespace; in an `impl` block: by an associated item), unless
/// [`InsertOptions::force`].
pub fn insert(resolver: &Resolver<'_>, parent: Option<&ItemPath>, source: &str, options: &InsertOptions) -> Result<Insertion, Error> {
	let anchor = match &options.position {
		InsertPosition::Before(anchor) | InsertPosition::After(anchor) => Some(ItemPath::parse(anchor)?),
		InsertPosition::End | InsertPosition::Start => None,
	};

	let target = match parent {
		Some(parent) => target(resolver, parent, anchor.as_ref())?,
		None => anchor_target(resolver, anchor.as_ref())?,
	};
	let canonical = resolver.canonical_path(target.item).to_string();
	let parent = parent.map_or_else(|| canonical.clone(), ToString::to_string);
	let parsed = parse_source(source, target.container)?;

	if parsed.items.is_empty() {
		return Err(Error::InvalidSource(format!("the source contains no {}", target.container.items())));
	}

	let collisions = collisions(resolver, &target, &parsed.items);
	let mut warnings = Vec::new();

	if !collisions.is_empty() {
		if !options.force {
			let mut names: Vec<&str> = collisions.iter().map(|(name, _)| name.as_str()).collect();

			names.dedup();

			return Err(Error::Collision {
				name: names.join("`, `"),
				collisions: collisions.iter().map(|(name, existing)| format!("`{name}`: {existing}")).collect(),
			});
		}

		warnings.extend(
			collisions
				.iter()
				.map(|(name, existing)| format!("inserted `{name}`, which collides with {existing}")),
		);
	}

	warnings.extend(missing_trait_items(resolver, &target, &parsed.items));

	let placement = placement(resolver, &target, &parent, &options.position, anchor.as_ref())?;
	let text = target.file.text();
	let indent = trivia::body_indent(text, target.body);
	let edit = trivia::insertion(text, placement, &parsed.text, &indent);
	let mut edits = EditSet::new();

	edits.replace(target.file, edit.range, edit.replacement);

	Ok(Insertion {
		edits,
		inserted: parsed
			.items
			.iter()
			.map(|item| (item.kind, item.name.as_ref().map(ToString::to_string)))
			.collect(),
		imports: (parsed.items.iter().enumerate())
			.filter(|(_, item)| item.kind == ItemKind::Use)
			.flat_map(|(index, item)| item.bindings.iter().map(move |binding| (index, binding.name.to_string())))
			.collect(),
		parent: canonical,
		file: target.file.path().to_path_buf(),
		warnings,
	})
}

/// Whether a path is a plain path, which may name items by their name, or imports as a `use` path: not a `use` path, a
/// qualified path, a field, or macro invocations.
fn is_plain(path: &ItemPath) -> bool {
	!path.import && path.qualifier.is_none() && path.field.is_none() && !path.macro_call
}

/// The item an anchor stands for among the items of its container: the item, or for an import its `use` item, and for
/// an item of an `extern` block or a static of a `thread_local!` the block or invocation (they are transparent).
fn item_of_container(ws: &Workspace, mut item: ItemId) -> ItemId {
	while let Some(parent) = ws.parent(item)
		&& matches!(ws.item(parent).kind, ItemKind::Use | ItemKind::ExternBlock | ItemKind::MacroCall)
	{
		item = parent;
	}

	item
}

/// Warnings for items inserted into a trait `impl` that the (loaded) trait does not have.
fn missing_trait_items(resolver: &Resolver<'_>, target: &Target<'_>, items: &[NewItem]) -> Vec<String> {
	let ws = resolver.workspace();
	let mut warnings = Vec::new();

	if target.container != Container::Impl {
		return warnings;
	}

	for trait_item in resolver.impl_traits(target.item) {
		for name in items.iter().filter_map(|item| item.name.as_ref()) {
			if !ws.children(trait_item).any(|child| ws.item(child).name.as_ref() == Some(name)) {
				warnings.push(format!("the trait `{}` has no item named `{name}`", resolver.canonical_path(trait_item)));
			}
		}
	}

	warnings
}

/// Descriptions of the bindings of `name` in a module that a new item or named import would clash with (glob imports
/// are shadowed instead).
fn module_collisions(resolver: &Resolver<'_>, module: ItemId, name: &str, namespace: Namespace) -> Vec<String> {
	(resolver.bindings(module, name, namespace).iter())
		.filter(|binding| !binding.glob)
		.map(|binding| describe_binding(resolver, binding))
		.collect()
}

/// The namespaces a new binding of a module takes: for imports, those their path resolves in (all of them when it
/// resolves to nothing).
fn module_namespaces(resolver: &Resolver<'_>, module: ItemId, binding: &NewBinding) -> Vec<Namespace> {
	let Some(import) = &binding.import else {
		return binding.namespaces.to_vec();
	};

	let path = PathRef {
		leading_colon: import.leading_colon,
		segments: (import.segments.iter())
			.map(|name| PathSegmentRef {
				name: name.clone(),
				range: TextRange::default(),
				has_arguments: false,
			})
			.collect(),
	};

	let resolved: Vec<Namespace> = (binding.namespaces.iter().copied())
		.filter(|&namespace| {
			let prefixes = resolver.resolve_prefixes(module, &path, Some(namespace), PathKind::Use);

			prefixes.last().is_some_and(|last| !last.is_empty())
		})
		.collect();

	if resolved.is_empty() { binding.namespaces.to_vec() } else { resolved }
}

/// Where the new items go.
fn placement(
	resolver: &Resolver<'_>,
	target: &Target<'_>,
	parent: &str,
	position: &InsertPosition,
	anchor: Option<&ItemPath>,
) -> Result<Placement, Error> {
	let ws = resolver.workspace();

	let children = || ws.children(target.item).map(|child| ws.item(child).range);

	// next to the first or last item (keeping comments that are not attached to them before or after the new items),
	// or into an empty container
	let anchor = match (position, anchor) {
		(InsertPosition::Start, _) => {
			let first = children().min_by_key(|range| range.start);

			return Ok(first.map_or(Placement::End(target.body), Placement::Before));
		}

		(InsertPosition::End, _) | (_, None) => {
			let last = children().max_by_key(|range| range.start);

			return Ok(last.map_or(Placement::End(target.body), Placement::After));
		}

		(_, Some(anchor)) => anchor,
	};

	let siblings = siblings(resolver, target, anchor);

	let (Some(first), Some(last)) = (siblings.first(), siblings.last()) else {
		return Err(match resolver.resolve_item_path(anchor).is_empty() {
			true => Error::NotFound(format!("{anchor}` in `{parent}")),
			false => Error::Unsupported(format!("`{anchor}` is not an item of `{parent}`")),
		});
	};

	Ok(match position {
		InsertPosition::After(_) => Placement::After(ws.item(*last).range),
		_ => Placement::Before(ws.item(*first).range),
	})
}

fn push_unique<T: PartialEq>(list: &mut Vec<T>, value: T) {
	if !list.contains(&value) {
		list.push(value);
	}
}

/// Plans replacing the source text of the item named by `path` (including its attributes and doc comments) with
/// `source`, which must parse as an item of the same kind (see [`ReplaceOptions::allow_kind_change`]).
/// The new text is re-indented to the item's indentation. With [`ReplaceOptions::all_variants`], every `cfg` variant
/// keeps its own `cfg` and `cfg_attr` attributes, unless `source` has some.
///
/// Comments attached above the item stay, and the replacement gets the line breaks of the file. Fails with
/// [`Error::NotFound`] when `path` names nothing, [`Error::Ambiguous`] when it names several items (`cfg` variants,
/// see [`ReplaceOptions::all_variants`]; items of `impl` blocks whose headers differ, which generic arguments in the
/// path tell apart; or items of one crate with the same `cfg`s, like `impl` blocks with the same header, which a
/// selector tells apart), and [`Error::InvalidSource`] when `source` is not a valid replacement.
pub fn replace(resolver: &Resolver<'_>, path: &ItemPath, source: &str, options: &ReplaceOptions) -> Result<Replacement, Error> {
	let ws = resolver.workspace();
	let mut resolved = resolver.resolve_item_path(path);

	super::check_private_imports(resolver, path, &mut resolved)?;

	let (items, imports) = use_items(resolver, resolved)?;
	let items = distinct_places(ws, items);

	if items.is_empty() {
		return Err(Error::NotFound(path.to_string()));
	}

	// (`impl` blocks whose headers differ are not `cfg` variants of each other, and neither are items of one crate with
	// the same `cfg`s, such as two `impl` blocks with the same header)
	if items.len() > 1 && (!options.all_variants || super::impl_headers_differ(ws, &items) || super::same_cfg_items(ws, &items)) {
		return Err(ambiguous(resolver, path, &items, &imports));
	}

	let mut plan = Replacement {
		edits: EditSet::new(),
		replaced: Vec::new(),
		files: Vec::new(),
		spans: Vec::new(),
		warnings: Vec::new(),
		notes: Vec::new(),
	};

	// parsed once per container kind (`cfg` variants may be in different containers)
	let mut parsed: Vec<(Container, ParsedSource)> = Vec::new();

	for &item in &items {
		let data = ws.item(item);
		let canonical = resolver.canonical_path(import_of(item, &imports)).to_string();
		let container = ws
			.parent(item)
			.and_then(|parent| Container::of_item(ws, parent))
			.ok_or_else(|| Error::Unsupported(format!("`{path}` is a crate root, which cannot be replaced")))?;

		let source = match parsed.iter().position(|(known, _)| *known == container) {
			Some(index) => &parsed[index].1,

			None => {
				parsed.push((container, parse_source(source, container)?));
				&parsed[parsed.len() - 1].1
			}
		};

		check_kinds(data.kind, &canonical, &source.items, container, options.allow_kind_change)?;

		let file = ws.file_of(item);
		let text = file.text();
		let indent = trivia::line_indent(text, data.range.start);
		let mut replacement = trivia::reindent(&source.text, indent, text);
		let mut range = data.range;

		// every variant keeps its own `cfg`, unless the source has one (so they do not all become unconditional)
		if items.len() > 1 && !source.has_cfg {
			let attributes = text.get(data.range.start..data.attrs.after_attrs).unwrap_or_default();
			let kept: String = (cfg_attributes(attributes).into_iter())
				.map(|attribute| format!("{attribute}{}{indent}", trivia::line_ending(text)))
				.collect();

			replacement.insert_str(0, &kept);
		}

		// the declarations of a `thread_local!` are separated by `;`, which only the last one may leave out (and the
		// file still parses without it, since macro bodies are only tokens); entries keep a `;` they had
		let followed = ws.parent(item).is_some_and(|parent| ws.children(parent).last() != Some(item));
		let separated = match container {
			Container::ThreadLocal => followed,
			Container::Entries => ws.item_text(item).ends_with(';'),
			_ => false,
		};

		if separated && !source.ends_with_semicolon {
			if source.trailing_comment {
				let why = match container {
					Container::ThreadLocal => "is followed by more declarations of its `thread_local!`",
					_ => "ends with `;` in its macro invocation",
				};

				return Err(Error::InvalidSource(format!("`{path}` {why}: end the source with `;`")));
			}

			let code = replacement.trim_end().len();

			replacement.insert(code, ';');
		}

		// a comment ending the replacement would comment out the code after the item on its line: move that code to
		// the next line
		if source.trailing_comment && code_follows(text, range.end) {
			let rest = &text[range.end..];

			range.end += rest.len() - rest.trim_start_matches([' ', '\t']).len();
			replacement.push_str(trivia::line_ending(text));
			replacement.push_str(indent);
		}

		plan.edits.replace(file, range, replacement);
		plan.warnings.extend(replacement_warnings(ws, item, &canonical, &source.items));
		push_unique(&mut plan.replaced, canonical);
		push_unique(&mut plan.files, file.path().to_path_buf());
	}

	if items.len() > 1 {
		let cfgs = match parsed.iter().any(|(_, source)| source.has_cfg) {
			true => "the `cfg`s of the source",
			false => "their own `cfg`s",
		};

		plan.warnings.push(format!(
			"replaced {} `cfg` variants of `{path}` with the same source, with {cfgs}",
			items.len()
		));
	}

	Ok(plan)
}

/// Warnings about what replacing `item` by `items` leaves as it is.
fn replacement_warnings(ws: &Workspace, item: ItemId, path: &str, items: &[NewItem]) -> Vec<String> {
	let data = ws.item(item);
	let mut warnings = Vec::new();

	if let Some(info) = data.module_info().filter(|info| !info.inline) {
		let file = info
			.file
			.map(|file| ws.krate(item.krate()).file(file).path().to_path_buf())
			.or_else(|| info.file_path.clone());

		let file = file.map(|file| format!(" `{}`", ws.display_path(&file).display())).unwrap_or_default();

		warnings.push(format!(
			"only the declaration of the module `{path}` is replaced; its file{file} is left as it is"
		));
	}

	let defines = |item: &NewItem, name: &SmolStr| {
		item.name.as_ref() == Some(name) || item.bindings.iter().any(|binding| binding.import.is_none() && binding.name == *name)
	};

	// (fields of tuple structs and variants are named by their place, which a replacement keeps)
	if let Some(name) = data.name.as_ref().filter(|_| data.kind != ItemKind::Field || data.name_range.is_some())
		&& !items.iter().any(|item| defines(item, name))
	{
		warnings.push(format!(
			"the replacement of `{path}` does not define `{name}`; references to it are not updated"
		));
	}

	// a `use` item replaced for its imports: the names they bound
	if data.kind == ItemKind::Use {
		let imported = |name: &SmolStr| items.iter().flat_map(|item| &item.bindings).any(|binding| binding.name == *name);

		for leaf in ws.children(item) {
			if let Some(name) = ws.item(leaf).import_info().and_then(ImportInfo::binding_name)
				&& !imported(name)
			{
				warnings.push(format!(
					"the replacement of `{path}` does not import `{name}`; code that uses `{name}` through it is not updated"
				));
			}
		}
	}

	warnings
}

/// Whether [`ReplaceOptions::all_variants`] lets [`replace`] replace the several items that `path` names, rather than
/// fail: whether they are `cfg` variants (or items of several crates). Items of `impl` blocks whose headers differ
/// (which generic arguments in the path tell apart) are not, and neither are two items of one crate with the same
/// `cfg`s (such as two `impl` blocks with the same header, which a selector tells apart).
pub fn replaces_all_variants(resolver: &Resolver<'_>, path: &ItemPath) -> bool {
	let ws = resolver.workspace();
	let mut resolved = resolver.resolve_item_path(path);

	if super::check_private_imports(resolver, path, &mut resolved).is_err() {
		return false;
	}

	let Ok((items, _)) = use_items(resolver, resolved) else {
		return false;
	};
	let items = distinct_places(ws, items);

	items.len() > 1 && !super::impl_headers_differ(ws, &items) && !super::same_cfg_items(ws, &items)
}

/// Whether two items are the same text (of a file loaded by several crates).
fn same_place(ws: &Workspace, a: ItemId, b: ItemId) -> bool {
	a == b || (ws.item(a).range == ws.item(b).range && ws.file_of(a).path() == ws.file_of(b).path())
}

/// The items of the container named by `anchor`, in source order: those that bind that name when it is a single
/// identifier, else those it resolves to. An import stands for its `use` item, and an item of an `extern` block or a
/// static of a `thread_local!` for the block or invocation.
fn siblings(resolver: &Resolver<'_>, target: &Target<'_>, anchor: &ItemPath) -> Vec<ItemId> {
	let ws = resolver.workspace();
	let children: Vec<ItemId> = ws.children(target.item).collect();

	if let (Anchor::None, None, false, [name]) = (anchor.anchor, &anchor.qualifier, anchor.import, anchor.segments.as_slice())
		&& is_plain(anchor)
	{
		let named: Vec<ItemId> = children.iter().copied().filter(|&child| binds_name(ws, child, name)).collect();

		if !named.is_empty() {
			return named;
		}
	}

	// the resolved items may be those of another crate that loads the same file; a path that names nothing may name
	// imports (of items that are not loaded) as a `use` path
	let mut resolved = resolver.resolve_item_path(anchor);

	if resolved.is_empty() && is_plain(anchor) {
		resolved = resolver.resolve_item_path(&ItemPath {
			import: true,
			..anchor.clone()
		});
	}

	let mut siblings: Vec<ItemId> = resolved
		.into_iter()
		.map(|item| item_of_container(ws, item))
		.filter_map(|item| children.iter().copied().find(|&child| same_place(ws, child, item)))
		.collect();

	siblings.sort_by_key(|&item| ws.item(item).range.start);
	siblings.dedup();
	siblings
}

/// The container named by `parent`. When it names several, a sibling named by `anchor` may tell them apart.
fn target<'ws>(resolver: &Resolver<'ws>, parent: &ItemPath, anchor: Option<&ItemPath>) -> Result<Target<'ws>, Error> {
	let ws = resolver.workspace();
	let items = resolver.resolve_item_path(parent);
	let mut targets: Vec<Target<'ws>> = Vec::new();
	let mut unsupported = None;
	let parent_text = parent.to_string();

	for &item in &items {
		match Target::new(ws, item, &parent_text) {
			Ok(target) if targets.iter().any(|known| known.same_place(&target)) => {}
			Ok(target) => targets.push(target),
			Err(error) => unsupported = unsupported.or(Some(error)),
		}
	}

	if targets.len() > 1
		&& let Some(anchor) = anchor
	{
		let with_anchor: Vec<Target<'ws>> = targets
			.iter()
			.copied()
			.filter(|target| !siblings(resolver, target, anchor).is_empty())
			.collect();

		if with_anchor.len() == 1 {
			targets = with_anchor;
		}
	}

	match targets.as_slice() {
		[] => Err(unsupported.unwrap_or_else(|| Error::NotFound(parent.to_string()))),
		[target] => Ok(*target),

		_ => Err(ambiguous(
			resolver,
			parent,
			&targets.iter().map(|target| target.item).collect::<Vec<_>>(),
			&[],
		)),
	}
}

/// Imports are replaced as their `use` items, which must import nothing that the path does not name.
fn use_items(resolver: &Resolver<'_>, items: Vec<ItemId>) -> Result<UseItems, Error> {
	let ws = resolver.workspace();
	let mut replaced = Vec::with_capacity(items.len());
	let mut imports = Vec::new();

	for &item in &items {
		if ws.item(item).kind != ItemKind::Import {
			replaced.push(item);
			continue;
		}

		let Some(use_item) = ws.parent(item) else {
			continue;
		};
		let leaves = ws.children(use_item).count();

		if ws.children(use_item).any(|leaf| !items.contains(&leaf)) {
			let file = ws.file_of(use_item);

			return Err(Error::Unsupported(format!(
				"`{}` is one of the {leaves} imports of the `use` item at {}:{}, which only replaces as a whole: \
				 insert a new `use` item and remove this import instead",
				resolver.canonical_path(item),
				ws.display_path(file.path()).display(),
				file.line_col(ws.item(use_item).range.start),
			)));
		}

		imports.push((use_item, item));
		replaced.push(use_item);
	}

	Ok((replaced, imports))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test_registry::locked_version;
	use crate::test_registry::registry_crate;

	/// Parsing the text of every `step`th item of a crate as a replacement tells the kind and name the loader gave it.
	fn check_classification(name: &str, root: PathBuf, step: usize) {
		let mut ws = Workspace::new(root.parent().unwrap());
		let krate = ws.load_crate(crate::CrateSpec::new(name, root));
		let mut checked = 0;

		for (index, (item, data)) in ws.krate(krate).items().enumerate() {
			let container = ws.parent(item).and_then(|parent| Container::of(ws.item(parent).kind));

			let Some(container) = container.filter(|_| data.kind != ItemKind::Import && index % step == 0) else {
				continue;
			};

			let text = ws.item_text(item);
			let parsed = parse_source(text, container).unwrap_or_else(|error| panic!("{name}: {error}\n{text}"));
			let found: Vec<(ItemKind, Option<&str>)> = parsed.items.iter().map(|new| (new.kind, new.name.as_deref())).collect();

			// (the fields of tuple structs are named by their place)
			let expected = data.name().filter(|_| data.kind != ItemKind::Field || data.name_range.is_some());

			assert_eq!(found, [(data.kind, expected)], "{name}: {text}");
			checked += 1;
		}

		assert!(checked > 100, "{name}: only {checked} items");
	}

	#[test]
	fn checks_kinds() {
		let item = |kind| NewItem {
			kind,
			name: Some("x".into()),
			bindings: Vec::new(),
		};
		let check = |kind, items: &[NewItem], allow| check_kinds(kind, "c::x", items, Container::Module, allow);

		assert!(check(ItemKind::Fn, &[item(ItemKind::Fn)], false).is_ok());
		assert!(check(ItemKind::Fn, &[item(ItemKind::Struct)], true).is_ok());
		assert!(check(ItemKind::Fn, &[item(ItemKind::Fn), item(ItemKind::Fn)], true).is_ok());

		match check(ItemKind::Fn, &[item(ItemKind::Struct)], false) {
			Err(Error::InvalidSource(message)) => {
				assert_eq!(
					message,
					"`c::x` is a fn, but the source is a struct (allow a kind change to replace it anyway)"
				)
			}

			other => panic!("{other:?}"),
		}

		match check(ItemKind::Fn, &[item(ItemKind::Fn), item(ItemKind::Fn)], false) {
			Err(Error::InvalidSource(message)) => assert!(message.starts_with("the source has 2 items"), "{message}"),
			other => panic!("{other:?}"),
		}

		match check(ItemKind::Fn, &[], true) {
			Err(Error::InvalidSource(message)) => assert_eq!(message, "the source contains no module items"),
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn classifies_items_like_the_loader() {
		check_classification("rscode", PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"), 1);

		match registry_crate("syn") {
			Some(syn) => check_classification("syn", syn.join("src/lib.rs"), 7),
			None => eprintln!("syn {} is not in cargo's registry; skipping", locked_version("syn")),
		}
	}

	#[test]
	fn names_kinds_with_articles() {
		assert_eq!(article(ItemKind::Fn), "a fn");
		assert_eq!(article(ItemKind::Enum), "an enum");
		assert_eq!(article(ItemKind::AssocFn), "an assoc-fn");
		assert_eq!(article(ItemKind::Impl), "an impl");
		assert_eq!(article(ItemKind::Use), "a use");
		assert_eq!(article(ItemKind::Union), "a union");
	}

	#[test]
	fn tells_whether_code_follows() {
		let text = "fn a() {} // c\nfn b() {} struct C;\nfn d() {}";

		assert!(!code_follows(text, text.find(" //").unwrap()));
		assert!(code_follows(text, text.find(" struct").unwrap()));
		assert!(!code_follows(text, text.len()));
		assert!(!code_follows(text, text.find("\nfn b").unwrap()));
	}
}
