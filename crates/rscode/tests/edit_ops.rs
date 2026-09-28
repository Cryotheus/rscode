//! Planning and applying `remove`, `replace`, `insert`, and `format` on crates on disk.
//!
//! Tests copy the `edit_ops` fixture (or write small crates) to a temporary directory, load the crates from there,
//! and check the planned edits with [`EditSet::preview`], or apply them and check the files (and that the fixture
//! still compiles).

use rscode::CfgContext;
use rscode::CrateSpec;
use rscode::EditSet;
use rscode::Error;
use rscode::ItemKind;
use rscode::ItemPath;
use rscode::MatchOptions;
use rscode::PathPattern;
use rscode::Resolver;
use rscode::Workspace;
use rscode::edit::FmtOptions;
use rscode::edit::InsertOptions;
use rscode::edit::InsertPosition;
use rscode::edit::RemoveOptions;
use rscode::edit::ReplaceOptions;
use rscode::model::Dependency;
use rscode::model::TargetKind;
use rscode::rscode_fmt::FormatOptions;
use rscode::rscode_fmt::RsFormatter;
use rscode::rscode_fmt::SortOptions;
use rscode::source::LineCol;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

/// A temporary directory, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
	fn new(name: &str) -> Self {
		static COUNTER: AtomicU32 = AtomicU32::new(0);

		let count = COUNTER.fetch_add(1, Ordering::Relaxed);
		let path = std::env::temp_dir().join(format!("rscode-edit-ops-{}-{name}-{count}", std::process::id()));

		let _ = fs::remove_dir_all(&path);
		fs::create_dir_all(&path).unwrap();

		Self(path)
	}

	/// A copy of the `edit_ops` fixture.
	fn fixture(name: &str) -> Self {
		let dir = Self::new(name);

		copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/edit_ops"), &dir.0);
		dir
	}

	/// A directory with the files.
	fn with_files(name: &str, files: &[(&str, &str)]) -> Self {
		let dir = Self::new(name);

		for (path, text) in files {
			dir.write(path, text);
		}

		dir
	}

	fn path(&self, relative: &str) -> PathBuf {
		self.0.join(relative)
	}

	fn read(&self, relative: &str) -> String {
		fs::read_to_string(self.path(relative)).unwrap_or_else(|error| panic!("{relative}: {error}"))
	}

	fn write(&self, relative: &str, text: &str) {
		let path = self.path(relative);

		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, text).unwrap();
	}

	fn exists(&self, relative: &str) -> bool {
		self.path(relative).exists()
	}

	/// A path relative to the directory.
	fn relative_path(&self, path: &Path) -> String {
		path.strip_prefix(&self.0).unwrap().to_string_lossy().replace('\\', "/")
	}

	/// Paths relative to the directory.
	fn relative(&self, paths: &[PathBuf]) -> Vec<String> {
		paths.iter().map(|path| self.relative_path(path)).collect()
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.0);
	}
}

fn copy_dir(from: &Path, to: &Path) {
	fs::create_dir_all(to).unwrap();

	for entry in fs::read_dir(from).unwrap() {
		let entry = entry.unwrap();
		let target = to.join(entry.file_name());

		if entry.file_type().unwrap().is_dir() {
			if entry.file_name() != "target" {
				copy_dir(&entry.path(), &target);
			}
		} else {
			fs::copy(entry.path(), target).unwrap();
		}
	}
}

/// A spec for a crate of the directory, with no features enabled (so `cfg(feature = ...)` is definitely false).
fn spec(dir: &TempDir, name: &str, root: &str) -> CrateSpec {
	let mut spec = CrateSpec::new(name, dir.path(root));

	spec.cfg = CfgContext::new().with_features(Vec::<&str>::new());
	spec
}

fn load_specs(dir: &TempDir, specs: impl IntoIterator<Item = CrateSpec>) -> Workspace {
	let mut ws = Workspace::new(&dir.0);

	for spec in specs {
		ws.load_crate(spec);
	}

	ws.link();

	for krate in ws.crates() {
		assert!(krate.diagnostics().is_empty(), "{:?}", krate.diagnostics());
	}

	ws
}

/// Loads the library crate `fixture` from `src/lib.rs`.
fn load(dir: &TempDir) -> Workspace {
	load_specs(dir, [spec(dir, "fixture", "src/lib.rs")])
}

/// A spec for a binary crate of the directory that depends on the library `library`.
fn bin_spec(dir: &TempDir, name: &str, root: &str, library: &str) -> CrateSpec {
	let mut bin = spec(dir, name, root);

	bin.kind = TargetKind::Bin;
	bin.dependencies.push(Dependency { name: library.into(), crate_name: library.into(), package: None, krate: None });
	bin
}

/// Loads the library and binary crates of the `edit_ops` fixture (the binary unselected unless `with_bin`).
fn load_fixture(dir: &TempDir, with_bin: bool) -> Workspace {
	let mut bin = bin_spec(dir, "edit_ops", "src/main.rs", "edit_ops");

	bin.selected = with_bin;
	load_specs(dir, [spec(dir, "edit_ops", "src/lib.rs"), bin])
}

fn path(text: &str) -> ItemPath {
	ItemPath::parse(text).unwrap_or_else(|error| panic!("{error}"))
}

fn paths(texts: &[&str]) -> Vec<ItemPath> {
	texts.iter().map(|text| path(text)).collect()
}

fn pattern(text: &str) -> PathPattern {
	PathPattern::parse(text, MatchOptions::default()).unwrap_or_else(|error| panic!("{error}"))
}

/// The new contents of the changed files, by path relative to the directory.
fn changes(dir: &TempDir, edits: &EditSet) -> BTreeMap<String, String> {
	(edits.preview().unwrap().into_iter())
		.filter(|change| change.is_changed())
		.map(|change| (dir.relative_path(&change.path), change.formatted))
		.collect()
}

/// The contents of a file after the edits.
fn edited(dir: &TempDir, edits: &EditSet, file: &str) -> String {
	changes(dir, edits).remove(file).unwrap_or_else(|| dir.read(file))
}

fn at(line: usize, column: usize) -> LineCol {
	LineCol { line, column }
}

