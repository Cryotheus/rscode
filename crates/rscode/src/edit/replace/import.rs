//! Adding imports to a module: as new `use` items where sorting puts them, or merged into the `use` items there,
//! following the module's import granularity.

use super::article;
use super::collisions;
use super::order;
use super::order::Siblings;
use super::parse::Container;
use super::parse::parse_source;
use super::target;
use crate::Error;
use crate::edit::EditSet;
use crate::edit::TextEdit;
use crate::edit::describe;
use crate::edit::trivia;
use crate::edit::trivia::Placement;
use crate::edit::trivia::Spacing;
use crate::model::ImportInfo;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::PathRef;
use crate::model::PathSegmentRef;
use crate::model::Visibility;
use crate::model::Workspace;
use crate::path::ItemPath;
use crate::resolve::Binding;
use crate::resolve::Namespace;
use crate::resolve::PathKind;
use crate::resolve::Res;
use crate::resolve::Resolver;
use crate::resolve::Viewpoint;
use crate::source::SourceFile;
use crate::source::TextRange;
use rscode_sort::ItemOrder;
use rscode_sort::StyleEdition;
use serde::Deserialize;
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::PathBuf;
use syn::UseTree;
use syn::ext::IdentExt;
use syn::spanned::Spanned;

/// What happened to an import (see [`ImportAddition::imports`]).
#[derive(Debug, Clone, Serialize)]
pub struct AddedImport {
	/// The import as a path: `std::fs`, `a::B as C`, `m::*`, or `m::{self}` (after its visibility, for re-exports).
	pub path: String,

	/// How it was added.
	pub outcome: ImportOutcome,

	/// The line of the `use` item that imports it (for [`ImportOutcome::InScope`], of the item that has its name, or 0
	/// for a prelude), once the edits are applied (1-based).
	pub line: usize,
}

/// What a module binds already of what an import imports, under the name the import binds (see [`bound_already`]).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Bound {
	/// An import of this `use` item.
	Import(ItemId),

	/// This item of the module (or `extern crate` item).
	Item(ItemId),

	/// A prelude.
	Prelude,
}

/// What the edits of [`add_imports`] are planned with.
#[derive(Clone, Copy)]
struct Context<'a, 'ws> {
	target: &'a super::Target<'ws>,
	leaves: &'a [Leaf],
	uses: &'a [UseItem],
	siblings: &'a Siblings,
	style_edition: StyleEdition,
}

/// The import granularity of a module, as its `use` items show it.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Granularity {
	/// One import per `use` item (or no `use` items).
	Item,

	/// One `use` item per module imported from.
	Module,

	/// Imports of several modules per `use` item.
	Crate,
}

/// The planned imports.
#[derive(Debug, Clone, Serialize)]
pub struct ImportAddition {
	/// The edits, to preview or apply.
	#[serde(skip)]
	pub edits: EditSet,

	/// Each import (each leaf of the given `use` trees), in the order given.
	pub imports: Vec<AddedImport>,

	/// The file of the module.
	pub file: PathBuf,

	/// Things to know about the imports.
	pub warnings: Vec<String>,
}

/// Options for [`add_imports`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ImportOptions {
	/// Import even when a name is already bound in the module (by an item or a named import).
	pub force: bool,
}

/// How an import was added.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportOutcome {
	/// In a new `use` item.
	Added,

	/// Merged into a `use` item, whose new text this is.
	Merged(String),

	/// The module imports it already: nothing changes.
	Present,

	/// The module has it in scope already, other than by an import: a bare name (or a path) that names an item of the
	/// module, or of a prelude. Nothing changes; the line is the item's (0 for a prelude).
	InScope,
}

/// The last part of a [`Leaf`].
#[derive(Debug, Clone, Eq, PartialEq)]
enum Last {
	/// `name`, or `name as alias` (`_` too).
	Name(String, Option<String>),

	/// `self` (or `self as alias`) in a group: the module the segments name.
	SelfModule(Option<String>),

	/// `*`
	Glob,
}

/// One import: a leaf of a `use` tree, with the path to it.
#[derive(Debug, Clone, Eq, PartialEq)]
struct Leaf {
	/// The visibility, as written (empty for private imports).
	vis: String,

	leading_colon: bool,

	/// The segments before the last part, as written (with `r#`).
	segments: Vec<String>,

	last: Last,
}

impl Leaf {
	/// The name the import binds: its alias, or the last segment of its path (`None` for globs and `_` imports).
	fn binding_name(&self) -> Option<&str> {
		match &self.last {
			Last::Name(_, Some(alias)) | Last::SelfModule(Some(alias)) if unraw(alias) == "_" => None,
			Last::Name(_, Some(alias)) | Last::SelfModule(Some(alias)) => Some(unraw(alias)),
			Last::Name(name, None) => Some(unraw(name)),
			Last::SelfModule(None) => self.segments.last().map(|segment| unraw(segment)),
			Last::Glob => None,
		}
	}

	/// Whether the import is a name without a path (like `Circle`, or `Circle as C`).
	fn is_bare(&self) -> bool {
		self.segments.is_empty() && !self.leading_colon && matches!(self.last, Last::Name(..))
	}

	/// Whether `info` (an import of a `use` item with the visibility `vis`) imports the same: by the same path, name,
	/// and visibility (`a::b` imports the module `b` like `a::b::{self}` does).
	fn is_imported_by(&self, info: &ImportInfo, vis: &Visibility) -> bool {
		let same_alias = |alias: &Option<String>| alias.as_deref().map(unraw) == info.alias.as_deref();
		let last_matches = match &self.last {
			Last::Name(_, alias) | Last::SelfModule(alias) => !info.glob && same_alias(alias),
			Last::Glob => info.glob,
		};

		last_matches
			&& info.path.leading_colon == self.leading_colon
			&& info.path.names().eq(self.names())
			&& compact(&vis_text(vis)) == compact(&self.vis)
	}

	/// The `use` item of the leaf alone: `use std::fs;`.
	fn item(&self) -> String {
		match self.vis.as_str() {
			"" => format!("use {};", self.path()),
			vis => format!("{vis} use {};", self.path()),
		}
	}

	/// The module the import is from: the path without the imported name (for `self` and globs, the whole path).
	fn module(&self) -> &[String] {
		&self.segments
	}

