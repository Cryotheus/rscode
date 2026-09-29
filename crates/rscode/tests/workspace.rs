//! Tests for planning and loading cargo workspaces (`rscode::workspace`), on the fixture workspaces in
//! `tests/fixtures/ws_*` (which have no registry dependencies, so everything works offline) and on this repository.

#![cfg(feature = "cargo")]

use rscode::CfgExpr;
use rscode::Error;
use rscode::Tristate;
use rscode::model::CrateSpec;
use rscode::model::Package;
use rscode::model::TargetKind;
use rscode::workspace::LoadOptions;
use rscode::workspace::TargetSelection;
use rscode::workspace::WorkspacePlan;
use rscode::workspace::load_workspace;
use rscode::workspace::plan_workspace;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

/// A fixture's path, with the platform's separators only (like cargo prints paths).
fn fixture(path: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(path).components().collect()
}

/// This repository's root manifest.
fn repository() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml")
}

/// Options for the manifest of a fixture (cargo's messages discarded).
fn options(manifest: &str) -> LoadOptions {
	LoadOptions {
		manifest_path: Some(fixture(manifest)),
		silent: true,
		..LoadOptions::default()
	}
}

fn virtual_ws() -> LoadOptions {
	options("ws_virtual/Cargo.toml")
}

fn devdeps() -> LoadOptions {
	options("ws_devdeps/Cargo.toml")
}

fn devdeps_v1() -> LoadOptions {
	options("ws_devdeps_v1/Cargo.toml")
}

/// Options for `ws_vendored`, whose registry dependency is taken from its `vendor` directory.
fn vendored() -> LoadOptions {
	with(options("ws_vendored/Cargo.toml"), |options| options.config = vendored_config(&fixture("ws_vendored")))
}

/// cargo configuration that replaces the registry with the `vendor` directory of a workspace.
fn vendored_config(workspace: &Path) -> Vec<String> {
	vec![
		r#"source.crates-io.replace-with = "vendored""#.to_owned(),
		format!("source.vendored.directory = '{}'", workspace.join("vendor").display()),
	]
}

/// Writes the files of a workspace into a new directory of cargo's directory for temporary test data.
fn temp_workspace(name: &str, files: &[(&str, &str)]) -> PathBuf {
	let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("workspace-tests").join(name);

	if root.exists() {
		std::fs::remove_dir_all(&root).unwrap();
	}

	for (path, contents) in files {
		let path = root.join(path);

		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(path, contents).unwrap();
	}

	root
}

/// Copies a fixture into a new directory of cargo's directory for temporary test data.
fn temp_copy(fixture_name: &str, name: &str) -> PathBuf {
	fn copy(from: &Path, to: &Path) {
		std::fs::create_dir_all(to).unwrap();

		for entry in std::fs::read_dir(from).unwrap() {
			let entry = entry.unwrap();

			if entry.file_type().unwrap().is_dir() {
				copy(&entry.path(), &to.join(entry.file_name()));
			} else {
				std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
			}
		}
	}

	let root = temp_workspace(name, &[]);

	copy(&fixture(fixture_name), &root);
	root
}

/// Options for the root manifest of a workspace outside the fixtures.
fn manifest(workspace: &Path) -> LoadOptions {
	LoadOptions {
		manifest_path: Some(workspace.join("Cargo.toml")),
		silent: true,
		..LoadOptions::default()
	}
}

fn strings(values: &[&str]) -> Vec<String> {
	values.iter().map(|&value| value.to_owned()).collect()
}

fn with(mut options: LoadOptions, change: impl FnOnce(&mut LoadOptions)) -> LoadOptions {
	change(&mut options);
	options
}

fn packages(options: LoadOptions, packages: &[&str]) -> LoadOptions {
	with(options, |options| options.packages = packages.iter().map(|&package| package.to_owned()).collect())
}

fn features(options: LoadOptions, features: &[&str]) -> LoadOptions {
	with(options, |options| options.features = features.iter().map(|&feature| feature.to_owned()).collect())
}

fn targets(options: LoadOptions, change: impl FnOnce(&mut TargetSelection)) -> LoadOptions {
	with(options, |options| change(&mut options.targets))
}

fn plan(options: &LoadOptions) -> WorkspacePlan {
	plan_workspace(options).unwrap_or_else(|error| panic!("planning failed: {error}"))
}

/// The error of a plan that must fail.
fn error(options: &LoadOptions) -> Error {
	match plan_workspace(options) {
		Ok(plan) => panic!("planning should fail, but planned {:?}", crates(&plan)),
		Err(error) => error,
	}
}

fn cargo_error(options: &LoadOptions) -> String {
	match error(options) {
		Error::Cargo(message) => message,
		other => panic!("expected a cargo error, got {other:?}"),
	}
}

/// `name kind` of every planned crate, with ` (unselected)` for unselected ones.
fn crates(plan: &WorkspacePlan) -> Vec<String> {
	plan.crates
		.iter()
		.map(|spec| {
			let unselected = if spec.selected { "" } else { " (unselected)" };

			format!("{} {}{unselected}", spec.name, spec.kind)
		})
		.collect()
}

fn package_names(plan: &WorkspacePlan) -> Vec<&str> {
	plan.packages.iter().map(|package| package.name.as_str()).collect()
}

fn package<'a>(plan: &'a WorkspacePlan, name: &str) -> &'a Package {
	plan.packages.iter().find(|package| package.name == name).unwrap_or_else(|| panic!("no package `{name}` in {:?}", package_names(plan)))
}

fn enabled_features<'a>(plan: &'a WorkspacePlan, name: &str) -> Vec<&'a str> {
	package(plan, name).enabled_features.iter().map(|feature| feature.as_str()).collect()
}

fn krate<'a>(plan: &'a WorkspacePlan, name: &str, kind: TargetKind) -> &'a CrateSpec {
	plan.crates
		.iter()
		.find(|spec| spec.name == name && spec.kind == kind)
		.unwrap_or_else(|| panic!("no {kind} crate `{name}` in {:?}", crates(plan)))
}

fn eval(spec: &CrateSpec, predicate: &str) -> Tristate {
	spec.cfg.eval(&CfgExpr::parse(predicate).unwrap())
}

/// `name` (or `name=crate_name` when they differ) `(package)` of every dependency in the extern prelude.
fn prelude(spec: &CrateSpec) -> Vec<String> {
	spec.dependencies
		.iter()
		.map(|dependency| {
			let name = if dependency.name == dependency.crate_name {
				dependency.name.to_string()
			} else {
				format!("{}={}", dependency.name, dependency.crate_name)
			};

			format!("{name} ({})", dependency.package.as_deref().unwrap_or("?"))
		})
		.collect()
}

fn host_is(predicate: bool) -> Tristate {
	Tristate::from(predicate)
}

// ---- package selection

#[test]
fn selects_default_members() {
	let plan = plan(&virtual_ws());

	assert_eq!(plan.root, fixture("ws_virtual"));
	assert_eq!(package_names(&plan), ["app", "tool"]);

	// `app-cli` requires the `cli` feature
	assert_eq!(crates(&plan), ["app lib", "app bin", "other bin", "tool bin"]);
}

#[test]
fn selects_packages_by_spec() {
	let selected = |specs: &[&str]| package_names(&plan(&packages(virtual_ws(), specs))).join(" ");

	assert_eq!(selected(&["real-core"]), "real-core");
	assert_eq!(selected(&["real-core@0.3.1"]), "real-core");
	assert_eq!(selected(&["real-*"]), "real-core");
	assert_eq!(selected(&["tool", "extra"]), "extra tool");
	assert_eq!(selected(&["*"]), "app real-core extra macros tool");
}

#[test]
fn selects_workspace_with_excludes() {
	let workspace = with(virtual_ws(), |options| options.workspace = true);
	let excluding = |excludes: &[&str]| {
		let options = with(workspace.clone(), |options| options.exclude = excludes.iter().map(|&exclude| exclude.to_owned()).collect());

		package_names(&plan(&options)).join(" ")
	};

	assert_eq!(package_names(&plan(&workspace)), ["app", "real-core", "extra", "macros", "tool"]);
	assert_eq!(excluding(&["tool", "ma*"]), "app real-core extra");

	// cargo only warns about unknown exclusions
	assert_eq!(excluding(&["nope"]), "app real-core extra macros tool");
}

