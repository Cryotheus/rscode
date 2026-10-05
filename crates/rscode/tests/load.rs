//! Loading crates: module file resolution, item extraction, locations, `cfg`s, and diagnostics.

mod common;

use common::locked_version;
use common::registry_crate;
use rscode::CfgContext;
use rscode::CrateId;
use rscode::CrateSpec;
use rscode::ItemId;
use rscode::ItemKind;
use rscode::Tristate;
use rscode::Workspace;
use rscode::model::DataShape;
use rscode::model::FnInfo;
use rscode::model::ItemDetail;
use rscode::model::Receiver;
use rscode::model::Severity;
use rscode::model::TypeRef;
use rscode::model::Visibility;
use rscode::source::LineCol;
use rscode::source::TextRange;
use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

fn fixture(name: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Loads a crate rooted at `root` (relative to the fixture directory `name`), which is also the workspace root.
fn load_with(name: &str, root: &str, cfg: CfgContext) -> (Workspace, CrateId) {
	let mut workspace = Workspace::new(fixture(name));
	let mut spec = CrateSpec::new(name, fixture(name).join(root));

	spec.cfg = cfg;

	let krate = workspace.load_crate(spec);

	(workspace, krate)
}

/// Loads a fixture crate with an empty cfg context (every `cfg` is unknown).
fn load(name: &str, root: &str) -> (Workspace, CrateId) {
	load_with(name, root, CfgContext::new())
}

/// A Linux-like configuration: `unix`, `target_os = "linux"`, and `debug_assertions` are set; features are unknown.
fn linux() -> CfgContext {
	let mut cfg = CfgContext::from_rustc_print_cfg("unix\ntarget_os=\"linux\"\ntarget_family=\"unix\"\n");

	cfg.set_name("debug_assertions", true);
	cfg
}

/// Finds an item by the names of its ancestors below the crate root.
fn find(workspace: &Workspace, krate: CrateId, path: &[&str]) -> ItemId {
	let mut current = workspace.krate(krate).root_module();

	for name in path {
		current = workspace
			.children(current)
			.find(|&child| workspace.item(child).name() == Some(name))
			.unwrap_or_else(|| panic!("no `{name}` in {path:?}"));
	}

	current
}

/// The `n`th child of `parent` of a kind.
fn nth(workspace: &Workspace, parent: ItemId, kind: ItemKind, n: usize) -> ItemId {
	workspace
		.children(parent)
		.filter(|&child| workspace.item(child).kind == kind)
		.nth(n)
		.unwrap_or_else(|| panic!("no {kind} #{n}"))
}

/// The text of a range of an item's file.
fn slice(workspace: &Workspace, id: ItemId, range: TextRange) -> &str {
	workspace.file_of(id).slice(range)
}

fn name_text(workspace: &Workspace, id: ItemId) -> &str {
	slice(workspace, id, workspace.item(id).name_range.expect("a name range"))
}

/// The rest of the line at an offset of an item's file.
fn line_at(workspace: &Workspace, id: ItemId, offset: usize) -> &str {
	workspace.file_of(id).text()[offset..].lines().next().unwrap_or("")
}

fn body_text(workspace: &Workspace, id: ItemId) -> &str {
	slice(workspace, id, workspace.item(id).body().expect("a body"))
}

/// A path relative to the workspace root, with `/` separators.
fn relative(workspace: &Workspace, path: &Path) -> String {
	workspace.display_path(path).display().to_string().replace(std::path::MAIN_SEPARATOR, "/")
}

/// The item tree of a crate, one item per line.
fn tree(workspace: &Workspace, krate: CrateId) -> String {
	fn walk(workspace: &Workspace, id: ItemId, depth: usize, out: &mut String) {
		let _ = writeln!(out, "{}{}", "  ".repeat(depth), label(workspace, id));

		for child in workspace.children(id) {
			walk(workspace, child, depth + 1, out);
		}
	}

	let mut out = String::new();

	walk(workspace, workspace.krate(krate).root_module(), 0, &mut out);
	out
}

fn label(workspace: &Workspace, id: ItemId) -> String {
	let item = workspace.item(id);
	let name = item.name().unwrap_or("_");

	match &item.detail {
		ItemDetail::Module(info) if !info.inline && !id.is_crate_root() => {
			let path = relative(workspace, info.file_path.as_deref().expect("a module file path"));
			let error = if info.load_error.is_some() { " (error)" } else { "" };

			format!("mod {name} [{path}{error}]")
		}

		ItemDetail::Impl(info) => {
			let unsafety = if info.is_unsafe { "unsafe " } else { "" };
			let negative = if info.negative { "!" } else { "" };

			match &info.trait_text {
				Some(trait_text) => format!("{unsafety}impl {negative}{trait_text} for {}", info.self_ty_text),
				None => format!("{unsafety}impl {}", info.self_ty_text),
			}
		}

		ItemDetail::Import(info) => {
			let suffix = if info.glob {
				"::*"
			} else if info.is_self {
				"::{self}"
			} else {
				""
			};

			let alias = info.alias.as_ref().map(|alias| format!(" as {alias}")).unwrap_or_default();

			format!("import {name}: {}{suffix}{alias}", info.path)
		}

		ItemDetail::Macro { path, .. } if item.name.is_none() => format!("{} {path}!", item.kind),
		_ => format!("{} {name}", item.kind),
	}
}

/// Diagnostics as `severity file:line:column: message`, with paths relative to the fixture (and `/` separators).
fn diagnostics(workspace: &Workspace, krate: CrateId) -> Vec<String> {
	// messages have paths as the loader makes them: absolute and normalized (on Windows, with `\` only)
	let root = format!("{}{}", std::path::absolute(workspace.root()).unwrap().display(), std::path::MAIN_SEPARATOR);

	workspace
		.krate(krate)
		.diagnostics()
		.iter()
		.map(|diagnostic| {
			let severity = match diagnostic.severity {
				Severity::Error => "error",
				Severity::Warning => "warning",
			};

			let file = diagnostic.file.as_deref().map(|file| relative(workspace, file)).unwrap_or_default();
			let location = diagnostic.location.map(|location| format!(":{location}")).unwrap_or_default();

			let message = diagnostic.message.replace(&root, "").replace(std::path::MAIN_SEPARATOR, "/");

			format!("{severity} {file}{location}: {message}")
		})
		.collect()
}

fn errors(workspace: &Workspace, krate: CrateId) -> Vec<String> {
	diagnostics(workspace, krate).into_iter().filter(|diagnostic| diagnostic.starts_with("error")).collect()
}

/// Where `proc_macro2` numbers the next text parsed on this thread from: it numbers the bytes of everything parsed on
/// a thread, for as long as the thread lives, and spans show the numbers.
fn next_parsed_offset() -> usize {
	let token = "x".parse::<proc_macro2::TokenStream>().unwrap().into_iter().next().unwrap();
	let span = format!("{:?}", token.span());
	let start = span.strip_prefix("bytes(").and_then(|span| span.split("..").next());

	start.and_then(|start| start.parse().ok()).unwrap_or_else(|| panic!("unexpected span: {span}"))
}

#[test]
fn parses_on_threads_of_its_own() {
	let root = Path::new(env!("CARGO_MANIFEST_DIR"));
	let before = next_parsed_offset();
	let mut workspace = Workspace::new(root);

	let krate = workspace.load_crate(CrateSpec::new("rscode", root.join("src/lib.rs")));

	workspace.link();

	let resolver = rscode::Resolver::new(&workspace);
	let views = rscode::View::new().path("crate::edit").unwrap().run_with(&resolver).unwrap();
	let replacement = rscode::edit::replace(
		&resolver,
		&rscode::ItemPath::parse("crate::edit::EditSet::new").unwrap(),
		"pub fn new() -> Self {\n\tSelf::default()\n}",
		&rscode::edit::ReplaceOptions::default(),
	)
	.unwrap();

	replacement.edits.preview().unwrap();

	// only the probes themselves were parsed here
	assert!(!views.is_empty() && workspace.krate(krate).files().len() > 20);
	assert!(next_parsed_offset() - before < 10, "{before} -> {}", next_parsed_offset());
}

#[test]
fn resolves_module_files_like_rustc() {
	let (workspace, krate) = load("load_modules", "src/lib.rs");

	let expected = "\
mod load_modules
  mod a [src/a.rs]
    mod b [src/a/b.rs]
      fn in_a_b
    mod c [src/c.rs]
      fn in_c
    mod inl
      mod d [src/a/inl/d.rs]
        fn in_a_inl_d
      mod e [src/a/inl/e.rs]
        fn in_a_inl_e
    mod inl_path
      mod d [src/x/d.rs]
        fn in_x_d
  mod b [src/b/mod.rs]
    mod child [src/b/child.rs]
      fn in_b_child
    mod rel [src/b/rel.rs]
      fn in_b_rel
  mod o [src/other/o.rs]
    mod child [src/other/child.rs]
      fn in_other_child
  mod m
    mod pp [src/m/pp.rs]
      fn in_m_pp
    mod q [src/m/q.rs]
      fn in_m_q
  mod pdir
    mod c [src/p/c.rs]
      fn in_p_c
  mod both [src/both.rs]
    fn in_both
  mod missing [src/missing.rs (error)]
  mod missing_inactive [src/missing_inactive.rs (error)]
  mod missing_maybe [src/missing_maybe.rs (error)]
  mod ca [src/via_cfg_attr.rs]
    fn in_via_cfg_attr
  mod fx [src/feature_x.rs]
    fn in_feature_x
  mod multi [src/first.rs]
    fn in_first
  mod type [src/type.rs]
    fn in_type
  mod cycle [src/cycle_a.rs]
    mod b [src/cycle_b.rs]
      mod a [src/cycle_a.rs (error)]
  mod broken [src/broken.rs (error)]
  mod shared1 [src/shared.rs]
    fn shared
  mod shared2 [src/shared.rs]
    fn shared
  mod inactive
    mod nested_missing [src/inactive/nested_missing.rs (error)]
  mod up [outside/up.rs]
    fn up
";

	assert_eq!(tree(&workspace, krate), expected);
}

#[test]
fn reports_module_problems() {
	let (workspace, krate) = load("load_modules", "src/lib.rs");

	assert_eq!(diagnostics(&workspace, krate), [
		"error src/lib.rs:16:5: file for module `both` found at both `src/both.rs` and `src/both/mod.rs`; using the former",
		"error src/lib.rs:17:5: file not found for module `missing`: neither `src/missing.rs` nor `src/missing/mod.rs` exists",
		"warning src/lib.rs:19:5: file not found for module `missing_inactive`: neither `src/missing_inactive.rs` nor `src/missing_inactive/mod.rs` exists",
		"error src/lib.rs:21:5: file not found for module `missing_maybe`: neither `src/missing_maybe.rs` nor `src/missing_maybe/mod.rs` exists",
		r#"warning src/lib.rs:24:27: cannot tell whether `cfg_attr(feature = "x", path = "feature_x.rs")` applies; assuming it does"#,
		"warning src/lib.rs:27:3: unused `path` attribute: the first one (`first.rs`) applies",
		"error src/cycle_b.rs:2:5: circular modules: `src/cycle_a.rs` -> `src/cycle_b.rs` -> `src/cycle_a.rs`",
		"error src/broken.rs:3:14: cannot parse file: cannot parse string into token stream",
		"warning src/lib.rs:39:6: file not found for module `nested_missing`: neither `src/inactive/nested_missing.rs` nor `src/inactive/nested_missing/mod.rs` exists",
	]);
}

#[test]
fn records_module_details() {
	let (workspace, krate) = load("load_modules", "src/lib.rs");
	let krate_data = workspace.krate(krate);
	let module = |path: &[&str]| workspace.item(find(&workspace, krate, path)).module_info().expect("a module").clone();

	// the crate root
	let root = workspace.item(krate_data.root_module());
	let root_info = root.module_info().unwrap();

	assert_eq!(root.name(), Some("load_modules"));
	assert_eq!(root.range, TextRange::new(0, krate_data.root_file().text().len()));
	assert_eq!(workspace.parent(krate_data.root_module()), None);
	assert!(root_info.dir_owner && !root_info.inline);
	assert_eq!(root_info.file.map(|file| file.index()), Some(0));
	assert_eq!(krate_data.root_file().path(), fixture("load_modules").join("src/lib.rs"));

	// directory ownership
	assert!(!module(&["a"]).dir_owner);
	assert!(module(&["b"]).dir_owner);
	assert!(module(&["o"]).dir_owner);
	assert!(module(&["multi"]).dir_owner);
	assert!(!module(&["both"]).dir_owner);
	assert!(!module(&["m"]).dir_owner);

	// inline modules
	let inline = module(&["m"]);
	let m = find(&workspace, krate, &["m"]);

	assert!(inline.inline && inline.file.is_none() && inline.file_path.is_none());
	assert!(slice(&workspace, m, inline.body.unwrap()).trim_start().starts_with("#[path = \"pp.rs\"]"));
	assert!(workspace.item_text(m).starts_with("mod m {") && workspace.item_text(m).ends_with('}'));
	assert_eq!(workspace.item(m).attrs.path, None);
	assert_eq!(workspace.item(find(&workspace, krate, &["pdir"])).attrs.path.as_deref(), Some("p"));

	// out-of-line modules: the item is the declaration, in the declaring file
	let a = find(&workspace, krate, &["a"]);

	assert_eq!(workspace.item_text(a), "mod a;");
	assert_eq!(workspace.file_of(a).path(), fixture("load_modules").join("src/lib.rs"));
	assert_eq!(workspace.item_text(find(&workspace, krate, &["multi"])), "#[path = \"first.rs\"]\n#[path = \"second.rs\"]\nmod multi;");
	assert_eq!(workspace.item(find(&workspace, krate, &["multi"])).attrs.path.as_deref(), Some("first.rs"));
	assert_eq!(workspace.item(find(&workspace, krate, &["fx"])).attrs.path.as_deref(), Some("feature_x.rs"));

	// raw identifiers name files without `r#`
	let r#type = find(&workspace, krate, &["type"]);

	assert_eq!(name_text(&workspace, r#type), "r#type");

	// errors
	let missing = module(&["missing"]);

	assert!(missing.file.is_none());
	assert_eq!(missing.file_path.as_deref(), Some(fixture("load_modules").join("src/missing.rs").as_path()));
	assert!(missing.load_error.unwrap().starts_with("file not found for module `missing`"));

	let broken = module(&["broken"]);

	assert!(broken.file.is_some(), "an unparsable file is still loaded");
	assert!(broken.load_error.unwrap().starts_with("cannot parse file"));
	assert_eq!(workspace.children(find(&workspace, krate, &["broken"])).count(), 0);

	let cycle = module(&["cycle", "b", "a"]);

	assert!(cycle.file.is_none());
	assert!(cycle.load_error.unwrap().starts_with("circular modules"));

	// files are loaded once and paths are normalized
	assert_eq!(module(&["shared1"]).file, module(&["shared2"]).file);
	assert_eq!(module(&["cycle"]).file_path, module(&["cycle", "b", "a"]).file_path);
	assert_eq!(module(&["up"]).file_path.unwrap(), fixture("load_modules").join("outside/up.rs"));

	let paths: Vec<String> = krate_data.files().iter().map(|file| relative(&workspace, file.path())).collect();
	let mut unique = paths.clone();

	unique.sort();
	unique.dedup();
	assert_eq!(unique.len(), paths.len(), "{paths:?}");
	assert_eq!(paths.len(), 25, "{paths:?}");
	assert!(!paths.iter().any(|path| path.contains("decoy") || path.contains("..")));

	for (index, file) in krate_data.files().iter().enumerate() {
		assert_eq!(krate_data.file_id(file.path()).map(|id| id.index()), Some(index));
	}
}

#[test]
fn evaluates_cfg_attr_paths() {
	// with features known, `feature = "x"` is false: the default file applies, without a warning
	let (workspace, krate) = load_with("load_modules", "src/lib.rs", CfgContext::new().with_features(Vec::<String>::new()));
	let fx = find(&workspace, krate, &["fx"]);

	assert_eq!(workspace.item(fx).attrs.path, None);
	assert_eq!(label(&workspace, fx), "mod fx [src/fx.rs]");
	assert!(!diagnostics(&workspace, krate).iter().any(|diagnostic| diagnostic.contains("feature_x")));

	// `missing_maybe` is definitely inactive now
	let messages = diagnostics(&workspace, krate);

	assert!(messages.iter().any(|message| message.starts_with("warning") && message.contains("`missing_maybe`")));

	let (workspace, krate) = load_with("load_modules", "src/lib.rs", CfgContext::new().with_features(["x"]));

	assert_eq!(label(&workspace, find(&workspace, krate, &["fx"])), "mod fx [src/feature_x.rs]");
	assert!(!diagnostics(&workspace, krate).iter().any(|diagnostic| diagnostic.contains("feature_x")));
}

#[test]
fn extracts_every_item_kind() {
	let (workspace, krate) = load("load_items", "lib.rs");

	let expected = "\
mod load_items
  struct Named
  struct Tuple
  struct Unit
  union Union
  enum Enum
    variant A
    variant B
    variant C
    variant D
  trait Trait
    assoc-const C
    assoc-type A
    assoc-fn required
    assoc-fn provided
    assoc-macro trait_macro!
  trait Auto
  trait-alias Alias
  type Type
  fn function
  fn const_unsafe
  fn asynchronous
  const CONST
  const _
  static STATIC
  static STATIC_MUT
  macro-rules local_macro
  macro-rules exported_macro
  macro-call local_macro!
  macro-call some::path::call!
  extern-crate alloc
  extern-crate my_core
  extern-crate _
  extern-block _
    foreign-fn foreign_fn
    foreign-static FOREIGN_STATIC
    foreign-static FOREIGN_MUT
    foreign-type Opaque
    foreign-macro foreign_macro!
  extern-block _
    foreign-fn safe_foreign
  extern-block _
    foreign-fn bare_abi
  impl Named
    assoc-fn new
    assoc-fn by_ref
    assoc-fn by_mut
    assoc-fn by_value
    assoc-fn by_mut_value
    assoc-fn boxed
    assoc-const K
    assoc-macro impl_macro!
  impl Clone for Named
    assoc-fn clone
  impl !Send for Unit
  unsafe impl Sync for Unit
  impl Trait for &T
  impl From<&'a str> for Tuple
    assoc-fn from
  impl crate::inner::Local for [Unit; 2]
  mod inner
    fn sup
    fn slf
    fn in_path
    trait Local
  use _
    import collections: std::collections::{self}
    import Map: std::collections::HashMap as Map
    import Entry: std::collections::hash_map::Entry
    import _: std::collections::hash_map::*
  use _
    import fmt: ::core::fmt
  use _
    import _: self::inner::sup as _
  use _
    import Renamed: crate::Named as Renamed
    import _: crate::Enum::*
  struct match
  fn ünïcödé
  fn no_body
  trait ConstTrait
    assoc-fn f
  impl ConstTrait for Unit
    assoc-fn f
  macro-rules decl_macro
  trait Restricted
  static NO_VALUE
  const NO_VALUE_CONST
  type Bounded
  use _
    import fmt2: ::std::fmt as fmt2
    import slf: inner::slf
  use _
    import sup: inner::sup
    import rc: ::std::rc
    import V: ::std::vec::Vec as V
    import U: crate::Unit as U
";

	assert_eq!(tree(&workspace, krate), expected);
	assert_eq!(diagnostics(&workspace, krate), Vec::<String>::new());

	// the arena is in document order and parents link back
	for (id, item) in workspace.krate(krate).items() {
		for child in workspace.children(id) {
			let child_item = workspace.item(child);

			assert_eq!(workspace.parent(child), Some(id));
			assert!(child > id);
			let contained = child_item.file != item.file || item.range.contains_range(child_item.range);

			assert!(contained, "{} contains {}", label(&workspace, id), label(&workspace, child));
		}
	}
}

#[test]
fn records_ranges_names_and_attributes() {
	let (workspace, krate) = load("load_items", "lib.rs");
	let named = find(&workspace, krate, &["Named"]);
	let item = workspace.item(named);

	// the plain comment above is not part of the item; doc comments and attributes are
	assert_eq!(workspace.item_text(named), "/// Named docs.\n#[derive(Debug)]\npub struct Named {\n\tpub a: u8,\n\tb: u16,\n}");
	assert_eq!(name_text(&workspace, named), "Named");
	assert_eq!(slice(&workspace, named, item.attrs.docs.unwrap()), "/// Named docs.");
	assert_eq!(line_at(&workspace, named, item.attrs.after_attrs), "pub struct Named {");
	assert_eq!(item.file.index(), 0);

	// no outer attributes: `after_attrs` is the start; inner attributes are inside
	let r#trait = find(&workspace, krate, &["Trait"]);
	let trait_item = workspace.item(r#trait);

	assert_eq!(trait_item.attrs.after_attrs, trait_item.range.start);
	assert_eq!(trait_item.attrs.docs, None);
	assert!(workspace.item_text(r#trait).starts_with("pub unsafe trait Trait: Clone {\n\t#![allow(unused)]"));
	assert!(body_text(&workspace, r#trait).starts_with("\n\t#![allow(unused)]\n\n\tconst C: u8 = 1;"));
	assert!(body_text(&workspace, r#trait).ends_with("trait_macro!();\n"));

	// variants include their attributes but not the separating comma
	let d = find(&workspace, krate, &["Enum", "D"]);

	assert_eq!(workspace.item_text(d), "/// Variant docs.\n\t#[cfg(test)]\n\tD");
	assert_eq!(workspace.item_text(find(&workspace, krate, &["Enum", "C"])), "C { x: i8 } = 3");

	// raw and unicode identifiers
	let r#match = find(&workspace, krate, &["match"]);

	assert_eq!(name_text(&workspace, r#match), "r#match");
	assert_eq!(workspace.item_text(r#match), "pub struct r#match;");

	let unicode = find(&workspace, krate, &["ünïcödé"]);
	let unicode_name = workspace.item(unicode).name_range.unwrap();
	let start = workspace.file_of(unicode).line_col(unicode_name.start);

	assert_eq!(name_text(&workspace, unicode), "ünïcödé");
	assert_eq!(start.column, 8);
	assert_eq!(workspace.file_of(unicode).line_col(unicode_name.end), LineCol {
		line: start.line,
		column: 15,
	});

	// flags
	assert!(workspace.item(find(&workspace, krate, &["exported_macro"])).attrs.macro_export);
	assert!(!workspace.item(find(&workspace, krate, &["local_macro"])).attrs.macro_export);

	// unnamed items have no name range
	let const_underscore = nth(&workspace, workspace.krate(krate).root_module(), ItemKind::Const, 1);

	assert_eq!(workspace.item(const_underscore).name, None);
	assert_eq!(workspace.item(const_underscore).name_range, None);
	assert_eq!(workspace.item_text(const_underscore), "const _: () = ();");
}

#[test]
fn records_visibility() {
	let (workspace, krate) = load("load_items", "lib.rs");
	let vis = |path: &[&str]| workspace.item(find(&workspace, krate, path)).vis.to_string();

	assert_eq!(vis(&["Named"]), "pub");
	assert_eq!(vis(&["Tuple"]), "pub(crate)");
	assert_eq!(vis(&["Unit"]), "private");
	assert_eq!(vis(&["inner", "sup"]), "pub(super)");
	assert_eq!(vis(&["inner", "slf"]), "pub(self)");
	assert_eq!(vis(&["inner", "in_path"]), "pub(in crate::inner)");
	assert_eq!(vis(&["my_core"]), "pub");
	assert_eq!(vis(&["Enum", "A"]), "inherited");
	assert_eq!(vis(&["Trait", "required"]), "inherited");

	let root = workspace.krate(krate).root_module();
	let inherent = nth(&workspace, root, ItemKind::Impl, 0);
	let clone = nth(&workspace, root, ItemKind::Impl, 1);

	assert_eq!(workspace.item(inherent).vis, Visibility::Private);
	assert_eq!(workspace.item(nth(&workspace, inherent, ItemKind::AssocFn, 0)).vis, Visibility::Public);
	assert_eq!(workspace.item(nth(&workspace, inherent, ItemKind::AssocFn, 1)).vis, Visibility::Private);
	assert_eq!(workspace.item(nth(&workspace, inherent, ItemKind::AssocConst, 0)).vis, Visibility::Crate);
	assert_eq!(workspace.item(nth(&workspace, clone, ItemKind::AssocFn, 0)).vis, Visibility::Inherited);

	let Visibility::InPath(path) = &workspace.item(find(&workspace, krate, &["inner", "in_path"])).vis else {
		panic!("not `pub(in ..)`");
	};

	let in_path = find(&workspace, krate, &["inner", "in_path"]);

	assert_eq!(path.names().collect::<Vec<_>>(), ["crate", "inner"]);
	assert_eq!(slice(&workspace, in_path, path.segments[1].range), "inner");

	// foreign items and imports
	let extern_block = nth(&workspace, root, ItemKind::ExternBlock, 0);

	assert_eq!(workspace.item(nth(&workspace, extern_block, ItemKind::ForeignFn, 0)).vis, Visibility::Public);
	assert_eq!(workspace.item(nth(&workspace, extern_block, ItemKind::ForeignStatic, 1)).vis, Visibility::Private);

	let use_fmt = nth(&workspace, root, ItemKind::Use, 1);

	assert_eq!(workspace.item(use_fmt).vis, Visibility::Public);
	assert_eq!(workspace.item(nth(&workspace, use_fmt, ItemKind::Import, 0)).vis, Visibility::Public);
}

#[test]
fn records_item_details() {
	let (workspace, krate) = load("load_items", "lib.rs");
	let root = workspace.krate(krate).root_module();
	let detail = |path: &[&str]| workspace.item(find(&workspace, krate, path)).detail.clone();

	// data
	let named = find(&workspace, krate, &["Named"]);

	assert!(matches!(detail(&["Named"]), ItemDetail::Data { shape: DataShape::Named, .. }));
	assert_eq!(body_text(&workspace, named), "\n\tpub a: u8,\n\tb: u16,\n");
	assert!(matches!(detail(&["Tuple"]), ItemDetail::Data { shape: DataShape::Tuple, .. }));
	assert_eq!(body_text(&workspace, find(&workspace, krate, &["Tuple"])), "u8, pub u16");
	assert_eq!(detail(&["Unit"]), ItemDetail::Data { shape: DataShape::Unit, body: None });
	assert_eq!(body_text(&workspace, find(&workspace, krate, &["Union"])), "\n\ta: u32,\n\tb: f32,\n");
	assert!(matches!(detail(&["Enum"]), ItemDetail::Data { shape: DataShape::Named, .. }));
	assert!(body_text(&workspace, find(&workspace, krate, &["Enum"])).starts_with("\n\tA,\n\tB(u8),"));
	assert_eq!(detail(&["Enum", "A"]), ItemDetail::Data { shape: DataShape::Unit, body: None });
	assert_eq!(body_text(&workspace, find(&workspace, krate, &["Enum", "B"])), "u8");
	assert_eq!(body_text(&workspace, find(&workspace, krate, &["Enum", "C"])), " x: i8 ");

	// functions
	let fn_info = |id: ItemId| workspace.item(id).fn_info().expect("a function").clone();
	let function = find(&workspace, krate, &["function"]);

	assert_eq!(slice(&workspace, function, fn_info(function).body.unwrap()), "{}");
	assert!(fn_info(find(&workspace, krate, &["const_unsafe"])).is_const);
	assert!(fn_info(find(&workspace, krate, &["const_unsafe"])).is_unsafe);
	assert!(fn_info(find(&workspace, krate, &["asynchronous"])).is_async);

	let inherent = nth(&workspace, root, ItemKind::Impl, 0);
	let method = |name: &str| workspace.children(inherent).find(|&id| workspace.item(id).name() == Some(name)).unwrap();
	let receiver = |name: &str| fn_info(method(name)).receiver;

	assert_eq!(receiver("new"), None);
	assert_eq!(receiver("by_ref"), Some(Receiver { reference: true, mutable: false, typed: false }));
	assert_eq!(receiver("by_mut"), Some(Receiver { reference: true, mutable: true, typed: false }));
	assert_eq!(receiver("by_value"), Some(Receiver { reference: false, mutable: false, typed: false }));
	assert_eq!(receiver("by_mut_value"), Some(Receiver { reference: false, mutable: true, typed: false }));
	assert_eq!(receiver("boxed"), Some(Receiver { reference: false, mutable: false, typed: true }));

	let new = nth(&workspace, inherent, ItemKind::AssocFn, 0);

	assert_eq!(slice(&workspace, new, fn_info(new).body.unwrap()), "{\n\t\ttodo!()\n\t}");

	// trait items: required methods have no body
	let required = find(&workspace, krate, &["Trait", "required"]);
	let provided = find(&workspace, krate, &["Trait", "provided"]);

	assert_eq!(fn_info(required), FnInfo {
		receiver: Some(Receiver { reference: true, mutable: false, typed: false }),
		..FnInfo::default()
	});

	assert_eq!(slice(&workspace, provided, fn_info(provided).body.unwrap()), "{}");
	assert_eq!(detail(&["Trait"]), ItemDetail::Trait {
		is_unsafe: true,
		is_auto: false,
		body: workspace.item(find(&workspace, krate, &["Trait"])).body(),
	});
	assert!(matches!(detail(&["Auto"]), ItemDetail::Trait { is_unsafe: false, is_auto: true, body: Some(_) }));
	assert_eq!(detail(&["Alias"]), ItemDetail::Trait { is_unsafe: false, is_auto: false, body: None });

	// statics
	assert_eq!(detail(&["STATIC"]), ItemDetail::Static { mutable: false, thread_local: false });
	assert_eq!(detail(&["STATIC_MUT"]), ItemDetail::Static { mutable: true, thread_local: false });

	// macros
	let local_macro = find(&workspace, krate, &["local_macro"]);

	assert!(matches!(detail(&["local_macro"]), ItemDetail::Macro { path, .. } if path == "macro_rules"));
	assert_eq!(body_text(&workspace, local_macro), "\n\t() => {};\n");

	let call = nth(&workspace, root, ItemKind::MacroCall, 1);

	assert!(matches!(&workspace.item(call).detail, ItemDetail::Macro { path, .. } if path == "some::path::call"));
	assert_eq!(body_text(&workspace, call), " tokens ");
	assert_eq!(workspace.item_text(call), "some::path::call! { tokens }");
	assert_eq!(workspace.item_text(nth(&workspace, root, ItemKind::MacroCall, 0)), "local_macro!();");

	// extern crates
	assert_eq!(detail(&["alloc"]), ItemDetail::ExternCrate { crate_name: "alloc".into(), alias: None });
	assert_eq!(detail(&["my_core"]), ItemDetail::ExternCrate { crate_name: "core".into(), alias: Some("my_core".into()) });
	assert_eq!(name_text(&workspace, find(&workspace, krate, &["my_core"])), "my_core");

	let std_underscore = nth(&workspace, root, ItemKind::ExternCrate, 2);

	assert_eq!(workspace.item(std_underscore).detail, ItemDetail::ExternCrate { crate_name: "std".into(), alias: Some("_".into()) });
	assert_eq!(workspace.item(std_underscore).name, None);

	// extern blocks
	let c_block = nth(&workspace, root, ItemKind::ExternBlock, 0);

	let unsafe_block = nth(&workspace, root, ItemKind::ExternBlock, 1);
	let bare_block = nth(&workspace, root, ItemKind::ExternBlock, 2);

	assert!(matches!(&workspace.item(c_block).detail, ItemDetail::ExternBlock { abi: Some(abi), is_unsafe: false, .. } if abi == "C"));
	assert!(matches!(workspace.item(unsafe_block).detail, ItemDetail::ExternBlock { is_unsafe: true, .. }));
	assert!(matches!(workspace.item(bare_block).detail, ItemDetail::ExternBlock { abi: None, .. }));
	assert_eq!(workspace.item(nth(&workspace, c_block, ItemKind::ForeignStatic, 1)).detail, ItemDetail::Static { mutable: true, thread_local: false });
	assert!(!fn_info(nth(&workspace, unsafe_block, ItemKind::ForeignFn, 0)).is_unsafe);
	assert_eq!(fn_info(nth(&workspace, c_block, ItemKind::ForeignFn, 0)).body, None);
}

#[test]
fn records_impl_blocks() {
	let (workspace, krate) = load("load_items", "lib.rs");
	let root = workspace.krate(krate).root_module();
	let impl_info = |n: usize| workspace.item(nth(&workspace, root, ItemKind::Impl, n)).impl_info().expect("an impl").clone();

	let inherent = impl_info(0);
	let inherent_id = nth(&workspace, root, ItemKind::Impl, 0);

	assert_eq!(inherent.self_ty_text, "Named");
	assert_eq!(inherent.trait_path, None);
	assert_eq!(inherent.self_ty.base_path().unwrap().names().collect::<Vec<_>>(), ["Named"]);
	assert!(slice(&workspace, inherent_id, inherent.body).starts_with("\n\t#![allow(unused)]\n\n\tpub fn new() -> Self {"));
	assert!(workspace.item_text(inherent_id).starts_with("impl Named {"));

	let negative = impl_info(2);

	assert!(negative.negative && !negative.is_unsafe);
	assert_eq!(negative.trait_text.as_deref(), Some("Send"));

	let unsafe_impl = impl_info(3);

	assert!(unsafe_impl.is_unsafe && !unsafe_impl.negative);
	assert!(workspace.item_text(nth(&workspace, root, ItemKind::Impl, 3)).starts_with("unsafe impl Sync"));

	// `&T`: an indirection to a path
	let reference = impl_info(4);

	assert_eq!(reference.self_ty, TypeRef::Indirect {
		inner: Box::new(TypeRef::Path {
			path: reference.self_ty.base_path().unwrap().clone()
		}),
	});
	assert_eq!(reference.self_ty.base_path().unwrap().names().collect::<Vec<_>>(), ["T"]);

	// generic arguments
	let from = impl_info(5);
	let from_id = nth(&workspace, root, ItemKind::Impl, 5);
	let trait_path = from.trait_path.unwrap();

	assert_eq!(trait_path.names().collect::<Vec<_>>(), ["From"]);
	assert!(trait_path.segments[0].has_arguments);
	assert_eq!(slice(&workspace, from_id, trait_path.segments[0].range), "From");

	// arrays are indirections; trait paths keep every segment
	let local = impl_info(6);
	let local_path = local.trait_path.unwrap();

	assert!(matches!(local.self_ty, TypeRef::Indirect { .. }));
	assert_eq!(local.self_ty.base_path().unwrap().names().collect::<Vec<_>>(), ["Unit"]);
	assert_eq!(local_path.to_string(), "crate::inner::Local");
	assert!(!local_path.segments[2].has_arguments);
}

#[test]
fn flattens_use_trees() {
	let (workspace, krate) = load("load_items", "lib.rs");
	let root = workspace.krate(krate).root_module();
	let collections = nth(&workspace, root, ItemKind::Use, 0);
	let imports: Vec<ItemId> = workspace.children(collections).collect();
	let info = |id: ItemId| workspace.item(id).import_info().expect("an import").clone();

	assert_eq!(workspace.item_text(collections), "use std::collections::{self, HashMap as Map, hash_map::{Entry, *}};");

	// `self` binds the module's name at the `self` token
	let this = imports[0];

	assert!(info(this).is_self);
	assert_eq!(info(this).path.to_string(), "std::collections");
	assert_eq!(workspace.item(this).name.as_deref(), Some("collections"));
	assert_eq!(workspace.item_text(this), "self");
	assert_eq!(name_text(&workspace, this), "self");
	assert_eq!(workspace.item(this).cfg, None);

	// renames bind the alias
	let map = imports[1];

	assert_eq!(workspace.item_text(map), "HashMap as Map");
	assert_eq!(name_text(&workspace, map), "Map");
	assert_eq!(slice(&workspace, map, info(map).alias_range.unwrap()), "Map");
	assert_eq!(slice(&workspace, map, info(map).path.segments[2].range), "HashMap");

	// leaves of nested groups
	let entry = imports[2];

	assert_eq!(workspace.item_text(entry), "Entry");
	assert_eq!(info(entry).path.to_string(), "std::collections::hash_map::Entry");
	assert_eq!(slice(&workspace, entry, info(entry).path.segments[0].range), "std");

	// globs bind nothing
	let glob = imports[3];

	assert!(info(glob).glob);
	assert_eq!(info(glob).path.to_string(), "std::collections::hash_map");
	assert_eq!(workspace.item_text(glob), "*");
	assert_eq!(workspace.item(glob).name_range, None);

	// a leading `::`
	let fmt = nth(&workspace, nth(&workspace, root, ItemKind::Use, 1), ItemKind::Import, 0);

	assert!(info(fmt).path.leading_colon);
	assert_eq!(workspace.item_text(fmt), "::core::fmt");
	assert_eq!(name_text(&workspace, fmt), "fmt");

	// `as _` binds nothing, but keeps the alias
	let underscore = nth(&workspace, nth(&workspace, root, ItemKind::Use, 2), ItemKind::Import, 0);

	assert_eq!(workspace.item(underscore).name, None);
	assert_eq!(workspace.item(underscore).name_range, None);
	assert_eq!(info(underscore).alias.as_deref(), Some("_"));
	assert_eq!(slice(&workspace, underscore, info(underscore).alias_range.unwrap()), "_");

	// leaves in a group with a path prefix span the prefix
	let variants = nth(&workspace, nth(&workspace, root, ItemKind::Use, 3), ItemKind::Import, 1);

	assert_eq!(workspace.item_text(variants), "Enum::*");
	assert_eq!(info(variants).path.to_string(), "crate::Enum");

	// `use {::a, b};` is not modeled by syn
	let rooted = nth(&workspace, root, ItemKind::Use, 4);
	let fmt2 = nth(&workspace, rooted, ItemKind::Import, 0);
	let slf = nth(&workspace, rooted, ItemKind::Import, 1);

	assert_eq!(workspace.item_text(rooted), "use {::std::fmt as fmt2, inner::slf};");
	assert_eq!(workspace.item_text(fmt2), "::std::fmt as fmt2");
	assert!(info(fmt2).path.leading_colon);
	assert_eq!(name_text(&workspace, fmt2), "fmt2");
	assert_eq!(workspace.item_text(slf), "inner::slf");
	assert!(!info(slf).path.leading_colon);

	// ... also in nested groups
	let nested = nth(&workspace, root, ItemKind::Use, 5);
	let texts: Vec<&str> = workspace.children(nested).map(|import| workspace.item_text(import)).collect();
	let names: Vec<&str> = workspace.children(nested).map(|import| name_text(&workspace, import)).collect();
	let rooted: Vec<bool> = workspace.children(nested).map(|import| info(import).path.leading_colon).collect();

	assert_eq!(workspace.item_text(nested), "pub use {inner::sup, {::std::rc, {::std::vec::Vec as V}}, crate::{Unit as U}};");
	assert_eq!(texts, ["inner::sup", "::std::rc", "::std::vec::Vec as V", "Unit as U"]);
	assert_eq!(names, ["sup", "rc", "V", "U"]);
	assert_eq!(rooted, [false, true, true, false]);
	assert!(workspace.children(nested).all(|import| workspace.item(import).vis == Visibility::Public));
}

/// The range of an import is its element of the innermost group, which can be removed with a comma.
#[test]
fn import_ranges_are_group_elements() {
	let (workspace, krate) = load("load_items", "lib.rs");
	let root = workspace.krate(krate).root_module();
	let text = |use_index: usize| -> Vec<&str> {
		let use_item = nth(&workspace, root, ItemKind::Use, use_index);

		workspace.children(use_item).map(|import| workspace.item_text(import)).collect()
	};

	// `use std::collections::{self, HashMap as Map, hash_map::{Entry, *}};`
	assert_eq!(text(0), ["self", "HashMap as Map", "Entry", "*"]);

	// `pub use ::core::fmt;`
	assert_eq!(text(1), ["::core::fmt"]);

	// `use crate::{Named as Renamed, Enum::*};`
	assert_eq!(text(3), ["Named as Renamed", "Enum::*"]);

	// the leaf's own use tree is a suffix of the range, starting at the last path segment
	let variants = nth(&workspace, nth(&workspace, root, ItemKind::Use, 3), ItemKind::Import, 1);
	let renamed = nth(&workspace, nth(&workspace, root, ItemKind::Use, 3), ItemKind::Import, 0);
	let leaf = |id: ItemId| {
		let item = workspace.item(id);
		let info = item.import_info().unwrap();
		let start = if info.glob { item.range.end - 1 } else { info.path.segments.last().unwrap().range.start };

		slice(&workspace, id, TextRange::new(start, item.range.end))
	};

	assert_eq!(leaf(variants), "*");
	assert_eq!(leaf(renamed), "Named as Renamed");
}

#[test]
fn classifies_verbatim_items() {
	let (workspace, krate) = load("load_items", "lib.rs");

	let no_body = find(&workspace, krate, &["no_body"]);

	assert_eq!(workspace.item_text(no_body), "fn no_body();");
	assert_eq!(name_text(&workspace, no_body), "no_body");
	assert_eq!(workspace.item(no_body).fn_info().unwrap().body, None);

	// re-parsed without `const`: details and associated items
	let const_trait = find(&workspace, krate, &["ConstTrait"]);

	assert_eq!(workspace.item_text(const_trait), "const trait ConstTrait {\n\tfn f();\n}");
	assert_eq!(body_text(&workspace, const_trait), "\n\tfn f();\n");
	assert_eq!(workspace.item_text(find(&workspace, krate, &["ConstTrait", "f"])), "fn f();");

	let root = workspace.krate(krate).root_module();
	let const_impl = nth(&workspace, root, ItemKind::Impl, 7);

	assert!(workspace.item_text(const_impl).starts_with("const impl ConstTrait for Unit {"));
	assert_eq!(workspace.item(const_impl).impl_info().unwrap().trait_text.as_deref(), Some("ConstTrait"));

	let decl_macro = find(&workspace, krate, &["decl_macro"]);

	assert_eq!(workspace.item(decl_macro).kind, ItemKind::MacroRules);
	assert!(matches!(&workspace.item(decl_macro).detail, ItemDetail::Macro { path, .. } if path == "macro"));
	assert_eq!(body_text(&workspace, decl_macro), "\n\t$x\n");
	assert_eq!(name_text(&workspace, decl_macro), "decl_macro");

	let restricted = find(&workspace, krate, &["Restricted"]);

	assert_eq!(workspace.item_text(restricted), "pub impl(crate) trait Restricted {}");
	assert_eq!(workspace.item(restricted).vis, Visibility::Public);
	assert_eq!(line_at(&workspace, restricted, workspace.item(restricted).attrs.after_attrs), "pub impl(crate) trait Restricted {}");

	assert_eq!(workspace.item_text(find(&workspace, krate, &["NO_VALUE"])), "static NO_VALUE: u8;");
	assert_eq!(workspace.item_text(find(&workspace, krate, &["NO_VALUE_CONST"])), "const NO_VALUE_CONST: u8;");
	assert_eq!(workspace.item_text(find(&workspace, krate, &["Bounded"])), "type Bounded: Clone = u8;");
}

#[test]
fn handles_bom_and_shebang() {
	let (workspace, krate) = load("load_bom_shebang", "main.rs");
	let text = workspace.krate(krate).root_file().text();

	assert!(text.starts_with("\u{feff}#!"), "the fixture must start with a byte order mark and a shebang");
	assert_eq!(diagnostics(&workspace, krate), Vec::<String>::new());

	let main = find(&workspace, krate, &["main"]);

	assert_eq!(workspace.item_text(main), "fn main() {}");
	assert_eq!(workspace.file_of(main).line_col(workspace.item(main).range.start), LineCol { line: 3, column: 1 });
	assert_eq!(workspace.item_text(find(&workspace, krate, &["S"])), "/// Docs.\nstruct S;");
	assert_eq!(workspace.item_text(find(&workspace, krate, &["m"])), "mod m;");

	// a module file with a byte order mark
	let in_m = find(&workspace, krate, &["m", "in_m"]);

	assert_eq!(workspace.item_text(in_m), "pub fn in_m() {}");
	assert_eq!(workspace.item(in_m).range.start, 3);
}

#[test]
fn handles_crlf_line_endings() {
	let (workspace, krate) = load_with("load_crlf", "lib.rs", linux());

	assert!(workspace.krate(krate).root_file().text().contains("\r\n"), "the fixture must have CRLF line endings");
	assert_eq!(diagnostics(&workspace, krate), Vec::<String>::new());

	let f = find(&workspace, krate, &["f"]);
	let item = workspace.item(f);

	assert_eq!(workspace.item_text(f), "/// Doc line one.\r\n/// Doc line two.\r\npub fn f() {\r\n}");
	assert_eq!(slice(&workspace, f, item.attrs.docs.unwrap()), "/// Doc line one.\r\n/// Doc line two.");
	assert_eq!(line_at(&workspace, f, item.attrs.after_attrs), "pub fn f() {");
	assert_eq!(workspace.file_of(f).line_col(item.attrs.after_attrs), LineCol { line: 5, column: 1 });

	let inline = find(&workspace, krate, &["inline"]);

	assert_eq!(body_text(&workspace, inline), "\r\n\tpub struct S;\r\n");
	assert_eq!(workspace.item_text(find(&workspace, krate, &["inline", "S"])), "pub struct S;");

	let g = find(&workspace, krate, &["m", "g"]);

	assert_eq!(workspace.item_text(g), "#[cfg(unix)]\r\npub fn g() {}");
	assert_eq!(workspace.is_active(g), Tristate::True);
}

#[test]
fn reports_unparsable_and_missing_roots() {
	let (workspace, krate) = load("load_broken_root", "lib.rs");
	let root = workspace.item(workspace.krate(krate).root_module());

	let expected = concat!(
		"error lib.rs:4:9: cannot parse file: expected one of: `for`, parentheses, `unsafe`, `fn`, `extern`, identifier, ",
		"`::`, `<`, `dyn`, square brackets, `*`, `&`, `!`, `impl`, `_`, lifetime",
	);

	assert_eq!(diagnostics(&workspace, krate), [expected]);
	assert_eq!(workspace.children(workspace.krate(krate).root_module()).count(), 0);
	assert!(root.module_info().unwrap().load_error.is_some());

	let (workspace, krate) = load("load_broken_root", "does_not_exist.rs");
	let krate_data = workspace.krate(krate);
	let diagnostics = krate_data.diagnostics();

	assert_eq!(diagnostics.len(), 1);
	assert_eq!(diagnostics[0].severity, Severity::Error);
	assert!(diagnostics[0].message.starts_with("cannot read the crate root: "), "{}", diagnostics[0].message);
	assert_eq!(krate_data.files().len(), 1);
	assert_eq!(krate_data.root_file().text(), "");
	assert_eq!(krate_data.items().count(), 1);
}

#[test]
fn locates_parse_errors_at_the_end_of_files() {
	let (workspace, krate) = load("load_truncated", "lib.rs");
	let eof = "cannot parse file: unexpected end of input, expected ";

	// at the last token, as rustc reports them (columns count a byte order mark, as `SourceFile::line_col` does)
	assert_eq!(
		diagnostics(&workspace, krate)
			.iter()
			.map(|diagnostic| diagnostic.split_once(eof).map_or(diagnostic.as_str(), |(location, _)| location))
			.collect::<Vec<_>>(),
		["error truncated.rs:2:12: ", "error bom.rs:1:16: ", "error group.rs:3:1: "]
	);

	assert_eq!(workspace.children(find(&workspace, krate, &["truncated"])).count(), 0);
	assert_eq!(workspace.item(find(&workspace, krate, &["fine"])).kind, ItemKind::Fn);
}

#[test]
fn evaluates_cfgs() {
	let (workspace, krate) = load_with("load_cfg", "lib.rs", linux());
	let item = |name: &str| workspace.item(find(&workspace, krate, &[name]));
	let cfg = |name: &str| item(name).cfg.as_ref().map(ToString::to_string);

	assert_eq!(cfg("unix_only").as_deref(), Some("unix"));
	assert_eq!(cfg("two_cfgs").as_deref(), Some(r#"all(unix, feature = "std")"#));
	assert_eq!(cfg("cfg_via_cfg_attr").as_deref(), Some(r#"any(not(feature = "serde"), test)"#));
	assert_eq!(cfg("nested_cfg_attr").as_deref(), Some(r#"any(not(feature = "std"), debug_assertions)"#));
	assert_eq!(cfg("false_cfg_attr"), None);
	assert_eq!(cfg("invalid_cfg").as_deref(), Some("this is not a predicate"));
	assert_eq!(cfg("hidden"), None);

	// inner attributes of a module's file apply to the module
	assert_eq!(cfg("file_cfg").as_deref(), Some("windows"));
	assert!(item("file_cfg").attrs.doc_hidden);

	let active = |path: &[&str]| workspace.is_active(find(&workspace, krate, path));

	assert_eq!(active(&["unix_only"]), Tristate::True);
	assert_eq!(active(&["two_cfgs"]), Tristate::Unknown);
	assert_eq!(active(&["cfg_via_cfg_attr"]), Tristate::Unknown);
	assert_eq!(active(&["nested_cfg_attr"]), Tristate::True);
	assert_eq!(active(&["invalid_cfg"]), Tristate::Unknown);
	assert_eq!(active(&["tests", "a_test"]), Tristate::False);
	assert_eq!(active(&["std_only", "inner"]), Tristate::Unknown);
	assert_eq!(active(&["file_cfg", "windows_only"]), Tristate::False);
	assert_eq!(workspace.effective_cfg(find(&workspace, krate, &["tests", "a_test"])).unwrap().to_string(), "test");

	// flags
	assert!(item("hidden").attrs.doc_hidden);
	assert!(item("maybe_hidden").attrs.doc_hidden);
	assert!(!item("unix_only").attrs.doc_hidden);
	assert!(workspace.item(find(&workspace, krate, &["tests", "a_test"])).attrs.test);

	// a definitely inactive module's file is loaded, but its problems are only warnings
	assert_eq!(diagnostics(&workspace, krate), [
		"warning lib.rs:20:3: invalid cfg predicate `this is not a predicate`: expected `name`, `key = \"value\"`, or `op(...)`",
		"warning broken_windows.rs:1:10: cannot parse file: cannot parse string into token stream",
	]);

	assert!(item("broken_windows").module_info().unwrap().load_error.is_some());

	// unless it might be active
	let (workspace, krate) = load("load_cfg", "lib.rs");

	assert!(errors(&workspace, krate).iter().any(|error| error.starts_with("error broken_windows.rs:1:10: ")));

	// with features
	let (workspace, krate) = load_with("load_cfg", "lib.rs", linux().with_features(["std"]));
	let item = |name: &str| workspace.item(find(&workspace, krate, &[name]));

	assert_eq!(item("cfg_via_cfg_attr").cfg, None);
	assert_eq!(item("nested_cfg_attr").cfg.as_ref().map(ToString::to_string).as_deref(), Some("debug_assertions"));
	assert!(!item("maybe_hidden").attrs.doc_hidden);
	assert_eq!(workspace.is_active(find(&workspace, krate, &["std_only", "inner"])), Tristate::True);
	assert_eq!(workspace.is_active(find(&workspace, krate, &["two_cfgs"])), Tristate::True);
}

/// A directory under the system's temporary directory, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
	fn new(name: &str) -> Self {
		let path = std::env::temp_dir().join(format!("rscode-load-{}-{name}", std::process::id()));

		let _ = std::fs::remove_dir_all(&path);
		std::fs::create_dir_all(&path).unwrap();
		Self(path)
	}

	fn write(&self, path: &str, contents: impl AsRef<[u8]>) {
		let path = self.0.join(path);

		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(path, contents).unwrap();
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

#[test]
fn reports_unreadable_module_files() {
	let dir = TempDir::new("unreadable");

	dir.write("lib.rs", "mod bad_utf8;\nmod dir_module;\nmod empty;\nmod comments;\n#[path = \"nope.rs\"]\nmod via_path;\n");
	dir.write("bad_utf8.rs", b"pub fn f() {}\n\xff\xfe\n");
	std::fs::create_dir_all(dir.0.join("dir_module.rs")).unwrap();
	dir.write("empty.rs", "");
	dir.write("comments.rs", "// only\n/* comments */\n");

	let mut workspace = Workspace::new(&dir.0);
	let krate = workspace.load_crate(CrateSpec::new("unreadable", dir.0.join("lib.rs")));
	let diagnostics = diagnostics(&workspace, krate);

	assert_eq!(diagnostics.len(), 3, "{diagnostics:?}");
	assert!(diagnostics[0].starts_with("error lib.rs:1:5: cannot read file `bad_utf8.rs` of module `bad_utf8`: "), "{}", diagnostics[0]);

	assert_eq!(
		diagnostics[1],
		"error lib.rs:2:5: file not found for module `dir_module`: neither `dir_module.rs` nor `dir_module/mod.rs` exists"
	);

	assert_eq!(diagnostics[2], "error lib.rs:6:5: file not found for module `via_path`: `nope.rs` does not exist");

	for name in ["bad_utf8", "dir_module", "empty", "comments", "via_path"] {
		let module = find(&workspace, krate, &[name]);
		let failed = matches!(name, "bad_utf8" | "dir_module" | "via_path");

		assert_eq!(workspace.children(module).count(), 0, "{name}");
		assert_eq!(workspace.item(module).module_info().unwrap().load_error.is_some(), failed, "{name}");
	}

	assert_eq!(workspace.krate(krate).files().len(), 3);
}

/// `..` after a symbolic link to a directory leads to the parent of the link's target, as in rustc (verified with
/// `rustc -Zunpretty=expanded`), not to the directory containing the link.
#[cfg(unix)]
#[test]
fn resolves_parent_directories_through_symbolic_links() {
	let dir = TempDir::new("symlinks");

	dir.write("proj/src/lib.rs", "mod m;\n");
	dir.write("proj/src/up.rs", "pub fn decoy_up() {}\n");
	dir.write("elsewhere/up.rs", "pub fn real_up() {}\n");
	dir.write("elsewhere/mdir/mod.rs", "#[path = \"../up.rs\"]\nmod up;\nmod child;\n#[path = \"./../mdir/../up.rs\"]\nmod again;\n");
	dir.write("elsewhere/mdir/child.rs", "pub fn child() {}\n");
	std::os::unix::fs::symlink("../../elsewhere/mdir", dir.0.join("proj/src/m")).unwrap();

	let mut workspace = Workspace::new(dir.0.join("proj"));
	let krate = workspace.load_crate(CrateSpec::new("symlinks", dir.0.join("proj/src/lib.rs")));
	let file_path = |path: &[&str]| workspace.item(find(&workspace, krate, path)).module_info().unwrap().file_path.clone().unwrap();
	let real_up = std::fs::canonicalize(dir.0.join("elsewhere/up.rs")).unwrap();

	assert_eq!(diagnostics(&workspace, krate), Vec::<String>::new());
	assert_eq!(workspace.item(find(&workspace, krate, &["m", "up", "real_up"])).kind, ItemKind::Fn);
	assert_eq!(workspace.item(find(&workspace, krate, &["m", "again", "real_up"])).kind, ItemKind::Fn);
	assert_eq!(file_path(&["m", "up"]), real_up);
	assert_eq!(file_path(&["m", "again"]), real_up);

	// other symbolic links are kept
	assert_eq!(file_path(&["m"]), dir.0.join("proj/src/m/mod.rs"));
	assert_eq!(file_path(&["m", "child"]), dir.0.join("proj/src/m/child.rs"));
	assert_eq!(workspace.item(find(&workspace, krate, &["m", "child", "child"])).kind, ItemKind::Fn);

	// the file is loaded once
	assert_eq!(workspace.krate(krate).files().len(), 4);
}

#[test]
fn normalizes_crate_root_paths() {
	// integration tests run in the package's directory
	let mut workspace = Workspace::new(fixture("load_items"));
	let relative = workspace.load_crate(CrateSpec::new("relative", "tests/fixtures/load_items/lib.rs"));
	let dotted = workspace.load_crate(CrateSpec::new("dotted", fixture("load_modules").join("../load_items/./lib.rs")));

	for krate in [relative, dotted] {
		assert_eq!(workspace.krate(krate).root_file().path(), fixture("load_items").join("lib.rs"));
		assert_eq!(errors(&workspace, krate), Vec::<String>::new());
	}

	assert_eq!(workspace.krate(relative).items().count(), workspace.krate(dotted).items().count());
}

#[test]
fn inactive_crate_roots_only_warn() {
	let (workspace, krate) = load("load_cfg_root", "lib.rs");
	let root = workspace.item(workspace.krate(krate).root_module());

	assert_eq!(root.cfg.as_ref().map(ToString::to_string).as_deref(), Some("any()"));
	assert_eq!(diagnostics(&workspace, krate), [
		"warning lib.rs:3:5: file not found for module `missing`: neither `missing.rs` nor `missing/mod.rs` exists"
	]);
	assert_eq!(workspace.is_active(find(&workspace, krate, &["f"])), Tristate::False);
}

/// Loads a crate and checks that it has no errors and a plausible number of items.
fn load_real_crate(name: &str, root: &Path, min_items: usize) -> (Workspace, CrateId) {
	let start = Instant::now();
	let mut workspace = Workspace::new(root.parent().unwrap());
	let krate = workspace.load_crate(CrateSpec::new(name, root));
	let elapsed = start.elapsed();
	let krate_data = workspace.krate(krate);
	let items = krate_data.items().count();

	eprintln!("loaded `{name}`: {} files, {items} items in {elapsed:?}", krate_data.files().len());
	assert_eq!(errors(&workspace, krate), Vec::<String>::new(), "{name}");
	assert!(items >= min_items, "{name}: only {items} items");

	for (id, item) in krate_data.items() {
		let text = workspace.file_of(id).text();

		assert!(item.range.end <= text.len() && text.is_char_boundary(item.range.start) && text.is_char_boundary(item.range.end));

		// outer attributes (and only they) precede `after_attrs`
		if !id.is_crate_root() {
			let is_attr = |text: &str| text.starts_with("#[") || text.starts_with("///") || text.starts_with("/**");
			let after = &text[item.attrs.after_attrs..];

			assert!(item.range.start <= item.attrs.after_attrs && item.attrs.after_attrs < item.range.end);
			assert!(!is_attr(after), "{name}: {}", after.lines().next().unwrap_or(""));
			assert_eq!(item.attrs.after_attrs > item.range.start, is_attr(&text[item.range.start..]), "{name}");
			assert!(item.attrs.docs.is_none_or(|docs| item.range.start <= docs.start && docs.end <= item.attrs.after_attrs));
		}

		// the name range of an import of `a::{self}` is the `self` token
		if let Some(name_range) = item.name_range
			&& !item.import_info().is_some_and(|info| info.is_self)
		{
			let name = item.name().unwrap();
			let written = workspace.file_of(id).slice(name_range);

			assert!(written == name || written.strip_prefix("r#") == Some(name), "{written} != {name}");
		}
	}

	(workspace, krate)
}

#[test]
fn loads_this_repository() {
	let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();

	load_real_crate("rscode", &crates.join("rscode/src/lib.rs"), 300);
	load_real_crate("rscode_fmt", &crates.join("rscode_fmt/src/lib.rs"), 20);
	load_real_crate("rscode_sort", &crates.join("rscode_sort/src/lib.rs"), 20);
	load_real_crate("cargo_rscode", &crates.join("cargo-rscode/src/main.rs"), 5);
}

/// Loads the source of syn (a dependency of rscode, so it is normally in cargo's registry) as a crate.
#[test]
fn loads_syn() {
	let version = locked_version("syn");

	let Some(syn) = registry_crate("syn") else {
		eprintln!("SKIPPED loads_syn: the source of syn {version} (from Cargo.lock) is not in cargo's registry");
		return;
	};

	let (workspace, krate) = load_real_crate("syn", &syn.join("src/lib.rs"), 2000);
	let item = |path: &[&str]| workspace.item(find(&workspace, krate, path));

	assert!(workspace.krate(krate).items().count() < 100_000);
	assert!(workspace.krate(krate).files().len() > 30);
	assert_eq!(item(&["item", "parsing", "parse_use_tree"]).kind, ItemKind::Fn);
	assert_eq!(item(&["parse_file"]).kind, ItemKind::Fn);
	assert!(workspace.krate(krate).items().any(|(_, data)| data.kind == ItemKind::Import && data.name() == Some("DeriveInput")));
}
