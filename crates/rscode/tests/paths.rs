//! Item paths and patterns through the public API.

use rscode::CanonicalPath;
use rscode::ItemPath;
use rscode::MatchOptions;
use rscode::PathPattern;
use rscode::path::Anchor;
use rscode::path::is_keyword;
use rscode::path::is_valid_ident;
use rscode::pattern::IdentPattern;
use rscode::pattern::SegmentPattern;

/// Canonical paths of a small crate `demo`, as the resolver would produce them.
fn demo_paths() -> Vec<CanonicalPath> {
	fn path(segments: &[&str], name: Option<&str>) -> CanonicalPath {
		CanonicalPath {
			segments: segments.iter().map(|&segment| segment.into()).collect(),
			impl_trait: None,
			self_ty_arguments: None,
			unresolved_self_ty: None,
			is_impl: false,
			is_import: false,
			name: name.map(Into::into),
		}
	}

	let widget = ["demo", "ui", "Widget"];

	vec![
		path(&[], Some("demo")),
		path(&["demo"], Some("ui")),
		path(&["demo", "ui"], Some("Widget")),
		path(&widget, Some("new")),
		CanonicalPath { impl_trait: Some("fmt::Display".into()), ..path(&widget, Some("fmt")) },
		CanonicalPath { impl_trait: Some("Debug".into()), ..path(&widget, Some("fmt")) },
		CanonicalPath { is_impl: true, impl_trait: Some("fmt::Display".into()), ..path(&widget, None) },
		CanonicalPath { is_impl: true, ..path(&widget, None) },
		path(&["demo", "ui"], Some("render")),
		path(&["demo", "ui", "Kind"], Some("Button")),
		path(&["demo", "io"], Some("type")),
		CanonicalPath {
			impl_trait: Some("Render".into()),
			unresolved_self_ty: Some("Vec<Widget>".into()),
			..path(&["demo", "ui"], Some("render"))
		},
	]
}

/// The displayed canonical paths of `demo` matched by `pattern`.
fn find(pattern: &str, options: MatchOptions) -> Vec<String> {
	let pattern = PathPattern::parse(pattern, options).unwrap_or_else(|error| panic!("{error}"));

	demo_paths().iter().filter(|path| pattern.matches(path, true)).map(ToString::to_string).collect()
}

#[test]
fn canonical_paths_display_like_rust_paths() {
	let displayed: Vec<String> = demo_paths().iter().map(ToString::to_string).collect();

	assert_eq!(
		displayed,
		[
			"demo",
			"demo::ui",
			"demo::ui::Widget",
			"demo::ui::Widget::new",
			"<demo::ui::Widget as fmt::Display>::fmt",
			"<demo::ui::Widget as Debug>::fmt",
			"impl fmt::Display for demo::ui::Widget",
			"impl demo::ui::Widget",
			"demo::ui::render",
			"demo::ui::Kind::Button",
			"demo::io::r#type",
			"demo::ui::<impl Render for Vec<Widget>>::render",
		]
	);
}

#[test]
fn finds_items_with_patterns() {
	let case = MatchOptions::default();

	assert_eq!(find("Widget", case), ["demo::ui::Widget"]);
	assert_eq!(find("widget", case), Vec::<String>::new());
	assert_eq!(find("widget", MatchOptions { ignore_case: true }), ["demo::ui::Widget"]);
	assert_eq!(find("*Widget*", case), ["demo::ui::Widget"]);
	assert_eq!(
		find("Widget::*", case),
		["demo::ui::Widget::new", "<demo::ui::Widget as fmt::Display>::fmt", "<demo::ui::Widget as Debug>::fmt"]
	);
	assert_eq!(find("render", case), ["demo::ui::render", "demo::ui::<impl Render for Vec<Widget>>::render"]);
	assert_eq!(find("crate::ui::render", case), ["demo::ui::render"]);
	assert_eq!(find("crate::ui::*", case), ["demo::ui::Widget", "demo::ui::render"]);
	// everything below `ui` but `impl` blocks
	assert_eq!(find("crate::ui::**", case).len(), 7);
	assert_eq!(find("::demo::io::*", case), ["demo::io::r#type"]);
	assert_eq!(find("r#type", case), ["demo::io::r#type"]);
	assert_eq!(find("Kind::**", case), ["demo::ui::Kind::Button"]);
	assert_eq!(find("crate", case), ["demo"]);
	assert_eq!(find("<Widget as Display>::fmt", case), ["<demo::ui::Widget as fmt::Display>::fmt"]);
	assert_eq!(find("<Widget as *>", case), ["impl fmt::Display for demo::ui::Widget"]);
	assert_eq!(find("impl Widget", case), ["impl demo::ui::Widget"]);
	assert_eq!(find("<Vec as Render>::*", case), ["demo::ui::<impl Render for Vec<Widget>>::render"]);
}