#[test]
fn unknown_packages_are_errors() {
	let message = cargo_error(&packages(virtual_ws(), &["nope"]));

	assert!(message.starts_with("package(s) `nope` not found in workspace `"), "{message}");
	assert!(message.contains("ws_virtual"), "{message}");

	let message = cargo_error(&packages(virtual_ws(), &["zz*"]));

	assert!(message.starts_with("package pattern(s) `zz*` not found in workspace"), "{message}");

	let message = cargo_error(&packages(virtual_ws(), &["real-core@9.9.9"]));

	assert!(message.contains("real-core@9.9.9"), "{message}");

	let message = cargo_error(&with(virtual_ws(), |options| options.exclude = vec!["app".to_owned()]));

	assert_eq!(message, "--exclude can only be used together with --workspace");
}

#[test]
fn selects_the_package_of_a_member_manifest() {
	let plan = plan(&options("ws_virtual/core/Cargo.toml"));

	assert_eq!(plan.root, fixture("ws_virtual"));
	assert_eq!(package_names(&plan), ["real-core"]);
	assert_eq!(crates(&plan), ["real_core lib"]);
}

#[test]
fn selects_the_root_package_of_a_rooted_workspace() {
	let root = plan(&options("ws_rooted/Cargo.toml"));
	let member = plan(&options("ws_rooted/member/Cargo.toml"));
	let workspace = plan(&with(options("ws_rooted/Cargo.toml"), |options| options.workspace = true));

	assert_eq!(package_names(&root), ["rooted"]);
	assert_eq!(package_names(&member), ["rooted-member"]);
	assert_eq!(member.root, fixture("ws_rooted"));

	// cargo lists the root package after the members it lists explicitly
	assert_eq!(package_names(&workspace), ["rooted-member", "rooted"]);
	assert_eq!(crates(&workspace), ["rooted_member lib", "rooted lib"]);
}

#[test]
fn finds_the_manifest_from_the_current_directory() {
	// tests run in the package's directory
	let plan = plan(&LoadOptions {
		silent: true,
		..LoadOptions::default()
	});

	assert_eq!(plan.root, Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap());
	assert_eq!(package_names(&plan), ["rscode"]);
	assert_eq!(crates(&plan), ["rscode lib"]);
}

#[test]
fn invalid_manifest_paths_are_errors() {
	let missing = cargo_error(&options("ws_virtual/nope/Cargo.toml"));
	let directory = cargo_error(&options("ws_virtual"));
	let script = cargo_error(&options("ws_virtual/app/src/lib.rs"));

	assert!(missing.contains("does not exist"), "{missing}");
	assert!(directory.contains("is a directory but expected a file"), "{directory}");

	// cargo takes a `.rs` file for a script with an embedded manifest
	assert!(script.contains("requires `-Zscript`"), "{script}");
}

// ---- target selection

#[test]
fn default_targets_skip_unmet_required_features() {
	let app = packages(virtual_ws(), &["app"]);

	assert_eq!(crates(&plan(&app)), ["app lib", "app bin", "other bin"]);
	assert_eq!(crates(&plan(&features(app.clone(), &["cli"]))), ["app lib", "app bin", "app_cli bin", "other bin"]);
	assert_eq!(crates(&plan(&features(app, &["app/cli"]))), ["app lib", "app bin", "app_cli bin", "other bin"]);
}

#[test]
fn named_targets_ignore_required_features() {
	let options = targets(packages(virtual_ws(), &["app"]), |targets| targets.bins = vec!["app-cli".to_owned()]);

	assert_eq!(crates(&plan(&options)), ["app_cli bin"]);
}

#[test]
fn selects_libraries() {
	let lib = |options: LoadOptions| crates(&plan(&targets(options, |targets| targets.lib = true)));

	assert_eq!(lib(virtual_ws()), ["app lib"]);
	assert_eq!(lib(packages(virtual_ws(), &["macros"])), ["macros proc-macro"]);

	let message = cargo_error(&targets(packages(virtual_ws(), &["tool"]), |targets| targets.lib = true));

	assert_eq!(message, "no library targets found in package `tool`");
}

#[test]
fn selects_binaries() {
	let app = packages(virtual_ws(), &["app"]);
	let bins = |names: &[&str]| {
		let options = targets(virtual_ws(), |targets| targets.bins = names.iter().map(|&name| name.to_owned()).collect());

		crates(&plan(&options))
	};

	assert_eq!(crates(&plan(&targets(app, |targets| targets.all_bins = true))), ["app bin", "other bin"]);
	assert_eq!(bins(&["other"]), ["other bin"]);
	assert_eq!(bins(&["other", "tool"]), ["other bin", "tool bin"]);

	// named by a glob pattern, so the required features do not matter
	assert_eq!(bins(&["app*"]), ["app bin", "app_cli bin"]);
	assert_eq!(bins(&["app", "a?p"]), ["app bin"]);
}

#[test]
fn selects_examples_tests_and_benches() {
	let app = packages(virtual_ws(), &["app"]);
	let workspace = with(virtual_ws(), |options| options.workspace = true);
	let chosen = |options: &LoadOptions, change: fn(&mut TargetSelection)| crates(&plan(&targets(options.clone(), change)));

	assert_eq!(chosen(&app, |targets| targets.all_examples = true), ["demo example"]);
	assert_eq!(chosen(&app, |targets| targets.examples = vec!["demo".to_owned()]), ["demo example"]);
	assert_eq!(chosen(&app, |targets| targets.all_tests = true), ["integration test"]);
	assert_eq!(chosen(&app, |targets| targets.tests = vec!["integration".to_owned()]), ["integration test"]);
	assert_eq!(chosen(&app, |targets| targets.all_benches = true), ["speed bench"]);
	assert_eq!(chosen(&app, |targets| targets.benches = vec!["speed".to_owned()]), ["speed bench"]);
	assert_eq!(chosen(&workspace, |targets| targets.all_tests = true), ["integration test", "expand test"]);
	assert_eq!(
		chosen(&app, |targets| {
			targets.lib = true;
			targets.all_tests = true;
			targets.examples = vec!["demo".to_owned()];
		}),
		["app lib", "demo example", "integration test"]
	);
}

#[test]
fn selects_all_targets() {
	let app = targets(packages(virtual_ws(), &["app"]), |targets| targets.all_targets = true);

	assert_eq!(crates(&plan(&app)), ["app lib", "app bin", "other bin", "demo example", "integration test", "speed bench"]);
	assert_eq!(
		crates(&plan(&features(app, &["cli"]))),
		["app lib", "app bin", "app_cli bin", "other bin", "demo example", "integration test", "speed bench"]
	);

	let all = targets(with(virtual_ws(), |options| options.workspace = true), |targets| targets.all_targets = true);

	assert_eq!(
		crates(&plan(&all)),
		[
			"app lib",
			"app bin",
			"other bin",
			"demo example",
			"integration test",
			"speed bench",
			"real_core lib",
			"extra lib",
			"macros proc-macro",
			"expand test",
			"tool bin",
		]
	);
}

#[test]
fn unknown_target_names_are_errors() {
	let bin = |options: LoadOptions, name: &str| cargo_error(&targets(options, |targets| targets.bins = vec![name.to_owned()]));

	assert_eq!(
		bin(virtual_ws(), "othre"),
		"no bin target named `othre` in default-run packages\n\nhelp: a target with a similar name exists: `other`"
	);
	assert_eq!(
		bin(virtual_ws(), "zzzzzz"),
		"no bin target named `zzzzzz` in default-run packages\nhelp: available bin targets:\n    app\n    app-cli\n    other\n    tool"
	);
	assert_eq!(
		bin(packages(virtual_ws(), &["tool"]), "other"),
		"no bin target named `other` in `tool` package\nhelp: available bin in `app` package:\n    other"
	);
	assert_eq!(
		bin(virtual_ws(), "zzzz*"),
		"no bin target matches pattern `zzzz*` in default-run packages\n\
		help: available bin targets:\n    app\n    app-cli\n    other\n    tool"
	);

	let example = cargo_error(&targets(virtual_ws(), |targets| targets.examples = vec!["nope".to_owned()]));

	assert_eq!(example, "no example target named `nope` in default-run packages\nhelp: available example targets:\n    demo");

	let test = cargo_error(&targets(packages(virtual_ws(), &["tool", "real-core"]), |targets| targets.tests = vec!["expand".to_owned()]));

	assert_eq!(test, "no test target named `expand` in `tool`, ... packages\nhelp: available test in `macros` package:\n    expand");
}

#[test]
fn bulk_filters_may_choose_nothing() {
	let plan = plan(&targets(packages(virtual_ws(), &["tool"]), |targets| targets.all_examples = true));

	assert!(plan.crates.is_empty());
	assert_eq!(package_names(&plan), ["tool"]);
}

