//! Rendering results as compact plain text for models: one line per item, file paths relative to the workspace
//! root, and locations as `file:line:column`.

use crate::Tristate;
use crate::edit::FileChange;
use crate::edit::Insertion;
use crate::edit::Removal;
use crate::edit::Rename;
use crate::edit::Replacement;
use crate::model::Crate;
use crate::model::ItemKind;
use crate::model::Package;
use crate::model::Severity;
use crate::model::Workspace;
use crate::query::FindMatch;
use crate::query::ItemView;
use crate::resolve::Reference;
use crate::resolve::ReferenceKind;
use crate::source::LineCol;
use crate::workspace::LoadOptions;
use smol_str::SmolStr;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;

/// Tool output longer than this many characters is truncated.
pub(crate) const MAX_OUTPUT_CHARS: usize = 100_000;

/// A path for display: relative to the workspace root when it is inside of it.
pub(crate) fn relative(root: &Path, path: &Path) -> PathBuf {
	match path.strip_prefix(root) {
		Ok(relative) if !relative.as_os_str().is_empty() => relative.to_path_buf(),
		_ => path.to_path_buf(),
	}
}

fn display(root: &Path, path: &Path) -> String {
	relative(root, path).display().to_string()
}

/// `file:line:column`
fn location(root: &Path, file: &Path, at: LineCol) -> String {
	format!("{}:{at}", display(root, file))
}

/// The last line of a range, given its exclusive end.
fn last_line(start: LineCol, end: LineCol) -> usize {
	match end.column <= 1 && end.line > start.line {
		true => end.line - 1,
		false => end.line,
	}
}

/// `1 item`, `2 items`
fn count(count: usize, singular: &str, plural: &str) -> String {
	match count {
		1 => format!("1 {singular}"),
		_ => format!("{count} {plural}"),
	}
}

/// The past tense of an edit (`removed`), or its future for dry runs (`would remove`).
fn done(dry_run: bool, past: &str, verb: &str) -> String {
	match dry_run {
		true => format!("would {verb}"),
		false => past.to_owned(),
	}
}

/// Which of a search's matches are shown.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct Page {
	pub(crate) offset: usize,
	pub(crate) limit: usize,

	/// Number of matches of the search.
	pub(crate) total: usize,
}

/// One line per match, then a line counting the matches (and how to get more).
///
/// `usable`: whether usable paths were requested (so that an item without any is marked).
pub(crate) fn find(root: &Path, pattern: &str, matches: &[FindMatch], page: Page, usable: bool) -> String {
	let mut out = String::new();

	for found in matches {
		out.push_str(&find_line(root, found, usable));
		out.push('\n');
	}

	out.push_str(&find_summary(pattern, matches.len(), page));
	out.push('\n');
	out
}

/// `path  kind  file:line:col-endline:endcol  cfg: …  inactive  -> imported  usable: …`
fn find_line(root: &Path, found: &FindMatch, usable: bool) -> String {
	let mut columns = vec![
		found.path.clone(),
		kind_label(found.kind, found.thread_local),
		format!("{}:{}-{}", display(root, &found.file), found.start, found.end),
	];

	if let Some(cfg) = &found.cfg {
		columns.push(format!("cfg: {cfg}"));
	}

	match found.active {
		Tristate::True => {}
		Tristate::False => columns.push("inactive".to_owned()),
		Tristate::Unknown => columns.push("cfg-unknown".to_owned()),
	}

	if !found.import_targets.is_empty() {
		columns.push(format!("-> {}", found.import_targets.join(", ")));
	}

	match (usable, found.usable_paths.is_empty()) {
		(false, _) => {}
		(true, true) => columns.push("usable: none (not visible)".to_owned()),
		(true, false) => columns.push(format!("usable: {}", found.usable_paths.join(", "))),
	}

	columns.join("  ")
}

fn find_summary(pattern: &str, shown: usize, page: Page) -> String {
	let Page {
		offset,
		limit,
		total,
	} = page;

	if total == 0 {
		let mut summary = format!("no items match `{pattern}`");

		if !pattern.contains('*') {
			let name = pattern.trim();
			let name = name.strip_prefix("use").filter(|rest| rest.starts_with(char::is_whitespace)).unwrap_or(name);
			let name = name.rsplit("::").next().unwrap_or(name).trim();

			write!(summary, " (without `*`, names must match exactly: try `*{name}*`, or `ignore_case`)").unwrap();
		}

		return summary;
	}

	let matches = count(total, "match", "matches");

	if shown == total {
		return matches;
	}

	if limit == 0 {
		return format!("{matches} (none listed: `limit` is 0)");
	}

	if shown == 0 {
		return format!("{matches}; none left after `offset` {offset}");
	}

	let mut summary = format!("{matches}; showing {}-{}", offset + 1, offset + shown);

	if offset + shown < total {
		write!(summary, "; for more, call again with `offset` {}", offset + shown).unwrap();
	}

	summary
}

