//! `use` items are ordered like rustfmt orders them, in style edition 2024 and before.
//!
//! Skipped (with a message) when rustfmt is not installed.

mod common;

use common::Rng;
use common::Rustfmt;
use rscode_sort::SortOptions;
use rscode_sort::Sorter;
use rscode_sort::StyleEdition;

/// Varied `use` items; no two of them compare equal in rustfmt (which would keep their input order).
const USES: &[&str] = &[
	"use std::collections::HashMap;",
	"use std::collections::{BTreeMap, hash_map};",
	"use std::fmt;",
	"use std::fmt::Display;",
	"use std::fmt::{self, Debug};",
	"use std::io::{self};",
	"use std::io::Write as _;",
	"use std::io as stdio;",
	"use crate::a::b;",
	"use crate::A;",
	"use crate::{c, d};",
	"use self::x::y;",
	"use self::X;",
	"use super::z;",
	"use super::super::w;",
	"use ::serde::Serialize;",
	"use ::log;",
	"use serde::Deserialize;",
	"use serde_json::Value;",
	"use core::*;",
	"use alloc::{vec, vec::Vec};",
	"use r#type::Foo;",
	"use typf::Bar;",
	"use x8::a;",
	"use x16::a;",
	"use x_1::a;",
	"use x08::a;",
	"use X::a;",
	"use _private::a;",
	"use a::{c, b};",
	"use a::{b};",
	"use a::b::{self as bee};",
	"use a::*;",
	"use a::{self as aa};",
	"use a::{e::{f, g}, h};",
	"use a::{e::*, h};",
	"#[cfg(unix)]\nuse unix_only::Thing;",
	"/// Documented.\nuse documented::Item;",
	"use Zeta;",
	"use alpha;",
	"use Alpha;",
	"use {Braced, braced};",
	"use Éclair;",
	"use éclair::a;",
	"use zébra::a;",
	"use zebra::a;",
	"use x日本::a;",
];

/// Names that the style editions order differently.
const NAMES: &[&str] = &[
	"use a::alpha;",
	"use a::Zeta;",
	"use a::ZETA;",
	"use a::Zeta2;",
	"use a::A;",
	"use a::Ab;",
	"use a::_b;",
	"use a::x9;",
	"use a::x10;",
	"use a::X9;",
	"use a::x_1;",
	"use r#as::a;",
	"use s::a;",
	"use ::c::a;",
	"use C::a;",
	"use a::{Beta, alpha};",
	"use a::{alpha, Beta, gamma};",
];

#[test]
fn private_uses_match_rustfmt() {
	for &(edition, style_edition) in EDITIONS {
		check_against_rustfmt(USES, edition, style_edition);
		check_against_rustfmt(NAMES, edition, style_edition);
	}
}

#[test]
fn reexports_match_rustfmt() {
	let reexports: Vec<String> = USES
		.iter()
		.chain(NAMES)
		.filter(|item| !item.contains("::self") && !item.contains("{self") && !item.contains("super::super"))
		.map(|item| item.replace("use ", "pub use "))
		.collect();
	let reexports: Vec<&str> = reexports.iter().map(String::as_str).collect();

	for &(edition, style_edition) in EDITIONS {
		check_against_rustfmt(&reexports, edition, style_edition);
	}
}

/// Editions passed to rustfmt, and the style editions they imply.
///
/// Not 2015, in which rustfmt drops the leading `::` of paths (`use ::log;` sorts like `use log;`).
const EDITIONS: &[(&str, StyleEdition)] =
	&[("2024", StyleEdition::E2024), ("2021", StyleEdition::E2021), ("2018", StyleEdition::E2018)];

fn check_against_rustfmt(items: &[&str], edition: &str, style_edition: StyleEdition) {
	let Some(rustfmt) = Rustfmt::find() else {
		eprintln!("skipped: rustfmt is not installed");
		return;
	};
	let sorter = Sorter::new(SortOptions::new().style_edition(style_edition));
	let mut rng = Rng::new(0x5eed_0f0e_0d0c);

	for round in 0..6 {
		let mut shuffled = items.to_vec();

		rng.shuffle(&mut shuffled);

		// one block without blank lines, so rustfmt sorts all items together
		let source = shuffled.join("\n") + "\n";
		let ours = sorter.sort_str(&source).unwrap();
		let theirs = rustfmt.format_for(&source, edition).unwrap();

		assert_eq!(identities(&ours).len(), items.len());
		assert_eq!(
			identities(&ours),
			identities(&theirs),
			"edition {edition}, round {round}\n--- ours:\n{ours}\n--- rustfmt:\n{theirs}"
		);
	}
}

/// Identifies every `use` item in order by its attributes and the set of paths it imports, which survive rustfmt's
/// rewriting of the item (such as sorting brace lists or `a::{b}` → `a::b`).
fn identities(source: &str) -> Vec<String> {
	let file = syn::parse_file(source).unwrap();

	file.items
		.iter()
		.map(|item| {
			let syn::Item::Use(item) = item else {
				panic!("not a use item");
			};
			let mut leaves = Vec::new();
			let prefix = if item.leading_colon.is_some() { "::" } else { "" };

			flatten(&item.tree, prefix.to_owned(), &mut leaves);
			leaves.sort();
			leaves.dedup();

			let attrs: Vec<String> = item.attrs.iter().map(|attr| quote::quote!(#attr).to_string()).collect();

			format!("{} {}", attrs.join(" "), leaves.join(", "))
		})
		.collect()
}

fn flatten(tree: &syn::UseTree, prefix: String, leaves: &mut Vec<String>) {
	match tree {
		syn::UseTree::Path(tree) => flatten(&tree.tree, format!("{prefix}{}::", tree.ident), leaves),
		syn::UseTree::Name(tree) => leaves.push(format!("{prefix}{}", tree.ident)),
		syn::UseTree::Rename(tree) => leaves.push(format!("{prefix}{} as {}", tree.ident, tree.rename)),
		syn::UseTree::Glob(_) => leaves.push(format!("{prefix}*")),
		syn::UseTree::Group(group) => {
			for tree in &group.items {
				flatten(tree, prefix.clone(), leaves);
			}
		}
	}
}
