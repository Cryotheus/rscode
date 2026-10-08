//! Rendering query results for people (human) and programs (JSON).
//!
//! Human output is rendered from display rows ([`MatchRow`], [`ViewRow`]) holding display-ready paths; JSON output
//! serializes rscode's own result types, with their file paths made display-ready the same way.

use crate::args::ShowField;
use rscode::ItemKind;
use rscode::Tristate;
use rscode::edit::EditSet;
use rscode::edit::FileChange;
use rscode::model::Diagnostic;
use rscode::query::FindMatch;
use rscode::query::ItemView;
use rscode::source::LineCol;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

/// A found item, ready for display.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct MatchRow {
	pub(crate) path: String,
	pub(crate) kind: ItemKind,
	pub(crate) krate: String,
	pub(crate) file: String,
	pub(crate) start: LineCol,
	pub(crate) end: LineCol,
	pub(crate) visibility: String,
	pub(crate) cfg: Option<String>,
	pub(crate) active: Tristate,
	pub(crate) usable_paths: Vec<String>,
	pub(crate) import_targets: Vec<String>,
	pub(crate) thread_local: bool,
	pub(crate) entry_macro: Option<String>,
}

impl MatchRow {
	pub(crate) fn new(found: &FindMatch, paths: &PathDisplay) -> Self {
		Self {
			path: found.path.clone(),
			kind: found.kind,
			krate: found.krate.clone(),
			file: paths.display(&found.file),
			start: found.start,
			end: found.end,
			visibility: found.visibility.clone(),
			cfg: found.cfg.clone(),
			active: found.active,
			usable_paths: found.usable_paths.clone(),
			import_targets: found.import_targets.clone(),
			thread_local: found.thread_local,
			entry_macro: found.entry_macro.clone(),
		}
	}
}

/// Displays file paths relative to the current directory when they are below it, absolute otherwise.
#[derive(Debug, Clone)]
pub(crate) struct PathDisplay {
	/// Relative paths (as in [`FindMatch::file`]) are relative to this directory: the workspace root.
	root: PathBuf,

	cwd: Option<PathBuf>,

	/// `--absolute-paths`
	absolute: bool,
}

impl PathDisplay {
	/// With the current directory as the process has it, like the paths of the loaded workspace (not canonicalized:
	/// on Windows that gives verbatim paths, `\\?\C:\...`, which no other path starts with).
	pub(crate) fn new(root: &Path, absolute: bool) -> Self {
		let cwd = std::env::current_dir().ok();

		Self::with_cwd(root, cwd.as_deref(), absolute)
	}

	pub(crate) fn with_cwd(root: &Path, cwd: Option<&Path>, absolute: bool) -> Self {
		Self {
			root: root.to_path_buf(),
			cwd: cwd.map(Path::to_path_buf),
			absolute,
		}
	}

	/// The path of a file in a diff: relative to the workspace root when below it (whatever the current directory, so
	/// that every file of a diff has the same base, as `patch -p1` and `git apply` need), absolute otherwise.
	pub(crate) fn diff_path(&self, path: &Path) -> PathBuf {
		let absolute = if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) };

		match absolute.strip_prefix(&self.root) {
			Ok(relative) if !self.absolute && !relative.as_os_str().is_empty() => relative.to_path_buf(),
			_ => absolute,
		}
	}

	pub(crate) fn display(&self, path: &Path) -> String {
		self.path(path).display().to_string()
	}

	pub(crate) fn path(&self, path: &Path) -> PathBuf {
		let absolute = if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) };

		if !self.absolute
			&& let Some(cwd) = &self.cwd
			&& let Ok(relative) = absolute.strip_prefix(cwd)
		{
			return match relative.as_os_str().is_empty() {
				true => PathBuf::from("."),
				false => relative.to_path_buf(),
			};
		}

		absolute
	}
}