/// Every item (and then each of its `impl` blocks) as a header line `// path (kind) file:line-endline` followed by
/// its text, with a blank line between items.
pub(crate) fn views(root: &Path, views: &[ItemView]) -> String {
	let mut blocks = Vec::new();

	for view in views {
		view_blocks(root, view, &mut blocks);
	}

	blocks.join("\n")
}

fn view_blocks(root: &Path, view: &ItemView, blocks: &mut Vec<String>) {
	let mut block = view_header(root, view);

	block.push('\n');
	block.push_str(&view.text);

	if !block.ends_with('\n') {
		block.push('\n');
	}

	blocks.push(block);

	for view in &view.impls {
		view_blocks(root, view, blocks);
	}
}

/// The name of a kind, marking statics declared by `thread_local!`.
fn kind_label(kind: ItemKind, thread_local: bool) -> String {
	match thread_local {
		true => format!("{} (thread_local!)", kind.name()),
		false => kind.name().to_owned(),
	}
}

fn view_header(root: &Path, view: &ItemView) -> String {
	let last = last_line(view.start, view.end);
	let kind = kind_label(view.kind, view.thread_local);
	let mut header = format!("// {} ({kind}) {}:{}", view.path, display(root, &view.file), view.start.line);

	if last > view.start.line {
		write!(header, "-{last}").unwrap();
	}

	if let Some(cfg) = &view.cfg {
		write!(header, " [cfg: {cfg}]").unwrap();
	}

	match view.active {
		Tristate::True => {}
		Tristate::False => header.push_str(" [inactive]"),
		Tristate::Unknown => header.push_str(" [cfg-unknown]"),
	}

	header
}

/// Packages (with features), crates, and load problems.
pub(crate) fn workspace_info(workspace: &Workspace, load: &LoadOptions) -> String {
	let root = workspace.root();
	let mut out = format!("workspace root: {}\n", root.display());

	writeln!(out, "selection: {}", selection(load)).unwrap();
	out.push_str("\npackages:\n");

	for package in workspace.packages() {
		let member = if package.is_member { "" } else { "  (not a workspace member)" };

		writeln!(out, "  {} {}  {}{member}", package.name, package.version, display(root, &package.manifest_path))
			.unwrap();

		if let Some(features) = features(package) {
			writeln!(out, "    features (* = enabled): {features}").unwrap();
		}
	}

	for member in workspace.unloaded_members() {
		writeln!(
			out,
			"  {} {}  {}  (not selected, so not loaded; crates: {})",
			member.name,
			member.version,
			display(root, &member.manifest_path),
			member.crate_names.join(", ")
		)
		.unwrap();
	}

	out.push_str("\ncrates:\n");

	for krate in workspace.crates() {
		let package = match krate.package() {
			Some(package) => format!("package {}  ", workspace.package(package).name),
			None => String::new(),
		};
		let selected = if krate.is_selected() { "selected" } else { "not selected" };

		writeln!(
			out,
			"  {}  {}  {}  edition {}  {package}{selected}  ({}, {})",
			krate.name(),
			krate.kind(),
			display(root, krate.root_file().path()),
			krate.edition(),
			count(krate.files().len(), "file", "files"),
			count(krate.items().count(), "item", "items"),
		)
		.unwrap();
	}

	if !workspace.unloaded_members().is_empty() {
		out.push_str(
			"\nPackages that are not selected are not searched: add them to `packages`, or set `workspace` to true.\n",
		);
	}

	let problems = diagnostics(workspace);

	match problems.is_empty() {
		true => out.push_str("\nload problems: none\n"),

		false => {
			out.push_str("\nload problems:\n");

			for problem in problems {
				writeln!(out, "  {problem}").unwrap();
			}
		}
	}

	out
}

/// The server's package and feature selection with a call's overrides, as far as it differs from cargo's defaults.
fn selection(load: &LoadOptions) -> String {
	let mut parts = Vec::new();

	match (load.workspace, load.packages.is_empty()) {
		(true, _) => parts.push("every workspace member".to_owned()),
		(false, false) => parts.push(format!("packages {}", load.packages.join(", "))),
		(false, true) => parts.push("cargo's default packages".to_owned()),
	}

	if load.workspace && !load.exclude.is_empty() {
		parts.push(format!("excluding {}", load.exclude.join(", ")));
	}

	if load.all_features {
		parts.push("all features".to_owned());
	} else if !load.features.is_empty() {
		parts.push(format!("features {}", load.features.join(", ")));
	}

	if load.no_default_features {
		parts.push("no default features".to_owned());
	}

	if load.targets.all_targets {
		parts.push("all targets".to_owned());
	}

	if let Some(target) = &load.target {
		parts.push(format!("target {target}"));
	}

	if !load.cfgs.is_empty() {
		parts.push(format!("cfg {}", load.cfgs.join(", ")));
	}

	parts.join("; ")
}