	/// The names of its path, unraw'd: the segments, and the imported name unless it is `self` or a glob.
	fn names(&self) -> Vec<&str> {
		let mut names: Vec<&str> = self.segments.iter().map(|segment| unraw(segment)).collect();

		if let Last::Name(name, _) = &self.last {
			names.push(unraw(name));
		}

		names
	}

	/// Whether the leaf imports `name` without renaming it, as the last segment of its path.
	fn names_module(&self, name: &syn::Ident) -> bool {
		matches!(&self.last, Last::Name(last, None) if name.unraw() == unraw(last))
	}

	/// The namespace the path of the import is looked up in: the type namespace for modules (`self`) and the modules
	/// of globs, every namespace (`None`) otherwise.
	fn namespace(&self) -> Option<Namespace> {
		match self.last {
			Last::Name(..) => None,
			Last::SelfModule(_) | Last::Glob => Some(Namespace::Type),
		}
	}

	/// The path of the import (with `::` in front when written so).
	fn path(&self) -> String {
		let colon = if self.leading_colon { "::" } else { "" };

		format!("{colon}{}", self.tree(0))
	}

	/// The path that the import names (for a glob, the module it imports from), to resolve.
	fn path_ref(&self) -> PathRef {
		PathRef {
			leading_colon: self.leading_colon,
			segments: (self.names().into_iter())
				.map(|name| PathSegmentRef {
					name: name.into(),
					range: TextRange::default(),
					has_arguments: false,
				})
				.collect(),
		}
	}

	/// The first `count` segments of its path (with the imported name), as written: `crate::a`.
	fn prefix(&self, count: usize) -> String {
		let colon = if self.leading_colon { "::" } else { "" };
		let mut segments: Vec<&str> = self.segments.iter().map(String::as_str).collect();

		if let Last::Name(name, _) = &self.last {
			segments.push(name);
		}

		format!("{colon}{}", segments[..count.min(segments.len())].join("::"))
	}

	/// The leaf as a `use` tree after its first `skip` segments: `io::Write`, `Write as W`, `*`, or `self`.
	fn tree(&self, skip: usize) -> String {
		let last = match &self.last {
			Last::Name(name, None) => name.clone(),
			Last::Name(name, Some(alias)) => format!("{name} as {alias}"),
			Last::SelfModule(None) if skip < self.segments.len() => "{self}".to_owned(),
			Last::SelfModule(None) => "self".to_owned(),
			Last::SelfModule(Some(alias)) if skip < self.segments.len() => format!("{{self as {alias}}}"),
			Last::SelfModule(Some(alias)) => format!("self as {alias}"),
			Last::Glob => "*".to_owned(),
		};

		let mut parts: Vec<&str> = self.segments.iter().skip(skip).map(String::as_str).collect();

		parts.push(&last);
		parts.join("::")
	}
}

/// Merging an import into the tree of a `use` item (on the parsing thread: spans are relative to `item`).
struct Merging<'a> {
	item: &'a str,
	style_edition: StyleEdition,
	line_ending: &'a str,
}

impl Merging<'_> {
	/// The edit inserting `new` (a tree) into `group`, before the first element that sorts after it.
	fn insert(&self, group: &syn::UseGroup, new: &str) -> Option<(Range<usize>, String)> {
		let tree: UseTree = syn::parse_str(new).ok()?;
		let range = group.span().byte_range();
		let multi_line = self.item[range.clone()].contains('\n');
		let le = self.line_ending;
		let sorts_after = |element: &&UseTree| {
			rscode_sort::use_tree_cmp(element, &tree, self.style_edition) == Ordering::Greater
		};
		let after = group.items.iter().find(sorts_after);

		if let Some(element) = after {
			let at = element.span().byte_range().start;
			let indent = trivia::line_indent(self.item, at);
			let separator = if multi_line { format!(",{le}{indent}") } else { ", ".to_owned() };

			return Some((at..at, format!("{new}{separator}")));
		}

		let Some(last) = group.items.pairs().next_back() else {
			// `{}`
			let at = range.start + 1;

			return Some((at..at, new.to_owned()));
		};

		let indent = trivia::line_indent(self.item, last.value().span().byte_range().start);

		// after the last element, and its comma if it has one
		let end = match last.punct() {
			Some(comma) => comma.span.byte_range().end,
			None => last.value().span().byte_range().end,
		};

		let inserted = match (last.punct().is_some(), multi_line) {
			(true, true) => format!("{le}{indent}{new},"),
			(true, false) => format!(" {new},"),
			(false, true) => format!(",{le}{indent}{new}"),
			(false, false) => format!(", {new}"),
		};

		Some((end..end, inserted))
	}

	/// Whether `element` of `group`, with the text at `range` of the item replaced by `replacement`, still sorts among
	/// the other elements (where it did).
	fn sorts_in(&self, group: &syn::UseGroup, element: &UseTree, range: &Range<usize>, replacement: &str) -> bool {
		let span = element.span().byte_range();
		let text = format!("{}{replacement}{}", &self.item[span.start..range.start], &self.item[range.end..span.end]);
		let Ok(new) = syn::parse_str::<UseTree>(&text) else {
			return false;
		};

		let elements: Vec<&UseTree> = group.items.iter().collect();
		let Some(index) = elements.iter().position(|&other| std::ptr::eq(other, element)) else {
			return false;
		};

		let after = |a: &UseTree, b: &UseTree| rscode_sort::use_tree_cmp(a, b, self.style_edition) == Ordering::Greater;
		let previous = index.checked_sub(1).map(|previous| elements[previous]);
		let next = elements.get(index + 1).copied();

		previous.is_none_or(|previous| !after(previous, &new) || after(previous, element))
			&& next.is_none_or(|next| !after(&new, next) || after(element, next))
	}

	/// The edit merging `leaf` (from its segment `depth`) into `tree`.
	fn walk(&self, tree: &UseTree, leaf: &Leaf, depth: usize) -> Option<(Range<usize>, String)> {
		let segment = leaf.segments.get(depth).map(|segment| unraw(segment));
		let same = |ident: &syn::Ident, name: Option<&str>| name.is_some_and(|name| ident.unraw() == name);

		match tree {
			UseTree::Path(path) if same(&path.ident, segment) => self.walk(&path.tree, leaf, depth + 1),

			// `a::b::c` with `a::b`: the module `b` itself joins as `self`
			UseTree::Path(path) if segment.is_none() && leaf.names_module(&path.ident) => {
				let mut module = leaf.clone();

				module.segments.push(path.ident.to_string());
				module.last = Last::SelfModule(None);
				self.walk(&path.tree, &module, depth + 1)
			}

			// into the element that shares the most segments with the leaf, unless that changes how it sorts among the
			// others: the leaf is then an element of its own
			UseTree::Group(group) => {
				let next = (group.items.iter())
					.filter(|element| shared_segments(element, leaf, depth) > 0)
					.min_by_key(|element| std::cmp::Reverse(shared_segments(element, leaf, depth)));

				let Some(element) = next else {
					return self.insert(group, &leaf.tree(depth));
				};

				let (range, replacement) = self.walk(element, leaf, depth)?;

				match self.sorts_in(group, element, &range, &replacement) {
					true => Some((range, replacement)),
					false => self.insert(group, &leaf.tree(depth)),
				}
			}

			// `io` with `io::Write`: `io::{self, Write}`
			UseTree::Name(name) if same(&name.ident, segment) => {
				Some((name.span().byte_range(), format!("{}::{{self, {}}}", name.ident, leaf.tree(depth + 1))))
			}

			// the same import
			UseTree::Name(name) if segment.is_none() && leaf.last == Last::Name(name.ident.to_string(), None) => None,

			// a path that diverges from the leaf's becomes a group with it: `fs` with `io` gives `{fs, io}`
			other => {
				let range = other.span().byte_range();
				let existing = &self.item[range.clone()];
				let new = leaf.tree(depth);
				let new_tree: UseTree = syn::parse_str(&new).ok()?;

				let group = match rscode_sort::use_tree_cmp(other, &new_tree, self.style_edition) {
					Ordering::Greater => format!("{{{new}, {existing}}}"),
					_ => format!("{{{existing}, {new}}}"),
				};

				Some((range, group))
			}
		}
	}
}

