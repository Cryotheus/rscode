//! Finding and viewing items of fixture crates.

mod common;

use common::locked_version;
use common::registry_crate;
use rscode::CfgContext;
use rscode::CrateSpec;
use rscode::Error;
use rscode::Find;
use rscode::ItemId;
use rscode::ItemKind;
use rscode::PathPattern;
use rscode::Resolver;
use rscode::Tristate;
use rscode::View;
use rscode::ViewMode;
use rscode::Viewpoint;
use rscode::Workspace;
use rscode::model::Dependency;
use rscode::pattern::IdentPattern;
use rscode::pattern::MatchOptions;
use rscode::query::FindMatch;
use rscode::query::ItemView;
use rscode::query::ViewOptions;
use rscode::query::outline_text;
use rscode::source::LineCol;
use std::path::Path;
use std::path::PathBuf;

fn fixture(path: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(path)
}

/// A Linux-like configuration without features.
fn linux() -> CfgContext {
	CfgContext::from_rustc_print_cfg("unix\ntarget_os=\"linux\"\n").with_features(Vec::<&str>::new())
}

/// `query_basic` (selected), and its dependency `query_dep` (loaded, but not selected). The workspace root is the
/// fixture directory.
fn workspace() -> Workspace {
	let mut workspace = Workspace::new(fixture(""));
	let mut basic = CrateSpec::new("query_basic", fixture("query_basic/src/lib.rs"));
	let mut dep = CrateSpec::new("query_dep", fixture("query_dep/src/lib.rs"));

	basic.cfg = linux();
	basic.dependencies.push(Dependency {
		name: "query_dep".into(),
		crate_name: "query_dep".into(),
		package: None,
		krate: None,
	});
	dep.cfg = linux();
	dep.selected = false;

	workspace.load_crate(basic);
	workspace.load_crate(dep);
	workspace.link();

	for krate in workspace.crates() {
		assert!(krate.diagnostics().is_empty(), "{:?}", krate.diagnostics());
	}

	workspace
}

/// The canonical paths of the matches of a search.
fn paths(resolver: &Resolver<'_>, find: Find) -> Vec<String> {
	find.run_with(resolver).unwrap().into_iter().map(|found| found.path).collect()
}

/// The canonical paths of the matches of patterns.
fn found(resolver: &Resolver<'_>, patterns: &[&str]) -> Vec<String> {
	let mut find = Find::new();

	for pattern in patterns {
		find = find.pattern(pattern).unwrap();
	}

	paths(resolver, find)
}

fn one(resolver: &Resolver<'_>, pattern: &str) -> FindMatch {
	let mut matches = Find::new().pattern(pattern).unwrap().imports(true).run_with(resolver).unwrap();

	assert_eq!(matches.len(), 1, "`{pattern}`: {matches:#?}");
	matches.remove(0)
}

fn views(resolver: &Resolver<'_>, path: &str, options: ViewOptions) -> Vec<ItemView> {
	View::with_options(options).path(path).unwrap().run_with(resolver).unwrap()
}

/// The text of the only view of a path.
fn text(resolver: &Resolver<'_>, path: &str, options: ViewOptions) -> String {
	let mut views = views(resolver, path, options);

	assert_eq!(views.len(), 1, "`{path}`: {views:#?}");
	views.remove(0).text
}

fn options(mode: ViewMode) -> ViewOptions {
	ViewOptions { mode, ..ViewOptions::default() }
}

fn at(line: usize, column: usize) -> LineCol {
	LineCol { line, column }
}

/// `text` with `/` replaced by the platform's path separator, which the paths of views have.
fn native(text: &str) -> String {
	text.replace('/', std::path::MAIN_SEPARATOR_STR)
}

/// Lines of a rendered view.
fn lines(lines: &[&str]) -> String {
	lines.join("\n")
}

#[test]
fn finds_by_identifier_patterns() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let circle = ["query_basic::shapes::Circle"];

	assert_eq!(found(&resolver, &["Circle"]), circle);
	assert_eq!(found(&resolver, &["Ci*"]), circle);
	assert_eq!(found(&resolver, &["*cle"]), circle);
	assert_eq!(found(&resolver, &["*irc*"]), circle);
	assert_eq!(found(&resolver, &["C*l*e"]), circle);
	assert_eq!(
		found(&resolver, &["S*e"]),
		["query_basic::shapes::Square", "query_basic::shapes::Kind::Square", "query_basic::shapes::Shape"]
	);
	assert_eq!(found(&resolver, &["Nothing*"]), Vec::<String>::new());

	// an item matching several patterns is found once
	assert_eq!(found(&resolver, &["Circle", "Ci*", "shapes::Circle"]), circle);

	// case
	assert_eq!(found(&resolver, &["circle"]), Vec::<String>::new());
	assert_eq!(paths(&resolver, Find::new().ignore_case(true).pattern("circle").unwrap()), circle);
	assert_eq!(paths(&resolver, Find::new().pattern("circle").unwrap().ignore_case(true)), Vec::<String>::new());

	// raw identifiers compare unraw'd
	assert_eq!(found(&resolver, &["match"]), ["query_basic::nested::r#match"]);
	assert_eq!(found(&resolver, &["r#match"]), ["query_basic::nested::r#match"]);

	// identifier patterns given literally
	let options = MatchOptions::default();
	let literal = |pattern: IdentPattern| paths(&resolver, Find::new().path_pattern(PathPattern::from_ident(pattern)));

	assert_eq!(literal(IdentPattern::contains("ircl", options)), circle);
	assert_eq!(literal(IdentPattern::starts_with("Circ", options)), circle);
	assert_eq!(literal(IdentPattern::ends_with("rcle", options)), circle);
}

