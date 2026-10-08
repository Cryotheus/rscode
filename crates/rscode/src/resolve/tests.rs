//! Resolver tests on models built from source by [`super::test_model`].

use super::test_model::TestCrate;
use super::test_model::workspace;
use super::*;
use crate::model::ItemKind;
use crate::model::PathSegmentRef;
use crate::model::TargetKind;
use crate::path::Anchor;
use crate::path::CanonicalPath;
use crate::path::Qualifier;
use crate::path::Selector;
use crate::source::TextRange;
use crate::test_registry::locked_version;
use crate::test_registry::registry_crate;
use rscode_fmt::Edition;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

/// The `impl` block or macro invocation if its canonical path, as displayed, does not name it alone (items of a file
/// that several crates load count once), unless its type is not a loaded item, its trait cannot be written in a user
/// path, or it is the invocation of a macro that is not understood. Blocks whose headers name other blocks too, and
/// invocations of a macro that a module invokes several times, must have a selector.
fn alone_round_trip_failure(ws: &Workspace, resolver: &Resolver<'_>, item: ItemId) -> Option<String> {
	let path = resolver.canonical_path(item);

	if path.unresolved_self_ty.is_some() || (ws.item(item).kind == ItemKind::MacroCall && !path.is_macro_call) {
		return None;
	}

	let parsed = ItemPath::parse(&path.to_string()).ok()?;
	let place = |item: ItemId| (ws.file_of(item).path().to_path_buf(), ws.item(item).range);
	let named = resolver.resolve_item_path(&parsed);

	let alone = named.contains(&item) && named.iter().all(|&named| place(named) == place(item));

	(!alone).then(|| format!("{path} at {:?} (names {})", place(item), named.len()))
}

fn assert_send_sync<T: Send + Sync>() {}

// The tests below mirror `tests/resolve.rs` (which needs the loader), assertion by assertion, on the same fixtures.

fn basic_fixture() -> Workspace {
	workspace([TestCrate::on_disk("resolve_basic", fixture("resolve_basic/src/lib.rs"))])
}

/// Named definitions and imports whose canonical path (as `::crate::...`, or `use ::crate::...`) does not resolve back
/// to them, and `impl` blocks and macro invocations whose canonical path (as displayed) does not name them alone.
fn canonical_path_round_trip_failures(ws: &Workspace, resolver: &Resolver<'_>) -> Vec<String> {
	let mut failures = Vec::new();

	for krate in ws.crates() {
		for (item, data) in krate.items() {
			if matches!(data.kind, ItemKind::Impl | ItemKind::MacroCall) {
				failures.extend(alone_round_trip_failure(ws, resolver, item));
				continue;
			}

			// `extern crate`s resolve to what they import
			let named = data.name.is_some() && data.kind.is_nameable() && data.kind != ItemKind::ExternCrate;

			if !named && data.kind != ItemKind::Import {
				continue;
			}

			let path = resolver.canonical_path(item);

			if path.unresolved_self_ty.is_some() || shadowed_by_variant(ws, resolver, item) {
				continue;
			}

			let mut segments = path.segments;

			// (fields follow their struct, union, or variant with a `.`)
			let field = path.name.clone().filter(|_| path.is_field);

			segments.extend(path.name.filter(|_| !path.is_field));

			let item_path = ItemPath {
				anchor: Anchor::Global,
				qualifier: None,
				segments,
				arguments: None,
				import: path.is_import,
				field,
				selector: None,
				macro_call: false,
			};

			// the path names the item for edits too: no private import of the same name hides it
			let mut items = resolver.resolve_item_path(&item_path);
			let edits = crate::edit::check_private_imports(resolver, &item_path, &mut items);

			if edits.is_err() || !items.contains(&item) {
				failures.push(show(resolver, item));
			}
		}
	}

	failures
}

#[test]
fn canonical_paths_of_every_kind() {
	let ws = single(
		r#"
		pub mod m {
			pub struct S;
			pub enum E { V }
			pub trait Tr { fn required(); }
			impl S { pub fn new() {} }
			impl Tr for S { fn required() {} }
			impl Tr for Vec<u8> { fn required() {} }
			extern "C" { fn foreign(); }
			use crate::m::S as Alias;
			use std::fmt::*;
			use std::io as _;
			extern crate core;
			const _: () = ();
		}
		"#,
	);

	let resolver = Resolver::new(&ws);
	let m = find(&ws, "t::m");
	let imports: Vec<String> = (ws.children(m))
		.filter(|&child| ws.item(child).kind == ItemKind::Use)
		.flat_map(|use_item| ws.children(use_item))
		.map(|import| show(&resolver, import))
		.collect();

	assert_eq!(show(&resolver, root(&ws, "t")), "t");
	assert_eq!(show(&resolver, m), "t::m");
	assert_eq!(show(&resolver, find(&ws, "t::m::S")), "t::m::S");
	assert_eq!(show(&resolver, find(&ws, "t::m::E::V")), "t::m::E::V");
	assert_eq!(show(&resolver, find(&ws, "t::m::Tr::required")), "t::m::Tr::required");
	assert_eq!(show(&resolver, find_impl(&ws, "t::m", "impl S")), "impl t::m::S");
	assert_eq!(show(&resolver, find_in_impl(&ws, "t::m", "impl S", "new")), "t::m::S::new");
	assert_eq!(show(&resolver, find_impl(&ws, "t::m", "impl Tr for S")), "impl Tr for t::m::S");
	assert_eq!(
		show(&resolver, find_in_impl(&ws, "t::m", "impl Tr for S", "required")),
		"<t::m::S as Tr>::required"
	);
	assert_eq!(
		show(&resolver, find_impl(&ws, "t::m", "impl Tr for Vec<u8>")),
		"t::m::<impl Tr for Vec<u8>>"
	);
	assert_eq!(
		show(&resolver, find_in_impl(&ws, "t::m", "impl Tr for Vec<u8>", "required")),
		"t::m::<impl Tr for Vec<u8>>::required"
	);
	assert_eq!(show(&resolver, find(&ws, "t::m::foreign")), "t::m::foreign");
	assert_eq!(show(&resolver, find(&ws, "t::m::core")), "t::m::core");
	assert_eq!(imports, ["t::m::Alias", "t::m::*", "t::m::_"]);

	let path = resolver.canonical_path(find_in_impl(&ws, "t::m", "impl Tr for Vec<u8>", "required"));

	assert_eq!(path.segments, ["t", "m"]);
	assert_eq!(path.unresolved_self_ty.as_deref(), Some("Vec<u8>"));
	assert_eq!(path.impl_trait.as_deref(), Some("Tr"));
	assert_eq!(path.name.as_deref(), Some("required"));
	assert!(!path.is_impl);
}

#[test]
fn cfg_variants_are_all_kept() {
	let ws = single(
		r#"
		#[cfg(unix)]
		pub struct Foo;

		#[cfg(windows)]
		pub struct Foo(u8);

		#[cfg(unix)]
		mod imp { pub fn f() {} }

		#[cfg(not(unix))]
		mod imp { pub fn f() {} }

		pub use imp::f;
		"#,
	);

	let resolver = Resolver::new(&ws);
	let foos = find_all(&ws, "t::Foo");
	let fs: Vec<ItemId> = find_all(&ws, "t::imp").iter().map(|&imp| ws.children(imp).next().unwrap()).collect();

	assert_eq!(foos.len(), 2);
	assert_eq!(resolver.resolve_item_path(&item_path("crate::Foo")), foos);
	assert_eq!(resolver.bindings(root(&ws, "t"), "Foo", Namespace::Type).len(), 2);
	assert_eq!(resolver.resolve_item_path(&item_path("crate::f")), fs);
	assert_eq!(resolver.resolve_item_path(&item_path("crate::imp::f")), fs);
}

#[test]
fn code_paths_with_keywords_and_preludes() {
	let ws = single(
		r#"
		pub mod a {
			pub mod b { pub fn f() {} }
			fn g() {}
		}

		mod shadow {
			pub struct Option;
			pub fn drop() {}
		}
		"#,
	);

	let resolver = Resolver::new(&ws);
	let b = find(&ws, "t::a::b");
	let shadow = find(&ws, "t::shadow");

	assert_eq!(resolve_in(&resolver, b, "super::g", Namespace::Value), ["t::a::g"]);
	assert_eq!(resolve_in(&resolver, b, "crate::a::b::f", Namespace::Value), ["t::a::b::f"]);
	assert_eq!(resolve_in(&resolver, b, "self::f", Namespace::Value), ["t::a::b::f"]);
	assert_eq!(resolve_in(&resolver, b, "super::super::a", Namespace::Type), ["t::a"]);
	assert!(resolve_in(&resolver, b, "super::super::super::a", Namespace::Type).is_empty());
	assert!(resolve_in(&resolver, b, "Self::f", Namespace::Value).is_empty());
	assert!(resolve_in(&resolver, b, "missing", Namespace::Value).is_empty());
	assert_eq!(resolve_in(&resolver, b, "self", Namespace::Type), ["t::a::b"]);
	assert!(
		resolve_in(&resolver, b, "self", Namespace::Value).is_empty(),
		"`self` alone is a module, or a local variable"
	);
	assert!(resolve_in(&resolver, b, "super", Namespace::Macro).is_empty());

	assert_eq!(resolve_in(&resolver, b, "Option", Namespace::Type), ["ext Option"]);
	assert_eq!(resolve_in(&resolver, b, "Some", Namespace::Value), ["ext Some"]);
	assert_eq!(resolve_in(&resolver, b, "Vec::new", Namespace::Value), ["ext Vec::new"]);
	assert_eq!(resolve_in(&resolver, b, "u8", Namespace::Type), ["builtin u8"]);
	assert_eq!(resolve_in(&resolver, b, "u8::MAX", Namespace::Value), ["ext u8::MAX"]);
	assert_eq!(resolve_in(&resolver, b, "println", Namespace::Macro), ["ext println"]);
	assert_eq!(resolve_in(&resolver, b, "concat", Namespace::Macro), ["builtin concat"]);
	assert_eq!(resolve_in(&resolver, b, "std::mem::swap", Namespace::Value), ["ext std::mem::swap"]);
	assert_eq!(resolve_in(&resolver, b, "::core::mem", Namespace::Type), ["ext core::mem"]);
	assert_eq!(resolve_in(&resolver, shadow, "Option", Namespace::Type), ["t::shadow::Option"]);
	assert_eq!(resolve_in(&resolver, shadow, "drop", Namespace::Value), ["t::shadow::drop"]);

	let prefixes: Vec<Vec<String>> = resolver
		.resolve_prefixes(b, &path_ref("crate::a::b::f"), Some(Namespace::Value), PathKind::Code)
		.iter()
		.map(|res| show_res(&resolver, res))
		.collect();

	assert_eq!(prefixes, [vec!["t"], vec!["t::a"], vec!["t::a::b"], vec!["t::a::b::f"]]);

	let prefixes = resolver.resolve_prefixes(b, &path_ref("nothing::here"), None, PathKind::Code);

	assert_eq!(prefixes, [Vec::new(), Vec::new()]);
}

#[test]
fn definitions_are_bound_in_their_namespaces() {
	let ws = single(
		r#"
		pub struct Named { x: u8 }
		pub struct Tuple(u8);
		pub struct Unit;
		pub enum E { A, B(u8), C { x: u8 } }
		pub union U { a: u8 }
		pub trait Tr {}
		pub type Alias = u8;
		pub fn f() {}
		pub const C: u8 = 0;
		pub static S: u8 = 0;
		pub mod m {}
		const _: () = ();
		extern "C" { fn ext(); static EXT: u8; type Opaque; }
		macro_rules! mac { () => {} }
		impl Named {}
		"#,
	);

	let resolver = Resolver::new(&ws);
	let root = root(&ws, "t");

	assert_eq!(
		resolver.names(root, Namespace::Type),
		["Alias", "E", "Named", "Opaque", "Tr", "Tuple", "U", "Unit", "m"]
	);
	assert_eq!(resolver.names(root, Namespace::Value), ["C", "EXT", "S", "Tuple", "Unit", "ext", "f"]);
	assert_eq!(resolver.names(root, Namespace::Macro), ["mac"]);
	assert_eq!(show_bindings(&resolver, root, "Unit", Namespace::Value), ["t::Unit"]);
	assert!(
		resolver.bindings(root, "A", Namespace::Type).is_empty(),
		"variants are not module members"
	);
	assert!(resolver.unresolved_imports().is_empty());
}

#[test]
fn definitions_are_never_dropped() {
	let source: String = (0..100).map(|index| format!("#[cfg(v{index})] pub struct Many;\n")).collect();
	let ws = single(&source);
	let resolver = Resolver::new(&ws);

	assert_eq!(resolver.bindings(root(&ws, "t"), "Many", Namespace::Type).len(), 100);
	assert_eq!(resolver.resolve_item_path(&item_path("crate::Many")).len(), 100);
}

