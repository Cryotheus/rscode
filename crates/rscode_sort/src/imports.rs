//! Ordering of `use` items like rustfmt orders them, in each style edition.
//!
//! Ported from rustfmt's `src/imports.rs` (`UseTree::from_ast`, `UseTree::normalize`, and the `Ord` impls of
//! `UseSegment` and `UseTree`; rustfmt is licensed MIT OR Apache-2.0). Only the comparison is replicated:
//! the items themselves are never rewritten.

use crate::StyleEdition;
use crate::version::rustfmt_version_cmp;
use std::cmp::Ordering;
use syn::ext::IdentExt;

/// The sort key of a `use` item: its normalized use tree.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct UseKey {
	path: Vec<Segment>,

	/// Whether names are compared by version sorting (style edition 2024), rather than by case, then by bytes.
	version_sorting: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum Segment {
	/// An identifier (prefixed with `::` when it starts a global path) and its `as` rename.
	Ident(String, Option<String>),

	/// `self`, with its `as` rename.
	SelfValue(Option<String>),

	/// `super`, with its `as` rename.
	Super(Option<String>),

	/// `crate`, with its `as` rename.
	Crate(Option<String>),

	/// `*`
	Glob,

	/// `{..}`
	List(Vec<UseKey>),
}

impl UseKey {
	/// The key of a `use` item, ordered like rustfmt orders them in `style_edition`.
	pub(crate) fn new(item: &syn::ItemUse, style_edition: StyleEdition) -> Self {
		let version_sorting = style_edition >= StyleEdition::E2024;
		let mut path = Vec::new();

		push_tree(&mut path, &item.tree, item.leading_colon.is_some(), version_sorting);

		// top-level items always have a visibility (possibly inherited) in rustfmt
		Self { path, version_sorting }.normalize(true, !item.attrs.is_empty())
	}

	fn nested(tree: &syn::UseTree, version_sorting: bool) -> Self {
		let mut path = Vec::new();

		push_tree(&mut path, tree, false, version_sorting);

		Self { path, version_sorting }
	}

	/// Whether the tree is `self` (with or without a rename), which rustfmt detects by printing the tree.
	fn is_self(&self) -> bool {
		matches!(self.path.as_slice(), [Segment::SelfValue(_)])
	}

	/// rustfmt's `UseTree::normalize`: `a::self` → `a`, `a::self as b` → `a as b`, `a::{b}` → `a::b`,
	/// and nested lists are normalized, sorted, and deduplicated.
	fn normalize(mut self, top_level: bool, has_attrs: bool) -> Self {
		let Some(mut last) = self.path.pop() else {
			return self;
		};

		// rustfmt removes `a::{}` and `use self;` without attributes
		if !has_attrs {
			match &last {
				Segment::List(list) if list.is_empty() => {
					self.path.clear();
					return self;
				}
				Segment::SelfValue(None) if self.path.is_empty() && top_level => {
					self.path.clear();
					return self;
				}
				_ => {}
			}
		}

		// `a::self` → `a`
		if let Segment::SelfValue(None) = last
			&& let Some(second_last) = self.path.last()
			&& !matches!(second_last, Segment::SelfValue(_))
		{
			return self;
		}

		// `a::self as b` → `a as b`
		if let Segment::SelfValue(Some(rename)) = &last
			&& let Some(Segment::Ident(_, old_rename @ None)) = self.path.last_mut()
		{
			*old_rename = Some(rename.clone());
			return self;
		}

		// `a::{b}` → `a::b`
		if let Segment::List(list) = &mut last
			&& list.len() == 1
			&& !list[0].is_self()
			&& let Some(single) = list.pop()
		{
			self.path.extend(single.path);
			return self.normalize(top_level, has_attrs);
		}

		if let Segment::List(list) = last {
			let mut list: Vec<Self> = list.into_iter().map(|tree| tree.normalize(false, false)).collect();

			list.sort_by(Self::rustfmt_cmp);
			list.dedup();
			last = Segment::List(list);
		}

		self.path.push(last);
		self
	}

	/// rustfmt's `Ord for UseTree`: segments are compared in order, skipping differences only in renames, then the
	/// shorter path sorts first.
	fn rustfmt_cmp(&self, other: &Self) -> Ordering {
		let version_sorting = self.version_sorting;

		for (a, b) in self.path.iter().zip(&other.path) {
			let ordering = a.rustfmt_cmp(b, version_sorting);

			if ordering.is_ne() && a.without_rename().rustfmt_cmp(&b.without_rename(), version_sorting).is_ne() {
				return ordering;
			}
		}

		self.path.len().cmp(&other.path.len())
	}
}

impl Ord for UseKey {
	fn cmp(&self, other: &Self) -> Ordering {
		self.rustfmt_cmp(other)
	}
}

