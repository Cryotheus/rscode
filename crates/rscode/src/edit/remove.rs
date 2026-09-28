//! Removing items.
//!
//! Items are removed with the comments attached to them (see [`trivia::removal_ranges`]); variants with their comma.
//! An out-of-line module takes its files along: the module's file and those of its descendants, and the module's
//! directory when nothing else is in it. Files that another (kept) module also loads are kept.
//!
//! With [`RemoveOptions::prune_imports`], imports that would break are removed too: those that only import removed
//! items, and those whose path goes through a removed module. A leaf of a `use` group is removed with its comma, a
//! group whose leaves all go is removed as a whole, and so is the `use` item when nothing is left of it.

use crate::Error;
use crate::edit::EditSet;
use crate::edit::trivia;
use crate::model::CrateId;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::Workspace;
use crate::path::ItemPath;
use crate::resolve::PathKind;
use crate::resolve::Reference;
use crate::resolve::ReferenceOptions;
use crate::resolve::References;
use crate::resolve::Res;
use crate::resolve::Resolver;
use crate::source::FileId;
use crate::source::LineCol;
use crate::source::SourceFile;
use crate::source::TextRange;
use proc_macro2::Delimiter;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

/// Options for [`remove`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct RemoveOptions {
	/// Keep the files of removed out-of-line modules.
	pub keep_files: bool,

	/// Also remove imports (`use` tree leaves) of the removed items.
	pub prune_imports: bool,

	/// Only remove `cfg` variants that are not definitely disabled.
	pub active_only: bool,
}

/// An item that is removed.
#[derive(Debug, Clone, Serialize)]
pub struct RemovedItem {
	/// The canonical path.
	pub path: String,

	/// The kind.
	pub kind: ItemKind,

	/// The file it is removed from.
	pub file: PathBuf,

	/// Where it starts.
	pub start: LineCol,

	/// Where it ends (exclusive).
	pub end: LineCol,
}

/// The planned removal.
#[derive(Debug, Clone, Serialize)]
pub struct Removal {
	/// The edits, to preview or apply.
	#[serde(skip)]
	pub edits: EditSet,

	/// The removed items (see [`remove`]).
	pub removed: Vec<RemovedItem>,

	/// References to the removed items that remain (and will no longer compile).
	pub dangling: Vec<Reference>,

	/// Things to know about the removal.
	pub warnings: Vec<String>,
}

/// Plans the removal of the items named by `paths` (every `cfg` variant), including their attached comments.
/// Removing an out-of-line module also deletes its files, unless [`RemoveOptions::keep_files`].
///
/// [`Removal::removed`] lists the removed items (items inside of other removed items are not listed), followed by
/// the pruned imports ([`ItemKind::Import`]). Fails with [`Error::NotFound`] when a path names nothing, with
/// [`Error::Ambiguous`] when it names `impl` blocks (or their items) with different headers (see
/// [`ItemPath`]'s generic arguments), and with [`Error::Unsupported`] for crate roots.
pub fn remove(resolver: &Resolver<'_>, paths: &[ItemPath], options: &RemoveOptions) -> Result<Removal, Error> {
	let ws = resolver.workspace();
	let targets = targets(resolver, paths, options.active_only)?;
	let mut plan = Removal { edits: EditSet::new(), removed: Vec::new(), dangling: Vec::new(), warnings: Vec::new() };
	let mut deletions = Deletions::default();

	for &item in &targets {
		match ws.item(item).kind {
			ItemKind::Variant => deletions.element(ws.file_of(item), ws.item(item).range),
			_ => deletions.item(ws.file_of(item), ws.item(item).range),
		}

		push_removed(&mut plan.removed, removed_item(resolver, item));
	}

	let removed = removed_items(ws, &targets, &deletions.ranges());

	if options.prune_imports {
		for (use_item, imports) in broken_imports(resolver, &removed) {
			prune(ws, use_item, &imports, &mut deletions);

			for import in imports {
				push_removed(&mut plan.removed, removed_item(resolver, import));
			}
		}
	}

	let deleted_files = match options.keep_files {
		true => Vec::new(),
		false => module_files(resolver, &targets, &removed, &mut plan.warnings),
	};

	let ranges = deletions.ranges();

	for (file, ranges) in ranges.values() {
		for &range in ranges {
			plan.edits.replace(file, range, "");
		}
	}

	for path in &deleted_files {
		plan.edits.delete_path(path);
	}

	plan.dangling = dangling(resolver, &removed, &ranges, &deleted_files, &mut plan.warnings);

	Ok(plan)
}

