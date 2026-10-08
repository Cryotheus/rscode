//! Reference search and renames on the fixture crates `tests/fixtures/rename_*`.
//!
//! Renames are planned on the fixtures themselves (and previewed), and applied to temporary copies, which are then
//! checked with `cargo check` to prove that they still compile.

use rscode::CrateSpec;
use rscode::Edition;
use rscode::Error;
use rscode::ItemPath;
use rscode::Resolver;
use rscode::Workspace;
use rscode::edit::Rename;
use rscode::edit::RenameOptions;
use rscode::model::Dependency;
use rscode::model::TargetKind;
use rscode::query::FindReferencesOptions;
use rscode::query::find_references;
use rscode::resolve::Reference;
use rscode::resolve::ReferenceKind;
use rscode::resolve::ReferenceOptions;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

fn fixture(name: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn dependency(name: &str) -> Dependency {
	Dependency {
		name: name.into(),
		crate_name: name.into(),
		package: None,
		krate: None,
	}
}

fn spec(name: &str, root: PathBuf, kind: TargetKind, dependencies: &[&str]) -> CrateSpec {
	let mut spec = CrateSpec::new(name, root);

	spec.edition = Edition::E2021;
	spec.kind = kind;
	spec.dependencies = dependencies.iter().map(|name| dependency(name)).collect();
	spec
}

fn load(root: &Path, specs: Vec<CrateSpec>) -> Workspace {
	let mut ws = Workspace::new(root);

	for spec in specs {
		ws.load_crate(spec);
	}

	ws.link();

	for krate in ws.crates() {
		assert!(krate.diagnostics().is_empty(), "{:?}", krate.diagnostics());
	}

	ws
}

/// `rename_items`: a library, and a binary using it.
fn load_items(root: &Path) -> Workspace {
	let lib = spec("rename_items", root.join("src/lib.rs"), TargetKind::Lib, &[]);
	let bin = spec("rename_items", root.join("src/main.rs"), TargetKind::Bin, &["rename_items"]);

	load(root, vec![lib, bin])
}

/// `rename_more`: a library, and a binary using it (which loads one of the library's module files as its own module).
fn load_more(root: &Path) -> Workspace {
	let lib = spec("rename_more", root.join("src/lib.rs"), TargetKind::Lib, &[]);
	let bin = spec("rename_more", root.join("src/main.rs"), TargetKind::Bin, &["rename_more"]);

	load(root, vec![lib, bin])
}

/// `rename_more` with the binary `src/bin/extra.rs`.
fn load_more_with_extra(root: &Path) -> Workspace {
	let lib = spec("rename_more", root.join("src/lib.rs"), TargetKind::Lib, &[]);
	let bin = spec("rename_more", root.join("src/main.rs"), TargetKind::Bin, &["rename_more"]);
	let extra = spec("extra", root.join("src/bin/extra.rs"), TargetKind::Bin, &["rename_more"]);

	load(root, vec![lib, bin, extra])
}

/// `rename_cfg`: a library.
fn load_cfg(root: &Path) -> Workspace {
	load(root, vec![spec("rename_cfg", root.join("src/lib.rs"), TargetKind::Lib, &[])])
}

/// `rename_macros`: a library.
fn load_macros(root: &Path) -> Workspace {
	load(root, vec![spec("rename_macros", root.join("src/lib.rs"), TargetKind::Lib, &[])])
}

fn load_modules(root: &Path) -> Workspace {
	load(root, vec![spec("rename_modules", root.join("src/lib.rs"), TargetKind::Lib, &[])])
}

/// `rename_ws`: the library `dep`, and the binary `app` depending on it.
fn load_ws(root: &Path) -> Workspace {
	let dep = spec("dep", root.join("dep/src/lib.rs"), TargetKind::Lib, &[]);
	let app = spec("app", root.join("app/src/main.rs"), TargetKind::Bin, &["dep"]);

	load(root, vec![dep, app])
}

fn path(text: &str) -> ItemPath {
	ItemPath::parse(text).unwrap_or_else(|error| panic!("{error}"))
}

/// `text` with `/` replaced by the platform's path separator, which the paths in messages have.
fn native(text: &str) -> String {
	text.replace('/', std::path::MAIN_SEPARATOR_STR)
}

/// References as `file:line:column Kind` (with `/` separators), with `?` for uncertain ones.
fn summary(ws: &Workspace, references: &[Reference]) -> Vec<String> {
	(references.iter())
		.map(|reference| {
			let file = ws.display_path(&reference.path).display().to_string().replace(std::path::MAIN_SEPARATOR, "/");
			let certain = if reference.certain { "" } else { "?" };

			format!("{file}:{}:{} {:?}{certain}", reference.start.line, reference.start.column, reference.kind)
		})
		.collect()
}

fn references(ws: &Workspace, item_path: &str, options: &ReferenceOptions) -> Vec<String> {
	let resolver = Resolver::new(ws);
	let targets = resolver.resolve_item_path(&path(item_path));

	assert!(!targets.is_empty(), "{item_path} names nothing");

	let found = resolver.find_references(&targets, options);

	assert!(found.notes.is_empty(), "{:?}", found.notes);
	summary(ws, &found.references)
}

/// [`rscode::query::find_references`] as `file:line:column[?] in item: line` rows (the item relative to the file's
/// module), with `?` for uncertain references.
fn found(ws: &Workspace, item_paths: &[&str], options: &FindReferencesOptions) -> Vec<String> {
	let paths: Vec<ItemPath> = item_paths.iter().map(|text| path(text)).collect();
	let report = find_references(&Resolver::new(ws), &paths, options).unwrap_or_else(|error| panic!("{error}"));

	assert!(report.notes.is_empty(), "{:?}", report.notes);

	(report.references.iter())
		.map(|found| {
			let reference = &found.reference;
			let file = ws.display_path(&reference.path).display().to_string().replace(std::path::MAIN_SEPARATOR, "/");
			let certain = if reference.certain { "" } else { "?" };
			let item = found.local_item.as_deref().map(|item| format!(" in {item}")).unwrap_or_default();

			format!("{file}:{}:{}{certain}{item}: {}", reference.start.line, reference.start.column, found.line)
		})
		.collect()
}

fn plan(ws: &Workspace, item_path: &str, new_name: &str, options: &RenameOptions) -> Result<Rename, Error> {
	rscode::edit::rename(&Resolver::new(ws), &path(item_path), new_name, options)
}

fn plan_ok(ws: &Workspace, item_path: &str, new_name: &str, options: &RenameOptions) -> Rename {
	plan(ws, item_path, new_name, options).unwrap_or_else(|error| panic!("renaming {item_path} to {new_name}: {error}"))
}

/// The new text of a file (relative to the workspace root) after a planned rename.
fn new_text(ws: &Workspace, rename: &Rename, file: &str) -> String {
	let changes = rename.edits.preview().unwrap();
	let file = ws.root().join(file);

	match changes.into_iter().find(|change| change.path == file) {
		Some(change) => change.formatted,
		None => std::fs::read_to_string(file).unwrap(),
	}
}

fn doc_links() -> RenameOptions {
	RenameOptions {
		references: ReferenceOptions {
			doc_links: true,
			..ReferenceOptions::default()
		},
		..RenameOptions::default()
	}
}

fn method_calls() -> RenameOptions {
	RenameOptions {
		references: ReferenceOptions {
			method_calls: true,
			..ReferenceOptions::default()
		},
		..RenameOptions::default()
	}
}

fn forced() -> RenameOptions {
	RenameOptions {
		force: true,
		..RenameOptions::default()
	}
}

/// The collision message of a refused rename.
#[track_caller]
fn collision(result: Result<Rename, Error>) -> String {
	match result {
		Err(error @ Error::Collision { .. }) => error.to_string(),
		Err(error) => panic!("expected a collision, got {error}"),
		Ok(rename) => panic!("expected a collision, got {:?}", rename.renamed),
	}
}

/// A copy of a fixture in cargo's directory for temporary test data, removed when dropped.
struct TempCopy(PathBuf);

impl TempCopy {
	fn new(name: &str, test: &str) -> Self {
		let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rename-tests").join(test);

		let _ = std::fs::remove_dir_all(&root);
		copy_dir(&fixture(name), &root);
		Self(root)
	}

	fn path(&self) -> &Path {
		&self.0
	}

	fn read(&self, file: &str) -> String {
		std::fs::read_to_string(self.0.join(file)).unwrap_or_else(|error| panic!("{file}: {error}"))
	}

	fn exists(&self, file: &str) -> bool {
		self.0.join(file).exists()
	}

	/// The names of the entries of a directory, as the file system has them, sorted.
	fn names(&self, directory: &str) -> Vec<String> {
		let mut names: Vec<String> = (std::fs::read_dir(self.0.join(directory)).unwrap())
			.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
			.collect();

		names.sort();
		names
	}
}

impl Drop for TempCopy {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

fn copy_dir(from: &Path, to: &Path) {
	std::fs::create_dir_all(to).unwrap();

	for entry in std::fs::read_dir(from).unwrap() {
		let entry = entry.unwrap();
		let target = to.join(entry.file_name());

		if entry.file_type().unwrap().is_dir() {
			copy_dir(&entry.path(), &target);
		} else {
			std::fs::copy(entry.path(), target).unwrap();
		}
	}
}

/// Proves that a crate (or workspace) still compiles, building in `$RSCODE_TEST_CHECK_TARGET_DIR` (or in a directory for
/// temporary test data).
#[track_caller]
fn cargo_check(dir: &Path) {
	cargo_check_with(dir, &[]);
}

/// [`cargo_check`] with more arguments for cargo (such as features).
#[track_caller]
fn cargo_check_with(dir: &Path, arguments: &[&str]) {
	let target_dir = std::env::var_os("RSCODE_TEST_CHECK_TARGET_DIR")
		.map(PathBuf::from)
		.unwrap_or_else(|| Path::new(env!("CARGO_TARGET_TMPDIR")).join("rename-check"));

	let output = Command::new(env!("CARGO"))
		.args(["check", "--quiet", "--all-targets", "--offline"])
		.args(arguments)
		.current_dir(dir)
		.env("CARGO_TARGET_DIR", target_dir)
		.output()
		.unwrap();

	assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

/// Loads a copy, plans a rename, and applies it.
#[track_caller]
fn apply(copy: &TempCopy, load: fn(&Path) -> Workspace, item_path: &str, new_name: &str, options: &RenameOptions) -> Rename {
	let ws = load(copy.path());
	let rename = plan_ok(&ws, item_path, new_name, options);

	rename.edits.apply().unwrap_or_else(|error| panic!("applying the rename of {item_path} to {new_name}: {error}"));
	rename
}

#[test]
fn finds_references_across_modules_and_crates() {
	let ws = load_items(&fixture("rename_items"));

	assert_eq!(
		references(&ws, "crate::shapes::Circle", &ReferenceOptions::all()),
		[
			"src/docs.rs:1:38 DocLink",
			"src/docs.rs:3:20 Import",
			"src/docs.rs:5:24 DocLink",
			"src/docs.rs:5:56 DocLink",
			"src/docs.rs:5:74 DocLink",
			"src/docs.rs:12:31 DocLink",
			"src/docs.rs:13:18 Path",
			"src/docs.rs:14:2 Path",
			"src/generics.rs:1:20 Import",
			"src/generics.rs:7:28 Path",
			"src/generics.rs:7:39 Path",
			"src/lib.rs:12:17 Import",
			"src/lib.rs:13:17 Import",
			"src/lib.rs:16:26 DocLink",
			"src/lib.rs:17:25 Path",
			"src/lib.rs:18:2 Path",
			"src/main.rs:2:19 Import",
			"src/main.rs:5:15 Path",
			"src/main.rs:9:20 Path",
			"src/shapes.rs:7:12 Definition",
			"src/shapes.rs:11:6 Path",
			"src/shapes.rs:22:27 Path",
			"src/shapes.rs:23:3 Path",
			"src/shapes.rs:27:23 Path",
			"src/traits.rs:17:30 Path",
			"src/util.rs:1:20 Import",
			"src/util.rs:23:18 Path",
			"src/util.rs:24:2 Path",
			"src/util.rs:36:21 Import",
		]
	);

	// doc links only on request
	let without_docs = references(&ws, "crate::shapes::Circle", &ReferenceOptions::default());

	assert_eq!(without_docs.len(), 23);
	assert!(without_docs.iter().all(|reference| !reference.ends_with("DocLink")), "{without_docs:?}");
}

#[test]
fn local_bindings_shadow_items() {
	let ws = load_items(&fixture("rename_items"));

	// local variables (`let`, closure parameters), a local function, and a nested function that sees the module's
	assert_eq!(
		references(&ws, "crate::util::helper", &ReferenceOptions::all()),
		[
			"src/main.rs:8:49 Path",
			"src/main.rs:8:76 Path",
			"src/util.rs:7:8 Definition",
			"src/util.rs:14:24 Path",
			"src/util.rs:20:8 Path",
			"src/util.rs:24:19 Path",
			"src/util.rs:43:3 Path",
			"src/util.rs:59:27 Path",
			"src/util.rs:63:7 Path",
		]
	);

	// generic parameters named like the type
	let circle = references(&ws, "crate::shapes::Circle", &ReferenceOptions::default());

	assert!(!circle.iter().any(|reference| reference.starts_with("src/generics.rs:3:") || reference.starts_with("src/generics.rs:13:")));
}

#[test]
fn associated_items_and_variants() {
	let ws = load_items(&fixture("rename_items"));

	// through `Type::`, an alias (`Round`), and a local import with an alias (`C`)
	assert_eq!(
		references(&ws, "crate::shapes::Circle::new", &ReferenceOptions::all()),
		[
			"src/docs.rs:5:64 DocLink",
			"src/docs.rs:14:10 Path",
			"src/lib.rs:18:10 Path",
			"src/main.rs:5:23 Path",
			"src/main.rs:9:28 Path",
			"src/main.rs:11:38 Path",
			"src/shapes.rs:13:9 Definition",
			"src/shapes.rs:23:11 Path",
			"src/util.rs:38:5 Path",
		]
	);

	// `Self::Round`, `Kind::Round`, and the pattern `Round` through a glob import in a function body
	assert_eq!(
		references(&ws, "crate::shapes::Kind::Round", &ReferenceOptions::all()),
		["src/main.rs:6:19 Path", "src/shapes.rs:34:2 Definition", "src/shapes.rs:41:24 Path", "src/shapes.rs:48:4 Path", "src/shapes.rs:57:9 Path"]
	);
}

#[test]
fn patterns_name_constants_and_unit_structs() {
	let ws = load_items(&fixture("rename_items"));

	// match arms, a guard of `matches!`, and an inline argument of a format string
	assert_eq!(
		references(&ws, "crate::util::LIMIT", &ReferenceOptions::default()),
		["src/util.rs:3:11 Definition", "src/util.rs:53:3 Path", "src/util.rs:54:20 Path", "src/util.rs:59:12 Path", "src/util.rs:67:42 Path"]
	);

	assert_eq!(
		references(&ws, "crate::patterns::ZERO", &ReferenceOptions::default()),
		["src/patterns.rs:4:11 Definition", "src/patterns.rs:10:3 Path", "src/patterns.rs:11:20 Path", "src/patterns.rs:20:4 Path"]
	);

	// `let Marker = marker;` matches the unit struct
	assert_eq!(
		references(&ws, "crate::patterns::Marker", &ReferenceOptions::default()),
		["src/patterns.rs:2:12 Definition", "src/patterns.rs:6:37 Path", "src/patterns.rs:7:6 Path"]
	);
}

#[test]
fn trait_items_and_uncertain_references() {
	let ws = load_items(&fixture("rename_items"));

	// `Area::area`, `<Square as Area>::area`, an `impl` in a function body, and `T::area` through the bounds of `T` (in
	// its declaration, or in `where` clauses, also of nested items); method calls, and `T::area` through a supertrait,
	// are uncertain
	let certain = [
		"src/traits.rs:2:5 Definition",
		"src/traits.rs:28:8 Path",
		"src/traits.rs:28:41 Path",
		"src/traits.rs:32:5 Path",
		"src/traits.rs:39:6 Definition",
		"src/traits.rs:51:5 Path",
		"src/traits.rs:58:6 Path",
		"src/traits.rs:67:6 Path",
	];
	let mut all = certain.to_vec();

	all.extend([
		"src/traits.rs:5:27 MethodCall?",
		"src/traits.rs:24:34 MethodCall?",
		"src/traits.rs:44:8 MethodCall?",
		"src/traits.rs:74:5 Path?",
	]);
	all.sort_by_key(|reference| {
		let mut parts = reference.split([':', ' ']).skip(1).map(|number| number.parse::<usize>().unwrap_or_default());

		(parts.next(), parts.next())
	});

	assert_eq!(references(&ws, "crate::traits::Area::area", &ReferenceOptions::all()), all);
	assert_eq!(references(&ws, "crate::traits::Area::area", &ReferenceOptions::default()), certain);
}

#[test]
fn macros() {
	let ws = load_items(&fixture("rename_items"));

	// a recursive invocation in the macro's own transcriber is certain; so is `$crate::macros::times`
	assert_eq!(
		references(&ws, "crate::triple", &ReferenceOptions::default()),
		[
			"src/macros.rs:8:14 Definition",
			"src/macros.rs:13:3 MacroToken",
			"src/macros.rs:13:11 MacroToken",
			"src/macros.rs:22:15 Path",
			"src/macros.rs:22:35 Path",
			"src/macros.rs:22:48 Path",
		]
	);

	assert_eq!(
		references(&ws, "crate::macros::times", &ReferenceOptions::default()),
		["src/macros.rs:10:19 MacroToken", "src/macros.rs:17:8 Definition"]
	);
	assert_eq!(references(&ws, "crate::macros::double", &ReferenceOptions::default()), ["src/macros.rs:1:14 Definition", "src/macros.rs:22:2 Path"]);
}

#[test]
fn macro_tokens_by_their_role() {
	let ws = load_macros(&fixture("rename_macros"));

	// paths (`hints::add`, `name!`) in transcribers resolve where the macro is defined; not after `.` or `fn`, nor in
	// paths starting elsewhere (`std::backtrace::Backtrace::capture`), whatever the options
	for options in [ReferenceOptions::default(), ReferenceOptions::all()] {
		assert_eq!(
			references(&ws, "crate::hints", &options),
			["src/lib.rs:3:9 Definition", "src/lib.rs:35:5 MacroToken", "src/lib.rs:39:27 MacroToken"]
		);
		assert_eq!(references(&ws, "crate::capture", &options), ["src/lib.rs:9:14 Definition", "src/lib.rs:17:19 MacroToken"]);
	}

	// a method called in a transcriber is uncertain
	assert_eq!(
		references(&ws, "crate::Counter::size", &ReferenceOptions::all()),
		["src/lib.rs:26:9 Definition", "src/lib.rs:35:21 MethodCall?"]
	);

	let copy = TempCopy::new("rename_macros", "macro-roles");

	apply(&copy, load_macros, "crate::hints", "sizes", &RenameOptions::default());
	apply(&copy, load_macros, "crate::capture", "grab", &RenameOptions::default());

	let lib = copy.read("src/lib.rs");

	assert!(lib.contains("\t\t\t\tsizes::add(self.size(), 1)\n"), "{lib}");
	assert!(lib.contains("pub fn hints(&self)") && lib.contains("self.hints() + crate::sizes::add(0, $name::ONE)"), "{lib}");
	assert!(lib.contains("Some(std::backtrace::Backtrace::capture())"), "{lib}");
	assert!(lib.contains("if $condition { grab!() } else { None }"), "{lib}");
	cargo_check(copy.path());
}

#[test]
fn names_bound_differently_under_other_cfgs_are_renamed_along() {
	let ws = load_cfg(&fixture("rename_cfg"));

	// `crate::chain` imports `Chain` under one `cfg` and defines it under the other: paths there name both
	let rename = plan_ok(&ws, "crate::Chain", "ErrorChain", &RenameOptions::default());

	assert_eq!(rename.renamed, ["rename_cfg::Chain", "rename_cfg::chain::Chain"]);
	assert_eq!(
		rename.warnings,
		["also renaming `rename_cfg::chain::Chain`: `rename_cfg::chain` binds `Chain` to it under other `cfg`s, instead of importing `rename_cfg::Chain`"]
	);

	// the same with imports of two items, and a note for an item imported under another name
	let rename = plan_ok(&ws, "crate::unix_impl::Handle", "Fd", &RenameOptions::default());

	assert_eq!(rename.renamed, ["rename_cfg::unix_impl::Handle", "rename_cfg::windows_impl::Handle"]);
	assert!(
		rename.warnings.contains(
			&"`rename_cfg::aliased` binds `Handle` to `rename_cfg::windows_impl::Socket` under other `cfg`s, which is not renamed: \
			  the renamed paths there do not compile with those `cfg`s"
				.to_owned()
		),
		"{:?}",
		rename.warnings
	);

	let copy = TempCopy::new("rename_cfg", "cfg-variants");

	apply(&copy, load_cfg, "crate::Chain", "ErrorChain", &RenameOptions::default());
	apply(&copy, load_cfg, "crate::unix_impl::Handle", "Fd", &RenameOptions::default());

	let lib = copy.read("src/lib.rs");

	assert!(lib.contains("\tpub(crate) struct ErrorChain(pub u8);\n\n\timpl ErrorChain {"), "{lib}");
	assert!(lib.contains("pub use crate::windows_impl::Fd;\n\n\tpub fn handle() -> Fd {\n\t\tFd\n"), "{lib}");
	cargo_check(copy.path());
	cargo_check_with(copy.path(), &["--no-default-features"]);
}

#[test]
fn cfg_variants_and_modules() {
	let ws = load_items(&fixture("rename_items"));

	assert_eq!(
		references(&ws, "crate::platform", &ReferenceOptions::default()),
		["src/lib.rs:22:8 Definition", "src/lib.rs:27:8 Definition", "src/main.rs:10:67 Path"]
	);

	assert_eq!(
		references(&ws, "crate::shapes", &ReferenceOptions::all()),
		[
			"src/docs.rs:1:30 DocLink",
			"src/docs.rs:3:12 Import",
			"src/generics.rs:1:12 Import",
			"src/lib.rs:8:9 Definition",
			"src/lib.rs:12:9 Import",
			"src/lib.rs:13:9 Import",
			"src/main.rs:1:19 Import",
			"src/traits.rs:17:22 Path",
			"src/util.rs:1:12 Import",
			"src/util.rs:36:13 Import",
		]
	);
}

#[test]
fn unparsable_files_are_noted() {
	let copy = TempCopy::new("rename_items", "unparsable");

	// only files mentioning a target can contain references
	std::fs::write(copy.path().join("src/keywords.rs"), "pub fn kind( -> Circle {}\n").unwrap();
	std::fs::write(copy.path().join("src/patterns.rs"), "pub fn broken( -> u8 {}\n").unwrap();

	let lib = spec("rename_items", copy.path().join("src/lib.rs"), TargetKind::Lib, &[]);
	let mut ws = Workspace::new(copy.path());

	ws.load_crate(lib);

	let resolver = Resolver::new(&ws);
	let targets = resolver.resolve_item_path(&path("crate::shapes::Circle"));
	let found = resolver.find_references(&targets, &ReferenceOptions::all());

	assert_eq!(found.notes.len(), 1, "{:?}", found.notes);
	assert!(
		found.notes[0].starts_with(&native("src/keywords.rs:1:12: references in this file were not searched")),
		"{:?}",
		found.notes
	);
	assert_eq!(found.references.len(), 26);
}

#[test]
fn found_references_name_their_items_and_lines() {
	let ws = load_items(&fixture("rename_items"));
	let default = FindReferencesOptions::default();

	// code in bodies is in its function (in a nested function too, which is not loaded); through a glob re-export too
	let line = "println!(\"{circle} {} {}\", rename_items::util::helper(1.0), rename_items::helper(2.0));";

	assert_eq!(
		found(&ws, &["crate::util::helper"], &default),
		[
			format!("src/main.rs:8:49 in main: {line}"),
			format!("src/main.rs:8:76 in main: {line}"),
			"src/util.rs:14:24 in shadowing: helper + crate::util::helper(1.0)".to_owned(),
			"src/util.rs:20:8 in closure: apply(helper(2.0))".to_owned(),
			"src/util.rs:24:19 in make: Circle { radius: helper(1.0) }".to_owned(),
			"src/util.rs:43:3 in outer: helper(2.0)".to_owned(),
			"src/util.rs:59:27 in show: format!(\"{LIMIT} {} {}\", helper(1.0), COUNTER)".to_owned(),
			"src/util.rs:63:7 in repeated: vec![helper(1.0); 3]".to_owned(),
		]
	);

	// imports (and the module's own docs) are in no item; doc links are in the documented item; definitions are only
	// found on request
	let options = FindReferencesOptions {
		references: ReferenceOptions {
			doc_links: true,
			..ReferenceOptions::default()
		},
		definitions: true,
	};

	let in_docs: Vec<String> = (found(&ws, &["crate::shapes::Circle"], &options).into_iter())
		.filter(|row| row.starts_with("src/docs.rs"))
		.collect();

	let line = "/// A disc, unlike a [`Circle`] (see [the constructor](Circle::new) and [Circle][]).";

	assert_eq!(
		in_docs,
		[
			"src/docs.rs:1:38: //! Docs mentioning [`crate::shapes::Circle`].".to_owned(),
			"src/docs.rs:3:20: use crate::shapes::Circle;".to_owned(),
			format!("src/docs.rs:5:24 in Disc: {line}"),
			format!("src/docs.rs:5:56 in Disc: {line}"),
			format!("src/docs.rs:5:74 in Disc: {line}"),
			"src/docs.rs:12:31 in make: /// Makes a [`Circle`](struct@Circle).".to_owned(),
			"src/docs.rs:13:18 in make: pub fn make() -> Circle {".to_owned(),
			"src/docs.rs:14:2 in make: Circle::new(2.0)".to_owned(),
		]
	);
	assert_eq!(
		found(&ws, &["crate::util::COUNTER"], &options),
		[
			"src/util.rs:5:12 in COUNTER: pub static COUNTER: u32 = 0;",
			"src/util.rs:54:28 in check: limit => limit > LIMIT + COUNTER,",
			"src/util.rs:59:40 in show: format!(\"{LIMIT} {} {}\", helper(1.0), COUNTER)",
		]
	);

	// a trait item comes with the items implementing it, which `Square::area` names; items of `impl`s of types of
	// other modules are named in full
	let line = "Area::area(square) + <Square as Area>::area(square) + Square::area(square)";

	assert_eq!(
		found(&ws, &["crate::traits::Area::area"], &default),
		[
			format!("src/traits.rs:28:8 in explicit: {line}"),
			format!("src/traits.rs:28:41 in explicit: {line}"),
			format!("src/traits.rs:28:64 in explicit: {line}"),
			"src/traits.rs:32:5 in generic: T::area(shape)".to_owned(),
			"src/traits.rs:51:5 in bounded_by_where: T::area(shape)".to_owned(),
			"src/traits.rs:58:6 in Wrapper::inner: T::area(&self.0)".to_owned(),
			"src/traits.rs:67:6 in Wrapper::bounded_later: T::area(&self.0)".to_owned(),
		]
	);

	// unnamed items are in no item of their own
	assert_eq!(found(&ws, &["crate::unnamed::Marker"], &default)[0], "src/unnamed.rs:6:10: let _ = Marker;");

	// uses through the alias of an import (`pub use shapes::Circle as Round;`) are uses of the item
	let through_alias: Vec<String> = (found(&ws, &["crate::Round"], &default).into_iter())
		.filter(|row| row.contains("Round"))
		.collect();

	assert_eq!(
		through_alias,
		[
			"src/lib.rs:13:17: pub use shapes::Circle as Round;",
			"src/main.rs:11:31 in main: println!(\"{}\", rename_items::Round::new(4.0).radius);",
		]
	);

	let definitions: Vec<String> = (found(&ws, &["crate::traits::Area::area"], &options).into_iter())
		.filter(|row| row.ends_with("fn area(&self) -> f64 {") || row.ends_with("fn area(&self) -> f64;"))
		.collect();

	assert_eq!(
		definitions,
		[
			"src/traits.rs:2:5 in Area::area: fn area(&self) -> f64;",
			"src/traits.rs:12:5 in <Square as Area>::area: fn area(&self) -> f64 {",
			"src/traits.rs:18:5 in <rename_items::shapes::Circle as Area>::area: fn area(&self) -> f64 {",
			"src/traits.rs:39:6 in local_impl: fn area(&self) -> f64 {",
		]
	);
}

#[test]
fn found_references_cover_related_items_and_refuse_unnamed_ones() {
	let ws = load_items(&fixture("rename_items"));
	let resolver = Resolver::new(&ws);
	let search = |text: &str, options: &FindReferencesOptions| find_references(&resolver, &[path(text)], options);

	let report = search("crate::traits::Area::area", &FindReferencesOptions::default()).unwrap();

	assert_eq!(
		report.targets,
		[
			"<rename_items::shapes::Circle as Area>::area",
			"<rename_items::traits::Square as Area>::area",
			"rename_items::traits::Area::area",
		]
	);
	assert_eq!((report.references.len(), report.files(), report.uncertain()), (7, 1, 0));

	// method calls are uncertain
	let options = FindReferencesOptions {
		references: ReferenceOptions::all(),
		definitions: false,
	};
	let report = search("crate::traits::Area::area", &options).unwrap();

	assert_eq!((report.references.len(), report.files(), report.uncertain()), (11, 1, 4));

	match search("crate::nope", &options) {
		Err(Error::NotFound(path)) => assert_eq!(path, "crate::nope"),
		other => panic!("{other:?}"),
	}

	let refused = [
		(
			"impl crate::shapes::Circle",
			"`impl rename_items::shapes::Circle` is an `impl` block, whose references cannot be searched",
		),
		(
			"use crate::util::Circle",
			"`use rename_items::util::Circle` is an import, whose references cannot be searched: search for the \
			 references of what it imports instead (its path without `use`)",
		),
		("crate", "`rename_items` is a crate root, whose references cannot be searched"),
		(
			"crate::shapes::Circle.radius",
			"`rename_items::shapes::Circle.radius` is a field, whose uses (field accesses, struct literals, and \
			 patterns) are not tracked",
		),
		(
			"crate::unnamed::marked!",
			"`rename_items::unnamed::marked!` is a macro invocation, whose references cannot be searched: search for \
			 the uses of the macro `marked!` with the path of the macro itself, without `!`",
		),
	];

	for (text, message) in refused {
		match search(text, &options) {
			Err(Error::Unsupported(found)) => assert_eq!(found, message),
			other => panic!("{other:?}"),
		}
	}
}

#[test]
fn renames_items_everywhere() {
	let ws = load_items(&fixture("rename_items"));
	let rename = plan_ok(&ws, "crate::shapes::Circle", "Disk", &RenameOptions::default());

	assert_eq!(rename.renamed, ["rename_items::shapes::Circle"]);
	assert_eq!(rename.references.len(), 23);
	assert!(rename.collisions.is_empty() && rename.warnings.is_empty(), "{rename:?}");

	// doc links are only reported, unless renamed on request
	assert_eq!(rename.uncertain.len(), 6);
	assert!(rename.uncertain.iter().all(|reference| reference.kind == ReferenceKind::DocLink));

	let shapes = new_text(&ws, &rename, "src/shapes.rs");

	for expected in [
		"pub struct Disk {",
		"impl Disk {",
		"pub fn doubled(&self) -> Disk {",
		"\t\tDisk::new(self.diameter())",
		"impl fmt::Display for Disk {",
	] {
		assert!(shapes.contains(expected), "{expected}: {shapes}");
	}

	assert!(shapes.contains("\t\tSelf { radius }"), "{shapes}");

	let lib = new_text(&ws, &rename, "src/lib.rs");

	assert!(lib.contains("pub use shapes::Disk;\npub use shapes::Disk as Round;\n"), "{lib}");
	assert!(lib.contains("/// The unit circle, a [`Circle`].\npub fn unit_circle() -> Disk {\n\tDisk::new(1.0)"), "{lib}");

	let main = new_text(&ws, &rename, "src/main.rs");

	assert!(main.contains("use rename_items::Disk;") && main.contains("Disk::new(2.0)") && main.contains("rename_items::Round::new(4.0)"), "{main}");

	let generics = new_text(&ws, &rename, "src/generics.rs");

	assert!(generics.contains("pub fn shadowed<Circle: Clone>(value: Circle) -> Circle {"), "{generics}");
	assert!(generics.contains("pub fn not_shadowed(value: Disk) -> Disk {"), "{generics}");
	assert!(generics.contains("impl<Circle> Wrapper<Circle> {"), "{generics}");
	assert!(new_text(&ws, &rename, "src/util.rs").contains("\tuse crate::shapes::Disk as C;\n"));

	// with doc links
	let rename = plan_ok(&ws, "crate::shapes::Circle", "Disk", &doc_links());
	let docs = new_text(&ws, &rename, "src/docs.rs");

	assert_eq!(rename.references.len(), 29);
	assert!(rename.uncertain.is_empty());
	assert!(docs.starts_with("//! Docs mentioning [`crate::shapes::Disk`].\n"), "{docs}");
	assert!(docs.contains("/// A disc, unlike a [`Disk`] (see [the constructor](Disk::new) and [Disk][]).\n"), "{docs}");
	assert!(docs.contains("/// // [Circle] in code is not a link\n"), "{docs}");
	assert!(docs.contains("/// Makes a [`Circle`](struct@Disk).\n"), "{docs}");
}

#[test]
fn trait_items_are_renamed_with_their_implementations() {
	let ws = load_items(&fixture("rename_items"));
	let rename = plan_ok(&ws, "crate::traits::Area::area", "surface", &RenameOptions::default());

	assert_eq!(
		rename.renamed,
		["<rename_items::shapes::Circle as Area>::area", "<rename_items::traits::Square as Area>::area", "rename_items::traits::Area::area"]
	);

	// method calls, and `T::area` through a supertrait, depend on types
	assert_eq!(
		summary(&ws, &rename.uncertain),
		["src/traits.rs:5:27 MethodCall?", "src/traits.rs:24:34 MethodCall?", "src/traits.rs:44:8 MethodCall?", "src/traits.rs:74:5 Path?"]
	);

	let traits = new_text(&ws, &rename, "src/traits.rs");

	assert!(traits.contains("\tArea::surface(square) + <Square as Area>::surface(square) + Square::surface(square)\n"), "{traits}");
	assert_eq!(traits.matches("fn surface(&self) -> f64").count(), 4, "{traits}");
	assert_eq!(traits.matches("T::surface(").count(), 4, "{traits}");
	assert!(traits.contains("self.area()") && traits.contains("S::area(shape)"), "{traits}");

	// the same through an implementation's item, with method calls
	let rename = plan_ok(&ws, "<crate::traits::Square as crate::traits::Area>::area", "surface", &method_calls());

	assert_eq!(rename.renamed.len(), 3, "{:?}", rename.renamed);
	assert!(rename.uncertain.is_empty(), "{:?}", rename.uncertain);

	let traits = new_text(&ws, &rename, "src/traits.rs");

	assert!(traits.contains("self.surface()") && traits.contains("shape.surface()") && traits.contains("S::surface(shape)"), "{traits}");
	assert!(!traits.contains("area("), "{traits}");
}

#[test]
fn rename_errors() {
	let ws = load_items(&fixture("rename_items"));

	for invalid in ["1x", "self", "Self", "_", "a-b", ""] {
		assert!(matches!(plan(&ws, "crate::shapes::Circle", invalid, &RenameOptions::default()), Err(Error::InvalidIdent(_))), "{invalid}");
	}

	assert!(matches!(plan(&ws, "crate::shapes::Nothing", "Disk", &RenameOptions::default()), Err(Error::NotFound(_))));

	for unsupported in ["<crate::shapes::Circle>", "crate"] {
		assert!(matches!(plan(&ws, unsupported, "Disk", &RenameOptions::default()), Err(Error::Unsupported(_))), "{unsupported}");
	}

	let unchanged = plan_ok(&ws, "crate::shapes::Circle", "Circle", &RenameOptions::default());

	assert!(unchanged.edits.is_empty() && unchanged.references.is_empty());
	assert_eq!(unchanged.warnings.len(), 1);
}

#[test]
fn collisions_are_refused_unless_forced() {
	let ws = load_items(&fixture("rename_items"));
	let refused = |item_path: &str, new_name: &str| collision(plan(&ws, item_path, new_name, &RenameOptions::default()));

	// an item of the same module, and one of a module importing the item by name
	let kind = native("`rename_items::shapes::Kind` in `rename_items::shapes` (src/shapes.rs:33:10)");
	let disc = native("`rename_items::docs::Disc` in `rename_items::docs` (src/docs.rs:10:12)");

	assert!(refused("crate::shapes::Circle", "Kind").contains(&kind));
	assert!(refused("crate::shapes::Circle", "Disc").contains(&disc));

	// an item of a module glob-importing the item's module
	assert!(refused("crate::util::helper", "unit_circle").contains("`rename_items::unit_circle` in `rename_items`"));

	// associated items, variants, and trait items
	assert!(refused("crate::shapes::Circle::new", "diameter").contains("`rename_items::shapes::Circle::diameter`"));
	assert!(refused("crate::shapes::Kind::Round", "Square").contains("`rename_items::shapes::Kind::Square`"));
	assert!(refused("crate::traits::Area::area", "describe").contains("`rename_items::traits::Area::describe`"));

	// modules, and their files
	let message = refused("crate::shapes", "util");

	assert!(message.contains("`rename_items::util` in `rename_items`"), "{message}");
	assert!(message.contains(&native("`src/util.rs` in `rename_items::shapes`")), "{message}");

	// forced: the binary imports both `Circle` and `Kind` by name
	let rename = plan_ok(&ws, "crate::shapes::Circle", "Kind", &forced());
	let collisions: Vec<(&str, &str, String)> = (rename.collisions.iter())
		.map(|collision| {
			// (with `/` separators)
			let file = ws.display_path(&collision.file).display().to_string().replace(std::path::MAIN_SEPARATOR, "/");
			let location = format!("{file}:{}", collision.start);

			(collision.scope.as_str(), collision.existing.as_str(), location)
		})
		.collect();

	assert_eq!(
		collisions,
		[
			("rename_items::shapes", "rename_items::shapes::Kind", "src/shapes.rs:33:10".to_owned()),
			("rename_items", "use rename_items::shapes::Kind", "src/main.rs:1:27".to_owned()),
		]
	);

	assert!(!rename.edits.is_empty());
}

#[test]
fn shadowed_prelude_names_are_warned_about() {
	let ws = load_items(&fixture("rename_items"));

	// `helper` is used by name in `util` (where it is defined), and in the crate root (through a glob import)
	let rename = plan_ok(&ws, "crate::util::helper", "drop", &RenameOptions::default());

	assert_eq!(
		rename.warnings,
		["`drop` is a name of the standard library's prelude, which the renamed `rename_items::util::helper` shadows in \
		  `rename_items::util`, `rename_items`"]
	);

	let rename = plan_ok(&ws, "crate::shapes::Circle", "u8", &RenameOptions::default());

	assert_eq!(rename.warnings.len(), 1, "{:?}", rename.warnings);
	assert!(rename.warnings[0].starts_with("`u8` is a built-in name, which the renamed `rename_items::shapes::Circle` shadows in"));
	assert!(plan_ok(&ws, "crate::shapes::Circle", "Disk", &RenameOptions::default()).warnings.is_empty());
}

#[test]
fn keyword_names_are_raw_identifiers() {
	let ws = load_items(&fixture("rename_items"));
	let rename = plan_ok(&ws, "crate::keywords::kind", "type", &RenameOptions::default());

	assert_eq!(
		new_text(&ws, &rename, "src/keywords.rs"),
		"pub fn r#type() -> u8 {\n\t1\n}\n\npub fn use_kind() -> u8 {\n\tr#type() + self::r#type()\n}\n"
	);

	let rename = plan_ok(&ws, "crate::keywords::kind", "r#kinds", &RenameOptions::default());

	assert!(new_text(&ws, &rename, "src/keywords.rs").contains("kinds() + self::kinds()"));
}

#[test]
fn applied_renames_compile() {
	let copy = TempCopy::new("rename_items", "applied");
	let all = RenameOptions {
		references: ReferenceOptions::all(),
		..RenameOptions::default()
	};

	apply(&copy, load_items, "crate::shapes::Circle", "Disk", &doc_links());
	apply(&copy, load_items, "crate::util::helper", "assist", &RenameOptions::default());
	apply(&copy, load_items, "crate::shapes::Disk::new", "create", &RenameOptions::default());
	apply(&copy, load_items, "crate::traits::Area::area", "surface", &method_calls());
	apply(&copy, load_items, "crate::shapes::Kind::Round", "Circular", &RenameOptions::default());
	apply(&copy, load_items, "crate::util::LIMIT", "MAXIMUM", &RenameOptions::default());
	apply(&copy, load_items, "crate::triple", "thrice", &RenameOptions::default());
	apply(&copy, load_items, "crate::macros::times", "multiply", &RenameOptions::default());
	apply(&copy, load_items, "crate::keywords::kind", "type", &RenameOptions::default());
	apply(&copy, load_items, "crate::platform", "system", &RenameOptions::default());
	apply(&copy, load_items, "crate::patterns::Marker", "Flag", &RenameOptions::default());
	apply(&copy, load_items, "crate::patterns::ZERO", "NOTHING", &RenameOptions::default());
	apply(&copy, load_items, "crate::util::COUNTER", "TALLY", &all);

	let rename = apply(&copy, load_items, "crate::shapes", "figures", &all);

	assert_eq!(rename.edits.moves(), [(copy.path().join("src/shapes.rs"), copy.path().join("src/figures.rs"))]);
	assert!(copy.exists("src/figures.rs") && !copy.exists("src/shapes.rs"));

	let util = copy.read("src/util.rs");

	assert!(util.contains("format!(\"{MAXIMUM} {} {}\", assist(1.0), TALLY)"), "{util}");
	assert!(util.contains("\tlet helper = 3.0;\n\n\thelper + crate::util::assist(1.0)"), "{util}");
	assert!(util.contains("\tfn helper(value: f64) -> f64 {"), "{util}");
	assert!(util.contains("\tC::create(1.0).radius"), "{util}");
	assert!(copy.read("src/macros.rs").contains("\t\tthrice!(thrice!($value))"));
	assert!(copy.read("src/main.rs").contains("let kind = Kind::Circular;"));
	cargo_check(copy.path());
}

#[test]
fn modules_are_renamed_with_their_files() {
	let copy = TempCopy::new("rename_modules", "modules");
	let ws = load_modules(copy.path());
	let message = collision(plan(&ws, "crate::plain", "occupied", &RenameOptions::default()));

	assert!(message.contains(&native("`src/occupied.rs` in `rename_modules::plain`")), "{message}");

	// (with `/` separators)
	let moves = |rename: &Rename| -> Vec<(String, String)> {
		let relative = |path: &Path| {
			path.strip_prefix(copy.path()).unwrap().display().to_string().replace(std::path::MAIN_SEPARATOR, "/")
		};

		rename.edits.moves().iter().map(|(from, to)| (relative(from), relative(to))).collect()
	};

	let rename = apply(&copy, load_modules, "crate::plain", "simple", &RenameOptions::default());

	assert_eq!(moves(&rename), [("src/plain.rs".to_owned(), "src/simple.rs".to_owned())]);

	let rename = apply(&copy, load_modules, "crate::nested", "layered", &RenameOptions::default());

	assert_eq!(
		moves(&rename),
		[("src/nested".to_owned(), "src/layered".to_owned()), ("src/nested.rs".to_owned(), "src/layered.rs".to_owned())]
	);

	assert!(copy.read("src/layered/child.rs").contains("pub(in crate::layered) fn restricted()"));

	let rename = apply(&copy, load_modules, "crate::dir", "folder", &RenameOptions::default());

	assert_eq!(moves(&rename), [("src/dir".to_owned(), "src/folder".to_owned())]);

	let rename = apply(&copy, load_modules, "crate::inline", "embedded", &RenameOptions::default());

	assert_eq!(moves(&rename), [("src/inline".to_owned(), "src/embedded".to_owned())]);

	// a module loaded with `#[path]` keeps its file
	let rename = apply(&copy, load_modules, "crate::custom", "special", &RenameOptions::default());

	assert!(moves(&rename).is_empty());
	assert_eq!(rename.warnings.len(), 1, "{:?}", rename.warnings);

	let lib = copy.read("src/lib.rs");

	for expected in [
		"pub mod folder;\npub mod layered;\npub mod simple;\n",
		"pub mod embedded {",
		"pub mod special;",
		"pub use simple::value as plain_value;",
	] {
		assert!(lib.contains(expected), "{expected}: {lib}");
	}

	for file in [
		"src/simple.rs",
		"src/layered.rs",
		"src/layered/child.rs",
		"src/folder/mod.rs",
		"src/folder/leaf.rs",
		"src/embedded/deep.rs",
		"src/custom_file.rs",
	] {
		assert!(copy.exists(file), "{file}");
	}

	cargo_check(copy.path());
}

#[test]
fn modules_are_renamed_to_names_in_another_case() {
	let copy = TempCopy::new("rename_modules", "case");
	let modules = [("plain", "Plain"), ("nested", "Nested"), ("dir", "Dir"), ("inline", "Inline")];

	// on file systems that ignore case (as on Windows and macOS), the new file names name the old files already
	for (old, new) in modules {
		apply(&copy, load_modules, &format!("crate::{old}"), new, &RenameOptions::default());
	}

	let upper = ["Dir", "Inline", "Nested", "Nested.rs", "Plain.rs", "custom_file.rs", "lib.rs", "occupied.rs"];

	assert_eq!(copy.names("src"), upper);
	assert_eq!(copy.names("src/Nested"), ["child.rs"]);
	assert!(copy.read("src/Nested/child.rs").contains("pub(in crate::Nested) fn restricted()"));

	cargo_check(copy.path());

	for (old, new) in modules {
		apply(&copy, load_modules, &format!("crate::{new}"), old, &RenameOptions::default());
	}

	let lower = ["custom_file.rs", "dir", "inline", "lib.rs", "nested", "nested.rs", "occupied.rs", "plain.rs"];

	assert_eq!(copy.names("src"), lower);
	assert_eq!(copy.read("src/lib.rs"), std::fs::read_to_string(fixture("rename_modules/src/lib.rs")).unwrap());
}

#[test]
fn renames_across_crates() {
	let copy = TempCopy::new("rename_ws", "crates");
	let ws = load_ws(copy.path());

	assert_eq!(
		references(&ws, "::dep::Widget", &ReferenceOptions::all()),
		[
			"app/src/main.rs:1:10 Import",
			"app/src/main.rs:4:22 Path",
			"app/src/main.rs:5:18 Path",
			"app/src/main.rs:5:27 Path",
			"dep/src/lib.rs:3:21 DocLink",
			"dep/src/lib.rs:4:12 Definition",
			"dep/src/lib.rs:6:6 Path",
			"dep/src/lib.rs:8:3 Path",
			"dep/src/lib.rs:16:18 Path",
		]
	);

	let rename = apply(&copy, load_ws, "::dep::Widget", "Gadget", &doc_links());

	assert_eq!(rename.edits.edited_files().count(), 2);
	apply(&copy, load_ws, "::dep::shout", "yell", &RenameOptions::default());
	apply(&copy, load_ws, "::dep::inner", "nested", &RenameOptions::default());
	apply(&copy, load_ws, "::dep::Gadget::label", "caption", &method_calls());

	let main = copy.read("app/src/main.rs");

	for expected in [
		"use dep::Gadget;",
		"::dep::Gadget::new()",
		"let other: dep::Gadget = Gadget::default();",
		"dep::nested::helper()",
		"dep::yell!(\"hi\")",
		"widget.caption()",
	] {
		assert!(main.contains(expected), "{expected}: {main}");
	}

	assert!(copy.read("dep/src/lib.rs").contains("/// A widget; see [`Gadget::new`].\npub struct Gadget;"));
	cargo_check(copy.path());
}

#[cfg(feature = "cargo")]
#[test]
fn renames_in_loaded_cargo_workspaces() {
	let options = rscode::LoadOptions {
		manifest_path: Some(fixture("rename_ws/Cargo.toml")),
		workspace: true,
		silent: true,
		..rscode::LoadOptions::default()
	};

	let ws = rscode::load_workspace(&options).unwrap();
	let rename = plan_ok(&ws, "::dep::Widget", "Gadget", &RenameOptions::default());

	assert_eq!(rename.renamed, ["dep::Widget"]);
	assert_eq!(rename.edits.edited_files().count(), 2);
	assert!(new_text(&ws, &rename, "app/src/main.rs").contains("let other: dep::Gadget = Gadget::default();"));
}

/// Finds references in `syn` (about 30k lines), if its source is in the cargo registry.
#[test]
fn searches_large_crates_quickly() {
	let home = std::env::home_dir().map(|home| home.join(".cargo"));
	let registry = std::env::var_os("CARGO_HOME").map(PathBuf::from).or(home).map(|home| home.join("registry/src"));

	let Some(root) = (registry.and_then(|registry| std::fs::read_dir(registry).ok()).into_iter().flatten().flatten())
		.map(|index| index.path().join("syn-3.0.6/src/lib.rs"))
		.find(|path| path.exists())
	else {
		eprintln!("syn 3.0.6 is not in the cargo registry; skipped");
		return;
	};

	let mut ws = Workspace::new(root.parent().unwrap());

	ws.load_crate(CrateSpec::new("syn", root));

	let resolver = Resolver::new(&ws);

	// (most of syn's syntax tree types are defined by macros, so they are not loaded)
	for item_path in ["crate::parse::ParseStream", "crate::parse::Parse", "crate::punctuated::Punctuated", "crate::Error", "crate::buffer::Cursor"] {
		let targets = resolver.resolve_item_path(&path(item_path));
		let started = Instant::now();
		let found = resolver.find_references(&targets, &ReferenceOptions::all());
		let elapsed = started.elapsed();

		println!("{item_path}: {} references in {elapsed:?}", found.references.len());
		assert!(!targets.is_empty() && !found.references.is_empty(), "{item_path}");
		assert!(found.notes.is_empty(), "{:?}", found.notes);
		assert!(elapsed.as_secs() < 10, "{item_path}: {elapsed:?}");
	}
}

#[test]
fn inherent_items_shadow_trait_items() {
	let ws = load_more(&fixture("rename_more"));
	let renamed = |item_path: &str, new_name: &str| {
		let rename = plan_ok(&ws, item_path, new_name, &RenameOptions::default());

		(rename.renamed, summary(&ws, &rename.references), summary(&ws, &rename.uncertain))
	};

	// `Counter::MAX`, and `Self::MAX` in the inherent `impl`, name the inherent constant
	let (items, references, _) = renamed("crate::counter::Counter::MAX", "CAP");

	assert_eq!(items, ["rename_more::counter::Counter::MAX"]);
	assert_eq!(references, ["src/counter.rs:16:12 Definition", "src/counter.rs:19:9 Path", "src/counter.rs:32:12 Path"]);

	// the trait's constant, with its implementation: `Self::MAX` in the trait, and `<_ as Limit>::MAX`
	let (items, references, _) = renamed("crate::counter::Limit::MAX", "CAP");

	assert_eq!(items, ["<rename_more::counter::Counter as Limit>::MAX", "rename_more::counter::Limit::MAX"]);

	assert_eq!(
		references,
		[
			"src/counter.rs:2:8 Definition",
			"src/counter.rs:5:9 Path",
			"src/counter.rs:24:8 Definition",
			"src/counter.rs:27:20 Path",
			"src/counter.rs:32:37 Path",
		]
	);

	// the same for methods: `Counter::limit` is the inherent method, `Limit::limit` the trait's
	let (items, references, _) = renamed("crate::counter::Counter::limit", "bound");

	assert_eq!(items, ["rename_more::counter::Counter::limit"]);
	assert_eq!(references, ["src/counter.rs:18:9 Definition", "src/counter.rs:32:51 Path"]);

	let (items, references, uncertain) = renamed("<crate::counter::Counter as crate::counter::Limit>::limit", "bound");

	assert_eq!(items, ["<rename_more::counter::Counter as Limit>::limit", "rename_more::counter::Limit::limit"]);
	assert_eq!(references, ["src/counter.rs:4:5 Definition", "src/counter.rs:26:5 Definition", "src/counter.rs:32:74 Path"]);
	assert_eq!(uncertain, ["src/counter.rs:9:28 MethodCall?"]);

	// a provided method, which `Counter::describe` names through the implementation of the trait
	let (items, references, _) = renamed("crate::counter::Limit::describe", "explain");

	assert_eq!(items, ["rename_more::counter::Limit::describe"]);
	assert_eq!(references, ["src/counter.rs:8:5 Definition", "src/counter.rs:32:99 Path"]);
}

#[test]
fn variants_through_glob_imports() {
	let ws = load_more(&fixture("rename_more"));
	let all = ReferenceOptions::all();

	// `Self::Dot`, and the patterns `Dot` through the module's glob import of the enum (in `match` and `matches!`)
	assert_eq!(
		references(&ws, "crate::variants::Shape::Dot", &all),
		[
			"src/variants.rs:1:82 DocLink",
			"src/variants.rs:7:2 Definition",
			"src/variants.rs:14:10 Path",
			"src/variants.rs:19:4 Path",
			"src/variants.rs:30:18 Path",
		]
	);

	// a tuple variant's constructor as a function (`map(Line)`), and in patterns
	assert_eq!(
		references(&ws, "crate::variants::Shape::Line", &all),
		[
			"src/variants.rs:8:2 Definition",
			"src/variants.rs:14:22 Path",
			"src/variants.rs:20:10 Path",
			"src/variants.rs:26:25 Path",
			"src/variants.rs:30:24 Path",
		]
	);

	// a struct variant (named like the standard library's `Box`, which the glob import shadows)
	assert_eq!(
		references(&ws, "crate::variants::Shape::Box", &all),
		["src/variants.rs:9:2 Definition", "src/variants.rs:14:31 Path", "src/variants.rs:20:32 Path"]
	);
}

#[test]
fn local_bindings_would_capture_references() {
	let ws = load_more(&fixture("rename_more"));

	// `let next = step(start);` uses the function before the variable exists, but `step(next)` would call the variable
	let message = collision(plan(&ws, "crate::captures::step", "next", &RenameOptions::default()));

	assert_eq!(message.matches("local variable").count(), 1, "{message}");
	assert!(
		message.contains(&native("`local variable next` in `rename_more::captures` (src/captures.rs:12:3)")),
		"{message}"
	);

	// the generic parameter `Item` would capture the type, but not the constructor
	let message = collision(plan(&ws, "crate::captures::Unit", "Item", &RenameOptions::default()));

	assert_eq!(message.matches("generic parameter").count(), 1, "{message}");
	assert!(
		message.contains(&native("`generic parameter Item` in `rename_more::captures` (src/captures.rs:15:24)")),
		"{message}"
	);

	// other names are fine, and forced renames report the captures
	plan_ok(&ws, "crate::captures::step", "advance", &RenameOptions::default());

	let rename = plan_ok(&ws, "crate::captures::step", "next", &forced());

	assert_eq!(rename.collisions.len(), 1);
	assert!(new_text(&ws, &rename, "src/captures.rs").contains("\tlet next = next(start);\n\n\t(next(next), item)"));
}

#[test]
fn files_loaded_by_several_crates() {
	let ws = load_more(&fixture("rename_more"));

	// the binary loads `src/shared.rs` too: its copies of the items are renamed with the library's
	let rename = plan_ok(&ws, "::rename_more::shared::common", "usual", &RenameOptions::default());

	assert_eq!(rename.renamed, ["rename_more::shared::common"]);
	assert_eq!(
		summary(&ws, &rename.references),
		["src/main.rs:8:31 Path", "src/shared.rs:1:8 Definition", "src/shared.rs:6:2 Path", "src/shared.rs:6:19 Path"]
	);

	// and both modules loading it are renamed, with one move of the file
	let rename = plan_ok(&ws, "::rename_more::shared", "common", &RenameOptions::default());
	let moves: Vec<(&Path, &Path)> = (rename.edits.moves().iter())
		.map(|(from, to)| (from.strip_prefix(ws.root()).unwrap(), to.strip_prefix(ws.root()).unwrap()))
		.collect();

	assert_eq!(moves, [(Path::new("src/shared.rs"), Path::new("src/common.rs"))]);

	assert_eq!(
		summary(&ws, &rename.references),
		["src/lib.rs:7:9 Definition", "src/main.rs:1:5 Definition", "src/main.rs:8:23 Path", "src/main.rs:8:41 Path", "src/main.rs:8:71 Path"]
	);

	// a module file that another module loads with `#[path]` cannot be moved
	let copy = TempCopy::new("rename_more", "path-shared");

	std::fs::create_dir_all(copy.path().join("src/bin")).unwrap();
	std::fs::write(copy.path().join("src/bin/extra.rs"), "#[path = \"../shared.rs\"]\nmod other;\n\nfn main() {\n\tother::twice();\n}\n").unwrap();

	let ws = load_more_with_extra(copy.path());

	match plan(&ws, "::rename_more::shared", "common", &RenameOptions::default()) {
		Err(Error::Unsupported(message)) => assert!(message.contains("module `extra::other` loads it with `#[path]`"), "{message}"),
		Err(error) => panic!("expected a refusal, got {error}"),
		Ok(rename) => panic!("expected a refusal, got {:?}", rename.renamed),
	}

	// the module with `#[path]` itself can be renamed (keeping its file), and the items of the file in all crates
	let rename = apply(&copy, load_more_with_extra, "::extra::other", "another", &RenameOptions::default());

	assert!(rename.edits.moves().is_empty());
	assert_eq!(rename.warnings, ["module `extra::other` is loaded from a file set with `#[path]`, which is not renamed"]);

	let rename = apply(&copy, load_more_with_extra, "::rename_more::shared::twice", "double", &RenameOptions::default());

	assert_eq!(rename.edits.edited_files().count(), 3);
	assert!(copy.read("src/bin/extra.rs").contains("mod another;\n\nfn main() {\n\tanother::double();"));
	cargo_check(copy.path());
}

#[test]
fn applied_renames_of_shadowing_and_shared_items_compile() {
	let copy = TempCopy::new("rename_more", "more");

	apply(&copy, load_more, "crate::counter::Counter::MAX", "CAP", &RenameOptions::default());
	apply(&copy, load_more, "crate::counter::Limit::MAX", "CEILING", &RenameOptions::default());
	apply(&copy, load_more, "crate::counter::Counter::limit", "bound", &RenameOptions::default());
	apply(&copy, load_more, "crate::counter::Limit::limit", "threshold", &method_calls());
	apply(&copy, load_more, "crate::counter::Limit::describe", "explain", &RenameOptions::default());

	// through a chain of re-exports, and an alias
	let rename = apply(&copy, load_more, "crate::Token", "Mark", &RenameOptions::default());

	assert_eq!(rename.renamed, ["rename_more::chain::inner::token::Token"]);

	// every `cfg` variant
	let rename = apply(&copy, load_more, "crate::Platform", "System", &RenameOptions::default());

	assert_eq!(rename.renamed, ["rename_more::Platform"]);
	assert_eq!(rename.references.iter().filter(|reference| reference.kind == ReferenceKind::Definition).count(), 2);

	apply(&copy, load_more, "crate::variants::Shape::Dot", "Point", &doc_links());
	apply(&copy, load_more, "crate::variants::Shape::Line", "Segment", &RenameOptions::default());
	apply(&copy, load_more, "crate::variants::Shape::Box", "Square", &RenameOptions::default());
	apply(&copy, load_more, "::rename_more::shared::common", "usual", &RenameOptions::default());
	apply(&copy, load_more, "::rename_more::shared", "common", &RenameOptions::default());

	let counter = copy.read("src/counter.rs");

	let values = "(Counter::CAP, <Counter as Limit>::CEILING, Counter::bound(counter), Limit::threshold(counter), Counter::explain(counter))";

	assert!(counter.contains(values), "{counter}");

	assert!(counter.contains("\tconst CEILING: u32;\n\n\tfn threshold(&self) -> u32 {\n\t\tSelf::CEILING\n"), "{counter}");
	assert!(counter.contains("format!(\"limit {}\", self.threshold())"), "{counter}");
	assert!(counter.contains("\tpub const CAP: u32 = 10;\n\n\tpub fn bound(&self) -> u32 {\n\t\tSelf::CAP\n"), "{counter}");
	assert!(counter.contains("\tconst CEILING: u32 = 20;\n\n\tfn threshold(&self) -> u32 {\n\t\t<Self as Limit>::CEILING\n"), "{counter}");

	assert_eq!(
		copy.read("src/chain.rs"),
		"pub mod inner {\n\tpub mod token {\n\t\t#[derive(Debug, Default)]\n\t\tpub struct Mark;\n\t}\n\n\tpub use self::token::Mark;\n}\n\npub use \
		 inner::Mark;\npub use self::inner::token::Mark as Tok;\n"
	);

	let main = copy.read("src/main.rs");

	for expected in [
		"mod common;\n",
		"use rename_more::Mark;\n",
		"let _ = (Mark, Tok, rename_more::chain::inner::token::Mark::default());",
		"println!(\"{} {} {}\", common::usual(), common::twice(), rename_more::common::twice());",
	] {
		assert!(main.contains(expected), "{expected}: {main}");
	}

	let variants = copy.read("src/variants.rs");

	for expected in [
		"[ ] and [`Shape::Point`].",
		"[Self::Point, Shape::Segment(1), Square { side: 2 }]",
		"\t\t\tPoint => 0,\n\t\t\tSelf::Segment(length) | Shape::Square { side: length } => length,",
		"map(Segment)",
		"matches!(shape, Point | Segment(0))",
	] {
		assert!(variants.contains(expected), "{expected}: {variants}");
	}

	assert!(copy.read("src/common.rs").contains("usual() + self::usual()"));
	assert!(copy.read("src/lib.rs").contains("pub mod common;\n") && copy.read("src/lib.rs").contains("pub struct System {"));
	cargo_check(copy.path());
}