/// Where the module of an import is from, for modules that group their imports by it (in the order of the groups).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
enum Origin {
	/// The standard library (`std`, `core`, `alloc`, ...).
	Std,

	/// Other crates.
	External,

	/// The crate itself (`crate`, `self`, `super`).
	Local,
}

/// Where an import is: in a `use` item (or item) at an offset of the original text, in the text inserted by an edit
/// (by its index in the edits) at an offset of that text, where an earlier leaf is, or in no line of the file.
#[derive(Debug, Clone, Copy)]
enum Place {
	Original(usize),
	Inserted(usize, usize),
	Same,
	Nowhere,
}

/// A `use` item of the module, as a target to merge imports into.
#[derive(Debug, Clone)]
struct UseItem {
	item: ItemId,
	range: TextRange,

	/// The modules its imports are from (see [`Leaf::module`]), unraw'd, and their full paths.
	modules: Vec<Vec<String>>,
	paths: Vec<Vec<String>>,

	leading_colon: bool,
	vis: String,

	/// Whether it may take more imports: it has no attributes or `cfg`.
	mergeable: bool,
}

/// Plans importing `imports` into the module named by `module` (`crate` for the crate root). Each import is a `use`
/// tree as written in a `use` item, with or without `use` (and a visibility before it, for re-exports) and `;`:
/// `std::fs`, `crate::a::{B, C}`, `x::Y as Z`, `m::*`, `pub use a::B`. Every leaf of the trees is one import.
///
/// What the module imports already is left as it is ([`ImportOutcome::Present`]): by the same path and name, with the
/// same visibility, or by another path to the same item under the same name (`super::a::B` for `use crate::a::B;`),
/// and so is what it has in scope otherwise ([`ImportOutcome::InScope`]). The other imports follow the module's
/// import granularity, as its `use` items of the import's visibility show it:
/// - with one import per `use` item (or no `use` items), each gets a `use` item of its own, where `cargo rscode sort`
///   puts it: among the `use` items (or re-exports) in order, on consecutive lines like theirs (see [`super::insert`]),
///   or, when blank lines group them by where their paths are from (the standard library, other crates, the crate
///   itself, like rustfmt's `group_imports = "StdExternalCrate"`), in order in its group (or in a group of its own
///   where its origin goes);
/// - with one `use` item per module, an import from a module that a `use` item has several imports from is merged
///   into it (and into one with a single import only when no module has several `use` items, and the modules of
///   those with one import and those with several do not contain each other);
/// - with `use` items of several modules, an import is merged into the `use` item that shares the longest path
///   prefix with it.
///
/// A bare name that names nothing in the module (like `Circle`) imports the item of the workspace of that name, by
/// the shortest path to it usable from the module (`crate::shapes::Circle`). A path into the loaded crates (starting
/// with `crate`, `self`, `super`, or a name of the module's scope) must name something visible from the module.
///
/// `use` items with attributes (or `cfg`s) and of other visibilities take no imports. Merged imports go into the
/// `{}` groups in the order rustfmt sorts them, and an import that diverges from a path of the item turns it into a
/// group (`use std::fs;` with `std::io` becomes `use std::{fs, io};`). Imports that cannot be merged, or whose `use`
/// item would then sort elsewhere, get `use` items of their own.
///
/// Fails with [`Error::InvalidSource`] for an import that is not a `use` tree (or has attributes), with
/// [`Error::Collision`] when a new import binds a name the module binds otherwise (unless [`ImportOptions::force`];
/// glob imports never collide), with [`Error::NotFound`] for a path into the loaded crates that names nothing, with
/// [`Error::Ambiguous`] for a bare name that names several items, and with [`Error::Unsupported`] when `module` is not
/// a module, for a bare name that names no item of the workspace, and for what the module cannot import (an item that
/// is not visible from it, or an associated item).
pub fn add_imports(
	resolver: &Resolver<'_>,
	module: &ItemPath,
	imports: &[String],
	options: &ImportOptions,
) -> Result<ImportAddition, Error> {
	let ws = resolver.workspace();
	let mut leaves = Vec::new();

	for import in imports {
		leaves.extend(parse_leaves(import)?);
	}

	if leaves.is_empty() {
		return Err(Error::InvalidSource("there is nothing to import".to_owned()));
	}

	let target = target(resolver, module, None)?;

	if target.container != Container::Module {
		let kind = ws.item(target.item).kind;

		return Err(Error::Unsupported(format!(
			"cannot import into `{module}`: it is {}, and imports go into modules",
			article(kind)
		)));
	}

	let file = target.file;
	let uses = use_items(ws, target.item, file)?;

	// where each import ends up: (leaf, outcome, the original offset of its `use` item (or item), or its insertion and
	// the offset of the item in its text); and the leaves to import, which the module does not bind already (nor an
	// earlier leaf)
	let mut placed: Vec<(usize, ImportOutcome, Place)> = Vec::new();
	let mut new: Vec<usize> = Vec::new();

	for index in 0..leaves.len() {
		let leaf = &leaves[index];
		let imports = |use_item: &UseItem| {
			let vis = &ws.item(use_item.item).vis;

			(ws.children(use_item.item).filter_map(|import| ws.item(import).import_info()))
				.any(|info| leaf.is_imported_by(info, vis))
		};
		let existing = (uses.iter())
			.filter(|use_item| ws.item(use_item.item).cfg.is_none())
			.find(|use_item| imports(use_item));
		let place_of = |item: ItemId| match ws.file_of(item).path() == file.path() {
			true => Place::Original(ws.item(item).attrs.after_attrs),
			false => Place::Nowhere,
		};

		let bound = match existing {
			Some(use_item) => Some(Bound::Import(use_item.item)),
			None => bound_already(resolver, target.item, leaf),
		};

		match bound {
			Some(Bound::Import(use_item)) => placed.push((index, ImportOutcome::Present, place_of(use_item))),
			Some(Bound::Item(item)) => placed.push((index, ImportOutcome::InScope, place_of(item))),
			Some(Bound::Prelude) => placed.push((index, ImportOutcome::InScope, Place::Nowhere)),

			None => {
				match path_of_bare_name(resolver, target.item, module, leaf)? {
					Some(path) => leaves[index] = path,
					None => check_path(resolver, target.item, module, leaf)?,
				}

				match new.iter().any(|&earlier| leaves[earlier] == leaves[index]) {
					true => placed.push((index, ImportOutcome::Present, Place::Same)),
					false => new.push(index),
				}
			}
		}
	}

	let new_leaves: Vec<&Leaf> = new.iter().map(|&index| &leaves[index]).collect();

	check_collisions(resolver, &target, &new_leaves, options)?;

	let style_edition = order::style_edition(ws, &target);
	let siblings = Siblings::of(ws, &target, style_edition);
	let line_ending = trivia::line_ending(file.text());
	let mut merged: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
	let mut lines: Vec<usize> = Vec::new();

	for &index in &new {
		match merge_target(&uses, &leaves[index]) {
			Some(use_index) => merged.entry(use_index).or_default().push(index),
			None => lines.push(index),
		}
	}

	let mut edits: Vec<TextEdit> = Vec::new();

	for (use_index, indices) in &merged {
		let use_item = &uses[*use_index];
		let original = &file.text()[use_item.range.as_range()];
		let mut text = original.to_owned();
		let mut done = Vec::new();

		for &index in indices {
			// (where the merged item would sort elsewhere, sorting would move it)
			let merged = merge(&text, &leaves[index], style_edition, line_ending)
				.filter(|merged| siblings.keeps_order(use_item.range, &order::item_order(merged, style_edition)));

			match merged {
				Some(merged) => {
					text = merged;
					done.push(index);
				}

				// (an item that does not take it: a `use` item of its own)
				None => lines.push(index),
			}
		}

		if let Some(edit) = minimal_edit(original, &text, use_item.range.start) {
			edits.push(edit);
		}

		for index in done {
			placed.push((index, ImportOutcome::Merged(text.clone()), Place::Original(use_item.range.start)));
		}
	}

	if !lines.is_empty() {
		let cx = Context {
			target: &target,
			leaves: &leaves,
			uses: &uses,
			siblings: &siblings,
			style_edition,
		};

		insert_lines(&cx, &lines, &mut edits, &mut placed)?;
	}

	placed.sort_by_key(|(index, ..)| *index);

	let mut plan = ImportAddition {
		edits: EditSet::new(),
		imports: Vec::new(),
		file: file.path().to_path_buf(),
		warnings: Vec::new(),
	};

	for (index, outcome, place) in placed {
		let line = match place {
			Place::Original(offset) => new_line(file, &edits, None, offset),
			Place::Inserted(edit, within) => {
				new_line(file, &edits, Some(edit), edits[edit].range.start) + newlines(&edits[edit].replacement[..within])
			}

			// (a leaf imported by an earlier one: the same line)
			Place::Same => (plan.imports.iter())
				.find(|import| import.path == display(&leaves[index]))
				.map_or(0, |import| import.line),

			Place::Nowhere => 0,
		};

		plan.imports.push(AddedImport {
			path: display(&leaves[index]),
			outcome,
			line,
		});
	}

	for edit in edits {
		plan.edits.replace(file, edit.range, edit.replacement);
	}

	Ok(plan)
}