/// The items to remove: what the paths name, without items inside of other items to remove.
fn targets(resolver: &Resolver<'_>, paths: &[ItemPath], active_only: bool) -> Result<Vec<ItemId>, Error> {
	let ws = resolver.workspace();
	let mut targets: Vec<ItemId> = Vec::new();

	for path in paths {
		let mut items = resolver.resolve_item_path(path);

		if items.is_empty() {
			return Err(Error::NotFound(path.to_string()));
		}

		if active_only {
			items.retain(|&item| ws.is_active(item).is_possible());

			if items.is_empty() {
				return Err(Error::NotFound(format!("{path}` whose `cfg` can be `true")));
			}
		}

		check_impl_headers(resolver, path, &items)?;

		for item in items {
			if item.is_crate_root() {
				return Err(Error::Unsupported(format!("`{path}` is a crate root, which cannot be removed")));
			}

			if !targets.contains(&item) {
				targets.push(item);
			}
		}
	}

	let all: HashSet<ItemId> = targets.iter().copied().collect();

	targets.retain(|&item| !ws.ancestors(item).any(|ancestor| all.contains(&ancestor)));

	Ok(targets)
}

/// Refuses a path naming `impl` blocks (or items of them) whose headers differ, such as `impl From<u8> for X` and
/// `impl From<u16> for X` for `impl From for X`: those are not `cfg` variants of each other, and the path can tell them
/// apart with generic arguments.
fn check_impl_headers(resolver: &Resolver<'_>, path: &ItemPath, items: &[ItemId]) -> Result<(), Error> {
	let ws = resolver.workspace();

	if !super::impl_headers_differ(ws, items) {
		return Ok(());
	}

	let candidates = items
		.iter()
		.map(|&item| {
			let file = ws.file_of(item);
			let start = file.line_col(ws.item(item).range.start);

			let path = resolver.canonical_path(item).distinct();

			format!("`{path}` at {}:{start}", ws.display_path(file.path()).display())
		})
		.collect();

	Err(Error::Ambiguous { path: path.to_string(), candidates })
}

/// Every item that is gone after the removal: the targets and the items inside of them, and the items of other
/// crates whose text is deleted (of files that several crates load).
fn removed_items(ws: &Workspace, targets: &[ItemId], deleted: &DeletedText<'_>) -> HashSet<ItemId> {
	let mut removed = HashSet::new();
	let mut stack = targets.to_vec();

	while let Some(item) = stack.pop() {
		if removed.insert(item) {
			stack.extend(ws.children(item));
		}
	}

	for krate in ws.crates() {
		// the range of a crate root is its whole file, which a removal may empty without removing the crate
		for (item, data) in krate.items().filter(|(item, _)| !item.is_crate_root()) {
			let path = krate.file(data.file).path();

			if deleted.get(path).is_some_and(|(_, ranges)| ranges.iter().any(|range| range.contains_range(data.range)))
			{
				removed.insert(item);
			}
		}
	}

	removed
}

fn removed_item(resolver: &Resolver<'_>, item: ItemId) -> RemovedItem {
	let ws = resolver.workspace();
	let data = ws.item(item);
	let file = ws.file_of(item);
	let (start, end) = file.locate(data.range);

	RemovedItem {
		path: resolver.canonical_path(item).to_string(),
		kind: data.kind,
		file: file.path().to_path_buf(),
		start,
		end,
	}
}

/// Adds an item unless it is listed (the same text is loaded by several crates).
fn push_removed(removed: &mut Vec<RemovedItem>, item: RemovedItem) {
	let listed =
		removed.iter().any(|known| (&known.path, &known.file, known.start) == (&item.path, &item.file, item.start));

	if !listed {
		removed.push(item);
	}
}