/// A viewed item, ready for display.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct ViewRow {
	pub(crate) path: String,
	pub(crate) kind: ItemKind,
	pub(crate) file: String,
	pub(crate) start: LineCol,
	pub(crate) end: LineCol,
	pub(crate) cfg: Option<String>,
	pub(crate) active: Tristate,
	pub(crate) thread_local: bool,
	pub(crate) entry_macro: Option<String>,
	pub(crate) text: String,
	pub(crate) impls: Vec<ViewRow>,
}

impl ViewRow {
	pub(crate) fn new(view: &ItemView, paths: &PathDisplay) -> Self {
		Self {
			path: view.path.clone(),
			kind: view.kind,
			file: paths.display(&view.file),
			start: view.start,
			end: view.end,
			cfg: view.cfg.clone(),
			active: view.active,
			thread_local: view.thread_local,
			entry_macro: view.entry_macro.clone(),
			text: view.text.clone(),
			impls: view.impls.iter().map(|view| Self::new(view, paths)).collect(),
		}
	}
}

/// A load problem, as `file:line:column: message`.
pub(crate) fn diagnostic(diagnostic: &Diagnostic, paths: &PathDisplay) -> String {
	match (&diagnostic.file, diagnostic.location) {
		(Some(file), Some(location)) => format!("{}:{location}: {}", paths.display(file), diagnostic.message),
		(Some(file), None) => format!("{}: {}", paths.display(file), diagnostic.message),
		(None, _) => diagnostic.message.clone(),
	}
}

/// File changes with display paths (for rscode_fmt's emitters).
pub(crate) fn display_changes(changes: &[FileChange], paths: &PathDisplay) -> Vec<FileChange> {
	changes
		.iter()
		.map(|change| FileChange {
			path: paths.path(&change.path),
			original: change.original.clone(),
			formatted: change.formatted.clone(),
		})
		.collect()
}

/// A unified diff of an edit set's text edits, followed by its moves and deletions.
pub(crate) fn edit_diff(edits: &EditSet, paths: &PathDisplay) -> Result<String, rscode::Error> {
	let mut diff = unified_diff(&edits.preview()?, paths);

	diff.push_str(&file_operations(edits.moves(), edits.deletions(), paths));
	Ok(diff)
}

/// rustfmt's `--file-lines` JSON: one `{"file", "range": [first, last]}` entry per item, in order.
pub(crate) fn file_lines(rows: &[MatchRow]) -> serde_json::Result<String> {
	#[derive(Serialize)]
	struct FileLines<'a> {
		file: &'a str,
		range: [usize; 2],
	}

	let entries: Vec<FileLines<'_>> = rows
		.iter()
		.map(|row| FileLines {
			file: &row.file,
			range: [row.start.line, last_line(row.start, row.end)],
		})
		.collect();

	json_line(&entries)
}

/// `rename <from> -> <to>` and `delete <path>` lines.
pub(crate) fn file_operations(moves: &[(PathBuf, PathBuf)], deletions: &[PathBuf], paths: &PathDisplay) -> String {
	let mut out = String::new();

	for (from, to) in moves {
		out.push_str(&format!("rename {} -> {}\n", paths.display(from), paths.display(to)));
	}

	for path in deletions {
		out.push_str(&format!("delete {}\n", paths.display(path)));
	}

	out
}