/// What `module` binds already of what `leaf` imports, under the name the leaf binds: for a bare name (without an
/// alias), whatever it names in the module (by any binding, or a prelude); for a path, what it names in every
/// namespace where it names a loaded item (or where the module binds the name), when the module binds the name to
/// that, other than by a glob import (as `use super::a::B;` binds what `crate::a::B` names).
fn bound_already(resolver: &Resolver<'_>, module: ItemId, leaf: &Leaf) -> Option<Bound> {
	let ws = resolver.workspace();
	let name = leaf.binding_name()?;
	let bare = leaf.is_bare() && matches!(leaf.last, Last::Name(_, None));
	let path = leaf.path_ref();
	let namespaces = match leaf.namespace() {
		Some(namespace) => vec![namespace],
		None => Namespace::ALL.to_vec(),
	};
	let mut bindings: Vec<Binding> = Vec::new();
	let mut matched = false;

	// (a bare name is looked up in the module's scope)
	let kind = if bare { PathKind::Code } else { PathKind::Use };

	for namespace in namespaces {
		let targets = resolver.resolve_prefixes(module, &path, Some(namespace), kind).pop().unwrap_or_default();
		let bound: Vec<Binding> = (resolver.bindings(module, name, namespace).into_iter())
			.filter(|binding| bare || !binding.glob)
			.collect();

		// (a path outside of the loaded crates names something in every namespace, as a guess)
		let guessed = targets.iter().all(|target| matches!(target, Res::External(_)));

		if targets.is_empty() || (bound.is_empty() && guessed && !bare) {
			continue;
		}

		let mut bound_to: Vec<Res> = bound.iter().map(|binding| binding.res.clone()).collect();

		bound_to.sort();
		bound_to.dedup();

		if !bare && bound_to != targets {
			return None;
		}

		matched = true;
		bindings.extend(bound);
	}

	if !matched {
		return None;
	}

	// the import that binds it, else the item
	let import = (bindings.iter())
		.filter_map(|binding| binding.import)
		.find(|&import| ws.item(import).kind == ItemKind::Import);

	if let Some(use_item) = import.and_then(|import| ws.parent(import)) {
		return Some(Bound::Import(use_item));
	}

	let item = bindings.iter().find_map(|binding| match (binding.import, &binding.res) {
		(Some(extern_crate), _) => Some(extern_crate),
		(None, Res::Item(item)) => Some(*item),
		_ => None,
	});

	Some(item.map_or(Bound::Prelude, Bound::Item))
}