/// Text to delete, by file.
#[derive(Debug, Default)]
struct Deletions<'ws> {
	files: BTreeMap<&'ws Path, FileDeletions<'ws>>,
}

#[derive(Debug)]
struct FileDeletions<'ws> {
	file: &'ws SourceFile,

	/// Items, removed with their attached comments and a line.
	items: Vec<TextRange>,

	/// Elements of comma-separated lists (variants, `use` group elements), removed with a comma.
	elements: Vec<TextRange>,
}

/// The ranges deleted from each file, with the file.
type DeletedText<'ws> = BTreeMap<&'ws Path, (&'ws SourceFile, Vec<TextRange>)>;

impl<'ws> Deletions<'ws> {
	fn file(&mut self, file: &'ws SourceFile) -> &mut FileDeletions<'ws> {
		self.files.entry(file.path()).or_insert_with(|| FileDeletions { file, items: Vec::new(), elements: Vec::new() })
	}

	fn item(&mut self, file: &'ws SourceFile, range: TextRange) {
		self.file(file).items.push(range);
	}

	fn element(&mut self, file: &'ws SourceFile, range: TextRange) {
		self.file(file).elements.push(range);
	}

	/// The ranges to delete, sorted, per file. Items separated only by trivia are removed as one block.
	fn ranges(&self) -> DeletedText<'ws> {
		(self.files.iter())
			.map(|(&path, deletions)| {
				let text = deletions.file.text();
				let mut ranges = trivia::removal_ranges(text, &deletions.items);

				ranges.extend(element_removal_ranges(text, &deletions.elements));
				ranges.sort();
				ranges.dedup();

				(path, (deletions.file, ranges))
			})
			.collect()
	}
}

/// The ranges to delete to remove elements of comma-separated lists. Consecutive elements of a list are removed
/// together, so that the list keeps no trailing comma it did not have.
fn element_removal_ranges(text: &str, elements: &[TextRange]) -> Vec<TextRange> {
	let mut elements = elements.to_vec();
	let mut blocks: Vec<TextRange> = Vec::with_capacity(elements.len());

	elements.sort();
	elements.dedup();

	for element in elements {
		match blocks.last_mut() {
			Some(block) if text.get(block.end..element.start).is_some_and(|between| between.trim() == ",") => {
				block.end = element.end;
			}
			_ => blocks.push(element),
		}
	}

	blocks.into_iter().map(|block| trivia::list_item_removal_range(text, block)).collect()
}

/// Imports that break when the removed items are gone, by their `use` item: imports whose targets are all removed,
/// and imports whose path goes through a removed item.
fn broken_imports(resolver: &Resolver<'_>, removed: &HashSet<ItemId>) -> BTreeMap<ItemId, Vec<ItemId>> {
	let ws = resolver.workspace();
	let is_removed = |res: &Res| matches!(res, Res::Item(item) if removed.contains(item));
	let mut imports: BTreeSet<ItemId> = BTreeSet::new();

	for &item in removed {
		for import in resolver.imports_of(item) {
			if !removed.contains(&import) && resolver.import_targets(import).iter().all(is_removed) {
				imports.insert(import);
			}
		}
	}

	// paths through a removed module or enum break even when they import something else (like a re-export in the
	// module, or an item that is not resolved)
	if removed.iter().any(|&item| matches!(ws.item(item).kind, ItemKind::Module | ItemKind::Enum)) {
		for krate in ws.crates() {
			for (import, data) in krate.items() {
				let Some(info) = data.import_info() else {
					continue;
				};

				if removed.contains(&import) || imports.contains(&import) {
					continue;
				}

				let prefixes = resolver.resolve_prefixes(ws.module_of(import), &info.path, None, PathKind::Use);

				if prefixes.iter().any(|resolutions| !resolutions.is_empty() && resolutions.iter().all(is_removed)) {
					imports.insert(import);
				}
			}
		}
	}

	let mut broken: BTreeMap<ItemId, Vec<ItemId>> = BTreeMap::new();

	for import in imports {
		if let Some(use_item) = ws.parent(import) {
			broken.entry(use_item).or_default().push(import);
		}
	}

	broken
}

/// Plans removing `imports` (leaves) of a `use` item: the whole item when nothing else is in it.
fn prune<'ws>(ws: &'ws Workspace, use_item: ItemId, imports: &[ItemId], deletions: &mut Deletions<'ws>) {
	let file = ws.file_of(use_item);
	let range = ws.item(use_item).range;

	if ws.children(use_item).all(|leaf| imports.contains(&leaf)) {
		deletions.item(file, range);
		return;
	}

	let leaves: Vec<TextRange> = imports.iter().map(|&import| ws.item(import).range).collect();

	for element in use_elements(file.text(), range, &leaves) {
		deletions.element(file, element);
	}
}