/// One line per item: the path, then the `--show` fields, separated by two spaces.
pub(crate) fn find_human(rows: &[MatchRow], show: &[ShowField]) -> String {
	let mut out = String::new();

	for row in rows {
		let mut columns = vec![row.path.clone()];

		for field in show {
			match field {
				ShowField::Kind => columns.push(kind_label(row.kind, row.thread_local, row.entry_macro.as_deref())),
				ShowField::Location => columns.push(format!("{}:{}", row.file, row.start)),
				ShowField::Span => columns.push(format!("{}:{}-{}", row.file, row.start, row.end)),
				ShowField::Vis => columns.push(row.visibility.clone()),
				ShowField::Crate => columns.push(format!("crate: {}", row.krate)),
				ShowField::Cfg => columns.extend(row.cfg.as_ref().map(|cfg| format!("cfg: {cfg}"))),

				ShowField::Usable => match row.usable_paths.is_empty() {
					true => columns.push("usable: none".to_owned()),
					false => columns.push(format!("usable: {}", row.usable_paths.join(", "))),
				},
			}
		}

		if !row.import_targets.is_empty() {
			columns.push(format!("-> {}", row.import_targets.join(", ")));
		}

		match row.active {
			Tristate::False => columns.push("inactive".to_owned()),
			Tristate::Unknown if show.contains(&ShowField::Cfg) => columns.push("cfg-unknown".to_owned()),
			_ => {}
		}

		out.push_str(&columns.join("  "));
		out.push('\n');
	}

	out
}

/// [`FindMatch`]es as a JSON array, with display paths.
pub(crate) fn find_json(matches: &[FindMatch], paths: &PathDisplay) -> serde_json::Result<String> {
	let matches: Vec<FindMatch> = matches
		.iter()
		.cloned()
		.map(|mut found| {
			found.file = paths.path(&found.file);
			found
		})
		.collect();

	json_line(&matches)
}

/// The formatted contents of files, each preceded by `<path>:` and a blank line when there are several (like
/// `rustfmt --emit stdout`).
pub(crate) fn formatted_contents(changes: &[FileChange]) -> String {
	match changes {
		[single] => single.formatted.clone(),

		_ => changes
			.iter()
			.map(|change| format!("{}:\n\n{}", change.path.display(), change.formatted))
			.collect(),
	}
}

/// Compact JSON followed by a line break.
pub(crate) fn json_line(value: &impl Serialize) -> serde_json::Result<String> {
	serde_json::to_string(value).map(|json| json + "\n")
}

/// The name of a kind, marking statics declared by `thread_local!` and by entries of other macro invocations.
fn kind_label(kind: ItemKind, thread_local: bool, entry_macro: Option<&str>) -> String {
	match (thread_local, entry_macro) {
		(true, _) => format!("{} (thread_local!)", kind.name()),
		(false, Some(name)) => format!("{} ({name}!)", kind.name()),
		(false, None) => kind.name().to_owned(),
	}
}

/// The last line an item occupies, given its exclusive end.
pub(crate) fn last_line(start: LineCol, end: LineCol) -> usize {
	if end.column <= 1 && end.line > start.line {
		end.line - 1
	} else {
		end.line
	}
}

/// A unified diff of file changes, with the paths of [`PathDisplay::diff_path`].
pub(crate) fn unified_diff(changes: &[FileChange], paths: &PathDisplay) -> String {
	let changes: Vec<FileChange> = changes
		.iter()
		.map(|change| FileChange {
			path: paths.diff_path(&change.path),
			original: change.original.clone(),
			formatted: change.formatted.clone(),
		})
		.collect();

	rscode::rscode_fmt::emit::unified_diff(&changes, 3)
}

/// For each item (and then each of its `impl` blocks) a `// path (kind) file:line-line` header line followed by its
/// text, with a blank line between items.
pub(crate) fn view_human(rows: &[ViewRow]) -> String {
	fn blocks(row: &ViewRow, out: &mut Vec<String>) {
		let kind = kind_label(row.kind, row.thread_local, row.entry_macro.as_deref());
		let mut block = format!(
			"// {} ({kind}) {}:{}-{}",
			row.path,
			row.file,
			row.start.line,
			last_line(row.start, row.end)
		);

		if let Some(cfg) = &row.cfg {
			block.push_str(&format!(" [cfg: {cfg}]"));
		}

		if row.active == Tristate::False {
			block.push_str(" [inactive]");
		}

		block.push('\n');
		block.push_str(&row.text);

		if !block.ends_with('\n') {
			block.push('\n');
		}

		out.push(block);

		for view in &row.impls {
			blocks(view, out);
		}
	}

	let mut out = Vec::new();

	for row in rows {
		blocks(row, &mut out);
	}

	out.join("\n")
}