/// The `[features]` table, with enabled features marked, and what each feature enables.
fn features(package: &Package) -> Option<String> {
	let names: BTreeSet<&str> = package.features.keys().chain(&package.enabled_features).map(SmolStr::as_str).collect();

	if names.is_empty() {
		return None;
	}

	let features: Vec<String> = names
		.into_iter()
		.map(|name| {
			let mark = if package.enabled_features.contains(name) { "*" } else { "" };

			match package.features.get(name).filter(|enables| !enables.is_empty()) {
				Some(enables) => {
					let enables: Vec<&str> = enables.iter().map(SmolStr::as_str).collect();

					format!("{name}{mark} = [{}]", enables.join(", "))
				}
				None => format!("{name}{mark}"),
			}
		})
		.collect();

	Some(features.join(", "))
}

/// Load problems of every crate, errors first, without duplicates (files loaded by several crates report the same
/// problems).
fn diagnostics(workspace: &Workspace) -> Vec<String> {
	let root = workspace.root();
	let mut problems = BTreeSet::new();

	for krate in workspace.crates() {
		for problem in krate.diagnostics() {
			let severity = match problem.severity {
				Severity::Error => "error",
				Severity::Warning => "warning",
			};
			let place = match (&problem.file, problem.location) {
				(Some(file), Some(at)) => format!("{}: ", location(root, file, at)),
				(Some(file), None) => format!("{}: ", display(root, file)),
				(None, _) => String::new(),
			};

			// `Error` sorts after `Warning`
			problems.insert((std::cmp::Reverse(problem.severity), format!("{severity}: {place}{}", problem.message)));
		}
	}

	problems.into_iter().map(|(_, problem)| problem).collect()
}

/// A note about load errors, since items of files that failed to load are missing from results. Empty without
/// errors.
pub(crate) fn load_errors_note(workspace: &Workspace) -> String {
	let errors: BTreeSet<String> = workspace
		.crates()
		.iter()
		.flat_map(Crate::diagnostics)
		.filter(|problem| problem.severity == Severity::Error)
		.map(|problem| format!("{:?}{:?}{}", problem.file, problem.location, problem.message))
		.collect();

	match errors.len() {
		0 => String::new(),
		errors => format!(
			"note: {} while loading; items of files that could not be loaded are missing (see `workspace_info`)\n",
			count(errors, "error", "errors")
		),
	}
}

fn reference_kind(kind: ReferenceKind) -> &'static str {
	match kind {
		ReferenceKind::Definition => "definition",
		ReferenceKind::Import => "import",
		ReferenceKind::Path => "path",
		ReferenceKind::MethodCall => "method call",
		ReferenceKind::MacroToken => "inside of a macro",
		ReferenceKind::DocLink => "doc link",
	}
}

fn reference(root: &Path, reference: &Reference) -> String {
	format!("{} ({})", location(root, &reference.path, reference.start), reference_kind(reference.kind))
}

fn warnings(out: &mut String, warnings: &[String]) {
	for warning in warnings {
		writeln!(out, "warning: {warning}").unwrap();
	}
}

/// What a rename did (or would do): renamed items, edited references per file, moved module files, occurrences
/// that were left alone, collisions, and warnings.
pub(crate) fn rename(root: &Path, plan: &Rename, new_name: &str, dry_run: bool) -> String {
	let mut out = String::new();

	writeln!(
		out,
		"{} {} to `{new_name}`:",
		done(dry_run, "renamed", "rename"),
		count(plan.renamed.len(), "item", "items")
	)
	.unwrap();

	for path in &plan.renamed {
		writeln!(out, "  {path}").unwrap();
	}

	// (references, definitions) per file
	let mut files: BTreeMap<String, (usize, usize)> = BTreeMap::new();

	for reference in &plan.references {
		let file = files.entry(display(root, &reference.path)).or_default();

		match reference.kind {
			ReferenceKind::Definition => file.1 += 1,
			_ => file.0 += 1,
		}
	}

	let references: usize = files.values().map(|(references, _)| references).sum();

	writeln!(
		out,
		"{} {} in {}:",
		done(dry_run, "updated", "update"),
		count(references, "reference", "references"),
		count(files.len(), "file", "files")
	)
	.unwrap();

	for (file, &(references, definitions)) in &files {
		let mut counts = Vec::new();

		if references > 0 {
			counts.push(count(references, "reference", "references"));
		}

		if definitions > 0 {
			counts.push(count(definitions, "definition", "definitions"));
		}

		writeln!(out, "  {file}: {}", counts.join(", ")).unwrap();
	}

	for (from, to) in plan.edits.moves() {
		writeln!(out, "{} {} -> {}", done(dry_run, "moved", "move"), display(root, from), display(root, to)).unwrap();
	}

	if !plan.uncertain.is_empty() {
		out.push_str(
			"possible references left unchanged (check them; see `method_calls`, `macro_tokens`, `doc_links`):\n",
		);

		for occurrence in &plan.uncertain {
			writeln!(out, "  {}", reference(root, occurrence)).unwrap();
		}
	}

	if !plan.collisions.is_empty() {
		out.push_str("collisions (renamed anyway because of `force`):\n");

		for collision in &plan.collisions {
			writeln!(
				out,
				"  `{new_name}` in {}: {} ({})",
				collision.scope,
				collision.existing,
				location(root, &collision.file, collision.start)
			)
			.unwrap();
		}
	}

	warnings(&mut out, &plan.warnings);
	out
}

