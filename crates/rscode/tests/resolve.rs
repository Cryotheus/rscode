//! Name resolution on crates loaded from fixtures.
//!
//! These tests need the loader, `ItemPath::parse`, and `CanonicalPath`'s `Display`.

use rscode::CrateSpec;
use rscode::Edition;
use rscode::ItemId;
use rscode::ItemPath;
use rscode::Resolver;
use rscode::Viewpoint;
use rscode::Workspace;
use rscode::model::Dependency;
use rscode::model::TargetKind;
use rscode::resolve::Namespace;
use rscode::resolve::Res;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

fn fixture(path: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(path)
}

fn dependency(name: &str, crate_name: &str) -> Dependency {
	Dependency {
		name: name.into(),
		crate_name: crate_name.into(),
		package: None,
		krate: None,
	}
}

fn load(specs: impl IntoIterator<Item = CrateSpec>) -> Workspace {
	let mut ws = Workspace::new(fixture(""));

	for spec in specs {
		ws.load_crate(spec);
	}

	ws.link();
	ws
}

fn load_one(name: &str, edition: Edition) -> Workspace {
	let mut spec = CrateSpec::new(name, fixture(&format!("{name}/src/lib.rs")));

	spec.edition = edition;
	load([spec])
}

fn path(text: &str) -> ItemPath {
	text.parse().unwrap_or_else(|error| panic!("{error}"))
}

fn resolve(resolver: &Resolver<'_>, text: &str) -> Vec<String> {
	resolver.resolve_item_path(&path(text)).into_iter().map(|item| resolver.canonical_path(item).to_string()).collect()
}

fn item(resolver: &Resolver<'_>, text: &str) -> ItemId {
	let items = resolver.resolve_item_path(&path(text));

	assert_eq!(items.len(), 1, "`{text}` names {} items", items.len());
	items[0]
}

fn show(resolver: &Resolver<'_>, items: &[ItemId]) -> Vec<String> {
	items.iter().map(|&item| resolver.canonical_path(item).to_string()).collect()
}

#[test]
fn reexports_globs_and_variants() {
	let ws = load_one("resolve_basic", Edition::E2021);
	let resolver = Resolver::new(&ws);

	assert!(ws.crates()[0].diagnostics().is_empty(), "{:?}", ws.crates()[0].diagnostics());
	assert_eq!(resolve(&resolver, "crate::Circle"), ["resolve_basic::shapes::Circle"]);
	assert_eq!(resolve(&resolver, "ShapeTrait"), ["resolve_basic::shapes::Shape"]);
	assert_eq!(resolve(&resolver, "crate::double"), ["resolve_basic::util::helpers::double"]);
	assert_eq!(resolve(&resolver, "crate::triple"), ["resolve_basic::util::helpers::triple"]);
	assert!(resolve(&resolver, "crate::hidden").is_empty(), "private items are not glob-imported");
	assert_eq!(resolve(&resolver, "crate::prelude::Box2"), ["resolve_basic::shapes::Square"]);
	assert_eq!(resolve(&resolver, "crate::prelude::shapes"), ["resolve_basic::shapes"]);
	assert_eq!(resolve(&resolver, "crate::prelude::Red"), ["resolve_basic::Color::Red"]);
	assert_eq!(resolve(&resolver, "::resolve_basic::Color::Blue"), ["resolve_basic::Color::Blue"]);
	assert_eq!(resolve(&resolver, "crate::Platform").len(), 2, "both cfg variants");
	assert_eq!(resolve(&resolver, "crate::exported"), ["resolve_basic::exported"]);
	assert_eq!(resolve(&resolver, "crate::macros::local_macro"), ["resolve_basic::macros::local_macro"]);
	assert!(resolver.unresolved_imports().is_empty());

	let prelude = item(&resolver, "crate::prelude");
	let values: Vec<String> = resolver.names(prelude, Namespace::Value).iter().map(ToString::to_string).collect();

	// `Circle` has named fields, and `Blue` is a struct variant: neither is a value
	assert_eq!(values, ["Box2", "Green", "Red"]);
}