#[test]
fn edition_2015_paths_are_crate_relative_in_use() {
	let source = r#"
		mod a { pub struct S; }
		mod b { use a::S; }
		mod c { use ::a::S as T; }
		mod d { use std::fmt; }
	"#;

	let ws = workspace([TestCrate::new("old", source).edition(Edition::E2015), TestCrate::new("new", source)]);
	let resolver = Resolver::new(&ws);

	assert_eq!(
		show_bindings(&resolver, find(&ws, "old::b"), "S", Namespace::Type),
		["old::a::S (import)"]
	);
	assert_eq!(
		show_bindings(&resolver, find(&ws, "old::c"), "T", Namespace::Type),
		["old::a::S (import)"]
	);
	assert_eq!(
		show_bindings(&resolver, find(&ws, "old::d"), "fmt", Namespace::Type),
		["ext std::fmt (import)"]
	);
	assert_eq!(resolve_in(&resolver, find(&ws, "old::d"), "::a::S", Namespace::Type), ["old::a::S"]);
	assert_eq!(
		resolve_in(&resolver, find(&ws, "old::d"), "::std::mem", Namespace::Type),
		["ext std::mem"]
	);

	let prefixes = resolver.resolve_prefixes(find(&ws, "old::d"), &path_ref("a::S"), Some(Namespace::Type), PathKind::Use);

	assert_eq!(show_res(&resolver, &prefixes[1]), ["old::a::S"]);
	assert!(resolver.resolve_path(find(&ws, "old::d"), &path_ref("a::S"), Namespace::Type).is_empty());

	// 2018+: `a` is not in scope in `b`, and `::a` is an (unknown) extern crate
	assert!(resolver.bindings(find(&ws, "new::b"), "S", Namespace::Type).is_empty());
	assert_eq!(show_bindings(&resolver, find(&ws, "new::c"), "T", Namespace::Type), ["ext a::S (import)"]);
	assert_eq!(
		show_bindings(&resolver, find(&ws, "new::d"), "fmt", Namespace::Type),
		["ext std::fmt (import)"]
	);
}

#[test]
fn edition_2015_use_paths_are_only_crate_relative() {
	let ws = workspace([
		TestCrate::new("mylib", "pub mod api { pub struct Client; } pub use api::Client;"),
		TestCrate::new("legacy", "mod sub { use mylib::Client; use core::mem; use std::fmt; }")
			.edition(Edition::E2015)
			.dep("mylib", "mylib"),
		TestCrate::new("declared", "extern crate mylib; mod sub { use mylib::Client; }")
			.edition(Edition::E2015)
			.dep("mylib", "mylib"),
		TestCrate::new("bare", "#![cfg_attr(not(feature = \"std\"), no_std)] mod sub { use core::mem; }").edition(Edition::E2015),
	]);

	let resolver = Resolver::new(&ws);
	let sub = find(&ws, "legacy::sub");
	let declared = find(&ws, "declared::sub");
	let client = find(&ws, "mylib::api::Client");

	// without `extern crate`, dependencies are only in scope in paths outside of `use`
	assert!(resolver.bindings(sub, "Client", Namespace::Type).is_empty());
	assert!(resolver.bindings(sub, "mem", Namespace::Type).is_empty(), "only `std` is injected");
	assert_eq!(show_bindings(&resolver, sub, "fmt", Namespace::Type), ["ext std::fmt (import)"]);
	assert!(resolve_in(&resolver, sub, "::mylib::Client", Namespace::Type).is_empty());
	assert_eq!(resolve_in(&resolver, sub, "mylib::Client", Namespace::Type), ["mylib::api::Client"]);
	assert_eq!(resolve_in(&resolver, sub, "::std::mem", Namespace::Type), ["ext std::mem"]);
	assert_eq!(resolve_in(&resolver, sub, "core::mem", Namespace::Type), ["ext core::mem"]);
	assert_eq!(
		resolver.usable_paths(client, Viewpoint::Module(sub)),
		["mylib::Client", "mylib::api::Client"]
	);

	assert_eq!(
		show_bindings(&resolver, declared, "Client", Namespace::Type),
		["mylib::api::Client (import)"]
	);
	assert_eq!(
		resolve_in(&resolver, declared, "::mylib::Client", Namespace::Type),
		["mylib::api::Client"]
	);
	assert_eq!(
		resolver.usable_paths(client, Viewpoint::Module(declared)),
		[
			"Client",
			"mylib::Client",
			"crate::sub::Client",
			"mylib::api::Client",
			"crate::mylib::Client",
			"crate::mylib::api::Client"
		]
	);

	// crates that may be `#![no_std]` get `core` too
	assert_eq!(
		show_bindings(&resolver, find(&ws, "bare::sub"), "mem", Namespace::Type),
		["ext core::mem (import)"]
	);
	assert!(
		resolver
			.unresolved_imports()
			.iter()
			.all(|&import| import.krate() != root(&ws, "bare").krate())
	);
}

#[test]
fn enum_variants_are_imported_by_name_and_glob() {
	let ws = single(
		r#"
		pub enum E { A, B(u8), C { x: u8 } }

		impl E {
			pub fn new() -> Self { E::A }
		}

		mod globbed { use crate::E::*; }
		mod named { use crate::E::{A, C}; use super::E::B as Bee; }
		"#,
	);

	let resolver = Resolver::new(&ws);
	let globbed = find(&ws, "t::globbed");
	let named = find(&ws, "t::named");

	assert_eq!(resolver.names(globbed, Namespace::Type), ["A", "B", "C"]);
	assert_eq!(resolver.names(globbed, Namespace::Value), ["A", "B"]);
	assert_eq!(show_bindings(&resolver, named, "Bee", Namespace::Value), ["t::E::B (import)"]);
	assert_eq!(show_bindings(&resolver, named, "C", Namespace::Type), ["t::E::C (import)"]);
	assert!(resolver.bindings(named, "C", Namespace::Value).is_empty());
	assert_eq!(resolve(&resolver, "crate::E::B"), ["t::E::B"]);
	assert_eq!(resolve(&resolver, "E::new"), ["t::E::new"]);
	assert_eq!(resolve_in(&resolver, globbed, "super::E::new", Namespace::Value), ["t::E::new"]);
}

#[test]
fn enum_variants_shadow_associated_items_in_their_namespaces() {
	let ws = single("pub enum E { A, B(u8), S { x: u8 } } impl E { pub const A: u8 = 0; pub fn B() {} pub const S: u8 = 1; pub fn new() {} }");
	let resolver = Resolver::new(&ws);
	let root = root(&ws, "t");
	let variant = |name: &str| {
		ws.children(find(&ws, "t::E"))
			.find(|&child| ws.item(child).name.as_deref() == Some(name))
			.unwrap()
	};
	let assoc = |name: &str| find_in_impl(&ws, "t", "impl E", name);

	assert_eq!(resolver.resolve_item_path(&item_path("crate::E::A")), [variant("A")]);
	assert_eq!(
		resolver.resolve_path(root, &path_ref("E::A"), Namespace::Value),
		[Res::Item(variant("A"))]
	);
	assert_eq!(
		resolver.resolve_path(root, &path_ref("E::B"), Namespace::Value),
		[Res::Item(variant("B"))]
	);

	// a struct variant is not a value
	assert_eq!(resolver.resolve_path(root, &path_ref("E::S"), Namespace::Value), [Res::Item(assoc("S"))]);
	assert_eq!(resolver.resolve_path(root, &path_ref("E::S"), Namespace::Type), [Res::Item(variant("S"))]);
	assert_eq!(resolver.resolve_item_path(&item_path("crate::E::S")), [variant("S"), assoc("S")]);

	// the shadowed items are reachable through their `impl`
	assert_eq!(resolver.resolve_item_path(&item_path("<E>::A")), [assoc("A")]);
	assert_eq!(resolve(&resolver, "E::new"), ["t::E::new"]);
	assert!(resolver.usable_paths(assoc("A"), Viewpoint::Foreign).is_empty());
	assert_eq!(resolver.usable_paths(assoc("S"), Viewpoint::Foreign), ["::t::E::S"]);
	assert_eq!(resolver.usable_paths(variant("A"), Viewpoint::Foreign), ["::t::E::A"]);
	assert_eq!(canonical_path_round_trip_failures(&ws, &resolver), Vec::<String>::new());
}

#[test]
fn explicit_bindings_shadow_globs() {
	let ws = single(
		r#"
		mod x { pub struct Thing; pub fn helper() {} }
		mod y { pub struct Thing; }
		mod by_import { use crate::x::*; use crate::y::Thing; }
		mod by_definition { use crate::x::*; pub struct Thing; }
		mod ambiguous { use crate::x::*; use crate::y::*; }
		mod ordered { use crate::y::Thing; use crate::x::*; }
		"#,
	);

	let resolver = Resolver::new(&ws);

	assert_eq!(
		show_bindings(&resolver, find(&ws, "t::by_import"), "Thing", Namespace::Type),
		["t::y::Thing (import)"]
	);
	assert_eq!(
		show_bindings(&resolver, find(&ws, "t::by_import"), "helper", Namespace::Value),
		["t::x::helper (glob)"]
	);
	assert_eq!(
		show_bindings(&resolver, find(&ws, "t::by_definition"), "Thing", Namespace::Type),
		["t::by_definition::Thing"]
	);
	assert_eq!(
		show_bindings(&resolver, find(&ws, "t::ordered"), "Thing", Namespace::Type),
		["t::y::Thing (import)"]
	);

	let mut ambiguous = show_bindings(&resolver, find(&ws, "t::ambiguous"), "Thing", Namespace::Type);

	ambiguous.sort();
	assert_eq!(ambiguous, ["t::x::Thing (glob)", "t::y::Thing (glob)"]);
}

#[test]
fn extern_crates_and_renamed_dependencies() {
	let ws = workspace([
		TestCrate::new("lib", "pub struct Thing; pub mod api { pub fn call() {} }"),
		TestCrate::new(
			"app",
			r#"
			extern crate lib as renamed_lib;
			extern crate self as me;
			extern crate std as stdlib;

			use renamed_lib::Thing;
			use other_name::api;
			use serde::Serialize;

			pub struct Local;

			mod m {
				use me::Local;
				use crate::stdlib::fmt;
				use renamed_lib::api::call;
			}
			"#,
		)
		.kind(TargetKind::Bin)
		.dep("lib", "lib")
		.dep("other_name", "lib")
		.dep("serde", "serde"),
	]);

	let resolver = Resolver::new(&ws);
	let app = root(&ws, "app");
	let lib = ws.crates()[0].id();
	let m = find(&ws, "app::m");

	assert_eq!(show_bindings(&resolver, app, "renamed_lib", Namespace::Type), ["lib (import)"]);
	assert_eq!(show_bindings(&resolver, app, "Thing", Namespace::Type), ["lib::Thing (import)"]);
	assert_eq!(show_bindings(&resolver, app, "api", Namespace::Type), ["lib::api (import)"]);
	assert_eq!(
		show_bindings(&resolver, app, "Serialize", Namespace::Macro),
		["ext serde::Serialize (import)"]
	);
	assert_eq!(show_bindings(&resolver, m, "Local", Namespace::Type), ["app::Local (import)"]);
	assert_eq!(show_bindings(&resolver, m, "fmt", Namespace::Type), ["ext std::fmt (import)"]);
	assert_eq!(show_bindings(&resolver, m, "call", Namespace::Value), ["lib::api::call (import)"]);

	assert_eq!(resolver.crate_by_name(app.krate(), "lib"), Some(lib));
	assert_eq!(resolver.crate_by_name(app.krate(), "other_name"), Some(lib));
	assert_eq!(resolver.crate_by_name(app.krate(), "renamed_lib"), Some(lib));
	assert_eq!(resolver.crate_by_name(app.krate(), "me"), Some(app.krate()));
	assert_eq!(resolver.crate_by_name(app.krate(), "serde"), None);
	assert_eq!(resolver.crate_by_name(lib, "app"), None);

	let serialize = ws
		.children(app)
		.flat_map(|child| ws.children(child))
		.find(|&import| ws.item(import).name.as_deref() == Some("Serialize"));

	assert_eq!(
		show_res(&resolver, &resolver.import_targets(serialize.unwrap())),
		["ext serde::Serialize"]
	);
}

#[test]
fn external_imports_bind_every_namespace() {
	let ws = single("use std::fmt::Display; use std::collections::{self, HashMap as Map};");
	let resolver = Resolver::new(&ws);
	let root = root(&ws, "t");

	for namespace in Namespace::ALL {
		assert_eq!(show_bindings(&resolver, root, "Display", namespace), ["ext std::fmt::Display (import)"]);
		assert_eq!(
			show_bindings(&resolver, root, "Map", namespace),
			["ext std::collections::HashMap (import)"]
		);
	}

	assert_eq!(
		show_bindings(&resolver, root, "collections", Namespace::Type),
		["ext std::collections (import)"]
	);
	assert!(resolver.bindings(root, "collections", Namespace::Value).is_empty());
	assert_eq!(
		resolve_in(&resolver, root, "Map::new", Namespace::Value),
		["ext std::collections::HashMap::new"]
	);
	assert!(resolve(&resolver, "crate::Display").is_empty(), "user paths only name loaded items");
}

#[test]
fn external_imports_do_not_shadow_glob_imports() {
	let ws = workspace([TestCrate::new(
		"t",
		r#"
		pub mod error { pub struct Error; }
		pub fn warn() {}
		pub struct Level;

		mod handler {
			use super::*;
			use log::error;
			use log::warn;
		}

		mod named {
			use super::*;
			use crate::error::Error as Level;
		}
		"#,
	)
	.dep("log", "log")]);

	let resolver = Resolver::new(&ws);
	let handler = find(&ws, "t::handler");

	// the namespaces `log::error` is in are unknown, so the glob-imported module stays visible next to it
	assert_eq!(
		show_bindings(&resolver, handler, "error", Namespace::Type),
		["t::error (glob)", "ext log::error (import)"]
	);
	assert_eq!(show_bindings(&resolver, handler, "error", Namespace::Macro), ["ext log::error (import)"]);
	assert_eq!(
		resolve_in(&resolver, handler, "error::Error", Namespace::Type),
		["t::error::Error", "ext log::error::Error"]
	);
	assert_eq!(resolve_in(&resolver, handler, "warn", Namespace::Value), ["t::warn", "ext log::warn"]);
	assert_eq!(resolve_in(&resolver, handler, "error", Namespace::Macro), ["ext log::error"]);

	// imports of loaded items still shadow glob imports
	assert_eq!(
		show_bindings(&resolver, find(&ws, "t::named"), "Level", Namespace::Type),
		["t::error::Error (import)"]
	);
	assert_eq!(
		show_bindings(&resolver, find(&ws, "t::named"), "Level", Namespace::Value),
		["t::error::Error (import)"]
	);
}

