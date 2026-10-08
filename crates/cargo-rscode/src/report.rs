//! Reports of edits: what was (or, for dry runs, would be) done, for people (a summary, plus warnings and notes)
//! and programs (JSON).

use crate::render::PathDisplay;
use crate::ui::Level;
use rscode::ItemKind;
use rscode::edit::Collision;
use rscode::edit::ImportAddition;
use rscode::edit::ImportOutcome;
use rscode::edit::Insertion;
use rscode::edit::ItemSpan;
use rscode::edit::ModuleCreation;
use rscode::edit::Removal;
use rscode::edit::Rename;
use rscode::edit::Replacement;
use rscode::resolve::Reference;
use rscode::resolve::ReferenceKind;
use rscode::source::LineCol;
use serde::Serialize;
use std::fmt::Display;
use std::path::Path;

/// An existing name a rename collides with.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub(crate) struct CollisionRow {
	pub(crate) scope: String,
	pub(crate) existing: String,

	#[serde(flatten)]
	pub(crate) location: Location,
}

impl CollisionRow {
	fn new(collision: &Collision, paths: &PathDisplay) -> Self {
		Self {
			scope: collision.scope.clone(),
			existing: collision.existing.clone(),
			location: Location::new(&collision.file, collision.start, paths),
		}
	}
}

/// A report of an edit operation.
pub(crate) trait EditReport: Serialize {
	/// The diff of a dry run.
	fn diff(&self) -> Option<&str>;

	fn dry_run(&self) -> bool;

	/// Warnings and notes.
	fn messages(&self) -> Vec<(Level, String)>;

	/// What was (or would be) done, one line each.
	fn summary(&self) -> String;
}

/// An item that `edit` edited, with its lines after the edit.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub(crate) struct EditedRow {
	/// The canonical path of the item.
	pub(crate) path: String,

	pub(crate) file: String,

	/// The first line of the item after the edit.
	pub(crate) start: usize,

	/// The last line of the item after the edit.
	pub(crate) end: usize,
}

impl EditedRow {
	fn new(span: &ItemSpan, paths: &PathDisplay) -> Self {
		Self {
			path: span.path.clone(),
			file: paths.display(&span.file),
			start: span.start,
			end: span.end,
		}
	}
}

/// `fmt`/`sort` writing files.
#[derive(Debug, Default, Serialize)]
pub(crate) struct FormatReport {
	/// Written files.
	pub(crate) files: Vec<String>,

	pub(crate) warnings: Vec<String>,

	/// Whether files were only sorted (`sort`).
	#[serde(skip)]
	pub(crate) sort_only: bool,
}

impl FormatReport {
	pub(crate) fn summary(&self) -> String {
		let verb = if self.sort_only { "sorted" } else { "formatted" };

		self.files.iter().map(|file| format!("{verb} {file}\n")).collect()
	}
}

/// `import`
#[derive(Debug, Default, Serialize)]
pub(crate) struct ImportReport {
	pub(crate) module: String,
	pub(crate) imports: Vec<ImportRow>,
	pub(crate) file: String,
	pub(crate) warnings: Vec<String>,
	pub(crate) dry_run: bool,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) diff: Option<String>,
}

impl ImportReport {
	pub(crate) fn new(plan: &ImportAddition, module: &str, dry_run: bool, paths: &PathDisplay) -> Self {
		Self {
			module: module.to_owned(),
			imports: (plan.imports.iter())
				.map(|import| ImportRow {
					path: import.path.clone(),
					outcome: match &import.outcome {
						ImportOutcome::Added => "added",
						ImportOutcome::Merged(_) => "merged",
						ImportOutcome::Present => "present",
					},
					item: match &import.outcome {
						ImportOutcome::Merged(item) => Some(item.clone()),
						_ => None,
					},
					line: import.line,
				})
				.collect(),
			file: paths.display(&plan.file),
			warnings: plan.warnings.clone(),
			dry_run,
			diff: None,
		}
	}
}

