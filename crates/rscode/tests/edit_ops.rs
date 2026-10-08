//! Planning and applying `remove`, `replace`, `edit_item`, `insert`, `create_module`, `add_imports`, and `format` on
//! crates on disk.
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

/// `text` with `/` replaced by the platform's path separator, which the paths in messages have.
fn native(text: &str) -> String {
	text.replace('/', std::path::MAIN_SEPARATOR_STR)
}

/// Asserts that sorting the library crate in `dir` like `cargo rscode sort` does changes nothing.
#[track_caller]
fn assert_sorted(dir: &TempDir) {
	let ws = load(dir);
	let options = FmtOptions {
		format: FormatOptions::new().formatter(RsFormatter::None).sort(Some(SortOptions::new())),
		..FmtOptions::default()
	};
	let formatting = rscode::edit::format(&Resolver::new(&ws), &[pattern("crate")], &options).unwrap();

	assert!(formatting.edits.is_empty(), "{:?}", changes(dir, &formatting.edits));
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
				[
					"`impl From<u8> for fixture::Wrapper` at src/lib.rs:3:1",
					"`impl From<u16> for fixture::Wrapper` at src/lib.rs:9:1"
				]
				.map(native)
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
				assert_eq!(
					candidates,
					["`<fixture::G<u8>>::get` at src/lib.rs:34:2", "`<fixture::G<u16>>::get` at src/lib.rs:40:2"]
						.map(native)
				);
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

	/// `impl` blocks of one crate with the same header and the same `cfg`s are not `cfg` variants: a path naming them all
	/// is ambiguous for replacements (even with `all_variants`) and removals, which wrote into or deleted every one of
	/// them before.
	#[test]
	fn does_not_take_impl_blocks_with_the_same_header_for_cfg_variants() {
		let lib = "\
pub struct Tools;

#[allow(dead_code)]
impl Tools {
	pub fn add_bots() {}
}

impl Tools {
	pub fn kick() {}
}

#[cfg(unix)]
impl Tools {
	pub fn os() {}
}

#[cfg(not(unix))]
impl Tools {
	pub fn os() {}
}
";
		let dir = TempDir::with_files("remove-same-header-impls", &[("src/lib.rs", lib)]);
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let all_variants = ReplaceOptions { all_variants: true, ..ReplaceOptions::default() };

		assert_eq!(resolver.resolve_item_path(&path("impl crate::Tools")).len(), 4);
		assert!(!rscode::edit::replaces_all_variants(&resolver, &path("impl crate::Tools")));

		let replaced = rscode::edit::replace(&resolver, &path("impl crate::Tools"), "impl Tools {}", &all_variants);

		assert!(matches!(replaced, Err(Error::Ambiguous { .. })), "{replaced:?}");

		match rscode::edit::remove(&resolver, &paths(&["impl crate::Tools"]), &RemoveOptions::default()) {
			Err(Error::Ambiguous { candidates, .. }) => assert_eq!(candidates.len(), 4, "{candidates:?}"),
			other => panic!("{other:?}"),
		}

		// a selector names one of them, as the candidates show
		let candidates = match replaced {
			Err(Error::Ambiguous { candidates, .. }) => candidates,
			other => panic!("{other:?}"),
		};

		assert_eq!(
			candidates,
			[
				"`impl fixture::Tools[add_bots]` (impl) at src/lib.rs:3:1",
				"`impl fixture::Tools[kick]` (impl) at src/lib.rs:8:1",
				"`impl fixture::Tools[3]` (impl) at src/lib.rs:12:1 with #[cfg(unix)]",
				"`impl fixture::Tools[4]` (impl) at src/lib.rs:17:1 with #[cfg(not(unix))]",
			]
			.map(native)
		);

		let source = "impl Tools {}";
		let replacement = rscode::edit::replace(&resolver, &path("impl fixture::Tools[kick]"), source, &ReplaceOptions::default()).unwrap();

		assert_eq!(edited(&dir, &replacement.edits, "src/lib.rs"), lib.replace("impl Tools {\n\tpub fn kick() {}\n}", source));
		assert_eq!(replacement.replaced, ["impl fixture::Tools[kick]"]);

		let removal = remove(&ws, &["impl crate::Tools[#allow]"], &RemoveOptions::default());
		let removed = "#[allow(dead_code)]\nimpl Tools {\n\tpub fn add_bots() {}\n}\n\n";

		assert_eq!(edited(&dir, &removal.edits, "src/lib.rs"), lib.replace(removed, ""));

		// found by paths with any selector, and by patterns (with wildcards) with the selectors that canonical paths show
		let found = |text: &str| -> Vec<u32> {
			let found = rscode::Find::new().pattern(text).unwrap().run_with(&resolver).unwrap();

			found.iter().map(|found| found.start.line as u32).collect()
		};

		assert_eq!(found("impl Tools[kick]"), [8]);
		assert_eq!(found("impl crate::Tools[2]"), [8]);
		assert_eq!(found("impl crate::Tools[#cfg]"), [12, 17]);
		assert_eq!(found("impl *Tools[3]"), [12]);
		assert_eq!(found("impl *Tools[#cfg]"), Vec::<u32>::new());

		// `cfg` variants (of items of such blocks too) still go together
		assert!(rscode::edit::replaces_all_variants(&resolver, &path("crate::Tools::os")));

		let replacement = rscode::edit::replace(&resolver, &path("crate::Tools::os"), "pub fn os() -> u8 { 0 }", &all_variants).unwrap();

		assert_eq!(edited(&dir, &replacement.edits, "src/lib.rs").matches("pub fn os() -> u8 { 0 }").count(), 2);

		let removal = remove(&ws, &["crate::Tools::os"], &RemoveOptions::default());

		assert_eq!(removal.removed.len(), 2);
		assert!(!edited(&dir, &removal.edits, "src/lib.rs").contains("fn os"));
	}

	/// An `impl` block without a `cfg` is compiled with every other block of its crate, so it is no `cfg` variant of a
	/// `#[cfg(test)]` block with the same header: a path naming both is ambiguous, rather than removing the test helpers
	/// with the other block, or replacing both by the same text (which defines its items twice).
	#[test]
	fn does_not_take_an_unconditional_impl_block_for_a_cfg_variant() {
		let lib = "\
pub struct Config;

impl Config {
	pub fn new() -> Self {
		Self
	}
}

#[cfg(test)]
impl Config {
	fn for_tests() -> Self {
		Self
	}
}

#[cfg(unix)]
impl Config {
	pub fn os() {}
}

#[cfg(not(unix))]
impl Config {
	pub fn os() {}
}
";
		let dir = TempDir::with_files("remove-unconditional-impl", &[("src/lib.rs", lib)]);
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let all_variants = ReplaceOptions { all_variants: true, ..ReplaceOptions::default() };

		match rscode::edit::remove(&resolver, &paths(&["impl crate::Config"]), &RemoveOptions::default()) {
			Err(Error::Ambiguous { candidates, .. }) => assert_eq!(
				candidates,
				[
					"`impl fixture::Config[new]` at src/lib.rs:3:1",
					"`impl fixture::Config[for_tests]` at src/lib.rs:9:1",
					"`impl fixture::Config[3]` at src/lib.rs:16:1",
					"`impl fixture::Config[4]` at src/lib.rs:21:1",
				]
				.map(native)
			),
			other => panic!("{other:?}"),
		}

		assert!(!rscode::edit::replaces_all_variants(&resolver, &path("impl crate::Config")));

		let replaced = rscode::edit::replace(&resolver, &path("impl crate::Config"), "impl Config {}", &all_variants);

		assert!(matches!(replaced, Err(Error::Ambiguous { .. })), "{replaced:?}");

		// a selector names one of them
		let removal = rscode::edit::remove(&resolver, &paths(&["impl crate::Config[for_tests]"]), &RemoveOptions::default());

		assert_eq!(removal.unwrap().removed.len(), 1);

		// `cfg` variants still go together
		assert!(rscode::edit::replaces_all_variants(&resolver, &path("crate::Config::os")));
		assert_eq!(remove(&ws, &["crate::Config::os"], &RemoveOptions::default()).removed.len(), 2);
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

		let kept =
			"the directory `src/docs` is not deleted: it has files that are not part of the module `edit_ops::docs`";

		assert_eq!(dir.relative(removal.edits.deletions()), ["src/docs/mod.rs"]);
		assert!(removal.warnings.contains(&native(kept)), "{:?}", removal.warnings);

		// a file another crate loads too
		let removal = remove(&ws, &["crate::util"], &RemoveOptions::default());

		let kept = native("`src/util.rs` is not deleted: the module `edit_ops::util` loads it too");

		assert!(removal.edits.deletions().is_empty());
		assert!(removal.warnings.contains(&kept), "{:?}", removal.warnings);

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
	fn replaces_items_with_source_pasted_from_views() {
		let dir = dir("replace-pasted");
		let ws = load(&dir);
		let expected = LIB.replace("\tleft + right\n", "\tright + left\n");

		// a view's header line and line numbers are no part of the item
		for source in [
			"// fixture::add (fn) src/lib.rs:4-7\n/// Adds.\npub fn add(left: i32, right: i32) -> i32 {\n\tright + left\n}\n",
			"   4 │ /// Adds.\n   5 │ pub fn add(left: i32, right: i32) -> i32 {\n   6 │ \tright + left\n   7 │ }",
			"// fixture::add (fn) src/lib.rs:4-7 [cfg: unix] [inactive]\n   4 │ /// Adds.\n   5 │ pub fn add(left: i32, right: \
			 i32) -> i32 {\n   6 │ \tright + left\n     │\n   7 │ }",
		] {
			let replacement = replace(&ws, "crate::add", source, &ReplaceOptions::default()).unwrap();
			let edited = edited(&dir, &replacement.edits, "src/lib.rs");

			assert_eq!(edited.replace("\t\n", "\n").replace("right + left\n\n}", "right + left\n}"), expected, "{source}");
		}

		// comments that merely look like one stay
		for comment in ["// add (fn)", "// Adapted from serde (MIT) src/de.rs:120-140", "// see a::b (fnord) x.rs:1"] {
			let source = format!("{comment}\n/// Adds.\npub fn add(left: i32, right: i32) -> i32 {{\n\tright + left\n}}\n");
			let replacement = replace(&ws, "crate::add", &source, &ReplaceOptions::default()).unwrap();

			assert!(edited(&dir, &replacement.edits, "src/lib.rs").contains(&format!("{comment}\n/// Adds.")), "{comment}");
		}

		// a view of several items has a header above each
		let source = "// fixture::S (struct) src/lib.rs:9\npub struct S;\n\n// <fixture::S>::m (assoc-fn) src/lib.rs:12-15\n\
		              pub struct T;\n";
		let options = InsertOptions::default();
		let insertion = rscode::edit::insert(&Resolver::new(&ws), Some(&path("crate::out")), source, &options).unwrap();

		assert_eq!(edited(&dir, &insertion.edits, "src/out.rs"), "pub fn o() {}\n\npub struct S;\n\npub struct T;\n");
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
					.map(native)
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
		let kept =
			"only the declaration of the module `fixture::out` is replaced; its file `src/out.rs` is left as it is";

		assert_eq!(replacement.warnings, [native(kept)]);
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

mod edit_item {
	use super::*;
	use rscode::View;
	use rscode::ViewMode;
	use rscode::edit::EditItemOptions;
	use rscode::edit::ItemEdit;
	use rscode::edit::ItemSpan;
	use rscode::edit::Replacement;
	use rscode::edit::TextReplacement;

	const INNER: &str = "//! Inner docs.\n\npub fn f() {}\n";

	const LIB: &str = "\
//! The fixture.

use std::fmt;

/// Shapes.
#[derive(Debug, Clone)]
pub struct Shape {
	/// The sides.
	pub sides: u8,
}

impl Shape {
	/// A triangle.
	pub fn triangle() -> Self {
		let sides = 3;

		Self { sides }
	}

	#[allow(dead_code)] #[allow(unused)]
	fn name(&self) -> &'static str {
		\"a shape
  with sides\"
	}
}

impl fmt::Display for Shape {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, \"{}\", self.name())
	}
}

#[cfg(feature = \"a\")]
pub fn variant() -> u8 {
	1
}

#[cfg(not(feature = \"a\"))]
pub fn variant() -> u8 {
	2
}

pub enum Kind {
	A,
	B,
}

mod inner;
";

	/// A crate with [`LIB`] as `lib` and [`INNER`] as its module `inner`.
	fn crate_dir(name: &str, lib: &str) -> TempDir {
		TempDir::with_files(name, &[("src/lib.rs", lib), ("src/inner.rs", INNER)])
	}

	fn edit(ws: &Workspace, target: &str, edit: ItemEdit) -> Result<Replacement, Error> {
		edit_with(ws, target, edit, &EditItemOptions::default())
	}

	fn edit_with(ws: &Workspace, target: &str, edit: ItemEdit, options: &EditItemOptions) -> Result<Replacement, Error> {
		rscode::edit::edit_item(&Resolver::new(ws), &path(target), &edit, options)
	}

	/// The message of an error that only says something.
	fn message(result: Result<Replacement, Error>) -> String {
		match result {
			Err(Error::InvalidSource(message) | Error::Unsupported(message) | Error::TextMismatch { message, .. }) => message,
			other => panic!("{other:?}"),
		}
	}

	/// An edit that replaces `old` with `new`.
	fn text(old: &str, new: &str) -> ItemEdit {
		texts(&[(old, new)])
	}

	/// An edit that replaces texts, in order.
	fn texts(replacements: &[(&str, &str)]) -> ItemEdit {
		ItemEdit {
			replacements: (replacements.iter())
				.map(|(old, new)| TextReplacement {
					old: (*old).to_owned(),
					new: (*new).to_owned(),
				})
				.collect(),
			..ItemEdit::default()
		}
	}

	/// The text of an item as views show it in full.
	fn view(ws: &Workspace, target: &str, line_numbers: bool) -> String {
		let views = View::new().mode(ViewMode::Full).line_numbers(line_numbers).item_path(path(target)).run(ws).unwrap();

		views[0].text.clone()
	}

	#[test]
	fn applies_item_edits_and_still_compiles() {
		let dir = TempDir::fixture("edit-item-apply");
		let ws = load_fixture(&dir, true);
		let resolver = Resolver::new(&ws);
		let options = EditItemOptions::default();
		let mut edits = EditSet::new();
		let mut plan = |target: &str, edit: ItemEdit| {
			edits.extend(rscode::edit::edit_item(&resolver, &path(target), &edit, &options).unwrap().edits);
		};

		plan(
			"crate::shapes::Circle::new",
			text("let radius = Radius(radius);\n\n\tSelf { radius }", "Self {\n\t\tradius: Radius(radius.abs()),\n\t}"),
		);
		plan(
			"crate::shapes::Circle",
			ItemEdit {
				add_attributes: vec!["must_use".to_owned()],
				doc: Some("A circle,\nround.".to_owned()),
				..ItemEdit::default()
			},
		);
		plan("crate::extra", text("\"plain\"", "\"simple\""));
		plan(
			"crate::unit",
			ItemEdit {
				visibility: Some("pub(crate)".to_owned()),
				remove_attributes: vec![],
				..ItemEdit::default()
			},
		);
		plan(
			"crate::shapes",
			ItemEdit {
				doc: Some("Shapes, round ones.".to_owned()),
				..ItemEdit::default()
			},
		);

		let applied = edits.apply().unwrap();
		let shapes = dir.read("src/shapes.rs");

		assert_eq!(dir.relative(&applied.written), ["src/lib.rs", "src/shapes.rs"]);
		assert!(shapes.starts_with("//! Shapes, round ones.\n\npub mod round;"), "{shapes}");
		assert!(shapes.contains("\t\tSelf {\n\t\t\tradius: Radius(radius.abs()),\n\t\t}\n\t}"), "{shapes}");
		assert!(shapes.contains("/// A circle,\n/// round.\n#[derive(Debug, Clone, Copy)]\n#[must_use]\npub struct"), "{shapes}");
		assert!(dir.read("src/lib.rs").contains("\"simple\"") && dir.read("src/lib.rs").contains("pub(crate) fn unit()"));
		cargo_check(&dir);
	}

	#[test]
	fn changes_attributes() {
		let dir = crate_dir("edit-attributes", LIB);
		let ws = load(&dir);
		let attributes = |target: &str, add: &[&str], remove: &[&str]| {
			edit(
				&ws,
				target,
				ItemEdit {
					add_attributes: add.iter().map(|text| (*text).to_owned()).collect(),
					remove_attributes: remove.iter().map(|text| (*text).to_owned()).collect(),
					..ItemEdit::default()
				},
			)
		};
		let edited = |result: Result<Replacement, Error>, file: &str| edited(&dir, &result.unwrap().edits, file);

		// removed by path or by exact text, with their line when nothing else is on it
		assert_eq!(edited(attributes("crate::Shape", &[], &["derive"]), "src/lib.rs"), LIB.replace("#[derive(Debug, Clone)]\n", ""));
		assert_eq!(
			edited(attributes("crate::Shape::name", &[], &["#[allow( unused )]"]), "src/lib.rs"),
			LIB.replace(" #[allow(unused)]", "")
		);
		assert_eq!(
			edited(attributes("crate::Shape::name", &[], &["allow(dead_code)"]), "src/lib.rs"),
			LIB.replace("#[allow(dead_code)] ", "")
		);
		assert_eq!(
			message(attributes("crate::Shape::name", &[], &["allow"])),
			"`allow` is the path of 2 attributes of `fixture::Shape::name` (`#[allow(dead_code)]`, `#[allow(unused)]`): \
			 give the exact text of the one to remove"
		);
		assert_eq!(
			message(attributes("crate::Shape", &[], &["inline"])),
			"`fixture::Shape` has no attribute `inline` (it has `#[derive(Debug, Clone)]`)"
		);

		// added after the others, on a line of their own (removing first)
		assert_eq!(
			edited(attributes("crate::Shape", &["derive(PartialEq)"], &["derive"]), "src/lib.rs"),
			LIB.replace("#[derive(Debug, Clone)]\n", "#[derive(PartialEq)]\n")
		);
		assert_eq!(
			edited(attributes("crate::Shape::triangle", &["#[must_use]"], &[]), "src/lib.rs"),
			LIB.replace("\t/// A triangle.\n", "\t/// A triangle.\n\t#[must_use]\n")
		);
		assert_eq!(
			edited(attributes("crate::Shape::name", &["inline"], &[]), "src/lib.rs"),
			LIB.replace("#[allow(unused)]\n", "#[allow(unused)]\n\t#[inline]\n")
		);
		assert_eq!(
			message(attributes("crate::Shape::name", &["allow(unused)"], &[])),
			"`fixture::Shape::name` already has the attribute `#[allow(unused)]`"
		);

		// the traits of a `derive` join the item's `derive`, but those that it derives already
		let plan = attributes("crate::Shape", &["derive(Clone, PartialEq)"], &[]).unwrap();

		assert_eq!(super::edited(&dir, &plan.edits, "src/lib.rs"), LIB.replace("(Debug, Clone)", "(Debug, Clone, PartialEq)"));
		assert_eq!(plan.notes, ["`fixture::Shape` already derives `Clone`"]);

		let plan = attributes("crate::Shape", &["#[derive(Debug)]"], &[]).unwrap();

		assert!(plan.spans.is_empty() && changes(&dir, &plan.edits).is_empty());
		assert_eq!(plan.notes, ["`fixture::Shape` already derives `Debug`"]);

		// inner attributes of crate roots and module files
		assert_eq!(
			edited(attributes("crate", &["allow(dead_code)"], &[]), "src/lib.rs"),
			LIB.replace("//! The fixture.\n", "//! The fixture.\n#![allow(dead_code)]\n")
		);
		assert_eq!(
			edited(attributes("crate::inner", &["#![allow(dead_code)]", "cfg(test)"], &[]), "src/inner.rs"),
			"//! Inner docs.\n#![allow(dead_code)]\n\npub fn f() {}\n"
		);
		assert!(message(attributes("crate::Shape", &["#![allow(dead_code)]"], &[])).contains("is an inner attribute"));

		// removing the inner attributes between blank lines removes a blank line with them
		let lib = LIB.replace("//! The fixture.\n", "//! The fixture.\n\n#![allow(dead_code)]\n");
		let dir = crate_dir("edit-inner-attributes", &lib);
		let ws = load(&dir);
		let plan = edit(
			&ws,
			"crate",
			ItemEdit {
				remove_attributes: vec!["allow".to_owned()],
				..ItemEdit::default()
			},
		)
		.unwrap();

		assert_eq!(super::edited(&dir, &plan.edits, "src/lib.rs"), LIB);
	}

	#[test]
	fn changes_doc_comments() {
		let dir = crate_dir("edit-docs", LIB);
		let ws = load(&dir);
		let doc = |target: &str, doc: &str| {
			edit(
				&ws,
				target,
				ItemEdit {
					doc: Some(doc.to_owned()),
					..ItemEdit::default()
				},
			)
			.unwrap()
		};
		let edited = |plan: Replacement, file: &str| edited(&dir, &plan.edits, file);

		assert_eq!(
			edited(doc("crate::Shape", "A shape.\n\nWith sides."), "src/lib.rs"),
			LIB.replace("/// Shapes.\n", "/// A shape.\n///\n/// With sides.\n")
		);
		assert_eq!(edited(doc("crate::Shape", ""), "src/lib.rs"), LIB.replace("/// Shapes.\n", ""));
		assert_eq!(edited(doc("crate::Kind", "/// Kinds."), "src/lib.rs"), LIB.replace("pub enum Kind", "/// Kinds.\npub enum Kind"));
		assert_eq!(edited(doc("crate::Kind::A", "The first."), "src/lib.rs"), LIB.replace("\tA,", "\t/// The first.\n\tA,"));

		// the lines of an item that new docs start
		let span = |plan: Replacement| (plan.spans[0].start, plan.spans[0].end);

		assert_eq!(span(doc("crate::Kind", "Kinds,\nof shapes.")), (43, 48));
		assert_eq!(span(doc("crate::Kind::A", "The first.")), (44, 45));

		// the docs of a module with a file are its inner docs
		assert_eq!(edited(doc("crate::inner", "Inner, changed."), "src/inner.rs"), INNER.replace("Inner docs.", "Inner, changed."));
		assert_eq!(edited(doc("crate::inner", ""), "src/inner.rs"), "pub fn f() {}\n");
		assert_eq!(edited(doc("crate", "The crate."), "src/lib.rs"), LIB.replace("The fixture.", "The crate."));

		// the same docs again change nothing
		let plan = doc("crate::Shape", "Shapes.");

		assert!(plan.replaced.is_empty() && plan.spans.is_empty());
		assert_eq!(plan.notes, ["`fixture::Shape` already has this doc comment"]);
		assert!(changes(&dir, &plan.edits).is_empty());
	}

	#[test]
	fn changes_visibilities() {
		let dir = crate_dir("edit-visibility", LIB);
		let ws = load(&dir);
		let visibility = |target: &str, visibility: &str| {
			edit(
				&ws,
				target,
				ItemEdit {
					visibility: Some(visibility.to_owned()),
					..ItemEdit::default()
				},
			)
		};
		let edited = |result: Result<Replacement, Error>, file: &str| edited(&dir, &result.unwrap().edits, file);

		assert_eq!(
			edited(visibility("crate::Shape::triangle", "pub(crate)"), "src/lib.rs"),
			LIB.replace("pub fn triangle", "pub(crate) fn triangle")
		);
		assert_eq!(edited(visibility("crate::Shape::name", "pub"), "src/lib.rs"), LIB.replace("\tfn name", "\tpub fn name"));
		assert_eq!(edited(visibility("crate::Shape", "private"), "src/lib.rs"), LIB.replace("pub struct Shape", "struct Shape"));
		assert_eq!(edited(visibility("crate::inner", "crate"), "src/lib.rs"), LIB.replace("mod inner;", "pub(crate) mod inner;"));
		assert_eq!(edited(visibility("use crate::fmt", "pub"), "src/lib.rs"), LIB.replace("use std::fmt;", "pub use std::fmt;"));

		let unchanged = visibility("crate::Shape", " pub ").unwrap();

		assert_eq!(unchanged.notes, ["`fixture::Shape` is pub already"]);
		assert!(changes(&dir, &unchanged.edits).is_empty());

		assert_eq!(
			message(visibility("crate::Kind::A", "pub")),
			"cannot change the visibility of `fixture::Kind::A`: it is a variant, which has the visibility of its parent"
		);
		assert!(message(visibility("<crate::Shape as std::fmt::Display>::fmt", "pub")).contains("visibility of its parent"));
		assert!(message(visibility("impl crate::Shape", "pub")).ends_with("it is an impl, which has no visibility"));
		assert!(message(visibility("crate", "pub")).ends_with("it is a crate root, which has no visibility"));
		assert!(message(visibility("crate::Shape", "public")).starts_with("`public` is not a visibility"));
	}

	#[test]
	fn edits_the_inner_attributes_of_inline_modules() {
		let lib = "pub mod a {\n\t//! Inner docs.\n\t#![allow(unused)]\n\n\tpub fn f() {}\n}\n\npub mod b {\n\tpub fn g() \
		 {}\n}\n\npub mod c { pub fn h() {} }\n";
		let dir = crate_dir("edit-inline-modules", lib);
		let ws = load(&dir);
		let edited = |target: &str, change: ItemEdit| {
			let plan = edit(&ws, target, change).unwrap();

			super::edited(&dir, &plan.edits, "src/lib.rs")
		};
		let doc = |doc: &str| ItemEdit {
			doc: Some(doc.to_owned()),
			..ItemEdit::default()
		};
		let attributes = |add: &str, remove: &str| ItemEdit {
			add_attributes: [add].into_iter().filter(|text| !text.is_empty()).map(str::to_owned).collect(),
			remove_attributes: [remove].into_iter().filter(|text| !text.is_empty()).map(str::to_owned).collect(),
			..ItemEdit::default()
		};

		// the inner docs of a module that has some (else outer ones)
		assert_eq!(edited("crate::a", doc("New.")), lib.replace("Inner docs.", "New."));
		assert_eq!(edited("crate::a", doc("")), lib.replace("\t//! Inner docs.\n", ""));
		assert_eq!(edited("crate::b", doc("B.")), lib.replace("pub mod b", "/// B.\npub mod b"));

		// inner attributes after the others, or first in the body
		assert_eq!(edited("crate::a", attributes("", "allow")), lib.replace("\t#![allow(unused)]\n", ""));
		assert_eq!(
			edited("crate::a", attributes("#![allow(dead_code)]", "")),
			lib.replace("(unused)]\n", "(unused)]\n\t#![allow(dead_code)]\n")
		);

		let with_attribute = lib.replace("pub mod b {\n", "pub mod b {\n\t#![allow(dead_code)]\n\n");

		assert_eq!(edited("crate::b", attributes("#![allow(dead_code)]", "")), with_attribute);
		assert_eq!(
			edited("crate::c", attributes("#![allow(dead_code)]", "")),
			lib.replace("c { ", "c { #![allow(dead_code)] ")
		);

		// removing the first lines of a body removes the blank line after them
		let dir = crate_dir("edit-inline-modules-removal", &with_attribute);
		let ws = load(&dir);
		let plan = edit(&ws, "crate::b", attributes("", "#![allow(dead_code)]")).unwrap();

		assert_eq!(super::edited(&dir, &plan.edits, "src/lib.rs"), lib);
	}

	#[test]
	fn edits_the_files_of_modules_and_crates_and_imports() {
		let dir = crate_dir("edit-files", LIB);
		let ws = load(&dir);
		let plan = edit(&ws, "crate::inner", text("pub fn f() {}", "pub fn g() {}")).unwrap();

		assert_eq!(edited(&dir, &plan.edits, "src/inner.rs"), INNER.replace("fn f", "fn g"));

		// text copied from the start of its view, with the `// file:` line that is not in the file
		for line_numbers in [false, true] {
			let old = view(&ws, "crate::inner", line_numbers).lines().take(2).collect::<Vec<_>>().join("\n");
			let plan = edit(&ws, "crate::inner", text(&old, &old.replace("Inner docs.", "Better docs."))).unwrap();

			assert!(old.contains("// file: src"), "{old}");
			assert_eq!(edited(&dir, &plan.edits, "src/inner.rs"), INNER.replace("Inner docs.", "Better docs."));
		}
		assert_eq!(
			plan.spans,
			[ItemSpan {
				path: "fixture::inner".to_owned(),
				file: dir.path("src/inner.rs"),
				start: 1,
				end: 3,
			}]
		);

		let plan = edit(&ws, "crate", text("mod inner;", "pub mod inner;")).unwrap();

		assert_eq!(edited(&dir, &plan.edits, "src/lib.rs"), LIB.replace("mod inner;", "pub mod inner;"));

		// an import's `use` item
		let plan = edit(&ws, "use crate::fmt", text("std::fmt", "core::fmt")).unwrap();

		assert_eq!(edited(&dir, &plan.edits, "src/lib.rs"), LIB.replace("std::fmt", "core::fmt"));
		assert_eq!(plan.replaced, ["use fixture::fmt"]);
	}

	#[test]
	fn edits_files_with_crlf_line_breaks() {
		let lib = LIB.replace('\n', "\r\n");
		let dir = crate_dir("edit-crlf", &lib);
		let ws = load(&dir);
		let viewed = view(&ws, "crate::Shape::triangle", false);
		let new = viewed.replace("let sides = 3;\n", "let sides = 3;\n\tlet more = sides;\n");
		let plan = edit(&ws, "crate::Shape::triangle", text(&viewed, &new)).unwrap();

		assert_eq!(
			edited(&dir, &plan.edits, "src/lib.rs"),
			lib.replace("let sides = 3;\r\n", "let sides = 3;\r\n\t\tlet more = sides;\r\n")
		);

		let plan = edit(
			&ws,
			"crate::Kind",
			ItemEdit {
				doc: Some("Kinds\nof shapes.".to_owned()),
				add_attributes: vec!["derive(Debug)".to_owned()],
				..ItemEdit::default()
			},
		)
		.unwrap();

		assert_eq!(
			edited(&dir, &plan.edits, "src/lib.rs"),
			lib.replace("pub enum Kind", "/// Kinds\r\n/// of shapes.\r\n#[derive(Debug)]\r\npub enum Kind")
		);
	}

	#[test]
	fn picks_the_cfg_variant_with_the_text() {
		let dir = crate_dir("edit-variants", LIB);
		let ws = load(&dir);
		let plan = edit(&ws, "crate::variant", text("2", "20")).unwrap();

		assert_eq!(edited(&dir, &plan.edits, "src/lib.rs"), LIB.replace("\t2\n", "\t20\n"));
		assert_eq!(
			plan.notes,
			[native(
				"of the 2 items that `crate::variant` names, only `fixture::variant` (fn) at src/lib.rs:38:1 with \
				 #[cfg(not(feature = \"a\"))] has the text to replace"
			)]
		);
		assert!(matches!(edit(&ws, "crate::variant", text("-> u8", "-> u16")), Err(Error::Ambiguous { .. })));
		assert_eq!(
			message(edit(&ws, "crate::variant", text("3", "30"))),
			"`old` not found in any of the 2 items that `crate::variant` names"
		);
		assert_eq!(
			message(edit(&ws, "crate::variant", text("pub fn variant() -> u16 {", ""))),
			native(
				"`old` not found in any of the 2 items that `crate::variant` names; the closest is in `fixture::variant` \
				 (src/lib.rs:33-36); the closest line is 34: `pub fn variant() -> u8 {`"
			)
		);

		// the replacement that fits none of them, or the one that the replacements got furthest in
		assert_eq!(
			message(edit(&ws, "crate::variant", texts(&[("-> u8", "-> u16"), ("zzz", "y")]))),
			"`old` number 2 (`zzz`) not found in any of the 2 items that `crate::variant` names"
		);
		assert_eq!(
			message(edit(&ws, "crate::variant", texts(&[("2", "20"), ("zzz", "y")]))),
			native("`old` number 2 (`zzz`) not found in `fixture::variant` (src/lib.rs:38-41)")
		);

		let several_dir = crate_dir("edit-variants-several", &LIB.replace("\t2\n", "\t2 * 2\n"));
		let several = load(&several_dir);

		assert_eq!(
			message(edit(&several, "crate::variant", text("2", "3"))),
			native("`old` occurs 2 times in `fixture::variant` (src/lib.rs:38-41), at line 40")
		);

		let all = EditItemOptions {
			all_variants: true,
			..EditItemOptions::default()
		};
		let plan = edit_with(&ws, "crate::variant", text("-> u8", "-> u16"), &all).unwrap();

		assert_eq!(edited(&dir, &plan.edits, "src/lib.rs"), LIB.replace("-> u8", "-> u16"));
		assert_eq!(plan.warnings, ["edited 2 `cfg` variants of `crate::variant`"]);
		assert_eq!(plan.spans.len(), 2);

		let visibility = ItemEdit {
			visibility: Some("pub(crate)".to_owned()),
			..ItemEdit::default()
		};

		assert!(matches!(edit(&ws, "crate::variant", visibility.clone()), Err(Error::Ambiguous { .. })));
		assert!(edit_with(&ws, "crate::variant", visibility, &all).is_ok());
	}

	#[test]
	fn refuses_edits_that_change_the_item() {
		let dir = crate_dir("edit-refusals", LIB);
		let ws = load(&dir);
		let method = "pub fn triangle() -> Self {\n\tlet sides = 3;\n\n\tSelf { sides }\n}";
		let kind_change = |new: &str| match edit(&ws, "crate::Shape::triangle", text(method, new)) {
			Err(Error::KindChange(message)) => message,
			other => panic!("{other:?}"),
		};

		assert_eq!(
			kind_change("pub const TRIANGLE: u8 = 3;"),
			native(
				"after the edit, `fixture::Shape::triangle` (at line 13 of src/lib.rs) is an assoc-const rather than an \
				 assoc-fn"
			)
		);
		assert_eq!(
			kind_change("fn a() {}\n\nfn b() {}"),
			native("after the edit, `fixture::Shape::triangle` (at line 13 of src/lib.rs) is 2 items")
		);
		assert!(
			message(edit(&ws, "crate::Shape::triangle", text("{ sides }", "{ sides"))).starts_with(&native(
				"after the edit, `fixture::Shape::triangle` (at line 13 of src/lib.rs) does not parse as associated items \
				 of an `impl` block at "
			))
		);

		let kind_change = EditItemOptions {
			allow_kind_change: true,
			..EditItemOptions::default()
		};

		assert!(edit_with(&ws, "crate::Shape::triangle", text(method, "pub const TRIANGLE: u8 = 3;"), &kind_change).is_ok());

		// a new name is only a warning
		let plan = edit(&ws, "crate::Shape::triangle", text("fn triangle", "fn trigon")).unwrap();

		assert_eq!(
			plan.warnings,
			["the replacement of `fixture::Shape::triangle` does not define `triangle`; references to it are not updated"]
		);
		assert!(message(edit(&ws, "crate::Shape", ItemEdit::default())).starts_with("nothing to change"));
		assert!(matches!(edit(&ws, "crate::nope", text("a", "b")), Err(Error::NotFound(_))));
	}

	#[test]
	fn replaces_text_as_written_or_as_views_print_it() {
		let dir = crate_dir("edit-text", LIB);
		let ws = load(&dir);
		let triangle = "crate::Shape::triangle";

		// as written
		let plan = edit(&ws, triangle, text("let sides = 3;", "let sides = 4;")).unwrap();

		assert_eq!(edited(&dir, &plan.edits, "src/lib.rs"), LIB.replace("= 3;", "= 4;"));
		assert_eq!(plan.replaced, ["fixture::Shape::triangle"]);
		assert_eq!(
			plan.spans,
			[ItemSpan {
				path: "fixture::Shape::triangle".to_owned(),
				file: dir.path("src/lib.rs"),
				start: 13,
				end: 18,
			}]
		);

		// (text without line breaks is also printed there: lines after the first of `new` are indented as in views)
		let plan = edit(&ws, triangle, text("let sides = 3;", "let sides = 3;\n\tlet more = sides;")).unwrap();

		assert_eq!(
			edited(&dir, &plan.edits, "src/lib.rs"),
			LIB.replace("let sides = 3;\n", "let sides = 3;\n\t\tlet more = sides;\n")
		);

		// as views print it: dedented
		let viewed = view(&ws, triangle, false);

		assert!(viewed.contains("\tlet sides = 3;\n\n\tSelf { sides }\n}"), "{viewed}");

		let plan = edit(&ws, triangle, text("\tlet sides = 3;\n\n\tSelf { sides }", "\tSelf {\n\t\tsides: 3,\n\t}")).unwrap();

		assert_eq!(
			edited(&dir, &plan.edits, "src/lib.rs"),
			LIB.replace("\t\tlet sides = 3;\n\n\t\tSelf { sides }", "\t\tSelf {\n\t\t\tsides: 3,\n\t\t}")
		);
		assert_eq!((plan.spans[0].start, plan.spans[0].end), (13, 18));

		// with the line numbers of numbered views
		let numbered = view(&ws, triangle, true);
		let old = numbered.lines().skip(2).take(3).collect::<Vec<_>>().join("\n");

		assert!(old.contains(" │ \tlet sides = 3;"), "{old}");

		let plan = edit(&ws, triangle, text(&old, &old.replace("= 3", "= 4"))).unwrap();

		assert_eq!(edited(&dir, &plan.edits, "src/lib.rs"), LIB.replace("= 3;", "= 4;"));

		// which of several occurrences, by the line numbers
		let plan = edit(&ws, triangle, text("  18 │ }", "  18 │ } // triangle")).unwrap();

		assert_eq!(edited(&dir, &plan.edits, "src/lib.rs"), LIB.replace("{ sides }\n\t}", "{ sides }\n\t} // triangle"));
		assert!(matches!(edit(&ws, triangle, text("}", "")), Err(Error::TextMismatch { lines, .. }) if lines == [17, 18]));

		// a whole view (with a line break after it), whose string keeps its continuation line
		let viewed = view(&ws, "crate::Shape::name", false) + "\n";
		let plan = edit(&ws, "crate::Shape::name", text(&viewed, &viewed.replace("with sides", "with corners"))).unwrap();

		assert_eq!(edited(&dir, &plan.edits, "src/lib.rs"), LIB.replace("with sides", "with corners"));

		// replacements in order, each in the text the ones before left
		let plan = edit(&ws, triangle, texts(&[("3", "4"), ("4;", "5;")])).unwrap();

		assert_eq!(edited(&dir, &plan.edits, "src/lib.rs"), LIB.replace("= 3;", "= 5;"));
	}

	#[test]
	fn tells_where_text_occurs_or_what_comes_closest() {
		let dir = crate_dir("edit-mismatch", LIB);
		let ws = load(&dir);
		let mismatch = |old: &str| match edit(&ws, "crate::Shape::triangle", text(old, "x")) {
			Err(Error::TextMismatch { message, lines }) => (message, lines),
			other => panic!("{other:?}"),
		};
		let triangle = native("`fixture::Shape::triangle` (src/lib.rs:13-18)");

		assert_eq!(mismatch("sides"), (format!("`old` occurs 2 times in {triangle}, at lines 15, 17"), vec![15, 17]));
		assert_eq!(
			mismatch("let sides = 3;\n\n    Self { sides }").0,
			format!(
				"`old` not found in {triangle}; it matches at line 15 if indentation is ignored (the file indents with \
				 tabs, `old` with spaces)"
			)
		);
		assert_eq!(mismatch("let sides = 6;").0, format!("`old` not found in {triangle}; the closest line is 15: `let sides = 3;`"));
		assert_eq!(
			mismatch("let sides = 3;\nSelf").0,
			format!("`old` not found in {triangle}; its first line is at line 15, but what follows differs")
		);
		assert_eq!(
			message(edit(&ws, "crate::Shape::triangle", texts(&[("3", "4"), ("3", "5")]))),
			format!("`old` number 2 (`3`) not found in {triangle}")
		);

		// whitespace at the end of a line, which views do not show
		let dir = crate_dir("edit-mismatch-ends", &LIB.replace("let sides = 3;\n", "let sides = 3; \n"));
		let ws = load(&dir);

		assert_eq!(
			message(edit(&ws, "crate::Shape::triangle", text("let sides = 3;\n\n\tSelf { sides }", "x"))),
			format!(
				"`old` not found in {triangle}; it matches at line 15 if whitespace at the ends of lines is ignored (line \
				 15 ends with whitespace that `old` lacks)"
			)
		);
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
		rscode::edit::insert(&Resolver::new(ws), Some(&path(parent)), source, options)
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
				assert_eq!(collisions, [native("`a`: `fixture::util::a` (fn) at src/lib.rs:6:2")]);
			}
			other => panic!("{other:?}"),
		}

		// imports, and several names
		match insert(&ws, "crate::util", "pub mod fmt {}\npub fn b() {}", &InsertOptions::default()) {
			Err(error @ Error::Collision { .. }) => assert_eq!(
				error.to_string(),
				native(
					"`fmt`, `b` collides with existing names:\n`fmt`: the import of `std::fmt` at src/lib.rs:4:6\n`b`: \
					 `fixture::util::b` (fn) at src/lib.rs:8:2"
				)
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

		let collision = native("inserted `a`, which collides with `fixture::util::a` (fn) at src/lib.rs:6:2");

		assert_eq!(insertion.warnings, [collision]);
	}

	#[test]
	fn refuses_colliding_imports() {
		let dir = dir("insert-imports");
		let ws = load(&dir);

		// in the namespaces their paths resolve in
		match insert(&ws, "crate::util", "use std::fmt;", &InsertOptions::default()) {
			Err(Error::Collision { collisions, .. }) => {
				assert_eq!(collisions, [native("`fmt`: the import of `std::fmt` at src/lib.rs:4:6")])
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

		// the candidates' paths tell the blocks apart by a selector
		match insert(&ws, "<crate::S>", "pub fn c() {}", &InsertOptions::default()) {
			Err(Error::Ambiguous { candidates, .. }) => {
				assert_eq!(
					candidates,
					["`impl fixture::S[a]` (impl) at src/lib.rs:3:1", "`impl fixture::S[b]` (impl) at src/lib.rs:7:1"]
						.map(native)
				)
			}
			other => panic!("{other:?}"),
		}

		let insertion =
			insert(&ws, "<crate::S>", "pub fn c() {}", &at_position(InsertPosition::After("b".to_owned()))).unwrap();

		assert!(edited(&dir, &insertion.edits, "src/lib.rs").ends_with("\tpub fn b() {}\n\n\tpub fn c() {}\n}\n"));

		for parent in ["impl fixture::S[b]", "impl crate::S[2]"] {
			let insertion = insert(&ws, parent, "pub fn c() {}", &InsertOptions::default()).unwrap();

			assert!(edited(&dir, &insertion.edits, "src/lib.rs").ends_with("\tpub fn b() {}\n\n\tpub fn c() {}\n}\n"));
		}
	}

	/// Without a parent, the container of the anchor is the parent.
	#[test]
	fn infers_the_parent_from_the_anchor() {
		let lib = "\
pub struct S;

impl S {
	pub fn a() {}

	#[cfg(unix)]
	pub fn x() {}
}

impl S {
	pub fn b() {}

	#[cfg(not(unix))]
	pub fn x() {}
}

pub mod m {
	pub fn f() {}
}
";
		let dir = TempDir::with_files("insert-inferred-parent", &[("src/lib.rs", lib)]);
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let insert = |position: InsertPosition| rscode::edit::insert(&resolver, None, "pub fn c() {}", &at_position(position));

		let insertion = insert(InsertPosition::After("crate::S::b".to_owned())).unwrap();

		assert_eq!(insertion.parent, "impl fixture::S[b]");
		assert_eq!(edited(&dir, &insertion.edits, "src/lib.rs"), lib.replace("pub fn b() {}\n", "pub fn b() {}\n\n\tpub fn c() {}\n"));

		let insertion = insert(InsertPosition::Before("m::f".to_owned())).unwrap();

		assert_eq!(insertion.parent, "fixture::m");
		assert_eq!(edited(&dir, &insertion.edits, "src/lib.rs"), lib.replace("\tpub fn f", "\tpub fn c() {}\n\n\tpub fn f"));

		// the anchor must name items of one container
		match insert(InsertPosition::After("crate::S::x".to_owned())) {
			Err(Error::Ambiguous { path, candidates }) => {
				assert_eq!(path, "crate::S::x");
				assert_eq!(
					candidates,
					["`impl fixture::S[a]` (impl) at src/lib.rs:3:1", "`impl fixture::S[b]` (impl) at src/lib.rs:10:1"].map(native)
				);
			}
			other => panic!("{other:?}"),
		}

		assert!(matches!(insert(InsertPosition::After("crate::nope".to_owned())), Err(Error::NotFound(path)) if path == "crate::nope"));
		assert!(matches!(insert(InsertPosition::After("crate".to_owned())), Err(Error::Unsupported(_))));
		assert!(matches!(insert(InsertPosition::End), Err(Error::Unsupported(_))));
	}

	/// A crate root laid out like `cargo rscode sort` lays it out, and a module whose `mod` declarations are separated
	/// by blank lines.
	const COMPACT: &[(&str, &str)] = &[
		(
			"src/lib.rs",
			"\
//! Docs.

extern crate alloc;

mod a;
mod c;
mod spaced;

use std::fmt;
use std::io;

pub use a::A;

const X: u8 = 1;

/// Documented.
const Y: u8 = 2;

pub struct S;

impl S {
	const A: u8 = 1;

	fn f() {}
}

mod tail {
	use std::fmt;
}
",
		),
		("src/a.rs", "pub struct A;\n\npub struct B;\n"),
		("src/c.rs", ""),
		("src/spaced.rs", "mod a;\n\nmod c;\n"),
		("src/spaced/a.rs", ""),
		("src/spaced/c.rs", ""),
	];

	/// One-line items without attributes join one-line siblings of their group without blank lines, as sorting lays
	/// them out, unless blank lines separate those siblings already. Other items get blank lines.
	#[test]
	fn joins_one_line_siblings_of_their_group() {
		let dir = TempDir::with_files("insert-compact", COMPACT);
		let ws = load(&dir);
		let lib = COMPACT[0].1;
		let inserted = |parent: &str, source: &str, position: InsertPosition, file: &str| {
			let insertion = insert(&ws, parent, source, &at_position(position)).unwrap();

			edited(&dir, &insertion.edits, file)
		};
		let after = |anchor: &str| InsertPosition::After(anchor.to_owned());
		let before = |anchor: &str| InsertPosition::Before(anchor.to_owned());

		let sorted_with = |old: &str, new: &str| {
			let text = lib.replace(old, new);

			// sorting changes nothing
			assert_eq!(rscode::rscode_sort::sort_str(&text).unwrap(), text);
			text
		};

		let cases = [
			("crate", "mod b;", after("crate::a"), "mod a;\nmod c;", "mod a;\nmod b;\nmod c;"),
			("crate", "mod b;", before("crate::c"), "mod a;\nmod c;", "mod a;\nmod b;\nmod c;"),
			("crate", "mod t;\nmod u;", after("crate::spaced"), "mod spaced;\n", "mod spaced;\nmod t;\nmod u;\n"),
			("crate", "use std::env;", before("use crate::fmt"), "\nuse std::fmt;", "\nuse std::env;\nuse std::fmt;"),
			("crate", "pub use a::B;", after("use crate::A"), "pub use a::A;\n", "pub use a::A;\npub use a::B;\n"),
			("crate", "const XX: u8 = 3;", after("crate::X"), "X: u8 = 1;\n", "X: u8 = 1;\nconst XX: u8 = 3;\n"),
			("crate", "use std::io;", InsertPosition::End, "\tuse std::fmt;\n", "\tuse std::fmt;\n\tuse std::io;\n"),
			("<crate::S>", "const B: u8 = 2;", after("A"), "= 1;\n\n\tfn f", "= 1;\n\tconst B: u8 = 2;\n\n\tfn f"),
		];

		for (parent, source, position, old, new) in cases {
			let parent = if position == InsertPosition::End { "crate::tail" } else { parent };

			let text = inserted(parent, source, position.clone(), "src/lib.rs");

			assert_eq!(text, sorted_with(old, new), "{source} {position:?}");
		}

		// next to an item with docs, other kinds of items, items with docs, and items of several groups
		let cases = [
			("const Z: u8 = 3;", after("crate::Y"), "const Y: u8 = 2;\n", "const Y: u8 = 2;\n\nconst Z: u8 = 3;\n"),
			("fn g() {}", after("crate::a"), "mod a;\n", "mod a;\n\nfn g() {}\n\n"),
			("/// B.\nmod b;", after("crate::a"), "mod a;\n", "mod a;\n\n/// B.\nmod b;\n\n"),
			("mod b;\nuse b::B;", after("crate::a"), "mod a;\n", "mod a;\n\nmod b;\nuse b::B;\n\n"),
		];

		for (source, position, old, new) in cases {
			assert_eq!(inserted("crate", source, position, "src/lib.rs"), lib.replace(old, new), "{source}");
		}

		// siblings separated by blank lines keep them
		let text = inserted("crate::spaced", "mod b;", after("crate::spaced::a"), "src/spaced.rs");

		assert_eq!(text, "mod a;\n\nmod b;\n\nmod c;\n");
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

			edits.extend(rscode::edit::insert(&resolver, Some(&path(parent)), source, &options).unwrap().edits);
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
mod create_module {
	use super::*;
	use rscode::edit::CreateModuleOptions;
	use rscode::edit::ModuleCreation;

	fn create(ws: &Workspace, parent: &str, name: &str, source: &str) -> Result<ModuleCreation, Error> {
		create_with(ws, parent, name, source, "")
	}

	fn create_with(ws: &Workspace, parent: &str, name: &str, source: &str, vis: &str) -> Result<ModuleCreation, Error> {
		let options = CreateModuleOptions { vis: vis.to_owned() };

		rscode::edit::create_module(&Resolver::new(ws), &path(parent), name, source, &options)
	}

	/// A crate laid out like `cargo rscode sort` lays it out.
	const SORTED: &[(&str, &str)] = &[
		("src/lib.rs", "//! Docs.\n\nextern crate alloc;\n\nmod b;\nmod d;\n\nuse std::fmt;\n\npub fn f() {}\n"),
		("src/b.rs", "use std::fmt;\n\npub fn g() {}\n"),
		("src/d.rs", "//! Docs of d.\n"),
	];

	/// The new module goes where sorting puts it, on a line next to the other `mod` declarations, and sorting the
	/// parent afterwards changes nothing.
	#[test]
	fn declares_new_modules_where_sorting_puts_them() {
		let dir = TempDir::with_files("create-sorted", SORTED);
		let ws = load(&dir);
		let lib = SORTED[0].1;

		for (name, old, new) in [
			("a", "mod b;\n", "mod a;\nmod b;\n"),
			("c", "mod b;\n", "mod b;\nmod c;\n"),
			("e", "mod d;\n", "mod d;\nmod e;\n"),
		] {
			let creation = create(&ws, "crate", name, "").unwrap();
			let text = edited(&dir, &creation.edits, "src/lib.rs");

			assert_eq!(text, lib.replace(old, new), "{name}");
			assert_eq!(rscode::rscode_sort::sort_str(&text).unwrap(), text, "{name}");
			assert_eq!(creation.line, text.lines().position(|line| line == format!("mod {name};")).unwrap() + 1);
		}

		// modules without `mod` declarations: after `extern crate` items, before `use` items and the rest, or after
		// what ends the body when there are no items
		for (parent, file, old, new) in [
			("crate::b", "src/b.rs", "use std::fmt;", "mod x;\n\nuse std::fmt;"),
			("crate::d", "src/d.rs", "//! Docs of d.\n", "//! Docs of d.\n\nmod x;\n"),
		] {
			let creation = create(&ws, parent, "x", "").unwrap();
			let text = edited(&dir, &creation.edits, file);

			assert_eq!(text, SORTED.iter().find(|(path, _)| *path == file).unwrap().1.replace(old, new), "{parent}");
			assert_eq!(rscode::rscode_sort::sort_str(&text).unwrap(), text, "{parent}");
		}

		// and once written, as `cargo rscode sort` sorts the crate
		let mut edits = EditSet::new();

		for (parent, name) in [("crate", "c"), ("crate", "a"), ("crate::b", "x")] {
			edits.extend(create(&ws, parent, name, "").unwrap().edits);
		}

		edits.apply().unwrap();
		assert_sorted(&dir);
	}

	#[test]
	fn creates_files_where_rustc_finds_them() {
		let dir = TempDir::fixture("create-files");
		let ws = load_fixture(&dir, false);
		let source = "//! Rendering.\n\npub fn draw() {\n    todo!()\n}\n";
		let creation = create(&ws, "crate", "render", source).unwrap();

		assert_eq!(creation.file, dir.path("src/render.rs"));
		assert_eq!(creation.declaration, "mod render;");
		assert_eq!(creation.declared_in, dir.path("src/lib.rs"));
		assert_eq!(creation.edits.created().collect::<Vec<_>>(), [dir.path("src/render.rs")]);

		// with the indentation style of the parent's file
		let changed = changes(&dir, &creation.edits);

		assert_eq!(changed["src/render.rs"], "//! Rendering.\n\npub fn draw() {\n\ttodo!()\n}\n");
		assert!(changed["src/lib.rs"].contains("pub mod placed;\n\nmod render;\npub mod shapes;\nmod util;\n"));
		assert_eq!(creation.line, 8);

		// in the directory of the parent's modules: of a `mod.rs` file, of a file of another name, of a module loaded
		// with `#[path]`, and of an inline module, whose declaration goes inside of its braces
		for (parent, file, declared, layout) in [
			("crate::nested", "src/nested/x.rs", "src/nested/mod.rs", "pub mod deep;\nmod x;\n\n/// Something"),
			("crate::shapes", "src/shapes/x.rs", "src/shapes.rs", "pub mod round;\nmod x;\n\nuse round::Radius;"),
			("crate::placed", "src/custom/x.rs", "src/custom/placed.rs", "mod x;\n\n/// A module loaded with `#[path]`."),
			("crate::inline", "src/inline/x.rs", "src/lib.rs", "mod inline {\n\tmod x;\n\n\t/// A function"),
		] {
			let creation = create(&ws, parent, "x", "").unwrap();

			assert_eq!(creation.file, dir.path(file), "{parent}");
			assert!(edited(&dir, &creation.edits, declared).contains(layout), "{parent}");
			assert_eq!(creation.edits.created().collect::<Vec<_>>(), [dir.path(file)], "{parent}");
		}

		// with a visibility, and a raw name
		let creation = create_with(&ws, "crate", "type", "", "pub(crate)").unwrap();

		assert_eq!(creation.declaration, "pub(crate) mod r#type;");
		assert_eq!(creation.file, dir.path("src/type.rs"));
	}

	#[test]
	fn follows_mod_rs_files() {
		let files = [("src/lib.rs", "mod a;\nmod c;\n"), ("src/a/mod.rs", ""), ("src/c/mod.rs", "")];
		let dir = TempDir::with_files("create-mod-rs", &files);
		let ws = load(&dir);
		let creation = create(&ws, "crate", "b", "struct B;").unwrap();

		assert_eq!(creation.file, dir.path("src/b/mod.rs"));
		assert_eq!(changes(&dir, &creation.edits)["src/b/mod.rs"], "struct B;\n");
		assert_eq!(edited(&dir, &creation.edits, "src/lib.rs"), "mod a;\nmod b;\nmod c;\n");
	}

	#[test]
	fn refuses_taken_names_existing_files_and_invalid_source() {
		let dir = TempDir::fixture("create-refusals");

		dir.write("src/orphan.rs", "");
		dir.write("src/stray/mod.rs", "");

		let ws = load_fixture(&dir, false);

		match create(&ws, "crate", "util", "") {
			Err(Error::Collision { name, collisions }) => {
				assert_eq!(name, "util");
				assert!(collisions[0].starts_with("`util`: `edit_ops::util` (mod) at src/lib.rs:"), "{collisions:?}");
			}
			other => panic!("{other:?}"),
		}

		for (name, file) in [("orphan", "src/orphan.rs"), ("stray", "src/stray/mod.rs")] {
			match create(&ws, "crate", name, "") {
				Err(Error::Io { path, source }) => {
					assert_eq!(path, dir.path(file));
					assert_eq!(source.kind(), std::io::ErrorKind::AlreadyExists);
					assert!(source.to_string().contains("exists already"), "{source}");
				}
				other => panic!("{other:?}"),
			}
		}

		for name in ["1x", "self", "a::b", ""] {
			assert!(matches!(create(&ws, "crate", name, ""), Err(Error::InvalidIdent(_))), "{name}");
		}

		match create(&ws, "crate", "x", "fn x( {}") {
			Err(Error::InvalidSource(message)) => {
				assert!(message.starts_with("the source does not parse as the file of a module at 1:"), "{message}")
			}
			other => panic!("{other:?}"),
		}

		for vis in ["pub(nope)", "unsafe", "#[cfg(test)]"] {
			match create_with(&ws, "crate", "x", "", vis) {
				Err(Error::InvalidSource(message)) => assert!(message.contains("is not a visibility"), "{message}"),
				other => panic!("{vis}: {other:?}"),
			}
		}

		match create(&ws, "<crate::shapes::Circle>", "x", "") {
			Err(Error::Unsupported(message)) => assert!(message.starts_with("cannot create a module in"), "{message}"),
			other => panic!("{other:?}"),
		}

		assert!(matches!(create(&ws, "crate::nope", "x", ""), Err(Error::NotFound(_))));
	}

	#[test]
	fn applies_new_modules_and_still_compiles() {
		let dir = TempDir::fixture("create-apply");
		let ws = load_fixture(&dir, false);
		let mut edits = EditSet::new();

		let modules = [
			("crate", "render", "/// Draws.\npub fn draw() -> u8 {\n    super::util::double(1) as u8\n}", "pub"),
			("crate::nested", "extra", "#![allow(dead_code)]\n\nstruct Extra;", ""),
			("crate::inline", "inner_child", "", "pub(crate)"),
		];

		for (parent, name, source, vis) in modules {
			edits.extend(create_with(&ws, parent, name, source, vis).unwrap().edits);
		}

		let applied = edits.apply().unwrap();

		assert_eq!(
			dir.relative(&applied.created),
			["src/inline/inner_child.rs", "src/nested/extra.rs", "src/render.rs"]
		);
		assert_eq!(dir.read("src/render.rs"), "/// Draws.\npub fn draw() -> u8 {\n\tsuper::util::double(1) as u8\n}\n");
		cargo_check(&dir);
	}
}

mod add_imports {
	use super::*;
	use rscode::edit::ImportAddition;
	use rscode::edit::ImportOptions;
	use rscode::edit::ImportOutcome;

	fn import(ws: &Workspace, module: &str, imports: &[&str]) -> Result<ImportAddition, Error> {
		let imports: Vec<String> = imports.iter().map(|import| import.to_string()).collect();

		rscode::edit::add_imports(&Resolver::new(ws), &path(module), &imports, &ImportOptions::default())
	}

	/// The imports' paths, outcomes (`+` added, `=` present, `>` merged), and lines.
	fn outcomes(addition: &ImportAddition) -> Vec<String> {
		(addition.imports.iter())
			.map(|import| {
				let outcome = match import.outcome {
					ImportOutcome::Added => "+",
					ImportOutcome::Present => "=",
					ImportOutcome::Merged(_) => ">",
				};

				format!("{outcome} {} {}", import.path, import.line)
			})
			.collect()
	}

	/// Asserts that sorting the text changes nothing, and returns it.
	#[track_caller]
	fn sorted(text: String) -> String {
		assert_eq!(rscode::rscode_sort::sort_str(&text).unwrap(), text);
		text
	}

	/// A crate with one import per `use` item, laid out like `cargo rscode sort` lays it out.
	const ITEMS: &[(&str, &str)] = &[
		(
			"src/lib.rs",
			"\
//! Docs.

mod a;

use crate::a::B;
use std::fmt;
use std::io;

pub use a::A;

pub fn f() {}

mod inline {
	use std::fmt;
}
",
		),
		("src/a.rs", "pub struct A;\n\npub struct B;\n\npub struct C;\n\nfn g() {}\n"),
	];

	#[test]
	fn adds_use_items_where_sorting_puts_them() {
		let dir = TempDir::with_files("import-items", ITEMS);
		let ws = load(&dir);
		let lib = ITEMS[0].1;
		let cases: &[(&[&str], &str, &str, &[&str])] = &[
			(&["std::env"], "use crate::a::B;\n", "use crate::a::B;\nuse std::env;\n", &["+ std::env 6"]),
			(&["use std::io::Write;"], "use std::io;\n", "use std::io;\nuse std::io::Write;\n", &["+ std::io::Write 8"]),
			(
				&["crate::a::{B, C}"],
				"use crate::a::B;\n",
				"use crate::a::B;\nuse crate::a::C;\n",
				&["= crate::a::B 5", "+ crate::a::C 6"],
			),
			(&["pub use a::Z"], "pub use a::A;\n", "pub use a::A;\npub use a::Z;\n", &["+ pub use a::Z 10"]),
			(
				&["std::env", "alloc::vec::Vec", "zzz::Z"],
				"use crate::a::B;\nuse std::fmt;\nuse std::io;\n",
				"use crate::a::B;\nuse alloc::vec::Vec;\nuse std::env;\nuse std::fmt;\nuse std::io;\nuse zzz::Z;\n",
				&["+ std::env 7", "+ alloc::vec::Vec 6", "+ zzz::Z 10"],
			),
		];

		for &(imports, old, new, expected) in cases {
			let addition = import(&ws, "crate", imports).unwrap();
			let text = sorted(edited(&dir, &addition.edits, "src/lib.rs"));

			assert_eq!(text, lib.replace(old, new), "{imports:?}");
			assert_eq!(outcomes(&addition), expected, "{imports:?}");
			assert_eq!(addition.file, dir.path("src/lib.rs"));
		}

		// and once written, as `cargo rscode sort` sorts the crate
		import(&ws, "crate", &["std::env", "alloc::vec::Vec", "zzz::Z", "pub use a::C"]).unwrap().edits.apply().unwrap();
		assert_sorted(&dir);

		// what is imported already changes nothing
		let addition = import(&ws, "crate", &["std::fmt", "std::fmt"]).unwrap();

		assert!(addition.edits.is_empty());
		assert_eq!(outcomes(&addition), ["= std::fmt 6", "= std::fmt 6"]);

		// in an inline module, indented
		let addition = import(&ws, "crate::inline", &["std::io"]).unwrap();
		let text = sorted(edited(&dir, &addition.edits, "src/lib.rs"));

		assert_eq!(text, lib.replace("\tuse std::fmt;\n", "\tuse std::fmt;\n\tuse std::io;\n"));

		// a module without `use` items: before the items that sort after them, or after what ends the body
		let addition = import(&ws, "crate::a", &["std::fmt"]).unwrap();

		assert_eq!(edited(&dir, &addition.edits, "src/a.rs"), format!("use std::fmt;\n\n{}", ITEMS[1].1));
	}

	/// In modules that group their `use` items by where the paths are from, imports go into their group.
	#[test]
	fn joins_the_group_of_their_origin() {
		let lib = "\
use std::fmt;
use std::io;

use serde::Serialize;

use crate::a::B;

mod a;
";
		let dir = TempDir::with_files("import-origins", &[("src/lib.rs", lib), ("src/a.rs", "pub struct B;\npub struct C;\n")]);
		let ws = load(&dir);

		for (import, old, new) in [
			("crate::a::C", "use crate::a::B;\n", "use crate::a::B;\nuse crate::a::C;\n"),
			("anyhow::Result", "use serde::Serialize;", "use anyhow::Result;\nuse serde::Serialize;"),
			("std::env", "use std::fmt;", "use std::env;\nuse std::fmt;"),
			("std::path::Path", "use std::io;\n", "use std::io;\nuse std::path::Path;\n"),
		] {
			let addition = self::import(&ws, "crate", &[import]).unwrap();

			assert_eq!(edited(&dir, &addition.edits, "src/lib.rs"), lib.replace(old, new), "{import}");
		}

		// without such groups, sorting decides
		let lib = "use std::fmt;\n\nuse crate::a::B;\nuse serde::Serialize;\n\nmod a;\n";
		let dir = TempDir::with_files("import-origins-mixed", &[("src/lib.rs", lib), ("src/a.rs", "pub struct B;\n")]);
		let ws = load(&dir);
		let addition = self::import(&ws, "crate", &["anyhow::Result"]).unwrap();

		assert_eq!(edited(&dir, &addition.edits, "src/lib.rs"), lib.replace("use std", "use anyhow::Result;\nuse std"));
	}

	/// A bare name that names nothing in the module imports the item of that name.
	#[test]
	fn imports_items_by_their_names() {
		let dir = TempDir::with_files("import-names", ITEMS);
		let ws = load(&dir);
		let addition = import(&ws, "crate::inline", &["C", "pub(crate) use C as See"]).unwrap();

		assert_eq!(outcomes(&addition), ["+ crate::a::C 14", "+ pub(crate) use crate::a::C as See 17"]);
		let text = edited(&dir, &addition.edits, "src/lib.rs");

		assert!(text.ends_with("\tuse crate::a::C;\n\tuse std::fmt;\n\n\tpub(crate) use crate::a::C as See;\n}\n"), "{text}");
		assert!(matches!(import(&ws, "crate", &["Nope"]), Err(Error::NotFound(name)) if name == "Nope"));

		// several items of the name
		let files = [("src/lib.rs", "mod a;\nmod b;\n"), ("src/a.rs", "pub struct X;\n"), ("src/b.rs", "pub struct X;\n")];
		let dir = TempDir::with_files("import-names-ambiguous", &files);
		let ws = load(&dir);

		match import(&ws, "crate", &["X"]) {
			Err(Error::Ambiguous { path, candidates }) => {
				assert_eq!(path, "X");
				assert_eq!(candidates.len(), 2, "{candidates:?}");
			}
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn refuses_collisions_and_what_is_not_an_import() {
		let dir = TempDir::with_files("import-refusals", ITEMS);
		let ws = load(&dir);

		match import(&ws, "crate", &["other::B"]) {
			Err(Error::Collision { name, collisions }) => {
				assert_eq!(name, "B");
				assert!(collisions[0].starts_with("`B`: the import of `fixture::a::B` at src/lib.rs:5:"), "{collisions:?}");
			}
			other => panic!("{other:?}"),
		}

		// glob imports are shadowed, and forcing imports anyway
		assert!(import(&ws, "crate", &["other::*"]).is_ok());

		let options = ImportOptions { force: true };
		let forced = rscode::edit::add_imports(&Resolver::new(&ws), &path("crate"), &["other::B".to_owned()], &options);

		assert!(forced.is_ok());

		for invalid in ["not a path!", "#[cfg(test)] use a::b", "", "self"] {
			assert!(matches!(import(&ws, "crate", &[invalid]), Err(Error::InvalidSource(_))), "{invalid:?}");
		}

		let lib = "pub struct S;\n\nimpl S {}\n";
		let dir = TempDir::with_files("import-impl", &[("src/lib.rs", lib)]);
		let ws = load(&dir);

		match import(&ws, "<crate::S>", &["std::fmt"]) {
			Err(Error::Unsupported(message)) => assert!(message.starts_with("cannot import into"), "{message}"),
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn merges_into_use_items_of_their_module() {
		let lib = "\
use crate::a::{B, C};
use std::collections::{BTreeMap, HashMap};
use std::{fmt, io};
#[allow(unused_imports)]
use std::{fs, path};

mod a;
";
		let files = [("src/lib.rs", lib), ("src/a.rs", "pub struct B;\npub struct C;\npub struct D;\n")];
		let dir = TempDir::with_files("import-module", &files);
		let ws = load(&dir);
		let cases: &[(&[&str], &str, &str)] = &[
			(&["std::env"], "use std::{fmt, io};", "use std::{env, fmt, io};"),
			(&["std::collections::BTreeSet"], "{BTreeMap, HashMap}", "{BTreeMap, BTreeSet, HashMap}"),
			(&["crate::a::D"], "use crate::a::{B, C};", "use crate::a::{B, C, D};"),
			(&["std::env", "std::process"], "use std::{fmt, io};", "use std::{env, fmt, io, process};"),
			(&["std::time::Duration"], "use std::{fmt, io};", "use std::time::Duration;\nuse std::{fmt, io};"),
		];

		for &(imports, old, new) in cases {
			let addition = import(&ws, "crate", imports).unwrap();

			assert_eq!(edited(&dir, &addition.edits, "src/lib.rs"), lib.replace(old, new), "{imports:?}");
		}

		let addition = import(&ws, "crate", &["std::env"]).unwrap();

		assert_eq!(outcomes(&addition), ["> std::env 3"]);
		assert_eq!(addition.imports[0].outcome, ImportOutcome::Merged("use std::{env, fmt, io};".to_owned()));
	}

	#[test]
	fn merges_into_use_items_of_several_modules() {
		let lib = "\
use crate::{a::B, c::D};
use std::fs;
use std::{
	fmt,
	io::{self, Read},
};

mod a;
mod c;
";
		let files = [("src/lib.rs", lib), ("src/a.rs", "pub struct B;\n"), ("src/c.rs", "pub struct D;\npub struct E;\n")];
		let dir = TempDir::with_files("import-crate", &files);
		let ws = load(&dir);
		let cases: &[(&[&str], &str, &str)] = &[
			(&["std::io::Write"], "{self, Read}", "{self, Read, Write}"),
			(&["std::env"], "\tfmt,\n", "\tenv,\n\tfmt,\n"),
			(&["std::path::Path"], "Read},\n", "Read},\n\tpath::Path,\n"),
			(&["crate::c::E"], "c::D}", "c::{D, E}}"),
			(&["crate::e::F"], "c::D}", "c::D, e::F}"),
			(&["std::fmt::Write"], "\tfmt,\n", "\tfmt::{self, Write},\n"),
		];

		for &(imports, old, new) in cases {
			let addition = import(&ws, "crate", imports).unwrap();

			assert_eq!(edited(&dir, &addition.edits, "src/lib.rs"), lib.replace(old, new), "{imports:?}");
		}

		// a path that diverges from the item's becomes a group
		let lib = "use crate::{a::B, c::D};\nuse std::fs;\n\nmod a;\nmod c;\n";
		let files = [("src/lib.rs", lib), ("src/a.rs", "pub struct B;\n"), ("src/c.rs", "pub struct D;\n")];
		let dir = TempDir::with_files("import-diverge", &files);
		let ws = load(&dir);

		for (imports, new) in [(&["std::io"], "use std::{fs, io};"), (&["std::fs::File"], "use std::fs::{self, File};")] {
			let addition = import(&ws, "crate", imports).unwrap();

			assert_eq!(edited(&dir, &addition.edits, "src/lib.rs"), lib.replace("use std::fs;", new), "{imports:?}");
		}
	}

	#[test]
	fn applies_imports_and_still_compiles() {
		let dir = TempDir::fixture("import-apply");
		let ws = load_fixture(&dir, false);
		let addition = import(&ws, "crate::nested", &["std::fmt::Write as _", "crate::shapes::Circle"]).unwrap();

		addition.edits.apply().unwrap();
		assert!(
			dir.read("src/nested/mod.rs")
				.contains("pub mod deep;\n\nuse crate::shapes::Circle;\nuse std::fmt::Write as _;\n\n/// Something")
		);
		cargo_check(&dir);
	}
}

mod real {
	use super::*;
	use rscode::View;
	use rscode::ViewMode;
	use rscode::edit::EditItemOptions;
	use rscode::edit::ItemEdit;
	use rscode::edit::TextReplacement;
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
		let (mut removed, mut replaced, mut inserted, mut edited_items) = (0, 0, 0, 0);

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

			// its whole view (with line numbers or not) is found, and changes nothing; a smaller sample, since views of
			// modules parse their files
			if let [target] = targets.as_slice()
				&& (index % 18 == 0 || container)
			{
				let view = View::new().mode(ViewMode::Full).line_numbers(true).items(&resolver, &[*target]).unwrap();

				// (without the line that names the file of a module)
				let skipped = usize::from(view[0].text.lines().next().is_some_and(|line| line.contains("// file: ")));
				let numbered: Vec<&str> = view[0].text.lines().skip(skipped).collect();
				let plain: Vec<&str> = (numbered.iter())
					.map(|line| line.split_once(" │").map_or(*line, |(_, text)| text.strip_prefix(' ').unwrap_or(text)))
					.collect();

				for text in [plain.join("\n"), numbered.join("\n")] {
					let edit = ItemEdit {
						replacements: vec![TextReplacement {
							old: text.clone(),
							new: text,
						}],
						..ItemEdit::default()
					};
					let plan = rscode::edit::edit_item(&resolver, &item_path, &edit, &EditItemOptions::default())
						.unwrap_or_else(|error| panic!("editing `{item_path}`: {error}"));

					let changes = plan.edits.preview().unwrap();

					assert!(changes.iter().all(|change| !change.is_changed()), "editing `{item_path}` changes it");
				}

				edited_items += 1;
			}

			// inserting into it (when it holds items) leaves parsable files
			if container {
				for position in [InsertPosition::Start, InsertPosition::End] {
					let options = InsertOptions { position, force: true };
					let insertion = rscode::edit::insert(&resolver, Some(&item_path), "fn rscode_inserted() {}", &options)
						.unwrap_or_else(|error| panic!("inserting into `{item_path}`: {error}"));

					insertion.edits.preview().unwrap_or_else(|error| panic!("inserting into `{item_path}`: {error}"));
				}

				inserted += 1;
			}
		}

		assert!(
			removed > 50 && replaced > 100 && inserted > 10 && edited_items > 50,
			"{removed} {replaced} {inserted} {edited_items}"
		);
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

	/// A thread-local static named as an anchor stands for its invocation.
	#[test]
	fn anchor_insertions() {
		let dir = crate_dir("thread-local-anchor");
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let options = rscode::edit::InsertOptions { position: InsertPosition::Before("DEPTH".to_owned()), force: false };
		let insertion = rscode::edit::insert(&resolver, Some(&path("crate::state")), "pub fn f() {}", &options).unwrap();

		assert!(edited(&dir, &insertion.edits, "src/lib.rs").contains("\tpub fn f() {}\n\n\tthread_local! {"));
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
					assert!(
						candidates[0].starts_with(&native("`use fixture::Circle` (import) at src/lib.rs:")),
						"{candidates:?}"
					);
					assert!(
						candidates[1].starts_with(&native("`fixture::shapes::Circle` (struct) at src/lib.rs:")),
						"{candidates:?}"
					);
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
		assert!(one.dangling.is_empty() && one.warnings.is_empty(), "{one:#?}");

		// what used the imported names through the imports is left dangling
		let circle = remove(&["use crate::Circle"]);
		let lines = |removal: &rscode::edit::Removal| removal.dangling.iter().map(|reference| reference.start.line).collect::<Vec<_>>();

		assert_eq!(lines(&circle), [32]);
		assert!(circle.warnings.is_empty(), "{:?}", circle.warnings);

		let all = remove(&["use crate::*", "use crate::assist"]);

		assert_eq!(lines(&all), [32, 32]);

		// method calls of traits in scope are not paths
		let trait_import = remove(&["use crate::_"]);

		assert!(trait_import.dangling.is_empty());
		assert_eq!(
			trait_import.warnings,
			["calls in `fixture` of methods of traits that `use fixture::_` brings into scope are not checked"]
		);

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
		let insertion = rscode::edit::insert(&resolver, Some(&path("crate")), "use shapes::Shape;", &options).unwrap();

		let text = edited(&dir, &insertion.edits, "src/lib.rs");

		// (joining the `use` items around it, which are on consecutive lines)
		assert!(text.contains("use shapes::Circle;\nuse shapes::Shape;\nuse shapes::{Shape as _"), "{text}");

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

	/// Imports under `cfg`s and several imports of one name.
	const VARIANTS: &str = "\
pub mod shapes {
	pub struct Circle;

	pub struct Square;

	pub trait T1 {}

	pub trait T2 {}
}

pub mod alt {
	pub struct Qux;
}

#[cfg(feature = \"a\")]
pub struct Qux;

#[cfg(not(feature = \"a\"))]
use alt::Qux;

#[cfg(feature = \"a\")]
use shapes::Circle as Pick;
#[cfg(not(feature = \"a\"))]
use shapes::Square as Pick;

use shapes::T1 as _;
use shapes::T2 as _;

mod traits {
	use crate::shapes::{T1 as _, T2 as _};
}

#[allow(unused_imports)]
mod globs {
	use crate::{alt::*, shapes::*};
}

pub fn make() -> (Qux, Pick) {
	todo!()
}
";

	fn variants_dir(name: &str) -> TempDir {
		let manifest = format!("{MANIFEST}\n[features]\na = []\n");

		TempDir::with_files(name, &[("Cargo.toml", manifest.as_str()), ("src/lib.rs", VARIANTS)])
	}

	/// A private import of an item's name does not hide the item: its path names it (the import is `use crate::Qux`).
	#[test]
	fn items_keep_their_paths_next_to_private_imports() {
		let dir = variants_dir("imports-own-items");
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let removal = rscode::edit::remove(&resolver, &paths(&["crate::Qux"]), &RemoveOptions::default()).unwrap();

		assert_eq!(removal.removed.iter().map(|item| item.path.as_str()).collect::<Vec<_>>(), ["fixture::Qux"]);

		let removal = rscode::edit::remove(&resolver, &paths(&["use crate::Qux"]), &RemoveOptions::default()).unwrap();

		assert_eq!(removal.removed.iter().map(|item| item.path.as_str()).collect::<Vec<_>>(), ["use fixture::Qux"]);
	}

	#[test]
	fn replacing_several_imports() {
		let dir = variants_dir("imports-replace-several");
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let replace = |path: &str, source: &str, all_variants: bool| {
			let options = ReplaceOptions { all_variants, ..ReplaceOptions::default() };

			rscode::edit::replace(&resolver, &self::path(path), source, &options)
		};
		let candidates = |result: Result<_, Error>| match result {
			Err(Error::Ambiguous { candidates, .. }) => candidates,
			other => panic!("{other:?}"),
		};

		// `as _` imports of one module with the same `cfg`s are not `cfg` variants of each other
		let unnamed = candidates(replace("use crate::_", "use shapes::T1 as _;", true));

		assert_eq!(unnamed.len(), 2, "{unnamed:?}");
		let import = native("`use fixture::_` (import) at src/lib.rs:");

		assert!(unnamed.iter().all(|candidate| candidate.starts_with(&import)), "{unnamed:?}");
		assert!(!rscode::edit::replaces_all_variants(&resolver, &path("use crate::_")));

		// `cfg` variants are, and are listed as the imports
		let picks = candidates(replace("use crate::Pick", "use shapes::Circle as Pick;", false));

		let import = native("`use fixture::Pick` (import) at src/lib.rs:");

		assert!(picks.iter().all(|candidate| candidate.starts_with(&import)), "{picks:?}");
		assert!(rscode::edit::replaces_all_variants(&resolver, &path("use crate::Pick")));

		// a `use` item all of whose imports the path names
		let traits = replace("use crate::traits::_", "use crate::shapes::T1 as _;", false).unwrap();

		assert!(edited(&dir, &traits.edits, "src/lib.rs").contains("mod traits {\n\tuse crate::shapes::T1 as _;\n}"));

		// a replacement that no longer imports the name
		let other = replace("use crate::Qux", "use crate::shapes::Circle;", false).unwrap();

		assert!(other.warnings.iter().any(|warning| warning.contains("does not import `Qux`")), "{:?}", other.warnings);

		let renamed = replace("use crate::Qux", "use crate::shapes::Circle as Qux;", false).unwrap();

		assert!(renamed.warnings.is_empty(), "{:?}", renamed.warnings);
	}

	#[test]
	fn anchors_by_name_and_views_and_finds() {
		let dir = variants_dir("imports-anchors");
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);

		// a single name stands for the `use` items that bind it
		let options = InsertOptions { position: InsertPosition::After("Pick".to_owned()), force: false };
		let insertion = rscode::edit::insert(&resolver, Some(&path("crate")), "pub struct Anchored;", &options).unwrap();
		let text = edited(&dir, &insertion.edits, "src/lib.rs");

		assert!(text.contains("use shapes::Square as Pick;\n\npub struct Anchored;\n"), "{text}");

		// the glob imports of one `use` item are shown once
		let views = rscode::View::new().path("use crate::globs::*").unwrap().run_with(&resolver).unwrap();

		assert_eq!(views.len(), 1, "{views:#?}");

		// a `use` pattern does not make other patterns match imports
		let find = |patterns: &[&str]| {
			let mut find = rscode::Find::new();

			for pattern in patterns {
				find = find.pattern(pattern).unwrap();
			}

			find.run_with(&resolver).unwrap().into_iter().map(|found| found.path).collect::<Vec<_>>()
		};

		assert_eq!(find(&["Circle", "use Nothing"]), find(&["Circle"]));
		assert_eq!(find(&["Circle", "use Pick"]).len(), find(&["Circle"]).len() + 2);
	}

	/// Imports through removed imports break: they are left dangling, or pruned.
	#[test]
	fn removing_imports_that_other_imports_go_through() {
		const CHAINS: &str = "\
macro_rules! make {
	($name:ident) => {
		pub struct $name;
	};
}

pub mod shapes {
	pub struct Foo;

	pub mod inner {
		pub struct Deep;
	}
}

pub mod generated {
	make!(Made);
}

pub use generated::Made;
pub use shapes::inner;
use shapes::Foo;

mod reexported {
	use crate::inner::Deep;

	pub fn h() -> Deep {
		Deep
	}
}

mod chained {
	use super::Foo;

	pub fn f() -> Foo {
		Foo
	}
}

mod unresolved {
	use crate::Made;

	pub fn g() -> Made {
		Made
	}
}
";
		let dir = TempDir::with_files("imports-chains", &[("Cargo.toml", MANIFEST), ("src/lib.rs", CHAINS)]);

		cargo_check(&dir);

		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let remove = |target: &str, prune_imports: bool| {
			let options = RemoveOptions { prune_imports, ..RemoveOptions::default() };

			rscode::edit::remove(&resolver, &paths(&[target]), &options).unwrap()
		};
		let lines = |removal: &rscode::edit::Removal| removal.dangling.iter().map(|reference| reference.start.line).collect::<Vec<_>>();
		let removed = |removal: &rscode::edit::Removal| removal.removed.iter().map(|item| item.path.clone()).collect::<Vec<_>>();

		// a re-export of a module, a private import, and a re-export of an item that is not loaded (made by a macro)
		assert_eq!(lines(&remove("use crate::inner", false)), [24, 26, 27]);
		assert_eq!(lines(&remove("use crate::Foo", false)), [32, 34, 35]);
		assert_eq!(lines(&remove("use crate::Made", false)), [40, 42, 43]);

		// pruned, the imports through them go too
		let inner = remove("use crate::inner", true);

		assert_eq!(removed(&inner), ["use fixture::inner", "use fixture::reexported::Deep"]);
		assert_eq!(lines(&inner), [26, 27]);

		let made = remove("use crate::Made", true);

		assert_eq!(removed(&made), ["use fixture::Made", "use fixture::unresolved::Made"]);
		assert_eq!(lines(&made), [42, 43]);
	}

	/// Names of items made by macros die through glob imports too, unless a glob import might still provide them; a
	/// removed import that shadowed a prelude name or a glob import is told about too.
	#[test]
	fn removing_imports_that_glob_imports_and_preludes_go_through() {
		const GLOBS: &str = "\
macro_rules! make {
	($name:ident) => {
		pub struct $name;
	};
}

pub mod made {
	make!(Made);

	pub type Result<T> = std::result::Result<T, ()>;
}

pub mod a {
	pub struct Foo;
}

pub mod b {
	pub struct Foo;
}

pub mod user {
	use crate::made::Made;

	pub fn f() -> Made {
		Made
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		fn g() -> Made {
			Made
		}
	}
}

pub mod still {
	use crate::made::*;
	use crate::made::Made;

	pub fn f() -> Made {
		Made
	}
}

pub mod prelude {
	use crate::made::Result;

	pub fn f() -> Result<()> {
		Ok(())
	}
}

pub mod shadowing {
	use crate::a::*;
	use crate::b::Foo;

	pub fn f() -> Foo {
		Foo
	}
}
";
		let dir = TempDir::with_files("imports-globs", &[("Cargo.toml", MANIFEST), ("src/lib.rs", GLOBS)]);

		cargo_check(&dir);

		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let remove = |target: &str| rscode::edit::remove(&resolver, &paths(&[target]), &RemoveOptions::default()).unwrap();
		let lines = |removal: &rscode::edit::Removal| removal.dangling.iter().map(|reference| reference.start.line).collect::<Vec<_>>();

		// through `use super::*`
		assert_eq!(lines(&remove("use crate::user::Made")), [24, 25, 32, 33]);

		// a glob import of a module with macro invocations might provide it
		assert!(remove("use crate::still::Made").dangling.is_empty());

		// the prelude's `Result` does not replace it
		assert_eq!(lines(&remove("use crate::prelude::Result")), [50]);

		// a glob import does, which is warned about
		let shadowing = remove("use crate::shadowing::Foo");

		assert!(shadowing.dangling.is_empty(), "{:?}", shadowing.dangling);
		assert_eq!(
			shadowing.warnings,
			["without `use fixture::shadowing::Foo`, `Foo` in `fixture::shadowing` names `fixture::a::Foo` instead of `fixture::b::Foo`"]
		);
	}
}

/// Fields of structs, unions, and variants are items: viewed, replaced, and removed by their paths.
mod fields {
	use super::*;
	use rscode::edit::RenameOptions;

	const LIB: &str = "\
/// A point.
pub struct Point {
	/// The x coordinate.
	pub x: i32,
	#[allow(dead_code)]
	y: i32,
}

pub struct Pair(pub u8, pub u16);

pub enum Shape {
	Circle { radius: f64 },
	Square(f64),
}

impl Point {
	pub fn y(&self) -> i32 {
		self.y
	}
}
";

	/// The file after replacing the item at `path` with `source`, which must give no warnings.
	fn replaced(dir: &TempDir, ws: &Workspace, path: &str, source: &str) -> String {
		let replacement = rscode::edit::replace(&Resolver::new(ws), &super::path(path), source, &ReplaceOptions::default())
			.unwrap_or_else(|error| panic!("{error}"));

		assert_eq!(replacement.warnings, Vec::<String>::new());
		edited(dir, &replacement.edits, "src/lib.rs")
	}

	#[test]
	fn are_viewed_by_their_paths() {
		let dir = TempDir::with_files("fields-view", &[("src/lib.rs", LIB)]);
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let views = rscode::View::new()
			.path("crate::Point.x")
			.unwrap()
			.path("crate::Pair.1")
			.unwrap()
			.run_with(&resolver)
			.unwrap();

		assert_eq!(
			views.iter().map(|view| (view.path.as_str(), view.kind, view.text.as_str())).collect::<Vec<_>>(),
			[
				("fixture::Point.x", ItemKind::Field, "/// The x coordinate.\npub x: i32"),
				("fixture::Pair.1", ItemKind::Field, "pub u16"),
			]
		);

		// `Point::y` is the method; the field is `Point.y`
		let named = |text: &str| -> Vec<String> {
			(resolver.resolve_item_path(&path(text)).into_iter()).map(|item| resolver.canonical_path(item).to_string()).collect()
		};

		assert_eq!(named("crate::Point::y"), ["fixture::Point::y"]);
		assert_eq!(named("crate::Point.y"), ["fixture::Point.y"]);
		assert_eq!(named("crate::Shape::Circle::radius"), ["fixture::Shape::Circle.radius"]);
	}

	#[test]
	fn are_replaced_without_their_comma() {
		let dir = TempDir::with_files("fields-replace", &[("src/lib.rs", LIB)]);
		let ws = load(&dir);

		assert_eq!(
			replaced(&dir, &ws, "crate::Point.x", "/// The first coordinate.\npub x: i64,"),
			LIB.replace("/// The x coordinate.\n\tpub x: i32,", "/// The first coordinate.\n\tpub x: i64,")
		);
		assert_eq!(replaced(&dir, &ws, "crate::Pair.1", "pub u32"), LIB.replace("pub u16", "pub u32"));
		assert_eq!(
			replaced(&dir, &ws, "crate::Shape::Circle.radius", "radius: f32"),
			LIB.replace("radius: f64", "radius: f32")
		);

		// only a field replaces a field
		let resolver = Resolver::new(&ws);
		let function = rscode::edit::replace(&resolver, &path("crate::Point.x"), "fn x() {}", &ReplaceOptions::default());

		assert!(matches!(function, Err(Error::InvalidSource(_))), "{function:?}");
	}

	#[test]
	fn are_removed_with_their_comma() {
		let dir = TempDir::with_files("fields-remove", &[("src/lib.rs", LIB)]);
		let ws = load(&dir);
		let targets = paths(&["crate::Point.y", "crate::Pair.0", "crate::Shape::Square.0"]);
		let removal = rscode::edit::remove(&Resolver::new(&ws), &targets, &RemoveOptions::default()).unwrap();

		assert_eq!(
			edited(&dir, &removal.edits, "src/lib.rs"),
			LIB.replace("\t#[allow(dead_code)]\n\ty: i32,\n", "")
				.replace("(pub u8, pub u16)", "(pub u16)")
				.replace("Square(f64)", "Square()")
		);
		assert_eq!(
			removal.warnings,
			["the uses of removed fields (field accesses, struct literals, and patterns) are not searched for: check them"]
		);
		assert_eq!(
			removal.removed.iter().map(|removed| (removed.path.as_str(), removed.kind)).collect::<Vec<_>>(),
			[
				("fixture::Point.y", ItemKind::Field),
				("fixture::Pair.0", ItemKind::Field),
				("fixture::Shape::Square.0", ItemKind::Field)
			]
		);
	}

	#[test]
	fn are_not_renamed() {
		let dir = TempDir::with_files("fields-rename", &[("src/lib.rs", LIB)]);
		let ws = load(&dir);

		match rscode::edit::rename(&Resolver::new(&ws), &path("crate::Point.x"), "z", &RenameOptions::default()) {
			Err(Error::Unsupported(message)) => assert!(message.starts_with("`fixture::Point.x` is a field, which cannot be renamed"), "{message}"),
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn applies_field_edits_and_still_compiles() {
		let dir = TempDir::with_files(
			"fields-apply",
			&[("Cargo.toml", "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n"), ("src/lib.rs", LIB)],
		);
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let mut edits = rscode::edit::replace(&resolver, &path("crate::Point.x"), "pub x: i64", &ReplaceOptions::default())
			.unwrap()
			.edits;

		edits.extend(rscode::edit::remove(&resolver, &paths(&["crate::Pair.0"]), &RemoveOptions::default()).unwrap().edits);
		edits.apply().unwrap();

		assert!(dir.read("src/lib.rs").contains("\tpub x: i64,\n") && dir.read("src/lib.rs").contains("Pair(pub u16);"));
		cargo_check(&dir);
	}
}

/// Item-position macro invocations are named by their module, the macro's name, and `!` (and an index).
mod macro_calls {
	use super::*;

	const LIB: &str = "\
macro_rules! commands {
	($(static $name:ident = $value:expr;)*) => { $(pub static $name: u8 = $value;)* };
}

commands! {
	static SAY = 1;
	static HELP = 2;
}

commands! {
	static QUIT = 3;
}

pub fn first() -> u8 {
	SAY + HELP + QUIT
}
";

	#[test]
	fn are_found_viewed_replaced_and_removed() {
		let dir = TempDir::with_files("macro-calls", &[("src/lib.rs", LIB)]);
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let found = |find: rscode::Find| -> Vec<String> {
			find.run_with(&resolver).unwrap().into_iter().map(|found| found.path).collect()
		};
		let both = ["fixture::commands![1]", "fixture::commands![2]"];

		// found by patterns with `!`, or by their kind, but not by other patterns
		assert_eq!(found(rscode::Find::new().pattern("**!").unwrap()), both);
		assert_eq!(found(rscode::Find::new().kind(ItemKind::MacroCall)), both);
		assert_eq!(found(rscode::Find::new().pattern("commands![2]").unwrap()), both[1..]);
		assert_eq!(
			found(rscode::Find::new().pattern("**").unwrap()),
			["fixture", "fixture::commands", "fixture::SAY", "fixture::HELP", "fixture::QUIT", "fixture::first"]
		);

		let views = rscode::View::new().path("crate::commands![2]").unwrap().run_with(&resolver).unwrap();

		assert_eq!((views[0].kind, views[0].text.as_str()), (ItemKind::MacroCall, "commands! {\n\tstatic QUIT = 3;\n}"));

		// a path naming several is ambiguous, with copyable candidates
		let source = "commands! {\n\tstatic QUIT = 4;\n}";

		match rscode::edit::replace(&resolver, &path("crate::commands!"), source, &ReplaceOptions::default()) {
			Err(Error::Ambiguous { candidates, .. }) => assert_eq!(
				candidates,
				[
					"`fixture::commands![1]` (macro-call) at src/lib.rs:5:1",
					"`fixture::commands![2]` (macro-call) at src/lib.rs:10:1"
				]
				.map(native)
			),
			other => panic!("{other:?}"),
		}

		// and so is removing them (rather than removing every one)
		match rscode::edit::remove(&resolver, &paths(&["crate::commands!"]), &RemoveOptions::default()) {
			Err(Error::Ambiguous { candidates, .. }) => assert_eq!(
				candidates,
				["`fixture::commands![1]` at src/lib.rs:5:1", "`fixture::commands![2]` at src/lib.rs:10:1"].map(native)
			),
			other => panic!("{other:?}"),
		}

		let replacement = rscode::edit::replace(&resolver, &path("crate::commands![2]"), source, &ReplaceOptions::default()).unwrap();

		assert_eq!(edited(&dir, &replacement.edits, "src/lib.rs"), LIB.replace("QUIT = 3", "QUIT = 4"));

		let removal = rscode::edit::remove(&resolver, &paths(&["commands![1]"]), &RemoveOptions::default()).unwrap();

		assert_eq!(
			edited(&dir, &removal.edits, "src/lib.rs"),
			LIB.replace("commands! {\n\tstatic SAY = 1;\n\tstatic HELP = 2;\n}\n\n", "")
		);
		assert_eq!(removal.removed[0].path, "fixture::commands![1]");

		// next to an invocation, without a parent
		let options = InsertOptions { position: InsertPosition::After("crate::commands![2]".to_owned()), force: false };
		let insertion = rscode::edit::insert(&resolver, None, "pub fn second() {}", &options).unwrap();

		assert_eq!(
			edited(&dir, &insertion.edits, "src/lib.rs"),
			LIB.replace("QUIT = 3;\n}\n", "QUIT = 3;\n}\n\npub fn second() {}\n")
		);
	}

	/// Invocations that are `cfg` variants of each other go together, unlike those compiled together.
	#[test]
	fn cfg_variants_are_removed_together() {
		let lib = "\
macro_rules! m {
	($($t:tt)*) => {};
}

#[cfg(unix)]
m! { static A = 1; }

#[cfg(not(unix))]
m! { static A = 2; }

m! { static B = 3; }
";
		let dir = TempDir::with_files("macro-calls-cfg", &[("src/lib.rs", lib)]);
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);

		assert!(rscode::edit::remove(&resolver, &paths(&["crate::m!"]), &RemoveOptions::default()).is_err());

		let remove = |paths: &[&str]| rscode::edit::remove(&resolver, &super::paths(paths), &RemoveOptions::default()).unwrap();

		assert_eq!(
			edited(&dir, &remove(&["crate::m![1]", "crate::m![2]"]).edits, "src/lib.rs"),
			"macro_rules! m {\n\t($($t:tt)*) => {};\n}\n\nm! { static B = 3; }\n"
		);
		assert_eq!(remove(&["crate::A"]).removed.len(), 2);
	}

	/// The `static` entries of an invocation are statics of the invocation, named like items of its module when it
	/// binds nothing else of their name, and bound in no scope.
	#[test]
	fn entries_are_found_viewed_replaced_and_removed() {
		let dir = TempDir::with_files("macro-entries", &[("src/lib.rs", LIB)]);
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let found = rscode::Find::new().pattern("SAY").unwrap().run_with(&resolver).unwrap();

		assert_eq!((found[0].path.as_str(), found[0].kind), ("fixture::SAY", ItemKind::Static));
		assert_eq!((found[0].thread_local, found[0].entry_macro.as_deref()), (false, Some("commands")));

		let views = rscode::View::new().path("crate::HELP").unwrap().run_with(&resolver).unwrap();

		assert_eq!(views[0].text, "static HELP = 2;");
		assert_eq!(views[0].entry_macro.as_deref(), Some("commands"));

		// code paths do not reach them: what the macro makes of them is unknown
		let root = ws.crates()[0].root_module();
		let say = rscode::model::PathRef {
			leading_colon: false,
			segments: vec![rscode::model::PathSegmentRef { name: "SAY".into(), range: Default::default(), has_arguments: false }],
		};

		assert!(resolver.resolve_path(root, &say, rscode::resolve::Namespace::Value).is_empty());

		// replaced as entries, keeping the `;` they had
		let replaced = |path: &str, source: &str| {
			let replacement = rscode::edit::replace(&resolver, &super::path(path), source, &ReplaceOptions::default()).unwrap();

			edited(&dir, &replacement.edits, "src/lib.rs")
		};

		assert_eq!(replaced("crate::QUIT", "static QUIT = 4"), LIB.replace("QUIT = 3", "QUIT = 4"));
		assert_eq!(replaced("crate::HELP", "/// Help.\nstatic HELP = 5;"), LIB.replace("static HELP = 2", "/// Help.\n\tstatic HELP = 5"));

		let function = rscode::edit::replace(&resolver, &path("crate::SAY"), "fn say() {}", &ReplaceOptions::default());

		assert!(matches!(function, Err(Error::InvalidSource(_))), "{function:?}");

		// removed with their `;`, and the invocation goes with the last one
		let removal = rscode::edit::remove(&resolver, &paths(&["crate::HELP"]), &RemoveOptions::default()).unwrap();

		assert_eq!(edited(&dir, &removal.edits, "src/lib.rs"), LIB.replace("\tstatic HELP = 2;\n", ""));
		assert_eq!(removal.warnings.len(), 1, "{:?}", removal.warnings);
		assert!(removal.warnings[0].starts_with("the uses of removed entries of macro invocations"), "{:?}", removal.warnings);

		let removal = rscode::edit::remove(&resolver, &paths(&["crate::QUIT"]), &RemoveOptions::default()).unwrap();

		assert_eq!(edited(&dir, &removal.edits, "src/lib.rs"), LIB.replace("commands! {\n\tstatic QUIT = 3;\n}\n\n", ""));

		match rscode::edit::rename(&resolver, &path("crate::SAY"), "TALK", &rscode::edit::RenameOptions::default()) {
			Err(Error::Unsupported(message)) => assert!(message.contains("is a static declared by a macro invocation"), "{message}"),
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn other_bodies_have_no_entries() {
		let lib = "macro_rules! m {\n\t($($t:tt)*) => {};\n}\n\nm! {\n\tstatic A = 1;\n\tfn f() {}\n}\n\npub static A: u8 = 2;\n";
		let dir = TempDir::with_files("macro-entries-none", &[("src/lib.rs", lib)]);
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let named: Vec<String> = (resolver.resolve_item_path(&path("crate::A")).into_iter())
			.map(|item| resolver.canonical_path(item).to_string())
			.collect();

		assert_eq!(named, ["fixture::A"]);
		assert_eq!(ws.children(resolver.resolve_item_path(&path("crate::m!"))[0]).count(), 0);
	}

	#[test]
	fn applies_macro_call_edits_and_still_compiles() {
		let dir = TempDir::with_files(
			"macro-calls-apply",
			&[("Cargo.toml", "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n"), ("src/lib.rs", LIB)],
		);
		let ws = load(&dir);
		let resolver = Resolver::new(&ws);
		let source = "commands! {\n\tstatic QUIT = 4;\n\tstatic STOP = 5;\n}";
		let mut edits = rscode::edit::replace(&resolver, &path("crate::commands![2]"), source, &ReplaceOptions::default())
			.unwrap()
			.edits;

		// an entry without its `;`, which the macro needs
		edits.extend(rscode::edit::replace(&resolver, &path("crate::HELP"), "static HELP = 7", &ReplaceOptions::default()).unwrap().edits);
		edits.apply().unwrap();

		assert!(dir.read("src/lib.rs").contains("\tstatic HELP = 7;\n}\n") && dir.read("src/lib.rs").contains("\tstatic STOP = 5;\n}\n"));
		cargo_check(&dir);
	}
}
