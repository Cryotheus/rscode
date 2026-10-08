//! Formatting and sorting items by path.
//!
//! Targets become [`FormatTarget`]s by file: a module's own file (for out-of-line modules and crate roots) or the
//! items themselves, and then [`Formatter::format_items`] formats every file once, leaving the text outside of the
//! targets untouched. Files are formatted in parallel, on short-lived threads.

use crate::Error;
use crate::edit::EditSet;
use crate::edit::FileChange;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::path::ItemPath;
use crate::pattern::PathPattern;
use crate::pattern::SegmentPattern;
use crate::resolve::Resolver;
use crate::source::LineCol;
use crate::source::SourceFile;
use rscode_fmt::Edition;
use rscode_fmt::FormatError;
use rscode_fmt::FormatOptions;
use rscode_fmt::FormatTarget;
use rscode_fmt::Formatter;
use rscode_fmt::rscode_sort::SortError;
use serde::Serialize;
use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// At most this many files are formatted at once (each runs a rustfmt process).
const MAX_THREADS: usize = 8;

#[derive(Debug)]
struct FileTargets<'ws> {
	file: &'ws SourceFile,

	/// The edition of the (first) crate the file is in.
	edition: Edition,

	targets: Vec<FormatTarget>,
}

/// Formatting targets, by file.
#[derive(Debug, Default)]
struct Files<'ws> {
	files: BTreeMap<&'ws Path, FileTargets<'ws>>,
}

impl<'ws> Files<'ws> {
	fn add(&mut self, file: &'ws SourceFile, edition: Edition, target: FormatTarget) {
		let targets = &mut self
			.files
			.entry(file.path())
			.or_insert_with(|| FileTargets {
				file,
				edition,
				targets: Vec::new(),
			})
			.targets;

		if !targets.contains(&target) {
			targets.push(target);
		}
	}

	fn add_item(&mut self, resolver: &Resolver<'ws>, item: ItemId, options: &FmtOptions, warnings: &mut Vec<String>) {
		let ws = resolver.workspace();
		let data = ws.item(item);
		let edition = ws.krate(item.krate()).edition();

		match data.kind {
			ItemKind::Module => self.add_module(resolver, item, options, warnings),

			// variants are formatted with their enum, statics declared by `thread_local!` with the invocation, and imports
			// with their `use` item
			ItemKind::Variant | ItemKind::Static | ItemKind::Import
				if ws
					.parent(item)
					.is_some_and(|parent| matches!(ws.item(parent).kind, ItemKind::Enum | ItemKind::MacroCall | ItemKind::Use)) =>
			{
				let parent = ws.parent(item).expect("checked above");

				self.add(ws.file_of(parent), edition, FormatTarget::Item(ws.item(parent).range.start));
			}

			// fields with their struct or union, or the enum of their variant
			ItemKind::Field => {
				let owner = (ws.ancestors(item))
					.find(|&ancestor| ws.item(ancestor).kind != ItemKind::Variant)
					.unwrap_or(item);

				self.add(ws.file_of(owner), edition, FormatTarget::Item(ws.item(owner).range.start));
			}

			_ => self.add(ws.file_of(item), edition, FormatTarget::Item(data.range.start)),
		}
	}

	/// A module's file (or the inline module itself), and unless skipping children, the files of the out-of-line modules
	/// inside of it.
	fn add_module(&mut self, resolver: &Resolver<'ws>, module: ItemId, options: &FmtOptions, warnings: &mut Vec<String>) {
		let ws = resolver.workspace();
		let mut stack = vec![(module, true)];

		while let Some((module, is_target)) = stack.pop() {
			let data = ws.item(module);
			let krate = ws.krate(module.krate());

			if !is_target && options.active_only && !ws.is_active(module).is_possible() {
				continue;
			}

			let Some(info) = data.module_info() else {
				continue;
			};

			match (info.inline, info.file, &info.load_error) {
				(true, _, _) if is_target => self.add(ws.file_of(module), krate.edition(), FormatTarget::Item(data.range.start)),
				(true, _, _) => {}
				(false, Some(file), None) => self.add(krate.file(file), krate.edition(), FormatTarget::File),

				(false, _, error) => {
					let why = error.as_deref().unwrap_or("its file is not loaded");

					warnings.push(format!("the module `{}` is not formatted: {why}", resolver.canonical_path(module)));
				}
			}

			if !options.skip_children {
				let children = ws.children(module).filter(|&child| ws.item(child).kind == ItemKind::Module);

				stack.extend(children.map(|child| (child, false)));
			}
		}
	}
}