/// The elements of `use` groups to delete to remove the leaves at `leaves` (the ranges of leaves in the model: their
/// element of the innermost enclosing group): a group element goes as a whole when all leaves in it go.
///
/// `use_item` must be a `use` item that keeps some of its leaves.
fn use_elements(text: &str, use_item: TextRange, leaves: &[TextRange]) -> Vec<TextRange> {
	let parsed = text.get(use_item.as_range()).and_then(|snippet| snippet.parse::<TokenStream>().ok());
	let tokens: Vec<TokenTree> = parsed.into_iter().flatten().collect();

	// the tree is between `use` and `;`
	let Some(start) = tokens.iter().position(|token| matches!(token, TokenTree::Ident(ident) if ident == "use")) else {
		return leaves.to_vec();
	};

	let tree = match &tokens[start + 1..] {
		[tree @ .., TokenTree::Punct(semicolon)] if semicolon.as_char() == ';' => tree,
		tree => tree,
	};

	let mut elements = Vec::new();

	match use_element(tree, use_item.start, leaves, &mut elements) {
		// the caller removes a `use` whose leaves all go as a whole
		Pruned::All | Pruned::Nothing => leaves.to_vec(),
		Pruned::Some => elements,
	}
}

/// How much of a `use` tree element is pruned.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Pruned {
	/// All of its leaves (it has at least one).
	All,

	/// Some of its leaves.
	Some,

	/// None of its leaves, or it has none (`{}`).
	Nothing,
}

/// Finds the pruned elements of a `use` tree element (its tokens), adding those to delete to `elements`.
///
/// `offset` is the offset of the tokens' text. A pruned element with pruned leaves only is not added itself: its
/// parent (group) decides.
fn use_element(tokens: &[TokenTree], offset: usize, leaves: &[TextRange], elements: &mut Vec<TextRange>) -> Pruned {
	let Some(range) = tokens_range(tokens, offset) else {
		return Pruned::Nothing;
	};

	let group = match tokens.last() {
		Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Brace => group,
		_ if leaves.contains(&range) => return Pruned::All,
		_ => return Pruned::Nothing,
	};

	let stream: Vec<TokenTree> = group.stream().into_iter().collect();
	let children: Vec<&[TokenTree]> = stream
		.split(|token| matches!(token, TokenTree::Punct(comma) if comma.as_char() == ','))
		.filter(|element| !element.is_empty())
		.collect();

	// children whose leaves all go, whether some child is partly pruned, and whether some child keeps leaves
	let mut all = Vec::new();
	let mut partly = false;
	let mut kept = false;

	for child in children {
		match use_element(child, offset, leaves, elements) {
			Pruned::All => all.extend(tokens_range(child, offset)),
			Pruned::Some => (partly, kept) = (true, true),
			Pruned::Nothing => kept |= !is_empty_group(child),
		}
	}

	if !all.is_empty() && !kept {
		return Pruned::All;
	}

	let pruned = partly || !all.is_empty();

	elements.extend(all);

	if pruned { Pruned::Some } else { Pruned::Nothing }
}

/// Whether an element is an empty group (`{}` or `a::{}`), which has no leaves.
fn is_empty_group(tokens: &[TokenTree]) -> bool {
	matches!(tokens.last(), Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Brace && group.stream().is_empty())
}