/// Fails with [`Error::Collision`] when the new imports bind names that the module binds otherwise (unless forced).
fn check_collisions(
	resolver: &Resolver<'_>,
	target: &super::Target<'_>,
	leaves: &[&Leaf],
	options: &ImportOptions,
) -> Result<(), Error> {
	if options.force || leaves.is_empty() {
		return Ok(());
	}

	let source: Vec<String> = leaves.iter().map(|leaf| leaf.item()).collect();
	let parsed = parse_source(&source.join("\n"), Container::Module)?;
	let collisions = collisions(resolver, target, &parsed.items);

	if collisions.is_empty() {
		return Ok(());
	}

	let mut names: Vec<&str> = collisions.iter().map(|(name, _)| name.as_str()).collect();

	names.dedup();

	Err(Error::Collision {
		name: names.join("`, `"),
		collisions: collisions.iter().map(|(name, existing)| format!("`{name}`: {existing}")).collect(),
	})
}

/// Checks that the path of `leaf` names something that `module` (named `module_path`) can import, when the path goes
/// into the loaded crates: when it starts with `crate`, `self`, `super`, or a name of a loaded item in the module's
/// scope. Fails with [`Error::NotFound`] when it names nothing, and with [`Error::Unsupported`] when what it names is
/// not visible from `module`, or is in something other than a module or an enum (such as an associated item). Paths
/// into other crates, and through modules where macros or unresolved imports may bind more names, are not checked.
fn check_path(resolver: &Resolver<'_>, module: ItemId, module_path: &ItemPath, leaf: &Leaf) -> Result<(), Error> {
	let ws = resolver.workspace();
	let path = leaf.path_ref();
	let prefixes = resolver.resolve_prefixes(module, &path, leaf.namespace(), PathKind::Use);
	let items = |found: &[Res]| -> Option<Vec<ItemId>> {
		(found.iter())
			.map(|res| match res {
				Res::Item(item) => Some(*item),
				_ => None,
			})
			.collect()
	};

	if !prefixes.first().and_then(|first| items(first)).is_some_and(|first| !first.is_empty()) {
		return Ok(());
	}

	if let Some(missing) = prefixes.iter().position(Vec::is_empty) {
		// (what the segment before names: the first one names loaded items)
		let Some(containers) = items(&prefixes[missing - 1]) else {
			return Ok(());
		};

		let name = &path.segments[missing].name;
		let unseen = |item: &ItemId| ws.item(*item).kind == ItemKind::Module && resolver.may_bind_unseen(*item, name);

		if containers.iter().any(unseen) {
			return Ok(());
		}

		let has_members = |item: &ItemId| matches!(ws.item(*item).kind, ItemKind::Module | ItemKind::Enum);

		return match containers.iter().find(|item| !has_members(item)) {
			Some(&container) if !containers.iter().any(has_members) => Err(Error::Unsupported(format!(
				"cannot import `{}`: `{}` is {}, and imports name items of modules and variants of enums",
				leaf.path(),
				leaf.prefix(missing),
				article(ws.item(container).kind)
			))),

			_ => Err(Error::NotFound(leaf.path())),
		};
	}

	let visible = resolver.resolve_visible_prefixes(module, &path, leaf.namespace());

	match visible.iter().position(Vec::is_empty) {
		None => Ok(()),

		Some(hidden) => {
			let through = match hidden + 1 == path.segments.len() {
				true => String::new(),
				false => format!(" (`{}` is not)", leaf.prefix(hidden + 1)),
			};

			Err(Error::Unsupported(format!(
				"`{}` is not visible from `{module_path}`{through}: make it visible there, or import it by the path of \
				 a re-export",
				leaf.path()
			)))
		}
	}
}