/// Options for [`format()`].
#[derive(Debug, Default, Clone)]
pub struct FmtOptions {
	/// Formatter, rustfmt, and sorting options. The edition passed to rustfmt defaults to each crate's edition,
	/// and rustfmt's config is searched from each file's directory, unless set explicitly.
	///
	/// `use` items are sorted for the style edition rustfmt formats with
	/// ([`RustFmtOptions::style_edition_in_effect`](rscode_fmt::RustFmtOptions::style_edition_in_effect)), whatever
	/// the sorting options say, so that formatting after sorting (and sorting after formatting) changes nothing.
	pub format: FormatOptions,

	/// Only process the targeted items themselves: child modules are neither sorted nor (when out-of-line)
	/// formatted.
	pub skip_children: bool,

	/// Only process `cfg` variants that are not definitely disabled.
	pub active_only: bool,
}

/// The planned formatting.
#[derive(Debug, Clone, Serialize)]
pub struct Formatting {
	/// The edits, to preview or apply.
	#[serde(skip)]
	pub edits: EditSet,

	/// Every processed file, changed or not.
	///
	/// Sorted by path. Paths are those the files were loaded from (absolute for workspaces loaded with cargo); callers
	/// display them relative to [`Workspace::root`](crate::Workspace::root) as they see fit.
	pub changes: Vec<FileChange>,

	/// Things to know about the formatting, such as targets that match nothing.
	pub warnings: Vec<String>,
}

/// The path a target without wildcards stands for (qualified targets are matched against canonical paths instead).
fn exact_path(pattern: &PathPattern) -> Option<ItemPath> {
	if pattern.is_qualified() {
		return None;
	}

	pattern.to_item_path()
}

/// The error of formatting a file, with its path.
fn file_error(path: &Path, error: FormatError) -> Error {
	match error {
		FormatError::Parse { message, line, column } | FormatError::Sort(SortError::Parse { message, line, column }) => Error::Parse {
			path: path.to_path_buf(),
			location: LineCol { line, column },
			message,
		},

		error => Error::io(path, std::io::Error::other(error)),
	}
}

/// Plans formatting (and sorting, if enabled) the items matching `targets`.
///
/// A target naming a module (`crate` for the crate root) formats the module's file(s) — every child module too
/// unless [`FmtOptions::skip_children`] — while other items are formatted in place.
/// Targets without wildcards are resolved like paths (following re-exports).
///
/// Other targets (with wildcards, or qualified like `<Type as Trait>::name`) are matched against the canonical paths
/// of the items of the selected crates, as by [`Find`](crate::Find): qualified targets match `impl` blocks and their
/// items. An enum variant stands for its enum. Fails with [`Error::NotFound`] when a target without wildcards names
/// nothing, and with the error of the formatter for the first file (by path) that cannot be formatted; targets with
/// wildcards that match nothing only give a warning.
pub fn format(resolver: &Resolver<'_>, targets: &[PathPattern], options: &FmtOptions) -> Result<Formatting, Error> {
	let ws = resolver.workspace();
	let mut plan = Formatting {
		edits: EditSet::new(),
		changes: Vec::new(),
		warnings: Vec::new(),
	};
	let mut files = Files::default();

	for pattern in targets {
		let path = exact_path(pattern);

		let mut items = match &path {
			Some(path) => resolver.resolve_item_path(path),
			None => matching(resolver, pattern),
		};

		if items.is_empty()
			&& let Some(path) = path
		{
			return Err(Error::NotFound(path.to_string()));
		}

		if options.active_only {
			items.retain(|&item| ws.is_active(item).is_possible());
		}

		if items.is_empty() {
			plan.warnings.push(format!("`{pattern}` matches no item"));
		}

		for item in items {
			files.add_item(resolver, item, options, &mut plan.warnings);
		}
	}

	let files: Vec<&FileTargets<'_>> = files.files.values().collect();
	let results = format_files(&files, options);

	for (file, result) in files.into_iter().zip(results) {
		let formatted = result.map_err(|error| file_error(file.file.path(), error))?;

		if formatted != file.file.text() {
			plan.edits.replace_file(file.file, formatted.clone());
		}

		plan.changes.push(FileChange {
			path: file.file.path().to_path_buf(),
			original: file.file.text().to_owned(),
			formatted,
		});
	}

	Ok(plan)
}

fn format_file(file: &FileTargets<'_>, options: &FmtOptions) -> Result<String, FormatError> {
	let mut format = options.format.clone();

	format.rustfmt.edition.get_or_insert(file.edition);

	if format.rustfmt.config_path.is_none() {
		format.rustfmt.config_path = file.file.path().parent().map(Path::to_path_buf);
	}

	if let Some(sort) = &mut format.sort {
		// `use` items are sorted like rustfmt sorts them, so that neither undoes what the other did
		sort.style_edition = format.rustfmt.style_edition_in_effect().into();

		if options.skip_children {
			sort.recursive = false;
		}
	}

	Formatter::new(format).format_items(file.file.text(), &file.targets)
}