/// [`ItemView`]s as a JSON array, with display paths.
pub(crate) fn view_json(views: &[ItemView], paths: &PathDisplay) -> serde_json::Result<String> {
	fn display(view: &ItemView, paths: &PathDisplay) -> ItemView {
		let mut view = view.clone();

		view.file = paths.path(&view.file);
		view.impls = view.impls.iter().map(|view| display(view, paths)).collect();
		view
	}

	let views: Vec<ItemView> = views.iter().map(|view| display(view, paths)).collect();

	json_line(&views)
}

#[cfg(test)]
mod tests {
	use super::*;
	use rscode::model::Severity;

	fn at(line: usize, column: usize) -> LineCol {
		LineCol { line, column }
	}

	#[test]
	fn diffs_have_one_base() {
		let below = PathDisplay::with_cwd(Path::new("/ws"), Some(Path::new("/ws/crates/b")), false);
		let change = |path: &str| FileChange {
			path: PathBuf::from(path),
			original: "fn a() {}\n".to_owned(),
			formatted: "fn b() {}\n".to_owned(),
		};
		let diff = unified_diff(&[change("/ws/crates/b/src/lib.rs"), change("/ws/src/lib.rs"), change("/x/y.rs")], &below);
		let headers: Vec<&str> = diff.lines().filter(|line| line.starts_with("---") || line.starts_with("+++")).collect();

		assert_eq!(
			headers,
			[
				"--- a/crates/b/src/lib.rs",
				"+++ b/crates/b/src/lib.rs",
				"--- a/src/lib.rs",
				"+++ b/src/lib.rs",
				"--- /x/y.rs",
				"+++ /x/y.rs"
			]
		);

		let absolute = PathDisplay::with_cwd(Path::new("/ws"), Some(Path::new("/ws")), true);

		assert_eq!(absolute.diff_path(Path::new("src/lib.rs")), Path::new("/ws/src/lib.rs"));
	}

	#[test]
	fn displays_paths_relative_to_the_current_directory() {
		let below = PathDisplay::with_cwd(Path::new("/ws"), Some(Path::new("/ws/crates/a")), false);

		assert_eq!(below.display(Path::new("crates/a/src/lib.rs")), "src/lib.rs");
		assert_eq!(below.display(Path::new("/ws/crates/a/src/lib.rs")), "src/lib.rs");
		// (compared as paths where the root is joined, which on Windows is with a `\`)
		assert_eq!(below.path(Path::new("crates/b/src/lib.rs")), Path::new("/ws/crates/b/src/lib.rs"));
		assert_eq!(below.display(Path::new("/elsewhere/x.rs")), "/elsewhere/x.rs");
		assert_eq!(below.display(Path::new("/ws/crates/a")), ".");

		let at_root = PathDisplay::with_cwd(Path::new("/ws"), Some(Path::new("/ws")), false);

		assert_eq!(at_root.display(Path::new("crates/a/src/lib.rs")), "crates/a/src/lib.rs");

		// a sibling directory sharing a name prefix is not "below"
		let prefix = PathDisplay::with_cwd(Path::new("/ws"), Some(Path::new("/ws/crates/a")), false);

		assert_eq!(prefix.display(Path::new("/ws/crates/ab/x.rs")), "/ws/crates/ab/x.rs");

		let absolute = PathDisplay::with_cwd(Path::new("/ws"), Some(Path::new("/ws")), true);

		assert_eq!(absolute.path(Path::new("src/lib.rs")), Path::new("/ws/src/lib.rs"));

		let unknown_cwd = PathDisplay::with_cwd(Path::new("/ws"), None, false);

		assert_eq!(unknown_cwd.path(Path::new("src/lib.rs")), Path::new("/ws/src/lib.rs"));
	}