#[test]
fn fields_are_named_by_paths_with_a_dot() {
	let ws = single(
		r#"
		pub struct Point { pub x: u8, y: u8 }
		pub struct Pair(pub u8, u16);
		pub enum Shape { Circle { radius: f64 }, Square(f64) }
		pub union U { a: u8 }
		pub struct Controlled { frames: u32, len: u32 }

		impl Controlled {
			pub fn len(&self) -> u32 { self.len }
		}
		"#,
	);

	let resolver = Resolver::new(&ws);
	let root = root(&ws, "t");

	assert_eq!(resolve(&resolver, "crate::Point.x"), ["t::Point.x"]);
	assert_eq!(resolve(&resolver, "::t::Pair.1"), ["t::Pair.1"]);
	assert_eq!(resolve(&resolver, "Shape::Circle.radius"), ["t::Shape::Circle.radius"]);
	assert_eq!(resolve(&resolver, "Shape::Square.0"), ["t::Shape::Square.0"]);
	assert_eq!(resolve(&resolver, "U.a"), ["t::U.a"]);
	assert!(resolve(&resolver, "Point.z").is_empty());
	assert!(resolve(&resolver, "Pair.2").is_empty());
	assert!(resolve(&resolver, "Shape.radius").is_empty());

	// `Type::name` names a field when nothing else has that name: an associated item of that name wins
	assert_eq!(resolve(&resolver, "Controlled::frames"), ["t::Controlled.frames"]);
	assert_eq!(resolve(&resolver, "Controlled::len"), ["t::Controlled::len"]);
	assert_eq!(resolve(&resolver, "Controlled.len"), ["t::Controlled.len"]);

	// code paths never reach fields, which are bound nowhere and have no usable paths
	let x = item(&resolver, "Point.x");
	let y = item(&resolver, "Point.y");
	let radius = item(&resolver, "Shape::Circle.radius");

	assert!(resolve_in(&resolver, root, "Point::x", Namespace::Value).is_empty());
	assert!(resolve_in(&resolver, root, "Controlled::frames", Namespace::Value).is_empty());
	assert!(resolver.usable_paths(x, Viewpoint::Foreign).is_empty());

	// fields of variants are as visible as their enum
	assert_eq!(resolver.visibility_scope(x), None);
	assert_eq!(resolver.visibility_scope(y), Some(root));
	assert_eq!(resolver.visibility_scope(radius), None);
	assert_eq!(canonical_path_round_trip_failures(&ws, &resolver), Vec::<String>::new());
}

fn find(ws: &Workspace, path: &str) -> ItemId {
	let found = find_all(ws, path);

	assert_eq!(found.len(), 1, "`{path}` names {} items", found.len());
	found[0]
}

/// Items at a path of names from a crate root, walking the item tree (not resolving names). Extern blocks are
/// transparent; imports are skipped.
fn find_all(ws: &Workspace, path: &str) -> Vec<ItemId> {
	let mut names = path.split("::");
	let mut current = vec![root(ws, names.next().expect("empty path"))];

	for name in names {
		current = current
			.iter()
			.flat_map(|&item| ws.children(item))
			.flat_map(|child| match ws.item(child).kind {
				ItemKind::ExternBlock => ws.children(child).collect(),
				_ => vec![child],
			})
			.filter(|&child| ws.item(child).kind != ItemKind::Import && ws.item(child).name.as_deref() == Some(name))
			.collect();
	}

	current
}

/// An `impl` block of a module (given as a tree path) by its header without generics (`impl Tr for Foo`).
fn find_impl(ws: &Workspace, module: &str, header: &str) -> ItemId {
	let module = find(ws, module);

	ws.children(module)
		.find(|&child| {
			ws.item(child).impl_info().is_some_and(|info| match &info.trait_text {
				Some(trait_text) => header == format!("impl {trait_text} for {}", info.self_ty_text),
				None => header == format!("impl {}", info.self_ty_text),
			})
		})
		.unwrap_or_else(|| panic!("no `{header}`"))
}

/// The import of `module` whose path (with `*` for globs) is `path`.
fn find_import(ws: &Workspace, module: ItemId, path: &str) -> ItemId {
	ws.children(module)
		.flat_map(|use_item| ws.children(use_item))
		.find(|&import| {
			let info = ws.item(import).import_info().expect("an import");
			let written = if info.glob { format!("{}::*", info.path) } else { info.path.to_string() };

			written == path
		})
		.unwrap_or_else(|| panic!("no `use {path}`"))
}

/// An item of an `impl` block.
fn find_in_impl(ws: &Workspace, module: &str, header: &str, name: &str) -> ItemId {
	let impl_block = find_impl(ws, module, header);

	ws.children(impl_block)
		.find(|&child| ws.item(child).name.as_deref() == Some(name))
		.expect("no such impl item")
}

fn fixture(path: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(path)
}

#[test]
fn fixture_edition_2015_paths() {
	let ws = workspace([TestCrate::on_disk("resolve_2015", fixture("resolve_2015/src/lib.rs")).edition(Edition::E2015)]);
	let resolver = Resolver::new(&ws);
	let s = item(&resolver, "crate::a::S");
	let b = item(&resolver, "crate::b");
	let c = item(&resolver, "crate::c");

	assert_eq!(
		resolver.bindings(b, "S", Namespace::Type).first().map(|binding| binding.res.clone()),
		Some(Res::Item(s))
	);
	assert_eq!(
		resolver.bindings(c, "T", Namespace::Type).first().map(|binding| binding.res.clone()),
		Some(Res::Item(s))
	);
	assert_eq!(
		resolver.bindings(c, "fmt", Namespace::Type).first().map(|binding| binding.res.clone()),
		Some(Res::External("std::fmt".into()))
	);
	assert!(resolver.unresolved_imports().is_empty());
}

#[test]
fn fixture_glob_cycles() {
	let ws = workspace([TestCrate::on_disk("resolve_cycles", fixture("resolve_cycles/src/lib.rs"))]);
	let resolver = Resolver::new(&ws);

	assert_eq!(resolve(&resolver, "crate::a::B"), ["resolve_cycles::b::B"]);
	assert_eq!(resolve(&resolver, "crate::b::A"), ["resolve_cycles::a::A"]);
	assert_eq!(resolve(&resolver, "crate::B"), ["resolve_cycles::b::B"]);
}

#[test]
fn fixture_impls_and_associated_items() {
	let ws = basic_fixture();
	let resolver = Resolver::new(&ws);
	let circle = item(&resolver, "crate::Circle");
	let shape = item(&resolver, "crate::shapes::Shape");
	let area = item(&resolver, "crate::shapes::Shape::area");

	assert_eq!(
		show_all(&resolver, &resolver.impls_of(circle)),
		[
			"impl resolve_basic::shapes::Circle",
			"impl Shape for resolve_basic::shapes::Circle",
			"impl fmt::Display for resolve_basic::shapes::Circle",
		]
	);

	assert_eq!(resolver.impls_of(shape).len(), 3);

	assert_eq!(
		show_all(&resolver, &resolver.trait_counterparts(area)),
		[
			"<resolve_basic::shapes::Circle as Shape>::area",
			"<resolve_basic::shapes::Square as Shape>::area",
			"resolve_basic::shapes::<impl Shape for Vec<T>>::area",
		]
	);

	assert_eq!(resolve(&resolver, "crate::Circle::new"), ["resolve_basic::shapes::Circle::new"]);
	assert_eq!(
		resolve(&resolver, "<Circle as Display>::fmt"),
		["<resolve_basic::shapes::Circle as fmt::Display>::fmt"]
	);
	assert_eq!(
		resolve(&resolver, "<crate::shapes::Square as Shape>::name"),
		["<resolve_basic::shapes::Square as Shape>::name"]
	);
	assert_eq!(resolve(&resolver, "<Circle>::new"), ["resolve_basic::shapes::Circle::new"]);

	let circle_area = item(&resolver, "<Circle as Shape>::area");

	assert_eq!(
		show_all(&resolver, &resolver.trait_counterparts(circle_area)),
		["resolve_basic::shapes::Shape::area"]
	);
	assert_eq!(
		show_all(&resolver, &resolver.associated_items(circle))[0],
		"resolve_basic::shapes::Circle::new"
	);
}

#[test]
fn fixture_reexports_globs_and_variants() {
	let ws = basic_fixture();
	let resolver = Resolver::new(&ws);

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

	// `Circle` has named fields, and `Blue` is a struct variant: neither is a value
	assert_eq!(resolver.names(prelude, Namespace::Value), ["Box2", "Green", "Red"]);
}