/// Proves that the edited crate in `dir` still compiles.
#[track_caller]
fn cargo_check(dir: &TempDir) {
	let output = Command::new(env!("CARGO"))
		.args(["check", "--quiet", "--all-targets", "--offline"])
		.current_dir(&dir.0)
		.env("CARGO_TARGET_DIR", dir.path("target"))
		.output()
		.unwrap();

	assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

mod remove {
	use super::*;

	fn remove(ws: &Workspace, targets: &[&str], options: &RemoveOptions) -> rscode::edit::Removal {
		rscode::edit::remove(&Resolver::new(ws), &paths(targets), options).unwrap_or_else(|error| panic!("{error}"))
	}

	const COMMENTS: &str = "\
//! Docs.

use std::fmt;

// About `a`.
/* More about `a`. */
/// Docs of `a`.
#[inline]
pub fn a() {} // trailing

// Not attached.

pub fn b() {}

pub struct S;

impl S {
	/// Docs of `m`.
	pub fn m(&self) {}

	pub fn n(&self) {}
}

impl fmt::Display for S {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(\"S\")
	}
}
";

	#[test]
	fn tells_impl_blocks_apart_by_their_generic_arguments() {
		let lib = "\
pub struct Wrapper(pub u32);

impl From<u8> for Wrapper {
	fn from(value: u8) -> Self {
		Self(value.into())
	}
}

impl From<u16> for Wrapper {
	fn from(value: u16) -> Self {
		Self(value.into())
	}
}

pub struct G<T>(pub T);

pub trait Tr {
	fn f(&self) -> u8;
}

impl Tr for G<u8> {
	fn f(&self) -> u8 {
		8
	}
}

impl Tr for G<u16> {
	fn f(&self) -> u8 {
		16
	}
}

impl G<u8> {
	pub fn get(&self) -> u8 {
		self.0
	}
}

impl G<u16> {
	pub fn get(&self) -> u16 {
		self.0
	}
}
";
		let dir = TempDir::with_files("remove-generic-impls", &[("src/lib.rs", lib)]);
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let named = |text: &str| -> Vec<String> {
			(resolver.resolve_item_path(&path(text)).into_iter()).map(|item| resolver.canonical_path(item).to_string()).collect()
		};

		// the paths shown for the `impl` blocks and their items tell them apart, and name them
		assert_eq!(named("impl From for crate::Wrapper"), ["impl From<u8> for fixture::Wrapper", "impl From<u16> for fixture::Wrapper"]);
		assert_eq!(named("impl From<u16> for fixture::Wrapper"), ["impl From<u16> for fixture::Wrapper"]);
		assert_eq!(named("<crate::Wrapper as From< u8 >>::from"), ["<fixture::Wrapper as From<u8>>::from"]);
		assert_eq!(named("<crate::G as crate::Tr>::f"), ["<fixture::G<u8> as Tr>::f", "<fixture::G<u16> as Tr>::f"]);
		assert_eq!(named("<fixture::G<u16> as Tr>::f"), ["<fixture::G<u16> as Tr>::f"]);
		assert!(named("<crate::G<u32> as crate::Tr>::f").is_empty());

		// a path naming several of them is not a path to `cfg` variants
		match rscode::edit::remove(&resolver, &paths(&["impl From for crate::Wrapper"]), &RemoveOptions::default()) {
			Err(Error::Ambiguous { candidates, .. }) => assert_eq!(
				candidates,
				["`impl From<u8> for fixture::Wrapper` at src/lib.rs:3:1", "`impl From<u16> for fixture::Wrapper` at src/lib.rs:9:1"]
			),
			other => panic!("{other:?}"),
		}

		let removal = remove(&ws, &["impl From<u8> for crate::Wrapper", "<crate::G<u16> as crate::Tr>::f"], &RemoveOptions::default());
		let edited = edited(&dir, &removal.edits, "src/lib.rs");

		assert!(!edited.contains("impl From<u8>") && edited.contains("impl From<u16> for Wrapper"), "{edited}");
		assert!(edited.contains("impl Tr for G<u16> {\n}") && edited.contains("\t\t8\n"), "{edited}");

		// found by patterns with generic arguments too
		let found = rscode::Find::new().pattern("<crate::G<u16> as *>::*").unwrap().run_with(&resolver).unwrap();

		assert_eq!(found.iter().map(|found| found.path.as_str()).collect::<Vec<_>>(), ["<fixture::G<u16> as Tr>::f"]);

		// items of inherent `impl`s too, whose canonical paths leave out the arguments
		assert_eq!(named("<crate::G<u16>>::get"), ["fixture::G::get"]);

		match rscode::edit::remove(&resolver, &paths(&["crate::G::get"]), &RemoveOptions::default()) {
			Err(Error::Ambiguous { candidates, .. }) => {
				assert_eq!(candidates, ["`<fixture::G<u8>>::get` at src/lib.rs:34:2", "`<fixture::G<u16>>::get` at src/lib.rs:40:2"]);
			}
			other => panic!("{other:?}"),
		}

		let found = rscode::Find::new().pattern("<G<u16>>::*").unwrap().run_with(&resolver).unwrap();

		assert_eq!(found.iter().map(|found| found.start.line).collect::<Vec<_>>(), [40]);

		// `all_variants` replaces `cfg` variants together, not these
		let all_variants = ReplaceOptions { all_variants: true, ..ReplaceOptions::default() };

		for (text, source) in [("<crate::G as Tr>::f", "fn f(&self) -> u8 {\n\t0\n}"), ("crate::G::get", "pub fn get(&self) {}")] {
			let replaced = rscode::edit::replace(&resolver, &path(text), source, &all_variants);

			assert!(matches!(replaced, Err(Error::Ambiguous { .. })), "{replaced:?}");
			assert!(!rscode::edit::replaces_all_variants(&resolver, &path(text)));
		}
	}

	#[test]
	fn removes_items_with_attached_comments() {
		let dir = TempDir::with_files("remove-comments", &[("src/lib.rs", COMMENTS)]);
		let ws = load(&dir);
		let removal = remove(&ws, &["crate::a"], &RemoveOptions::default());

		assert_eq!(
			edited(&dir, &removal.edits, "src/lib.rs"),
			COMMENTS.replace(
				"// About `a`.\n/* More about `a`. */\n/// Docs of `a`.\n#[inline]\npub fn a() {} // trailing\n\n",
				""
			)
		);
		assert_eq!(removal.removed.len(), 1);

		let removed = &removal.removed[0];

		assert_eq!((removed.path.as_str(), removed.kind), ("fixture::a", ItemKind::Fn));
		assert_eq!(removed.file, dir.path("src/lib.rs"));
		assert_eq!((removed.start, removed.end), (at(7, 1), at(9, 14)));
		assert!(removal.edits.deletions().is_empty());
	}

	#[test]
	fn removes_items_of_impl_blocks_and_impl_blocks() {
		let dir = TempDir::with_files("remove-impl", &[("src/lib.rs", COMMENTS)]);
		let ws = load(&dir);
		let removal = remove(&ws, &["crate::S::m"], &RemoveOptions::default());

		assert_eq!(
			edited(&dir, &removal.edits, "src/lib.rs"),
			COMMENTS.replace("\t/// Docs of `m`.\n\tpub fn m(&self) {}\n\n", "")
		);
		assert_eq!(removal.removed[0].path, "fixture::S::m");
		assert_eq!(removal.removed[0].kind, ItemKind::AssocFn);

		let removal = remove(&ws, &["<crate::S as fmt::Display>"], &RemoveOptions::default());
		let expected = COMMENTS.split("\n\nimpl fmt::Display").next().unwrap().to_owned() + "\n";

		assert_eq!(edited(&dir, &removal.edits, "src/lib.rs"), expected);
		assert_eq!(removal.removed[0].path, "impl fmt::Display for fixture::S");
		assert_eq!(removal.removed[0].kind, ItemKind::Impl);
	}

	const VARIANTS: &str = "\
pub enum One { A, B, C }

pub enum Lines {
	A,
	// About `B`.
	B = 2, // two
	C
}

pub enum Trailing {
	A,
	B,
}
";

	#[test]
	fn removes_variants_with_their_commas() {
		let dir = TempDir::with_files("remove-variants", &[("src/lib.rs", VARIANTS)]);
		let ws = load(&dir);
		let cases: &[(&[&str], &str, &str)] = &[
			(&["crate::One::A"], "{ A, B, C }", "{ B, C }"),
			(&["crate::One::B"], "{ A, B, C }", "{ A, C }"),
			(&["crate::One::C"], "{ A, B, C }", "{ A, B }"),
			(&["crate::One::B", "crate::One::C"], "{ A, B, C }", "{ A }"),
			(&["crate::Lines::B"], "\t// About `B`.\n\tB = 2, // two\n", ""),
			(&["crate::Lines::C"], "\tC\n", ""),
			(&["crate::Lines::A"], "\tA,\n", ""),
			(&["crate::Trailing::B"], "\tA,\n\tB,\n", "\tA,\n"),
		];

		for (targets, from, to) in cases {
			let removal = remove(&ws, targets, &RemoveOptions::default());

			assert_eq!(edited(&dir, &removal.edits, "src/lib.rs"), VARIANTS.replacen(from, to, 1), "{targets:?}");
			assert!(removal.removed.iter().all(|item| item.kind == ItemKind::Variant));
		}
	}

	#[test]
	fn removes_inline_modules() {
		let lib = "mod inline {\n\tpub fn f() {}\n\n\tmod deeper;\n}\n\npub fn g() {}\n";
		let dir =
			TempDir::with_files("remove-inline", &[("src/lib.rs", lib), ("src/inline/deeper.rs", "pub fn d() {}\n")]);
		let ws = load(&dir);
		let removal = remove(&ws, &["crate::inline", "crate::inline::f"], &RemoveOptions::default());

		assert_eq!(edited(&dir, &removal.edits, "src/lib.rs"), "pub fn g() {}\n");

		// items inside of other removed items are not listed
		assert_eq!(removal.removed.len(), 1);
		assert_eq!(removal.removed[0].path, "fixture::inline");

		// the files of out-of-line modules inside of inline modules go too, with their directory
		assert_eq!(dir.relative(removal.edits.deletions()), ["src/inline"]);

		// unless it has other files
		dir.write("src/inline/notes.txt", "notes\n");

		let removal = remove(&ws, &["crate::inline"], &RemoveOptions::default());

		assert_eq!(dir.relative(removal.edits.deletions()), ["src/inline/deeper.rs"]);
		assert!(removal.warnings.iter().all(|warning| !warning.contains("directory")), "{:?}", removal.warnings);
	}

	#[test]
	fn deletes_the_files_of_out_of_line_modules() {
		let dir = TempDir::fixture("remove-files");
		let ws = load_fixture(&dir, false);

		// `mod.rs` with a child: the whole directory
		let removal = remove(&ws, &["crate::nested"], &RemoveOptions::default());

		assert_eq!(dir.relative(removal.edits.deletions()), ["src/nested"]);
		assert!(!edited(&dir, &removal.edits, "src/lib.rs").contains("mod nested"));
		assert!(removal.warnings.iter().all(|warning| warning.contains("references")), "{:?}", removal.warnings);

		// a non-`mod.rs` file and the directory of its children
		let removal = remove(&ws, &["crate::shapes"], &RemoveOptions::default());

		assert_eq!(dir.relative(removal.edits.deletions()), ["src/shapes.rs", "src/shapes"]);

		// a file loaded with `#[path]`, and its directory when nothing else is in it
		let removal = remove(&ws, &["crate::placed"], &RemoveOptions::default());

		assert_eq!(dir.relative(removal.edits.deletions()), ["src/custom"]);
		assert!(edited(&dir, &removal.edits, "src/lib.rs").contains("pub mod nested;\npub mod shapes;\n"));

		dir.write("src/custom/other.rs", "\n");

		let removal = remove(&ws, &["crate::placed"], &RemoveOptions::default());

		assert_eq!(dir.relative(removal.edits.deletions()), ["src/custom/placed.rs"]);
		assert!(removal.warnings.iter().all(|warning| !warning.contains("directory")), "{:?}", removal.warnings);

		// a directory with other files: only the module files go
		let removal = remove(&ws, &["crate::docs"], &RemoveOptions::default());

		assert_eq!(dir.relative(removal.edits.deletions()), ["src/docs/mod.rs"]);
		assert!(
			removal.warnings.iter().any(|warning| {
				warning
					== "the directory `src/docs` is not deleted: it has files that are not part of the module `edit_ops::docs`"
			}),
			"{:?}",
			removal.warnings
		);

		// a file another crate loads too
		let removal = remove(&ws, &["crate::util"], &RemoveOptions::default());

		assert!(removal.edits.deletions().is_empty());
		assert!(
			removal
				.warnings
				.iter()
				.any(|warning| warning == "`src/util.rs` is not deleted: the module `edit_ops::util` loads it too"),
			"{:?}",
			removal.warnings
		);

		// or none at all
		let options = RemoveOptions { keep_files: true, ..RemoveOptions::default() };
		let removal = remove(&ws, &["crate::nested", "crate::shapes"], &options);

		assert!(removal.edits.deletions().is_empty());
		assert_eq!(changes(&dir, &removal.edits).len(), 1);
	}

	#[test]
	fn applies_removals_and_still_compiles() {
		let dir = TempDir::fixture("remove-apply");
		let ws = load_fixture(&dir, true);
		let targets =
			["crate::nested", "crate::shapes::Kind::Square", "crate::placed", "crate::extra", "crate::util::zeta"];
		let removal = remove(&ws, &targets, &RemoveOptions::default());

		// `util.rs` is loaded by both crates, and both remove `zeta`
		let removed: Vec<&str> = removal.removed.iter().map(|item| item.path.as_str()).collect();

		assert_eq!(
			removed,
			[
				"edit_ops::nested",
				"edit_ops::shapes::Kind::Square",
				"edit_ops::placed",
				"edit_ops::extra",
				"edit_ops::extra",
				"edit_ops::util::zeta",
			]
		);

		let applied = removal.edits.apply().unwrap();

		assert_eq!(dir.relative(&applied.written), ["src/lib.rs", "src/shapes.rs", "src/util.rs"]);
		assert_eq!(dir.relative(&applied.deleted), ["src/nested", "src/custom"]);
		assert!(!dir.exists("src/nested") && !dir.exists("src/custom"));
		assert_eq!(
			dir.read("src/util.rs"),
			"pub(crate) fn double(value: i32) -> i32 {\n\tvalue * 2\n}\n\n#[allow(dead_code)]\npub(crate)   fn   alpha( )->u8{2}\n"
		);
		assert!(dir.read("src/shapes.rs").ends_with("pub enum Kind {\n\tRound,\n\tTriangle,\n}\n"));

		let lib = dir.read("src/lib.rs");

		assert!(!lib.contains("extra") && !lib.contains("nested") && !lib.contains("placed"), "{lib}");
		assert!(lib.contains("} // trailing comment of add\n\n/// Doubles a number."), "{lib}");
		cargo_check(&dir);
	}

	const IMPORTS: &str = "\
pub mod gone {
	pub struct A;
	pub struct B;

	pub mod deeper {
		pub struct C;
	}
}

pub mod kept {
	pub struct K;
	pub struct L;
}

pub mod other {
	pub struct M;
	pub struct N;
}

pub struct Removed;

use gone::A;
use gone::{B, deeper::C};
use kept::{K, self as kept_module};
use crate::{Removed as R, kept::K as K2};
use crate::{kept::{K as K3, L}, other::{M, N}};
pub use gone::*;
use self::gone as g;
";

	fn prune(ws: &Workspace, targets: &[&str]) -> rscode::edit::Removal {
		remove(ws, targets, &RemoveOptions { prune_imports: true, ..RemoveOptions::default() })
	}

	#[test]
	fn prunes_imports_of_removed_items() {
		let app = "use fixture::gone::A;\nuse fixture::kept::K;\n\nfn main() {}\n";
		let dir = TempDir::with_files("remove-prune", &[("src/lib.rs", IMPORTS), ("src/main.rs", app)]);
		let ws = load_specs(&dir, [spec(&dir, "fixture", "src/lib.rs"), bin_spec(&dir, "app", "src/main.rs", "fixture")]);

		// whole `use` items (named, group, glob, and renamed imports), also in other crates
		let removal = prune(&ws, &["crate::gone"]);
		let lib = edited(&dir, &removal.edits, "src/lib.rs");

		assert!(lib.starts_with("pub mod kept {"), "{lib}");
		assert!(
			lib.ends_with(
				"pub struct Removed;\n\nuse kept::{K, self as kept_module};\nuse crate::{Removed as R, kept::K as K2};\nuse crate::{kept::{K as K3, L}, other::{M, N}};\n"
			),
			"{lib}"
		);
		assert_eq!(edited(&dir, &removal.edits, "src/main.rs"), "use fixture::kept::K;\n\nfn main() {}\n");

		let kinds: Vec<(&str, ItemKind)> = removal.removed.iter().map(|item| (item.path.as_str(), item.kind)).collect();

		assert_eq!(
			kinds,
			[
				("fixture::gone", ItemKind::Module),
				("use fixture::A", ItemKind::Import),
				("use fixture::B", ItemKind::Import),
				("use fixture::C", ItemKind::Import),
				("use fixture::*", ItemKind::Import),
				("use fixture::g", ItemKind::Import),
				("use app::A", ItemKind::Import),
			]
		);

		// a leaf of a group
		let removal = prune(&ws, &["crate::Removed"]);

		assert!(edited(&dir, &removal.edits, "src/lib.rs").contains("\nuse crate::{kept::K as K2};\n"));

		// a nested group whose leaves all go
		let removal = prune(&ws, &["crate::other"]);

		assert!(
			edited(&dir, &removal.edits, "src/lib.rs")
				.ends_with("\nuse crate::{kept::{K as K3, L}};\npub use gone::*;\nuse self::gone as g;\n")
		);

		// a leaf of a nested group
		let removal = prune(&ws, &["crate::kept::L"]);
		let lib = edited(&dir, &removal.edits, "src/lib.rs");

		assert!(lib.contains("\nuse crate::{kept::{K as K3}, other::{M, N}};\n"), "{lib}");
		assert!(lib.contains("\tpub struct K;\n}\n"), "{lib}");

		// without pruning, imports stay
		let removal = remove(&ws, &["crate::gone"], &RemoveOptions::default());

		assert!(edited(&dir, &removal.edits, "src/lib.rs").contains("use gone::{B, deeper::C};"));
		assert!(!changes(&dir, &removal.edits).contains_key("src/main.rs"));
	}

	#[test]
	fn prunes_imports_through_removed_modules() {
		let lib =
			"pub mod a {\n\tpub use crate::b::Thing;\n}\n\npub mod b {\n\tpub struct Thing;\n}\n\nuse a::Thing;\n";
		let dir = TempDir::with_files("remove-prune-through", &[("src/lib.rs", lib)]);
		let ws = load(&dir);

		// `a::Thing` is `b::Thing`, which stays, but `a` goes
		let removal = prune(&ws, &["crate::a"]);

		assert_eq!(edited(&dir, &removal.edits, "src/lib.rs"), "pub mod b {\n\tpub struct Thing;\n}\n");
	}

	#[test]
	fn keeps_imports_of_crates_whose_root_file_is_emptied() {
		let files = [("src/lib.rs", "pub fn a() {}\n"), ("src/main.rs", "use fixture as f;\n\nfn main() {}\n")];
		let dir = TempDir::with_files("remove-emptied-root", &files);
		let ws = load_specs(&dir, [spec(&dir, "fixture", "src/lib.rs"), bin_spec(&dir, "app", "src/main.rs", "fixture")]);
		let removal = prune(&ws, &["crate::a"]);

		assert_eq!(changes(&dir, &removal.edits), BTreeMap::from([("src/lib.rs".to_owned(), String::new())]));
		assert_eq!(removal.removed.len(), 1);
	}

	#[test]
	fn removes_cfg_variants() {
		let dir = TempDir::fixture("remove-cfg");
		let ws = load_fixture(&dir, false);

		// every variant by default
		let removal = remove(&ws, &["crate::extra"], &RemoveOptions::default());
		let lines: Vec<usize> = removal.removed.iter().map(|item| item.start.line).collect();

		assert_eq!(lines, [25, 31]);
		assert!(!edited(&dir, &removal.edits, "src/lib.rs").contains("extra"));

		// only those that may be active (no features are enabled)
		let options = RemoveOptions { active_only: true, ..RemoveOptions::default() };
		let removal = remove(&ws, &["crate::extra"], &options);
		let lib = edited(&dir, &removal.edits, "src/lib.rs");

		assert_eq!(removal.removed.len(), 1);
		assert!(
			lib.contains("#[cfg(feature = \"extra\")]") && !lib.contains("#[cfg(not(feature = \"extra\"))]"),
			"{lib}"
		);

		let lib = "#[cfg(feature = \"never\")]\npub fn never() {}\n";
		let dir = TempDir::with_files("remove-cfg-none", &[("src/lib.rs", lib)]);
		let ws = load(&dir);

		match rscode::edit::remove(&Resolver::new(&ws), &paths(&["crate::never"]), &options) {
			Err(Error::NotFound(message)) => assert_eq!(message, "crate::never` whose `cfg` can be `true"),
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn rejects_unknown_paths_and_crate_roots() {
		let dir = TempDir::with_files("remove-errors", &[("src/lib.rs", "pub fn a() {}\n")]);
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let options = RemoveOptions::default();

		assert!(matches!(
			rscode::edit::remove(&resolver, &paths(&["crate::a", "crate::nope"]), &options),
			Err(Error::NotFound(path)) if path == "crate::nope"
		));
		assert!(matches!(rscode::edit::remove(&resolver, &paths(&["crate"]), &options), Err(Error::Unsupported(_))));
	}

	#[test]
	fn reports_dangling_references() {
		let dir = TempDir::fixture("remove-dangling");
		let ws = load_fixture(&dir, true);
		let removal = remove(&ws, &["crate::util::double"], &RemoveOptions::default());
		let dangling: Vec<(String, LineCol)> =
			removal.dangling.iter().map(|reference| (dir.relative_path(&reference.path), reference.start)).collect();

		assert_eq!(dangling, [("src/lib.rs".to_owned(), at(22, 8)), ("src/main.rs".to_owned(), at(4, 22))]);

		// references inside of removed items (and pruned imports) are not dangling
		let removal = remove(&ws, &["crate::shapes::round", "crate::shapes::Circle"], &RemoveOptions::default());
		let files: Vec<String> = removal.dangling.iter().map(|reference| dir.relative_path(&reference.path)).collect();

		assert!(files.iter().all(|file| file == "src/lib.rs" || file == "src/shapes.rs"), "{files:?}");
		assert!(!removal.dangling.is_empty());
	}
}

mod replace {
	use super::*;

	fn replace(
		ws: &Workspace,
		target: &str,
		source: &str,
		options: &ReplaceOptions,
	) -> Result<rscode::edit::Replacement, Error> {
		rscode::edit::replace(&Resolver::new(ws), &path(target), source, options)
	}

	fn invalid_source(result: Result<rscode::edit::Replacement, Error>) -> String {
		match result {
			Err(Error::InvalidSource(message)) => message,
			other => panic!("expected invalid source, got {other:?}"),
		}
	}

	const LIB: &str = "\
use std::fmt;

// Attached to `add`.
/// Adds.
pub fn add(left: i32, right: i32) -> i32 {
	left + right
}

pub struct S;

impl S {
	/// Docs.
	pub fn m(&self) -> u8 {
		1
	}
}

pub enum E { A, B, C }

#[cfg(feature = \"x\")]
pub fn v() -> u8 {
	1
}

#[cfg(not(feature = \"x\"))]
pub fn v() -> u8 {
	2
}

pub struct One; pub struct Two;

pub mod out;
";

	fn dir(name: &str) -> TempDir {
		TempDir::with_files(name, &[("src/lib.rs", LIB), ("src/out.rs", "pub fn o() {}\n")])
	}

	#[test]
	fn replaces_items_with_their_docs_and_attributes() {
		let dir = dir("replace-fn");
		let ws = load(&dir);
		let source = "/// Adds, reversed.\npub fn add(left: i32, right: i32) -> i32 {\n    right + left\n}\n";
		let replacement = replace(&ws, "crate::add", source, &ReplaceOptions::default()).unwrap();

		// indented like the file (with tabs), and the comment above stays
		assert_eq!(
			edited(&dir, &replacement.edits, "src/lib.rs"),
			LIB.replace(
				"/// Adds.\npub fn add(left: i32, right: i32) -> i32 {\n\tleft + right\n}",
				"/// Adds, reversed.\npub fn add(left: i32, right: i32) -> i32 {\n\tright + left\n}"
			)
		);
		assert_eq!(replacement.replaced, ["fixture::add"]);
		assert_eq!(replacement.files, [dir.path("src/lib.rs")]);
		assert!(replacement.warnings.is_empty(), "{:?}", replacement.warnings);
	}

	#[test]
	fn replaces_associated_items_at_their_indentation() {
		let dir = dir("replace-assoc");
		let ws = load(&dir);
		let source = "pub fn m(&self) -> u8 {\n    if true {\n        2\n    } else {\n        3\n    }\n}";
		let replacement = replace(&ws, "crate::S::m", source, &ReplaceOptions::default()).unwrap();

		assert_eq!(
			edited(&dir, &replacement.edits, "src/lib.rs"),
			LIB.replace(
				"\t/// Docs.\n\tpub fn m(&self) -> u8 {\n\t\t1\n\t}",
				"\tpub fn m(&self) -> u8 {\n\t\tif true {\n\t\t\t2\n\t\t} else {\n\t\t\t3\n\t\t}\n\t}"
			)
		);

		// items of other containers do not parse
		let message = invalid_source(replace(&ws, "crate::S::m", "pub struct X;", &ReplaceOptions::default()));

		assert!(
			message.starts_with("the source does not parse as associated items of an `impl` block at 1:5:"),
			"{message}"
		);
	}

	#[test]
	fn keeps_line_endings() {
		let lib = "pub fn a() {\r\n\tlet x = 1;\r\n}\r\n";
		let dir = TempDir::with_files("replace-crlf", &[("src/lib.rs", lib)]);
		let ws = load(&dir);
		let replacement =
			replace(&ws, "crate::a", "pub fn a() {\n\tlet y = 2;\n}\n", &ReplaceOptions::default()).unwrap();

		assert_eq!(edited(&dir, &replacement.edits, "src/lib.rs"), "pub fn a() {\r\n\tlet y = 2;\r\n}\r\n");
	}

	#[test]
	fn refuses_other_kinds_unless_allowed() {
		let dir = dir("replace-kinds");
		let ws = load(&dir);

		assert_eq!(
			invalid_source(replace(&ws, "crate::add", "pub struct Add;", &ReplaceOptions::default())),
			"`fixture::add` is a fn, but the source is a struct (allow a kind change to replace it anyway)"
		);
		assert!(
			invalid_source(replace(&ws, "crate::add", "pub fn add() {}\npub fn sub() {}", &ReplaceOptions::default()))
				.starts_with("the source has 2 items, but `fixture::add` can only be replaced by one fn")
		);
		assert_eq!(
			invalid_source(replace(&ws, "crate::add", "// nothing\n", &ReplaceOptions::default())),
			"the source contains no module items"
		);

		let options = ReplaceOptions { allow_kind_change: true, ..ReplaceOptions::default() };
		let replacement = replace(&ws, "crate::add", "pub struct Add;\n\npub fn sum() {}", &options).unwrap();

		assert!(
			edited(&dir, &replacement.edits, "src/lib.rs")
				.contains("// Attached to `add`.\npub struct Add;\n\npub fn sum() {}\n\npub struct S;")
		);
		assert_eq!(
			replacement.warnings,
			["the replacement of `fixture::add` does not define `add`; references to it are not updated"]
		);
	}

	#[test]
	fn reports_where_the_source_does_not_parse() {
		let dir = dir("replace-invalid");
		let ws = load(&dir);
		let message = invalid_source(replace(
			&ws,
			"crate::add",
			"pub fn add(left: i32) -> i32 {\n\tleft +\n}",
			&ReplaceOptions::default(),
		));

		assert_eq!(
			message,
			"the source does not parse as module items at 3:1: unexpected end of input, expected an expression"
		);
	}

	#[test]
	fn replaces_variants() {
		let dir = dir("replace-variant");
		let ws = load(&dir);
		let replacement = replace(&ws, "crate::E::B", "B(u8),", &ReplaceOptions::default()).unwrap();

		assert!(edited(&dir, &replacement.edits, "src/lib.rs").contains("pub enum E { A, B(u8), C }"));
		assert_eq!(replacement.replaced, ["fixture::E::B"]);
	}

	#[test]
	fn replaces_cfg_variants_only_when_asked() {
		let dir = dir("replace-cfg");
		let ws = load(&dir);

		match replace(&ws, "crate::v", "pub fn v() -> u8 {\n\t3\n}", &ReplaceOptions::default()) {
			Err(Error::Ambiguous { path, candidates }) => {
				assert_eq!(path, "crate::v");
				assert_eq!(
					candidates,
					[
						"`fixture::v` (fn) at src/lib.rs:20:1 with #[cfg(feature = \"x\")]",
						"`fixture::v` (fn) at src/lib.rs:25:1 with #[cfg(not(feature = \"x\"))]",
					]
				);
			}
			other => panic!("{other:?}"),
		}

		assert!(rscode::edit::replaces_all_variants(&Resolver::new(&ws), &ItemPath::parse("crate::v").unwrap()));

		let options = ReplaceOptions { all_variants: true, ..ReplaceOptions::default() };
		let replacement = replace(&ws, "crate::v", "#[cfg(any())]\npub fn v() -> u8 {\n\t3\n}", &options).unwrap();
		let lib = edited(&dir, &replacement.edits, "src/lib.rs");

		assert_eq!(lib.matches("#[cfg(any())]\npub fn v() -> u8 {\n\t3\n}").count(), 2, "{lib}");
		assert_eq!(replacement.replaced, ["fixture::v"]);
		assert_eq!(
			replacement.warnings,
			["replaced 2 `cfg` variants of `crate::v` with the same source, with the `cfg`s of the source"]
		);

		// without `cfg`s in the source, every variant keeps its own (rather than all becoming unconditional)
		let source = "/// Three.\npub fn v() -> u8 {\n\t3\n}";
		let replacement = replace(&ws, "crate::v", source, &options).unwrap();
		let lib = edited(&dir, &replacement.edits, "src/lib.rs");

		assert!(lib.contains("#[cfg(feature = \"x\")]\n/// Three.\npub fn v() -> u8 {\n\t3\n}\n\n#[cfg(not(feature = \"x\"))]\n/// Three."), "{lib}");
		assert_eq!(lib.matches("pub fn v() -> u8 {\n\t3\n}").count(), 2, "{lib}");
		assert_eq!(replacement.warnings, ["replaced 2 `cfg` variants of `crate::v` with the same source, with their own `cfg`s"]);
	}

	#[test]
	fn keeps_code_after_trailing_comments() {
		let dir = dir("replace-trailing");
		let ws = load(&dir);
		let replacement =
			replace(&ws, "crate::One", "pub struct One; // the first", &ReplaceOptions::default()).unwrap();

		assert!(
			edited(&dir, &replacement.edits, "src/lib.rs")
				.contains("\npub struct One; // the first\npub struct Two;\n")
		);

		let replacement =
			replace(&ws, "crate::Two", "pub struct Two; // the second", &ReplaceOptions::default()).unwrap();

		assert!(
			edited(&dir, &replacement.edits, "src/lib.rs")
				.contains("\npub struct One; pub struct Two; // the second\n")
		);
	}

	#[test]
	fn warns_about_module_files() {
		let dir = dir("replace-module");
		let ws = load(&dir);
		let replacement = replace(&ws, "crate::out", "pub mod out {}", &ReplaceOptions::default()).unwrap();

		assert!(edited(&dir, &replacement.edits, "src/lib.rs").ends_with("\npub mod out {}\n"));
		assert_eq!(
			replacement.warnings,
			["only the declaration of the module `fixture::out` is replaced; its file `src/out.rs` is left as it is"]
		);
	}

	#[test]
	fn rejects_unknown_paths_and_crate_roots() {
		let dir = dir("replace-errors");
		let ws = load(&dir);

		assert!(matches!(
			replace(&ws, "crate::nope", "fn nope() {}", &ReplaceOptions::default()),
			Err(Error::NotFound(_))
		));
		assert!(matches!(replace(&ws, "crate", "fn x() {}", &ReplaceOptions::default()), Err(Error::Unsupported(_))));
	}

	#[test]
	fn applies_replacements_and_still_compiles() {
		let dir = TempDir::fixture("replace-apply");
		let ws = load_fixture(&dir, true);
		let resolver = Resolver::new(&ws);
		let options = ReplaceOptions::default();
		let mut edits = EditSet::new();

		let method = "/// Creates a circle.\npub fn new(radius: f64) -> Self {\n    let radius = Radius(radius.abs());\n\n    Self { radius }\n}\n";

		edits.extend(
			rscode::edit::replace(&resolver, &path("<crate::shapes::Circle>::new"), method, &options).unwrap().edits,
		);
		edits.extend(
			rscode::edit::replace(&resolver, &path("crate::shapes::Kind::Square"), "Square { side: f64 }", &options)
				.unwrap()
				.edits,
		);
		edits.extend(
			rscode::edit::replace(
				&resolver,
				&path("crate::util::double"),
				"pub(crate) fn double(value: i32) -> i32 {\n\tvalue + value\n}",
				&options,
			)
			.unwrap()
			.edits,
		);

		let applied = edits.apply().unwrap();

		assert_eq!(dir.relative(&applied.written), ["src/shapes.rs", "src/util.rs"]);
		assert!(dir.read("src/shapes.rs").contains(
			"\t/// Creates a circle.\n\tpub fn new(radius: f64) -> Self {\n\t\tlet radius = Radius(radius.abs());\n\n\t\tSelf { radius }\n\t}\n"
		));
		assert!(dir.read("src/shapes.rs").contains("\tRound,\n\tSquare { side: f64 },\n\tTriangle,\n"));
		assert!(dir.read("src/util.rs").starts_with("pub(crate) fn double(value: i32) -> i32 {\n\tvalue + value\n}\n"));
		cargo_check(&dir);
	}
}

mod insert {
	use super::*;

	fn insert(
		ws: &Workspace,
		parent: &str,
		source: &str,
		options: &InsertOptions,
	) -> Result<rscode::edit::Insertion, Error> {
		rscode::edit::insert(&Resolver::new(ws), &path(parent), source, options)
	}

	fn at_position(position: InsertPosition) -> InsertOptions {
		InsertOptions { position, force: false }
	}

	const LIB: &str = "\
//! Docs.

pub mod util {
	use std::fmt;

	pub fn a() {}

	pub fn b() {}
}

pub mod empty {}

pub struct S;

impl S {
	pub fn new() -> Self {
		S
	}
}

pub trait Tr {
	fn f(&self);
}

impl Tr for S {
	fn f(&self) {}
}

pub mod globbed {
	use super::*;
}

pub mod out;
";

	fn dir(name: &str) -> TempDir {
		TempDir::with_files(name, &[("src/lib.rs", LIB), ("src/out.rs", "//! Out.\n\npub fn o() {}\n")])
	}

	#[test]
	fn inserts_at_the_end_or_start_of_module_files() {
		let dir = dir("insert-files");
		let ws = load(&dir);
		let insertion = insert(&ws, "crate", "pub fn last() {}\n", &InsertOptions::default()).unwrap();

		assert_eq!(edited(&dir, &insertion.edits, "src/lib.rs"), format!("{LIB}\npub fn last() {{}}\n"));
		assert_eq!(insertion.inserted, [(ItemKind::Fn, Some("last".to_owned()))]);
		assert_eq!(insertion.file, dir.path("src/lib.rs"));
		assert!(insertion.warnings.is_empty());

		// after inner doc comments
		let insertion = insert(&ws, "crate", "pub fn first() {}", &at_position(InsertPosition::Start)).unwrap();

		assert_eq!(
			edited(&dir, &insertion.edits, "src/lib.rs"),
			LIB.replace("//! Docs.\n\n", "//! Docs.\n\npub fn first() {}\n\n")
		);

		// the file of an out-of-line module
		let insertion = insert(&ws, "crate::out", "pub struct O;", &at_position(InsertPosition::Start)).unwrap();

		assert_eq!(insertion.file, dir.path("src/out.rs"));
		assert_eq!(edited(&dir, &insertion.edits, "src/out.rs"), "//! Out.\n\npub struct O;\n\npub fn o() {}\n");
	}

	#[test]
	fn inserts_next_to_siblings() {
		let dir = dir("insert-siblings");
		let ws = load(&dir);

		// by name
		let insertion =
			insert(&ws, "crate::util", "pub fn c() {}", &at_position(InsertPosition::Before("b".to_owned()))).unwrap();

		assert!(
			edited(&dir, &insertion.edits, "src/lib.rs")
				.contains("\tpub fn a() {}\n\n\tpub fn c() {}\n\n\tpub fn b() {}\n}")
		);

		// by path
		let position = InsertPosition::After("crate::util::a".to_owned());
		let insertion = insert(&ws, "crate::util", "pub fn c() {\n    todo!()\n}", &at_position(position)).unwrap();

		assert!(
			edited(&dir, &insertion.edits, "src/lib.rs")
				.contains("\tpub fn a() {}\n\n\tpub fn c() {\n\t\ttodo!()\n\t}\n\n\tpub fn b() {}\n}")
		);

		// at the end and start of inline modules
		let insertion = insert(&ws, "crate::util", "pub fn z() {}", &InsertOptions::default()).unwrap();

		assert!(edited(&dir, &insertion.edits, "src/lib.rs").contains("\tpub fn b() {}\n\n\tpub fn z() {}\n}"));

		let insertion = insert(&ws, "crate::util", "pub fn z() {}", &at_position(InsertPosition::Start)).unwrap();

		assert!(
			edited(&dir, &insertion.edits, "src/lib.rs")
				.contains("pub mod util {\n\tpub fn z() {}\n\n\tuse std::fmt;\n")
		);

		let insertion = insert(&ws, "crate::empty", "pub fn z() {}", &InsertOptions::default()).unwrap();

		assert!(edited(&dir, &insertion.edits, "src/lib.rs").contains("pub mod empty {\n\tpub fn z() {}\n}\n"));
	}

	#[test]
	fn inserts_into_impl_blocks_and_traits() {
		let dir = dir("insert-impl");
		let ws = load(&dir);
		let insertion =
			insert(&ws, "<crate::S>", "pub fn get(&self) -> u8 {\n    1\n}", &InsertOptions::default()).unwrap();

		assert!(
			edited(&dir, &insertion.edits, "src/lib.rs")
				.contains("\t\tS\n\t}\n\n\tpub fn get(&self) -> u8 {\n\t\t1\n\t}\n}")
		);
		assert_eq!(insertion.inserted, [(ItemKind::AssocFn, Some("get".to_owned()))]);

		let insertion = insert(&ws, "crate::Tr", "fn g(&self) {}", &InsertOptions::default()).unwrap();

		assert!(
			edited(&dir, &insertion.edits, "src/lib.rs")
				.contains("pub trait Tr {\n\tfn f(&self);\n\n\tfn g(&self) {}\n}")
		);

		// an item that the trait does not have
		let insertion = insert(&ws, "<crate::S as Tr>", "fn h(&self) {}", &InsertOptions::default()).unwrap();

		assert!(
			edited(&dir, &insertion.edits, "src/lib.rs")
				.contains("impl Tr for S {\n\tfn f(&self) {}\n\n\tfn h(&self) {}\n}")
		);
		assert_eq!(insertion.warnings, ["the trait `fixture::Tr` has no item named `h`"]);
	}

	#[test]
	fn follows_the_indentation_style_of_the_file() {
		let lib = "pub struct S;\n\nimpl S {\n  pub fn a() {}\n}\n\nmod m {\n}\n";
		let dir = TempDir::with_files("insert-style", &[("src/lib.rs", lib)]);
		let ws = load(&dir);
		let insertion =
			insert(&ws, "<crate::S>", "pub fn b() {\n\tif true {\n\t\treturn;\n\t}\n}", &InsertOptions::default())
				.unwrap();

		assert_eq!(
			edited(&dir, &insertion.edits, "src/lib.rs"),
			"pub struct S;\n\nimpl S {\n  pub fn a() {}\n\n  pub fn b() {\n    if true {\n      return;\n    }\n  }\n}\n\nmod m {\n}\n"
		);

		let insertion = insert(&ws, "crate::m", "fn c() {}", &InsertOptions::default()).unwrap();

		assert!(edited(&dir, &insertion.edits, "src/lib.rs").ends_with("mod m {\n  fn c() {}\n}\n"));
	}

	#[test]
	fn refuses_collisions_unless_forced() {
		let dir = dir("insert-collisions");
		let ws = load(&dir);

		match insert(&ws, "crate::util", "pub fn a() {}", &InsertOptions::default()) {
			Err(Error::Collision { name, collisions }) => {
				assert_eq!(name, "a");
				assert_eq!(collisions, ["`a`: `fixture::util::a` (fn) at src/lib.rs:6:2"]);
			}
			other => panic!("{other:?}"),
		}

		// imports, and several names
		match insert(&ws, "crate::util", "pub mod fmt {}\npub fn b() {}", &InsertOptions::default()) {
			Err(error @ Error::Collision { .. }) => assert_eq!(
				error.to_string(),
				"`fmt`, `b` collides with existing names:\n`fmt`: the import of `std::fmt` at \
				 src/lib.rs:4:6\n`b`: `fixture::util::b` (fn) at src/lib.rs:8:2"
			),
			other => panic!("{other:?}"),
		}

		// associated items, and items of the source
		assert!(matches!(
			insert(&ws, "<crate::S>", "fn new() -> Self { S }", &InsertOptions::default()),
			Err(Error::Collision { .. })
		));

		match insert(&ws, "crate::empty", "fn x() {}\nfn x() {}", &InsertOptions::default()) {
			Err(Error::Collision { collisions, .. }) => assert_eq!(collisions, ["`x`: another item of the source"]),
			other => panic!("{other:?}"),
		}

		// different namespaces, and glob imports, do not collide
		assert!(insert(&ws, "crate::util", "pub struct a {}", &InsertOptions::default()).is_ok());
		assert!(insert(&ws, "crate::globbed", "pub struct S;", &InsertOptions::default()).is_ok());

		let insertion =
			insert(&ws, "crate::util", "pub fn a() {}", &InsertOptions { force: true, ..InsertOptions::default() })
				.unwrap();

		assert_eq!(insertion.warnings, ["inserted `a`, which collides with `fixture::util::a` (fn) at src/lib.rs:6:2"]);
	}

	#[test]
	fn refuses_colliding_imports() {
		let dir = dir("insert-imports");
		let ws = load(&dir);

		// in the namespaces their paths resolve in
		match insert(&ws, "crate::util", "use std::fmt;", &InsertOptions::default()) {
			Err(Error::Collision { collisions, .. }) => {
				assert_eq!(collisions, ["`fmt`: the import of `std::fmt` at src/lib.rs:4:6"])
			}
			other => panic!("{other:?}"),
		}

		match insert(&ws, "crate", "use crate::util::{a, b as c};", &InsertOptions::default()) {
			Ok(insertion) => assert_eq!(insertion.inserted, [(ItemKind::Use, None)]),
			other => panic!("{other:?}"),
		}

		// `crate::S` is a unit struct (a type and a value), `crate::util::a` a function
		assert!(matches!(insert(&ws, "crate::util", "use crate::S as a;", &InsertOptions::default()), Err(Error::Collision { .. })));
		assert!(insert(&ws, "crate::util", "use crate::Tr as a;", &InsertOptions::default()).is_ok());
	}

	#[test]
	fn inserts_into_files_of_several_crates_once() {
		let dir = TempDir::fixture("insert-shared");
		let ws = load_fixture(&dir, true);
		let insertion = insert(&ws, "crate::util", "pub(crate) fn one() -> i32 {\n\t1\n}", &InsertOptions::default()).unwrap();

		assert_eq!(changes(&dir, &insertion.edits).keys().collect::<Vec<_>>(), ["src/util.rs"]);
		assert!(edited(&dir, &insertion.edits, "src/util.rs").ends_with("{2}\n\npub(crate) fn one() -> i32 {\n\t1\n}\n"));
		assert!(matches!(insert(&ws, "crate::util", "fn double() {}", &InsertOptions::default()), Err(Error::Collision { .. })));
	}

	#[test]
	fn inserts_at_the_end_before_trailing_comments() {
		let lib = "pub mod m {\n\tpub fn a() {} // about a\n\n\t// the end of m\n}\n\n// the end of the file\n";
		let dir = TempDir::with_files("insert-trailing", &[("src/lib.rs", lib)]);
		let ws = load(&dir);
		let insertion = insert(&ws, "crate::m", "pub fn b() {}", &InsertOptions::default()).unwrap();

		assert_eq!(
			edited(&dir, &insertion.edits, "src/lib.rs"),
			"pub mod m {\n\tpub fn a() {} // about a\n\n\tpub fn b() {}\n\n\t// the end of m\n}\n\n// the end of the file\n"
		);

		let insertion = insert(&ws, "crate", "pub fn c() {}", &InsertOptions::default()).unwrap();

		assert_eq!(
			edited(&dir, &insertion.edits, "src/lib.rs"),
			lib.replace("}\n\n// the end of the file", "}\n\npub fn c() {}\n\n// the end of the file")
		);
	}

	#[test]
	fn keeps_line_endings() {
		let lib = "//! Docs.\r\n\r\npub fn a() {}\r\n";
		let dir = TempDir::with_files("insert-crlf", &[("src/lib.rs", lib)]);
		let ws = load(&dir);
		let insertion = insert(&ws, "crate", "pub fn b() {\n\ttodo!()\n}\n", &InsertOptions::default()).unwrap();

		// nothing is indented in the file, so indentation is 4 spaces
		assert_eq!(
			edited(&dir, &insertion.edits, "src/lib.rs"),
			"//! Docs.\r\n\r\npub fn a() {}\r\n\r\npub fn b() {\r\n    todo!()\r\n}\r\n"
		);
	}

	#[test]
	fn rejects_other_containers_and_positions() {
		let dir = dir("insert-errors");
		let ws = load(&dir);
		let options = InsertOptions::default();

		assert!(matches!(insert(&ws, "crate::nope", "fn x() {}", &options), Err(Error::NotFound(_))));

		match insert(&ws, "crate::util::a", "fn x() {}", &options) {
			Err(Error::Unsupported(message)) => assert_eq!(
				message,
				"cannot insert into `crate::util::a`: it is a fn, and items can only be inserted into modules, `impl` blocks, and \
				 traits"
			),
			other => panic!("{other:?}"),
		}

		match insert(&ws, "crate::util", "fn x() {}", &at_position(InsertPosition::Before("crate::S".to_owned()))) {
			Err(Error::Unsupported(message)) => assert_eq!(message, "`crate::S` is not an item of `crate::util`"),
			other => panic!("{other:?}"),
		}

		match insert(&ws, "crate::util", "fn x() {}", &at_position(InsertPosition::After("zzz".to_owned()))) {
			Err(error @ Error::NotFound(_)) => {
				assert_eq!(error.to_string(), "no item found for `zzz` in `crate::util`")
			}
			other => panic!("{other:?}"),
		}

		assert!(matches!(
			insert(&ws, "crate::util", "fn x() {}", &at_position(InsertPosition::After("a b".to_owned()))),
			Err(Error::PathParse(_))
		));

		match insert(&ws, "<crate::S>", "struct X;", &options) {
			Err(Error::InvalidSource(message)) => {
				assert!(message.contains("associated items of an `impl` block at 1:1"), "{message}")
			}
			other => panic!("{other:?}"),
		}

		match insert(&ws, "<crate::S>", "", &options) {
			Err(Error::InvalidSource(message)) => {
				assert_eq!(message, "the source contains no associated items of an `impl` block")
			}
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn tells_impl_blocks_apart_by_the_sibling() {
		let lib = "pub struct S;\n\nimpl S {\n\tpub fn a() {}\n}\n\nimpl S {\n\tpub fn b() {}\n}\n";
		let dir = TempDir::with_files("insert-ambiguous", &[("src/lib.rs", lib)]);
		let ws = load(&dir);

		match insert(&ws, "<crate::S>", "pub fn c() {}", &InsertOptions::default()) {
			Err(Error::Ambiguous { candidates, .. }) => {
				assert_eq!(
					candidates,
					["`impl fixture::S` (impl) at src/lib.rs:3:1", "`impl fixture::S` (impl) at src/lib.rs:7:1"]
				)
			}
			other => panic!("{other:?}"),
		}

		let insertion =
			insert(&ws, "<crate::S>", "pub fn c() {}", &at_position(InsertPosition::After("b".to_owned()))).unwrap();

		assert!(edited(&dir, &insertion.edits, "src/lib.rs").ends_with("\tpub fn b() {}\n\n\tpub fn c() {}\n}\n"));
	}

	#[test]
	fn applies_insertions_and_still_compiles() {
		let dir = TempDir::fixture("insert-apply");
		let ws = load_fixture(&dir, true);
		let resolver = Resolver::new(&ws);
		let mut edits = EditSet::new();
		let insertions = [
			("crate::util", "pub(crate) fn triple(value: i32) -> i32 {\n    value * 3\n}", InsertPosition::Start),
			(
				"<crate::shapes::Circle>",
				"/// The diameter.\npub fn diameter(&self) -> f64 {\n    self.radius.0 * 2.0\n}",
				InsertPosition::After("new".to_owned()),
			),
			(
				"crate::shapes::Shape",
				"/// The name.\nfn name(&self) -> &'static str {\n    \"shape\"\n}",
				InsertPosition::End,
			),
			(
				"crate::nested",
				"/// Also nested.\npub struct Also;",
				InsertPosition::Before("crate::nested::Nested".to_owned()),
			),
		];

		for (parent, source, position) in insertions {
			let options = InsertOptions { position, force: false };

			edits.extend(rscode::edit::insert(&resolver, &path(parent), source, &options).unwrap().edits);
		}

		let applied = edits.apply().unwrap();

		assert_eq!(dir.relative(&applied.written), ["src/nested/mod.rs", "src/shapes.rs", "src/util.rs"]);
		assert!(
			dir.read("src/util.rs")
				.starts_with("pub(crate) fn triple(value: i32) -> i32 {\n\tvalue * 3\n}\n\npub(crate) fn double")
		);
		assert!(dir.read("src/shapes.rs").contains(
			"\t\tSelf { radius }\n\t}\n\n\t/// The diameter.\n\tpub fn diameter(&self) -> f64 {\n\t\tself.radius.0 * 2.0\n\t}\n\n\t/// The radius."
		));
		assert!(dir.read("src/shapes.rs").contains(
			"\tfn area(&self) -> f64;\n\n\t/// The name.\n\tfn name(&self) -> &'static str {\n\t\t\"shape\"\n\t}\n}"
		));
		assert_eq!(
			dir.read("src/nested/mod.rs"),
			"//! A module in a `mod.rs` file.\n\npub mod deep;\n\n/// Also nested.\npub struct Also;\n\n/// Something nested.\npub struct Nested;\n"
		);
		cargo_check(&dir);
	}
}

mod format {
	use super::*;

	fn format(ws: &Workspace, targets: &[&str], options: &FmtOptions) -> Result<rscode::edit::Formatting, Error> {
		let targets: Vec<PathPattern> = targets.iter().map(|target| pattern(target)).collect();

		rscode::edit::format(&Resolver::new(ws), &targets, options)
	}

	fn rustfmt() -> FmtOptions {
		FmtOptions { format: FormatOptions::new().formatter(RsFormatter::RustFmt), ..FmtOptions::default() }
	}

	fn sort_only() -> FmtOptions {
		FmtOptions {
			format: FormatOptions::new().formatter(RsFormatter::None).sort(Some(SortOptions::new())),
			..FmtOptions::default()
		}
	}

	/// The files that the formatting processed, and whether they change.
	fn processed(dir: &TempDir, formatting: &rscode::edit::Formatting) -> Vec<(String, bool)> {
		(formatting.changes.iter()).map(|change| (dir.relative_path(&change.path), change.is_changed())).collect()
	}

	const UTIL: &str = "pub(crate) fn double(value: i32) -> i32 {\n\tvalue * 2\n}\n\n#[allow(dead_code)]\npub(crate)   fn   zeta( )->u8{1}\n\n#[allow(dead_code)]\npub(crate)   fn   alpha( )->u8{2}\n";

	#[test]
	fn formats_single_items() {
		let dir = TempDir::fixture("format-item");
		let ws = load_fixture(&dir, false);
		let formatting = format(&ws, &["crate::util::alpha"], &rustfmt()).unwrap();

		// with the fixture's `rustfmt.toml` (hard tabs)
		assert_eq!(
			edited(&dir, &formatting.edits, "src/util.rs"),
			UTIL.replace("pub(crate)   fn   alpha( )->u8{2}", "pub(crate) fn alpha() -> u8 {\n\t2\n}")
		);
		assert_eq!(processed(&dir, &formatting), [("src/util.rs".to_owned(), true)]);
		assert!(formatting.warnings.is_empty(), "{:?}", formatting.warnings);

		// the fixture's configuration is found from the file, unless set
		let mut options = rustfmt();

		options.format.rustfmt.config_path = Some(dir.path("src/nested"));
		options.format.rustfmt.config = vec![("hard_tabs".to_owned(), "false".to_owned())];

		let formatting = format(&ws, &["crate::util::alpha"], &options).unwrap();

		assert!(edited(&dir, &formatting.edits, "src/util.rs").ends_with("pub(crate) fn alpha() -> u8 {\n    2\n}\n"));
	}

	#[test]
	fn formats_modules_and_their_children() {
		let dir = TempDir::fixture("format-modules");

		dir.write(
			"src/shapes/round.rs",
			"/// A radius.\n#[derive(Debug, Clone, Copy)]\npub struct Radius( pub f64 );\n",
		);

		let ws = load_fixture(&dir, false);
		let formatting = format(&ws, &["crate::shapes"], &rustfmt()).unwrap();

		// sorted by path (by component, like `EditSet::preview`)
		assert_eq!(
			processed(&dir, &formatting),
			[("src/shapes/round.rs".to_owned(), true), ("src/shapes.rs".to_owned(), false)]
		);
		assert_eq!(
			edited(&dir, &formatting.edits, "src/shapes/round.rs"),
			"/// A radius.\n#[derive(Debug, Clone, Copy)]\npub struct Radius(pub f64);\n"
		);

		let options = FmtOptions { skip_children: true, ..rustfmt() };
		let formatting = format(&ws, &["crate::shapes"], &options).unwrap();

		assert_eq!(processed(&dir, &formatting), [("src/shapes.rs".to_owned(), false)]);
		assert!(formatting.edits.is_empty());
	}

	#[test]
	fn sorts_when_asked() {
		let dir = TempDir::fixture("format-sort");
		let ws = load_fixture(&dir, false);
		let formatting = format(&ws, &["crate::util"], &sort_only()).unwrap();
		let util = edited(&dir, &formatting.edits, "src/util.rs");
		let position = |text: &str| util.find(text).unwrap_or_else(|| panic!("{text}: {util}"));

		// sorted, not formatted
		assert!(position("fn   alpha") < position("fn double"), "{util}");
		assert!(position("fn double") < position("fn   zeta"), "{util}");

		// nothing to do without sorting and formatting
		let options = FmtOptions { format: FormatOptions::new().formatter(RsFormatter::None), ..FmtOptions::default() };
		let formatting = format(&ws, &["crate::util"], &options).unwrap();

		assert_eq!(processed(&dir, &formatting), [("src/util.rs".to_owned(), false)]);
		assert!(formatting.edits.is_empty());
	}

	#[test]
	fn skips_children_when_sorting() {
		let lib = "pub mod m {\n\tpub fn b() {}\n\tpub fn a() {}\n\n\tpub mod inner {\n\t\tpub fn d() {}\n\t\tpub fn c() {}\n\t}\n}\n";
		let dir = TempDir::with_files("format-sort-children", &[("src/lib.rs", lib)]);
		let ws = load(&dir);

		let formatting = format(&ws, &["crate::m"], &sort_only()).unwrap();
		let sorted = edited(&dir, &formatting.edits, "src/lib.rs");

		assert!(sorted.find("fn a").unwrap() < sorted.find("fn b").unwrap(), "{sorted}");
		assert!(sorted.find("fn c").unwrap() < sorted.find("fn d").unwrap(), "{sorted}");

		let formatting = format(&ws, &["crate::m"], &FmtOptions { skip_children: true, ..sort_only() }).unwrap();
		let sorted = edited(&dir, &formatting.edits, "src/lib.rs");

		assert!(sorted.find("fn a").unwrap() < sorted.find("fn b").unwrap(), "{sorted}");
		assert!(sorted.find("fn d").unwrap() < sorted.find("fn c").unwrap(), "{sorted}");
	}

	#[test]
	fn formats_with_prettyplease() {
		let dir = TempDir::fixture("format-prettyplease");
		let ws = load_fixture(&dir, false);
		let options =
			FmtOptions { format: FormatOptions::new().formatter(RsFormatter::PrettyPlease), ..FmtOptions::default() };
		let formatting = format(&ws, &["crate::util::zeta"], &options).unwrap();
		let util = edited(&dir, &formatting.edits, "src/util.rs");

		assert!(util.contains("pub(crate) fn zeta() -> u8 {"), "{util}");
		assert!(util.contains("pub(crate)   fn   alpha( )->u8{2}"), "{util}");
	}

	#[test]
	fn formats_every_file_of_crates() {
		let dir = TempDir::fixture("format-crate");
		let ws = load_fixture(&dir, true);
		let formatting = format(&ws, &["crate"], &rustfmt()).unwrap();
		let files: Vec<String> = processed(&dir, &formatting).into_iter().map(|(file, _)| file).collect();

		assert_eq!(
			files,
			[
				"src/custom/placed.rs",
				"src/docs/mod.rs",
				"src/lib.rs",
				"src/main.rs",
				"src/nested/deep.rs",
				"src/nested/mod.rs",
				"src/shapes/round.rs",
				"src/shapes.rs",
				"src/util.rs",
			]
		);
		assert_eq!(changes(&dir, &formatting.edits).keys().collect::<Vec<_>>(), ["src/util.rs"]);
	}

	#[test]
	fn matches_patterns() {
		let dir = TempDir::fixture("format-patterns");
		let ws = load_fixture(&dir, false);

		// every item of `util`
		let formatting = format(&ws, &["crate::util::*"], &rustfmt()).unwrap();
		let util = edited(&dir, &formatting.edits, "src/util.rs");

		assert!(
			util.contains("pub(crate) fn zeta() -> u8 {\n\t1\n}")
				&& util.contains("pub(crate) fn alpha() -> u8 {\n\t2\n}"),
			"{util}"
		);

		// at any depth
		let formatting = format(&ws, &["**::alpha"], &rustfmt()).unwrap();
		let util = edited(&dir, &formatting.edits, "src/util.rs");

		assert!(
			util.contains("pub(crate)   fn   zeta( )->u8{1}") && util.contains("pub(crate) fn alpha() -> u8 {"),
			"{util}"
		);

		// items of `impl` blocks, and the blocks
		dir.write(
			"src/shapes.rs",
			&dir.read("src/shapes.rs").replace("pub fn new(radius: f64) -> Self {", "pub fn new( radius: f64 )->Self{"),
		);

		let ws = load_fixture(&dir, false);

		for target in ["<crate::shapes::Circle>::new", "<*Circle>::new", "<crate::shapes::Circle>", "impl *Circle"] {
			let formatting = format(&ws, &[target], &rustfmt()).unwrap();

			assert!(
				edited(&dir, &formatting.edits, "src/shapes.rs").contains("pub fn new(radius: f64) -> Self {"),
				"{target}"
			);
		}

		let formatting = format(&ws, &["<crate::shapes::Circle as Shape>"], &rustfmt()).unwrap();

		assert!(formatting.edits.is_empty());
	}

	#[test]
	fn reports_targets_that_match_nothing() {
		let dir = TempDir::fixture("format-unmatched");
		let ws = load_fixture(&dir, false);

		assert!(
			matches!(format(&ws, &["crate::nope"], &rustfmt()), Err(Error::NotFound(path)) if path == "crate::nope")
		);

		let formatting = format(&ws, &["crate::nope*"], &rustfmt()).unwrap();

		assert!(formatting.changes.is_empty());
		assert_eq!(formatting.warnings, ["`crate::nope*` matches no item"]);
	}

	#[test]
	fn formats_active_cfg_variants_only_when_asked() {
		let lib = "#[cfg(feature = \"x\")]\nmod x;\n\nmod y;\n";
		let files = [("src/lib.rs", lib), ("src/x.rs", "fn  x( ) {}\n"), ("src/y.rs", "fn  y( ) {}\n")];
		let dir = TempDir::with_files("format-active", &files);
		let ws = load(&dir);
		let formatting = format(&ws, &["crate"], &rustfmt()).unwrap();

		assert_eq!(processed(&dir, &formatting).len(), 3);

		let formatting = format(&ws, &["crate"], &FmtOptions { active_only: true, ..rustfmt() }).unwrap();

		assert_eq!(processed(&dir, &formatting), [("src/lib.rs".to_owned(), false), ("src/y.rs".to_owned(), true)]);
	}

	#[test]
	fn reports_errors_with_the_file() {
		let dir = TempDir::fixture("format-error");
		let ws = load_fixture(&dir, false);
		let mut options = rustfmt();

		options.format.rustfmt.program = Some(dir.path("no-such-rustfmt"));

		match format(&ws, &["crate::util::alpha"], &options) {
			Err(Error::Io { path, source }) => {
				assert_eq!(path, dir.path("src/util.rs"));
				assert!(source.to_string().starts_with("failed to run rustfmt"), "{source}");
			}
			other => panic!("{other:?}"),
		}

		// unparsable module files are skipped
		let lib = "mod broken;\nmod fine;\n";
		let files = [("src/lib.rs", lib), ("src/broken.rs", "fn broken( {}\n"), ("src/fine.rs", "fn  fine( ) {}\n")];
		let dir = TempDir::with_files("format-broken", &files);
		let mut ws = Workspace::new(&dir.0);

		ws.load_crate(spec(&dir, "fixture", "src/lib.rs"));

		let formatting = format(&ws, &["crate"], &rustfmt()).unwrap();

		assert_eq!(processed(&dir, &formatting), [("src/fine.rs".to_owned(), true), ("src/lib.rs".to_owned(), false)]);
		assert_eq!(formatting.warnings.len(), 1);
		assert!(
			formatting.warnings[0].starts_with("the module `fixture::broken` is not formatted: cannot parse file"),
			"{:?}",
			formatting.warnings
		);
	}

	#[test]
	fn sorts_imports_like_rustfmt_in_every_style_edition() {
		let lib = "use b::x9;\nuse b::x10;\nuse d::Zeta;\nuse d::alpha;\n\npub fn f() {}\n";
		let earlier = "use b::x10;\nuse b::x9;\nuse d::alpha;\nuse d::Zeta;\n\npub fn f() {}\n";
		let dir = TempDir::with_files("format-style-edition", &[("src/lib.rs", lib)]);
		let load_edition = |edition| {
			let mut spec = spec(&dir, "fixture", "src/lib.rs");

			spec.edition = edition;
			load_specs(&dir, [spec])
		};

		// style edition 2024 (the edition's) sorts by version; neither sorting nor rustfmt changes the order
		let ws = load_edition(rscode::Edition::E2024);

		assert!(format(&ws, &["crate"], &sort_only()).unwrap().edits.is_empty());
		assert!(format(&ws, &["crate"], &rustfmt()).unwrap().edits.is_empty());

		// earlier style editions put `snake_case` first, and compare bytes
		let ws = load_edition(rscode::Edition::E2021);
		let sorted = format(&ws, &["crate"], &sort_only()).unwrap();

		assert_eq!(edited(&dir, &sorted.edits, "src/lib.rs"), earlier);
		assert_eq!(edited(&dir, &format(&ws, &["crate"], &rustfmt()).unwrap().edits, "src/lib.rs"), earlier);

		dir.write("src/lib.rs", earlier);

		let ws = load_edition(rscode::Edition::E2021);

		assert!(format(&ws, &["crate"], &sort_only()).unwrap().edits.is_empty());
		assert!(format(&ws, &["crate"], &rustfmt()).unwrap().edits.is_empty());

		// unless rustfmt's configuration sets a style edition
		dir.write("rustfmt.toml", "style_edition = \"2024\"\n");

		let sorted = format(&ws, &["crate"], &sort_only()).unwrap();

		assert_eq!(edited(&dir, &sorted.edits, "src/lib.rs"), lib);
	}

	#[test]
	fn formats_deeply_nested_files_in_parallel() {
		// several files are formatted on worker threads, which need as large a stack as the caller: a default one
		// overflows at a few hundred levels, aborting the process
		let depth = 1000;
		let deep = format!("pub fn f() -> i32 {{\n\t{}1{}\n}}\n", "(".repeat(depth), ")".repeat(depth));
		let lib = "mod deep;\nmod deeper;\n\npub fn b() {}\n";
		let dir = TempDir::with_files("format-deep", &[("src/lib.rs", lib), ("src/deep.rs", &deep), ("src/deeper.rs", &deep)]);

		let processed = std::thread::scope(|scope| {
			let thread = std::thread::Builder::new().stack_size(rscode::rscode_fmt::RECOMMENDED_STACK_SIZE);
			let formatted = thread.spawn_scoped(scope, || {
				let ws = load(&dir);
				let formatting = format(&ws, &["crate"], &sort_only()).unwrap();

				processed(&dir, &formatting)
			});

			formatted.unwrap().join().unwrap()
		});

		assert_eq!(
			processed,
			[("src/deep.rs".to_owned(), false), ("src/deeper.rs".to_owned(), false), ("src/lib.rs".to_owned(), false)]
		);
	}

	#[test]
	fn applies_formatting_and_still_compiles() {
		let dir = TempDir::fixture("format-apply");
		let ws = load_fixture(&dir, true);
		let options =
			FmtOptions { format: FormatOptions::new().sort(Some(SortOptions::new())), ..FmtOptions::default() };
		let formatting = format(&ws, &["crate"], &options).unwrap();
		let applied = formatting.edits.apply().unwrap();

		assert!(dir.relative(&applied.written).contains(&"src/util.rs".to_owned()));
		assert_eq!(
			dir.read("src/util.rs"),
			"#[allow(dead_code)]\npub(crate) fn alpha() -> u8 {\n\t2\n}\n\npub(crate) fn double(value: i32) -> i32 {\n\tvalue * 2\n}\n\n#[allow(dead_code)]\npub(crate) fn zeta() -> u8 {\n\t1\n}\n"
		);
		cargo_check(&dir);

		// formatting again changes nothing
		let ws = load_fixture(&dir, true);

		assert!(format(&ws, &["crate"], &options).unwrap().edits.is_empty());
	}
}

/// Planning edits of every kind on this crate's own source (never applied): the plans preview as parsable files.
mod real {
	use super::*;
	use rscode::path::CanonicalPath;

	fn load_self() -> Workspace {
		let root = Path::new(env!("CARGO_MANIFEST_DIR"));
		let mut ws = Workspace::new(root);

		ws.load_crate(CrateSpec::new("rscode", root.join("src/lib.rs")));
		ws.link();
		ws
	}

	/// A path naming the item with this canonical path, anchored at its crate.
	fn anchored(canonical: &CanonicalPath) -> Option<ItemPath> {
		let text = canonical.to_string();

		// `impl` blocks and items of `impl`s of types that are not loaded have no path
		(canonical.unresolved_self_ty.is_none() && !canonical.is_impl && canonical.impl_trait.is_none() && !text.contains('<'))
			.then(|| path(&format!("::{text}")))
	}

	#[test]
	fn plans_edits_of_real_code() {
		let ws = load_self();
		let resolver = Resolver::new(&ws);
		let krate = &ws.crates()[0];
		let (mut removed, mut replaced, mut inserted) = (0, 0, 0);

		for (index, (item, data)) in krate.items().enumerate() {
			let named = data.name.is_some() && data.kind != ItemKind::Import && !item.is_crate_root();
			let container = matches!(data.kind, ItemKind::Module | ItemKind::Trait);

			// a sample of the items, and every container
			let Some(item_path) = (named && (index % 9 == 0 || container))
				.then(|| anchored(&resolver.canonical_path(item)))
				.flatten()
			else {
				continue;
			};

			// removing it (and imports of it) leaves parsable files; a smaller sample, since every removal searches
			// the crate for references
			if index % 63 == 0 || data.kind == ItemKind::Module {
				let options = RemoveOptions { prune_imports: true, ..RemoveOptions::default() };
				let removal = rscode::edit::remove(&resolver, std::slice::from_ref(&item_path), &options).unwrap();

				removal.edits.preview().unwrap_or_else(|error| panic!("removing `{item_path}`: {error}"));
				removed += 1;
			}

			// replacing it with its own text changes nothing
			let targets = resolver.resolve_item_path(&item_path);

			if let [target] = targets.as_slice() {
				let text = ws.item_text(*target);
				let replacement = rscode::edit::replace(&resolver, &item_path, text, &ReplaceOptions::default())
					.unwrap_or_else(|error| panic!("replacing `{item_path}`: {error}"));
				let changes = replacement.edits.preview().unwrap();

				// lines only of whitespace become empty
				if !text.lines().any(|line| line.trim().is_empty() && !line.is_empty()) {
					assert!(changes.iter().all(|change| !change.is_changed()), "replacing `{item_path}` changes it");
				}

				replaced += 1;
			}

			// inserting into it (when it holds items) leaves parsable files
			if container {
				for position in [InsertPosition::Start, InsertPosition::End] {
					let options = InsertOptions { position, force: true };
					let insertion = rscode::edit::insert(&resolver, &item_path, "fn rscode_inserted() {}", &options)
						.unwrap_or_else(|error| panic!("inserting into `{item_path}`: {error}"));

					insertion.edits.preview().unwrap_or_else(|error| panic!("inserting into `{item_path}`: {error}"));
				}

				inserted += 1;
			}
		}

		assert!(removed > 50 && replaced > 100 && inserted > 10, "{removed} {replaced} {inserted}");
	}
}

/// Statics declared by `thread_local!` are items of the module the invocation is in.
mod thread_locals {
	use super::*;
	use rscode::edit::RenameOptions;

	const LIB: &str = "\
use std::cell::Cell;
use std::cell::RefCell;

pub mod state {
	use super::Counter;

	thread_local! {
		/// The current depth.
		pub static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };

		pub(crate) static COUNTER: std::cell::RefCell<Counter> = std::cell::RefCell::new(Counter::new())
	}
}

#[cfg(any())]
std::thread_local!(static NEVER: Cell<u8> = Cell::new(1));

std::thread_local!(static ONE: Cell<u8> = Cell::new(1));

#[derive(Default)]
pub struct Counter(u8);

impl Counter {
	pub fn new() -> Self {
		Self(0)
	}
}

pub fn depth() -> u32 {
	let counted = state::COUNTER.with(|counter| counter.borrow().0);

	state::DEPTH.with(Cell::get) + u32::from(ONE.with(Cell::get)) + u32::from(counted)
}

pub fn unused() -> RefCell<u8> {
	RefCell::new(0)
}
";

	const MANIFEST: &str = "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n";

	fn crate_dir(name: &str) -> TempDir {
		TempDir::with_files(name, &[("Cargo.toml", MANIFEST), ("src/lib.rs", LIB)])
	}

	#[test]
	fn are_found_resolved_and_viewed() {
		let dir = crate_dir("thread-local-load");
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let found = rscode::Find::new().kind(ItemKind::Static).pattern("**").unwrap().run_with(&resolver).unwrap();
		let statics: Vec<String> = found.iter().map(|found| found.path.to_string()).collect();

		assert_eq!(statics, ["fixture::state::DEPTH", "fixture::state::COUNTER", "fixture::NEVER", "fixture::ONE"]);

		let depth = resolver.resolve_item_path(&path("crate::state::DEPTH"));

		assert_eq!(depth.len(), 1);
		assert_eq!(ws.item(depth[0]).detail, rscode::model::ItemDetail::Static { mutable: false, thread_local: true });
		assert_eq!(ws.item(ws.parent(depth[0]).expect("the invocation")).kind, ItemKind::MacroCall);

		let one = resolver.resolve_item_path(&path("crate::ONE"));

		assert!(one.len() == 1 && ws.item(one[0]).is_thread_local());
		assert_eq!(
			ws.item_text(depth[0]),
			"/// The current depth.\n\t\tpub static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };"
		);

		let counter = resolver.resolve_item_path(&path("state::COUNTER"))[0];

		assert_eq!(
			ws.item_text(counter),
			"pub(crate) static COUNTER: std::cell::RefCell<Counter> = std::cell::RefCell::new(Counter::new())"
		);

		// the `cfg` of the invocation applies to its statics
		let never = resolver.resolve_item_path(&path("crate::NEVER"))[0];

		assert_eq!(ws.effective_cfg(never).unwrap().to_string(), "any()");
		assert!(!ws.is_active(never).is_possible());
	}

	#[test]
	fn rename_updates_declarations_and_the_code_inside() {
		let dir = crate_dir("thread-local-rename");
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let options = RenameOptions::default();
		let depth = rscode::edit::rename(&resolver, &path("crate::state::DEPTH"), "LEVEL", &options).unwrap();
		let text = edited(&dir, &depth.edits, "src/lib.rs");

		assert!(text.contains("pub static LEVEL: std::cell::Cell<u32>"), "{text}");
		assert!(text.contains("state::LEVEL.with(Cell::get)"), "{text}");

		// references in the declarations are certain, not only possible (like names in other macro bodies)
		let counter = rscode::edit::rename(&resolver, &path("crate::Counter"), "Tally", &options).unwrap();
		let text = edited(&dir, &counter.edits, "src/lib.rs");

		assert!(text.contains("std::cell::RefCell<Tally> = std::cell::RefCell::new(Tally::new())"), "{text}");
		counter.edits.apply().unwrap();
		cargo_check(&dir);
	}

	#[test]
	fn removing_every_declaration_removes_the_invocation() {
		let dir = crate_dir("thread-local-remove");
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let options = RemoveOptions::default();
		let remove = |targets: &[&str]| rscode::edit::remove(&resolver, &paths(targets), &options).unwrap();

		let text = edited(&dir, &remove(&["crate::state::COUNTER"]).edits, "src/lib.rs");

		assert!(
			text.contains(
				"\tthread_local! {\n\t\t/// The current depth.\n\t\tpub static DEPTH: std::cell::Cell<u32> = const { \
				 std::cell::Cell::new(0) };\n\t}\n"
			),
			"{text}"
		);

		let text = edited(&dir, &remove(&["crate::state::DEPTH", "crate::state::COUNTER"]).edits, "src/lib.rs");

		assert!(text.contains("pub mod state {\n\tuse super::Counter;\n}\n"), "{text}");

		let never = remove(&["crate::NEVER"]);
		let text = edited(&dir, &never.edits, "src/lib.rs");

		assert!(!text.contains("any()") && !text.contains("NEVER"), "{text}");
		never.edits.apply().unwrap();
		cargo_check(&dir);
	}

	#[test]
	fn replacements_keep_the_declarations_separated() {
		let dir = crate_dir("thread-local-replace");
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let options = ReplaceOptions::default();

		// the `;` separating it from the next declaration is added
		let depth = "pub static DEPTH: std::cell::Cell<u32> = std::cell::Cell::new(7)";
		let replaced = rscode::edit::replace(&resolver, &path("crate::state::DEPTH"), depth, &options).unwrap();
		let text = edited(&dir, &replaced.edits, "src/lib.rs");

		assert!(
			text.contains("\t\tpub static DEPTH: std::cell::Cell<u32> = std::cell::Cell::new(7);\n\n\t\tpub(crate) static"),
			"{text}"
		);

		match rscode::edit::replace(&resolver, &path("crate::state::DEPTH"), &format!("{depth} // seven"), &options) {
			Err(Error::InvalidSource(message)) => assert!(message.contains("end the source with `;`"), "{message}"),
			other => panic!("{other:?}"),
		}

		// only declarations are accepted
		assert!(matches!(
			rscode::edit::replace(&resolver, &path("crate::ONE"), "const ONE: u8 = 1;", &options),
			Err(Error::InvalidSource(_))
		));

		replaced.edits.apply().unwrap();
		cargo_check(&dir);
	}

	/// A `thread_local!` in a function body declares local statics, which shadow the module's.
	#[test]
	fn local_declarations_shadow_module_items() {
		let lib = "\
use std::cell::Cell;

pub static X: u8 = 0;

pub fn local() -> u8 {
	thread_local!(static X: Cell<u8> = Cell::new(1));

	X.with(Cell::get)
}

pub fn global() -> u8 {
	X
}
";
		let dir = TempDir::with_files("thread-local-shadow", &[("Cargo.toml", MANIFEST), ("src/lib.rs", lib)]);
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let renamed = rscode::edit::rename(&resolver, &path("crate::X"), "Y", &RenameOptions::default()).unwrap();
		let text = edited(&dir, &renamed.edits, "src/lib.rs");

		assert_eq!(text, lib.replace("pub static X", "pub static Y").replace("\tX\n}", "\tY\n}"));
		renamed.edits.apply().unwrap();
		cargo_check(&dir);
	}

	/// Statics declared by `thread_local!` are formatted with their invocation, and only it: rustfmt re-indents a braced
	/// body without formatting it, and leaves a parenthesized one alone.
	#[test]
	fn are_formatted_with_their_invocation() {
		let dir = crate_dir("thread-local-format");

		dir.write("rustfmt.toml", "hard_tabs = false\n");

		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let options =
			FmtOptions { format: FormatOptions::new().formatter(RsFormatter::RustFmt).sort(None), ..FmtOptions::default() };
		let targets = [pattern("crate::ONE"), pattern("crate::state::*")];
		let formatting = rscode::edit::format(&resolver, &targets, &options).unwrap();

		assert!(formatting.warnings.is_empty(), "{:?}", formatting.warnings);

		// only the invocation in `state` changes (rustfmt indents it with spaces); the rest of the file keeps its tabs
		let expected = LIB
			.replace("\tthread_local! {\n\t\t/// The current depth.\n\t\tpub static DEPTH", "    thread_local! {\n        /// The current depth.\n        pub static DEPTH")
			.replace("\n\t\tpub(crate) static COUNTER", "\n        pub(crate) static COUNTER")
			.replace("Counter::new())\n\t}\n}", "Counter::new())\n    }\n}");

		assert_eq!(edited(&dir, &formatting.edits, "src/lib.rs"), expected);
	}
}

/// Imports are named by `use` paths (`use crate::Circle`); other paths go through them.
mod imports {
	use super::*;
	use rscode::edit::InsertOptions;
	use rscode::edit::RenameOptions;

	const LIB: &str = "\
pub mod shapes {
	pub struct Circle;

	pub struct Square;

	pub trait Shape {
		fn area(&self) -> f64 {
			1.0
		}
	}

	impl Shape for Circle {}

	impl Shape for Square {}
}

pub mod util {
	pub fn helper() -> f64 {
		2.0
	}
}

use shapes::Circle;
use shapes::{Shape as _, Square};
#[allow(unused_imports)]
use shapes::{Circle as Round, Square as Block};
use util::*;

pub use util::helper as assist;

pub fn total() -> f64 {
	Circle.area() + Square.area() + helper() + assist()
}
";

	const MANIFEST: &str = "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n";

	fn crate_dir(name: &str) -> TempDir {
		TempDir::with_files(name, &[("Cargo.toml", MANIFEST), ("src/lib.rs", LIB)])
	}

	fn paths_of(resolver: &Resolver<'_>, path: &str) -> Vec<String> {
		let mut paths: Vec<String> =
			resolver.resolve_item_path(&self::path(path)).iter().map(|&item| resolver.canonical_path(item).to_string()).collect();

		paths.sort();
		paths
	}

	#[test]
	fn use_paths_name_imports() {
		let dir = crate_dir("imports-resolve");
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);

		assert_eq!(paths_of(&resolver, "use crate::Circle"), ["use fixture::Circle"]);
		assert_eq!(paths_of(&resolver, "use fixture::Round"), ["use fixture::Round"]);
		assert_eq!(paths_of(&resolver, "use crate::*"), ["use fixture::*"]);
		assert_eq!(paths_of(&resolver, "use crate::_"), ["use fixture::_"]);
		assert_eq!(paths_of(&resolver, "use Square"), ["use fixture::Square"]);
		assert_eq!(paths_of(&resolver, "use crate::assist"), ["use fixture::assist"]);
		assert_eq!(paths_of(&resolver, "use crate::shapes::Circle"), Vec::<String>::new());

		// other paths go through them
		assert_eq!(paths_of(&resolver, "crate::Circle"), ["fixture::shapes::Circle"]);

		// a view shows the whole `use` item
		let views = rscode::View::new().path("use crate::Round").unwrap().run_with(&resolver).unwrap();

		assert_eq!(views.len(), 1);
		assert_eq!(views[0].path, "use fixture::Round");
		assert_eq!(views[0].kind, ItemKind::Import);
		assert_eq!(views[0].text, "#[allow(unused_imports)]\nuse shapes::{Circle as Round, Square as Block};");
	}

	#[test]
	fn use_patterns_find_imports() {
		let dir = crate_dir("imports-find");
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let find = |pattern: &str| {
			let found = rscode::Find::new().pattern(pattern).unwrap().run_with(&resolver).unwrap();
			let mut paths: Vec<String> = found.iter().map(|found| found.path.clone()).collect();

			paths.sort();
			paths
		};

		assert_eq!(
			find("use crate::*"),
			[
				"use fixture::*",
				"use fixture::Block",
				"use fixture::Circle",
				"use fixture::Round",
				"use fixture::Square",
				"use fixture::_",
				"use fixture::assist",
			]
		);
		assert_eq!(find("use Circle"), ["use fixture::Circle"]);
		assert_eq!(find("Circle"), ["fixture::shapes::Circle"]);
	}

	#[test]
	fn paths_through_private_imports_are_ambiguous_for_edits() {
		let dir = crate_dir("imports-ambiguous");
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);

		for result in [
			rscode::edit::remove(&resolver, &paths(&["crate::Circle"]), &RemoveOptions::default()).map(|_| ()),
			rscode::edit::replace(&resolver, &path("crate::Circle"), "pub struct Circle;", &ReplaceOptions::default()).map(|_| ()),
		] {
			match result {
				Err(Error::Ambiguous { candidates, .. }) => {
					assert_eq!(candidates.len(), 2, "{candidates:?}");
					assert!(candidates[0].starts_with("`use fixture::Circle` (import) at src/lib.rs:"), "{candidates:?}");
					assert!(candidates[1].starts_with("`fixture::shapes::Circle` (struct) at src/lib.rs:"), "{candidates:?}");
				}
				other => panic!("{other:?}"),
			}
		}

		// re-exports are paths to what they export
		let removal = rscode::edit::remove(&resolver, &paths(&["crate::assist"]), &RemoveOptions::default()).unwrap();

		assert_eq!(removal.removed[0].path, "fixture::util::helper");
		assert!(!rscode::edit::replaces_all_variants(&resolver, &path("crate::Circle")));
	}

	#[test]
	fn removing_imports() {
		let dir = crate_dir("imports-remove");
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let remove = |targets: &[&str]| rscode::edit::remove(&resolver, &paths(targets), &RemoveOptions::default()).unwrap();

		// a leaf of a group
		let one = remove(&["use crate::Round"]);

		assert!(edited(&dir, &one.edits, "src/lib.rs").contains("#[allow(unused_imports)]\nuse shapes::{Square as Block};\n"));
		assert_eq!(one.removed[0].path, "use fixture::Round");
		assert!(one.warnings.iter().any(|warning| warning.contains("uses `Round` through `use fixture::Round`")), "{:?}", one.warnings);

		// every leaf: the whole `use` item goes, with its attributes
		let both = remove(&["use crate::Round", "use crate::Block"]);
		let text = edited(&dir, &both.edits, "src/lib.rs");

		assert!(!text.contains("unused_imports") && !text.contains("Round") && !text.contains("Block"), "{text}");
		assert!(text.contains("use shapes::{Shape as _, Square};\nuse util::*;\n"), "{text}");

		both.edits.apply().unwrap();
		cargo_check(&dir);
	}

	#[test]
	fn replacing_inserting_formatting_and_renaming_imports() {
		let dir = crate_dir("imports-edit");
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);

		// an import is replaced as its `use` item, which must import nothing else
		let replaced =
			rscode::edit::replace(&resolver, &path("use crate::Circle"), "use crate::shapes::Circle;", &ReplaceOptions::default())
				.unwrap();

		assert_eq!(replaced.replaced, ["use fixture::Circle"]);
		assert!(edited(&dir, &replaced.edits, "src/lib.rs").contains("\nuse crate::shapes::Circle;\nuse shapes::{Shape as _, Square};"));

		match rscode::edit::replace(&resolver, &path("use crate::Round"), "use shapes::Circle as Round;", &ReplaceOptions::default()) {
			Err(Error::Unsupported(message)) => assert!(message.contains("is one of the 2 imports of the `use` item"), "{message}"),
			other => panic!("{other:?}"),
		}

		// an anchor that is an import stands for its `use` item
		let options = InsertOptions { position: InsertPosition::After("use crate::Circle".to_owned()), force: false };
		let insertion = rscode::edit::insert(&resolver, &path("crate"), "use shapes::Shape;", &options).unwrap();

		let text = edited(&dir, &insertion.edits, "src/lib.rs");

		assert!(text.contains("use shapes::Circle;\n\nuse shapes::Shape;\n\nuse shapes::{Shape as _"), "{text}");

		// formatting an import formats its `use` item
		let options =
			FmtOptions { format: FormatOptions::new().formatter(RsFormatter::RustFmt).sort(None), ..FmtOptions::default() };
		let formatting = rscode::edit::format(&resolver, &[pattern("use crate::Round")], &options).unwrap();

		assert!(formatting.warnings.is_empty(), "{:?}", formatting.warnings);

		// imports are not renamed
		match rscode::edit::rename(&resolver, &path("use crate::Circle"), "Disk", &RenameOptions::default()) {
			Err(Error::Unsupported(message)) => {
				assert!(message.starts_with("`use fixture::Circle` is an import, which cannot be renamed: rename what it imports"), "{message}");
			}
			other => panic!("{other:?}"),
		}

		replaced.edits.apply().unwrap();
		cargo_check(&dir);
	}
}