#[test]
fn finds_by_path_patterns() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);

	let deep = ["query_basic::nested::outer::inner::deep"];

	assert_eq!(found(&resolver, &["**::deep"]), deep);
	assert_eq!(found(&resolver, &["**deep"]), deep);
	assert_eq!(found(&resolver, &["inner**"]), deep);
	assert_eq!(found(&resolver, &["outer::**::deep"]), deep);
	assert_eq!(found(&resolver, &["nested::**::inner::deep"]), deep);
	assert_eq!(found(&resolver, &["nested::*::deep"]), Vec::<String>::new());
	assert_eq!(found(&resolver, &["outer**::deep"]), deep);
	assert_eq!(
		found(&resolver, &["r#type::*"]),
		[
			"query_basic::nested::r#type::Unit",
			"query_basic::nested::r#type::Alias",
			"query_basic::nested::r#type::Bits",
		]
	);
	assert_eq!(
		found(&resolver, &["nested::outer::*"]),
		[
			"query_basic::nested::outer::inner",
			"query_basic::nested::outer::shallow",
			"query_basic::nested::outer::TABLE",
			"query_basic::nested::outer::SHORT",
			"query_basic::nested::outer::GREETING",
			"query_basic::nested::outer::text",
			"query_basic::nested::outer::Limits",
			"query_basic::nested::outer::abs",
		]
	);

	// items of `impl` blocks elsewhere are below their type; the order is by file, then position
	assert_eq!(
		found(&resolver, &["shapes::**"]),
		[
			"query_basic::shapes::Circle::new",
			"<query_basic::shapes::Circle as Shape>::area",
			"<query_basic::shapes::Square as Shape>::area",
			"<query_basic::shapes::Square as Shape>::name",
			"<query_basic::shapes::Circle as std::fmt::Display>::fmt",
			"query_basic::shapes::Circle",
			"query_basic::shapes::Square",
			"query_basic::shapes::Kind",
			"query_basic::shapes::Kind::Round",
			"query_basic::shapes::Kind::Square",
			"query_basic::shapes::Shape",
			"query_basic::shapes::Shape::area",
			"query_basic::shapes::Shape::name",
		]
	);

	// `crate::` anchors at the crate root; the root itself is found too
	assert_eq!(
		found(&resolver, &["crate::*"]),
		[
			"query_basic::shapes",
			"query_basic::impls",
			"query_basic::nested",
			"query_basic::add",
			"query_basic::platform",
			"query_basic::platform",
			"query_basic::extra",
		]
	);
	assert_eq!(found(&resolver, &["crate"]), ["query_basic"]);
	assert_eq!(found(&resolver, &["query_basic"]), ["query_basic"]);
	assert_eq!(found(&resolver, &["crate::shapes::Circle"]), ["query_basic::shapes::Circle"]);

	// paths without wildcards also name what they name through re-exports, like paths everywhere else
	assert_eq!(found(&resolver, &["crate::Circle"]), ["query_basic::shapes::Circle"]);
	assert_eq!(found(&resolver, &["query_basic::Circle"]), ["query_basic::shapes::Circle"]);
	assert_eq!(found(&resolver, &["::query_basic::Circle"]), ["query_basic::shapes::Circle"]);
	assert_eq!(found(&resolver, &["crate::Circle*"]), Vec::<String>::new());
}

#[test]
fn searches_unselected_crates_by_crate_name_only() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);

	assert_eq!(found(&resolver, &["DepItem"]), Vec::<String>::new());
	assert_eq!(found(&resolver, &["crate::DepItem"]), Vec::<String>::new());
	assert_eq!(found(&resolver, &["query_dep::DepIt*"]), Vec::<String>::new());

	// a path naming the crate names its items (as `::query_dep::DepItem`)
	assert_eq!(found(&resolver, &["query_dep::DepItem"]), ["query_dep::DepItem"]);
	assert_eq!(found(&resolver, &["::query_dep::DepItem"]), ["query_dep::DepItem"]);
	assert_eq!(
		found(&resolver, &["::query_dep::**"]),
		["query_dep::DepItem", "query_dep::inner", "query_dep::inner::helper"]
	);
	assert_eq!(found(&resolver, &["::*::inner"]), ["query_dep::inner"]);
	assert_eq!(found(&resolver, &["::query_basic::add"]), ["query_basic::add"]);
	assert_eq!(found(&resolver, &["::query_dep"]), ["query_dep"]);

	// crates are in the order they were loaded
	assert_eq!(found(&resolver, &["::*::**::*e*"]).last().map(String::as_str), Some("query_dep::inner::helper"));
}

#[test]
fn finds_impl_blocks_and_their_items_by_qualified_patterns() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);

	assert_eq!(found(&resolver, &["<Circle as Shape>::area"]), ["<query_basic::shapes::Circle as Shape>::area"]);
	assert_eq!(
		found(&resolver, &["<* as Shape>::area"]),
		["<query_basic::shapes::Circle as Shape>::area", "<query_basic::shapes::Square as Shape>::area"]
	);
	assert_eq!(
		found(&resolver, &["<Circle as *>::*"]),
		["<query_basic::shapes::Circle as Shape>::area", "<query_basic::shapes::Circle as std::fmt::Display>::fmt"]
	);
	assert_eq!(found(&resolver, &["<Circle>::*"]), ["query_basic::shapes::Circle::new"]);
	assert_eq!(
		found(&resolver, &["impl Shape for *"]),
		["impl Shape for query_basic::shapes::Circle", "impl Shape for query_basic::shapes::Square"]
	);
	assert_eq!(found(&resolver, &["impl *"]), ["impl query_basic::shapes::Circle"]);
	assert_eq!(
		found(&resolver, &["impl Display for Circle"]),
		["impl std::fmt::Display for query_basic::shapes::Circle"]
	);
	assert_eq!(found(&resolver, &["<crate::shapes::Circle as Shape>"]), ["impl Shape for query_basic::shapes::Circle"]);

	// types through re-exports (`crate::Circle` is `crate::shapes::Circle`)
	assert_eq!(found(&resolver, &["<crate::Circle as Shape>"]), ["impl Shape for query_basic::shapes::Circle"]);
	assert_eq!(
		found(&resolver, &["<crate::Circle as *>::*"]),
		["<query_basic::shapes::Circle as Shape>::area", "<query_basic::shapes::Circle as std::fmt::Display>::fmt"]
	);
	assert_eq!(found(&resolver, &["impl crate::Circle"]), ["impl query_basic::shapes::Circle"]);
	assert_eq!(found(&resolver, &["<crate::Circle as *>::new"]), Vec::<String>::new());

	// variants and trait items are not items of `impl` blocks
	assert_eq!(found(&resolver, &["<Kind>::Round"]), Vec::<String>::new());
	assert_eq!(found(&resolver, &["<Shape>::area"]), Vec::<String>::new());

	// unqualified patterns find items of `impl` blocks, but not `impl` blocks
	assert_eq!(
		found(&resolver, &["Circle::*"]),
		[
			"query_basic::shapes::Circle::new",
			"<query_basic::shapes::Circle as Shape>::area",
			"<query_basic::shapes::Circle as std::fmt::Display>::fmt",
		]
	);
	assert!(paths(&resolver, Find::new().pattern("**").unwrap().kind(ItemKind::Impl)).is_empty());
	assert_eq!(paths(&resolver, Find::new().kind(ItemKind::Impl)).len(), 4);
}