/// Whitespace removed, to compare token text.
fn compact(text: &str) -> String {
	text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// An import for messages: its path, after its visibility.
fn display(leaf: &Leaf) -> String {
	match leaf.vis.as_str() {
		"" => leaf.path(),
		vis => format!("{vis} use {}", leaf.path()),
	}
}

/// The granularity the `use` items show (see [`Granularity`]).
fn granularity(uses: &[&UseItem]) -> Granularity {
	if uses.iter().all(|use_item| use_item.paths.len() <= 1) {
		return Granularity::Item;
	}

	match uses.iter().all(|use_item| use_item.modules.windows(2).all(|pair| pair[0] == pair[1])) {
		true => Granularity::Module,
		false => Granularity::Crate,
	}
}

/// Where a `use` item for `leaf` (of the order `order`) goes in a module that groups its `use` items by origin into
/// runs that blank lines separate (the standard library, other crates, then the crate itself, as with rustfmt's
/// `group_imports = "StdExternalCrate"`), and how it is spaced there: in order in the run of its origin, which it
/// joins without blank lines; or, when no run has its origin and the runs are in the order of their origins, as a run
/// of its own where its origin goes. `None` when the module does not group them so (sorting then decides).
fn grouped_placement(
	siblings: &Siblings,
	uses: &[UseItem],
	leaf: &Leaf,
	order: &ItemOrder,
) -> Option<(Placement, Spacing)> {
	let mut runs = siblings.runs(order);

	if runs.len() < 2 {
		return None;
	}

	let origin_of = |index: &usize| {
		let use_item = uses.iter().find(|use_item| use_item.range == siblings.range(*index))?;

		Some(origin(use_item.paths.first()?.first().map(String::as_str)))
	};

	// every run of one origin, and every origin in one run
	let mut origins = Vec::with_capacity(runs.len());

	for run in &runs {
		let first = origin_of(run.first()?)?;

		if run.iter().any(|index| origin_of(index) != Some(first)) || origins.contains(&first) {
			return None;
		}

		origins.push(first);
	}

	let leaf_origin = origin(leaf.segments.first().map(String::as_str).or_else(|| leaf.names().first().copied()));

	if let Some(run) = origins.iter().position(|&origin| origin == leaf_origin) {
		let run = runs.swap_remove(run);
		let placement = siblings.placement_in(&run, order)?;

		return Some((placement, siblings.spacing_in(placement, &run)));
	}

	if !origins.windows(2).all(|pair| pair[0] < pair[1]) {
		return None;
	}

	let placement = match origins.iter().position(|&origin| origin > leaf_origin) {
		Some(later) => Placement::Before(siblings.range(*runs[later].first()?)),
		None => Placement::After(siblings.range(*runs.last()?.last()?)),
	};

	Some((placement, Spacing::default()))
}

/// Whether the `use` items of a module of [`Granularity::Module`] clearly keep one item per module, so that an import
/// may turn an item of one import into a group: no module has several items, and the modules of the items of one
/// import and of those of several do not contain each other (`use std::fs;` next to `use std::io::{self, Write};` is
/// as likely to be one import per item, with a group where it reads better).
fn groups_by_module(uses: &[&UseItem]) -> bool {
	let module = |use_item: &&UseItem| use_item.modules.first().cloned().unwrap_or_default();
	let (groups, singles): (Vec<&UseItem>, Vec<&UseItem>) = uses.iter().partition(|use_item| use_item.paths.len() > 1);
	let nested = |a: &[String], b: &[String]| a.starts_with(b) || b.starts_with(a);
	let mut modules: Vec<Vec<String>> = uses.iter().map(module).collect();

	modules.sort();

	modules.windows(2).all(|pair| pair[0] != pair[1])
		&& (singles.iter()).all(|single| groups.iter().all(|group| !nested(&module(single), &module(group))))
}

/// Inserts `use` items for the leaves of `lines`, where sorting puts them (several at one place as one block, in
/// order, with a blank line between items of different groups, such as `use` and `pub use` items).
fn insert_lines(
	cx: &Context<'_, '_>,
	lines: &[usize],
	edits: &mut Vec<TextEdit>,
	placed: &mut Vec<(usize, ImportOutcome, Place)>,
) -> Result<(), Error> {
	let Context {
		target,
		leaves,
		uses,
		siblings,
		style_edition,
	} = *cx;
	let text = target.file.text();
	let items: Vec<String> = lines.iter().map(|&index| leaves[index].item()).collect();
	let orders = order::new_orders(&items.join("\n"), Container::Module, style_edition)
		.ok_or_else(|| Error::InvalidSource(format!("cannot import `{}`", items.join(" "))))?;
	let mut ordered: Vec<(usize, ItemOrder)> = lines.iter().copied().zip(orders).collect();

	ordered.sort_by(|(_, a), (_, b)| a.compare(b).unwrap_or(Ordering::Equal));

	let indent = trivia::body_indent(text, target.body);

	// the leaves for each place, in order
	let mut places: Vec<(Placement, Vec<(usize, ItemOrder)>)> = Vec::new();

	for (index, order) in ordered {
		let placement = (grouped_placement(siblings, uses, &leaves[index], &order).map(|(placement, _)| placement))
			.or_else(|| siblings.sorted_placement(&order))
			.unwrap_or(Placement::End(target.body));

		match places.iter_mut().find(|(known, _)| *known == placement) {
			Some((_, group)) => group.push((index, order)),
			None => places.push((placement, vec![(index, order)])),
		}
	}

	for (placement, group) in places {
		let mut source = String::new();

		for (position, (index, order)) in group.iter().enumerate() {
			if let Some((_, previous)) = position.checked_sub(1).map(|previous| &group[previous]) {
				source.push_str(if order.same_group(previous) { "\n" } else { "\n\n" });
			}

			source.push_str(&leaves[*index].item());
		}

		// spaced like the run of their origin, or the first items like their group above, the last ones like theirs
		// below
		let ((first, first_order), (_, last_order)) = (&group[0], &group[group.len() - 1]);
		let spacing = match grouped_placement(siblings, uses, &leaves[*first], first_order) {
			Some((_, spacing)) => spacing,

			None => Spacing {
				compact_above: siblings.spacing(placement, Some(std::slice::from_ref(first_order))).compact_above,
				compact_below: siblings.spacing(placement, Some(std::slice::from_ref(last_order))).compact_below,
			},
		};
		let edit = trivia::insertion(text, placement, &source, &indent, spacing);
		let mut from = 0;

		for (index, _) in &group {
			let item = leaves[*index].item();
			let within = edit.replacement[from..].find(item.as_str()).map_or(from, |offset| from + offset);

			placed.push((*index, ImportOutcome::Added, Place::Inserted(edits.len(), within)));
			from = within + item.len();
		}

		edits.push(edit);
	}

	Ok(())
}

/// Calls `leaf` with the segments and the last part of every leaf of a `use` tree below `prefix`.
fn leaves_of(tree: &UseTree, prefix: &mut Vec<String>, leaf: &mut impl FnMut(Vec<String>, Last)) {
	match tree {
		UseTree::Path(path) => {
			prefix.push(path.ident.to_string());
			leaves_of(&path.tree, prefix, leaf);
			prefix.pop();
		}

		UseTree::Name(name) if name.ident == "self" => leaf(prefix.clone(), Last::SelfModule(None)),
		UseTree::Name(name) => leaf(prefix.clone(), Last::Name(name.ident.to_string(), None)),
		UseTree::Rename(rename) => {
			let alias = Some(rename.rename.to_string());

			match rename.ident == "self" {
				true => leaf(prefix.clone(), Last::SelfModule(alias)),
				false => leaf(prefix.clone(), Last::Name(rename.ident.to_string(), alias)),
			}
		}

		UseTree::Glob(_) => leaf(prefix.clone(), Last::Glob),

		UseTree::Group(group) => {
			for tree in &group.items {
				leaves_of(tree, prefix, leaf);
			}
		}
	}
}

/// The text of the `use` item `item` with `leaf` merged into its tree, in the `{}` group where it belongs (in the
/// order rustfmt sorts groups in `style_edition`); `None` when it cannot be merged.
fn merge(item: &str, leaf: &Leaf, style_edition: StyleEdition, line_ending: &str) -> Option<String> {
	crate::source::isolated(|| {
		let parsed: syn::ItemUse = syn::parse_str(item).ok()?;
		let merging = Merging {
			item,
			style_edition,
			line_ending,
		};
		let (range, replacement) = merging.walk(&parsed.tree, leaf, 0)?;

		Some(format!("{}{replacement}{}", &item[..range.start], &item[range.end..]))
	})
}

/// The `use` item that `leaf` merges into, by its index in `uses`, if any: as the granularity of the items that may
/// take it (those of its visibility without attributes) has it.
fn merge_target(uses: &[UseItem], leaf: &Leaf) -> Option<usize> {
	let module: Vec<String> = leaf.module().iter().map(|segment| unraw(segment).to_owned()).collect();
	let names = leaf.names();
	let mergeable: Vec<&UseItem> = (uses.iter())
		.filter(|use_item| use_item.mergeable && compact(&use_item.vis) == compact(&leaf.vis))
		.collect();
	let candidates = (uses.iter().enumerate())
		.filter(|(_, use_item)| use_item.mergeable && use_item.leading_colon == leaf.leading_colon)
		.filter(|(_, use_item)| compact(&use_item.vis) == compact(&leaf.vis));

	match granularity(&mergeable) {
		Granularity::Item => None,

		Granularity::Module if module.is_empty() => None,
		Granularity::Module => {
			let groups_singles = groups_by_module(&mergeable);

			(candidates.filter(|(_, use_item)| use_item.modules.contains(&module)))
				.find(|(_, use_item)| use_item.paths.len() > 1 || groups_singles)
				.map(|(index, _)| index)
		}

		// the item with the longest shared prefix, and of those, the one with the most imports sharing it
		Granularity::Crate => {
			let shared = |path: &Vec<String>| path.iter().zip(&names).take_while(|(a, b)| a.as_str() == **b).count();

			candidates
				.map(|(index, use_item)| {
					let longest = use_item.paths.iter().map(shared).max().unwrap_or(0);
					let sharing = use_item.paths.iter().filter(|path| shared(path) == longest).count();

					(index, longest, sharing)
				})
				.filter(|&(_, longest, _)| longest > 0)
				.max_by_key(|&(index, longest, sharing)| (longest, sharing, std::cmp::Reverse(index)))
				.map(|(index, ..)| index)
		}
	}
}

/// The edit turning `old` (at `offset` in its file) into `new`: the range between their common start and end.
fn minimal_edit(old: &str, new: &str, offset: usize) -> Option<TextEdit> {
	if old == new {
		return None;
	}

	let prefix: usize = old.chars().zip(new.chars()).take_while(|(a, b)| a == b).map(|(c, _)| c.len_utf8()).sum();
	let suffix: usize = (old[prefix..].chars().rev())
		.zip(new[prefix..].chars().rev())
		.take_while(|(a, b)| a == b)
		.map(|(c, _)| c.len_utf8())
		.sum();

	Some(TextEdit {
		range: TextRange::new(offset + prefix, offset + old.len() - suffix),
		replacement: new[prefix..new.len() - suffix].to_owned(),
	})
}

/// The line (1-based) at which the text at `offset` of the original file is once `edits` are applied (other than
/// `exclude`, an insertion at `offset`, whose text goes there).
fn new_line(file: &SourceFile, edits: &[TextEdit], exclude: Option<usize>, offset: usize) -> usize {
	let text = file.text();
	let shift: isize = (edits.iter().enumerate())
		.filter(|&(index, edit)| Some(index) != exclude && edit.range.end <= offset)
		.map(|(_, edit)| newlines(&edit.replacement) as isize - newlines(&text[edit.range.as_range()]) as isize)
		.sum();

	(file.line_col(offset).line as isize + shift) as usize
}

fn newlines(text: &str) -> usize {
	text.matches('\n').count()
}

/// Where the module that a path starts with is from (see [`grouped_placement`]).
fn origin(first: Option<&str>) -> Origin {
	match first.map(unraw) {
		Some("std" | "core" | "alloc" | "proc_macro" | "test") => Origin::Std,
		Some("crate" | "self" | "super") => Origin::Local,
		_ => Origin::External,
	}
}

/// The leaves of an import as given: a `use` tree, with or without `use` (and a visibility before it) and `;`.
fn parse_leaves(text: &str) -> Result<Vec<Leaf>, Error> {
	let trimmed = text.trim().trim_end_matches(';').trim_end();
	let invalid = || {
		Error::InvalidSource(format!(
			"`{}` is not an import: give a `use` path, such as `std::fs`, `crate::a::{{B, C}}`, `x::Y as Z`, or `m::*`",
			text.trim()
		))
	};

	crate::source::isolated(|| {
		let sources = [format!("{trimmed};"), format!("use {trimmed};")];
		let (source, item) = (sources.iter())
			.find_map(|source| Some((source, syn::parse_str::<syn::ItemUse>(source).ok()?)))
			.ok_or_else(invalid)?;

		if !item.attrs.is_empty() {
			return Err(Error::InvalidSource(format!(
				"`{}` has attributes: imports are added without them (insert a `use` item with attributes instead)",
				text.trim()
			)));
		}

		// as written
		let vis = match &item.vis {
			syn::Visibility::Inherited => String::new(),
			vis => source.get(vis.span().byte_range()).unwrap_or_default().to_owned(),
		};
		let mut leaves = Vec::new();

		leaves_of(&item.tree, &mut Vec::new(), &mut |segments, last| {
			leaves.push(Leaf {
				vis: vis.clone(),
				leading_colon: item.leading_colon.is_some(),
				segments,
				last,
			})
		});

		// (`self` names the module before it)
		match leaves.iter().any(|leaf| leaf.segments.is_empty() && matches!(leaf.last, Last::SelfModule(_))) {
			true => Err(invalid()),
			false => Ok(leaves),
		}
	})
}

/// The import of the item that a bare name (an import without a path, like `Circle`) means, when the name names
/// nothing in `module` (no crate and no item in scope): the shortest path to the item of the workspace of that name
/// usable from `module` (named `module_path`, like `crate::shapes::Circle`). Fails with [`Error::Unsupported`] when no
/// item of the workspace has the name, or the one that has it cannot be named from `module`, and with
/// [`Error::Ambiguous`] when several items have it.
fn path_of_bare_name(
	resolver: &Resolver<'_>,
	module: ItemId,
	module_path: &ItemPath,
	leaf: &Leaf,
) -> Result<Option<Leaf>, Error> {
	let Last::Name(name, alias) = &leaf.last else {
		return Ok(None);
	};

	if !leaf.segments.is_empty() || leaf.leading_colon {
		return Ok(None);
	}

	let ws = resolver.workspace();
	let bare = PathRef {
		leading_colon: false,
		segments: vec![PathSegmentRef {
			name: unraw(name).into(),
			range: TextRange::default(),
			has_arguments: false,
		}],
	};

	if resolver.resolve_prefixes(module, &bare, None, PathKind::Use).last().is_some_and(|found| !found.is_empty()) {
		return Ok(None);
	}

	// (a dependency that is not loaded)
	if (ws.krate(module.krate()).dependencies().iter()).any(|dependency| dependency.name == unraw(name)) {
		return Ok(None);
	}

	let importable = |kind: ItemKind| {
		use ItemKind::*;

		matches!(kind, Module | Struct | Enum | Union | Trait | TraitAlias | TypeAlias | Fn | Const | Static | Variant)
	};
	let mut found: Vec<(String, ItemId)> = (ws.crates().iter())
		.flat_map(|krate| krate.items())
		.filter(|(item, data)| !item.is_crate_root() && importable(data.kind))
		.filter(|(_, data)| data.name.as_deref() == Some(unraw(name)))
		.map(|(item, _)| (resolver.canonical_path(item).to_string(), item))
		.collect();

	found.sort();
	found.dedup_by(|a, b| a.0 == b.0);

	let item = match found.as_slice() {
		[] => {
			return Err(Error::Unsupported(format!(
				"no item of the workspace is named `{name}`: name an item of another crate by its path (like \
				 `std::collections::HashSet`)"
			)));
		}

		[(_, item)] => *item,

		found => {
			return Err(Error::Ambiguous {
				path: name.clone(),
				candidates: found.iter().map(|&(_, item)| describe(resolver, item)).collect(),
			});
		}
	};

	let Some(path) = resolver.usable_paths(item, Viewpoint::Module(module)).into_iter().next() else {
		let item_path = resolver.canonical_path(item);

		return Err(match item.krate() == module.krate() {
			true => Error::Unsupported(format!(
				"`{name}` names `{item_path}`, which is not visible from `{module_path}`: make it visible there, or \
				 re-export it"
			)),
			false => unnameable(resolver, module, item, name),
		});
	};
	let leading_colon = path.starts_with("::");
	let mut segments: Vec<String> = path.trim_start_matches("::").split("::").map(str::to_owned).collect();
	let last = segments.pop().unwrap_or_default();

	Ok(Some(Leaf {
		vis: leaf.vis.clone(),
		leading_colon,
		segments,
		last: Last::Name(last, alias.clone()),
	}))
}

/// How many segments of `leaf` from its segment `depth` on a path of `tree` starts with.
fn shared_segments(tree: &UseTree, leaf: &Leaf, depth: usize) -> usize {
	let same = |ident: &syn::Ident| leaf.segments.get(depth).is_some_and(|segment| ident.unraw() == unraw(segment));

	match tree {
		UseTree::Path(path) if same(&path.ident) => 1 + shared_segments(&path.tree, leaf, depth + 1),
		UseTree::Name(name) if same(&name.ident) => 1,
		UseTree::Group(group) => (group.items.iter())
			.map(|element| shared_segments(element, leaf, depth))
			.max()
			.unwrap_or(0),
		_ => 0,
	}
}

/// The error for a bare name whose item `module` cannot name: the item is private, or its crate is not a dependency of
/// the module's crate.
fn unnameable(resolver: &Resolver<'_>, module: ItemId, item: ItemId, name: &str) -> Error {
	let ws = resolver.workspace();
	let from = ws.krate(module.krate());
	let krate = ws.krate(item.krate());
	let depends = (from.dependencies().iter()).any(|dependency| dependency.krate == Some(item.krate()));
	let why = if item.krate() == module.krate() || depends {
		"it is private (or in a private module)".to_owned()
	} else if !krate.kind().is_lib() {
		format!("it is in the {} crate `{}`, which no crate can depend on", krate.kind(), krate.name())
	} else {
		let package = from.package().map_or(from.name(), |package| &ws.package(package).name);

		format!(
			"its crate `{}` is not a dependency of `{}` (add it to the `[dependencies]` of the package `{package}`)",
			krate.name(),
			from.name()
		)
	};

	Error::Unsupported(format!(
		"cannot import `{name}`: the only item with that name, `{}`, cannot be named from `{}`: {why}",
		resolver.canonical_path(item),
		resolver.canonical_path(module)
	))
}

/// A name without `r#`.
fn unraw(name: &str) -> &str {
	name.strip_prefix("r#").unwrap_or(name)
}

/// The `use` items of a module, in source order.
fn use_items(ws: &Workspace, module: ItemId, file: &SourceFile) -> Result<Vec<UseItem>, Error> {
	let mut uses = Vec::new();

	for child in ws.children(module).filter(|&child| ws.item(child).kind == ItemKind::Use) {
		let data = ws.item(child);
		let infos: Vec<&ImportInfo> = ws.children(child).filter_map(|import| ws.item(import).import_info()).collect();
		let path = |info: &ImportInfo| info.path.names().map(|name| unraw(name).to_owned()).collect::<Vec<String>>();
		let module_of = |info: &ImportInfo| {
			let mut module = path(info);

			if !info.glob && !info.is_self {
				module.pop();
			}

			module
		};

		uses.push(UseItem {
			item: child,
			range: data.range,
			modules: infos.iter().map(|info| module_of(info)).collect(),
			paths: infos.iter().map(|info| path(info)).collect(),
			leading_colon: infos.first().is_some_and(|info| info.path.leading_colon),
			vis: vis_text(&data.vis),
			mergeable: data.cfg.is_none() && data.range.start == data.attrs.after_attrs && !infos.is_empty(),
		});
	}

	uses.sort_by_key(|use_item| use_item.range.start);

	// (the text of every `use` item is in the module's file)
	match uses.iter().all(|use_item| file.text().get(use_item.range.as_range()).is_some()) {
		true => Ok(uses),
		false => Err(Error::Unsupported("the `use` items of the module are not in its file".to_owned())),
	}
}

/// A visibility as written (empty for private items).
fn vis_text(vis: &Visibility) -> String {
	match vis {
		Visibility::Private | Visibility::Inherited => String::new(),
		vis => vis.to_string(),
	}
}