/// The range of tokens (lexed from the text at `offset`).
fn tokens_range(tokens: &[TokenTree], offset: usize) -> Option<TextRange> {
	let (first, last) = (tokens.first()?, tokens.last()?);

	Some(TextRange::new(offset + first.span().byte_range().start, offset + last.span().byte_range().end))
}

/// Plans deleting the files of removed modules: every module file of their subtrees, except files that kept modules
/// load too, and the directory of a module when all files in it go. Returns the paths to delete.
fn module_files(
	resolver: &Resolver<'_>,
	targets: &[ItemId],
	removed: &HashSet<ItemId>,
	warnings: &mut Vec<String>,
) -> Vec<PathBuf> {
	let ws = resolver.workspace();
	let mut kept: HashMap<&Path, ItemId> = HashMap::new();

	for krate in ws.crates() {
		for (module, data) in krate.items() {
			if let Some(file) = data.module_info().and_then(|info| info.file)
				&& !removed.contains(&module)
			{
				kept.entry(krate.file(file).path()).or_insert(module);
			}
		}
	}

	let mut deleted: Vec<PathBuf> = Vec::new();

	for &module in targets.iter().filter(|&&target| ws.item(target).kind == ItemKind::Module) {
		let mut files: Vec<&Path> = Vec::new();

		for file in subtree_files(ws, module) {
			match kept.get(file) {
				Some(&other) => warnings.push(format!(
					"`{}` is not deleted: the module `{}` loads it too",
					ws.display_path(file).display(),
					resolver.canonical_path(other),
				)),
				None if !files.contains(&file) => files.push(file),
				None => {}
			}
		}

		match module_directory(ws, module).filter(|(directory, _)| directory.is_dir()) {
			Some((directory, _)) if only_files_of(&directory, &files) => {
				deleted.extend(files.iter().filter(|file| !file.starts_with(&directory)).map(|file| file.to_path_buf()));
				deleted.push(directory);
			}

			Some((directory, true)) => {
				warnings.push(format!(
					"the directory `{}` is not deleted: it has files that are not part of the module `{}`",
					ws.display_path(&directory).display(),
					resolver.canonical_path(module),
				));
				deleted.extend(files.iter().map(|file| file.to_path_buf()));
			}

			_ => deleted.extend(files.iter().map(|file| file.to_path_buf())),
		}
	}

	let mut unique = Vec::with_capacity(deleted.len());

	for path in deleted {
		if !unique.contains(&path) {
			unique.push(path);
		}
	}

	unique
}

/// The files of a module and of the out-of-line modules inside of it.
fn subtree_files(ws: &Workspace, module: ItemId) -> Vec<&Path> {
	let mut files = Vec::new();
	let mut stack = vec![module];

	while let Some(module) = stack.pop() {
		let data = ws.item(module);

		if let Some(file) = data.module_info().filter(|info| !info.inline).and_then(|info| info.file) {
			files.push(ws.krate(module.krate()).file(file).path());
		}

		stack.extend(ws.children(module).filter(|&child| ws.item(child).kind == ItemKind::Module));
	}

	files
}

/// The directory where the files of a module's children are, and whether it is the module's own: the directory of a
/// `mod.rs` file, or the directory named after a module loaded from a non-`mod.rs` file (rather than, for a file loaded
/// with `#[path]` or an inline module, where the files happen to be).
fn module_directory(ws: &Workspace, module: ItemId) -> Option<(PathBuf, bool)> {
	let data = ws.item(module);
	let info = data.module_info()?;

	if info.inline {
		// where the files of its out-of-line children (declared without `#[path]`) are
		return ws.children(module).find_map(|child| {
			let child_data = ws.item(child);
			let child_info = child_data.module_info().filter(|info| !info.inline && child_data.attrs.path.is_none())?;
			let file = ws.krate(child.krate()).file(child_info.file?).path();
			let parent = file.parent()?;
			let directory = if file.file_name()? == "mod.rs" { parent.parent()? } else { parent };

			Some((directory.to_path_buf(), false))
		});
	}

	let file = ws.krate(module.krate()).file(info.file?).path();
	let parent = file.parent()?;

	if file.file_name().is_some_and(|name| name == "mod.rs") {
		return Some((parent.to_path_buf(), true));
	}

	match info.dir_owner {
		true => Some((parent.to_path_buf(), false)),
		false => Some((parent.join(data.name.as_deref().unwrap_or_default()), true)),
	}
}

