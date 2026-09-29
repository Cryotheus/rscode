//! Modifying operations: removing, renaming, replacing, inserting, and formatting items.
//!
//! Operations produce an [`EditSet`] describing every change, which can be previewed (as new file contents or a
//! diff) or applied. Applying is all-or-nothing: every edited file must still parse before anything is written, and
//! when writing fails partway, what was changed before is undone.

mod format;
mod remove;
mod rename;
mod replace;
pub(crate) mod trivia;

pub use format::Formatting;
pub use format::FmtOptions;
pub use format::format;
pub use remove::RemoveOptions;
pub use remove::Removal;
pub use remove::RemovedItem;
pub use remove::remove;
pub use rename::Collision;
pub use rename::Rename;
pub use rename::RenameOptions;
pub use rename::rename;
pub use replace::InsertOptions;
pub use replace::InsertPosition;
pub use replace::Insertion;
pub use replace::ReplaceOptions;
pub use replace::Replacement;
pub use replace::insert;
pub use replace::replace;
pub use replace::replaces_all_variants;
pub use rscode_fmt::emit::FileChange;

use crate::Error;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::Workspace;
use crate::path::ItemPath;
use crate::resolve::Resolver;
use crate::source::LineCol;
use crate::source::SourceFile;
use crate::source::TextRange;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Refuses a plain path whose last segment names `items` (what it resolves to) only through private imports of its
/// module, like `crate::Foo` with `use shapes::Foo;` in the crate root: it could mean what they import as well as the
/// imports themselves (named `use crate::Foo`). Re-exports (`pub use`) are paths to what they export.
///
/// When the module also binds the name otherwise (it defines an item of that name under other `cfg`s, or in another
/// namespace), the path names that instead: `items` loses what is named only through the private imports. So the path
/// of every item names it (and imports are named by `use` paths).
pub(crate) fn check_private_imports(resolver: &Resolver<'_>, path: &ItemPath, items: &mut Vec<ItemId>) -> Result<(), Error> {
	let Some(imports) = narrow_private_imports(resolver, path, items) else {
		return Ok(());
	};
	let candidates = imports.iter().chain(items.iter()).map(|&item| describe(resolver, item)).collect();

	Err(Error::Ambiguous { path: path.to_string(), candidates })
}

/// Drops from `items` what the last segment of a plain path names only through private imports of its module, when
/// the module also binds it otherwise (see [`check_private_imports`]). Returns the private imports of the modules that
/// bind it only through them, if any (each module decides on its own: those of a library and a binary, and `cfg`
/// variants).
pub(crate) fn narrow_private_imports(resolver: &Resolver<'_>, path: &ItemPath, items: &mut Vec<ItemId>) -> Option<Vec<ItemId>> {
	let found = resolver.private_imports_of(path, items);

	items.retain(|item| !found.shadowed.contains(item));
	(!found.imports.is_empty()).then_some(found.imports)
}

/// An item for messages: its canonical path (imports as `use` paths), kind (for crate roots, the kind of the crate),
/// location, and `cfg`.
pub(crate) fn describe(resolver: &Resolver<'_>, item: ItemId) -> String {
	let ws = resolver.workspace();
	let data = ws.item(item);
	let file = ws.file_of(item);
	let kind = match item.is_crate_root() {
		true => format!("{} crate root", ws.krate(item.krate()).kind()),
		false => data.kind.to_string(),
	};
	let mut text = format!(
		"`{}` ({kind}) at {}:{}",
		resolver.canonical_path(item).distinct(),
		ws.display_path(file.path()).display(),
		file.line_col(data.range.start),
	);

	if let Some(cfg) = ws.effective_cfg(item) {
		text.push_str(&format!(" with #[cfg({cfg})]"));
	}

	text
}

/// Whether items are (or are in) `impl` blocks whose headers differ, apart from `cfg`s: such as `impl From<u8> for X`
/// and `impl From<u16> for X`, which are not `cfg` variants of each other.
pub(crate) fn impl_headers_differ(ws: &Workspace, items: &[ItemId]) -> bool {
	let mut headers = Vec::new();

	for &item in items {
		let impl_block = match ws.item(item).kind {
			ItemKind::Impl => Some(item),
			_ => ws.parent(item).filter(|&parent| ws.item(parent).kind == ItemKind::Impl),
		};

		let header = impl_block
			.and_then(|impl_block| ws.item(impl_block).impl_info())
			.map(|info| (info.negative, info.trait_text.clone(), info.self_ty_text.clone()));

		if let Some(header) = header
			&& !headers.contains(&header)
		{
			headers.push(header);
		}
	}

	headers.len() > 1
}

/// A replacement of a range of text.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
pub struct TextEdit {
	/// The replaced range of the original text (empty for insertions).
	pub range: TextRange,

	/// The new text (empty for deletions).
	pub replacement: String,
}

/// The edits of one file.
#[derive(Debug, Clone)]
pub(crate) struct FileEdits {
	/// The text the edits' ranges refer to.
	pub(crate) original: Arc<str>,

	/// Edits in the order they were added.
	pub(crate) edits: Vec<TextEdit>,
}

impl FileEdits {
	/// Adds an edit unless an identical one exists.
	fn push(&mut self, edit: TextEdit) {
		if !self.edits.contains(&edit) {
			self.edits.push(edit);
		}
	}