#[test]
fn filters_by_kind() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let kinds = |pattern: &str, kinds: &[ItemKind]| {
		let mut find = Find::new().pattern(pattern).unwrap();

		for &kind in kinds {
			find = find.kind(kind);
		}

		paths(&resolver, find)
	};

	assert_eq!(
		kinds("*", &[ItemKind::Variant]),
		["query_basic::shapes::Kind::Round", "query_basic::shapes::Kind::Square"]
	);
	assert_eq!(
		kinds("area", &[ItemKind::AssocFn]),
		[
			"<query_basic::shapes::Circle as Shape>::area",
			"<query_basic::shapes::Square as Shape>::area",
			"query_basic::shapes::Shape::area",
		]
	);
	assert_eq!(
		kinds("*", &[ItemKind::Union, ItemKind::TypeAlias]),
		["query_basic::nested::r#type::Alias", "query_basic::nested::r#type::Bits"]
	);
	assert_eq!(kinds("*", &[ItemKind::ForeignFn]), ["query_basic::nested::outer::abs"]);
	assert_eq!(kinds("*", &[ItemKind::MacroRules]), ["query_basic::nested::square"]);
	assert_eq!(kinds("*", &[ItemKind::AssocConst]), ["query_basic::nested::outer::Limits::MAX"]);
	assert_eq!(
		kinds("*", &[ItemKind::Static, ItemKind::Const]),
		[
			"query_basic::nested::outer::TABLE",
			"query_basic::nested::outer::SHORT",
			"query_basic::nested::outer::GREETING",
			"query_basic::nested::COUNTER",
		]
	);

	// never found: unnamed items
	for kind in [ItemKind::Use, ItemKind::MacroCall, ItemKind::ExternBlock] {
		assert!(kinds("*", &[kind]).is_empty(), "{kind}");
		assert!(paths(&resolver, Find::new().kind(kind)).is_empty(), "{kind}");
	}

	// without patterns, everything of the kinds in the selected crates
	assert_eq!(
		paths(&resolver, Find::new().kind(ItemKind::Struct)),
		["query_basic::nested::r#type::Unit", "query_basic::shapes::Circle", "query_basic::shapes::Square",]
	);
}

#[test]
fn finds_cfg_variants() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let platform = Find::new().pattern("platform").unwrap().run_with(&resolver).unwrap();

	assert_eq!(platform.len(), 2);
	assert_eq!((platform[0].cfg.as_deref(), platform[0].active), (Some("unix"), Tristate::True));
	assert_eq!((platform[1].cfg.as_deref(), platform[1].active), (Some("not(unix)"), Tristate::False));

	let active = Find::new().pattern("platform").unwrap().active_only(true).run_with(&resolver).unwrap();

	assert_eq!(active.len(), 1);
	assert_eq!(active[0].item, platform[0].item);

	// the cfg of an ancestor applies
	let bonus = one(&resolver, "bonus");

	assert_eq!((bonus.cfg.as_deref(), bonus.active), (Some("feature = \"extra\""), Tristate::False));
	assert!(found(&resolver, &["extra::**"]).len() == 1);
	assert!(paths(&resolver, Find::new().pattern("extra::**").unwrap().active_only(true)).is_empty());
}

#[test]
fn finds_imports_with_their_targets() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let circles = Find::new().pattern("Circle").unwrap().imports(true).run_with(&resolver).unwrap();
	let summary: Vec<(&str, ItemKind, Vec<String>)> =
		circles.iter().map(|found| (found.path.as_str(), found.kind, found.import_targets.clone())).collect();
	let circle = || vec!["query_basic::shapes::Circle".to_owned()];

	assert_eq!(
		summary,
		[
			("use query_basic::impls::Circle", ItemKind::Import, circle()),
			("use query_basic::Circle", ItemKind::Import, circle()),
			("query_basic::shapes::Circle", ItemKind::Struct, Vec::new()),
		]
	);

	// asking for the kind is asking for imports
	let imports = Find::new().pattern("*").unwrap().kind(ItemKind::Import).run_with(&resolver).unwrap();
	let summary: Vec<(&str, Vec<String>)> =
		imports.iter().map(|found| (found.path.as_str(), found.import_targets.clone())).collect();

	assert_eq!(
		summary,
		[
			("use query_basic::impls::Circle", circle()),
			("use query_basic::impls::Shape", vec!["query_basic::shapes::Shape".to_owned()]),
			("use query_basic::impls::Square", vec!["query_basic::shapes::Square".to_owned()]),
			("use query_basic::*", vec!["query_basic::nested".to_owned()]),
			("use query_basic::Circle", circle()),
			("use query_basic::ShapeTrait", vec!["query_basic::shapes::Shape".to_owned()]),
		]
	);

	// the import is named `ShapeTrait`; as a path, `ShapeTrait` names the trait
	assert_eq!(found(&resolver, &["ShapeTrait"]), ["query_basic::shapes::Shape"]);
	assert_eq!(found(&resolver, &["ShapeTrai*"]), Vec::<String>::new());

	let import = Find::new().pattern("ShapeTrait").unwrap().kind(ItemKind::Import).run_with(&resolver).unwrap();

	assert_eq!(import.len(), 1, "{import:#?}");
	assert_eq!(import[0].visibility, "pub");
}