#[test]
fn loads_all_members() {
	let plan = plan(&with(packages(virtual_ws(), &["tool"]), |options| options.load_all_members = true));

	assert_eq!(
		crates(&plan),
		[
			"tool bin",
			"app lib (unselected)",
			"app bin (unselected)",
			"app_cli bin (unselected)",
			"other bin (unselected)",
			"demo example (unselected)",
			"integration test (unselected)",
			"speed bench (unselected)",
			"real_core lib (unselected)",
			"extra lib (unselected)",
			"macros proc-macro (unselected)",
			"expand test (unselected)",
		]
	);

	assert_eq!(package_names(&plan), ["app", "real-core", "extra", "macros", "tool"]);
	assert_eq!(plan.selected_crates().count(), 1);

	// features that the selected `tool` enables on the members it depends on
	assert_eq!(enabled_features(&plan, "app"), ["default"]);
	assert_eq!(enabled_features(&plan, "real-core"), ["alloc", "default", "std"]);
	assert_eq!(enabled_features(&plan, "macros"), ["default", "pmf"]);

	// through `weird-name`, which is not a member (and on Windows, directly)
	if cfg!(not(windows)) {
		assert_eq!(enabled_features(&plan, "extra"), ["shiny"]);
	}

	// only loaded (unselected), with its default features
	let plan = self::plan(&with(packages(virtual_ws(), &["real-core"]), |options| options.load_all_members = true));

	assert_eq!(enabled_features(&plan, "extra"), ["basic", "default"]);
	assert_eq!(enabled_features(&plan, "app"), ["default"]);
	assert!(enabled_features(&plan, "tool").is_empty());

	// selected crates are not duplicated
	let all = self::plan(&with(virtual_ws(), |options| {
		options.workspace = true;
		options.targets.all_targets = true;
		options.load_all_members = true;
	}));

	assert_eq!(crates(&all).iter().filter(|name| name.ends_with("(unselected)")).collect::<Vec<_>>(), ["app_cli bin (unselected)"]);
}

#[test]
fn package_indices_match_the_package_list() {
	let plan = plan(&with(virtual_ws(), |options| {
		options.workspace = true;
		options.load_all_members = true;
	}));

	for spec in &plan.crates {
		let package = &plan.packages[spec.package.unwrap().index()];
		let manifest_dir = package.manifest_path.parent().unwrap();

		assert!(spec.root.starts_with(manifest_dir), "{} is not in package {}", spec.root.display(), package.name);
	}
}

// ---- crates

#[test]
fn describes_crates() {
	let plan = plan(&targets(with(virtual_ws(), |options| options.workspace = true), |targets| targets.all_targets = true));
	let describe = |name: &str, kind: TargetKind| {
		let spec = krate(&plan, name, kind);
		let root = spec.root.strip_prefix(fixture("ws_virtual")).unwrap().display().to_string();
		let root = root.replace(std::path::MAIN_SEPARATOR, "/");

		format!("{root} {} {}", spec.edition, plan.packages[spec.package.unwrap().index()].name)
	};

	assert_eq!(describe("app", TargetKind::Lib), "app/src/lib.rs 2024 app");
	assert_eq!(describe("app", TargetKind::Bin), "app/src/main.rs 2024 app");
	assert_eq!(describe("other", TargetKind::Bin), "app/src/bin/other.rs 2024 app");
	assert_eq!(describe("demo", TargetKind::Example), "app/examples/demo.rs 2024 app");
	assert_eq!(describe("integration", TargetKind::Test), "app/tests/integration.rs 2024 app");
	assert_eq!(describe("speed", TargetKind::Bench), "app/benches/speed.rs 2024 app");
	assert_eq!(describe("real_core", TargetKind::Lib), "core/src/lib.rs 2021 real-core");
	assert_eq!(describe("extra", TargetKind::Lib), "extra/src/lib.rs 2018 extra");
	assert_eq!(describe("macros", TargetKind::ProcMacro), "macros/src/lib.rs 2021 macros");
	assert_eq!(describe("expand", TargetKind::Test), "macros/tests/expand.rs 2021 macros");
	assert_eq!(describe("tool", TargetKind::Bin), "tool/src/main.rs 2018 tool");

	let rooted = self::plan(&options("ws_rooted/Cargo.toml"));

	assert_eq!(krate(&rooted, "rooted", TargetKind::Lib).edition.as_str(), "2015");
}

#[test]
fn records_the_members_that_are_not_loaded() {
	// the default members are `app` and `tool`
	let plan = plan(&virtual_ws());
	let unloaded: Vec<(&str, Vec<&str>)> = plan
		.unloaded_members
		.iter()
		.map(|member| (member.name.as_str(), member.crate_names.iter().map(|name| name.as_str()).collect()))
		.collect();

	assert_eq!(unloaded, [("real-core", vec!["real_core"]), ("extra", vec!["extra"]), ("macros", vec!["macros", "expand"])]);
	assert_eq!(plan.unloaded_members[0].manifest_path, fixture("ws_virtual/core/Cargo.toml"));
	assert_eq!(plan.unloaded_members[0].version, "0.3.1");

	// every member is loaded, as unselected crates, when references are searched everywhere
	assert!(self::plan(&with(virtual_ws(), |options| options.load_all_members = true)).unloaded_members.is_empty());
	assert!(self::plan(&with(virtual_ws(), |options| options.workspace = true)).unloaded_members.is_empty());

	let workspace = plan.load();
	let path = |text: &str| rscode::ItemPath::parse(text).unwrap();

	assert_eq!(workspace.unloaded_members().len(), 3);
	assert_eq!(workspace.unloaded_member_of(&path("::expand::x")).map(|member| member.name.as_str()), Some("macros"));
	assert_eq!(workspace.unloaded_member_of(&path("<real_core::X as Y>::z")).map(|member| member.name.as_str()), Some("real-core"));
	assert!(workspace.unloaded_member_of(&path("crate::extra")).is_none());
	assert!(workspace.unloaded_member_of(&path("app::x")).is_none());
}

#[test]
fn describes_packages() {
	let plan = plan(&packages(virtual_ws(), &["real-core", "app"]));
	let core = package(&plan, "real-core");
	let table: BTreeMap<&str, Vec<&str>> = core
		.features
		.iter()
		.map(|(name, values)| (name.as_str(), values.iter().map(|value| value.as_str()).collect()))
		.collect();

	assert_eq!(core.version, "0.3.1");
	assert_eq!(core.manifest_path, fixture("ws_virtual/core/Cargo.toml"));
	assert!(core.is_member);
	assert_eq!(
		table,
		BTreeMap::from([
			("alloc", vec![]),
			("default", vec!["std"]),
			("full", vec!["std", "alloc", "serde"]),
			("serde", vec!["dep:extra"]),
			("shiny", vec!["extra?/shiny"]),
			("std", vec![]),
		])
	);

	assert_eq!(package(&plan, "app").features["json"], ["kore/serde"]);
}

// ---- features