	#[test]
	fn last_lines() {
		assert_eq!(last_line(at(3, 1), at(5, 2)), 5);
		assert_eq!(last_line(at(3, 5), at(3, 9)), 3);
		// an end at the start of a line is exclusive of that line
		assert_eq!(last_line(at(3, 1), at(6, 1)), 5);
		assert_eq!(last_line(at(3, 1), at(3, 1)), 3);
	}

	fn paths() -> PathDisplay {
		PathDisplay::with_cwd(Path::new("/ws"), Some(Path::new("/ws")), false)
	}

	#[test]
	fn renders_diagnostics() {
		let mut problem = Diagnostic {
			severity: Severity::Error,
			message: "expected `;`".to_owned(),
			file: Some(PathBuf::from("/ws/src/lib.rs")),
			location: Some(at(3, 7)),
		};

		assert_eq!(diagnostic(&problem, &paths()), "src/lib.rs:3:7: expected `;`");

		problem.location = None;

		assert_eq!(diagnostic(&problem, &paths()), "src/lib.rs: expected `;`");

		problem.file = None;

		assert_eq!(diagnostic(&problem, &paths()), "expected `;`");
	}

	#[test]
	fn renders_file_lines() {
		let rows = [
			row("demo::add", ItemKind::Fn, "src/lib.rs", at(9, 1), at(11, 2)),
			row("demo::shapes::Circle", ItemKind::Struct, "src/shapes.rs", at(4, 1), at(8, 1)),
			row("demo::X", ItemKind::Const, "src/lib.rs", at(2, 1), at(2, 20)),
		];

		assert_eq!(
			file_lines(&rows).unwrap(),
			"[{\"file\":\"src/lib.rs\",\"range\":[9,11]},{\"file\":\"src/shapes.rs\",\"range\":[4,7]},\
			 {\"file\":\"src/lib.rs\",\"range\":[2,2]}]\n"
		);
		assert_eq!(file_lines(&[]).unwrap(), "[]\n");
	}

	#[test]
	fn renders_file_operations() {
		let moves = [(PathBuf::from("/ws/src/old.rs"), PathBuf::from("/ws/src/new.rs"))];
		let deletions = [PathBuf::from("/ws/src/gone"), PathBuf::from("/tmp/x.rs")];

		assert_eq!(
			file_operations(&moves, &deletions, &paths()),
			"rename src/old.rs -> src/new.rs\ndelete src/gone\ndelete /tmp/x.rs\n"
		);
		assert_eq!(file_operations(&[], &[], &paths()), "");
	}

	#[test]
	fn renders_find_matches() {
		let mut circle = row("demo::shapes::Circle", ItemKind::Struct, "src/shapes.rs", at(4, 1), at(7, 2));
		let mut extra = row("demo::extra", ItemKind::Fn, "src/lib.rs", at(20, 1), at(23, 2));

		extra.cfg = Some("feature = \"extra\"".to_owned());
		extra.active = Tristate::False;

		let default = [ShowField::Kind, ShowField::Location];

		assert_eq!(
			find_human(&[circle.clone(), extra.clone()], &default),
			"demo::shapes::Circle  struct  src/shapes.rs:4:1\ndemo::extra  fn  src/lib.rs:20:1  inactive\n"
		);

		circle.usable_paths = vec!["crate::Circle".to_owned(), "crate::shapes::Circle".to_owned()];
		circle.cfg = Some("unix".to_owned());
		circle.active = Tristate::Unknown;

		let all = [
			ShowField::Kind,
			ShowField::Span,
			ShowField::Vis,
			ShowField::Crate,
			ShowField::Cfg,
			ShowField::Usable,
		];

		assert_eq!(
			find_human(&[circle], &all),
			"demo::shapes::Circle  struct  src/shapes.rs:4:1-7:2  pub  crate: demo  cfg: unix  \
			 usable: crate::Circle, crate::shapes::Circle  cfg-unknown\n"
		);
		assert_eq!(find_human(&[extra.clone()], &[]), "demo::extra  inactive\n");
		assert_eq!(find_human(&[extra], &[ShowField::Usable]), "demo::extra  usable: none  inactive\n");
		assert_eq!(find_human(&[], &default), "");
	}