	/// The text with every edit applied.
	///
	/// Edits are applied in order of position; insertions at the same position in the order they were added.
	/// Overlapping deletions are merged. Any other overlap is an error: two edits overlap if their ranges intersect,
	/// or if one is an insertion inside of (or at the start of) the other's range.
	fn apply(&self, path: &Path) -> Result<String, Error> {
		let original: &str = &self.original;
		let mut sorted: Vec<&TextEdit> = self.edits.iter().collect();

		// stable, so insertions at the same position keep their order
		sorted.sort_by_key(|edit| (edit.range.start, edit.range.end));

		let mut accepted: Vec<(TextRange, &str)> = Vec::with_capacity(sorted.len());

		// the last accepted replacement (non-empty range), which reaches furthest since they are disjoint and sorted
		let mut last_replaced: Option<usize> = None;
		let mut last_insertion: Option<usize> = None;

		for edit in sorted {
			let range = edit.range;

			check_range(path, original, range)?;

			let overlapped = last_replaced.filter(|&index| accepted[index].0.end > range.start);

			if range.is_empty() {
				if let Some(index) = overlapped {
					return Err(overlap(path, accepted[index].0, range));
				}

				last_insertion = Some(range.start);
				accepted.push((range, &edit.replacement));
				continue;
			}

			if last_insertion == Some(range.start) {
				return Err(overlap(path, TextRange::new(range.start, range.start), range));
			}

			match overlapped {
				Some(index) if accepted[index].1.is_empty() && edit.replacement.is_empty() => {
					let merged = &mut accepted[index].0;

					merged.end = merged.end.max(range.end);
				}
				Some(index) => return Err(overlap(path, accepted[index].0, range)),
				None => {
					last_replaced = Some(accepted.len());
					accepted.push((range, &edit.replacement));
				}
			}
		}

		let mut text = String::with_capacity(original.len());
		let mut position = 0;

		for (range, replacement) in accepted {
			text.push_str(&original[position..range.start]);
			text.push_str(replacement);
			position = range.end;
		}

		text.push_str(&original[position..]);

		Ok(text)
	}
}

/// A set of changes to files.
#[derive(Debug, Default, Clone)]
pub struct EditSet {
	pub(crate) files: BTreeMap<PathBuf, FileEdits>,

	/// Files or directories to move, after text edits are written. Edits are keyed by the original path.
	pub(crate) moves: Vec<(PathBuf, PathBuf)>,

	/// Files or directories to delete.
	pub(crate) deletions: Vec<PathBuf>,
}

impl EditSet {
	/// An empty edit set.
	pub fn new() -> Self {
		Self::default()
	}

	/// Adds a replacement. Identical duplicate edits (from files loaded by multiple crates) are merged.
	///
	/// Edits of a file are keyed by its path and refer to the text of the first [`SourceFile`] given for that path.
	pub fn replace(&mut self, file: &SourceFile, range: TextRange, replacement: impl Into<String>) {
		let edit = TextEdit { range, replacement: replacement.into() };

		self.files
			.entry(file.path().to_path_buf())
			.or_insert_with(|| FileEdits { original: file.shared_text().clone(), edits: Vec::new() })
			.push(edit);
	}

	/// Replaces a file's whole text.
	pub fn replace_file(&mut self, file: &SourceFile, text: impl Into<String>) {
		self.replace(file, TextRange::new(0, file.text().len()), text);
	}

	/// Moves (renames) a file or directory. Duplicate moves are ignored.
	pub fn move_path(&mut self, from: impl Into<PathBuf>, to: impl Into<PathBuf>) {
		let entry = (from.into(), to.into());

		if !self.moves.contains(&entry) {
			self.moves.push(entry);
		}
	}

	/// Deletes a file or directory (recursively). Duplicate deletions are ignored.
	pub fn delete_path(&mut self, path: impl Into<PathBuf>) {
		let path = path.into();

		if !self.deletions.contains(&path) {
			self.deletions.push(path);
		}
	}

	/// Merges another edit set into this one (identical edits, moves, and deletions are merged).
	pub fn extend(&mut self, other: EditSet) {
		for (path, file) in other.files {
			match self.files.entry(path) {
				Entry::Vacant(entry) => {
					entry.insert(file);
				}
				Entry::Occupied(mut entry) => {
					let target = entry.get_mut();

					for edit in file.edits {
						target.push(edit);
					}
				}
			}
		}

		for (from, to) in other.moves {
			self.move_path(from, to);
		}

		for path in other.deletions {
			self.delete_path(path);
		}
	}

	/// Whether there is nothing to do.
	pub fn is_empty(&self) -> bool {
		self.files.is_empty() && self.moves.is_empty() && self.deletions.is_empty()
	}

	/// Planned moves, in order: `(from, to)`.
	pub fn moves(&self) -> &[(PathBuf, PathBuf)] {
		&self.moves
	}

	/// Planned deletions.
	pub fn deletions(&self) -> &[PathBuf] {
		&self.deletions
	}

	/// Paths of the files with text edits.
	pub fn edited_files(&self) -> impl Iterator<Item = &Path> {
		self.files.keys().map(PathBuf::as_path)
	}

	/// The text edits of a file, in the order they were added (none for a file that is not edited). Their ranges refer
	/// to the text the file was loaded with; [`EditSet::preview`] applies them.
	pub fn edits(&self, path: &Path) -> &[TextEdit] {
		self.files.get(path).map_or(&[], |file| &file.edits)
	}

	/// Computes the new contents of every edited file, sorted by path (including files whose edits do not change
	/// them; see [`FileChange::is_changed`]).
	///
	/// Fails with [`Error::OverlappingEdits`] when edits of a file overlap (non-identical), and with
	/// [`Error::EditBreaksSyntax`] when an edited `.rs` file no longer parses.
	///
	/// Edited files are parsed on a thread of their own (see [Threads](crate#threads)).
	pub fn preview(&self) -> Result<Vec<FileChange>, Error> {
		crate::source::isolated(|| {
			let mut changes = Vec::with_capacity(self.files.len());

			for (path, file) in &self.files {
				let formatted = file.apply(path)?;

				if formatted != *file.original && is_rust_file(path) {
					check_syntax(path, &formatted)?;
				}

				changes.push(FileChange { path: path.clone(), original: file.original.to_string(), formatted });
			}

			Ok(changes)
		})
	}

	/// A unified diff of all text edits, followed by lines describing moves (`rename <from> -> <to>`) and deletions
	/// (`delete <path>`). Paths are shown as stored (absolute when loaded from a workspace); see
	/// [`EditSet::diff_relative_to`].
	pub fn diff(&self) -> Result<String, Error> {
		self.render_diff(Path::to_path_buf)
	}