#[test]
fn patterns_round_trip_through_display() {
	let options = MatchOptions::default();

	for text in ["foo", "crate::a::*", "::dep::**::Error", "<Foo as Display>::fmt", "<*>", "*Error*", "a::**::b"] {
		let pattern = PathPattern::parse(text, options).unwrap();

		assert_eq!(pattern.to_string(), text);
		assert_eq!(PathPattern::parse(&pattern.to_string(), options).unwrap(), pattern);
	}
}

#[test]
fn builds_patterns_from_parts() {
	let options = MatchOptions::default();
	let pattern = PathPattern::from_ident(IdentPattern::starts_with("Wid", options));

	assert_eq!(pattern.anchor, Anchor::None);
	assert!(matches!(pattern.segments.as_slice(), [SegmentPattern::Ident(_)]));
	assert!(pattern.matches(&demo_paths()[2], true));
	assert!(pattern.matches_segments(&["demo", "ui", "Widget"]));
	assert!(!pattern.is_qualified());
	assert_eq!(pattern.to_item_path(), None);
	assert_eq!(
		PathPattern::parse("crate::ui::Widget", options).unwrap().to_item_path(),
		Some(ItemPath::parse("crate::ui::Widget").unwrap())
	);
}

#[test]
fn parses_item_paths() {
	let path: ItemPath = "crate::ui::Widget::new".parse().unwrap();

	assert_eq!(path.anchor, Anchor::Crate);
	assert_eq!(path.segments, ["ui", "Widget", "new"]);
	assert_eq!(path.name().map(|name| name.as_str()), Some("new"));
	assert_eq!(path.to_string(), "crate::ui::Widget::new");

	let qualified = ItemPath::parse("impl fmt::Display for crate::ui::Widget").unwrap();

	assert_eq!(qualified.to_string(), "<crate::ui::Widget as fmt::Display>");
	assert_eq!(ItemPath::parse("super::super::x").unwrap().anchor, Anchor::Super(2));
	assert_eq!(ItemPath::from_segments(["io", "type"]).to_string(), "io::r#type");

	// generic arguments tell apart `impl` blocks of one type and trait
	let qualified = ItemPath::parse("<crate::ui::Widget<u8> as From< io::Error >>::from").unwrap();

	assert_eq!(qualified.to_string(), "<crate::ui::Widget<u8> as From<io::Error>>::from");

	let error = ItemPath::parse("Vec<u8>::new").unwrap_err();

	assert_eq!(
		error.to_string(),
		"invalid path `Vec<u8>::new`: generic arguments are only supported in the type and trait of `<Type as Trait>` \
		 and `impl Trait for Type`"
	);

	let error: rscode::Error = error.into();

	assert!(matches!(error, rscode::Error::PathParse(_)));
}

#[test]
fn validates_identifiers() {
	assert!(is_keyword("gen"));
	assert!(!is_keyword("union"));
	assert!(is_valid_ident("r#gen"));
	assert!(is_valid_ident("naïve"));
	assert!(!is_valid_ident("gen"));
	assert!(!is_valid_ident("r#crate"));
}