impl PartialOrd for UseKey {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

impl Segment {
	fn from_ident(ident: &syn::Ident, global: bool, rename: Option<String>) -> Self {
		let name = ident.to_string();

		match name.as_str() {
			"self" => Self::SelfValue(rename),
			"super" => Self::Super(rename),
			"crate" => Self::Crate(rename),
			_ if global => Self::Ident(format!("::{name}"), rename),
			_ => Self::Ident(name, rename),
		}
	}

	fn rank(&self) -> u8 {
		match self {
			Self::SelfValue(_) => 0,
			Self::Super(_) => 1,
			Self::Crate(_) => 2,
			Self::Ident(..) => 3,
			Self::Glob => 4,
			Self::List(_) => 5,
		}
	}

	fn without_rename(&self) -> Self {
		match self {
			Self::Ident(name, _) => Self::Ident(name.clone(), None),
			Self::SelfValue(_) => Self::SelfValue(None),
			Self::Super(_) => Self::Super(None),
			Self::Crate(_) => Self::Crate(None),
			Self::Glob => Self::Glob,
			Self::List(list) => Self::List(list.clone()),
		}
	}

	/// rustfmt's `Ord for UseSegment`: with version sorting in style edition 2024, and by case, then by bytes, in
	/// earlier style editions.
	fn rustfmt_cmp(&self, other: &Self, version_sorting: bool) -> Ordering {
		let names = |a: &str, b: &str| match version_sorting {
			true => rustfmt_version_cmp(unraw(a), unraw(b)),
			false => a.cmp(b),
		};

		match (self, other) {
			(Self::SelfValue(a), Self::SelfValue(b))
			| (Self::Super(a), Self::Super(b))
			| (Self::Crate(a), Self::Crate(b)) => match (a, b) {
				(Some(a), Some(b)) => names(a, b),
				_ => a.cmp(b),
			},
			(Self::Glob, Self::Glob) => Ordering::Equal,
			(Self::Ident(a, a_rename), Self::Ident(b, b_rename)) => {
				let ordering = match version_sorting {
					true => rustfmt_version_cmp(unraw(a), unraw(b)),
					false => case_key(a).cmp(&case_key(b)),
				};

				ordering.then_with(|| match (a_rename, b_rename) {
					(None, None) => Ordering::Equal,
					(None, Some(_)) => Ordering::Less,
					(Some(_), None) => Ordering::Greater,
					(Some(a), Some(b)) => names(a, b),
				})
			}
			(Self::List(a), Self::List(b)) => {
				for (a, b) in a.iter().zip(b) {
					let ordering = a.rustfmt_cmp(b);

					if ordering.is_ne() {
						return ordering;
					}
				}

				a.len().cmp(&b.len())
			}
			_ => self.rank().cmp(&other.rank()),
		}
	}
}

/// rustfmt compares identifiers without their `r#` prefix in style edition 2024.
fn unraw(name: &str) -> &str {
	name.trim_start_matches("r#")
}

/// The order of identifiers before style edition 2024 (with their `r#` prefixes): `snake_case`, then `CamelCase`,
/// then `UPPER_SNAKE_CASE`, and each by bytes.
fn case_key(name: &str) -> (bool, bool, &str) {
	let upper_snake_case = name.chars().all(|c| c.is_uppercase() || c == '_' || c.is_numeric());

	(upper_snake_case, name.starts_with(char::is_uppercase), name)
}

/// Appends the segments of a use tree, like rustfmt's `UseTree::from_ast`.
///
/// `global` is set when the tree follows a leading `::` that has not been attached to a segment yet.
fn push_tree(path: &mut Vec<Segment>, tree: &syn::UseTree, global: bool, version_sorting: bool) {
	match tree {
		syn::UseTree::Path(tree) => {
			path.push(Segment::from_ident(&tree.ident, global, None));
			push_tree(path, &tree.tree, false, version_sorting);
		}
		syn::UseTree::Name(tree) => path.push(Segment::from_ident(&tree.ident, global, None)),
		syn::UseTree::Rename(tree) => {
			let rename = if tree.rename == "_" {
				Some("_".to_owned())
			} else if tree.rename.unraw() == tree.ident.unraw() {
				None
			} else {
				Some(tree.rename.to_string())
			};

			path.push(Segment::from_ident(&tree.ident, global, rename));
		}
		syn::UseTree::Glob(_) => {
			// `::*`
			if global {
				path.push(Segment::Ident(String::new(), None));
			}

			path.push(Segment::Glob);
		}
		syn::UseTree::Group(group) => {
			// `::{..}`
			if global {
				path.push(Segment::Ident(String::new(), None));
			}

			path.push(Segment::List(group.items.iter().map(|tree| UseKey::nested(tree, version_sorting)).collect()));
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn key_in(style_edition: StyleEdition, tree: &str) -> UseKey {
		UseKey::new(&syn::parse_str(&format!("use {tree};")).unwrap(), style_edition)
	}

	fn key(tree: &str) -> UseKey {
		key_in(StyleEdition::E2024, tree)
	}

	#[track_caller]
	fn assert_less_in(style_edition: StyleEdition, a: &str, b: &str) {
		let (key_a, key_b) = (key_in(style_edition, a), key_in(style_edition, b));

		assert_eq!(key_a.cmp(&key_b), Ordering::Less, "{a} < {b} ({style_edition:?})");
		assert_eq!(key_b.cmp(&key_a), Ordering::Greater, "{b} > {a} ({style_edition:?})");
	}

	#[track_caller]
	fn assert_less(a: &str, b: &str) {
		assert_less_in(StyleEdition::E2024, a, b);
	}

	#[track_caller]
	fn assert_equivalent(a: &str, b: &str) {
		assert_eq!(key(a).cmp(&key(b)), Ordering::Equal, "{a} == {b}");
	}

	#[test]
	fn normalization() {
		assert_eq!(key("a::self"), key("a"));
		assert_eq!(key("a::self as foo"), key("a as foo"));
		assert_ne!(key("a::{self}"), key("a"));
		assert_eq!(key("a::{b}"), key("a::b"));
		assert_eq!(key("a::{b::{c}}"), key("a::b::c"));
		assert_eq!(key("a::{b, c::self}"), key("a::{b, c}"));
		assert_eq!(key("a::{b as bar, c::self}"), key("a::{b as bar, c}"));
		assert_eq!(key("a::{c, b, b}"), key("a::{b, c}"));
		assert_eq!(key("a as a"), key("a"));
		assert_eq!(key("a::{}").path, Vec::new());
		assert_ne!(UseKey::new(&syn::parse_str("#[cfg(x)] use a::{};").unwrap(), StyleEdition::E2024).path, Vec::new());
	}

	#[test]
	fn ordering_of_segment_kinds() {
		for style_edition in StyleEdition::ALL.iter().copied() {
			let assert_less = |a, b| assert_less_in(style_edition, a, b);

			// rustfmt's own test cases that do not depend on the style edition
			assert_less("a", "aa");
			assert_less("a", "a::a");
			assert_less("a", "*");
			assert_less("a", "{a, b}");
			assert_less("*", "{a, b}");
			assert_less("aaaaaaaaaaaaaaa::{bb, cc, dddddddd}", "aaaaaaaaaaaaaaa::{bb, cc, ddddddddd}");
			assert_less("serde::de::{Deserialize}", "serde_json");
			assert_less("a::b::c", "a::b::*");
			assert_less("foo::{Bar, Baz}", "{Bar, Baz}");
			assert_less("foo::{qux as bar}", "foo::{self as bar}");
			assert_less("foo::{qux as bar}", "foo::{baz, qux as bar}");
			assert_less("foo::{self as bar, baz}", "foo::{baz, qux as bar}");
			assert_less("foo", "foo::Bar");
			assert_less("std::cmp::{d, c, b, a}", "std::cmp::{b, e, g, f}");

			// `self` < `super` < `crate` < identifiers
			assert_less("self::a", "super::a");
			assert_less("super::a", "crate::a");
			assert_less("crate::a", "a::a");
			assert_less("crate::z", "a");
		}
	}

	#[test]
	fn earlier_style_editions_order_by_case_then_bytes() {
		for style_edition in [StyleEdition::E2015, StyleEdition::E2018, StyleEdition::E2021] {
			let assert_less = |a, b| assert_less_in(style_edition, a, b);

			// `snake_case` < `CamelCase` < `UPPER_SNAKE_CASE` (where a single capital letter counts)
			assert_less("foo", "Foo");
			assert_less("a::alpha", "a::Zeta");
			assert_less("a::Zeta", "a::ZETA");
			assert_less("Ab", "A");
			assert_less("_b", "a");
			assert_less("_b", "A");

			// then by bytes, with `r#` prefixes
			assert_less("x10", "x9");
			assert_less("x08", "x_1");
			assert_less("r#type", "s");
			assert_less("::std::fmt", "Foo");
			assert_less("::std::fmt", "std::fmt");
		}

		// the same pairs in style edition 2024
		assert_less("Foo", "foo");
		assert_less("a::Zeta", "a::alpha");
		assert_less("x9", "x10");
		assert_less("s", "r#type");
	}

	#[test]
	fn style_edition_2024_version_sorting() {
		assert_less("A", "a");
		assert_less("_b", "A");
		assert_less("_b", "a");
		assert_less("x8", "x16");
		assert_less("a::Zeta", "a::alpha");
		assert_less("r#type", "typf");
		assert_equivalent("r#foo", "foo");

		// a leading `::` is part of the first segment
		assert_less("::std::fmt", "Foo");
		assert_less("::std::fmt", "std::fmt");
		assert_less("::*", "::a");
		assert_less("_a", "::a");
	}

	#[test]
	fn renames_are_ignored_except_as_a_last_resort() {
		// a rename sorts after the unrenamed path when it's shorter
		assert_less("a as c", "a::b");
		assert_equivalent("a as c", "a");
		assert_equivalent("a as c", "a as b");
		assert_equivalent("a::{b as c}", "a::b");
		assert_less("a::{b as z, c}", "a::{b, d}");
	}
}