impl EditReport for ImportReport {
	fn diff(&self) -> Option<&str> {
		self.diff.as_deref()
	}

	fn dry_run(&self) -> bool {
		self.dry_run
	}

	fn messages(&self) -> Vec<(Level, String)> {
		self.warnings.iter().map(|warning| (Level::Warning, warning.clone())).collect()
	}

	fn summary(&self) -> String {
		let mut out = String::new();

		for import in &self.imports {
			let (path, file, line) = (&import.path, &self.file, import.line);

			out.push_str(&match (import.outcome, &import.item) {
				("added", _) => format!("{} `{path}` ({file}:{line})\n", done(self.dry_run, "import", "imported")),

				("merged", Some(item)) if !item.contains('\n') => {
					format!("{} `{path}` into `{item}` ({file}:{line})\n", done(self.dry_run, "merge", "merged"))
				}

				("merged", _) => {
					format!("{} `{path}` into the `use` item at {file}:{line}\n", done(self.dry_run, "merge", "merged"))
				}

				_ => format!("`{path}` is already imported ({file}:{line})\n"),
			});
		}

		out
	}
}

/// An import of an [`ImportReport`].
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ImportRow {
	pub(crate) path: String,

	/// `added`, `merged`, or `present`.
	pub(crate) outcome: &'static str,

	/// The new text of the `use` item it was merged into.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) item: Option<String>,

	pub(crate) line: usize,
}

/// An inserted item.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub(crate) struct InsertedRow {
	pub(crate) kind: ItemKind,
	pub(crate) name: Option<String>,
}

/// `insert`
#[derive(Debug, Default, Serialize)]
pub(crate) struct InsertionReport {
	pub(crate) parent: String,
	pub(crate) inserted: Vec<InsertedRow>,
	pub(crate) file: String,

	/// Files formatted afterwards (`--fmt`).
	pub(crate) formatted: Vec<String>,

	pub(crate) warnings: Vec<String>,
	pub(crate) dry_run: bool,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) diff: Option<String>,
}

impl InsertionReport {
	pub(crate) fn new(plan: &Insertion, parent: &str, dry_run: bool, paths: &PathDisplay) -> Self {
		Self {
			parent: parent.to_owned(),
			inserted: plan
				.inserted
				.iter()
				.map(|(kind, name)| InsertedRow {
					kind: *kind,
					name: name.clone(),
				})
				.collect(),
			file: paths.display(&plan.file),
			formatted: Vec::new(),
			warnings: plan.warnings.clone(),
			dry_run,
			diff: None,
		}
	}
}

impl EditReport for InsertionReport {
	fn diff(&self) -> Option<&str> {
		self.diff.as_deref()
	}

	fn dry_run(&self) -> bool {
		self.dry_run
	}

	fn messages(&self) -> Vec<(Level, String)> {
		self.warnings.iter().map(|warning| (Level::Warning, warning.clone())).collect()
	}

	fn summary(&self) -> String {
		let mut out = String::new();

		for item in &self.inserted {
			let verb = done(self.dry_run, "insert", "inserted");
			let name = item.name.as_ref().map(|name| format!(" {name}")).unwrap_or_default();

			out.push_str(&format!("{verb} {}{name} into {} ({})\n", item.kind, self.parent, self.file));
		}

		for file in &self.formatted {
			out.push_str(&format!("formatted {file}\n"));
		}

		out
	}
}

/// `edit`
#[derive(Debug, Default, Serialize)]
pub(crate) struct ItemEditReport {
	/// The edited items.
	pub(crate) edited: Vec<EditedRow>,

	pub(crate) files: Vec<String>,

	/// Files formatted afterwards (`--fmt`).
	pub(crate) formatted: Vec<String>,

	pub(crate) warnings: Vec<String>,

	/// Parts of the edit that changed nothing.
	pub(crate) notes: Vec<String>,