#[test]
fn finds_usable_paths() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let root = ItemId::crate_root(workspace.crates()[0].id());
	let usable = |pattern: &str, viewpoint: Viewpoint| {
		let found = Find::new().pattern(pattern).unwrap().from(viewpoint).run_with(&resolver).unwrap();

		assert_eq!(found.len(), 1, "{found:#?}");
		found[0].usable_paths.clone()
	};

	let local = usable("crate::shapes::Circle", Viewpoint::Module(root));

	assert!(local.contains(&"crate::Circle".to_owned()), "{local:?}");
	assert!(local.contains(&"crate::shapes::Circle".to_owned()), "{local:?}");
	assert_eq!(
		usable("crate::shapes::Circle", Viewpoint::Foreign),
		["::query_basic::Circle", "::query_basic::shapes::Circle"]
	);

	// private modules are not usable from other crates
	assert!(usable("crate::impls", Viewpoint::Foreign).is_empty());
	assert_eq!(usable("crate::impls", Viewpoint::Module(root)), ["impls", "crate::impls"]);

	// without a viewpoint, none are computed
	assert!(one(&resolver, "crate::add").usable_paths.is_empty());
}

#[test]
fn orders_and_limits_matches() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let functions = || Find::new().pattern("*").unwrap().kind(ItemKind::Fn);

	assert_eq!(
		paths(&resolver, functions()),
		[
			"query_basic::add",
			"query_basic::platform",
			"query_basic::platform",
			"query_basic::extra::bonus",
			"query_basic::nested::outer::inner::deep",
			"query_basic::nested::outer::shallow",
			"query_basic::nested::outer::text",
			"query_basic::nested::r#match",
		]
	);
	assert_eq!(paths(&resolver, functions().limit(2)), ["query_basic::add", "query_basic::platform"]);
	assert!(paths(&resolver, functions().limit(0)).is_empty());

	// crate by crate
	let everywhere = paths(&resolver, Find::new().pattern("::*::**").unwrap().kind(ItemKind::Fn));

	assert_eq!(everywhere.len(), 9);
	assert_eq!(everywhere[8], "query_dep::inner::helper");
}

#[test]
fn fills_every_field() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let add = one(&resolver, "crate::add");

	assert_eq!(add.path, "query_basic::add");
	assert_eq!(add.kind, ItemKind::Fn);
	assert_eq!(add.krate, "query_basic");
	assert_eq!(add.package, None);
	assert_eq!(add.file, Path::new("query_basic/src/lib.rs"));
	assert_eq!((add.start, add.end), (at(11, 1), at(16, 2)));
	assert_eq!(workspace.file_of(add.item).slice(add.range), workspace.item_text(add.item));
	assert_eq!(add.visibility, "pub");
	assert_eq!(add.cfg, None);
	assert_eq!(add.active, Tristate::True);
	assert!(add.usable_paths.is_empty() && add.import_targets.is_empty());

	let json = serde_json::to_value(&add).unwrap();

	assert_eq!(
		json,
		serde_json::json!({
			"path": "query_basic::add",
			"kind": "fn",
			"crate": "query_basic",
			"package": null,
			"file": native("query_basic/src/lib.rs"),
			"start": {"line": 11, "column": 1},
			"end": {"line": 16, "column": 2},
			"visibility": "pub",
			"cfg": null,
			"active": "true",
		})
	);

	let area = one(&resolver, "<Circle as Shape>::area");

	assert_eq!(area.file, Path::new("query_basic/src/impls.rs"));
	assert_eq!((area.start, area.end), (at(15, 2), at(17, 3)));
	assert_eq!(area.visibility, "inherited");

	let root = one(&resolver, "crate");

	assert_eq!((root.kind, root.start), (ItemKind::Module, at(1, 1)));
}

#[test]
fn views_items_in_full() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let add = views(&resolver, "crate::add", ViewOptions::default());

	assert_eq!(add.len(), 1);
	assert_eq!(add[0].path, "query_basic::add");
	assert_eq!(add[0].kind, ItemKind::Fn);
	assert_eq!(add[0].file, Path::new("query_basic/src/lib.rs"));
	assert_eq!((add[0].start, add[0].end), (at(11, 1), at(16, 2)));
	assert_eq!((add[0].cfg.as_deref(), add[0].active), (None, Tristate::True));
	assert_eq!(
		add[0].text,
		lines(&[
			"/// Adds two numbers.",
			"///",
			"/// With a second paragraph.",
			"pub fn add(left: i32, right: i32) -> i32 {",
			"\tleft + right",
			"}",
		])
	);

	let full = options(ViewMode::Full);

	assert_eq!(text(&resolver, "crate::add", full.clone()), add[0].text);
	assert_eq!(
		text(&resolver, "crate::shapes::Circle", full.clone()),
		lines(&[
			"/// A circle.",
			"#[derive(Debug, Clone, Copy)]",
			"pub struct Circle {",
			"\t/// The radius.",
			"\tpub radius: f64,",
			"}",
		])
	);

	// items of other crates
	assert_eq!(
		text(&resolver, "::query_dep::DepItem", full),
		lines(&["/// An item of the dependency.", "pub struct DepItem;"])
	);
}