/// Formats every file (in order), on up to [`MAX_THREADS`] threads of their own (see [Threads](crate#threads)).
fn format_files(files: &[&FileTargets<'_>], options: &FmtOptions) -> Vec<Result<String, FormatError>> {
	let threads = std::thread::available_parallelism()
		.map_or(1, NonZeroUsize::get)
		.min(MAX_THREADS)
		.min(files.len());
	let next = AtomicUsize::new(0);

	let work = || {
		let mut done = Vec::new();

		loop {
			let index = next.fetch_add(1, Ordering::Relaxed);

			let Some(file) = files.get(index) else {
				break done;
			};

			done.push((index, format_file(file, options)));
		}
	};

	let mut results: Vec<(usize, Result<String, FormatError>)> = std::thread::scope(|scope| {
		// formatting recurses as deeply as the code is nested, which overflows the default stack of spawned threads
		let workers: Vec<_> = (0..threads)
			.filter_map(|_| {
				let thread = std::thread::Builder::new().stack_size(rscode_fmt::RECOMMENDED_STACK_SIZE);

				thread.spawn_scoped(scope, work).ok()
			})
			.collect();

		// without threads (when none could be started), everything is done here
		let mut results = if workers.is_empty() { work() } else { Vec::new() };

		for worker in workers {
			match worker.join() {
				Ok(done) => results.extend(done),
				Err(panic) => std::panic::resume_unwind(panic),
			}
		}

		results
	});

	results.sort_by_key(|&(index, _)| index);
	results.into_iter().map(|(_, result)| result).collect()
}

/// Items of the selected crates whose canonical paths match `pattern`.
fn matching(resolver: &Resolver<'_>, pattern: &PathPattern) -> Vec<ItemId> {
	let ws = resolver.workspace();
	let qualified = pattern.is_qualified();

	// most items fail on their name, which is cheaper to check than the canonical path
	let name_pattern = match pattern.segments.last() {
		Some(SegmentPattern::Ident(name)) if !qualified => Some(name),
		_ => None,
	};

	let mut items = Vec::new();

	for krate in ws.selected_crates() {
		for (item, data) in krate.items() {
			let candidate = match qualified {
				true => data.kind == ItemKind::Impl || ws.parent(item).is_some_and(|parent| ws.item(parent).kind == ItemKind::Impl),

				// `use` patterns match imports, other patterns never do
				false if pattern.is_import() => data.kind == ItemKind::Import,

				false => data.kind.is_nameable() && data.kind != ItemKind::Import && data.name.is_some(),
			};

			if !candidate {
				continue;
			}

			if let (Some(name_pattern), Some(name)) = (name_pattern, &data.name)
				&& !name_pattern.matches(name)
			{
				continue;
			}

			if pattern.matches(&resolver.canonical_path(item), true) {
				items.push(item);
			}
		}
	}

	items
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::pattern::MatchOptions;

	fn pattern(text: &str) -> PathPattern {
		PathPattern::parse(text, MatchOptions::default()).unwrap()
	}

	#[test]
	fn reports_errors_with_paths() {
		let path = Path::new("/x/src/lib.rs");
		let parse = FormatError::Parse {
			message: "expected `;`".to_owned(),
			line: 3,
			column: 7,
		};

		match file_error(path, parse) {
			Error::Parse {
				path: error_path,
				location,
				message,
			} => {
				assert_eq!(error_path, path);
				assert_eq!(location, LineCol { line: 3, column: 7 });
				assert_eq!(message, "expected `;`");
			}

			other => panic!("{other:?}"),
		}

		let sort = FormatError::Sort(SortError::Parse {
			message: "m".to_owned(),
			line: 1,
			column: 2,
		});

		assert!(matches!(
			file_error(path, sort),
			Error::Parse {
				location: LineCol { line: 1, column: 2 },
				..
			}
		));

		let failed = file_error(path, FormatError::RustFmt { stderr: "boom".to_owned() });

		assert_eq!(failed.to_string(), "/x/src/lib.rs: rustfmt failed: boom");
	}

	#[test]
	fn resolves_exact_targets() {
		assert_eq!(exact_path(&pattern("crate")), Some(ItemPath::parse("crate").unwrap()));
		assert_eq!(exact_path(&pattern("crate::a::b")), Some(ItemPath::parse("crate::a::b").unwrap()));
		assert_eq!(exact_path(&pattern("::dep::x")), Some(ItemPath::parse("::dep::x").unwrap()));
		assert_eq!(exact_path(&pattern("a::b")), Some(ItemPath::parse("a::b").unwrap()));
		assert_eq!(exact_path(&pattern("crate::a*")), None);
		assert_eq!(exact_path(&pattern("crate::**")), None);
		assert_eq!(exact_path(&pattern("<Foo as Bar>::baz")), None);
		assert_eq!(exact_path(&PathPattern::parse("Foo", MatchOptions { ignore_case: true }).unwrap()), None);
	}
}
