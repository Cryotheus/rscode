//! Finding where items are used: their references in every loaded crate, each with the item it is in and its line.

use crate::Error;
use crate::edit::add_related;
use crate::edit::narrow_private_imports;
use crate::model::CrateId;
use crate::model::ItemData;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::Workspace;
use crate::path::CanonicalPath;
use crate::path::ItemPath;
use crate::resolve::Reference;
use crate::resolve::ReferenceKind;
use crate::resolve::ReferenceOptions;
use crate::resolve::Resolver;
use crate::source::FileId;
use crate::source::SourceFile;
use crate::source::TextRange;
use serde::Deserialize;
use serde::Serialize;
use smol_str::SmolStr;
use std::collections::HashMap;

/// The longest line of code shown for a reference, in characters: longer lines are cut around the reference.
const MAX_LINE_CHARS: usize = 100;

/// Options for [`find_references`].
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct FindReferencesOptions {
	/// Which uncertain occurrences (method calls, names inside of macros) and doc links to look for too.
	pub references: ReferenceOptions,

	/// Also report the definitions of the items: their names where they are declared, and the items implementing
	/// them in `impl` blocks that are not loaded (inside of function bodies).
	pub definitions: bool,
}

/// A reference found by [`find_references`], with where it is.
#[derive(Debug, Clone, Serialize)]
pub struct FoundReference {
	/// The reference.
	#[serde(flatten)]
	pub reference: Reference,

	/// The canonical path of the module whose file the reference is in (the file's own module, not an inline module
	/// inside of it).
	pub module: String,

	/// The canonical path of the innermost item the reference is in, below [`FoundReference::module`]: for code in a
	/// function body, the function; for a doc link, the documented item. `None` at the top level of the module (in
	/// its imports, for example).
	pub item: Option<String>,

	/// [`FoundReference::item`] relative to [`FoundReference::module`] when it is below it (`Type::method`,
	/// `tests::case`, `impl Display for Type`), or else its canonical path (the items of an `impl` of a type of another
	/// module).
	pub local_item: Option<String>,

	/// The line of code of the reference, without leading and trailing whitespace. Lines longer than 100 characters
	/// are cut around the reference, with `...` where they are cut.
	pub line: String,
}

/// The modules of each crate by their own file (see [`ModuleInfo::file`](crate::model::ModuleInfo::file)), to find
/// the module that a reference's file belongs to.
struct ModuleFiles<'ws> {
	ws: &'ws Workspace,

	/// The modules whose own file each file is, per crate, computed when first needed.
	crates: HashMap<CrateId, HashMap<FileId, Vec<ItemId>>>,
}

impl<'ws> ModuleFiles<'ws> {
	fn new(ws: &'ws Workspace) -> Self {
		Self {
			ws,
			crates: HashMap::new(),
		}
	}

	/// The modules of `krate` whose own file is `file` (one, unless `#[path]` attributes load it several times).
	fn of(&mut self, krate: CrateId, file: FileId) -> &[ItemId] {
		let ws = self.ws;
		let files = self.crates.entry(krate).or_insert_with(|| {
			let mut files: HashMap<FileId, Vec<ItemId>> = HashMap::new();

			for (item, data) in ws.krate(krate).items() {
				if let Some(file) = data.module_info().and_then(|info| info.file) {
					files.entry(file).or_default().push(item);
				}
			}

			files
		});

		files.get(&file).map_or(&[], Vec::as_slice)
	}
}

/// The references found by [`find_references`].
#[derive(Debug, Default, Clone, Serialize)]
pub struct ReferenceReport {
	/// The canonical paths of the items whose references were searched, sorted and without duplicates.
	pub targets: Vec<String>,

	/// Sorted by file path and position, with at most one reference per range.
	pub references: Vec<FoundReference>,

	/// Human-readable notes about places that could not be searched (files that do not parse, ...).
	pub notes: Vec<String>,
}

impl ReferenceReport {
	/// The number of files with references.
	pub fn files(&self) -> usize {
		let mut files: Vec<_> = self.references.iter().map(|found| &found.reference.path).collect();

		files.dedup();
		files.len()
	}

	/// The number of references that might not refer to the targets (see [`Reference::certain`]).
	pub fn uncertain(&self) -> usize {
		self.references.iter().filter(|found| !found.reference.certain).count()
	}
}