#[test]
fn enables_default_features() {
	let plan = plan(&packages(virtual_ws(), &["real-core"]));
	let core = krate(&plan, "real_core", TargetKind::Lib);

	assert_eq!(enabled_features(&plan, "real-core"), ["default", "std"]);
	assert_eq!(eval(core, r#"feature = "std""#), Tristate::True);
	assert_eq!(eval(core, r#"feature = "alloc""#), Tristate::False);
	assert_eq!(eval(core, r#"feature = "serde""#), Tristate::False);

	// the optional dependency is not activated
	assert!(prelude(core).is_empty());
}

#[test]
fn enables_requested_features() {
	let core = packages(virtual_ws(), &["real-core"]);

	for requested in [&["alloc,serde"][..], &["alloc serde"], &["alloc", "serde"], &["real-core/alloc", "serde"]] {
		let plan = plan(&features(core.clone(), requested));

		assert_eq!(enabled_features(&plan, "real-core"), ["alloc", "default", "serde", "std"], "{requested:?}");
		assert_eq!(prelude(krate(&plan, "real_core", TargetKind::Lib)), ["extra (extra)"], "{requested:?}");
	}

	let plan = plan(&features(core, &["full"]));

	assert_eq!(enabled_features(&plan, "real-core"), ["alloc", "default", "full", "serde", "std"]);
}

#[test]
fn enables_all_features() {
	let plan = plan(&with(packages(virtual_ws(), &["real-core"]), |options| {
		options.all_features = true;
		options.load_all_members = true;
	}));

	assert_eq!(enabled_features(&plan, "real-core"), ["alloc", "default", "full", "serde", "shiny", "std"]);

	// activated by `serde` without default features, with `shiny` from the weak `extra?/shiny`
	assert_eq!(enabled_features(&plan, "extra"), ["shiny"]);
}

#[test]
fn disables_default_features() {
	let plan = plan(&with(packages(virtual_ws(), &["real-core"]), |options| options.no_default_features = true));
	let core = krate(&plan, "real_core", TargetKind::Lib);

	assert!(enabled_features(&plan, "real-core").is_empty());
	assert_eq!(eval(core, r#"feature = "std""#), Tristate::False);
	assert_eq!(eval(core, r#"feature = "default""#), Tristate::False);
}

#[test]
fn enables_features_of_dependencies() {
	// `json = ["kore/serde"]` reaches `real-core` through its renamed dependency
	let plan = plan(&with(features(virtual_ws(), &["app/json"]), |options| options.workspace = true));

	assert_eq!(enabled_features(&plan, "app"), ["default", "json"]);
	assert_eq!(enabled_features(&plan, "real-core"), ["alloc", "default", "serde", "std"]);
	assert_eq!(enabled_features(&plan, "tool"), Vec::<&str>::new());

	// its own default features, `shiny` from `tool`'s dependency `weird-name`, and on Windows `windows` from `tool`
	let extra =
		if cfg!(windows) { vec!["basic", "default", "shiny", "windows"] } else { vec!["basic", "default", "shiny"] };

	assert_eq!(enabled_features(&plan, "extra"), extra);

	// without being selected itself, `extra` only gets what `real-core` enables (no default features)
	let plan = self::plan(&with(features(packages(virtual_ws(), &["app"]), &["json"]), |options| options.load_all_members = true));

	assert!(enabled_features(&plan, "extra").is_empty());
	assert_eq!(prelude(krate(&plan, "real_core", TargetKind::Lib)), ["extra (extra)"]);

	// `dependency/feature` on the command line
	let plan = self::plan(&with(features(packages(virtual_ws(), &["app"]), &["kore/serde"]), |options| options.load_all_members = true));

	assert_eq!(enabled_features(&plan, "app"), ["default"]);
	assert_eq!(enabled_features(&plan, "real-core"), ["alloc", "default", "serde", "std"]);
}

#[test]
fn enables_weak_dependency_features_only_with_the_dependency() {
	let core = with(packages(virtual_ws(), &["real-core"]), |options| options.load_all_members = true);
	let weak = plan(&features(core.clone(), &["shiny"]));

	assert_eq!(enabled_features(&weak, "real-core"), ["default", "shiny", "std"]);
	assert!(prelude(krate(&weak, "real_core", TargetKind::Lib)).is_empty());

	// only loaded (unselected), with its default features
	assert_eq!(enabled_features(&weak, "extra"), ["basic", "default"]);

	let activated = plan(&features(core, &["shiny", "serde"]));

	assert_eq!(prelude(krate(&activated, "real_core", TargetKind::Lib)), ["extra (extra)"]);
	assert_eq!(enabled_features(&activated, "extra"), ["shiny"]);
}

#[test]
fn unknown_features_are_errors() {
	let message = cargo_error(&features(virtual_ws(), &["nope"]));

	assert!(message.contains("nope"), "{message}");

	let message = cargo_error(&features(packages(virtual_ws(), &["real-core"]), &["dep:extra"]));

	assert!(message.contains("not allowed to use explicit `dep:` syntax"), "{message}");
}

/// Removes the versions that dependencies are locked to (` (locked to 1.0.0)`) from an error of cargo's resolvers.
fn without_locked_versions(message: &str) -> String {
	let mut unlocked = String::new();
	let mut rest = message;

	while let Some(start) = rest.find(" (locked to ") {
		unlocked.push_str(&rest[..start]);
		rest = &rest[start..];
		rest = &rest[rest.find(')').map_or(rest.len(), |end| end + 1)..];
	}

	unlocked.push_str(rest);
	unlocked
}

/// The error of the default feature resolution, which must be the exact resolution's but for the versions that
/// dependencies are locked to (which only cargo's resolvers know).
fn resolution_error(options: &LoadOptions) -> String {
	let message = cargo_error(options);
	let exact = cargo_error(&with(options.clone(), |options| options.exact_features = true));

	assert_eq!(message, without_locked_versions(&exact));
	message
}

#[test]
fn features_that_dependencies_do_not_have_are_errors() {
	let app = packages(virtual_ws(), &["app"]);

	assert_eq!(
		resolution_error(&features(app.clone(), &["kore/nope"])),
		format!(
			"failed to select a version for `real-core`.\n    ... required by package `app v0.1.0 ({})`\nversions that meet the \
			requirements `*` are: 0.3.1\n\npackage `app` depends on `real-core` with feature `nope` but `real-core` does not have \
			that feature.\nhelp: available features: alloc, default, full, serde, shiny, std\n\n\nfailed to select a version for \
			`real-core` which could resolve this conflict",
			fixture("ws_virtual/app").display()
		)
	);

	// through `tool`'s dependency on `app`
	let message = resolution_error(&features(virtual_ws(), &["app/nope"]));

	assert!(message.contains("\n    ... required by package `tool v0.2.0 ("), "{message}");
	assert!(
		message.contains(
			"package `tool` depends on `app` with feature `nope` but `app` does not have that feature.\nhelp: available features: cli, \
			default, json\n"
		),
		"{message}"
	);

	// an optional dependency without an implicit feature, and a required dependency
	let message = resolution_error(&features(app, &["kore/extra"]));

	assert!(
		message.contains(
			"note: an optional dependency with that name exists, but that dependency uses the \"dep:\" syntax in the features \
			table, so it does not have an implicit feature with that name.\n"
		),
		"{message}"
	);

	let message = resolution_error(&features(packages(virtual_ws(), &["tool"]), &["app/macros"]));

	assert!(
		message.contains("note: a required dependency with that name exists, but only optional dependencies can be used as features.\n"),
		"{message}"
	);

	// requested of the root package's dependency, by resolver version 1's handling of features on the command line
	let message = resolution_error(&features(options("ws_rooted/Cargo.toml"), &["rooted-member/nope"]));

	assert!(message.contains("\nhelp: there is a feature `loud` with a similar name\n"), "{message}");
}

#[test]
fn requested_features_that_packages_do_not_have_are_errors() {
	let core = fixture("ws_virtual/core");
	let rooted = options("ws_rooted/Cargo.toml");

	// an optional dependency without an implicit feature
	assert_eq!(
		resolution_error(&features(packages(virtual_ws(), &["real-core"]), &["extra"])),
		format!(
			"package `real-core v0.3.1 ({})` does not have feature `extra`\n\nhelp: an optional dependency with that name exists, \
			but the `features` table includes it with the \"dep:\" syntax so it does not have an implicit feature with that \
			name\nDependency `extra` would be enabled by these features:\n\t- `serde`",
			core.display()
		)
	);

	// resolver version 1 passes the features to the root package unchecked
	assert_eq!(
		resolution_error(&features(rooted.clone(), &["nope"])),
		format!(
			"package `rooted v1.0.0 ({})` does not have the feature `nope`\n\nhelp: a feature with a similar name exists: `loud`",
			fixture("ws_rooted").display()
		)
	);

	assert_eq!(
		resolution_error(&features(rooted.clone(), &["rooted-member"])),
		format!(
			"package `rooted v1.0.0 ({})` does not have feature `rooted-member`\n\nhelp: a dependency with that name exists but it \
			is required dependency and only optional dependencies can be used as features.",
			fixture("ws_rooted").display()
		)
	);

	assert_eq!(
		resolution_error(&features(rooted, &["nonexistent/feature"])),
		format!("package `rooted v1.0.0 ({})` does not have a dependency named `nonexistent`", fixture("ws_rooted").display())
	);

	assert_eq!(
		resolution_error(&features(packages(options("ws_rooted/Cargo.toml"), &["rooted-member"]), &["rooted-member/nope"])),
		format!(
			"package `rooted-member v0.1.0 ({})` does not have the feature `nope`\n\nhelp: a feature with a similar name exists: `loud`",
			fixture("ws_rooted/member").display()
		)
	);
}

/// A workspace whose member `a` depends on the member `b`.
fn two_members(name: &str, a_dependency: &str, b_features: &str) -> PathBuf {
	let a = format!("[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{a_dependency}\n");
	let b = format!("[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\n{b_features}\n");

	temp_workspace(name, &[
		("Cargo.toml", "[workspace]\nresolver = \"2\"\nmembers = [\"a\", \"b\"]\n"),
		("a/Cargo.toml", &a),
		("a/src/lib.rs", ""),
		("b/Cargo.toml", &b),
		("b/src/lib.rs", ""),
	])
}

#[test]
fn features_are_checked_in_the_whole_workspace() {
	// like cargo, which resolves every member (with all features) for `Cargo.lock`, even when only `b` is selected
	let typo = two_members("dependency_feature_typo", r#"b = { path = "../b", features = ["typo"] }"#, "real = []");

	for selected in ["a", "b"] {
		assert_eq!(
			resolution_error(&packages(manifest(&typo), &[selected])),
			format!(
				"failed to select a version for `b`.\n    ... required by package `a v0.1.0 ({})`\nversions that meet the \
				requirements `*` are: 0.1.0\n\npackage `a` depends on `b` with feature `typo` but `b` does not have that \
				feature.\nhelp: available features: real\n\n\nfailed to select a version for `b` which could resolve this conflict",
				typo.join("a").display()
			)
		);
	}

	let cyclic = two_members("cyclic_feature", r#"b = { path = "../b" }"#, "cycle = [\"cycle\"]");

	for selected in ["a", "b"] {
		let message = resolution_error(&packages(manifest(&cyclic), &[selected]));

		assert_eq!(message, "cyclic feature dependency: feature `cycle` depends on itself");
	}

	// cargo's dependency resolver also activates the optional dependencies that features refer to weakly
	let weak = temp_workspace("weak_dependency_feature_typo", &[
		("Cargo.toml", "[workspace]\nresolver = \"2\"\nmembers = [\"a\"]\nexclude = [\"outside\", \"b\"]\n"),
		(
			"a/Cargo.toml",
			"[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n\
			outside = { path = \"../outside\", features = [\"shiny\"] }\n",
		),
		("a/src/lib.rs", ""),
		(
			"outside/Cargo.toml",
			"[package]\nname = \"outside\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\nshiny = [\"b?/typo\"]\n\n\
			[dependencies]\nb = { path = \"../b\", optional = true }\n",
		),
		("outside/src/lib.rs", ""),
		("b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\nreal = []\n"),
		("b/src/lib.rs", ""),
	]);

	assert_eq!(
		resolution_error(&manifest(&weak)),
		format!(
			"failed to select a version for `b`.\n    ... required by package `outside v0.1.0 ({})`\n    ... which satisfies path \
			dependency `outside` of package `a v0.1.0 ({})`\nversions that meet the requirements `*` are: 0.1.0\n\npackage \
			`outside` depends on `b` with feature `typo` but `b` does not have that feature.\nhelp: available features: real\n\n\n\
			failed to select a version for `b` which could resolve this conflict",
			weak.join("outside").display(),
			weak.join("a").display()
		)
	);
}

#[test]
fn unifies_dev_dependency_features_when_tests_are_loaded() {
	let app = with(packages(virtual_ws(), &["app"]), |options| options.load_all_members = true);

	// the dev-dependency on `extra` is not in use: `extra` gets its default features
	assert_eq!(enabled_features(&plan(&app), "extra"), ["basic", "default"]);

	let tests = plan(&targets(app, |targets| targets.all_tests = true));

	assert_eq!(enabled_features(&tests, "extra"), ["basic", "default", "shiny"]);
}

#[test]
fn resolver_v1_unifies_dev_dependency_features() {
	let plan = plan(&with(options("ws_rooted/Cargo.toml"), |options| options.workspace = true));

	assert_eq!(enabled_features(&plan, "rooted"), ["default"]);
	assert_eq!(enabled_features(&plan, "rooted-member"), ["default", "testing"]);

	let loud = self::plan(&with(options("ws_rooted/Cargo.toml"), |options| {
		options.features = vec!["loud".to_owned()];
		options.load_all_members = true;
	}));

	assert_eq!(enabled_features(&loud, "rooted"), ["default", "loud"]);
	assert_eq!(enabled_features(&loud, "rooted-member"), ["default", "loud", "testing"]);

	// with resolver version 1, the root package is resolved as if it was built even when it is not selected
	let member = self::plan(&packages(options("ws_rooted/Cargo.toml"), &["rooted-member"]));

	assert_eq!(package_names(&member), ["rooted-member"]);
	assert_eq!(enabled_features(&member, "rooted-member"), ["default", "testing"]);
}

/// `bottom`'s features, and the extern prelude of its library (which has `helper` with the `from-middle-dev` feature).
fn bottom(options: &LoadOptions) -> (Vec<String>, Vec<String>) {
	let plan = plan(options);
	let features = enabled_features(&plan, "bottom").into_iter().map(str::to_owned).collect();

	(features, prelude(krate(&plan, "bottom", TargetKind::Lib)))
}

#[test]
fn resolves_the_dev_dependencies_of_built_packages_only() {
	let all_targets = |options: LoadOptions| targets(options, |targets| targets.all_targets = true);
	let helper = strings(&["helper (helper)"]);

	// `top`'s dev-dependency is resolved, but neither `middle`'s (which is not built) nor `outside`'s (not a member)
	assert_eq!(bottom(&all_targets(packages(devdeps(), &["top", "bottom"]))), (strings(&["from-outside", "from-top-dev"]), vec![]));

	assert_eq!(
		bottom(&all_targets(with(devdeps(), |options| options.workspace = true))),
		(strings(&["from-middle-dev", "from-outside", "from-top-dev"]), helper.clone())
	);

	let lib_and_tests = |targets: &mut TargetSelection| {
		targets.lib = true;
		targets.all_tests = true;
	};

	assert_eq!(bottom(&targets(packages(devdeps(), &["middle", "bottom"]), lib_and_tests)), (strings(&["from-middle-dev"]), helper));

	// no dev-dependency is resolved without examples, tests, or benchmarks
	assert_eq!(bottom(&packages(devdeps(), &["top", "bottom"])), (strings(&["from-outside"]), vec![]));

	// packages that are only loaded get the features the built ones enable on them
	let loaded = plan(&with(all_targets(packages(devdeps(), &["top"])), |options| options.load_all_members = true));

	assert_eq!(enabled_features(&loaded, "bottom"), ["from-outside", "from-top-dev"]);
	assert!(prelude(krate(&loaded, "bottom", TargetKind::Lib)).is_empty());
	assert!(enabled_features(&loaded, "middle").is_empty());
}

#[test]
fn resolver_v1_resolves_the_dev_dependencies_of_built_packages_only() {
	let helper = strings(&["helper (helper)"]);

	// `top`, the root package, is resolved as if it was built even when only `bottom` is selected
	assert_eq!(bottom(&packages(devdeps_v1(), &["bottom"])), (strings(&["from-outside", "from-top-dev"]), vec![]));
	assert_eq!(bottom(&options("ws_devdeps_v1/bottom/Cargo.toml")), (vec![], vec![]));

	assert_eq!(
		bottom(&packages(devdeps_v1(), &["middle", "bottom"])),
		(strings(&["from-middle-dev", "from-outside", "from-top-dev"]), helper.clone())
	);

	assert_eq!(
		bottom(&with(devdeps_v1(), |options| options.workspace = true)),
		(strings(&["from-middle-dev", "from-outside", "from-top-dev"]), helper)
	);

	let loaded = plan(&with(devdeps_v1(), |options| options.load_all_members = true));

	assert_eq!(enabled_features(&loaded, "bottom"), ["from-outside", "from-top-dev"]);
	assert!(enabled_features(&loaded, "middle").is_empty());
}

/// Everything feature resolution decides, per package and per crate.
fn resolution(plan: &WorkspacePlan) -> Vec<String> {
	let packages = plan.packages.iter().map(|package| format!("{}: {:?}", package.name, package.enabled_features));
	let crates = plan.crates.iter().map(|spec| format!("{} {}: {:?} {:?}", spec.name, spec.kind, spec.cfg.features(), prelude(spec)));

	packages.chain(crates).collect()
}

/// Plans with the default and the exact feature resolution, which must agree. Returns both durations.
fn assert_resolutions_agree(options: &LoadOptions) -> (Duration, Duration) {
	let start = Instant::now();
	let cheap = plan(options);
	let cheap_time = start.elapsed();
	let start = Instant::now();
	let exact = plan(&with(options.clone(), |options| options.exact_features = true));
	let exact_time = start.elapsed();

	assert_eq!(resolution(&cheap), resolution(&exact), "{options:#?}");
	(cheap_time, exact_time)
}

#[test]
fn feature_resolutions_agree() {
	let workspace = with(virtual_ws(), |options| options.workspace = true);
	let all_targets = |options: LoadOptions| targets(options, |targets| targets.all_targets = true);

	let cases = [
		virtual_ws(),
		workspace.clone(),
		all_targets(workspace.clone()),
		features(workspace.clone(), &["app/json"]),
		with(workspace.clone(), |options| options.all_features = true),
		with(workspace.clone(), |options| options.no_default_features = true),
		with(all_targets(workspace.clone()), |options| options.target = Some("x86_64-pc-windows-msvc".to_owned())),
		features(packages(virtual_ws(), &["real-core"]), &["shiny"]),
		features(packages(virtual_ws(), &["real-core"]), &["shiny,serde"]),
		features(packages(virtual_ws(), &["app"]), &["json", "cli"]),
		all_targets(packages(virtual_ws(), &["app"])),
		packages(virtual_ws(), &["macros"]),
		options("ws_rooted/Cargo.toml"),
		with(options("ws_rooted/Cargo.toml"), |options| options.workspace = true),
		features(options("ws_rooted/member/Cargo.toml"), &["loud"]),
		packages(options("ws_rooted/Cargo.toml"), &["rooted-member"]),
		// `app` is not built, so its dev-dependency on `extra` is not resolved (but `weird-name` enables `shiny` too)
		all_targets(packages(virtual_ws(), &["tool", "extra"])),
		with(packages(virtual_ws(), &["tool"]), |options| options.load_all_members = true),
		with(all_targets(packages(virtual_ws(), &["app"])), |options| options.load_all_members = true),
	];

	for options in &cases {
		assert_resolutions_agree(options);
	}
}

#[test]
fn feature_resolutions_of_dev_dependencies_agree() {
	let all_targets = |options: LoadOptions| targets(options, |targets| targets.all_targets = true);
	let load_all = |options: LoadOptions| with(options, |options| options.load_all_members = true);

	let cases = [
		all_targets(packages(devdeps(), &["top", "bottom"])),
		all_targets(with(devdeps(), |options| options.workspace = true)),
		targets(packages(devdeps(), &["middle", "bottom"]), |targets| targets.all_tests = true),
		packages(devdeps(), &["top", "bottom"]),
		load_all(all_targets(packages(devdeps(), &["top"]))),
		load_all(packages(devdeps(), &["middle"])),
		features(packages(devdeps(), &["top"]), &["outside/extra"]),
		devdeps_v1(),
		packages(devdeps_v1(), &["bottom"]),
		packages(devdeps_v1(), &["middle", "bottom"]),
		with(devdeps_v1(), |options| options.workspace = true),
		options("ws_devdeps_v1/bottom/Cargo.toml"),
		load_all(devdeps_v1()),
		load_all(all_targets(options("ws_devdeps_v1/middle/Cargo.toml"))),
		features(devdeps_v1(), &["outside/extra"]),
	];

	for options in &cases {
		assert_resolutions_agree(options);
	}
}

// ---- cfg contexts

#[test]
fn evaluates_cfgs_per_crate() {
	let plan = plan(&targets(with(virtual_ws(), |options| options.workspace = true), |targets| targets.all_targets = true));
	let app = krate(&plan, "app", TargetKind::Lib);

	assert_eq!(eval(app, "unix"), host_is(cfg!(unix)));
	assert_eq!(eval(app, "windows"), host_is(cfg!(windows)));
	assert_eq!(eval(app, r#"target_pointer_width = "64""#), host_is(cfg!(target_pointer_width = "64")));
	assert_eq!(eval(app, "debug_assertions"), Tristate::True);
	assert_eq!(eval(app, "test"), Tristate::False);
	assert_eq!(eval(app, "proc_macro"), Tristate::False);
	assert_eq!(eval(app, "doc"), Tristate::False);
	assert_eq!(eval(app, r#"feature = "default""#), Tristate::True);
	assert_eq!(eval(app, r#"feature = "cli""#), Tristate::False);
	assert_eq!(eval(app, "set_by_a_build_script"), Tristate::Unknown);

	for (name, kind, test) in [
		("app", TargetKind::Bin, Tristate::False),
		("demo", TargetKind::Example, Tristate::False),
		("integration", TargetKind::Test, Tristate::True),
		("speed", TargetKind::Bench, Tristate::True),
		("expand", TargetKind::Test, Tristate::True),
	] {
		let spec = krate(&plan, name, kind);

		assert_eq!(eval(spec, "test"), test, "{name}");
		assert_eq!(eval(spec, "proc_macro"), Tristate::False, "{name}");
	}

	let macros = krate(&plan, "macros", TargetKind::ProcMacro);

	assert_eq!(eval(macros, "proc_macro"), Tristate::True);
	assert_eq!(eval(macros, "test"), Tristate::False);
	assert_eq!(eval(macros, r#"feature = "pmf""#), Tristate::True);
}

#[test]
fn enables_extra_cfgs() {
	let plan = plan(&with(packages(virtual_ws(), &["real-core"]), |options| {
		options.cfgs = vec!["my_cfg".to_owned(), r#"feature="extra_feature""#.to_owned(), r#"key = "value""#.to_owned(), "test".to_owned()];
	}));

	let core = krate(&plan, "real_core", TargetKind::Lib);

	assert_eq!(eval(core, "my_cfg"), Tristate::True);
	assert_eq!(eval(core, "test"), Tristate::True);
	assert_eq!(eval(core, r#"feature = "extra_feature""#), Tristate::True);
	assert_eq!(eval(core, r#"feature = "std""#), Tristate::True);
	assert_eq!(eval(core, r#"key = "value""#), Tristate::True);
	assert_eq!(eval(core, r#"key = "other""#), Tristate::False);
	assert_eq!(eval(core, "other_cfg"), Tristate::Unknown);
}

#[test]
fn honors_rustflags_from_cargo_configuration() {
	for exact_features in [false, true] {
		let plan = plan(&with(packages(virtual_ws(), &["real-core"]), |options| {
			options.config = vec![r#"build.rustflags = ["--cfg", "from_config", "--cfg", 'flag="on"']"#.to_owned()];
			options.exact_features = exact_features;
		}));

		let core = krate(&plan, "real_core", TargetKind::Lib);

		assert_eq!(eval(core, "from_config"), Tristate::True);
		assert_eq!(eval(core, r#"flag = "on""#), Tristate::True);
		assert_eq!(eval(core, r#"flag = "off""#), Tristate::False);
	}

	let message = cargo_error(&with(virtual_ws(), |options| options.config = vec!["not valid toml [".to_owned()]));

	assert!(message.contains("--config"), "{message}");
}

#[test]
fn invalid_cfgs_are_errors() {
	for spec in ["all(", "not(unix)", r#"feature = 1"#, ""] {
		let options = with(virtual_ws(), |options| options.cfgs = vec![spec.to_owned()]);

		assert!(matches!(error(&options), Error::CfgParse(_)), "{spec}");
	}
}

#[test]
fn evaluates_cfgs_for_a_target() {
	let plan = plan(&with(virtual_ws(), |options| {
		options.workspace = true;
		options.target = Some("x86_64-pc-windows-msvc".to_owned());
	}));

	let app = krate(&plan, "app", TargetKind::Lib);

	assert_eq!(eval(app, "windows"), Tristate::True);
	assert_eq!(eval(app, "unix"), Tristate::False);
	assert_eq!(eval(app, r#"target_env = "msvc""#), Tristate::True);

	// proc-macros are compiled for the host
	let macros = krate(&plan, "macros", TargetKind::ProcMacro);

	assert_eq!(eval(macros, "unix"), host_is(cfg!(unix)));
	assert_eq!(eval(macros, r#"target_env = "msvc""#), host_is(cfg!(target_env = "msvc")));

	// `tool` has a Windows-only dependency on `extra`, with its `windows` feature
	assert_eq!(prelude(krate(&plan, "tool", TargetKind::Bin)), ["app (app)", "extra (extra)", "oddly_named (weird-name)"]);
	assert_eq!(enabled_features(&plan, "extra"), ["basic", "default", "shiny", "windows"]);
}

#[test]
fn unknown_targets_are_errors() {
	let error = error(&with(virtual_ws(), |options| options.target = Some("not-a-target-triple".to_owned())));

	assert!(matches!(&error, Error::Rustc(message) if message.contains("not-a-target-triple")), "{error:?}");
}

// ---- dependencies

#[test]
fn builds_extern_preludes() {
	let plan = plan(&targets(with(virtual_ws(), |options| options.workspace = true), |targets| targets.all_targets = true));
	let crate_prelude = |name: &str, kind: TargetKind| prelude(krate(&plan, name, kind));

	// `kore` is `real-core`, renamed; binaries, examples, tests, and benches also see their package's library
	assert_eq!(crate_prelude("app", TargetKind::Lib), ["kore=real_core (real-core)", "macros (macros)"]);
	assert_eq!(crate_prelude("app", TargetKind::Bin), ["app (app)", "kore=real_core (real-core)", "macros (macros)"]);
	assert_eq!(crate_prelude("other", TargetKind::Bin), ["app (app)", "kore=real_core (real-core)", "macros (macros)"]);

	// dev-dependencies are only available to examples, tests, and benchmarks
	for (name, kind) in [("demo", TargetKind::Example), ("integration", TargetKind::Test), ("speed", TargetKind::Bench)] {
		assert_eq!(crate_prelude(name, kind), ["app (app)", "extra (extra)", "kore=real_core (real-core)", "macros (macros)"], "{name}");
	}

	assert_eq!(crate_prelude("macros", TargetKind::ProcMacro), Vec::<String>::new());
	assert_eq!(crate_prelude("expand", TargetKind::Test), ["macros (macros)"]);
	assert_eq!(crate_prelude("real_core", TargetKind::Lib), Vec::<String>::new());

	// `weird-name` is not a member, and its library is `oddly_named`
	let tool = if cfg!(windows) {
		vec!["app (app)", "extra (extra)", "oddly_named (weird-name)"]
	} else {
		vec!["app (app)", "oddly_named (weird-name)"]
	};

	assert_eq!(crate_prelude("tool", TargetKind::Bin), tool);

	for dependency in &krate(&plan, "app", TargetKind::Lib).dependencies {
		assert_eq!(dependency.krate, None);
	}
}

#[test]
fn resolves_external_dependency_names() {
	let plan = plan(&with(LoadOptions::default(), |options| {
		options.manifest_path = Some(repository());
		options.packages = vec!["rscode".to_owned()];
		options.silent = true;
	}));

	let rscode = prelude(krate(&plan, "rscode", TargetKind::Lib));

	for expected in ["proc_macro2 (proc-macro2)", "rscode_fmt (rscode_fmt)", "syn (syn)", "cargo (cargo)"] {
		assert!(rscode.contains(&expected.to_owned()), "{expected} not in {rscode:?}");
	}

	// optional dependencies of features that are not enabled
	assert!(!rscode.iter().any(|dependency| dependency.starts_with("rmcp")), "{rscode:?}");
}

#[test]
fn resolves_registry_dependencies_offline() {
	let exact = with(vendored(), |options| options.exact_features = true);
	let crate_prelude = |options: &LoadOptions| prelude(krate(&plan(options), "vendored", TargetKind::Lib));

	// the library of `fancy-thing` is `fancy`, which only cargo's resolvers find out
	assert_eq!(crate_prelude(&exact), ["fancy (fancy-thing)"]);
	assert_eq!(crate_prelude(&vendored()), ["fancy_thing (fancy-thing)"]);
}

// ---- required features

#[test]
fn required_features_of_dependencies() {
	let top = packages(devdeps(), &["top"]);

	for exact_features in [false, true] {
		let exact = |options: LoadOptions| with(options, |options| options.exact_features = exact_features);

		// `needs-extra` requires `outside/extra`, a feature of a path dependency that is not a member
		assert_eq!(crates(&plan(&exact(top.clone()))), ["top lib"]);
		assert_eq!(crates(&plan(&exact(features(top.clone(), &["outside/extra"])))), ["top lib", "needs_extra bin"]);
		assert_eq!(crates(&plan(&exact(features(devdeps_v1(), &["outside/extra"])))), ["top lib", "needs_extra bin"]);
	}

	// `needs-derive` requires `fancy-thing/derive`, a feature of a registry dependency, which only cargo's resolvers know
	let exact = with(vendored(), |options| options.exact_features = true);

	assert_eq!(crates(&plan(&exact)), ["vendored lib"]);
	assert_eq!(crates(&plan(&features(exact, &["fancy-thing/derive"]))), ["vendored lib", "needs_derive bin"]);
	assert_eq!(crates(&plan(&vendored())), ["vendored lib", "needs_derive bin"]);
}

/// A package with a binary that requires `required`.
fn requiring(name: &str, required: &str) -> PathBuf {
	let manifest = format!(
		"[package]\nname = \"requiring\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[features]\nfeature = []\n\n\
		[dependencies]\nlocal = {{ path = \"local\", optional = true }}\n\n[[bin]]\nname = \"requiring\"\npath = \"main.rs\"\n\
		required-features = [{required}]\n"
	);

	temp_workspace(name, &[
		("Cargo.toml", &manifest),
		("main.rs", "fn main() {}\n"),
		("local/Cargo.toml", "[package]\nname = \"local\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
		("local/src/lib.rs", ""),
	])
}

#[test]
fn invalid_required_features_are_errors() {
	let dep = requiring("required_dep", r#""dep:local""#);
	let weak = requiring("required_weak", r#""local?/feature""#);

	for exact_features in [false, true] {
		let options = |workspace: &Path| with(manifest(workspace), |options| options.exact_features = exact_features);

		assert_eq!(
			cargo_error(&options(&dep)),
			"invalid feature `dep:local` in required-features of target `requiring`: `dep:` prefixed feature values are not allowed \
			in required-features"
		);

		assert_eq!(
			cargo_error(&options(&weak)),
			"invalid feature `local?/feature` in required-features of target `requiring`: optional dependency with `?` is not \
			allowed in required-features"
		);
	}
}

// ---- lockfiles

#[test]
fn locked_resolution_requires_an_up_to_date_lockfile() {
	let missing = temp_copy("ws_vendored", "lockfile_missing");
	let outdated = temp_copy("ws_vendored", "lockfile_outdated");
	let stale = "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n\
		[[package]]\nname = \"vendored\"\nversion = \"0.1.0\"\n";

	std::fs::remove_file(missing.join("Cargo.lock")).unwrap();
	std::fs::write(outdated.join("Cargo.lock"), stale).unwrap();

	for (flag, lock) in [("--locked", (true, false)), ("--frozen", (false, true))] {
		let locked = |workspace: &Path| {
			with(manifest(workspace), |options| {
				options.config = vendored_config(workspace);
				options.exact_features = true;
				(options.locked, options.frozen) = lock;
			})
		};

		// the fixture's lockfile is up to date
		assert_eq!(crates(&plan(&locked(&fixture("ws_vendored")))), ["vendored lib"]);

		let error = |action: &str, lockfile: &Path| {
			format!(
				"cannot {action} the lock file {} because {flag} was passed to prevent this\nhelp: to generate the lock file without \
				accessing the network, remove the {flag} flag and use --offline instead.",
				lockfile.display()
			)
		};

		let lockfile = missing.join("Cargo.lock");

		assert_eq!(cargo_error(&locked(&missing)), error("create", &lockfile));
		assert!(!lockfile.exists());

		let lockfile = outdated.join("Cargo.lock");

		assert_eq!(cargo_error(&locked(&outdated)), error("update", &lockfile));
		assert_eq!(std::fs::read_to_string(&lockfile).unwrap(), stale);

		// the default resolution resolves no versions
		assert_eq!(crates(&plan(&with(locked(&missing), |options| options.exact_features = false))), ["vendored lib", "needs_derive bin"]);
	}
}

// ---- this repository

/// Plans this repository with both feature resolutions, printing how long they took.
#[test]
fn plans_this_repository() {
	let repository = LoadOptions {
		manifest_path: Some(repository()),
		silent: true,
		..LoadOptions::default()
	};

	let default = plan(&repository);

	assert_eq!(package_names(&default), ["cargo-rscode"]);
	assert_eq!(crates(&default), ["cargo_rscode bin"]);
	assert_eq!(enabled_features(&default, "cargo-rscode"), ["default", "mcp"]);

	let workspace = with(repository.clone(), |options| options.workspace = true);
	let all = plan(&workspace);

	assert_eq!(package_names(&all), ["cargo-rscode", "rscode", "rscode_fmt", "rscode_sort"]);
	assert_eq!(enabled_features(&all, "rscode"), ["cargo", "clap", "default", "mcp"]);
	assert_eq!(enabled_features(&all, "rscode_fmt"), ["clap", "default"]);
	assert_eq!(enabled_features(&all, "rscode_sort"), ["clap", "default"]);

	let cases = [
		("default", repository.clone()),
		("--workspace", workspace.clone()),
		("--workspace --all-targets", targets(workspace.clone(), |targets| targets.all_targets = true)),
		("-p rscode --no-default-features", with(packages(repository.clone(), &["rscode"]), |options| options.no_default_features = true)),
		("-p 'rscode*'", packages(repository.clone(), &["rscode*"])),
		(
			"-p rscode_sort -p rscode_fmt -F rscode_sort/clap",
			features(packages(repository.clone(), &["rscode_sort", "rscode_fmt"]), &["rscode_sort/clap"]),
		),
		("--workspace --exclude cargo-rscode -F rscode/mcp", with(features(workspace, &["rscode/mcp"]), |options| {
			options.exclude = vec!["cargo-rscode".to_owned()];
		})),
	];

	for (flags, options) in cases {
		let (cheap, exact) = assert_resolutions_agree(&options);

		eprintln!("planned this repository with `{flags}`: {cheap:?} (default), {exact:?} (exact features)");
	}

	// warm
	let start = Instant::now();
	let runs = 5;

	for _ in 0..runs {
		plan(&repository);
	}

	eprintln!("planned this repository in {:?} on average", start.elapsed() / runs);
}

// ---- side effects

/// Every file below a directory, with its size and modification time.
fn snapshot(directory: &Path) -> BTreeMap<PathBuf, (u64, SystemTime)> {
	let mut files = BTreeMap::new();
	let mut pending = vec![directory.to_path_buf()];

	while let Some(directory) = pending.pop() {
		for entry in std::fs::read_dir(&directory).unwrap() {
			let entry = entry.unwrap();
			let metadata = entry.metadata().unwrap();

			if metadata.is_dir() {
				pending.push(entry.path());
			}

			files.insert(entry.path(), (metadata.len(), metadata.modified().unwrap()));
		}
	}

	files
}

#[test]
fn planning_modifies_no_files() {
	let fixtures = fixture("");
	let before = snapshot(&fixtures);

	let cases = [
		virtual_ws(),
		options("ws_rooted/Cargo.toml"),
		options("ws_rooted/member/Cargo.toml"),
		devdeps(),
		devdeps_v1(),
		options("ws_devdeps_v1/bottom/Cargo.toml"),
		vendored(),
		with(vendored(), |options| options.locked = true),
	];

	for case in cases {
		for exact_features in [false, true] {
			plan(&with(case.clone(), |options| {
				options.workspace = true;
				options.exact_features = exact_features;
				options.targets.all_targets = true;
				options.load_all_members = true;
			}));
		}
	}

	assert_eq!(before, snapshot(&fixtures));

	for workspace in ["ws_virtual", "ws_rooted", "ws_devdeps", "ws_devdeps_v1", "ws_vendored"] {
		assert!(!fixture(workspace).join("target").exists());
	}

	// only `ws_vendored` has a lockfile (and it is up to date)
	for workspace in ["ws_virtual", "ws_rooted", "ws_devdeps", "ws_devdeps_v1"] {
		assert!(!fixture(workspace).join("Cargo.lock").exists());
	}
}

/// Set for the child processes of the tests that check what planning writes to stdout and stderr.
const CHILD: &str = "RSCODE_WORKSPACE_TEST_CHILD";

/// Whether this is the child process of [`run_in_child`].
fn in_child() -> bool {
	std::env::var_os(CHILD).is_some()
}

/// Runs a test in a child process (in which [`in_child`] is true) and returns its stderr, after checking that the test
/// passed and that its stdout only has libtest's own lines.
fn run_in_child(name: &str) -> String {
	let output = std::process::Command::new(std::env::current_exe().unwrap())
		.args([name, "--exact", "--nocapture", "--test-threads", "1"])
		.env(CHILD, "1")
		.output()
		.unwrap();

	let stdout = String::from_utf8(output.stdout).unwrap();
	let stderr = String::from_utf8(output.stderr).unwrap();
	let test_line = format!("test {name} ... ok");
	let libtest = |line: &str| {
		line.is_empty() || line == "running 1 test" || line == test_line || line.starts_with("test result: ok. 1 passed")
	};

	let unexpected: Vec<&str> = stdout.lines().filter(|line| !libtest(line)).collect();

	assert!(output.status.success(), "{stdout}\n{stderr}");
	assert!(unexpected.is_empty(), "unexpected output on stdout: {unexpected:?}");
	stderr
}

/// Plans (with cargo's messages enabled) in a child process, whose stdout must only have libtest's own lines: the MCP
/// server uses stdout for the protocol.
#[test]
fn writes_nothing_to_stdout() {
	if in_child() {
		for exact_features in [false, true] {
			plan_workspace(&with(virtual_ws(), |options| {
				options.workspace = true;
				options.exclude = vec!["nope".to_owned()];
				options.targets.all_targets = true;
				options.load_all_members = true;
				options.exact_features = exact_features;
				options.silent = false;
			}))
			.unwrap();
		}

		return;
	}

	let stderr = run_in_child("writes_nothing_to_stdout");

	assert_eq!(stderr.matches("warning: excluded package(s) `nope` not found in workspace").count(), 2, "{stderr}");
}

/// Warns like cargo about `required-features` that do not exist (in a child process, to see stderr).
#[test]
fn warns_about_required_features_that_do_not_exist() {
	let workspace = requiring("required_missing", r#""nope", "local/nope", "missing/feature""#);

	if in_child() {
		for exact_features in [false, true] {
			let plan = plan_workspace(&with(manifest(&workspace), |options| {
				options.exact_features = exact_features;
				options.silent = false;
			}))
			.unwrap();

			// the binary's required features are not enabled
			assert!(plan.crates.is_empty(), "{:?}", crates(&plan));
		}

		return;
	}

	let stderr = run_in_child("warns_about_required_features_that_do_not_exist");
	let invalid = "warning: invalid feature `{}` in required-features of target `requiring`: ";
	let warnings = [
		("nope", "`nope` is not present in [features] section".to_owned()),
		("local/nope", format!("feature `nope` does not exist in package `local v0.1.0 ({})`", workspace.join("local").display())),
		("missing/feature", "dependency `missing` does not exist".to_owned()),
	];

	for (feature, warning) in warnings {
		let line = format!("{}{warning}\n", invalid.replace("{}", feature));

		assert_eq!(stderr.matches(&line).count(), 2, "{line} in {stderr}");
	}
}

// ---- loading (needs the crate loader)

#[test]
fn loads_and_links_the_fixture_workspace() {
	let options = targets(with(virtual_ws(), |options| options.workspace = true), |targets| targets.all_targets = true);
	let workspace = load_workspace(&options).unwrap();
	let find = |name: &str, kind: TargetKind| {
		workspace
			.crates()
			.iter()
			.find(|krate| krate.name() == name && krate.kind() == kind)
			.unwrap_or_else(|| panic!("no {kind} crate `{name}`"))
	};

	assert_eq!(workspace.root(), fixture("ws_virtual"));
	assert_eq!(workspace.crates().len(), 11);
	assert_eq!(workspace.packages().len(), 5);

	for krate in workspace.crates() {
		assert!(krate.diagnostics().is_empty(), "{}: {:?}", krate.name(), krate.diagnostics());
		assert!(krate.is_selected());
	}

	let app = find("app", TargetKind::Lib);
	let core = find("real_core", TargetKind::Lib);
	let macros = find("macros", TargetKind::ProcMacro);
	let binary = find("app", TargetKind::Bin);
	let linked = |krate: &rscode::Crate, name: &str| {
		let dependency = krate.dependencies().iter().find(|dependency| dependency.name == name)?;

		dependency.krate
	};

	assert_eq!(linked(app, "kore"), Some(core.id()));
	assert_eq!(linked(app, "macros"), Some(macros.id()));
	assert_eq!(linked(binary, "app"), Some(app.id()));
	assert_eq!(linked(binary, "kore"), Some(core.id()));
	assert_eq!(linked(find("demo", TargetKind::Example), "extra"), Some(find("extra", TargetKind::Lib).id()));
	assert_eq!(workspace.package(app.package().unwrap()).name, "app");
}

#[test]
fn loads_this_repository() {
	let start = Instant::now();
	let workspace = load_workspace(&LoadOptions {
		manifest_path: Some(repository()),
		workspace: true,
		load_all_members: true,
		silent: true,
		..LoadOptions::default()
	})
	.unwrap();

	eprintln!("loaded this repository ({} crates) in {:?}", workspace.crates().len(), start.elapsed());

	assert_eq!(workspace.selected_crates().count(), 4);

	for krate in workspace.crates() {
		let is_error = |diagnostic: &&rscode::model::Diagnostic| diagnostic.severity == rscode::model::Severity::Error;
		let errors: Vec<_> = krate.diagnostics().iter().filter(is_error).collect();

		assert!(errors.is_empty(), "{}: {errors:?}", krate.name());
	}
}