#[test]
fn views_without_docs() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let no_docs = ViewOptions { docs: false, ..ViewOptions::default() };

	assert_eq!(
		text(&resolver, "crate::add", no_docs.clone()),
		lines(&["pub fn add(left: i32, right: i32) -> i32 {", "\tleft + right", "}"])
	);

	// nested items' and fields' docs go too
	assert_eq!(
		text(&resolver, "crate::shapes::Circle", no_docs.clone()),
		lines(&["#[derive(Debug, Clone, Copy)]", "pub struct Circle {", "\tpub radius: f64,", "}"])
	);
	assert_eq!(
		text(&resolver, "crate::shapes", no_docs.clone()),
		lines(&[
			&format!("// file: {}", native("query_basic/src/shapes.rs")),
			"#[derive(Debug, Clone, Copy)]",
			"pub struct Circle {",
			"\tpub radius: f64,",
			"}",
			"",
			"pub struct Square(pub f64);",
			"",
			"pub enum Kind {",
			"\tRound,",
			"\tSquare,",
			"}",
			"",
			"pub trait Shape {",
			"\tfn area(&self) -> f64;",
			"",
			"\tfn name(&self) -> &'static str { ... }",
			"}",
		])
	);

	let numbered = ViewOptions { line_numbers: true, ..no_docs };

	assert_eq!(
		text(&resolver, "crate::nested::outer::inner::deep", numbered),
		lines(&["   8 │ pub fn deep() -> u8 {", "   9 │ \t1", "  10 │ }"])
	);
}

#[test]
fn views_with_line_numbers() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let numbered = ViewOptions { line_numbers: true, ..ViewOptions::default() };

	assert_eq!(
		text(&resolver, "crate::add", numbered.clone()),
		lines(&[
			"  11 │ /// Adds two numbers.",
			"  12 │ ///",
			"  13 │ /// With a second paragraph.",
			"  14 │ pub fn add(left: i32, right: i32) -> i32 {",
			"  15 │ \tleft + right",
			"  16 │ }",
		])
	);

	// outlines keep the numbers of the lines they come from
	assert_eq!(
		text(&resolver, "crate::impls", numbered.clone()),
		lines(&[
			&format!("     │ // file: {}", native("query_basic/src/impls.rs")),
			"   1 │ //! Implementations, away from their types.",
			"   2 │",
			"   3 │ use crate::shapes::Circle;",
			"   4 │ use crate::shapes::Shape;",
			"   5 │ use crate::shapes::Square;",
			"   6 │",
			"   7 │ impl Circle {",
			"   8 │ \t/// Creates a circle.",
			"   9 │ \tpub fn new(radius: f64) -> Self { ... }",
			"  12 │ }",
			"  13 │",
			"  14 │ impl Shape for Circle {",
			"  15 │ \tfn area(&self) -> f64 { ... }",
			"  18 │ }",
			"  19 │",
			"  20 │ // squares are simple",
			"  21 │ impl Shape for Square {",
			"  22 │ \tfn area(&self) -> f64 { ... }",
			"  25 │",
			"  28 │ \tfn name(&self) -> &'static str { ... }",
			"  31 │ }",
			"  32 │",
			"  33 │ impl std::fmt::Display for Circle {",
			"  34 │ \tfn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { ... }",
			"  37 │ }",
		])
	);

	// the file of a module in full
	let full = text(&resolver, "crate::shapes", ViewOptions { mode: ViewMode::Full, ..numbered });
	let head = lines(&[
		&format!("     │ // file: {}", native("query_basic/src/shapes.rs")),
		"   1 │ //! Shapes.",
		"   2 │",
		"   3 │ /// A circle.",
	]);

	assert!(full.starts_with(&head), "{full}");
	assert!(full.ends_with(&lines(&["  28 │ \t}", "  29 │ }"])), "{full}");
}

#[test]
fn outlines_modules() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let auto = ViewOptions::default();
	let shapes = views(&resolver, "crate::shapes", auto.clone());

	// the view of an out-of-line module is located at its declaration
	assert_eq!(shapes[0].kind, ItemKind::Module);
	assert_eq!(shapes[0].file, Path::new("query_basic/src/lib.rs"));
	assert_eq!((shapes[0].start, shapes[0].end), (at(3, 1), at(3, 16)));
	assert_eq!(
		shapes[0].text,
		lines(&[
			&format!("// file: {}", native("query_basic/src/shapes.rs")),
			"//! Shapes.",
			"",
			"/// A circle.",
			"#[derive(Debug, Clone, Copy)]",
			"pub struct Circle {",
			"\t/// The radius.",
			"\tpub radius: f64,",
			"}",
			"",
			"/// A square.",
			"pub struct Square(pub f64);",
			"",
			"/// Kinds of shapes.",
			"pub enum Kind {",
			"\t/// Round.",
			"\tRound,",
			"\tSquare,",
			"}",
			"",
			"/// Something with an area.",
			"pub trait Shape {",
			"\t/// The area.",
			"\tfn area(&self) -> f64;",
			"",
			"\t/// The name, by default \"shape\".",
			"\tfn name(&self) -> &'static str { ... }",
			"}",
		])
	);

	// nested inline modules are collapsed; macros (but for the declarations of `thread_local!`) and functions elided
	assert_eq!(
		text(&resolver, "crate::nested", auto.clone()),
		lines(&[
			&format!("// file: {}", native("query_basic/src/nested.rs")),
			"//! Nesting, macros, constants, and raw identifiers.",
			"",
			"/// An outer module.",
			"pub mod outer { ... }",
			"",
			"macro_rules! square { ... }",
			"",
			"thread_local!(static COUNTER: u8 = 0);",
			"",
			"pub fn r#match() -> u8 { ... }",
			"",
			"pub mod r#type { ... }",
		])
	);

	// an inline module keeps its header, and its items are outlined (long initializers too)
	assert_eq!(
		text(&resolver, "crate::nested::outer", auto.clone()),
		lines(&[
			"/// An outer module.",
			"pub mod outer {",
			"\t/// An inner module.",
			"\tpub mod inner { ... }",
			"",
			"\tpub fn shallow() {}",
			"",
			"\t/// A table.",
			"\tpub const TABLE: [u8; 3] = ...;",
			"",
			"\tpub const SHORT: u8 = 1;",
			"",
			"\tpub static GREETING: &str = ...;",
			"",
			"\tpub fn text() -> &'static str { ... }",
			"",
			"\tpub trait Limits {",
			"\t\tconst MAX: u8 = ...;",
			"",
			"\t\tfn check() -> bool;",
			"\t}",
			"",
			"\tunsafe extern \"C\" {",
			"\t\tpub fn abs(value: i32) -> i32;",
			"\t}",
			"}",
		])
	);

	// a nested inline module is dedented
	assert_eq!(
		text(&resolver, "crate::nested::r#type", auto),
		lines(&[
			"pub mod r#type {",
			"\t/// A unit struct.",
			"\tpub struct Unit; // with a trailing comment",
			"",
			"\tpub type Alias = Unit;",
			"",
			"\tpub union Bits {",
			"\t\tpub int: u32,",
			"\t\tpub float: f32,",
			"\t}",
			"}",
		])
	);

	// the crate root
	assert_eq!(
		text(&resolver, "crate", ViewOptions { docs: false, ..ViewOptions::default() }),
		lines(&[
			&format!("// file: {}", native("query_basic/src/lib.rs")),
			"pub mod shapes;",
			"mod impls;",
			"pub mod nested;",
			"",
			"pub use nested::*;",
			"pub use shapes::Circle;",
			"pub use shapes::Shape as ShapeTrait;",
			"",
			"pub fn add(left: i32, right: i32) -> i32 { ... }",
			"",
			"#[cfg(unix)]",
			"pub fn platform() -> &'static str { ... }",
			"",
			"#[cfg(not(unix))]",
			"pub fn platform() -> &'static str { ... }",
			"",
			"#[cfg(feature = \"extra\")]",
			"pub mod extra { ... }",
		])
	);
}