/// Refuses an item that code never refers to by a name of its own.
fn check_searchable(resolver: &Resolver<'_>, item: ItemId) -> Result<(), Error> {
	let data = resolver.workspace().item(item);
	let untracked = match data.kind {
		ItemKind::Field => Some("a field, whose uses (field accesses, struct literals, and patterns)"),
		_ if resolver.workspace().entry_macro(item).is_some() => Some("an entry of a macro invocation, whose uses"),
		_ => None,
	};

	if let Some(what) = untracked {
		return Err(Error::Unsupported(format!("`{}` is {what} are not tracked", resolver.canonical_path(item))));
	}

	let what = match data.kind {
		_ if item.is_crate_root() => "a crate root",
		ItemKind::Impl => "an `impl` block",
		ItemKind::Use | ItemKind::Import => "an import",
		ItemKind::MacroCall | ItemKind::AssocMacro | ItemKind::ForeignMacro => "a macro invocation",
		ItemKind::ExternBlock => "an `extern` block",
		ItemKind::ExternCrate => "an `extern crate` item",
		_ if data.name.as_ref().is_none_or(|name| name == "_") => "an unnamed item",
		_ => return Ok(()),
	};

	let hint = match data.macro_name() {
		_ if data.kind == ItemKind::Import => {
			": search for the references of what it imports instead (its path without `use`)".to_owned()
		}

		Some(name) => {
			format!(": search for the uses of the macro `{name}!` with the path of the macro itself, without `!`")
		}

		None => String::new(),
	};

	Err(Error::Unsupported(format!(
		"`{}` is {what}, whose references cannot be searched{hint}",
		resolver.canonical_path(item)
	)))
}

/// The loaded child of `parent` whose range contains `offset` (children are in source order).
fn child_at(ws: &Workspace, parent: ItemId, offset: usize) -> Option<ItemId> {
	let children = &ws.item(parent).children;
	let krate = parent.krate();
	let count = children.partition_point(|&child| ws.item(ItemId::new(krate, child)).range.start <= offset);
	let child = ItemId::new(krate, *children.get(count.checked_sub(1)?)?);

	ws.item(child).range.contains(offset).then_some(child)
}

/// Finds the references to the items that `paths` name in every loaded crate, selected or not (to search every
/// workspace member, load them all with `LoadOptions::load_all_members`).
///
/// A path names every `cfg` variant of an item, and through private imports what they import (see
/// [`edit::rename`](crate::edit::rename), whose search this is). The items of traits are searched with the items of
/// the trait's `impl`s, and the other way around, since they share their name; so are the copies of items that other
/// crates load from the same file. Certain references are always found (paths, including those in imports, and paths
/// through the aliases of imports, like `New` after `use a::Old as New;`); uncertain ones and doc links only as
/// `options` ask (see [`ReferenceOptions`]). Attributes are not searched (derives, attribute macro arguments, paths in
/// strings like serde's `default = "path"`).
///
/// Fails with [`Error::NotFound`] when a path names nothing, and with [`Error::Unsupported`] when it names an item
/// that code does not refer to by a name of its own (an `impl` block, an import, a crate root, ...).
pub fn find_references(
	resolver: &Resolver<'_>,
	paths: &[ItemPath],
	options: &FindReferencesOptions,
) -> Result<ReferenceReport, Error> {
	let mut targets = Vec::new();

	for path in paths {
		let mut items = resolver.resolve_item_path(path);

		if items.is_empty() {
			return Err(Error::NotFound(path.to_string()));
		}

		// a path through private imports names what they import, unless its module also binds the name otherwise
		narrow_private_imports(resolver, path, &mut items);

		for &item in &items {
			check_searchable(resolver, item)?;
		}

		targets.extend(items);
	}

	targets.sort();
	targets.dedup();
	add_related(resolver, &mut targets);

	// uses through the aliases of the targets' imports (`use a::Old as New;` and uses of `New`) name them too
	let aliases = renaming_imports(resolver, &targets);
	let lost = match aliases.is_empty() {
		true => Vec::new(),
		false => resolver.lost_bindings(&Resolver::without_imports(resolver.workspace(), &aliases)),
	};
	let found = resolver.find_references_through(&targets, &lost, &[], &options.references);
	let mut modules = ModuleFiles::new(resolver.workspace());
	let references = (found.references.into_iter())
		.filter(|reference| options.definitions || reference.kind != ReferenceKind::Definition)
		.map(|reference| locate(resolver, &mut modules, reference))
		.collect();
	let mut targets: Vec<String> = targets.iter().map(|&target| resolver.canonical_path(target).to_string()).collect();

	targets.sort();
	targets.dedup();

	Ok(ReferenceReport {
		targets,
		references,
		notes: found.notes,
	})
}

/// The innermost item below `module` (in the module's own file) whose range contains `offset`, or `module`.
fn innermost(ws: &Workspace, module: ItemId, offset: usize) -> ItemId {
	let mut item = module;

	while let Some(child) = child_at(ws, item, offset) {
		item = child;

		// (the items of an out-of-line module are in its own file)
		if ws.item(child).module_info().is_some_and(|info| !info.inline) {
			break;
		}
	}

	item
}

/// Whether a reference in an item is better described by what the item is in: the line shows imports, and macro
/// invocations, `extern` blocks, and unnamed items (`const _: () = { ... };`) have no paths of their own.
fn is_transparent(data: &ItemData) -> bool {
	match data.kind {
		ItemKind::Use
		| ItemKind::Import
		| ItemKind::MacroCall
		| ItemKind::AssocMacro
		| ItemKind::ForeignMacro
		| ItemKind::ExternBlock => true,

		ItemKind::Impl => false,
		_ => data.name.is_none(),
	}
}