	/// Like [`EditSet::diff`], with paths shown relative to `base` when they are inside of it (for example
	/// [`Workspace::root`](crate::Workspace::root)).
	pub fn diff_relative_to(&self, base: &Path) -> Result<String, Error> {
		self.render_diff(|path| path.strip_prefix(base).unwrap_or(path).to_path_buf())
	}

	fn render_diff(&self, display: impl Fn(&Path) -> PathBuf) -> Result<String, Error> {
		let mut changes = self.preview()?;

		for change in &mut changes {
			change.path = display(&change.path);
		}

		let mut diff = rscode_fmt::emit::unified_diff(&changes, 3);

		if !(diff.is_empty() || diff.ends_with('\n')) {
			diff.push('\n');
		}

		for (from, to) in &self.moves {
			diff.push_str(&format!("rename {} -> {}\n", display(from).display(), display(to).display()));
		}

		for path in &self.deletions {
			diff.push_str(&format!("delete {}\n", display(path).display()));
		}

		Ok(diff)
	}

	/// Validates everything (like [`EditSet::preview`]), then writes files, then performs moves and deletions.
	///
	/// Before anything is written, this also checks that every edited file still has the contents its edits were
	/// computed from (so changes made after loading are not overwritten), that moved and deleted paths exist, and
	/// that no move would overwrite an existing path. (A move to the path of the moved file or directory in another
	/// case, which names it already on file systems that ignore case, as on Windows and macOS, changes the case of its
	/// name.)
	///
	/// Applying is all-or-nothing: new contents are written to temporary files next to their files first, and deleted
	/// paths are moved to temporary names, to be removed once everything else succeeded. When a step fails (such as on
	/// Windows, on a file that another process has open, or on a directory with such a file), the steps before it are
	/// undone, and [`Error::Apply`] tells what failed (and what could not be undone, if anything). Symbolic links are
	/// written through, and permissions are preserved.
	pub fn apply(&self) -> Result<Applied, Error> {
		let changes = self.preview()?;
		let to_write: Vec<&FileChange> = changes.iter().filter(|change| change.is_changed()).collect();

		for change in &to_write {
			check_unmodified(change)?;
		}

		let deletions = self.effective_deletions();

		self.check_moves_and_deletions(&deletions)?;

		let mut transaction = Transaction::default();

		if let Err(Failure { path, source }) = transaction.perform(&to_write, &self.moves, &deletions) {
			return Err(Error::Apply { path, source, kept: transaction.undo() });
		}

		Ok(Applied {
			written: to_write.iter().map(|change| change.path.clone()).collect(),
			moved: self.moves.clone(),
			deleted: deletions.iter().map(|path| path.to_path_buf()).collect(),
			warnings: transaction.commit(),
		})
	}

	/// Deletions that are not inside of another deleted directory.
	fn effective_deletions(&self) -> Vec<&Path> {
		self.deletions
			.iter()
			.filter(|path| !self.deletions.iter().any(|other| other != *path && path.starts_with(other)))
			.map(PathBuf::as_path)
			.collect()
	}

	/// Checks that moves and deletions can be performed in order, tracking which paths they create and remove.
	fn check_moves_and_deletions(&self, deletions: &[&Path]) -> Result<(), Error> {
		// later events win: (path, whether it exists afterwards, with everything below it)
		let mut events: Vec<(&Path, bool)> = Vec::new();
		let exists = |events: &[(&Path, bool)], path: &Path| {
			events
				.iter()
				.rev()
				.find(|(event, _)| path.starts_with(event))
				.map_or_else(|| fs::symlink_metadata(path).is_ok(), |&(_, exists)| exists)
		};

		for (from, to) in &self.moves {
			if !exists(&events, from) {
				return Err(Error::io(
					from,
					io::Error::new(io::ErrorKind::NotFound, "cannot move a path that does not exist"),
				));
			}

			// (on file systems that ignore case, a path of `from` in another case names it: the move changes the case)
			let on_disk = |path: &Path| !events.iter().any(|(event, _)| path.starts_with(event));

			if exists(&events, to) && !(on_disk(from) && on_disk(to) && names_same_entry(from, to)) {
				let message = format!("cannot move `{}` here: the destination exists", from.display());

				return Err(Error::io(to, io::Error::new(io::ErrorKind::AlreadyExists, message)));
			}

			if to.starts_with(from) {
				return Err(Error::io(
					to,
					io::Error::new(io::ErrorKind::InvalidInput, "cannot move a directory into itself"),
				));
			}

			events.push((from, false));
			events.push((to, true));
		}

		for &path in deletions {
			if !exists(&events, path) {
				return Err(Error::io(
					path,
					io::Error::new(io::ErrorKind::NotFound, "cannot delete a path that does not exist"),
				));
			}

			events.push((path, false));
		}

		Ok(())
	}
}

/// What [`EditSet::apply`] did.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Applied {
	/// Files whose contents were written (unchanged files are skipped).
	pub written: Vec<PathBuf>,

	/// Paths that were moved, in order.
	pub moved: Vec<(PathBuf, PathBuf)>,

	/// Paths that were deleted (not repeating paths inside of deleted directories).
	pub deleted: Vec<PathBuf>,

	/// Problems that did not keep the edit from being applied (such as a deleted path that could not be removed from
	/// the temporary name it was moved to).
	pub warnings: Vec<String>,
}

fn overlap(path: &Path, first: TextRange, second: TextRange) -> Error {
	Error::OverlappingEdits { path: path.to_path_buf(), first: first.as_range(), second: second.as_range() }
}

/// Checks that a range is within the text and on character boundaries.
fn check_range(path: &Path, text: &str, range: TextRange) -> Result<(), Error> {
	if range.start <= range.end && text.is_char_boundary(range.start) && text.is_char_boundary(range.end) {
		return Ok(());
	}

	Err(Error::InvalidSource(format!(
		"{}: edit of bytes {}..{} is outside of the text ({} bytes) or splits a character",
		path.display(),
		range.start,
		range.end,
		text.len(),
	)))
}

fn is_rust_file(path: &Path) -> bool {
	path.extension().is_some_and(|extension| extension == "rs")
}