/// What a removal did (or would do): removed items, deleted files, dangling references, and warnings.
pub(crate) fn removal(root: &Path, plan: &Removal, dry_run: bool) -> String {
	let mut out = String::new();

	writeln!(out, "{} {}:", done(dry_run, "removed", "remove"), count(plan.removed.len(), "item", "items")).unwrap();

	for item in &plan.removed {
		writeln!(out, "  {} ({}) {}", item.path, item.kind, location(root, &item.file, item.start)).unwrap();
	}

	for path in plan.edits.deletions() {
		writeln!(out, "{} {}", done(dry_run, "deleted", "delete"), display(root, path)).unwrap();
	}

	if !plan.dangling.is_empty() {
		out.push_str("references left dangling (they will no longer compile):\n");

		for dangling in &plan.dangling {
			writeln!(out, "  {}", reference(root, dangling)).unwrap();
		}
	}

	warnings(&mut out, &plan.warnings);
	out
}

/// What a replacement did (or would do).
pub(crate) fn replacement(root: &Path, plan: &Replacement, dry_run: bool) -> String {
	let mut out = String::new();
	let files: Vec<String> = plan.files.iter().map(|file| display(root, file)).collect();

	writeln!(
		out,
		"{} {} in {}:",
		done(dry_run, "replaced", "replace"),
		count(plan.replaced.len(), "item", "items"),
		files.join(", ")
	)
	.unwrap();

	for path in &plan.replaced {
		writeln!(out, "  {path}").unwrap();
	}

	warnings(&mut out, &plan.warnings);
	out
}

/// What an insertion did (or would do).
pub(crate) fn insertion(root: &Path, plan: &Insertion, parent: &str, dry_run: bool) -> String {
	let mut out = String::new();

	writeln!(
		out,
		"{} {} into `{parent}` ({}):",
		done(dry_run, "inserted", "insert"),
		count(plan.inserted.len(), "item", "items"),
		display(root, &plan.file)
	)
	.unwrap();

	for (kind, name) in &plan.inserted {
		match name {
			Some(name) => writeln!(out, "  {kind} {name}").unwrap(),
			None => writeln!(out, "  {kind}").unwrap(),
		}
	}

	warnings(&mut out, &plan.warnings);
	out
}

/// Whether formatting would change the processed files (for `check`).
pub(crate) fn format_check(root: &Path, changes: &[FileChange], warnings: &[String]) -> String {
	let changed: Vec<&FileChange> = changes.iter().filter(|change| change.is_changed()).collect();
	let mut out = match (changes.len(), changed.len()) {
		(0, _) => "no files matched the targets\n".to_owned(),
		(processed, 0) => {
			format!("{} already formatted; nothing would change\n", count(processed, "file is", "files are"))
		}
		(processed, _) => format!("{} of {} would change:\n", changed.len(), count(processed, "file", "files")),
	};

	for change in &changed {
		writeln!(out, "  {}", display(root, &change.path)).unwrap();
	}

	self::warnings(&mut out, warnings);
	out
}

/// Which files were formatted.
pub(crate) fn format_written(root: &Path, processed: usize, written: &[PathBuf], warnings: &[String]) -> String {
	let mut out = match (processed, written.len()) {
		(0, _) => "no files matched the targets\n".to_owned(),
		(processed, 0) => format!("{} already formatted\n", count(processed, "file was", "files were")),
		(processed, written) => format!("formatted {written} of {}:\n", count(processed, "file", "files")),
	};

	for file in written {
		writeln!(out, "  {}", display(root, file)).unwrap();
	}

	self::warnings(&mut out, warnings);
	out
}

/// A unified diff of the changed files, followed by `rename <from> -> <to>` and `delete <path>` lines, with paths
/// relative to the workspace root.
pub(crate) fn diff(root: &Path, changes: &[FileChange], moves: &[(PathBuf, PathBuf)], deletions: &[PathBuf]) -> String {
	let changes: Vec<FileChange> = changes
		.iter()
		.filter(|change| change.is_changed())
		.map(|change| FileChange {
			path: relative(root, &change.path),
			original: change.original.clone(),
			formatted: change.formatted.clone(),
		})
		.collect();
	let mut out = match changes.is_empty() {
		true => String::new(),
		false => crate::rscode_fmt::emit::unified_diff(&changes, 3),
	};

	if !out.is_empty() && !out.ends_with('\n') {
		out.push('\n');
	}

	for (from, to) in moves {
		writeln!(out, "rename {} -> {}", display(root, from), display(root, to)).unwrap();
	}

	for path in deletions {
		writeln!(out, "delete {}", display(root, path)).unwrap();
	}

	out
}