#[test]
fn fixture_usable_paths_and_visibility() {
	let ws = basic_fixture();
	let resolver = Resolver::new(&ws);
	let root = ws.crates()[0].root_module();
	let circle = item(&resolver, "crate::Circle");
	let triple = item(&resolver, "crate::util::helpers::triple");

	assert_eq!(
		resolver.usable_paths(circle, Viewpoint::Foreign),
		[
			"::resolve_basic::Circle",
			"::resolve_basic::shapes::Circle",
			"::resolve_basic::prelude::Circle",
			"::resolve_basic::prelude::shapes::Circle"
		]
	);

	assert!(resolver.usable_paths(triple, Viewpoint::Foreign).is_empty());
	assert_eq!(
		resolver.usable_paths(triple, Viewpoint::Module(root)),
		["triple", "crate::triple", "crate::util::helpers::triple"]
	);

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
fn fixture_workspace_with_renamed_dependency() {
	let ws = workspace([
		TestCrate::on_disk("mylib", fixture("resolve_workspace/mylib/src/lib.rs")),
		TestCrate::on_disk("app", fixture("resolve_workspace/app/src/main.rs"))
			.kind(TargetKind::Bin)
			.dep("mylib", "mylib")
			.dep("renamed", "mylib"),
	]);

	let resolver = Resolver::new(&ws);
	let client = item(&resolver, "::mylib::Client");
	let app_root = ws.crates()[1].root_module();

	assert_eq!(resolver.crate_by_name(app_root.krate(), "renamed"), Some(client.krate()));
	assert_eq!(resolve(&resolver, "renamed::Client"), ["mylib::api::Client"]);
	assert_eq!(resolve(&resolver, "::app::C"), ["mylib::api::Client"]);

	assert_eq!(
		resolver.usable_paths(client, Viewpoint::Module(app_root)),
		[
			"C",
			"crate::C",
			"mylib::Client",
			"renamed::Client",
			"crate::api::Client",
			"mylib::api::Client",
			"renamed::api::Client"
		]
	);

	assert!(
		resolver
			.usable_paths(item(&resolver, "::mylib::hidden::Secret"), Viewpoint::Module(app_root))
			.is_empty()
	);
	assert_eq!(resolve(&resolver, "::mylib::Client::connect"), ["mylib::api::Client::connect"]);
}

#[test]
fn foreign_paths_through_unselected_crates_only_reach_their_own_items() {
	let ws = workspace([
		TestCrate::new("a", "pub struct X;"),
		TestCrate::new("b", "pub use a::X; pub struct Y;").dep("a", "a").unselected(),
	]);
	let resolver = Resolver::new(&ws);
	let x = find(&ws, "a::X");

	assert_eq!(resolver.usable_paths(x, Viewpoint::Foreign), ["::a::X"]);
	assert_eq!(resolver.usable_paths(find(&ws, "b::Y"), Viewpoint::Foreign), ["::b::Y"]);
	assert_eq!(resolver.usable_paths(x, Viewpoint::Module(root(&ws, "b"))), ["X", "a::X", "crate::X"]);
}

#[test]
fn glob_cycles_terminate() {
	let ws = single(
		r#"
		mod a { pub use super::b::*; pub struct A; }
		mod b { pub use super::a::*; pub struct B; pub use super::c::*; }
		mod c { pub use super::a::*; pub use super::b::*; pub fn from_c() {} }
		mod d { pub use self::*; pub use crate::d::*; }
		"#,
	);

	let resolver = Resolver::new(&ws);

	assert_eq!(resolve(&resolver, "crate::a::B"), ["t::b::B"]);
	assert_eq!(resolve(&resolver, "crate::b::A"), ["t::a::A"]);
	assert_eq!(resolve(&resolver, "crate::a::from_c"), ["t::c::from_c"]);
	assert_eq!(resolve(&resolver, "crate::c::A"), ["t::a::A"]);
	assert_eq!(resolver.names(find(&ws, "t::a"), Namespace::Type), ["A", "B"]);
	assert_eq!(show_bindings(&resolver, find(&ws, "t::a"), "A", Namespace::Type), ["t::a::A"]);
	assert!(resolver.names(find(&ws, "t::d"), Namespace::Type).is_empty());
	assert_eq!(
		resolver.usable_paths(find(&ws, "t::c::from_c"), Viewpoint::Module(find(&ws, "t::d"))),
		["crate::a::from_c", "crate::b::from_c", "crate::c::from_c"]
	);
}

#[test]
fn glob_heavy_crates_resolve_quickly() {
	// a chain of nested modules re-exporting their parents, and a star of modules re-exporting each other through
	// the crate root
	let items = |prefix: &str| (0..20).map(|index| format!("pub struct {prefix}{index}; ")).collect::<String>();
	let mut chain = String::new();

	for depth in 0..100 {
		chain.push_str(&format!("pub mod c{depth} {{ pub use super::*; {} ", items(&format!("C{depth}_"))));
	}

	chain.push_str(&"}".repeat(100));

	let star: String = (0..100)
		.map(|index| {
			format!(
				"pub use s{index}::*; pub mod s{index} {{ pub use crate::*; {} }} ",
				items(&format!("S{index}_"))
			)
		})
		.collect();

	// parsing 100 nested modules recurses deeply (in syn and the model builder, not in the resolver)
	let ws = std::thread::Builder::new()
		.stack_size(256 << 20)
		.spawn(move || workspace([TestCrate::new("chain", &chain), TestCrate::new("star", &star)]))
		.expect("spawn")
		.join()
		.expect("model");

	let started = Instant::now();
	let resolver = Resolver::new(&ws);
	let elapsed = started.elapsed();

	println!("glob-heavy crates resolved in {elapsed:?}");

	let deepest = format!("chain{}", (0..100).map(|depth| format!("::c{depth}")).collect::<String>());

	assert_eq!(resolver.names(find(&ws, &deepest), Namespace::Type).len(), 100 * 20 + 100);
	assert_eq!(resolver.names(find(&ws, "star::s42"), Namespace::Type).len(), 100 * 20 + 100);
	assert_eq!(resolve(&resolver, "::star::s7::S99_3"), ["star::s99::S99_3"]);
	assert_eq!(
		resolver.usable_paths(find(&ws, "star::s0::S0_0"), Viewpoint::Foreign)[..2],
		["::star::S0_0", "::star::s0::S0_0"]
	);
}

#[test]
fn glob_imports_respect_visibility() {
	let ws = single(
		r#"
		mod src {
			pub struct Public;
			struct Private;
			pub(crate) struct CrateVisible;
			pub(super) fn to_parent() {}

			mod inner {
				use super::*;
			}
		}

		mod dst {
			pub use crate::src::*;
		}
		"#,
	);

	let resolver = Resolver::new(&ws);
	let dst = find(&ws, "t::dst");
	let inner = find(&ws, "t::src::inner");

	assert_eq!(resolver.names(dst, Namespace::Type), ["CrateVisible", "Public"]);
	assert_eq!(resolver.names(dst, Namespace::Value), ["CrateVisible", "Public", "to_parent"]);
	assert_eq!(show_bindings(&resolver, dst, "Public", Namespace::Type), ["t::src::Public (glob)"]);
	assert_eq!(resolver.names(inner, Namespace::Type), ["CrateVisible", "Private", "Public", "inner"]);

	// re-exported with the narrower of both visibilities
	assert!(resolver.usable_paths(find(&ws, "t::src::CrateVisible"), Viewpoint::Foreign).is_empty());
}

#[test]
fn globs_are_not_read_before_named_imports_of_the_same_name_resolve() {
	// `reader` and `m`'s named import are processed before `chain` resolves; a naive fixpoint would let `reader`
	// see `m`'s glob binding of `Thing` before the named import shadows it
	let ws = single(
		r#"
		mod reader { pub use crate::m::Thing; }
		mod m { pub use crate::early::*; pub use crate::chain::Thing; }
		mod chain { pub use crate::real::Thing; }
		mod real { pub struct Thing; }
		mod early { pub struct Thing; pub struct Other; }
		"#,
	);

	let resolver = Resolver::new(&ws);

	assert_eq!(resolve(&resolver, "crate::reader::Thing"), ["t::real::Thing"]);
	assert_eq!(resolve(&resolver, "crate::m::Thing"), ["t::real::Thing"]);
	assert_eq!(resolve(&resolver, "crate::m::Other"), ["t::early::Other"]);
}

#[test]
fn guessed_namespaces_survive_reexports() {
	let ws = workspace([TestCrate::new(
		"t",
		r#"
		mod a { pub use log::error; pub use std::sync::Arc; }
		mod b { pub use crate::a::error; pub use crate::c::*; }
		mod c { pub mod error { pub struct E; } }
		mod d { pub use crate::a::*; use std::sync::Arc; }
		"#,
	)
	.dep("log", "log")]);

	let resolver = Resolver::new(&ws);

	assert_eq!(
		show_bindings(&resolver, find(&ws, "t::b"), "error", Namespace::Type),
		["ext log::error (import)", "t::c::error (glob)"]
	);
	assert_eq!(
		show_bindings(&resolver, find(&ws, "t::b"), "error", Namespace::Macro),
		["ext log::error (import)"]
	);

	// a glob binding of the same target as a named import is redundant
	assert_eq!(
		show_bindings(&resolver, find(&ws, "t::d"), "Arc", Namespace::Type),
		["ext std::sync::Arc (import)"]
	);
	assert_eq!(
		show_bindings(&resolver, find(&ws, "t::d"), "error", Namespace::Value),
		["ext log::error (glob)"]
	);
}

#[test]
fn impl_self_types_resolve_through_imports_aliases_and_references() {
	let ws = single(
		r#"
		pub mod types {
			pub struct Foo;
			pub struct Gen<T>(T);
			pub type Alias = Foo;
			pub trait Tr {}
		}

		mod impls {
			use crate::types::{Foo, Gen, Alias as Renamed};

			struct T;

			impl Foo { pub fn new() {} }
			impl crate::types::Tr for &Foo {}
			impl<T> Gen<T> { fn get() {} }
			impl Renamed { fn via_alias() {} }
			impl<T> crate::types::Tr for T {}
			impl std::fmt::Display for Gen<u8> { fn fmt() {} }
			impl Tr2 for Vec<u8> {}
			impl T {}
		}
		"#,
	);

	let resolver = Resolver::new(&ws);
	let self_types = |header: &str| show_all(&resolver, &resolver.impl_self_types(find_impl(&ws, "t::impls", header)));
	let traits = |header: &str| show_all(&resolver, &resolver.impl_traits(find_impl(&ws, "t::impls", header)));

	assert_eq!(self_types("impl Foo"), ["t::types::Foo"]);
	assert_eq!(self_types("impl crate::types::Tr for &Foo"), ["t::types::Foo"]);
	assert_eq!(self_types("impl Gen<T>"), ["t::types::Gen"]);
	assert_eq!(self_types("impl Renamed"), ["t::types::Alias"]);
	assert_eq!(self_types("impl crate::types::Tr for T"), Vec::<String>::new());
	assert_eq!(self_types("impl std::fmt::Display for Gen<u8>"), ["t::types::Gen"]);
	assert_eq!(self_types("impl Tr2 for Vec<u8>"), Vec::<String>::new());
	assert_eq!(self_types("impl T"), ["t::impls::T"]);

	assert_eq!(traits("impl crate::types::Tr for &Foo"), ["t::types::Tr"]);
	assert_eq!(traits("impl std::fmt::Display for Gen<u8>"), Vec::<String>::new());

	let foo = find(&ws, "t::types::Foo");
	let tr = find(&ws, "t::types::Tr");

	assert_eq!(
		show_all(&resolver, &resolver.impls_of(foo)),
		["impl t::types::Foo", "impl crate::types::Tr for t::types::Foo"]
	);
	assert_eq!(
		show_all(&resolver, &resolver.impls_of(tr)),
		["impl crate::types::Tr for t::types::Foo", "t::impls::<impl crate::types::Tr for T>"]
	);
	assert_eq!(
		show(&resolver, find_impl(&ws, "t::impls", "impl Tr2 for Vec<u8>")),
		"t::impls::<impl Tr2 for Vec<u8>>"
	);
	assert_eq!(
		show(&resolver, find_in_impl(&ws, "t::impls", "impl std::fmt::Display for Gen<u8>", "fmt")),
		"<t::types::Gen as std::fmt::Display>::fmt"
	);
	assert_eq!(
		show(&resolver, find_in_impl(&ws, "t::impls", "impl Renamed", "via_alias")),
		"t::types::Alias::via_alias"
	);
}

#[test]
fn import_cycles_are_given_up_on_without_losing_other_imports() {
	let ws = single(
		r#"
		mod a { pub use crate::b::x; }
		mod b { pub use crate::a::x; }

		mod gen { pub use self::missing1::*; pub use self::missing2::*; pub struct Here; }
		mod reader { pub use crate::gen::Here; pub use crate::gen::Nowhere; }

		mod m { pub use crate::src::*; pub use crate::a::x as Thing; }
		mod src { pub struct Thing; }
		mod uses_m { pub use crate::m::Thing; }
		"#,
	);

	let resolver = Resolver::new(&ws);

	assert!(resolver.bindings(find(&ws, "t::a"), "x", Namespace::Value).is_empty());
	assert_eq!(
		show_bindings(&resolver, find(&ws, "t::reader"), "Here", Namespace::Type),
		["t::gen::Here (import)"]
	);
	assert_eq!(
		show_bindings(&resolver, find(&ws, "t::uses_m"), "Thing", Namespace::Type),
		["t::src::Thing (import)"]
	);

	let mut unresolved: Vec<String> = resolver
		.unresolved_imports()
		.iter()
		.map(|&import| ws.item(import).import_info().unwrap().path.to_string())
		.collect();

	unresolved.sort();
	assert_eq!(
		unresolved,
		[
			"crate::a::x",
			"crate::a::x",
			"crate::b::x",
			"crate::gen::Nowhere",
			"self::missing1",
			"self::missing2"
		]
	);
}

#[test]
fn imports_through_glob_imported_modules() {
	let ws = single(
		r#"
		mod outer {
			pub mod inner { pub struct X; }
			pub enum Kind { A, B }
		}

		mod user {
			use inner::X as Y;
			use Kind::*;
			use crate::outer::*;
		}
		"#,
	);

	let resolver = Resolver::new(&ws);
	let user = find(&ws, "t::user");
	let outer = find(&ws, "t::outer");
	let kind = find(&ws, "t::outer::Kind");

	assert_eq!(show_bindings(&resolver, user, "Y", Namespace::Type), ["t::outer::inner::X (import)"]);
	assert_eq!(show_bindings(&resolver, user, "A", Namespace::Value), ["t::outer::Kind::A (glob)"]);
	assert_eq!(show_bindings(&resolver, user, "inner", Namespace::Type), ["t::outer::inner (glob)"]);

	let globs: Vec<ItemId> = ws
		.children(user)
		.flat_map(|use_item| ws.children(use_item))
		.filter(|&import| ws.item(import).import_info().unwrap().glob)
		.collect();

	assert_eq!(show_res(&resolver, &resolver.import_targets(globs[0])), ["t::outer::Kind"]);
	assert_eq!(show_res(&resolver, &resolver.import_targets(globs[1])), ["t::outer"]);
	assert_eq!(resolver.imports_of(kind), [globs[0]]);
	assert_eq!(resolver.imports_of(outer), [globs[1]]);

	// the bindings of a non-module item's scope are those of its module
	assert_eq!(show_bindings(&resolver, kind, "inner", Namespace::Type), ["t::outer::inner"]);
}

#[test]
fn inherent_impls_of_bare_trait_objects_are_not_trait_items() {
	let ws = workspace([TestCrate::new("t", "pub trait Tr { fn req(&self); } impl Tr { pub fn inherent_on_dyn(&self) {} }").edition(Edition::E2018)]);
	let resolver = Resolver::new(&ws);
	let tr = find(&ws, "t::Tr");
	let impl_block = find_impl(&ws, "t", "impl Tr");
	let inherent = find_in_impl(&ws, "t", "impl Tr", "inherent_on_dyn");

	assert_eq!(show(&resolver, impl_block), "t::<impl Tr>");
	assert_eq!(show(&resolver, inherent), "t::<impl Tr>::inherent_on_dyn");
	assert_eq!(resolver.impl_self_types(impl_block), [tr]);
	assert_eq!(resolver.impls_of(tr), [impl_block]);
	assert_eq!(resolver.associated_items(tr), [find(&ws, "t::Tr::req")]);
	assert!(resolve(&resolver, "crate::Tr::inherent_on_dyn").is_empty());
	assert_eq!(resolver.resolve_item_path(&item_path("<Tr>::inherent_on_dyn")), [inherent]);
	assert_eq!(canonical_path_round_trip_failures(&ws, &resolver), Vec::<String>::new());
}

/// The single item a user path names.
fn item(resolver: &Resolver<'_>, text: &str) -> ItemId {
	let items = resolver.resolve_item_path(&item_path(text));

	assert_eq!(items.len(), 1, "`{text}` names {} items", items.len());
	items[0]
}

/// An [`ItemPath`] from text (`crate::a`, `::dep::b`, `a::b`, `<A as B>::c`, `<A>`, `use a::b`); `ItemPath::parse`
/// lives elsewhere.
fn item_path(text: &str) -> ItemPath {
	let text = text.trim();

	if let Some(rest) = text.strip_prefix("use ") {
		return ItemPath {
			import: true,
			..item_path(rest)
		};
	}

	if let Some(rest) = text.strip_prefix('<') {
		let close = rest.rfind('>').expect("unclosed qualifier");
		let (self_ty, trait_path) = match rest[..close].split_once(" as ") {
			Some((self_ty, trait_path)) => (self_ty, Some(trait_path)),
			None => (&rest[..close], None),
		};
		let mut rest = &rest[close + 1..];

		// a selector after the qualifier: `[name]`, `[#attribute]`, or `[index]`
		let selector = rest.strip_prefix('[').map(|selector| {
			let (selector, after) = selector.split_once(']').expect("unclosed selector");

			rest = after;

			match (selector.strip_prefix('#'), selector.parse()) {
				(Some(attribute), _) => Selector::Attribute(attribute.to_owned()),
				(None, Ok(index)) => Selector::Index(index),
				(None, Err(_)) => Selector::Item(selector.into()),
			}
		});

		return ItemPath {
			anchor: Anchor::None,
			qualifier: Some(Qualifier {
				self_ty: Box::new(item_path(self_ty)),
				trait_path: trait_path.map(|trait_path| Box::new(item_path(trait_path))),
				selector,
			}),
			segments: segments(rest.trim_start_matches("::")),
			arguments: None,
			import: false,
			field: None,
			selector: None,
			macro_call: false,
		};
	}

	// macro invocations, with an optional index
	if let Some((name, index)) = text.split_once('!') {
		return ItemPath {
			macro_call: true,
			selector: index.trim_matches(['[', ']']).parse().ok().map(Selector::Index),
			..item_path(name)
		};
	}

	// a field after a `.`
	if let Some((owner, field)) = text.split_once('.') {
		return ItemPath {
			field: Some(field.into()),
			..item_path(owner)
		};
	}

	let (anchor, rest) = if let Some(rest) = text.strip_prefix("::") {
		(Anchor::Global, rest)
	} else if let Some(rest) = text.strip_prefix("crate") {
		(Anchor::Crate, rest.trim_start_matches("::"))
	} else if let Some(rest) = text.strip_prefix("self") {
		(Anchor::SelfModule, rest.trim_start_matches("::"))
	} else if let Some(rest) = text.strip_prefix("super") {
		(Anchor::Super(1), rest.trim_start_matches("::"))
	} else {
		(Anchor::None, text)
	};

	ItemPath {
		anchor,
		qualifier: None,
		segments: segments(rest),
		arguments: None,
		import: false,
		field: None,
		selector: None,
		macro_call: false,
	}
}

#[test]
fn long_import_chains_resolve_in_linear_time() {
	// every link can only be resolved after the next one, in both directions, and through glob imports (whose bindings
	// are quadratic in number: the glob chain is shorter)
	let mut reverse = String::new();
	let mut forward = String::new();
	let mut alternating = String::new();
	let mut globs = String::new();

	for index in 0..2000 {
		reverse.push_str(&format!("pub mod m{index} {{ pub use crate::m{}::X; }}\n", index + 1));
		forward.push_str(&format!("pub mod m{} {{ pub use crate::m{index}::X; }}\n", index + 1));
		alternating.push_str(&format!(
			"pub mod a{index} {{ pub use crate::b{index}::X; }}\npub mod b{index} {{ pub use crate::a{}::*; }}\n",
			index + 1
		));
	}

	for index in 0..300 {
		globs.push_str(&format!(
			"pub mod g{index} {{ pub use crate::g{}::*; pub struct S{index}; }}\n",
			index + 1
		));
	}

	reverse.push_str("pub mod m2000 { pub struct X; }");
	forward.push_str("pub mod m0 { pub struct X; }");
	alternating.push_str("pub mod a2000 { pub struct X; }");
	globs.push_str("pub mod g300 { pub struct Last; }");

	let ws = workspace([
		TestCrate::new("reverse", &reverse),
		TestCrate::new("forward", &forward),
		TestCrate::new("alternating", &alternating),
		TestCrate::new("globs", &globs),
	]);

	let started = Instant::now();
	let resolver = Resolver::new(&ws);
	let elapsed = started.elapsed();

	println!("import chains resolved in {elapsed:?}");

	assert_eq!(resolve(&resolver, "::reverse::m0::X"), ["reverse::m2000::X"]);
	assert_eq!(resolve(&resolver, "::forward::m2000::X"), ["forward::m0::X"]);
	assert_eq!(resolve(&resolver, "::alternating::a0::X"), ["alternating::a2000::X"]);
	assert_eq!(resolve(&resolver, "::globs::g0::Last"), ["globs::g300::Last"]);
	assert_eq!(resolver.names(find(&ws, "globs::g0"), Namespace::Type).len(), 301);
	assert!(resolver.unresolved_imports().is_empty());

	// resolving the imports again for every link took minutes (without optimizations); now it takes milliseconds
	assert!(elapsed.as_secs() < 30, "{elapsed:?}");
}

#[test]
fn macro_invocations_are_named_by_their_macro_and_a_bang() {
	let ws = single(
		r#"
		macro_rules! commands { ($($t:tt)*) => {}; }

		commands! { static SAY = 1; }
		commands! { static QUIT = 2; }
		other::commands! { static HELP = 3; }
		crate::single!();

		pub mod m {
			thread_local! { static COUNTER: u8 = 0; }
			commands! { static INNER = 4; }
		}

		impl S { inside!(); }
		"#,
	);

	let resolver = Resolver::new(&ws);

	// the invocations of every macro whose path ends with the name; the canonical paths of several have an index
	assert_eq!(resolve(&resolver, "crate::commands!"), ["t::commands![1]", "t::commands![2]", "t::commands![3]"]);
	assert_eq!(resolve(&resolver, "commands![2]"), ["t::commands![2]"]);
	assert_eq!(resolve(&resolver, "::t::single!"), ["t::single!"]);
	assert_eq!(resolve(&resolver, "m::commands!"), ["t::m::commands!"]);
	assert_eq!(resolve(&resolver, "m::thread_local!"), ["t::m::thread_local!"]);
	assert!(resolve(&resolver, "commands![4]").is_empty());
	assert!(resolve(&resolver, "m::single!").is_empty());
	assert!(resolve(&resolver, "inside!").is_empty());

	// the macro definition is not an invocation
	assert_eq!(resolve(&resolver, "crate::commands"), ["t::commands"]);
	assert_eq!(canonical_path_round_trip_failures(&ws, &resolver), Vec::<String>::new());
}

#[test]
fn macro_rules_scopes() {
	let ws = single(
		r#"
		mod macros {
			#[macro_export]
			macro_rules! exported { () => {} }

			macro_rules! local { () => {} }

			pub(crate) use local;

			mod child {}
		}

		mod other {}

		#[macro_use]
		mod with_use {
			macro_rules! from_use { () => {} }
		}

		pub macro decl() {}
		"#,
	);

	let resolver = Resolver::new(&ws);
	let root = root(&ws, "t");
	let exported = find(&ws, "t::macros::exported");
	let local = find(&ws, "t::macros::local");
	let other = find(&ws, "t::other");
	let child = find(&ws, "t::macros::child");

	assert_eq!(show(&resolver, exported), "t::exported");
	assert_eq!(show(&resolver, local), "t::macros::local");
	assert_eq!(show_bindings(&resolver, root, "exported", Namespace::Macro), ["t::exported"]);
	assert_eq!(resolve(&resolver, "crate::exported"), ["t::exported"]);
	assert_eq!(resolve(&resolver, "crate::macros::local"), ["t::macros::local"]);
	assert_eq!(resolve(&resolver, "crate::macros::exported"), ["t::exported"]);
	assert_eq!(resolve(&resolver, "crate::decl"), ["t::decl"]);

	assert_eq!(show_bindings(&resolver, child, "local", Namespace::Macro), ["t::macros::local"]);
	assert!(resolver.bindings(other, "local", Namespace::Macro).is_empty());
	assert_eq!(show_bindings(&resolver, root, "from_use", Namespace::Macro), ["t::with_use::from_use"]);
	assert_eq!(show_bindings(&resolver, other, "from_use", Namespace::Macro), ["t::with_use::from_use"]);
	assert_eq!(resolver.names(child, Namespace::Macro), ["exported", "from_use", "local"]);
	assert_eq!(resolve_in(&resolver, child, "local", Namespace::Macro), ["t::macros::local"]);
	assert_eq!(resolve_in(&resolver, other, "crate::exported", Namespace::Macro), ["t::exported"]);

	assert_eq!(resolver.usable_paths(local, Viewpoint::Module(child)), ["local", "crate::macros::local"]);
	assert_eq!(resolver.usable_paths(exported, Viewpoint::Module(other)), ["crate::exported"]);
	assert_eq!(resolver.usable_paths(exported, Viewpoint::Foreign), ["::t::exported"]);
	assert!(resolver.usable_paths(local, Viewpoint::Foreign).is_empty());
}

#[test]
fn malformed_imports_bind_nothing() {
	let ws = workspace([
		TestCrate::new("dep", "#[macro_export] macro_rules! dep_macro { () => {} } pub struct Thing;"),
		TestCrate::new(
			"t",
			r#"
			#[macro_use]
			extern crate dep as _;

			pub trait Tr { fn m(); }

			use crate::Tr::m;
			use super::*;
			use {self as me};
			use crate;
			use self::Missing::*;
			use crate::Tr::*;
			pub use self::*;

			mod inner {
				use super::super::Nothing;
				pub(in crate::nowhere) fn f() {}
			}
			"#,
		)
		.dep("dep", "dep"),
	]);

	let resolver = Resolver::new(&ws);
	let root = root(&ws, "t");

	assert!(resolver.bindings(root, "_", Namespace::Type).is_empty());
	assert!(
		resolver.bindings(root, "m", Namespace::Value).is_empty(),
		"trait items cannot be imported"
	);
	assert!(resolver.bindings(root, "me", Namespace::Type).is_empty());
	assert!(resolver.bindings(root, "crate", Namespace::Type).is_empty());
	assert_eq!(
		resolve_in(&resolver, find(&ws, "t::inner"), "dep_macro", Namespace::Macro),
		["dep::dep_macro"]
	);
	assert_eq!(resolve_in(&resolver, root, "dep::Thing", Namespace::Type), ["dep::Thing"]);

	let mut unresolved: Vec<String> = resolver
		.unresolved_imports()
		.iter()
		.map(|&import| ws.item(import).import_info().unwrap().path.to_string())
		.collect();

	unresolved.sort();
	assert_eq!(unresolved, ["", "crate::Tr::m", "self::Missing", "super", "super::super::Nothing"]);

	// an unresolvable `pub(in path)` is treated as `pub(crate)`
	assert_eq!(resolver.visibility_scope(find(&ws, "t::inner::f")), Some(root));
}

#[test]
fn named_imports_shadow_globs_only_in_the_namespaces_they_bind() {
	// rustc: `reader::util` is the module `real::util` and the function `fns::util`; `reader::Thing` is the struct
	// `real::Thing` and the constant `real_val::Thing` (`decoy`'s glob bindings are shadowed in `m`)
	let ws = single(
		r#"
		pub mod reader { pub use crate::m::util; pub use crate::m::Thing; }
		pub mod decoy { pub mod util { pub struct Decoy; } pub struct Thing; }
		pub mod real { pub mod util { pub struct Real; } pub struct Thing {} }
		pub mod fns { pub fn util() {} }
		pub mod real_val { pub const Thing: u8 = 0; }
		pub mod b { pub use crate::real::*; pub use crate::fns::util; pub use crate::real_val::Thing; }
		pub mod m { pub use crate::decoy::*; pub use crate::b::util; pub use crate::b::Thing; }
		"#,
	);

	let resolver = Resolver::new(&ws);
	let reader = find(&ws, "t::reader");
	let m = find(&ws, "t::m");

	for module in [reader, m] {
		assert_eq!(show_bindings(&resolver, module, "util", Namespace::Type), ["t::real::util (import)"]);
		assert_eq!(show_bindings(&resolver, module, "util", Namespace::Value), ["t::fns::util (import)"]);
		assert_eq!(show_bindings(&resolver, module, "Thing", Namespace::Type), ["t::real::Thing (import)"]);
		assert_eq!(
			show_bindings(&resolver, module, "Thing", Namespace::Value),
			["t::real_val::Thing (import)"]
		);
	}

	assert!(resolve_in(&resolver, reader, "util::Decoy", Namespace::Type).is_empty());
	assert_eq!(resolve_in(&resolver, reader, "util::Real", Namespace::Type), ["t::real::util::Real"]);
	assert!(resolver.unresolved_imports().is_empty());
}

#[test]
fn named_imports_shadow_the_preludes_in_import_paths() {
	let ws = workspace([
		TestCrate::new("mylib", "pub struct Client; pub mod api { pub struct Api; }"),
		TestCrate::new(
			"app",
			r#"
			mod types { pub enum Option { Nothing, Just(u8) } }
			mod shadows { pub mod core { pub struct X; } }

			mod user {
				use Option::Just as J;
				use crate::types::Option;
				use core::X;
				use crate::shadows::core;
			}

			mod m {
				use mylib::*;
				use crate::shim::mylib;
			}

			mod shim { pub use crate::shim2::mylib; }
			mod shim2 { pub mod mylib { pub struct Local; } }
			"#,
		)
		.dep("mylib", "mylib"),
		TestCrate::new(
			"old",
			r#"
			mod user { use core::X; }
			mod shadows { pub mod core { pub struct X; } }
			pub use shadows::core;
			"#,
		)
		.edition(Edition::E2015),
	]);

	let resolver = Resolver::new(&ws);
	let user = find(&ws, "app::user");
	let m = find(&ws, "app::m");

	for namespace in [Namespace::Type, Namespace::Value] {
		assert_eq!(show_bindings(&resolver, user, "J", namespace), ["app::types::Option::Just (import)"]);
		assert_eq!(show_bindings(&resolver, user, "X", namespace), ["app::shadows::core::X (import)"]);
		assert_eq!(
			show_bindings(&resolver, find(&ws, "old::user"), "X", namespace),
			["old::shadows::core::X (import)"]
		);
	}

	assert!(resolver.bindings(user, "J", Namespace::Macro).is_empty());
	assert_eq!(resolver.names(m, Namespace::Type), ["Local", "mylib"]);
	assert_eq!(
		show_res(&resolver, &resolver.import_targets(find_import(&ws, m, "mylib::*"))),
		["app::shim2::mylib"]
	);
	assert!(resolver.unresolved_imports().is_empty());
}

#[test]
fn nested_imports_with_self_super_and_crate() {
	let ws = single(
		r#"
		pub mod a {
			pub mod b {
				pub struct S;
				pub fn g() {}
			}

			pub use self::b::S;
			pub(crate) use super::top as renamed;
		}

		fn top() {}

		mod c {
			use super::a::b::{self, g};
			use crate::a::S as AS;
			use super::a::renamed;
		}
		"#,
	);

	let resolver = Resolver::new(&ws);
	let c = find(&ws, "t::c");

	assert_eq!(show_bindings(&resolver, c, "b", Namespace::Type), ["t::a::b (import)"]);
	assert!(resolver.bindings(c, "b", Namespace::Value).is_empty(), "`self` imports bind only types");
	assert_eq!(show_bindings(&resolver, c, "g", Namespace::Value), ["t::a::b::g (import)"]);
	assert_eq!(show_bindings(&resolver, c, "AS", Namespace::Type), ["t::a::b::S (import)"]);
	assert_eq!(show_bindings(&resolver, c, "AS", Namespace::Value), ["t::a::b::S (import)"]);
	assert_eq!(show_bindings(&resolver, c, "renamed", Namespace::Value), ["t::top (import)"]);
	assert_eq!(resolve(&resolver, "crate::a::renamed"), ["t::top"]);
	assert_eq!(resolve(&resolver, "a::S"), ["t::a::b::S"]);
}

#[test]
fn out_of_line_modules_and_path_attributes() {
	let ws = workspace([TestCrate::new("t", "mod a; #[path = \"other/c.rs\"] mod c; pub use a::b::Deep;")
		.file("a.rs", "pub mod b;")
		.file("a/b.rs", "pub struct Deep;")
		.file("other/c.rs", "mod d; pub use self::d::D;")
		.file("other/d.rs", "pub struct D;")]);

	let resolver = Resolver::new(&ws);

	assert_eq!(resolve(&resolver, "crate::Deep"), ["t::a::b::Deep"]);
	assert_eq!(resolve(&resolver, "crate::c::D"), ["t::c::d::D"]);
}

/// A [`PathRef`] as written in code.
fn path_ref(text: &str) -> PathRef {
	let (leading_colon, rest) = match text.strip_prefix("::") {
		Some(rest) => (true, rest),
		None => (false, text),
	};

	PathRef {
		leading_colon,
		segments: rest
			.split("::")
			.map(|name| PathSegmentRef {
				name: name.trim_start_matches("r#").into(),
				range: TextRange::default(),
				has_arguments: false,
			})
			.collect(),
	}
}

#[test]
#[ignore = "debugging aid"]
fn print_unresolved_imports() {
	let ws = workspace(real_crates());
	let resolver = Resolver::new(&ws);

	for &import in resolver.unresolved_imports() {
		let path = ws.item(import).import_info().map(|info| info.path.to_string()).unwrap_or_default();

		println!(
			"unresolved {} {path} (in {})",
			ws.krate(import.krate()).name(),
			show(&resolver, ws.module_of(import))
		);
	}
}

/// Each module decides on its own whether a path names items only through its private imports: another crate's item of
/// the same name (a binary's, whose root is also `crate`) does not make the library's import name its own item.
#[test]
fn private_imports_are_judged_per_module() {
	let ws = workspace([
		TestCrate::new("lib", "mod shapes { pub struct Foo; } use shapes::Foo;"),
		TestCrate::new("bin", "struct Foo;"),
	]);
	let resolver = Resolver::new(&ws);
	let path = item_path("crate::Foo");
	let mut items = resolver.resolve_item_path(&path);

	assert_eq!(items.len(), 2);
	assert!(crate::edit::check_private_imports(&resolver, &path, &mut items).is_err());
}

/// For edits, a plain path that names items only through private imports of its module is ambiguous (it could mean
/// the imports, `use` paths); but the path of every item names it, whatever the imports of the same name.
#[test]
fn private_imports_leave_every_item_its_path() {
	let ws = single(concat!(
		"pub mod alt { pub struct Qux; pub mod inner {} } ",
		"#[cfg(feature = \"a\")] pub struct Qux; ",
		"#[cfg(not(feature = \"a\"))] use alt::Qux; ",
		"pub fn helper() {} use alt::inner as helper; ",
		"mod shapes { pub struct Deep; pub struct Bar; } ",
		"use shapes::Bar as Private; ",
		"pub(crate) use shapes::Deep as CrateDeep; ",
		"pub mod m { pub(in crate::m) use super::shapes::Bar as InBar; pub(crate) use super::shapes::Bar as CrateBar; }",
	));
	let resolver = Resolver::new(&ws);
	let for_edits = |path: &str| {
		let path = item_path(path);
		let mut items = resolver.resolve_item_path(&path);

		crate::edit::check_private_imports(&resolver, &path, &mut items)
			.map(|()| items.iter().map(|&item| show(&resolver, item)).collect::<Vec<_>>())
			.map_err(|error| error.to_string())
	};

	// the module's own items, under other `cfg`s or in another namespace
	assert_eq!(for_edits("crate::Qux"), Ok(vec!["t::Qux".to_owned()]));
	assert_eq!(for_edits("crate::helper"), Ok(vec!["t::helper".to_owned()]));

	// imports no more visible than their module
	for path in ["crate::Private", "crate::CrateDeep", "crate::m::InBar"] {
		assert!(for_edits(path).is_err_and(|error| error.contains("is ambiguous")), "{path}");
	}

	// a re-export is a path to what it exports
	assert_eq!(for_edits("crate::m::CrateBar"), Ok(vec!["t::shapes::Bar".to_owned()]));
	assert_eq!(canonical_path_round_trip_failures(&ws, &resolver), Vec::<String>::new());
}

#[test]
fn proc_macros_are_bound_by_the_names_they_define() {
	let ws = workspace([
		TestCrate::new(
			"my_derive",
			r#"
			extern crate proc_macro;

			use proc_macro::TokenStream;

			/// Derives `MyDerive`.
			#[proc_macro_derive(MyDerive, attributes(my))]
			pub fn derive_my_derive(input: TokenStream) -> TokenStream { input }

			#[proc_macro]
			pub fn make(input: TokenStream) -> TokenStream { input }

			#[proc_macro_attribute]
			pub fn route(_: TokenStream, input: TokenStream) -> TokenStream { input }

			fn helper() {}
			"#,
		)
		.kind(TargetKind::ProcMacro),
		TestCrate::new("app", "use my_derive::{MyDerive, make, route};").dep("my_derive", "my_derive"),
	]);

	let resolver = Resolver::new(&ws);
	let app = root(&ws, "app");

	assert_eq!(
		show_bindings(&resolver, app, "MyDerive", Namespace::Macro),
		["my_derive::derive_my_derive (import)"]
	);
	assert_eq!(show_bindings(&resolver, app, "make", Namespace::Macro), ["my_derive::make (import)"]);
	assert_eq!(show_bindings(&resolver, app, "route", Namespace::Macro), ["my_derive::route (import)"]);
	// (`TokenStream` is imported from outside of the loaded crates: its namespace is unknown)
	assert_eq!(
		resolver.names(root(&ws, "my_derive"), Namespace::Macro),
		["MyDerive", "TokenStream", "make", "route"]
	);
	assert!(resolver.unresolved_imports().is_empty());
}

#[test]
fn pub_in_paths_name_the_enclosing_cfg_variant() {
	let ws = single(
		r#"
		#[cfg(unix)]
		pub mod sys {
			pub mod inner { pub(in crate::sys) fn helper() {} }
			use self::inner::helper as h;
		}

		#[cfg(windows)]
		pub mod sys {
			pub mod inner { pub(in crate::sys) fn helper() {} }
			use self::inner::helper as h;
		}
		"#,
	);

	let resolver = Resolver::new(&ws);
	let variants = find_all(&ws, "t::sys");

	assert_eq!(variants.len(), 2);

	for sys in variants {
		let inner = ws.children(sys).find(|&child| ws.item(child).name.as_deref() == Some("inner")).unwrap();
		let helper = ws.children(inner).next().unwrap();
		let h: Vec<Res> = resolver
			.bindings(sys, "h", Namespace::Value)
			.into_iter()
			.map(|binding| binding.res)
			.collect();

		assert!(resolver.is_visible_from(helper, sys));
		assert_eq!(resolver.visibility_scope(helper), Some(sys));
		assert_eq!(h, [Res::Item(helper)]);
		assert_eq!(
			resolver.usable_paths(helper, Viewpoint::Module(sys)),
			["h", "crate::sys::h", "crate::sys::inner::helper"]
		);
	}
}

#[test]
fn qualified_paths_name_traits_and_types_by_their_last_segments() {
	let ws = single(
		r#"
		pub mod shapes {
			pub trait Shape { fn area(&self) -> f64; }
			pub struct Circle;
			pub struct Square;
			impl Shape for Circle { fn area(&self) -> f64 { 0.0 } }
			impl Shape for Square { fn area(&self) -> f64 { 1.0 } }
		}

		pub mod other {
			pub trait Shape { fn area(&self); }
			impl self::Shape for crate::shapes::Circle { fn area(&self) {} }
		}

		pub use shapes::Circle;
		"#,
	);

	let resolver = Resolver::new(&ws);

	// neither `Shape` nor `Square` is at the crate root: impls whose trait or type ends with them match (and so the
	// first path names both, which its selector tells apart)
	assert_eq!(
		resolve(&resolver, "<Circle as Shape>::area"),
		["<t::shapes::Circle as Shape>[1]::area", "<t::shapes::Circle as self::Shape>::area"]
	);
	assert_eq!(resolve(&resolver, "<Square as Shape>::area"), ["<t::shapes::Square as Shape>::area"]);
	assert_eq!(resolve(&resolver, "<shapes::Square as Shape>"), ["impl Shape for t::shapes::Square"]);
	assert_eq!(
		resolve(&resolver, "<Circle as shapes::Shape>::area"),
		["<t::shapes::Circle as Shape>[1]::area"]
	);
	assert_eq!(
		resolve(&resolver, "<Circle as other::Shape>::area"),
		["<t::shapes::Circle as self::Shape>::area"]
	);
	assert_eq!(
		resolve(&resolver, "<Circle as t::other::Shape>::area"),
		["<t::shapes::Circle as self::Shape>::area"]
	);
	assert!(resolve(&resolver, "<Circle as crate::Shape>").is_empty(), "anchored paths must resolve");
	assert!(resolve(&resolver, "<Circle as ape>").is_empty(), "segments match whole");
	assert!(resolve(&resolver, "<crate::Square as Shape>").is_empty());
	assert!(resolve(&resolver, "<Circle>").is_empty());
}

#[test]
fn qualified_paths_select_impls() {
	let ws = single(
		r#"
		use std::fmt;

		pub struct Foo;

		impl fmt::Display for Foo { fn fmt() {} }
		impl fmt::Debug for Foo { fn fmt() {} }

		pub trait Tr { fn m(); }

		impl Tr for Foo { fn m() {} }
		impl Foo { fn new() {} }
		impl Tr for u8 { fn m() {} }
		"#,
	);

	let resolver = Resolver::new(&ws);

	assert_eq!(resolve(&resolver, "<Foo as Display>::fmt"), ["<t::Foo as fmt::Display>::fmt"]);
	assert_eq!(resolve(&resolver, "<Foo as fmt::Debug>::fmt"), ["<t::Foo as fmt::Debug>::fmt"]);
	assert_eq!(
		resolve(&resolver, "<crate::Foo as std::fmt::Display>::fmt"),
		["<t::Foo as fmt::Display>::fmt"]
	);
	assert_eq!(resolve(&resolver, "<Foo as Tr>::m"), ["<t::Foo as Tr>::m"]);
	assert_eq!(resolve(&resolver, "<Foo as crate::Tr>"), ["impl Tr for t::Foo"]);
	assert_eq!(resolve(&resolver, "<Foo>::new"), ["t::Foo::new"]);
	assert_eq!(resolve(&resolver, "<Foo>"), ["impl t::Foo"]);
	assert_eq!(resolve(&resolver, "<u8 as Tr>::m"), ["t::<impl Tr for u8>::m"]);
	assert!(resolve(&resolver, "<Foo as Missing>").is_empty());
	assert!(resolve(&resolver, "<Foo as Tr>::missing").is_empty());
	assert_eq!(
		resolve(&resolver, "Foo::fmt"),
		["<t::Foo as fmt::Display>::fmt", "<t::Foo as fmt::Debug>::fmt"]
	);
}

#[test]
fn readers_see_every_cfg_variant_of_named_imports() {
	let ws = single(
		r#"
		mod reader { pub use crate::m::Thing; }
		mod glob_reader { pub use crate::globbed::Thing; }
		mod globbed { pub use crate::m::*; }

		mod m {
			#[cfg(unix)]
			pub use crate::chain::Thing;

			#[cfg(windows)]
			pub use crate::windows::Thing;
		}

		mod chain { pub use crate::unix::Thing; }
		mod unix { pub struct Thing; }
		mod windows { pub struct Thing; }
		"#,
	);

	let resolver = Resolver::new(&ws);

	for module in ["t::reader", "t::glob_reader"] {
		let mut bindings = show_bindings(&resolver, find(&ws, module), "Thing", Namespace::Value);

		bindings.sort();
		assert_eq!(bindings, ["t::unix::Thing (import)", "t::windows::Thing (import)"], "{module}");
	}
}

/// The crates of this repository, and syn's source from the cargo registry at the version `Cargo.lock` locks (when
/// present, unselected).
fn real_crates() -> Vec<TestCrate> {
	let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
	let crates_dir = manifest_dir.parent().expect("crates directory");
	let mut crates = Vec::new();

	for entry in std::fs::read_dir(crates_dir).expect("crates directory").flatten() {
		let name = entry.file_name().to_string_lossy().replace('-', "_");

		for root in ["src/lib.rs", "src/main.rs"] {
			let path = entry.path().join(root);

			if path.exists() {
				let krate = TestCrate::on_disk(&name, path).dep("rscode", "rscode");

				crates.push(krate.dep("rscode_fmt", "rscode_fmt").dep("rscode_sort", "rscode_sort"));
			}
		}
	}

	if let Some(syn) = registry_crate("syn") {
		crates.push(
			TestCrate::on_disk("syn", syn.join("src/lib.rs"))
				.dep("proc_macro2", "proc_macro2")
				.dep("quote", "quote")
				.unselected(),
		);
	}

	crates
}

#[test]
fn reexport_chains_resolve_to_definitions() {
	let ws = single(
		r#"
		mod a {
			pub mod b { pub struct Deep; }
			pub use self::b::Deep as Renamed;
		}

		pub use a::Renamed;

		pub mod c {
			pub use crate::Renamed as Again;
			pub use super::d::Forward;
		}

		pub mod d {
			pub use crate::c::Again as Forward;
		}
		"#,
	);

	let resolver = Resolver::new(&ws);
	let deep = find(&ws, "t::a::b::Deep");

	assert_eq!(resolve(&resolver, "crate::c::Again"), ["t::a::b::Deep"]);
	assert_eq!(resolve(&resolver, "c::Forward"), ["t::a::b::Deep"]);
	assert_eq!(resolve(&resolver, "::t::d::Forward"), ["t::a::b::Deep"]);

	let importers: Vec<String> = resolver.imports_of(deep).iter().map(|&import| show(&resolver, import)).collect();

	assert_eq!(
		importers,
		["t::a::Renamed", "t::Renamed", "t::c::Again", "t::c::Forward", "t::d::Forward"]
	);
}

fn resolve(resolver: &Resolver<'_>, path: &str) -> Vec<String> {
	show_all(resolver, &resolver.resolve_item_path(&item_path(path)))
}

fn resolve_in(resolver: &Resolver<'_>, module: ItemId, path: &str, namespace: Namespace) -> Vec<String> {
	show_res(resolver, &resolver.resolve_path(module, &path_ref(path), namespace))
}

#[test]
fn resolver_is_send_and_sync() {
	assert_send_sync::<Resolver<'static>>();
}

#[test]
#[ignore = "slow; needs large crates in the cargo registry"]
fn robustness_on_large_registry_crates() {
	// dependencies of this workspace, at the versions `Cargo.lock` locks
	let crates = [
		("cargo", "src/lib.rs"),
		("gix", "src/lib.rs"),
		("rustix", "src/lib.rs"),
		("clap_builder", "src/lib.rs"),
		("regex", "src/lib.rs"),
		("libc", "src/lib.rs"),
		("rmcp", "src/lib.rs"),
	];

	for (name, lib) in crates {
		let version = locked_version(name);

		let Some(dir) = registry_crate(name) else {
			println!("{name} {version}: not in cargo's registry, skipped");
			continue;
		};

		let root = dir.join(lib);

		assert!(root.exists(), "{name} {version} has no {lib}: update its library path");

		let started = Instant::now();
		let ws = workspace([TestCrate::on_disk(name, root)]);
		let loaded = started.elapsed();
		let started = Instant::now();
		let resolver = Resolver::new(&ws);
		let resolved = started.elapsed();
		let started = Instant::now();
		let foreign: usize = ws.crates()[0]
			.items()
			.map(|(item, _)| resolver.usable_paths(item, Viewpoint::Foreign).len())
			.sum();
		let failures = canonical_path_round_trip_failures(&ws, &resolver);

		println!(
			"{name} {version}: {} items, model {loaded:?}, resolver {resolved:?}, usable paths {:?} ({foreign}), {} unresolved imports, {} round trip failures",
			ws.crates()[0].items().count(),
			started.elapsed(),
			resolver.unresolved_imports().len(),
			failures.len()
		);

		assert_eq!(failures, Vec::<String>::new());

		// without dependencies, only imports of other crates (or of items made by macros) stay unresolved
		for &import in resolver.unresolved_imports() {
			let path = ws.item(import).import_info().map(|info| info.path.to_string()).unwrap_or_default();

			if ["crate", "self", "super"].contains(&path.split("::").next().unwrap_or_default()) {
				println!("  unresolved crate-relative import `{path}` in {}", show(&resolver, ws.module_of(import)));
			}
		}
	}
}

#[test]
fn robustness_on_real_crates() {
	let crates = real_crates();
	let count = crates.len();
	let started = Instant::now();
	let ws = workspace(crates);
	let loaded = started.elapsed();
	let started = Instant::now();
	let resolver = Resolver::new(&ws);
	let resolved = started.elapsed();
	let items: usize = ws.crates().iter().map(|krate| krate.items().count()).sum();
	let started = Instant::now();
	let mut paths = 0;

	for krate in ws.crates() {
		for (item, _) in krate.items() {
			let path = resolver.canonical_path(item);

			paths += usize::from(path.name.is_some());

			if item.index() % 17 == 0 {
				resolver.usable_paths(item, Viewpoint::Foreign);
				resolver.usable_paths(item, Viewpoint::Module(krate.root_module()));
			}
		}
	}

	println!(
		"{count} crates, {items} items: model {loaded:?}, resolver {resolved:?}, queries {:?}; {} unresolved imports",
		started.elapsed(),
		resolver.unresolved_imports().len()
	);

	assert!(paths > 0);

	// syn re-exports its API from the crate root (its AST types are defined by macros, which are invisible)
	if ws.crates().iter().any(|krate| krate.name() == "syn") {
		assert_eq!(resolve(&resolver, "::syn::Error"), ["syn::error::Error"]);
		assert_eq!(resolve(&resolver, "::syn::parse_file"), ["syn::parse_file"]);
		assert_eq!(resolve(&resolver, "::syn::visit::Visit"), ["syn::gen::visit::Visit"]);
	}

	assert!(!resolve(&resolver, "::rscode::Resolver").is_empty());
	assert_eq!(canonical_path_round_trip_failures(&ws, &resolver), Vec::<String>::new());
}

/// The root module of the crate named `name`.
fn root(ws: &Workspace, name: &str) -> ItemId {
	ws.crates()
		.iter()
		.find(|krate| krate.name() == name)
		.expect("no such crate")
		.root_module()
}

fn segments(text: &str) -> Vec<SmolStr> {
	text.split("::")
		.filter(|segment| !segment.is_empty())
		.map(|segment| segment.trim_start_matches("r#").into())
		.collect()
}

#[test]
fn selectors_tell_impl_blocks_with_the_same_header_apart() {
	let ws = single(
		r#"
		pub struct Tools;
		pub trait Tr { fn f(); }

		#[tool_router(router = edit)]
		impl Tools { fn add_bots() {} fn shared() {} }

		#[tool_router(router = query)]
		#[rmcp::handler]
		impl Tools {}

		impl Tools { fn kick() {} fn shared() {} }

		#[cfg(unix)]
		impl Tr for Tools { fn f() {} }

		#[cfg(not(unix))]
		impl Tr for Tools { fn f() {} }

		pub struct Single;

		impl Single { fn new() {} }
		"#,
	);

	let resolver = Resolver::new(&ws);

	// the canonical paths of blocks that share a header carry the first selector that tells them apart
	assert_eq!(
		resolve(&resolver, "<Tools>"),
		["impl t::Tools[add_bots]", "impl t::Tools[#rmcp::handler]", "impl t::Tools[kick]"]
	);
	assert_eq!(resolve(&resolver, "<Tools as Tr>"), ["impl Tr for t::Tools[1]", "impl Tr for t::Tools[2]"]);
	assert_eq!(resolve(&resolver, "<Tools as Tr>::f"), ["<t::Tools as Tr>[1]::f", "<t::Tools as Tr>[2]::f"]);
	assert_eq!(resolve(&resolver, "<Single>"), ["impl t::Single"]);
	assert_eq!(resolve(&resolver, "Single::new"), ["t::Single::new"]);
	assert_eq!(resolve(&resolver, "Tools::kick"), ["t::Tools::kick"]);

	// selectors pick blocks by an item, an attribute (or the end of its path), or their place
	assert_eq!(resolve(&resolver, "<Tools>[shared]"), ["impl t::Tools[add_bots]", "impl t::Tools[kick]"]);
	assert_eq!(resolve(&resolver, "<Tools>[#tool_router]"), ["impl t::Tools[add_bots]", "impl t::Tools[#rmcp::handler]"]);
	assert_eq!(resolve(&resolver, "<Tools>[#handler]"), ["impl t::Tools[#rmcp::handler]"]);
	assert_eq!(resolve(&resolver, "<Tools>[#router]"), Vec::<String>::new());
	assert_eq!(resolve(&resolver, "<Tools>[3]::kick"), ["t::Tools::kick"]);
	assert_eq!(resolve(&resolver, "<Tools as Tr>[2]::f"), ["<t::Tools as Tr>[2]::f"]);
	assert!(resolve(&resolver, "<Tools>[4]").is_empty());
	assert!(resolve(&resolver, "<Tools>[add_bots]::kick").is_empty());
	assert!(resolve(&resolver, "<Single>[2]").is_empty());
	assert_eq!(canonical_path_round_trip_failures(&ws, &resolver), Vec::<String>::new());

	// a selector that picks nothing gets a hint listing the blocks
	assert_eq!(
		resolver.selector_hint(&item_path("<Tools as Tr>[3]")).as_deref(),
		Some("`<Tools as Tr>` names `impl Tr for t::Tools[1]`, `impl Tr for t::Tools[2]`")
	);
	assert_eq!(resolver.selector_hint(&item_path("<Tools as Tr>")), None);
	assert_eq!(resolver.selector_hint(&item_path("<Missing>[1]")), None);
}

#[test]
fn self_referential_imports_terminate() {
	let ws = single(
		r#"
		use std as a;
		use core as b;
		use self::b::x as a;
		use self::a::y as b;
		use self::a::z as a;
		use self::loop1 as loop2;
		use self::loop2 as loop1;
		pub use self::m::*;
		pub mod m { pub use super::*; pub use crate::m as again; }
		"#,
	);

	let resolver = Resolver::new(&ws);
	let root = root(&ws, "t");

	assert!(resolver.bindings(root, "a", Namespace::Type).len() <= 64);
	assert!(resolver.bindings(root, "loop1", Namespace::Type).is_empty());
	assert_eq!(resolve(&resolver, "crate::m::again::again::m"), ["t::m"]);

	// the shortest of the endless paths through the module re-exporting itself
	let paths = resolver.usable_paths(find(&ws, "t::m"), Viewpoint::Foreign);

	assert_eq!(paths.len(), 16);
	assert_eq!(
		paths[..6],
		["::t::m", "::t::again", "::t::m::m", "::t::again::m", "::t::m::again", "::t::again::again"]
	);
}

#[test]
fn self_super_paths() {
	let ws = single("pub mod a { pub fn x() {} pub mod b { use self::super::x as y; use self::super::super::a::x as z; } }");
	let resolver = Resolver::new(&ws);
	let b = find(&ws, "t::a::b");

	assert_eq!(show_bindings(&resolver, b, "y", Namespace::Value), ["t::a::x (import)"]);
	assert_eq!(show_bindings(&resolver, b, "z", Namespace::Value), ["t::a::x (import)"]);
	assert_eq!(resolve_in(&resolver, b, "self::super::x", Namespace::Value), ["t::a::x"]);
	assert_eq!(resolve_in(&resolver, b, "self::super", Namespace::Type), ["t::a"]);
	assert!(resolve_in(&resolver, b, "self::super::super::super", Namespace::Type).is_empty());
	assert!(resolver.unresolved_imports().is_empty());
}

/// Whether `item` is an associated item of an enum that a variant of the same name shadows in its namespace (`E::A`
/// then names the variant; the associated item is only reachable through its `impl`, as `<E>::A`).
fn shadowed_by_variant(ws: &Workspace, resolver: &Resolver<'_>, item: ItemId) -> bool {
	let data = ws.item(item);

	let Some(impl_block) = ws.parent(item).filter(|&parent| ws.item(parent).kind == ItemKind::Impl) else {
		return false;
	};

	resolver
		.impl_self_types(impl_block)
		.into_iter()
		.filter(|&owner| ws.item(owner).kind == ItemKind::Enum)
		.any(|owner| {
			ws.children(owner).any(|variant| {
				let variant_data = ws.item(variant);

				variant_data.name == data.name
					&& names::namespaces(variant_data)
						.iter()
						.any(|namespace| names::namespaces(data).contains(namespace))
			})
		})
}

/// Formats a canonical path like its `Display` (which lives elsewhere).
fn show(resolver: &Resolver<'_>, item: ItemId) -> String {
	let path = resolver.canonical_path(item);
	let segments = path.segments.join("::");
	let name = path.name.as_deref().unwrap_or("?");

	match (path.is_impl, &path.unresolved_self_ty, &path.impl_trait) {
		(true, None, Some(trait_text)) => format!("impl {trait_text} for {segments}{}", show_selector(&path)),
		(true, None, None) => format!("impl {segments}{}", show_selector(&path)),
		(true, Some(ty), Some(trait_text)) => format!("{segments}::<impl {trait_text} for {ty}>"),
		(true, Some(ty), None) => format!("{segments}::<impl {ty}>"),
		(false, None, Some(trait_text)) => format!("<{segments} as {trait_text}>{}::{name}", show_selector(&path)),
		(false, None, None) if segments.is_empty() => name.to_owned(),
		(false, None, None) if path.is_field => format!("{segments}.{name}"),
		(false, None, None) if path.is_macro_call => format!("{segments}::{name}!{}", show_selector(&path)),
		(false, None, None) => format!("{segments}::{name}"),
		(false, Some(ty), Some(trait_text)) => format!("{segments}::<impl {trait_text} for {ty}>::{name}"),
		(false, Some(ty), None) => format!("{segments}::<impl {ty}>::{name}"),
	}
}

fn show_all(resolver: &Resolver<'_>, items: &[ItemId]) -> Vec<String> {
	items.iter().map(|&item| show(resolver, item)).collect()
}

/// Bindings as `path`, with ` (import)` or ` (glob)` for imported ones.
fn show_bindings(resolver: &Resolver<'_>, module: ItemId, name: &str, namespace: Namespace) -> Vec<String> {
	resolver
		.bindings(module, name, namespace)
		.iter()
		.map(|binding| {
			let res = show_res(resolver, std::slice::from_ref(&binding.res)).remove(0);

			match (binding.glob, binding.import) {
				(true, _) => format!("{res} (glob)"),
				(false, Some(_)) => format!("{res} (import)"),
				(false, None) => res,
			}
		})
		.collect()
}

fn show_res(resolver: &Resolver<'_>, res: &[Res]) -> Vec<String> {
	res.iter()
		.map(|res| match res {
			Res::Item(item) => show(resolver, *item),
			Res::External(path) => format!("ext {path}"),
			Res::Builtin(name) => format!("builtin {name}"),
		})
		.collect()
}

/// Formats the selector of a canonical path (`[name]`, `[#attribute]`, `[2]`), if any.
fn show_selector(path: &CanonicalPath) -> String {
	match &path.selector {
		None => String::new(),
		Some(Selector::Item(name)) => format!("[{name}]"),
		Some(Selector::Attribute(attribute)) => format!("[#{attribute}]"),
		Some(Selector::Index(index)) => format!("[{index}]"),
	}
}

fn single(source: &str) -> Workspace {
	workspace([TestCrate::new("t", source)])
}

#[test]
fn trait_items_and_their_counterparts() {
	let ws = single(
		r#"
		pub trait Tr {
			fn m(&self);
			const C: u8;
			type A;
			fn other();
		}

		pub struct X;
		pub struct Y;

		impl Tr for X { fn m(&self) {} const C: u8 = 0; type A = u8; fn other() {} }
		impl Tr for Y { fn m(&self) {} const C: u8 = 1; type A = u16; fn other() {} }
		impl X { fn m(&self) {} }
		"#,
	);

	let resolver = Resolver::new(&ws);
	let trait_m = find(&ws, "t::Tr::m");
	let x_m = find_in_impl(&ws, "t", "impl Tr for X", "m");
	let inherent_m = find_in_impl(&ws, "t", "impl X", "m");

	assert_eq!(
		show_all(&resolver, &resolver.trait_counterparts(trait_m)),
		["<t::X as Tr>::m", "<t::Y as Tr>::m"]
	);
	assert_eq!(show_all(&resolver, &resolver.trait_counterparts(x_m)), ["t::Tr::m"]);
	assert!(resolver.trait_counterparts(inherent_m).is_empty());
	assert_eq!(
		show_all(&resolver, &resolver.trait_counterparts(find(&ws, "t::Tr::A"))),
		["<t::X as Tr>::A", "<t::Y as Tr>::A"]
	);

	let x = find(&ws, "t::X");

	assert_eq!(
		show_all(&resolver, &resolver.associated_items(x)),
		["t::X::m", "<t::X as Tr>::m", "<t::X as Tr>::C", "<t::X as Tr>::A", "<t::X as Tr>::other"]
	);
	assert_eq!(
		show_all(&resolver, &resolver.associated_items(find(&ws, "t::Tr"))),
		["t::Tr::m", "t::Tr::C", "t::Tr::A", "t::Tr::other"]
	);
	assert_eq!(resolve(&resolver, "crate::X::m"), ["<t::X as Tr>::m", "t::X::m"]);
	assert_eq!(resolve(&resolver, "X::C"), ["<t::X as Tr>::C"]);
	assert_eq!(resolve(&resolver, "crate::Tr::m"), ["t::Tr::m"]);
	assert_eq!(resolve_in(&resolver, root(&ws, "t"), "X::A", Namespace::Type), ["<t::X as Tr>::A"]);
	assert!(resolve_in(&resolver, root(&ws, "t"), "X::A", Namespace::Value).is_empty());
}

#[test]
fn unresolvable_named_imports_stop_blocking_globs() {
	let ws = single(
		r#"
		mod m { pub use crate::src::*; pub use crate::nowhere::Thing; }
		mod src { pub struct Thing; }
		mod reader { pub use crate::m::Thing; }
		"#,
	);

	let resolver = Resolver::new(&ws);

	assert_eq!(resolve(&resolver, "crate::reader::Thing"), ["t::src::Thing"]);
	assert_eq!(show_all(&resolver, resolver.unresolved_imports()), ["t::m::Thing"]);
}

#[test]
fn unselected_crates_are_only_reachable_by_name() {
	let ws = workspace([
		TestCrate::new("dep", "pub struct Thing;").unselected(),
		TestCrate::new("main", "pub struct Thing;"),
	]);
	let resolver = Resolver::new(&ws);

	assert_eq!(resolve(&resolver, "crate::Thing"), ["main::Thing"]);
	assert_eq!(resolve(&resolver, "Thing"), ["main::Thing"]);
	assert_eq!(resolve(&resolver, "::dep::Thing"), ["dep::Thing"]);
	assert_eq!(resolve(&resolver, "dep::Thing"), ["dep::Thing"]);
}

#[test]
fn usable_paths_across_crates() {
	let ws = workspace([
		TestCrate::new(
			"mylib",
			r#"
			pub mod api { pub struct Client; }
			pub use api::Client;
			mod hidden { pub struct Secret; }
			"#,
		),
		TestCrate::new(
			"app",
			r#"
			use mylib::api;
			use renamed::Client as C;

			mod sub {}
			"#,
		)
		.edition(Edition::E2018)
		.dep("mylib", "mylib")
		.dep("renamed", "mylib"),
		TestCrate::new("legacy", "mod sub {}").edition(Edition::E2015).dep("mylib", "mylib"),
	]);

	let resolver = Resolver::new(&ws);
	let client = find(&ws, "mylib::api::Client");
	let secret = find(&ws, "mylib::hidden::Secret");
	let app = root(&ws, "app");

	assert_eq!(
		resolver.usable_paths(client, Viewpoint::Module(app)),
		[
			"C",
			"crate::C",
			"mylib::Client",
			"renamed::Client",
			"crate::api::Client",
			"mylib::api::Client",
			"renamed::api::Client"
		]
	);
	// private imports of the crate root are visible in its descendants
	assert_eq!(
		resolver.usable_paths(client, Viewpoint::Module(find(&ws, "app::sub"))),
		[
			"crate::C",
			"mylib::Client",
			"renamed::Client",
			"crate::api::Client",
			"mylib::api::Client",
			"renamed::api::Client"
		]
	);
	// 2015 paths outside of `use` start with extern prelude names too (`::mylib` would need an `extern crate`)
	assert_eq!(
		resolver.usable_paths(client, Viewpoint::Module(find(&ws, "legacy::sub"))),
		["mylib::Client", "mylib::api::Client"]
	);
	assert!(resolver.usable_paths(secret, Viewpoint::Module(app)).is_empty());
	assert_eq!(
		resolver.usable_paths(client, Viewpoint::Foreign),
		["::mylib::Client", "::mylib::api::Client"]
	);

	assert_eq!(resolve(&resolver, "::mylib::Client"), ["mylib::api::Client"]);
	assert_eq!(resolve(&resolver, "mylib::api::Client"), ["mylib::api::Client"]);
	assert_eq!(resolve(&resolver, "::renamed::Client"), ["mylib::api::Client"]);
	assert_eq!(resolve(&resolver, "crate::C"), ["mylib::api::Client"]);
	assert_eq!(resolve(&resolver, "::mylib"), ["mylib"]);
	assert!(resolve(&resolver, "self::C").is_empty());
	assert!(resolve(&resolver, "super::C").is_empty());
}

#[test]
fn usable_paths_from_inside_and_outside() {
	let ws = single(
		r#"
		mod private {
			pub struct S;
			pub(crate) struct C;

			impl S {
				pub fn new() {}
				fn secret() {}
			}
		}

		pub use private::S;

		pub mod public {
			pub use crate::private::S as Renamed;
			pub(crate) use crate::private::C;
		}

		pub enum E { A }

		pub use E::A as ExportedA;
		"#,
	);

	let resolver = Resolver::new(&ws);
	let root = root(&ws, "t");
	let public = find(&ws, "t::public");
	let s = find(&ws, "t::private::S");
	let c = find(&ws, "t::private::C");
	let new = find_in_impl(&ws, "t::private", "impl S", "new");
	let secret = find_in_impl(&ws, "t::private", "impl S", "secret");
	let a = find(&ws, "t::E::A");

	assert_eq!(resolver.usable_paths(s, Viewpoint::Foreign), ["::t::S", "::t::public::Renamed"]);
	assert!(resolver.usable_paths(c, Viewpoint::Foreign).is_empty());
	assert_eq!(
		resolver.usable_paths(s, Viewpoint::Module(root)),
		["S", "crate::S", "crate::private::S", "crate::public::Renamed"]
	);
	assert_eq!(
		resolver.usable_paths(c, Viewpoint::Module(public)),
		["C", "crate::public::C", "crate::private::C"]
	);
	assert_eq!(
		resolver.usable_paths(new, Viewpoint::Foreign),
		["::t::S::new", "::t::public::Renamed::new"]
	);
	assert_eq!(
		resolver.usable_paths(secret, Viewpoint::Module(find(&ws, "t::private"))),
		[
			"S::secret",
			"crate::S::secret",
			"crate::private::S::secret",
			"crate::public::Renamed::secret"
		]
	);
	assert!(resolver.usable_paths(secret, Viewpoint::Module(root)).is_empty());
	assert_eq!(
		resolver.usable_paths(a, Viewpoint::Module(root)),
		["ExportedA", "E::A", "crate::ExportedA", "crate::E::A"]
	);
	assert_eq!(resolver.usable_paths(root, Viewpoint::Module(public)), ["crate"]);
	assert_eq!(resolver.usable_paths(root, Viewpoint::Foreign), ["::t"]);

	let use_item = ws.children(root).find(|&child| ws.item(child).kind == ItemKind::Use).unwrap();

	assert!(resolver.usable_paths(use_item, Viewpoint::Foreign).is_empty());
}

#[test]
fn usable_paths_through_modules_with_several_paths() {
	let ws = single(
		r#"
		pub mod very { pub mod deeply { pub mod nested { pub struct Foo; } } }
		pub use very::deeply::nested as n;
		pub mod a { pub struct Bar; }
		pub use a as b;
		"#,
	);

	let resolver = Resolver::new(&ws);
	let foo = find(&ws, "t::very::deeply::nested::Foo");
	let bar = find(&ws, "t::a::Bar");

	assert_eq!(
		resolver.usable_paths(foo, Viewpoint::Foreign),
		["::t::n::Foo", "::t::very::deeply::nested::Foo"]
	);
	assert_eq!(resolver.usable_paths(bar, Viewpoint::Foreign), ["::t::a::Bar", "::t::b::Bar"]);
	assert_eq!(
		resolver.usable_paths(foo, Viewpoint::Module(root(&ws, "t"))),
		["crate::n::Foo", "crate::very::deeply::nested::Foo"]
	);
	assert_eq!(
		resolver.usable_paths(find(&ws, "t::very::deeply::nested"), Viewpoint::Foreign),
		["::t::n", "::t::very::deeply::nested"]
	);

	// at most 16 paths, the shortest ones
	let aliases: String = (0..20).map(|index| format!("pub use m as m{index:02}; ")).collect();
	let ws = single(&format!("pub mod m {{ pub struct X; }} {aliases}"));
	let resolver = Resolver::new(&ws);
	let paths = resolver.usable_paths(find(&ws, "t::m::X"), Viewpoint::Foreign);

	assert_eq!(paths.len(), 16);
	assert_eq!(paths[..3], ["::t::m::X", "::t::m00::X", "::t::m01::X"]);
	assert_eq!(paths[15], "::t::m14::X");
}

#[test]
fn visibility_scopes() {
	let ws = workspace([
		TestCrate::new(
			"t",
			r#"
			pub mod a {
				pub(crate) fn krate() {}
				pub(super) fn sup() {}
				pub(in crate::a) fn in_a() {}
				fn private() {}
				pub fn public() {}

				pub mod b {
					pub(in crate::a) fn in_a2() {}
					pub(super) fn sup_b() {}
					pub(self) fn self_b() {}
				}

				pub enum E { V }

				pub trait Tr { fn m(); }

				struct S;

				impl S { fn inherent() {} }
				impl Tr for S { fn m() {} }
			}

			pub mod c {}
			"#,
		),
		TestCrate::new("u", ""),
	]);

	let resolver = Resolver::new(&ws);
	let visible = |item: &str, module: &str| resolver.is_visible_from(find(&ws, item), find(&ws, module));

	assert!(visible("t::a::krate", "t::c"));
	assert!(visible("t::a::sup", "t::c"));
	assert!(visible("t::a::in_a", "t::a::b"));
	assert!(!visible("t::a::in_a", "t::c"));
	assert!(visible("t::a::private", "t::a::b"));
	assert!(!visible("t::a::private", "t::c"));
	assert!(visible("t::a::b::in_a2", "t::a"));
	assert!(!visible("t::a::b::in_a2", "t"));
	assert!(visible("t::a::b::sup_b", "t::a"));
	assert!(!visible("t::a::b::sup_b", "t::c"));
	assert!(visible("t::a::b::self_b", "t::a::b"));
	assert!(!visible("t::a::b::self_b", "t::a"));
	assert!(visible("t::a::E::V", "u"));
	assert!(visible("t::a::Tr::m", "u"));
	assert!(visible("t::a::public", "u"));
	assert!(!visible("t::a::krate", "u"));
	assert!(!visible("t::a::S", "t::c"));

	let inherent = find_in_impl(&ws, "t::a", "impl S", "inherent");
	let trait_impl_item = find_in_impl(&ws, "t::a", "impl Tr for S", "m");

	assert!(resolver.is_visible_from(inherent, find(&ws, "t::a::b")));
	assert!(!resolver.is_visible_from(inherent, find(&ws, "t::c")));
	assert!(resolver.is_visible_from(trait_impl_item, find(&ws, "u")));

	assert_eq!(resolver.visibility_scope(find(&ws, "t::a::krate")), Some(root(&ws, "t")));
	assert_eq!(resolver.visibility_scope(find(&ws, "t::a::b::in_a2")), Some(find(&ws, "t::a")));
	assert_eq!(resolver.visibility_scope(find(&ws, "t::a::public")), None);
	assert_eq!(resolver.visibility_scope(root(&ws, "t")), None);
}