	pub(crate) dry_run: bool,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) diff: Option<String>,
}

impl ItemEditReport {
	pub(crate) fn new(plan: &Replacement, dry_run: bool, paths: &PathDisplay) -> Self {
		Self {
			edited: plan.spans.iter().map(|span| EditedRow::new(span, paths)).collect(),
			files: plan.files.iter().map(|file| paths.display(file)).collect(),
			formatted: Vec::new(),
			warnings: plan.warnings.clone(),
			notes: plan.notes.clone(),
			dry_run,
			diff: None,
		}
	}
}

impl EditReport for ItemEditReport {
	fn diff(&self) -> Option<&str> {
		self.diff.as_deref()
	}

	fn dry_run(&self) -> bool {
		self.dry_run
	}

	fn messages(&self) -> Vec<(Level, String)> {
		(self.warnings.iter().map(|warning| (Level::Warning, warning.clone())))
			.chain(self.notes.iter().map(|note| (Level::Note, note.clone())))
			.collect()
	}

	fn summary(&self) -> String {
		let mut out = String::new();

		if self.edited.is_empty() {
			out.push_str(if self.dry_run { "nothing would change\n" } else { "nothing changed\n" });
		}

		for row in &self.edited {
			let lines = match row.end > row.start {
				true => format!("{}-{}", row.start, row.end),
				false => row.start.to_string(),
			};

			out.push_str(&format!("{} {} ({}:{lines})\n", done(self.dry_run, "edit", "edited"), row.path, row.file));
		}

		for file in &self.formatted {
			out.push_str(&format!("formatted {file}\n"));
		}

		out
	}
}

/// A position in a file.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub(crate) struct Location {
	pub(crate) file: String,
	pub(crate) line: usize,
	pub(crate) column: usize,
}

impl Location {
	pub(crate) fn new(file: &Path, at: LineCol, paths: &PathDisplay) -> Self {
		Self {
			file: paths.display(file),
			line: at.line,
			column: at.column,
		}
	}
}

impl Display for Location {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}:{}:{}", self.file, self.line, self.column)
	}
}

/// `create-module`
#[derive(Debug, Default, Serialize)]
pub(crate) struct ModuleReport {
	pub(crate) parent: String,

	/// The new module's file.
	pub(crate) created: String,

	/// The module's declaration (`mod name;`).
	pub(crate) declaration: String,

	/// The file of the declaration, and its line there.
	pub(crate) file: String,
	pub(crate) line: usize,

	pub(crate) warnings: Vec<String>,
	pub(crate) dry_run: bool,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) diff: Option<String>,
}

impl ModuleReport {
	pub(crate) fn new(plan: &ModuleCreation, parent: &str, dry_run: bool, paths: &PathDisplay) -> Self {
		Self {
			parent: parent.to_owned(),
			created: paths.display(&plan.file),
			declaration: plan.declaration.clone(),
			file: paths.display(&plan.declared_in),
			line: plan.line,
			warnings: plan.warnings.clone(),
			dry_run,
			diff: None,
		}
	}
}

impl EditReport for ModuleReport {
	fn diff(&self) -> Option<&str> {
		self.diff.as_deref()
	}

	fn dry_run(&self) -> bool {
		self.dry_run
	}

	fn messages(&self) -> Vec<(Level, String)> {
		self.warnings.iter().map(|warning| (Level::Warning, warning.clone())).collect()
	}

	fn summary(&self) -> String {
		format!(
			"{} {}; {} `{}` into {} ({}:{})\n",
			done(self.dry_run, "create", "created"),
			self.created,
			done(self.dry_run, "insert", "inserted"),
			self.declaration,
			self.parent,
			self.file,
			self.line
		)
	}
}

/// An occurrence of a name.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub(crate) struct Occurrence {
	#[serde(flatten)]
	pub(crate) location: Location,

	pub(crate) kind: ReferenceKind,
}