#[test]
fn outlines_other_items() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let outline = options(ViewMode::Outline);

	assert_eq!(
		text(&resolver, "crate::add", outline.clone()),
		lines(&[
			"/// Adds two numbers.",
			"///",
			"/// With a second paragraph.",
			"pub fn add(left: i32, right: i32) -> i32 { ... }",
		])
	);
	assert_eq!(
		text(&resolver, "crate::shapes::Shape", outline.clone()),
		lines(&[
			"/// Something with an area.",
			"pub trait Shape {",
			"\t/// The area.",
			"\tfn area(&self) -> f64;",
			"",
			"\t/// The name, by default \"shape\".",
			"\tfn name(&self) -> &'static str { ... }",
			"}",
		])
	);
	assert_eq!(text(&resolver, "crate::nested::outer::GREETING", outline.clone()), "pub static GREETING: &str = ...;");
	assert_eq!(text(&resolver, "crate::nested::outer::SHORT", outline.clone()), "pub const SHORT: u8 = 1;");
	assert_eq!(text(&resolver, "crate::nested::outer::Limits::MAX", outline.clone()), "const MAX: u8 = ...;");
	assert_eq!(
		text(&resolver, "crate::shapes::Kind", outline.clone()),
		lines(&["/// Kinds of shapes.", "pub enum Kind {", "\t/// Round.", "\tRound,", "\tSquare,", "}"])
	);
	assert_eq!(
		text(&resolver, "<crate::shapes::Square as crate::shapes::Shape>", outline),
		lines(&[
			"impl Shape for Square {",
			"\tfn area(&self) -> f64 { ... }",
			"",
			"\tfn name(&self) -> &'static str { ... }",
			"}",
		])
	);
}

#[test]
fn outline_text_keeps_indentation() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let item = |path: &str| {
		let items = resolver.resolve_item_path(&path.parse().unwrap());

		assert_eq!(items.len(), 1, "{path}");
		items[0]
	};

	assert_eq!(
		outline_text(&workspace, item("crate::nested::outer::inner"), true),
		lines(&[
			"\t/// An inner module.",
			"\tpub mod inner {",
			"\t\t/// Deeply nested.",
			"\t\tpub fn deep() -> u8 { ... }",
			"\t}",
		])
	);
	assert_eq!(
		outline_text(&workspace, item("crate::nested::outer::inner::deep"), false),
		"\t\tpub fn deep() -> u8 { ... }"
	);
	assert_eq!(outline_text(&workspace, item("crate::nested::outer::shallow"), true), "\tpub fn shallow() {}");
}

#[test]
fn dedents_nested_items() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let auto = ViewOptions::default();

	assert_eq!(
		text(&resolver, "crate::nested::outer::inner::deep", auto.clone()),
		lines(&["/// Deeply nested.", "pub fn deep() -> u8 {", "\t1", "}"])
	);

	// lines starting inside of string literals keep their indentation
	assert_eq!(
		text(&resolver, "crate::nested::outer::text", auto.clone()),
		lines(&["pub fn text() -> &'static str {", "\tlet s = \"first", "second\";", "", "\ts", "}"])
	);
	assert_eq!(
		text(&resolver, "crate::nested::outer::GREETING", auto.clone()),
		lines(&["pub static GREETING: &str = \"hello", "  world\";"])
	);
	assert_eq!(
		text(&resolver, "crate::nested::square", auto.clone()),
		lines(&["macro_rules! square {", "\t($x:expr) => {", "\t\t$x * $x", "\t};", "}"])
	);
	assert_eq!(
		text(&resolver, "crate::nested::r#type::Unit", auto),
		lines(&["/// A unit struct.", "pub struct Unit;"])
	);
}