/// Cuts text longer than `max_chars` characters (at a line break when there is one near the limit) and says what
/// was cut, and how to get less output from the tool (see [`truncation_hint`]).
pub(crate) fn truncate(text: String, max_chars: usize, hint: &str) -> String {
	let Some((limit, _)) = text.char_indices().nth(max_chars) else {
		return text;
	};

	// prefer cutting after a whole line, unless that loses too much
	let cut = match text[..limit].rfind('\n') {
		Some(line_end) if line_end + 1 >= limit / 2 => line_end + 1,
		_ => limit,
	};
	let rest = &text[cut..];

	format!(
		"{}\n[output truncated: {} ({}) not shown; {hint}]\n",
		&text[..cut],
		count(rest.lines().count(), "more line", "more lines"),
		count(rest.chars().count(), "character", "characters"),
	)
}

/// How to get less output from a tool, in terms of its own parameters.
pub(crate) fn truncation_hint(tool: &str) -> &'static str {
	match tool {
		"workspace_info" => "narrow the request: fewer `packages`",
		"find_items" => "narrow the request: a more specific `pattern`, `kinds`, or a smaller `limit` (or an `offset`)",
		"view_items" => "narrow the request: fewer or more specific `paths`, or `mode` `outline`",
		"remove_items" => {
			"narrow the request: fewer `paths`; without `dry_run`, the edit is written and only summarized"
		}
		"format_items" => {
			"narrow the request: fewer or more specific `targets`, or `skip_children`; without `check`, the changes are \
			 written and only the files are listed"
		}
		_ => "without `dry_run`, the edit is written and only summarized",
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::CfgContext;
	use crate::ItemKind;
	use crate::edit::Collision;
	use crate::edit::EditSet;
	use crate::edit::RemovedItem;
	use crate::model::CrateId;
	use crate::model::CrateSpec;
	use crate::model::Diagnostic;
	use crate::model::ItemId;
	use crate::model::PackageId;
	use crate::model::TargetKind;
	use crate::source::FileId;
	use crate::source::SourceFile;
	use crate::source::TextRange;

	const ROOT: &str = "/ws";

	fn root() -> &'static Path {
		Path::new(ROOT)
	}

	fn at(line: usize, column: usize) -> LineCol {
		LineCol { line, column }
	}

	fn found(path: &str, kind: ItemKind, file: &str, start: LineCol, end: LineCol) -> FindMatch {
		FindMatch {
			item: ItemId::new(CrateId(0), 1),
			path: path.to_owned(),
			kind,
			krate: "demo".to_owned(),
			package: Some("demo".to_owned()),
			file: PathBuf::from(file),
			start,
			end,
			range: TextRange::new(0, 1),
			visibility: "pub".to_owned(),
			cfg: None,
			active: Tristate::True,
			usable_paths: Vec::new(),
			import_targets: Vec::new(),
			thread_local: false,
		}
	}

	fn view(path: &str, kind: ItemKind, start: LineCol, end: LineCol, text: &str) -> ItemView {
		ItemView {
			item: ItemId::new(CrateId(0), 1),
			path: path.to_owned(),
			kind,
			file: PathBuf::from("/ws/src/lib.rs"),
			start,
			end,
			cfg: None,
			active: Tristate::True,
			thread_local: false,
			text: text.to_owned(),
			impls: Vec::new(),
		}
	}

	fn reference(kind: ReferenceKind, file: &str, start: LineCol) -> Reference {
		Reference {
			target: ItemId::new(CrateId(0), 1),
			kind,
			krate: CrateId(0),
			file: FileId(0),
			path: PathBuf::from(file),
			range: TextRange::new(0, 1),
			start,
			certain: kind != ReferenceKind::MethodCall,
		}
	}

	#[test]
	fn paths_are_relative_to_the_workspace_root() {
		assert_eq!(relative(root(), Path::new("/ws/src/lib.rs")), Path::new("src/lib.rs"));
		assert_eq!(relative(root(), Path::new("src/lib.rs")), Path::new("src/lib.rs"));
		assert_eq!(relative(root(), Path::new("/elsewhere/lib.rs")), Path::new("/elsewhere/lib.rs"));
		assert_eq!(relative(root(), Path::new("/ws")), Path::new("/ws"));

		// a sibling directory sharing a prefix is not inside
		assert_eq!(relative(root(), Path::new("/wsx/lib.rs")), Path::new("/wsx/lib.rs"));
	}

	#[test]
	fn last_lines() {
		assert_eq!(last_line(at(3, 1), at(5, 2)), 5);
		assert_eq!(last_line(at(3, 5), at(3, 9)), 3);
		assert_eq!(last_line(at(3, 1), at(6, 1)), 5);
		assert_eq!(last_line(at(3, 1), at(3, 1)), 3);
	}

	#[test]
	fn find_lines() {
		let mut plain = found("demo::a::Foo", ItemKind::Struct, "/ws/src/a.rs", at(3, 1), at(5, 2));
		let line = find_line(root(), &plain, false);

		assert_eq!(line, "demo::a::Foo  struct  src/a.rs:3:1-5:2");

		plain.cfg = Some("feature = \"x\"".to_owned());
		plain.active = Tristate::False;
		assert_eq!(
			find_line(root(), &plain, false),
			"demo::a::Foo  struct  src/a.rs:3:1-5:2  cfg: feature = \"x\"  inactive"
		);

		plain.active = Tristate::Unknown;
		plain.usable_paths = vec!["crate::Foo".to_owned(), "crate::a::Foo".to_owned()];
		assert_eq!(
			find_line(root(), &plain, true),
			"demo::a::Foo  struct  src/a.rs:3:1-5:2  cfg: feature = \"x\"  cfg-unknown  usable: crate::Foo, crate::a::Foo"
		);

		let mut import = found("demo::Foo", ItemKind::Import, "src/lib.rs", at(1, 9), at(1, 15));

		import.import_targets = vec!["demo::a::Foo".to_owned()];
		assert_eq!(
			find_line(root(), &import, true),
			"demo::Foo  import  src/lib.rs:1:9-1:15  -> demo::a::Foo  usable: none (not visible)"
		);
	}

	#[test]
	fn find_summaries() {
		let page = |offset, limit, total| Page {
			offset,
			limit,
			total,
		};

		assert_eq!(find_summary("Foo", 1, page(0, 100, 1)), "1 match");
		assert_eq!(find_summary("*", 3, page(0, 100, 3)), "3 matches");
		assert_eq!(find_summary("*", 2, page(0, 2, 5)), "5 matches; showing 1-2; for more, call again with `offset` 2");
		assert_eq!(find_summary("*", 2, page(2, 2, 5)), "5 matches; showing 3-4; for more, call again with `offset` 4");
		assert_eq!(find_summary("*", 1, page(4, 2, 5)), "5 matches; showing 5-5");
		assert_eq!(find_summary("*", 0, page(0, 0, 5)), "5 matches (none listed: `limit` is 0)");
		assert_eq!(find_summary("*", 0, page(9, 2, 5)), "5 matches; none left after `offset` 9");
		assert_eq!(
			find_summary("crate::a::Foo", 0, page(0, 100, 0)),
			"no items match `crate::a::Foo` (without `*`, names must match exactly: try `*Foo*`, or `ignore_case`)"
		);
		assert_eq!(find_summary("*Foo*", 0, page(0, 100, 0)), "no items match `*Foo*`");
	}

	#[test]
	fn find_output() {
		let matches = vec![
			found("demo::a", ItemKind::Module, "src/a.rs", at(1, 1), at(9, 1)),
			found("demo::a::f", ItemKind::Fn, "src/a.rs", at(2, 1), at(4, 2)),
		];
		let text = find(
			root(),
			"demo::a**",
			&matches,
			Page {
				offset: 0,
				limit: 2,
				total: 3,
			},
			false,
		);

		assert_eq!(
			text,
			"demo::a  mod  src/a.rs:1:1-9:1\ndemo::a::f  fn  src/a.rs:2:1-4:2\n3 matches; showing 1-2; for more, call again with \
			 `offset` 2\n"
		);
	}

	#[test]
	fn view_output() {
		let mut foo =
			view("demo::Foo", ItemKind::Struct, at(3, 1), at(5, 2), "   3 │ struct Foo {\n   4 │ \tx: u8,\n   5 │ }");
		let mut display = view(
			"impl Display for demo::Foo",
			ItemKind::Impl,
			at(7, 1),
			at(9, 2),
			"   7 │ impl Display for Foo { ... }\n",
		);

		display.cfg = Some("feature = \"std\"".to_owned());
		display.active = Tristate::False;
		foo.impls.push(display);

		let mut bar = view("demo::BAR", ItemKind::Const, at(11, 1), at(12, 1), "  11 │ const BAR: u8 = 1;\n");

		bar.active = Tristate::Unknown;

		let text = views(root(), &[foo, bar]);

		assert_eq!(
			text,
			"// demo::Foo (struct) src/lib.rs:3-5\n   3 │ struct Foo {\n   4 │ \tx: u8,\n   5 │ }\n\n// impl Display for demo::Foo \
			 (impl) src/lib.rs:7-9 [cfg: feature = \"std\"] [inactive]\n   7 │ impl Display for Foo { ... }\n\n// demo::BAR (const) \
			 src/lib.rs:11 [cfg-unknown]\n  11 │ const BAR: u8 = 1;\n"
		);
	}

	fn package(name: &str, member: bool) -> Package {
		Package {
			name: name.into(),
			version: "0.1.0".to_owned(),
			manifest_path: PathBuf::from(format!("/ws/{name}/Cargo.toml")),
			features: BTreeMap::new(),
			enabled_features: BTreeSet::new(),
			is_member: member,
		}
	}

	fn krate(workspace: &Workspace, name: &str, kind: TargetKind, package: u32, selected: bool) -> Crate {
		let id = CrateId(workspace.crates.len() as u32);
		let root = PathBuf::from(format!("/ws/{name}/src/lib.rs"));
		let mut spec = CrateSpec::new(name, &root);

		spec.kind = kind;
		spec.package = Some(PackageId(package));
		spec.selected = selected;
		spec.cfg = CfgContext::new();

		Crate {
			id,
			spec,
			files: vec![SourceFile::new(root, "")],
			items: Vec::new(),
			diagnostics: Vec::new(),
		}
	}

	#[test]
	fn workspace_overview() {
		let mut workspace = Workspace::new(ROOT);
		let mut app = package("app", true);

		app.features.insert("default".into(), vec![SmolStr::new("json")]);
		app.features.insert("json".into(), vec![SmolStr::new("dep:serde_json")]);
		app.features.insert("extra".into(), Vec::new());
		app.enabled_features.extend([SmolStr::new("default"), SmolStr::new("json"), SmolStr::new("serde_json")]);
		workspace.add_package(app);
		workspace.add_package(package("util", false));

		let mut app_lib = krate(&workspace, "app", TargetKind::Lib, 0, true);
		let error = Diagnostic {
			severity: Severity::Error,
			message: "expected `;`".to_owned(),
			file: Some(PathBuf::from("/ws/app/src/broken.rs")),
			location: Some(at(3, 7)),
		};

		app_lib.diagnostics.push(Diagnostic {
			severity: Severity::Warning,
			message: "file not found for module `gone`".to_owned(),
			file: Some(PathBuf::from("/ws/app/src/lib.rs")),
			location: None,
		});
		app_lib.diagnostics.push(error.clone());
		workspace.crates.push(app_lib);

		// the same file loaded again by another crate reports the same problem
		let mut util_lib = krate(&workspace, "util", TargetKind::Lib, 1, false);

		util_lib.diagnostics.push(error);
		workspace.crates.push(util_lib);

		let load = LoadOptions {
			packages: vec!["app".to_owned()],
			features: vec!["extra".to_owned()],
			..LoadOptions::default()
		};

		assert_eq!(
			workspace_info(&workspace, &load),
			"workspace root: /ws\nselection: packages app; features extra\n\npackages:\n  app 0.1.0  app/Cargo.toml\n    features \
			 (* = enabled): default* = [json], extra, json* = [dep:serde_json], serde_json*\n  util 0.1.0  util/Cargo.toml  (not \
			 a workspace member)\n\ncrates:\n  app  lib  app/src/lib.rs  edition 2024  package app  selected  (1 file, 0 items)\n  \
			 util  lib  util/src/lib.rs  edition 2024  package util  not selected  (1 file, 0 items)\n\nload problems:\n  error: \
			 app/src/broken.rs:3:7: expected `;`\n  warning: app/src/lib.rs: file not found for module `gone`\n"
		);
		assert_eq!(
			load_errors_note(&workspace),
			"note: 1 error while loading; items of files that could not be loaded are missing (see `workspace_info`)\n"
		);

		workspace.crates.clear();
		assert_eq!(load_errors_note(&workspace), "");
		assert!(workspace_info(&workspace, &LoadOptions::default()).ends_with("\nload problems: none\n"));
	}

	#[test]
	fn selections() {
		assert_eq!(selection(&LoadOptions::default()), "cargo's default packages");

		let load = LoadOptions {
			workspace: true,
			exclude: vec!["big".to_owned()],
			features: vec!["a".to_owned()],
			all_features: true,
			no_default_features: true,
			target: Some("wasm32-unknown-unknown".to_owned()),
			cfgs: vec!["test".to_owned()],
			..LoadOptions::default()
		};

		assert_eq!(
			selection(&load),
			"every workspace member; excluding big; all features; no default features; target wasm32-unknown-unknown; cfg test"
		);
	}

	#[test]
	fn rename_reports() {
		let mut edits = EditSet::new();

		edits.move_path("/ws/src/old.rs", "/ws/src/new.rs");

		let plan = Rename {
			edits,
			renamed: vec!["demo::old".to_owned()],
			references: vec![
				reference(ReferenceKind::Definition, "/ws/src/lib.rs", at(1, 5)),
				reference(ReferenceKind::Import, "/ws/src/main.rs", at(1, 11)),
				reference(ReferenceKind::Path, "/ws/src/main.rs", at(4, 5)),
			],
			uncertain: vec![reference(ReferenceKind::MethodCall, "/ws/src/main.rs", at(9, 7))],
			collisions: vec![Collision {
				scope: "demo".to_owned(),
				existing: "demo::new".to_owned(),
				file: PathBuf::from("/ws/src/lib.rs"),
				start: at(12, 1),
			}],
			warnings: vec!["a macro may use `old`".to_owned()],
		};

		assert_eq!(
			rename(root(), &plan, "new", false),
			"renamed 1 item to `new`:\n  demo::old\nupdated 2 references in 2 files:\n  src/lib.rs: 1 definition\n  src/main.rs: 2 \
			 references\nmoved src/old.rs -> src/new.rs\npossible references left unchanged (check them; see \
			 `method_calls`, `macro_tokens`, `doc_links`):\n  src/main.rs:9:7 (method call)\ncollisions (renamed anyway because \
			 of `force`):\n  `new` in demo: demo::new (src/lib.rs:12:1)\nwarning: a macro may use `old`\n"
		);
		assert!(
			rename(root(), &plan, "new", true)
				.starts_with("would rename 1 item to `new`:\n  demo::old\nwould update 2 references")
		);
	}

	#[test]
	fn removal_reports() {
		let mut edits = EditSet::new();

		edits.delete_path("/ws/src/a.rs");

		let plan = Removal {
			edits,
			removed: vec![RemovedItem {
				path: "demo::a".to_owned(),
				kind: ItemKind::Module,
				file: PathBuf::from("/ws/src/lib.rs"),
				start: at(2, 1),
				end: at(2, 7),
			}],
			dangling: vec![reference(ReferenceKind::Path, "/ws/src/main.rs", at(3, 5))],
			warnings: Vec::new(),
		};

		assert_eq!(
			removal(root(), &plan, false),
			"removed 1 item:\n  demo::a (mod) src/lib.rs:2:1\ndeleted src/a.rs\nreferences left dangling (they will no longer \
			 compile):\n  src/main.rs:3:5 (path)\n"
		);
		assert_eq!(
			removal(root(), &plan, true),
			"would remove 1 item:\n  demo::a (mod) src/lib.rs:2:1\nwould delete src/a.rs\nreferences left dangling (they will no \
			 longer compile):\n  src/main.rs:3:5 (path)\n"
		);
	}

	#[test]
	fn replacement_and_insertion_reports() {
		let replaced = Replacement {
			edits: EditSet::new(),
			replaced: vec!["demo::f".to_owned(), "demo::f".to_owned()],
			files: vec![PathBuf::from("/ws/src/unix.rs"), PathBuf::from("/ws/src/windows.rs")],
			warnings: vec!["the new item is named `g`".to_owned()],
		};

		assert_eq!(
			replacement(root(), &replaced, true),
			"would replace 2 items in src/unix.rs, src/windows.rs:\n  demo::f\n  demo::f\nwarning: the new item is named `g`\n"
		);

		let inserted = Insertion {
			edits: EditSet::new(),
			inserted: vec![(ItemKind::AssocFn, Some("new".to_owned())), (ItemKind::MacroCall, None)],
			file: PathBuf::from("/ws/src/lib.rs"),
			warnings: Vec::new(),
		};

		assert_eq!(
			insertion(root(), &inserted, "impl demo::Foo", false),
			"inserted 2 items into `impl demo::Foo` (src/lib.rs):\n  assoc-fn new\n  macro-call\n"
		);
	}

	fn change(path: &str, original: &str, formatted: &str) -> FileChange {
		FileChange {
			path: PathBuf::from(path),
			original: original.to_owned(),
			formatted: formatted.to_owned(),
		}
	}

	#[test]
	fn format_reports() {
		let changes = [change("/ws/src/lib.rs", "a", "b"), change("/ws/src/a.rs", "x", "x")];

		assert_eq!(format_check(root(), &changes, &[]), "1 of 2 files would change:\n  src/lib.rs\n");
		assert_eq!(format_check(root(), &changes[1..], &[]), "1 file is already formatted; nothing would change\n");
		assert_eq!(format_check(root(), &[], &["w".to_owned()]), "no files matched the targets\nwarning: w\n");

		assert_eq!(
			format_written(root(), 2, &[PathBuf::from("/ws/src/lib.rs")], &[]),
			"formatted 1 of 2 files:\n  src/lib.rs\n"
		);
		assert_eq!(format_written(root(), 3, &[], &[]), "3 files were already formatted\n");
		assert_eq!(format_written(root(), 0, &[], &[]), "no files matched the targets\n");
	}

	#[test]
	fn diffs_of_file_operations() {
		let unchanged = [change("/ws/src/lib.rs", "x", "x")];
		let moves = [(PathBuf::from("/ws/src/a.rs"), PathBuf::from("/ws/src/b.rs"))];
		let deletions = [PathBuf::from("/ws/src/c")];

		assert_eq!(diff(root(), &unchanged, &moves, &deletions), "rename src/a.rs -> src/b.rs\ndelete src/c\n");
		assert_eq!(diff(root(), &[], &[], &[]), "");
	}

	#[test]
	fn truncation() {
		let truncate = |text: String, max_chars| truncate(text, max_chars, "hint");

		assert_eq!(truncate("short".to_owned(), 10), "short");
		assert_eq!(truncate("exactly10!".to_owned(), 10), "exactly10!");

		// cut after the last whole line within the limit
		let text = "line one\nline two\nline three\n".to_owned();
		let cut = truncate(text, 20);

		assert!(
			cut.starts_with("line one\nline two\n\n[output truncated: 1 more line (11 characters) not shown;"),
			"{cut}"
		);

		// a single long line is cut at the limit, on a char boundary
		let cut = truncate("ééééé".to_owned(), 2);

		assert!(cut.starts_with("éé\n[output truncated: 1 more line (3 characters) not shown;"), "{cut}");

		// a line break too early is ignored
		let cut = truncate(format!("a\n{}", "b".repeat(30)), 20);

		assert!(
			cut.starts_with(&format!("a\n{}\n[output truncated: 1 more line (12 characters)", "b".repeat(18))),
			"{cut}"
		);
	}
}