/// Whether every file below `directory` is one of `files` (`false` when it cannot be listed).
fn only_files_of(directory: &Path, files: &[&Path]) -> bool {
	fn walk(directory: &Path, files: &[&Path]) -> io::Result<bool> {
		for entry in fs::read_dir(directory)? {
			let path = entry?.path();

			let only = match fs::symlink_metadata(&path)?.is_dir() {
				true => walk(&path, files)?,
				false => files.contains(&path.as_path()),
			};

			if !only {
				return Ok(false);
			}
		}

		Ok(true)
	}

	walk(directory, files).unwrap_or(false)
}

/// Certain references to removed items that are not removed themselves.
fn dangling(
	resolver: &Resolver<'_>,
	removed: &HashSet<ItemId>,
	deleted: &DeletedText<'_>,
	deleted_files: &[PathBuf],
	warnings: &mut Vec<String>,
) -> Vec<Reference> {
	let ws = resolver.workspace();
	let mut targets: Vec<ItemId> = (removed.iter().copied())
		.filter(|&item| ws.item(item).name.is_some() && ws.item(item).kind != ItemKind::Import)
		.collect();

	targets.sort();

	let references = match search_references(resolver, &targets) {
		Ok(references) => references,
		Err(message) => {
			warnings.push(format!("dangling references are not reported: searching for references failed ({message})"));
			return Vec::new();
		}
	};

	warnings.extend(references.notes);

	// the files of removed modules, which their crates no longer compile (even when they are kept)
	let dropped: HashSet<(CrateId, FileId)> = (removed.iter())
		.filter_map(|&item| {
			let info = ws.item(item).module_info().filter(|info| !info.inline)?;

			Some((item.krate(), info.file?))
		})
		.collect();

	let is_deleted = |reference: &Reference| {
		dropped.contains(&(reference.krate, reference.file))
			|| deleted_files.iter().any(|path| reference.path.starts_with(path))
			|| deleted
				.get(reference.path.as_path())
				.is_some_and(|(_, ranges)| ranges.iter().any(|range| range.contains_range(reference.range)))
	};

	let mut dangling: Vec<Reference> =
		references.references.into_iter().filter(|reference| reference.certain && !is_deleted(reference)).collect();

	dangling.sort_by(|a, b| (&a.path, a.range.start, a.range.end).cmp(&(&b.path, b.range.start, b.range.end)));
	dangling.dedup_by(|a, b| a.path == b.path && a.range == b.range);
	dangling
}