#[test]
fn impls_and_associated_items() {
	let ws = load_one("resolve_basic", Edition::E2021);
	let resolver = Resolver::new(&ws);
	let circle = item(&resolver, "crate::Circle");
	let shape = item(&resolver, "crate::shapes::Shape");
	let area = item(&resolver, "crate::shapes::Shape::area");

	assert_eq!(
		show(&resolver, &resolver.impls_of(circle)),
		[
			"impl resolve_basic::shapes::Circle",
			"impl Shape for resolve_basic::shapes::Circle",
			"impl fmt::Display for resolve_basic::shapes::Circle",
		]
	);

	assert_eq!(resolver.impls_of(shape).len(), 3);

	assert_eq!(
		show(&resolver, &resolver.trait_counterparts(area)),
		[
			"<resolve_basic::shapes::Circle as Shape>::area",
			"<resolve_basic::shapes::Square as Shape>::area",
			"resolve_basic::shapes::<impl Shape for Vec<T>>::area",
		]
	);

	assert_eq!(resolve(&resolver, "crate::Circle::new"), ["resolve_basic::shapes::Circle::new"]);
	assert_eq!(resolve(&resolver, "<Circle as Display>::fmt"), ["<resolve_basic::shapes::Circle as fmt::Display>::fmt"]);
	assert_eq!(resolve(&resolver, "<crate::shapes::Square as Shape>::name"), ["<resolve_basic::shapes::Square as Shape>::name"]);
	assert_eq!(resolve(&resolver, "<Circle>::new"), ["resolve_basic::shapes::Circle::new"]);

	let circle_area = item(&resolver, "<Circle as Shape>::area");

	assert_eq!(show(&resolver, &resolver.trait_counterparts(circle_area)), ["resolve_basic::shapes::Shape::area"]);
	assert_eq!(show(&resolver, &resolver.associated_items(circle))[0], "resolve_basic::shapes::Circle::new");
}

#[test]
fn usable_paths_and_visibility() {
	let ws = load_one("resolve_basic", Edition::E2021);
	let resolver = Resolver::new(&ws);
	let root = ws.crates()[0].root_module();
	let circle = item(&resolver, "crate::Circle");
	let triple = item(&resolver, "crate::util::helpers::triple");

	assert_eq!(
		resolver.usable_paths(circle, Viewpoint::Foreign),
		["::resolve_basic::Circle", "::resolve_basic::shapes::Circle", "::resolve_basic::prelude::Circle", "::resolve_basic::prelude::shapes::Circle"]
	);

	assert!(resolver.usable_paths(triple, Viewpoint::Foreign).is_empty());
	assert_eq!(resolver.usable_paths(triple, Viewpoint::Module(root)), ["triple", "crate::triple", "crate::util::helpers::triple"]);

	let vis = item(&resolver, "crate::vis");
	let inner = item(&resolver, "crate::vis::inner");
	let visible = |item_path: &str, module: ItemId| resolver.is_visible_from(item(&resolver, item_path), module);

	assert!(visible("crate::vis::crate_visible", root));
	assert!(visible("crate::vis::super_visible", root));
	assert!(visible("crate::vis::in_vis", inner));
	assert!(!visible("crate::vis::in_vis", root));
	assert!(visible("crate::vis::inner::in_vis_inner", vis));
	assert!(!visible("crate::vis::private", root));
}

#[test]
fn edition_2015_paths() {
	let ws = load_one("resolve_2015", Edition::E2015);
	let resolver = Resolver::new(&ws);
	let s = item(&resolver, "crate::a::S");
	let b = item(&resolver, "crate::b");
	let c = item(&resolver, "crate::c");

	assert_eq!(resolver.bindings(b, "S", Namespace::Type).first().map(|binding| binding.res.clone()), Some(Res::Item(s)));
	assert_eq!(resolver.bindings(c, "T", Namespace::Type).first().map(|binding| binding.res.clone()), Some(Res::Item(s)));
	assert_eq!(resolver.bindings(c, "fmt", Namespace::Type).first().map(|binding| binding.res.clone()), Some(Res::External("std::fmt".into())));
	assert!(resolver.unresolved_imports().is_empty());
}