impl Occurrence {
	pub(crate) fn new(reference: &Reference, paths: &PathDisplay) -> Self {
		Self {
			location: Location::new(&reference.path, reference.start, paths),
			kind: reference.kind,
		}
	}
}

/// `remove`
#[derive(Debug, Default, Serialize)]
pub(crate) struct RemovalReport {
	pub(crate) removed: Vec<RemovedRow>,

	/// Deleted files and directories.
	pub(crate) deleted: Vec<String>,

	/// References to the removed items that remain.
	pub(crate) dangling: Vec<Location>,

	pub(crate) warnings: Vec<String>,

	/// Files with text edits.
	pub(crate) files: Vec<String>,

	pub(crate) dry_run: bool,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) diff: Option<String>,
}

impl RemovalReport {
	pub(crate) fn new(plan: &Removal, dry_run: bool, paths: &PathDisplay) -> Self {
		Self {
			removed: plan
				.removed
				.iter()
				.map(|item| RemovedRow {
					path: item.path.clone(),
					kind: item.kind,
					location: Location::new(&item.file, item.start, paths),
				})
				.collect(),
			deleted: plan.edits.deletions().iter().map(|path| paths.display(path)).collect(),
			dangling: plan
				.dangling
				.iter()
				.map(|reference| Location::new(&reference.path, reference.start, paths))
				.collect(),
			warnings: plan.warnings.clone(),
			files: plan.edits.edited_files().map(|file| paths.display(file)).collect(),
			dry_run,
			diff: None,
		}
	}
}

impl EditReport for RemovalReport {
	fn diff(&self) -> Option<&str> {
		self.diff.as_deref()
	}

	fn dry_run(&self) -> bool {
		self.dry_run
	}

	fn messages(&self) -> Vec<(Level, String)> {
		let warnings = self.warnings.iter().map(|warning| (Level::Warning, warning.clone()));
		let dangling = self
			.dangling
			.iter()
			.map(|location| (Level::Warning, format!("dangling reference at {location}")));

		warnings.chain(dangling).collect()
	}

	fn summary(&self) -> String {
		let mut out = String::new();

		for item in &self.removed {
			let verb = done(self.dry_run, "remove", "removed");

			out.push_str(&format!("{verb} {} ({}) {}\n", item.path, item.kind, item.location));
		}

		for path in &self.deleted {
			out.push_str(&format!("{} {path}\n", done(self.dry_run, "delete", "deleted")));
		}

		out
	}
}

/// A removed item.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub(crate) struct RemovedRow {
	pub(crate) path: String,
	pub(crate) kind: ItemKind,

	#[serde(flatten)]
	pub(crate) location: Location,
}

/// `rename`
#[derive(Debug, Default, Serialize)]
pub(crate) struct RenameReport {
	pub(crate) new_name: String,

	/// Canonical paths of the renamed items.
	pub(crate) renamed: Vec<String>,

	/// Every edited occurrence, definitions included.
	pub(crate) references: Vec<Occurrence>,

	/// Occurrences that might refer to the items, left untouched.
	pub(crate) uncertain: Vec<Occurrence>,

	/// Collisions with existing names (with `--force`).
	pub(crate) collisions: Vec<CollisionRow>,

	pub(crate) warnings: Vec<String>,

	/// Files with text edits.
	pub(crate) files: Vec<String>,

	/// Moved module files: `[from, to]`.
	pub(crate) moved: Vec<[String; 2]>,

	pub(crate) dry_run: bool,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) diff: Option<String>,
}