/// Searches for references, which is only a courtesy of the removal: a failure (a panic, as for syntax the search
/// does not handle) is reported rather than failing the removal.
fn search_references(resolver: &Resolver<'_>, targets: &[ItemId]) -> Result<References, String> {
	let search = || resolver.find_references(targets, &ReferenceOptions::default());

	std::panic::catch_unwind(std::panic::AssertUnwindSafe(search)).map_err(|payload| {
		(payload.downcast_ref::<&str>().map(|message| message.to_string()))
			.or_else(|| payload.downcast_ref::<String>().cloned())
			.unwrap_or_else(|| "unknown error".to_owned())
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The range of the unique occurrence of `needle`.
	fn find(text: &str, needle: &str) -> TextRange {
		let start = text.find(needle).unwrap_or_else(|| panic!("`{needle}` not found"));

		assert_eq!(text.rfind(needle), Some(start), "`{needle}` is ambiguous");

		TextRange::new(start, start + needle.len())
	}

	/// Removes the leaves (unique substrings) of the `use` item that is all of `text`.
	fn prune_leaves(text: &str, leaves: &[&str]) -> String {
		let leaves: Vec<TextRange> = leaves.iter().map(|leaf| find(text, leaf)).collect();
		let elements = use_elements(text, TextRange::new(0, text.len()), &leaves);

		delete(text, &element_removal_ranges(text, &elements))
	}

	/// The text without the (sorted, possibly overlapping) ranges.
	fn delete(text: &str, ranges: &[TextRange]) -> String {
		let mut out = String::new();
		let mut position = 0;

		for range in ranges {
			out.push_str(&text[position..range.start.max(position)]);
			position = position.max(range.end);
		}

		out + &text[position..]
	}

	#[test]
	fn removes_consecutive_elements_together() {
		fn remove_elements(text: &str, names: &[&str]) -> String {
			let elements: Vec<TextRange> = names.iter().map(|name| find(text, name)).collect();

			delete(text, &element_removal_ranges(text, &elements))
		}

		let text = "enum E { A, B, C, D }";

		assert_eq!(remove_elements(text, &["C", "D"]), "enum E { A, B }");
		assert_eq!(remove_elements(text, &["B", "C"]), "enum E { A, D }");
		assert_eq!(remove_elements(text, &["A", "D"]), "enum E { B, C }");
		assert_eq!(remove_elements(text, &["D", "B"]), "enum E { A, C }");
		assert_eq!(remove_elements(text, &["A", "B", "C", "D"]), "enum E { }");

		let text = "enum E {\n    A,\n    B,\n    C,\n}\n";

		assert_eq!(remove_elements(text, &["B", "C"]), "enum E {\n    A,\n}\n");
		assert_eq!(remove_elements(text, &["A", "C"]), "enum E {\n    B,\n}\n");
	}

	#[test]
	fn prunes_leaves_of_groups() {
		assert_eq!(prune_leaves("use a::{B, C, D};", &["B"]), "use a::{C, D};");
		assert_eq!(prune_leaves("use a::{B, C, D};", &["D"]), "use a::{B, C};");
		assert_eq!(prune_leaves("use a::{B, C, D};", &["B", "C"]), "use a::{D};");
		assert_eq!(prune_leaves("use a::{B, C, D};", &["C", "D"]), "use a::{B};");
		assert_eq!(prune_leaves("use a::{B, C, D};", &["B", "D"]), "use a::{C};");
		assert_eq!(prune_leaves("use a::{b::C, D};", &["b::C"]), "use a::{D};");
	}

	#[test]
	fn prunes_nested_groups_as_a_whole() {
		assert_eq!(prune_leaves("use a::{b::{X, Y}, Z};", &["X", "Y"]), "use a::{Z};");
		assert_eq!(prune_leaves("use a::{b::{X, Y}, Z};", &["Y"]), "use a::{b::{X}, Z};");
		assert_eq!(prune_leaves("use a::{b::{c::{X}, Y}, Z};", &["X"]), "use a::{b::{Y}, Z};");
		assert_eq!(prune_leaves("use a::{b::{c::{X}, Y}, Z};", &["X", "Y"]), "use a::{Z};");
		assert_eq!(prune_leaves("use a::{b::{X, Y}, {}, Z};", &["X", "Y"]), "use a::{{}, Z};");

		let text = "pub use a::{\n    b::{X, Y},\n    Z as W,\n    c::*,\n};";

		assert_eq!(prune_leaves(text, &["X", "Y"]), "pub use a::{\n    Z as W,\n    c::*,\n};");
		assert_eq!(prune_leaves(text, &["Z as W", "c::*"]), "pub use a::{\n    b::{X, Y},\n};");
	}

	#[test]
	fn prunes_rooted_elements() {
		assert_eq!(prune_leaves("use {::a::B, c::D};", &["::a::B"]), "use {c::D};");
		assert_eq!(prune_leaves("#[cfg(x)]\n/// docs\nuse ::a::{B, C};", &["C"]), "#[cfg(x)]\n/// docs\nuse ::a::{B};");
	}

	#[test]
	fn falls_back_to_leaves() {
		// no `use` keyword to find the tree after
		let text = "a::{B, C}";

		assert_eq!(use_elements(text, TextRange::new(0, text.len()), &[find(text, "B")]), [find(text, "B")]);
	}
}