#[test]
fn glob_cycles() {
	let ws = load_one("resolve_cycles", Edition::E2021);
	let resolver = Resolver::new(&ws);

	assert_eq!(resolve(&resolver, "crate::a::B"), ["resolve_cycles::b::B"]);
	assert_eq!(resolve(&resolver, "crate::b::A"), ["resolve_cycles::a::A"]);
	assert_eq!(resolve(&resolver, "crate::B"), ["resolve_cycles::b::B"]);
}

#[test]
fn workspace_with_renamed_dependency() {
	let lib = CrateSpec::new("mylib", fixture("resolve_workspace/mylib/src/lib.rs"));
	let mut app = CrateSpec::new("app", fixture("resolve_workspace/app/src/main.rs"));

	app.kind = TargetKind::Bin;
	app.dependencies = vec![dependency("mylib", "mylib"), dependency("renamed", "mylib")];

	let ws = load([lib, app]);
	let resolver = Resolver::new(&ws);
	let client = item(&resolver, "::mylib::Client");
	let app_root = ws.crates()[1].root_module();

	assert_eq!(resolver.crate_by_name(app_root.krate(), "renamed"), Some(client.krate()));
	assert_eq!(resolve(&resolver, "renamed::Client"), ["mylib::api::Client"]);
	assert_eq!(resolve(&resolver, "::app::C"), ["mylib::api::Client"]);

	assert_eq!(
		resolver.usable_paths(client, Viewpoint::Module(app_root)),
		["C", "crate::C", "mylib::Client", "renamed::Client", "crate::api::Client", "mylib::api::Client", "renamed::api::Client"]
	);

	assert!(resolver.usable_paths(item(&resolver, "::mylib::hidden::Secret"), Viewpoint::Module(app_root)).is_empty());
	assert_eq!(resolve(&resolver, "::mylib::Client::connect"), ["mylib::api::Client::connect"]);
}

#[test]
fn robustness_on_this_repository_and_syn() {
	let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("crates directory");
	let mut specs = Vec::new();

	for entry in std::fs::read_dir(crates_dir).expect("crates directory").flatten() {
		let name = entry.file_name().to_string_lossy().replace('-', "_");

		for (root, kind) in [("src/lib.rs", TargetKind::Lib), ("src/main.rs", TargetKind::Bin)] {
			let path = entry.path().join(root);

			if path.exists() {
				let mut spec = CrateSpec::new(name.clone(), path);

				spec.kind = kind;
				spec.dependencies = ["rscode", "rscode_fmt", "rscode_sort", "serde", "syn", "proc_macro2", "quote", "smol_str"]
					.iter()
					.map(|name| dependency(name, name))
					.collect();

				specs.push(spec);
			}
		}
	}

	let home = std::env::home_dir().map(|home| home.join(".cargo"));
	let registry = std::env::var_os("CARGO_HOME").map(PathBuf::from).or(home).map(|home| home.join("registry/src"));

	let syn = (registry.and_then(|registry| std::fs::read_dir(registry).ok()).into_iter().flatten().flatten())
		.map(|index| index.path().join("syn-3.0.6/src/lib.rs"))
		.find(|path| path.exists());

	if let Some(syn) = syn {
		let mut spec = CrateSpec::new("syn", syn);

		spec.selected = false;
		specs.push(spec);
	}

	let started = Instant::now();
	let ws = load(specs);
	let loaded = started.elapsed();
	let started = Instant::now();
	let resolver = Resolver::new(&ws);
	let resolved = started.elapsed();
	let started = Instant::now();

	for krate in ws.crates() {
		for (item, _) in krate.items() {
			resolver.canonical_path(item);
			resolver.usable_paths(item, Viewpoint::Foreign);
		}
	}

	let items: usize = ws.crates().iter().map(|krate| krate.items().count()).sum();

	println!(
		"{} crates, {items} items: loaded in {loaded:?}, resolved in {resolved:?}, queried in {:?}; {} unresolved imports",
		ws.crates().len(),
		started.elapsed(),
		resolver.unresolved_imports().len()
	);

	assert_eq!(resolve(&resolver, "::rscode::Resolver"), ["rscode::resolve::Resolver"]);
}