#[test]
fn views_impls() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let with_impls = ViewOptions { impls: true, ..ViewOptions::default() };
	let circle = views(&resolver, "crate::shapes::Circle", with_impls.clone());

	assert_eq!(circle.len(), 1);

	let impls: Vec<(&str, &str)> =
		circle[0].impls.iter().map(|view| (view.path.as_str(), view.text.as_str())).collect();

	assert_eq!(
		impls,
		[
			(
				"impl query_basic::shapes::Circle",
				lines(&["impl Circle {", "\t/// Creates a circle.", "\tpub fn new(radius: f64) -> Self { ... }", "}"])
					.as_str()
			),
			(
				"impl Shape for query_basic::shapes::Circle",
				lines(&["impl Shape for Circle {", "\tfn area(&self) -> f64 { ... }", "}"]).as_str()
			),
			(
				"impl std::fmt::Display for query_basic::shapes::Circle",
				lines(&[
					"impl std::fmt::Display for Circle {",
					"\tfn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { ... }",
					"}",
				])
				.as_str()
			),
		]
	);
	assert_eq!(circle[0].impls[0].kind, ItemKind::Impl);
	assert_eq!(circle[0].impls[0].file, Path::new("query_basic/src/impls.rs"));
	assert_eq!((circle[0].impls[0].start, circle[0].impls[0].end), (at(7, 1), at(12, 2)));
	assert!(circle[0].impls.iter().all(|view| view.impls.is_empty()));

	// in full
	let full = views(&resolver, "crate::shapes::Circle", ViewOptions { mode: ViewMode::Full, ..with_impls.clone() });

	assert_eq!(
		full[0].impls[1].text,
		lines(&[
			"impl Shape for Circle {",
			"\tfn area(&self) -> f64 {",
			"\t\tstd::f64::consts::PI * self.radius * self.radius",
			"\t}",
			"}",
		])
	);

	// the implementations of a trait
	let shape = views(&resolver, "crate::shapes::Shape", with_impls.clone());
	let impls: Vec<&str> = shape[0].impls.iter().map(|view| view.path.as_str()).collect();

	assert_eq!(impls, ["impl Shape for query_basic::shapes::Circle", "impl Shape for query_basic::shapes::Square"]);

	// functions have none
	assert!(views(&resolver, "crate::add", with_impls.clone())[0].impls.is_empty());

	// without the option, none
	assert!(views(&resolver, "crate::shapes::Circle", ViewOptions::default())[0].impls.is_empty());
}

#[test]
fn views_paths_in_order_without_duplicates() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let view = |paths: &[&str], options: ViewOptions| {
		let mut view = View::with_options(options);

		for path in paths {
			view = view.path(path).unwrap();
		}

		view.run_with(&resolver).unwrap().into_iter().map(|view| (view.path, view.active)).collect::<Vec<_>>()
	};

	assert_eq!(
		view(&["crate::add", "crate::Circle", "crate::shapes::Circle", "crate::add"], ViewOptions::default()),
		[("query_basic::add".to_owned(), Tristate::True), ("query_basic::shapes::Circle".to_owned(), Tristate::True),]
	);

	// every cfg variant, unless only active ones are wanted
	assert_eq!(
		view(&["crate::platform"], ViewOptions::default()),
		[("query_basic::platform".to_owned(), Tristate::True), ("query_basic::platform".to_owned(), Tristate::False),]
	);
	assert_eq!(
		view(&["crate::platform"], ViewOptions { active_only: true, ..ViewOptions::default() }),
		[("query_basic::platform".to_owned(), Tristate::True)]
	);

	// specific items
	let items: Vec<ItemId> =
		Find::new().pattern("platform").unwrap().run_with(&resolver).unwrap().iter().map(|found| found.item).collect();
	let viewed = View::new().items(&resolver, &[items[1], items[0], items[1]]).unwrap();

	assert_eq!(viewed.iter().map(|view| view.item).collect::<Vec<_>>(), [items[1], items[0]]);
	assert_eq!(viewed[0].cfg.as_deref(), Some("not(unix)"));
	assert_eq!(
		viewed[0].text,
		lines(&["/// Elsewhere.", "#[cfg(not(unix))]", "pub fn platform() -> &'static str {", "\t\"other\"", "}"])
	);
}

#[test]
fn reports_paths_naming_nothing() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);

	for path in ["crate::missing", "crate::shapes::Circle::missing", "<crate::shapes::Circle as Missing>"] {
		match View::new().path("crate::add").unwrap().path(path).unwrap().run_with(&resolver) {
			Err(Error::NotFound(missing)) => assert_eq!(missing, path.parse::<rscode::ItemPath>().unwrap().to_string()),
			other => panic!("`{path}`: {other:?}"),
		}
	}

	assert_eq!(
		View::new().path("crate::missing").unwrap().run_with(&resolver).unwrap_err().to_string(),
		"no item found for `crate::missing`"
	);
}

#[test]
fn serializes_views() {
	let workspace = workspace();
	let resolver = Resolver::new(&workspace);
	let view = &views(&resolver, "crate::nested::r#type::Unit", ViewOptions::default())[0];

	assert_eq!(
		serde_json::to_value(view).unwrap(),
		serde_json::json!({
			"path": "query_basic::nested::r#type::Unit",
			"kind": "struct",
			"file": native("query_basic/src/nested.rs"),
			"start": {"line": 59, "column": 2},
			"end": {"line": 60, "column": 18},
			"cfg": null,
			"active": "true",
			"text": "/// A unit struct.\npub struct Unit;",
		})
	);
}