impl RenameReport {
	pub(crate) fn new(plan: &Rename, new_name: &str, dry_run: bool, paths: &PathDisplay) -> Self {
		Self {
			new_name: new_name.to_owned(),
			renamed: plan.renamed.clone(),
			references: plan.references.iter().map(|reference| Occurrence::new(reference, paths)).collect(),
			uncertain: plan.uncertain.iter().map(|reference| Occurrence::new(reference, paths)).collect(),
			collisions: plan.collisions.iter().map(|collision| CollisionRow::new(collision, paths)).collect(),
			warnings: plan.warnings.clone(),
			files: plan.edits.edited_files().map(|file| paths.display(file)).collect(),
			moved: plan
				.edits
				.moves()
				.iter()
				.map(|(from, to)| [paths.display(from), paths.display(to)])
				.collect(),
			dry_run,
			diff: None,
		}
	}
}

impl EditReport for RenameReport {
	fn diff(&self) -> Option<&str> {
		self.diff.as_deref()
	}

	fn dry_run(&self) -> bool {
		self.dry_run
	}

	fn messages(&self) -> Vec<(Level, String)> {
		let warnings = self.warnings.iter().map(|warning| (Level::Warning, warning.clone()));
		let collisions = self.collisions.iter().map(|collision| {
			let message = format!(
				"`{}` collides with `{}` in `{}` ({})",
				self.new_name, collision.existing, collision.scope, collision.location
			);

			(Level::Warning, message)
		});
		let uncertain = self
			.uncertain
			.iter()
			.map(|occurrence| (Level::Note, format!("possible reference not updated: {}", occurrence.location)));

		warnings.chain(collisions).chain(uncertain).collect()
	}

	fn summary(&self) -> String {
		let references = self
			.references
			.iter()
			.filter(|reference| reference.kind != ReferenceKind::Definition)
			.count();
		let mut out = format!(
			"{} {}, {} {} in {}\n",
			done(self.dry_run, "rename", "renamed"),
			count(self.renamed.len(), "item", "items"),
			if self.dry_run { "update" } else { "updated" },
			count(references, "reference", "references"),
			count(self.files.len(), "file", "files"),
		);

		for [from, to] in &self.moved {
			out.push_str(&format!("{} {from} -> {to}\n", done(self.dry_run, "move", "moved")));
		}

		out
	}
}

/// `replace`
#[derive(Debug, Default, Serialize)]
pub(crate) struct ReplacementReport {
	/// Canonical paths of the replaced items.
	pub(crate) replaced: Vec<String>,

	pub(crate) files: Vec<String>,

	/// Files formatted afterwards (`--fmt`).
	pub(crate) formatted: Vec<String>,

	pub(crate) warnings: Vec<String>,
	pub(crate) dry_run: bool,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) diff: Option<String>,
}

impl ReplacementReport {
	pub(crate) fn new(plan: &Replacement, dry_run: bool, paths: &PathDisplay) -> Self {
		Self {
			replaced: plan.replaced.clone(),
			files: plan.files.iter().map(|file| paths.display(file)).collect(),
			formatted: Vec::new(),
			warnings: plan.warnings.clone(),
			dry_run,
			diff: None,
		}
	}
}

impl EditReport for ReplacementReport {
	fn diff(&self) -> Option<&str> {
		self.diff.as_deref()
	}

	fn dry_run(&self) -> bool {
		self.dry_run
	}

	fn messages(&self) -> Vec<(Level, String)> {
		self.warnings.iter().map(|warning| (Level::Warning, warning.clone())).collect()
	}

	fn summary(&self) -> String {
		let mut out = String::new();

		for path in &self.replaced {
			out.push_str(&format!("{} {path}\n", done(self.dry_run, "replace", "replaced")));
		}

		for file in &self.formatted {
			out.push_str(&format!("formatted {file}\n"));
		}

		out
	}
}

/// `1 item`, `2 items`
fn count(count: usize, singular: &str, plural: &str) -> String {
	match count {
		1 => format!("1 {singular}"),
		_ => format!("{count} {plural}"),
	}
}