/// Checks that an edited text still parses.
fn check_syntax(path: &Path, text: &str) -> Result<(), Error> {
	let Err(error) = syn::parse_file(text) else {
		return Ok(());
	};

	// spans are thread-local: read the location here, on the parsing thread
	let start = error.span().start();

	Err(Error::EditBreaksSyntax {
		path: path.to_path_buf(),
		location: LineCol {
			line: start.line,
			column: start.column + 1 + usize::from(start.line == 1 && text.starts_with('\u{feff}')),
		},
		message: error.to_string(),
	})
}

/// Checks that a file on disk still has the contents the edits were computed from.
fn check_unmodified(change: &FileChange) -> Result<(), Error> {
	let current = fs::read(&change.path).map_err(|source| Error::io(&change.path, source))?;

	if current != change.original.as_bytes() {
		let message = "the file changed since it was loaded; reload and try again";

		return Err(Error::io(&change.path, io::Error::other(message)));
	}

	Ok(())
}

/// Whether moving `from` to `to` would overwrite an existing file or directory: whether `to` names one, other than
/// `from` itself in another case (see [`names_same_entry`]).
pub(crate) fn destination_exists(from: &Path, to: &Path) -> bool {
	fs::symlink_metadata(to).is_ok() && !names_same_entry(from, to)
}

/// Whether `to` (which exists) names the entry of its directory that `from` names: whether their names differ only in
/// case, the directory has a single entry of that name in any case (so the file system ignores case in it, as on
/// Windows and macOS), and both paths lead to the same file (and not to two files whose names differ otherwise, such
/// as in how their accents are encoded, on file systems that ignore that but not case).
fn names_same_entry(from: &Path, to: &Path) -> bool {
	let key = to.file_name().and_then(case_key);

	if from == to || from.parent() != to.parent() || key.is_none() || from.file_name().and_then(case_key) != key {
		return false;
	}

	let directory = to.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
	let Ok(entries) = fs::read_dir(directory) else {
		return false;
	};

	entries.filter_map(Result::ok).filter(|entry| case_key(&entry.file_name()) == key).count() == 1
		&& same_file::is_same_file(from, to).unwrap_or(false)
}

/// A file name in upper case (as Windows compares names), to compare names ignoring case.
fn case_key(name: &OsStr) -> Option<String> {
	name.to_str().map(str::to_uppercase)
}

/// A step of [`EditSet::apply`] that failed: the path it failed on, and why.
#[derive(Debug)]
struct Failure {
	path: PathBuf,
	source: io::Error,
}

impl Failure {
	fn at(path: &Path, source: io::Error) -> Self {
		Self { path: path.to_path_buf(), source }
	}
}

/// The steps [`EditSet::apply`] performed, which are undone when a later one fails.
#[derive(Default)]
struct Transaction<'a> {
	steps: Vec<Step<'a>>,
}