/// A file with CRLF line breaks and a byte order mark.
#[test]
fn views_crlf_files() {
	let mut workspace = Workspace::new(fixture(""));

	workspace.load_crate(CrateSpec::new("query_crlf", fixture("query_crlf/lib.rs")));

	let resolver = Resolver::new(&workspace);
	let numbered = ViewOptions { line_numbers: true, ..ViewOptions::default() };

	// line breaks become `\n`, also inside of strings
	assert_eq!(
		text(&resolver, "crate::inline::f", ViewOptions::default()),
		lines(&["/// Docs.", "pub fn f() -> &'static str {", "\t\"a", "b\"", "}"])
	);
	assert_eq!(
		text(&resolver, "crate::inline::f", numbered),
		lines(&["   4 │ /// Docs.", "   5 │ pub fn f() -> &'static str {", "   6 │ \t\"a", "   7 │ b\"", "   8 │ }"])
	);
	assert_eq!(
		text(&resolver, "crate::inline::f", ViewOptions { docs: false, ..ViewOptions::default() }),
		lines(&["pub fn f() -> &'static str {", "\t\"a", "b\"", "}"])
	);

	// without the byte order mark
	assert_eq!(
		text(&resolver, "crate", options(ViewMode::Full)),
		lines(&[
			&format!("// file: {}", native("query_crlf/lib.rs")),
			"//! CRLF line breaks and a byte order mark.",
			"",
			"pub mod inline {",
			"\t/// Docs.",
			"\tpub fn f() -> &'static str {",
			"\t\t\"a",
			"b\"",
			"\t}",
			"}",
		])
	);
	assert_eq!(
		text(&resolver, "crate", ViewOptions { docs: false, ..ViewOptions::default() }),
		lines(&[&format!("// file: {}", native("query_crlf/lib.rs")), "pub mod inline { ... }"])
	);

	let f = one(&resolver, "f");

	assert_eq!((f.start, f.end), (at(4, 2), at(8, 3)));
}

/// Lines with their leading and trailing whitespace removed, without blank lines at the start and end.
fn trimmed_lines(text: &str) -> Vec<&str> {
	let lines: Vec<&str> = text.lines().map(str::trim).collect();
	let start = lines.iter().position(|line| !line.is_empty()).unwrap_or(lines.len());
	let end = lines.iter().rposition(|line| !line.is_empty()).map_or(start, |index| index + 1);

	lines[start..end].to_vec()
}

/// The line numbers of a view rendered with line numbers (`None` for lines that are not from the file).
fn line_numbers(text: &str) -> Vec<Option<usize>> {
	(text.lines())
		.map(|line| {
			let (number, _) = line.split_once(" │").unwrap_or_else(|| panic!("no line number in {line:?}"));

			number.trim().parse().ok()
		})
		.collect()
}

/// Every item of large crates from the cargo registry is found, and viewed in every mode without panicking. Views
/// in full keep the text of every line, and line numbers only increase.
#[test]
#[ignore = "slow; needs large crates in the cargo registry"]
fn robustness_on_large_registry_crates() {
	// dependencies of this workspace, at the versions `Cargo.lock` locks
	let crates = [
		("syn", "src/lib.rs"),
		("cargo", "src/lib.rs"),
		("regex", "src/lib.rs"),
		("libc", "src/lib.rs"),
		("serde_json", "src/lib.rs"),
		("tokio", "src/lib.rs"),
	];

	for (name, lib) in crates {
		let version = locked_version(name);

		let Some(dir) = registry_crate(name) else {
			println!("{name} {version}: not in cargo's registry, skipped");
			continue;
		};

		assert!(dir.join(lib).exists(), "{name} {version} has no {lib}: update its library path");

		let mut workspace = Workspace::new(&dir);

		workspace.load_crate(CrateSpec::new(name, dir.join(lib)));

		let resolver = Resolver::new(&workspace);
		let started = std::time::Instant::now();
		let all = Find::new().run_with(&resolver).unwrap();
		let unqualified = Find::new().pattern("**").unwrap().run_with(&resolver).unwrap();
		let impls = all.iter().filter(|found| found.kind == ItemKind::Impl).count();

		assert_eq!(all.len(), unqualified.len() + impls, "{name}");

		let items: Vec<ItemId> = all.iter().map(|found| found.item).collect();
		let found = started.elapsed();
		let started = std::time::Instant::now();

		for (mode, docs, numbered) in [
			(ViewMode::Full, true, false),
			(ViewMode::Full, true, true),
			(ViewMode::Outline, true, true),
			(ViewMode::Outline, false, true),
			(ViewMode::Auto, false, false),
		] {
			let options = ViewOptions { mode, docs, line_numbers: numbered, ..ViewOptions::default() };
			let views = View::with_options(options).items(&resolver, &items).unwrap();

			assert_eq!(views.len(), items.len());

			for view in &views {
				let data = workspace.item(view.item);
				let has_file = data.module_info().is_some_and(|info| !info.inline && info.file.is_some());

				assert!(
					!view.text.contains('\r') || view.text.lines().all(|line| !line.ends_with('\r')),
					"{}",
					view.path
				);

				if numbered {
					let numbers = line_numbers(&view.text);
					let from_file: Vec<usize> = numbers.iter().flatten().copied().collect();

					assert!(from_file.windows(2).all(|pair| pair[0] < pair[1]), "{}: {numbers:?}", view.path);

					if mode == ViewMode::Full {
						assert!(from_file.windows(2).all(|pair| pair[0] + 1 == pair[1]), "{}: {numbers:?}", view.path);
					}
				} else if mode == ViewMode::Full {
					let viewed = trimmed_lines(&view.text);

					if has_file {
						let file = workspace.krate(view.item.krate()).file(data.module_info().unwrap().file.unwrap());

						assert_eq!(
							viewed[1..],
							trimmed_lines(file.text().trim_start_matches('\u{feff}')),
							"{}",
							view.path
						);
					} else {
						assert_eq!(viewed, trimmed_lines(workspace.item_text(view.item)), "{}", view.path);
					}
				}
			}
		}

		for (item, data) in workspace.crates()[0].items() {
			if data.kind == ItemKind::Module {
				outline_text(&workspace, item, true);
			}
		}

		println!("{name} {version}: {} items, found in {found:?}, viewed 5 times in {:?}", items.len(), started.elapsed());
	}
}