/// The past tense of a verb, or `would <verb>` for dry runs.
fn done(dry_run: bool, verb: &str, past: &str) -> String {
	match dry_run {
		true => format!("would {verb}"),
		false => past.to_owned(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use rscode::CrateSpec;
	use rscode::ItemId;
	use rscode::Workspace;
	use rscode::edit::EditSet;
	use rscode::edit::RemovedItem;
	use std::path::PathBuf;

	fn at(line: usize, column: usize) -> LineCol {
		LineCol { line, column }
	}

	#[test]
	fn builds_import_reports() {
		let import = |path: &str, outcome: ImportOutcome, line: usize| rscode::edit::AddedImport {
			path: path.to_owned(),
			outcome,
			line,
		};
		let plan = ImportAddition {
			edits: EditSet::new(),
			imports: vec![
				import("std::env", ImportOutcome::Added, 2),
				import("std::fs", ImportOutcome::Merged("use std::{fs, io};".to_owned()), 1),
				import("std::fmt", ImportOutcome::Present, 3),
			],
			file: PathBuf::from("/ws/src/lib.rs"),
			warnings: Vec::new(),
		};
		let report = ImportReport::new(&plan, "crate", false, &paths());

		assert_eq!(
			report.summary(),
			"imported `std::env` (src/lib.rs:2)\nmerged `std::fs` into `use std::{fs, io};` (src/lib.rs:1)\n\
			 `std::fmt` is already imported (src/lib.rs:3)\n"
		);
		assert_eq!(
			serde_json::to_value(&report).unwrap()["imports"],
			serde_json::json!([
				{"path": "std::env", "outcome": "added", "line": 2},
				{"path": "std::fs", "outcome": "merged", "item": "use std::{fs, io};", "line": 1},
				{"path": "std::fmt", "outcome": "present", "line": 3},
			])
		);
		assert!(ImportReport::new(&plan, "crate", true, &paths()).summary().starts_with("would import `std::env`"));
	}

	#[test]
	fn builds_insertion_reports() {
		let plan = Insertion {
			edits: EditSet::new(),
			inserted: vec![(ItemKind::Fn, Some("helper".to_owned())), (ItemKind::Impl, None)],
			imports: Vec::new(),
			parent: "demo::util".to_owned(),
			container: ItemId::crate_root(Workspace::new("/ws").load_crate(CrateSpec::new("demo", "/ws/src/lib.rs"))),
			file: PathBuf::from("/ws/src/util.rs"),
			warnings: Vec::new(),
		};
		let report = InsertionReport::new(&plan, "crate::util", false, &paths());

		assert_eq!(
			report.summary(),
			"inserted fn helper into crate::util (src/util.rs)\ninserted impl into crate::util (src/util.rs)\n"
		);

		let json = serde_json::to_value(&report).unwrap();

		assert_eq!(json["inserted"][0], serde_json::json!({"kind": "fn", "name": "helper"}));
		assert_eq!(json["inserted"][1], serde_json::json!({"kind": "impl", "name": null}));
		assert_eq!(
			InsertionReport::new(&plan, "crate::util", true, &paths()).summary(),
			"would insert fn helper into crate::util (src/util.rs)\nwould insert impl into crate::util (src/util.rs)\n"
		);
	}

	#[test]
	fn builds_item_edit_reports() {
		let plan = Replacement {
			edits: EditSet::new(),
			replaced: vec!["demo::add".to_owned()],
			files: vec![PathBuf::from("/ws/src/lib.rs")],
			spans: vec![ItemSpan {
				path: "demo::add".to_owned(),
				file: PathBuf::from("/ws/src/lib.rs"),
				start: 3,
				end: 5,
			}],
			warnings: vec!["w".to_owned()],
			notes: vec!["n".to_owned()],
		};
		let report = ItemEditReport::new(&plan, false, &paths());

		assert_eq!(report.summary(), "edited demo::add (src/lib.rs:3-5)\n");
		assert_eq!(report.messages(), [(Level::Warning, "w".to_owned()), (Level::Note, "n".to_owned())]);
		assert_eq!(
			serde_json::to_value(&report).unwrap(),
			serde_json::json!({
				"edited": [{ "path": "demo::add", "file": "src/lib.rs", "start": 3, "end": 5 }],
				"files": ["src/lib.rs"],
				"formatted": [],
				"warnings": ["w"],
				"notes": ["n"],
				"dry_run": false,
			})
		);

		let unchanged = Replacement {
			spans: Vec::new(),
			..plan
		};

		assert_eq!(ItemEditReport::new(&unchanged, true, &paths()).summary(), "nothing would change\n");
	}

	#[test]
	fn builds_module_reports() {
		let plan = ModuleCreation {
			edits: EditSet::new(),
			file: PathBuf::from("/ws/src/util/render.rs"),
			declaration: "pub mod render;".to_owned(),
			declared_in: PathBuf::from("/ws/src/util.rs"),
			line: 3,
			warnings: Vec::new(),
		};
		let report = ModuleReport::new(&plan, "crate::util", false, &paths());

		assert_eq!(
			report.summary(),
			"created src/util/render.rs; inserted `pub mod render;` into crate::util (src/util.rs:3)\n"
		);
		assert_eq!(
			serde_json::to_value(&report).unwrap(),
			serde_json::json!({
				"parent": "crate::util",
				"created": "src/util/render.rs",
				"declaration": "pub mod render;",
				"file": "src/util.rs",
				"line": 3,
				"warnings": [],
				"dry_run": false,
			})
		);
		assert_eq!(
			ModuleReport::new(&plan, "crate::util", true, &paths()).summary(),
			"would create src/util/render.rs; would insert `pub mod render;` into crate::util (src/util.rs:3)\n"
		);
	}

	#[test]
	fn builds_removal_reports() {
		let mut edits = EditSet::new();

		edits.delete_path("/ws/src/gone.rs");

		let plan = Removal {
			edits,
			removed: vec![RemovedItem {
				path: "demo::gone".to_owned(),
				kind: ItemKind::Module,
				file: PathBuf::from("src/lib.rs"),
				start: at(2, 1),
				end: at(2, 10),
			}],
			dangling: Vec::new(),
			warnings: Vec::new(),
		};
		let report = RemovalReport::new(&plan, false, &paths());

		assert_eq!(report.summary(), "removed demo::gone (mod) src/lib.rs:2:1\ndeleted src/gone.rs\n");
		assert!(report.messages().is_empty());

		let dry = RemovalReport {
			dry_run: true,
			dangling: vec![Location {
				file: "src/main.rs".to_owned(),
				line: 3,
				column: 9,
			}],
			..RemovalReport::new(&plan, true, &paths())
		};

		assert_eq!(dry.summary(), "would remove demo::gone (mod) src/lib.rs:2:1\nwould delete src/gone.rs\n");
		assert_eq!(dry.messages(), [(Level::Warning, "dangling reference at src/main.rs:3:9".to_owned())]);
	}

	#[test]
	fn builds_rename_reports() {
		let mut edits = EditSet::new();

		edits.move_path("/ws/src/old.rs", "/ws/src/new.rs");

		let plan = Rename {
			edits,
			renamed: vec!["demo::old".to_owned()],
			references: Vec::new(),
			uncertain: Vec::new(),
			collisions: vec![Collision {
				scope: "demo".to_owned(),
				existing: "demo::new".to_owned(),
				file: PathBuf::from("/ws/src/lib.rs"),
				start: at(3, 1),
			}],
			warnings: vec!["macro bodies were not searched".to_owned()],
		};
		let report = RenameReport::new(&plan, "new", true, &paths());

		assert_eq!(report.renamed, ["demo::old"]);
		assert_eq!(report.moved, [["src/old.rs".to_owned(), "src/new.rs".to_owned()]]);
		assert_eq!(report.collisions[0].location.to_string(), "src/lib.rs:3:1");
		assert!(report.dry_run && report.diff.is_none());
		assert_eq!(
			report.summary(),
			"would rename 1 item, update 0 references in 0 files\nwould move src/old.rs -> src/new.rs\n"
		);
		assert_eq!(
			report.messages(),
			[
				(Level::Warning, "macro bodies were not searched".to_owned()),
				(Level::Warning, "`new` collides with `demo::new` in `demo` (src/lib.rs:3:1)".to_owned()),
			]
		);
	}

	#[test]
	fn builds_replacement_reports() {
		let plan = Replacement {
			edits: EditSet::new(),
			replaced: vec!["demo::add".to_owned()],
			files: vec![PathBuf::from("/ws/src/lib.rs")],
			spans: Vec::new(),
			warnings: vec!["w".to_owned()],
			notes: Vec::new(),
		};
		let mut report = ReplacementReport::new(&plan, false, &paths());

		assert_eq!(report.files, ["src/lib.rs"]);
		assert_eq!(report.summary(), "replaced demo::add\n");

		report.formatted.push("src/lib.rs".to_owned());

		assert_eq!(report.summary(), "replaced demo::add\nformatted src/lib.rs\n");
		assert_eq!(report.messages(), [(Level::Warning, "w".to_owned())]);
		assert_eq!(ReplacementReport::new(&plan, true, &paths()).summary(), "would replace demo::add\n");
	}

	fn occurrence(file: &str, line: usize, kind: ReferenceKind) -> Occurrence {
		Occurrence {
			location: Location {
				file: file.to_owned(),
				line,
				column: 5,
			},
			kind,
		}
	}

	fn paths() -> PathDisplay {
		PathDisplay::with_cwd(Path::new("/ws"), Some(Path::new("/ws")), false)
	}

	#[test]
	fn pluralizes() {
		assert_eq!(count(0, "item", "items"), "0 items");
		assert_eq!(count(1, "item", "items"), "1 item");
		assert_eq!(count(2, "file", "files"), "2 files");
	}

	#[test]
	fn summarizes_formatting() {
		let mut report = FormatReport {
			files: vec!["src/lib.rs".to_owned(), "src/a.rs".to_owned()],
			warnings: vec!["w".to_owned()],
			sort_only: false,
		};

		assert_eq!(report.summary(), "formatted src/lib.rs\nformatted src/a.rs\n");
		assert_eq!(
			serde_json::to_value(&report).unwrap(),
			serde_json::json!({"files": ["src/lib.rs", "src/a.rs"], "warnings": ["w"]})
		);

		report.sort_only = true;

		assert_eq!(report.summary(), "sorted src/lib.rs\nsorted src/a.rs\n");
		assert_eq!(FormatReport::default().summary(), "");
	}

	#[test]
	fn summarizes_renames() {
		let report = RenameReport {
			new_name: "Disc".to_owned(),
			renamed: vec!["demo::Circle".to_owned(), "demo::Circle".to_owned()],
			references: vec![
				occurrence("src/shapes.rs", 4, ReferenceKind::Definition),
				occurrence("src/lib.rs", 6, ReferenceKind::Import),
				occurrence("src/main.rs", 2, ReferenceKind::Path),
			],
			uncertain: vec![occurrence("src/main.rs", 9, ReferenceKind::MethodCall)],
			files: vec!["src/shapes.rs".to_owned(), "src/lib.rs".to_owned(), "src/main.rs".to_owned()],
			..RenameReport::default()
		};

		assert_eq!(report.summary(), "renamed 2 items, updated 2 references in 3 files\n");
		assert_eq!(
			report.messages(),
			[(Level::Note, "possible reference not updated: src/main.rs:9:5".to_owned())]
		);

		let json = serde_json::to_value(&report).unwrap();

		assert_eq!(
			json["references"][1],
			serde_json::json!({"file": "src/lib.rs", "line": 6, "column": 5, "kind": "import"})
		);
		assert_eq!(json["dry_run"], false);
		assert!(json.get("diff").is_none());
	}
}