/// The line of code of `range`, trimmed, and cut around the range when it is longer than [`MAX_LINE_CHARS`].
fn line_of(source: &SourceFile, range: TextRange) -> String {
	let text = source.text();
	let index = source.line_index();
	let line = source.line_col(range.start).line;
	let start = index.line_start(line);
	let whole = &text[start..index.line_end(text, line)];
	let trimmed = whole.trim();
	let chars = trimmed.chars().count();

	if chars <= MAX_LINE_CHARS {
		return trimmed.to_owned();
	}

	// a window with the reference a third of the way in, unless that is near an end of the line
	let trimmed_start = start + (whole.len() - whole.trim_start().len());
	let at = text.get(trimmed_start..range.start).map_or(0, |before| before.chars().count());
	let first = at.saturating_sub(MAX_LINE_CHARS / 3).min(chars - MAX_LINE_CHARS);
	let window: String = trimmed.chars().skip(first).take(MAX_LINE_CHARS).collect();
	let before = if first > 0 { "..." } else { "" };
	let after = if first + MAX_LINE_CHARS < chars { "..." } else { "" };

	format!("{before}{window}{after}")
}

/// A reference with the module whose file it is in, the item it is in, and its line.
fn locate(resolver: &Resolver<'_>, modules: &mut ModuleFiles<'_>, reference: Reference) -> FoundReference {
	let ws = resolver.workspace();
	let offset = reference.range.start;
	let candidates = modules.of(reference.krate, reference.file);

	// (with several modules from the file, the one with an item containing the reference)
	let (module, mut item) = (candidates.iter())
		.map(|&module| (module, innermost(ws, module, offset)))
		.find(|(module, item)| item != module)
		.or_else(|| candidates.first().map(|&module| (module, module)))
		.unwrap_or_else(|| {
			let root = ItemId::crate_root(reference.krate);

			(root, root)
		});

	while item != module && is_transparent(ws.item(item)) {
		item = ws.parent(item).unwrap_or(module);
	}

	let module_path = resolver.canonical_path(module);
	let (item, local_item) = match item == module {
		true => (None, None),

		false => {
			let path = resolver.canonical_path(item);

			(Some(path.to_string()), Some(relative(&path, &module_path)))
		}
	};

	FoundReference {
		module: module_path.to_string(),
		item,
		local_item,
		line: line_of(ws.krate(reference.krate).file(reference.file), reference.range),
		reference,
	}
}

/// `path` without the segments of `module` when it starts with them (`Type::method` for `my_crate::m::Type::method`
/// in `my_crate::m`), or else all of it.
fn relative(path: &CanonicalPath, module: &CanonicalPath) -> String {
	let prefix: Vec<SmolStr> = module.segments.iter().chain(&module.name).cloned().collect();

	if path.is_import || !path.segments.starts_with(&prefix) {
		return path.to_string();
	}

	let mut relative = path.clone();

	relative.segments.drain(..prefix.len());
	relative.to_string()
}

/// The imports of `targets` that bind them under another name (`use a::Old as New;`), in every loaded crate.
fn renaming_imports(resolver: &Resolver<'_>, targets: &[ItemId]) -> Vec<ItemId> {
	let ws = resolver.workspace();
	let mut imports: Vec<ItemId> = Vec::new();

	for &target in targets {
		let name = ws.item(target).name.as_ref();

		for import in resolver.imports_of(target) {
			let binding = ws.item(import).import_info().and_then(|info| info.binding_name());

			if binding.is_some_and(|binding| Some(binding) != name) {
				imports.push(import);
			}
		}
	}

	imports.sort();
	imports.dedup();
	imports
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::source::LineIndex;

	#[test]
	fn long_lines_are_cut_around_the_reference() {
		let long = format!("\t\tlet x = {}target{};\n", "a + ".repeat(40), " + b".repeat(40));
		let source = SourceFile::new("lib.rs".into(), long.clone());
		let start = long.find("target").unwrap();
		let line = line_of(&source, TextRange::new(start, start + 6));

		assert!(line.starts_with("...") && line.ends_with("..."), "{line}");
		assert!(line.contains("target"), "{line}");
		assert_eq!(line.chars().count(), MAX_LINE_CHARS + 6);

		// near the start of the line, nothing is cut before it
		let start = long.find("let").unwrap();
		let line = line_of(&source, TextRange::new(start, start + 3));

		assert!(line.starts_with("let x = a + ") && line.ends_with("..."), "{line}");
		assert_eq!(LineIndex::new(&line).line_count(), 1);
	}

	#[test]
	fn short_lines_are_trimmed() {
		let text = "fn a() {\r\n\t  b();  \r\n}\r\n";
		let source = SourceFile::new("lib.rs".into(), text);
		let start = text.find('b').unwrap();

		assert_eq!(line_of(&source, TextRange::new(start, start + 1)), "b();");
	}
}