	#[test]
	fn renders_formatted_contents() {
		let change = |path: &str, formatted: &str| FileChange {
			path: PathBuf::from(path),
			original: String::new(),
			formatted: formatted.to_owned(),
		};

		assert_eq!(formatted_contents(&[change("src/lib.rs", "fn a() {}\n")]), "fn a() {}\n");
		assert_eq!(
			formatted_contents(&[change("src/lib.rs", "mod a;\n"), change("src/a.rs", "fn b() {}\n")]),
			"src/lib.rs:\n\nmod a;\nsrc/a.rs:\n\nfn b() {}\n"
		);
		assert_eq!(formatted_contents(&[]), "");

		let changes = display_changes(&[change("/ws/src/a.rs", "x")], &paths());

		assert_eq!(changes[0].path, Path::new("src/a.rs"));
		assert_eq!(changes[0].formatted, "x");
	}

	#[test]
	fn renders_imports() {
		let mut import = row("demo::Circle", ItemKind::Import, "src/lib.rs", at(4, 9), at(4, 23));

		import.import_targets = vec!["demo::shapes::Circle".to_owned()];

		assert_eq!(
			find_human(&[import], &[ShowField::Kind]),
			"demo::Circle  import  -> demo::shapes::Circle\n"
		);
	}

	#[test]
	fn renders_views() {
		let method = ViewRow {
			path: "impl demo::shapes::Circle".to_owned(),
			kind: ItemKind::Impl,
			file: "src/shapes.rs".to_owned(),
			start: at(9, 1),
			end: at(13, 2),
			cfg: None,
			active: Tristate::True,
			thread_local: false,
			text: "impl Circle {\n\tpub fn new(radius: f64) -> Self { ... }\n}".to_owned(),
			impls: Vec::new(),
			entry_macro: None,
		};
		let circle = ViewRow {
			path: "demo::shapes::Circle".to_owned(),
			kind: ItemKind::Struct,
			file: "src/shapes.rs".to_owned(),
			start: at(4, 1),
			end: at(7, 2),
			cfg: None,
			active: Tristate::True,
			thread_local: false,
			text: "pub struct Circle {\n\tpub radius: f64,\n}\n".to_owned(),
			impls: vec![method],
			entry_macro: None,
		};
		let extra = ViewRow {
			path: "demo::extra".to_owned(),
			kind: ItemKind::Fn,
			file: "src/lib.rs".to_owned(),
			start: at(20, 1),
			end: at(23, 1),
			cfg: Some("feature = \"extra\"".to_owned()),
			active: Tristate::False,
			thread_local: false,
			text: "fn extra() {}".to_owned(),
			impls: Vec::new(),
			entry_macro: None,
		};

		assert_eq!(
			view_human(&[circle, extra]),
			"// demo::shapes::Circle (struct) src/shapes.rs:4-7\n\
			 pub struct Circle {\n\tpub radius: f64,\n}\n\
			 \n\
			 // impl demo::shapes::Circle (impl) src/shapes.rs:9-13\n\
			 impl Circle {\n\tpub fn new(radius: f64) -> Self { ... }\n}\n\
			 \n\
			 // demo::extra (fn) src/lib.rs:20-22 [cfg: feature = \"extra\"] [inactive]\n\
			 fn extra() {}\n"
		);
		assert_eq!(view_human(&[]), "");
	}

	fn row(path: &str, kind: ItemKind, file: &str, start: LineCol, end: LineCol) -> MatchRow {
		MatchRow {
			path: path.to_owned(),
			kind,
			krate: "demo".to_owned(),
			file: file.to_owned(),
			start,
			end,
			visibility: "pub".to_owned(),
			cfg: None,
			active: Tristate::True,
			usable_paths: Vec::new(),
			import_targets: Vec::new(),
			thread_local: false,
			entry_macro: None,
		}
	}
}