/// A step of [`EditSet::apply`].
enum Step<'a> {
	/// A file was replaced with its new contents (the file of `path`, or the file a symbolic link at `path` points to);
	/// undone by writing its original contents back, unless it changed since.
	Written { path: &'a Path, original: &'a str, written: &'a str },

	/// A directory was created to move a path into; undone by removing it (if it is empty).
	Created(PathBuf),

	/// A file or directory was moved, maybe through a temporary name (to change the case of its name); undone by moving
	/// it back (through that name again).
	Moved { from: PathBuf, to: PathBuf, through: Option<PathBuf> },

	/// A file or directory to delete was moved to a temporary name, to be removed once every step succeeded; undone by
	/// moving it back.
	Deleted { path: &'a Path, temporary: PathBuf },
}

impl<'a> Transaction<'a> {
	/// Writes files, then moves paths, then moves the paths to delete to temporary names, until a step fails.
	fn perform(
		&mut self,
		changes: &[&'a FileChange],
		moves: &[(PathBuf, PathBuf)],
		deletions: &[&'a Path],
	) -> Result<(), Failure> {
		self.write(changes)?;

		for (from, to) in moves {
			self.move_path(from, to)?;
		}

		for &path in deletions {
			let temporary = unused_temporary_path(path);

			fs::rename(path, &temporary).map_err(|source| Failure::at(path, source))?;
			self.steps.push(Step::Deleted { path, temporary });
		}

		Ok(())
	}

	/// Writes every change to a temporary file, then renames them all over their files (so that failing to write one
	/// changes nothing).
	fn write(&mut self, changes: &[&'a FileChange]) -> Result<(), Failure> {
		let mut staged = Vec::with_capacity(changes.len());

		for change in changes {
			match stage(&change.path, &change.formatted) {
				Ok(file) => staged.push(file),
				Err(failure) => {
					remove_temporaries(&staged);
					return Err(failure);
				}
			}
		}

		for (index, (file, change)) in staged.iter().zip(changes).enumerate() {
			if let Err(source) = fs::rename(&file.temporary, &file.target) {
				remove_temporaries(&staged[index..]);
				return Err(Failure::at(file.path, source));
			}

			self.steps.push(Step::Written {
				path: &change.path,
				original: &change.original,
				written: &change.formatted,
			});
		}

		Ok(())
	}

	/// Moves a path, creating the directories it moves into. A move that only changes the case of the name goes through
	/// a temporary name: moving a path to a name it has already (ignoring case) changes nothing on some file systems
	/// that ignore case (Linux's, and under Wine).
	fn move_path(&mut self, from: &Path, to: &Path) -> Result<(), Failure> {
		self.create_parents(to)?;

		if fs::symlink_metadata(to).is_err() {
			return self.rename(from, to);
		}

		if !names_same_entry(from, to) {
			let message = format!("cannot move `{}` here: the destination exists", from.display());

			return Err(Failure::at(to, io::Error::new(io::ErrorKind::AlreadyExists, message)));
		}

		let temporary = unused_temporary_path(from);

		fs::rename(from, &temporary).map_err(|source| Failure::at(from, source))?;

		let moved = fs::rename(&temporary, to);
		let step = match moved {
			Ok(()) => Step::Moved { from: from.to_path_buf(), to: to.to_path_buf(), through: Some(temporary) },

			// (undone like a move to the temporary name)
			Err(_) => Step::Moved { from: from.to_path_buf(), to: temporary, through: None },
		};

		self.steps.push(step);
		moved.map_err(|source| Failure::at(from, source))
	}

	fn rename(&mut self, from: &Path, to: &Path) -> Result<(), Failure> {
		fs::rename(from, to).map_err(|source| Failure::at(from, source))?;
		self.steps.push(Step::Moved { from: from.to_path_buf(), to: to.to_path_buf(), through: None });
		Ok(())
	}

	/// Creates the missing directories a path is in.
	fn create_parents(&mut self, path: &Path) -> Result<(), Failure> {
		let missing: Vec<&Path> = (path.ancestors().skip(1))
			.take_while(|directory| !directory.as_os_str().is_empty() && fs::symlink_metadata(directory).is_err())
			.collect();

		// the outermost first
		for &directory in missing.iter().rev() {
			match fs::create_dir(directory) {
				Ok(()) => self.steps.push(Step::Created(directory.to_path_buf())),

				// (created by someone else in the meantime)
				Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
				Err(source) => return Err(Failure::at(directory, source)),
			}
		}

		Ok(())
	}

	/// Undoes every step, the last first. Returns the changes that stay because undoing them failed, one line each.
	fn undo(self) -> Vec<String> {
		self.steps.into_iter().rev().filter_map(|step| step.undo().err()).collect()
	}

	/// Removes the paths moved to temporary names for deletion. The edit is applied by then, so failing to remove one
	/// is only a warning (returned).
	fn commit(self) -> Vec<String> {
		let mut warnings = Vec::new();

		for step in self.steps {
			if let Step::Deleted { path, temporary } = step
				&& let Err(error) = delete_path(&temporary)
			{
				warnings.push(format!(
					"`{}` was deleted, but removing it from its temporary name `{}` failed: {error}",
					path.display(),
					temporary.display()
				));
			}
		}

		warnings
	}
}

impl Step<'_> {
	/// Undoes the step, or tells what stays changed.
	fn undo(&self) -> Result<(), String> {
		match self {
			Step::Written { path, original, written } => restore(path, original, written)
				.map_err(|error| format!("`{}` was not restored to its original contents ({error})", path.display())),

			Step::Created(directory) => fs::remove_dir(directory)
				.map_err(|error| format!("the new directory `{}` stays ({error})", directory.display())),

			Step::Moved { from, to, through } => match through {
				None => move_back(to, from).map_err(|error| stays_moved(from, to, error)),
				Some(temporary) => {
					move_back(to, temporary).map_err(|error| stays_moved(from, to, error))?;
					move_back(temporary, from).map_err(|error| stays_moved(from, temporary, error))
				}
			},

			Step::Deleted { path, temporary } => {
				move_back(temporary, path).map_err(|error| stays_moved(path, temporary, error))
			}
		}
	}
}

/// That a moved path stays where it was moved (as undoing the move failed), for [`Error::Apply`].
fn stays_moved(path: &Path, at: &Path, error: io::Error) -> String {
	format!("`{}` stays moved to `{}` ({error})", path.display(), at.display())
}

/// A file written next to the file it will replace.
struct Staged<'a> {
	/// The path the edits are keyed by.
	path: &'a Path,

	/// The file to replace (`path`, or the file a symbolic link at `path` points to).
	target: PathBuf,

	temporary: PathBuf,
}

fn remove_temporaries(staged: &[Staged<'_>]) {
	for file in staged {
		// best effort: the temporary file may not exist anymore
		let _ = fs::remove_file(&file.temporary);
	}
}

/// Writes contents to a new temporary file in the directory of the file of `path` (which may be a symbolic link to it),
/// with the file's permissions.
fn stage<'a>(path: &'a Path, contents: &str) -> Result<Staged<'a>, Failure> {
	let failure = |source| Failure::at(path, source);
	let is_link = fs::symlink_metadata(path).map_err(failure)?.file_type().is_symlink();
	let target = if is_link { fs::canonicalize(path).map_err(failure)? } else { path.to_path_buf() };
	let permissions = fs::metadata(&target).map_err(failure)?.permissions();

	loop {
		let temporary = temporary_path(&target);
		let mut file = match fs::OpenOptions::new().write(true).create_new(true).open(&temporary) {
			Ok(file) => file,
			Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
			Err(error) => return Err(failure(error)),
		};
		let written = file
			.write_all(contents.as_bytes())
			.and_then(|()| file.set_permissions(permissions.clone()))
			.and_then(|()| file.sync_all());

		if let Err(error) = written {
			drop(file);

			// best effort: the error that matters is the write error
			let _ = fs::remove_file(&temporary);

			return Err(failure(error));
		}

		return Ok(Staged { path, target, temporary });
	}
}

/// Writes a file's original contents back (like new contents are written), unless it does not have the contents
/// written to it anymore: someone else's changes are not undone.
fn restore(path: &Path, original: &str, written: &str) -> io::Result<()> {
	if fs::read(path)? != written.as_bytes() {
		return Err(io::Error::other("it changed after the edit wrote it"));
	}

	let file = stage(path, original).map_err(|failure| failure.source)?;

	fs::rename(&file.temporary, &file.target).inspect_err(|_| remove_temporaries(std::slice::from_ref(&file)))
}

/// Moves a path back to where it was, unless something else is there now.
fn move_back(from: &Path, to: &Path) -> io::Result<()> {
	if fs::symlink_metadata(to).is_ok() {
		return Err(io::Error::new(io::ErrorKind::AlreadyExists, "something else is there now"));
	}

	fs::rename(from, to)
}

fn delete_path(path: &Path) -> io::Result<()> {
	match fs::symlink_metadata(path)?.is_dir() {
		true => fs::remove_dir_all(path),
		false => fs::remove_file(path),
	}
}

/// A new path next to `path`, for a temporary file or name: `.<name>.rscode-<process>-<count>.tmp`.
fn temporary_path(path: &Path) -> PathBuf {
	static COUNTER: AtomicU64 = AtomicU64::new(0);

	let count = COUNTER.fetch_add(1, Ordering::Relaxed);
	let name = path.file_name().map_or_else(|| "file".into(), |name| name.to_string_lossy());

	path.with_file_name(format!(".{name}.rscode-{}-{count}.tmp", std::process::id()))
}

/// A temporary path next to `path` (see [`temporary_path`]) that names nothing yet.
fn unused_temporary_path(path: &Path) -> PathBuf {
	loop {
		let temporary = temporary_path(path);

		if fs::symlink_metadata(&temporary).is_err() {
			return temporary;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn file(path: &str, text: &str) -> SourceFile {
		SourceFile::new(PathBuf::from(path), text)
	}

	fn preview_text(edits: &EditSet) -> String {
		let changes = edits.preview().unwrap();

		assert_eq!(changes.len(), 1);
		changes.into_iter().next().unwrap().formatted
	}

	fn range(start: usize, end: usize) -> TextRange {
		TextRange::new(start, end)
	}

	#[test]
	fn applies_edits_in_order() {
		let source = file("/x/a.rs", "fn a() {}\nfn b() {}\n");
		let mut edits = EditSet::new();

		edits.replace(&source, range(13, 14), "c");
		edits.replace(&source, range(3, 4), "z");
		edits.replace(&source, range(0, 0), "// head\n");
		edits.replace(&source, range(20, 20), "fn d() {}\n");

		assert_eq!(preview_text(&edits), "// head\nfn z() {}\nfn c() {}\nfn d() {}\n");
	}

	#[test]
	fn lists_the_edits_of_files() {
		let source = file("/x/a.rs", "fn a() {}\n");
		let mut edits = EditSet::new();

		edits.replace(&source, range(3, 4), "z");
		edits.replace(&source, range(0, 0), "// head\n");
		edits.replace(&source, range(3, 4), "z");

		assert_eq!(
			edits.edits(Path::new("/x/a.rs")),
			[
				TextEdit { range: range(3, 4), replacement: "z".to_owned() },
				TextEdit { range: range(0, 0), replacement: "// head\n".to_owned() }
			]
		);
		assert!(edits.edits(Path::new("/x/b.rs")).is_empty());
	}

	#[test]
	fn insertions_at_one_position_keep_their_order() {
		let source = file("/x/a.rs", "struct A;\n");
		let mut edits = EditSet::new();

		edits.replace(&source, range(10, 10), "struct B;\n");
		edits.replace(&source, range(10, 10), "struct C;\n");
		edits.replace(&source, range(0, 9), "struct Z;");
		edits.replace(&source, range(10, 10), "struct D;\n");

		assert_eq!(preview_text(&edits), "struct Z;\nstruct B;\nstruct C;\nstruct D;\n");
	}

	#[test]
	fn merges_identical_edits() {
		let source = file("/x/a.rs", "fn a() {}");
		let mut edits = EditSet::new();

		edits.replace(&source, range(3, 4), "b");
		edits.replace(&source, range(3, 4), "b");
		edits.replace(&source, range(0, 0), "// x\n");
		edits.replace(&source, range(0, 0), "// x\n");

		assert_eq!(edits.files[Path::new("/x/a.rs")].edits.len(), 2);
		assert_eq!(preview_text(&edits), "// x\nfn b() {}");
	}

	#[test]
	fn rejects_overlapping_edits() {
		// edits (start, end, replacement), and the ranges reported as overlapping
		type Case<'a> = (&'a [(usize, usize, &'a str)], (usize, usize), (usize, usize));

		let source = file("/x/a.rs", "fn abc() {}");
		let cases: &[Case<'_>] = &[
			// intersecting replacements
			(&[(3, 5, "x"), (4, 6, "y")], (3, 5), (4, 6)),
			// same range, different text
			(&[(3, 6, "x"), (3, 6, "y")], (3, 6), (3, 6)),
			// containment
			(&[(0, 11, "x"), (3, 4, "y")], (0, 11), (3, 4)),
			// an insertion inside of a replaced range
			(&[(3, 6, "x"), (4, 4, "y")], (3, 6), (4, 4)),
			// an insertion at the start of a replaced range
			(&[(3, 6, "x"), (3, 3, "y")], (3, 3), (3, 6)),
			// a deletion intersecting a replacement
			(&[(3, 6, ""), (5, 8, "y")], (3, 6), (5, 8)),
		];

		for (index, (edits, first, second)) in cases.iter().enumerate() {
			let mut set = EditSet::new();

			for &(start, end, text) in *edits {
				set.replace(&source, range(start, end), text);
			}

			match set.preview() {
				Err(Error::OverlappingEdits { path, first: found_first, second: found_second }) => {
					assert_eq!(path, Path::new("/x/a.rs"));
					assert_eq!((found_first.start, found_first.end), *first, "case {index}");
					assert_eq!((found_second.start, found_second.end), *second, "case {index}");
				}
				other => panic!("case {index}: expected overlapping edits, got {other:?}"),
			}
		}
	}

	#[test]
	fn adjacent_edits_do_not_overlap() {
		let source = file("/x/a.rs", "fn abc() {}");
		let mut edits = EditSet::new();

		edits.replace(&source, range(3, 4), "x");
		edits.replace(&source, range(4, 5), "y");
		edits.replace(&source, range(5, 5), "z");
		edits.replace(&source, range(5, 6), "w");

		// the insertion at 5 is at the start of the replaced 5..6
		assert!(matches!(edits.preview(), Err(Error::OverlappingEdits { .. })));

		let mut edits = EditSet::new();

		edits.replace(&source, range(3, 4), "x");
		edits.replace(&source, range(4, 5), "y");
		edits.replace(&source, range(6, 6), "_z");

		assert_eq!(preview_text(&edits), "fn xyc_z() {}");
	}

	#[test]
	fn merges_overlapping_deletions() {
		let source = file("/x/a.rs", "struct A;\n\nstruct B;\n\nstruct C;\n");
		let mut edits = EditSet::new();

		// `A` with the blank line after it, and `B` with the blank line before it
		edits.replace(&source, range(0, 11), "");
		edits.replace(&source, range(10, 20), "");
		edits.replace(&source, range(12, 15), "");

		assert_eq!(preview_text(&edits), "\n\nstruct C;\n");

		let mut edits = EditSet::new();

		edits.replace(&source, range(0, 11), "");
		edits.replace(&source, range(5, 5), "x");

		assert!(matches!(edits.preview(), Err(Error::OverlappingEdits { .. })));
	}

	#[test]
	fn rejects_edits_outside_of_the_text() {
		let source = file("/x/a.rs", "struct Ü;");
		let mut edits = EditSet::new();

		edits.replace(&source, range(8, 30), "");
		assert!(matches!(edits.preview(), Err(Error::InvalidSource(_))));

		let mut edits = EditSet::new();

		// inside of the two-byte `Ü`
		edits.replace(&source, range(8, 8), "x");
		assert!(matches!(edits.preview(), Err(Error::InvalidSource(_))));

		let mut edits = EditSet::new();

		edits.replace(&source, TextRange { start: 5, end: 3 }, "x");
		assert!(matches!(edits.preview(), Err(Error::InvalidSource(_))));
	}

	#[test]
	fn rejects_edits_that_break_syntax() {
		let source = file("/x/src/a.rs", "fn a() {}\nfn b() {\n    let x = 1;\n}\n");
		let mut edits = EditSet::new();

		// `let x = ;`
		edits.replace(&source, range(31, 32), "");

		match edits.preview() {
			Err(Error::EditBreaksSyntax { path, location, message }) => {
				assert_eq!(path, Path::new("/x/src/a.rs"));
				assert_eq!(location, LineCol { line: 3, column: 13 });
				assert!(!message.is_empty());
			}
			other => panic!("expected a syntax error, got {other:?}"),
		}

		// with a byte order mark, columns of the first line count from after it
		let source = file("/x/src/b.rs", "\u{feff}fn a() {}");
		let mut edits = EditSet::new();

		// `fn 1() {}`
		edits.replace(&source, range(6, 7), "1");

		match edits.preview() {
			Err(Error::EditBreaksSyntax { location, .. }) => assert_eq!(location, LineCol { line: 1, column: 5 }),
			other => panic!("expected a syntax error, got {other:?}"),
		}

		// other files are not parsed
		let manifest = file("/x/Cargo.toml", "[package]\n");
		let mut edits = EditSet::new();

		edits.replace(&manifest, range(0, 0), "}{");
		assert_eq!(preview_text(&edits), "}{[package]\n");
	}

	#[test]
	fn unchanged_files_are_not_validated() {
		let source = file("/x/a.rs", "fn broken( {}");
		let mut edits = EditSet::new();

		edits.replace(&source, range(3, 9), "broken");

		let changes = edits.preview().unwrap();

		assert!(!changes[0].is_changed());
	}

	#[test]
	fn previews_are_sorted_by_path() {
		let mut edits = EditSet::new();

		for name in ["/x/c.rs", "/x/a.rs", "/x/b.rs"] {
			edits.replace(&file(name, "struct S;"), range(7, 8), "T");
		}

		let paths: Vec<PathBuf> = edits.preview().unwrap().into_iter().map(|change| change.path).collect();

		assert_eq!(paths, [PathBuf::from("/x/a.rs"), PathBuf::from("/x/b.rs"), PathBuf::from("/x/c.rs")]);
		assert_eq!(edits.edited_files().count(), 3);
	}

	#[test]
	fn replaces_whole_files() {
		let source = file("/x/a.rs", "fn   a( ) { }");
		let mut edits = EditSet::new();

		edits.replace_file(&source, "fn a() {}\n");
		assert_eq!(preview_text(&edits), "fn a() {}\n");
	}

	#[test]
	fn extends_edit_sets() {
		let a = file("/x/a.rs", "struct A;");
		let b = file("/x/b.rs", "struct B;");
		let mut first = EditSet::new();
		let mut second = EditSet::new();

		first.replace(&a, range(7, 8), "X");
		first.move_path("/x/m.rs", "/x/n.rs");
		first.delete_path("/x/old.rs");
		second.replace(&a, range(7, 8), "X");
		second.replace(&a, range(0, 0), "pub ");
		second.replace(&b, range(7, 8), "Y");
		second.move_path("/x/m.rs", "/x/n.rs");
		second.move_path("/x/p.rs", "/x/q.rs");
		second.delete_path("/x/old.rs");
		second.delete_path("/x/older.rs");

		first.extend(second);

		assert_eq!(first.files[Path::new("/x/a.rs")].edits.len(), 2);
		assert_eq!(first.moves().len(), 2);
		assert_eq!(first.deletions(), [PathBuf::from("/x/old.rs"), PathBuf::from("/x/older.rs")]);

		let changes = first.preview().unwrap();

		assert_eq!(changes[0].formatted, "pub struct X;");
		assert_eq!(changes[1].formatted, "struct Y;");
		assert!(!first.is_empty());
		assert!(EditSet::new().is_empty());
	}

	#[test]
	fn effective_deletions_skip_nested_paths() {
		let mut edits = EditSet::new();

		edits.delete_path("/x/a/b.rs");
		edits.delete_path("/x/a");
		edits.delete_path("/x/ab.rs");
		edits.delete_path("/x/a");

		assert_eq!(edits.deletions().len(), 3);
		assert_eq!(edits.effective_deletions(), [Path::new("/x/a"), Path::new("/x/ab.rs")]);
	}

	/// A new directory for temporary test data, removed when dropped.
	struct TempDir(PathBuf);

	impl TempDir {
		fn new(name: &str) -> Self {
			let path = std::env::temp_dir().join(format!("rscode-edit-{}-{name}", std::process::id()));

			let _ = fs::remove_dir_all(&path);
			fs::create_dir_all(&path).unwrap();
			Self(path)
		}

		fn path(&self, relative: &str) -> PathBuf {
			self.0.join(relative)
		}

		/// Writes a file (creating its directory).
		fn write(&self, relative: &str, text: &str) -> PathBuf {
			let path = self.path(relative);

			fs::create_dir_all(path.parent().unwrap()).unwrap();
			fs::write(&path, text).unwrap();
			path
		}

		/// Every directory and file (with its contents) below the directory, with `/` separators, sorted.
		fn snapshot(&self) -> Vec<(String, Option<String>)> {
			fn walk(directory: &Path, root: &Path, entries: &mut Vec<(String, Option<String>)>) {
				for entry in fs::read_dir(directory).unwrap() {
					let path = entry.unwrap().path();
					let name = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");

					if path.is_dir() {
						entries.push((name, None));
						walk(&path, root, entries);
					} else {
						entries.push((name, Some(fs::read_to_string(&path).unwrap())));
					}
				}
			}

			let mut entries = Vec::new();

			walk(&self.0, &self.0, &mut entries);
			entries.sort();
			entries
		}
	}

	impl Drop for TempDir {
		fn drop(&mut self) {
			let _ = fs::remove_dir_all(&self.0);
		}
	}

	fn change(path: &Path, original: &str, formatted: &str) -> FileChange {
		FileChange { path: path.to_path_buf(), original: original.to_owned(), formatted: formatted.to_owned() }
	}

	#[test]
	fn undoes_what_was_applied() {
		let dir = TempDir::new("undo");
		let lib = dir.write("src/lib.rs", "mod a;\nmod gone;\n");
		let a = dir.write("src/a.rs", "struct A;\n");

		dir.write("src/Case.rs", "struct Case;\n");
		dir.write("src/moved/x.rs", "struct X;\n");
		dir.write("src/gone.rs", "struct Gone;\n");
		dir.write("src/gone/inner.rs", "struct Inner;\n");

		let changes = [change(&lib, "mod a;\nmod gone;\n", "mod b;\n"), change(&a, "struct A;\n", "struct B;\n")];
		let changes: Vec<&FileChange> = changes.iter().collect();
		let moves = [
			(a.clone(), dir.path("src/b.rs")),
			// (only the case changes, on file systems that ignore case)
			(dir.path("src/Case.rs"), dir.path("src/case.rs")),
			// into directories that do not exist yet
			(dir.path("src/moved"), dir.path("src/deep/er/moved")),
		];
		let deletions = [dir.path("src/gone.rs"), dir.path("src/gone")];
		let deletions: Vec<&Path> = deletions.iter().map(PathBuf::as_path).collect();
		let before = dir.snapshot();

		// undone after writing, after each move, and after each deletion
		let prefixes = (0..=moves.len())
			.map(|moved| (moved, 0))
			.chain((1..=deletions.len()).map(|deleted| (moves.len(), deleted)));

		for (moved, deleted) in prefixes {
			let mut transaction = Transaction::default();

			transaction.perform(&changes, &moves[..moved], &deletions[..deleted]).unwrap();

			assert_ne!(dir.snapshot(), before);
			assert!(transaction.undo().is_empty());
			assert_eq!(dir.snapshot(), before, "after {moved} moves and {deleted} deletions");
		}

		// committed, the deleted paths are removed from their temporary names
		let mut transaction = Transaction::default();

		transaction.perform(&changes, &moves, &deletions).unwrap();

		assert!(transaction.commit().is_empty());

		let file = |name: &str, text: &str| (name.to_owned(), Some(text.to_owned()));
		let directory = |name: &str| (name.to_owned(), None);

		assert_eq!(
			dir.snapshot(),
			[
				directory("src"),
				file("src/b.rs", "struct B;\n"),
				file("src/case.rs", "struct Case;\n"),
				directory("src/deep"),
				directory("src/deep/er"),
				directory("src/deep/er/moved"),
				file("src/deep/er/moved/x.rs", "struct X;\n"),
				file("src/lib.rs", "mod b;\n"),
			]
		);
	}

	#[test]
	fn undoing_keeps_what_others_changed() {
		let dir = TempDir::new("undo-conflict");
		let a = dir.write("a.rs", "struct A;\n");
		let c = dir.write("c.rs", "struct C;\n");
		let changes = [change(&a, "struct A;\n", "struct B;\n"), change(&c, "struct C;\n", "struct D;\n")];
		let changes: Vec<&FileChange> = changes.iter().collect();
		let mut transaction = Transaction::default();

		transaction.perform(&changes, &[(a.clone(), dir.path("b.rs"))], &[]).unwrap();

		// someone else creates `a.rs` again, and changes `c.rs`: moving `b.rs` back, or writing `c.rs` back, would
		// overwrite what they wrote
		dir.write("a.rs", "struct Other;\n");
		dir.write("c.rs", "struct Changed;\n");

		let stays_moved =
			format!("`{}` stays moved to `{}` (something else is there now)", a.display(), dir.path("b.rs").display());
		let not_restored = |path: &Path| {
			let reason = "it changed after the edit wrote it";

			format!("`{}` was not restored to its original contents ({reason})", path.display())
		};

		assert_eq!(transaction.undo(), [stays_moved, not_restored(&c), not_restored(&a)]);
		assert_eq!(fs::read_to_string(&a).unwrap(), "struct Other;\n");
		assert_eq!(fs::read_to_string(dir.path("b.rs")).unwrap(), "struct B;\n");
		assert_eq!(fs::read_to_string(&c).unwrap(), "struct Changed;\n");
	}

	#[test]
	fn names_the_same_entry_only_in_another_case() {
		let dir = TempDir::new("case");
		let upper = dir.write("Name.rs", "");
		let lower = dir.path("name.rs");

		dir.write("sub/name.rs", "");

		assert!(!names_same_entry(&upper, &upper));
		assert!(!names_same_entry(&upper, &dir.path("sub/name.rs")));
		assert!(destination_exists(&upper, &dir.path("sub/name.rs")));

		// `name.rs` names `Name.rs` on file systems that ignore case (and nothing on others)
		assert!(!destination_exists(&upper, &lower));

		match fs::symlink_metadata(&lower).is_ok() {
			true => assert!(names_same_entry(&upper, &lower)),

			// where names in another case name other files
			false => {
				dir.write("name.rs", "");

				assert!(!names_same_entry(&upper, &lower));
				assert!(destination_exists(&upper, &lower));
			}
		}
	}
}
