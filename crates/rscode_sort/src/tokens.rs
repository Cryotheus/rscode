//! Token-based sorting: the syntax tree is reordered and re-emitted as tokens.

use crate::SortError;
use crate::SortOptions;
use crate::cryotheum;
use crate::cryotheum::Plan;
use proc_macro2::TokenStream;
use quote::ToTokens;

pub(crate) fn sort(tokens: TokenStream, options: &SortOptions) -> Result<TokenStream, SortError> {
	let mut file: syn::File = syn::parse2(tokens).map_err(|error| SortError::from_syn(&error))?;

	sort_items(&mut file.items, options, true);

	Ok(file.into_token_stream())
}

/// The text that breaks ties between items with equal sort keys: the item's token text, where the items of nested
/// containers are in a canonical order (by their own tie text) and `extern {}` is spelled `extern "C" {}`.
///
/// It does not depend on the original order of nested items, nor on whether they get sorted (which depends on the
/// options and targets), nor on rustfmt's rewrite of `extern {}`, so sorting again, or after rustfmt, keeps the order.
pub(crate) fn tie_text(item: &syn::Item) -> String {
	let is_container = match item {
		syn::Item::Mod(module) => module.content.is_some(),
		syn::Item::Impl(_) | syn::Item::Trait(_) | syn::Item::ForeignMod(_) => true,
		_ => false,
	};

	if is_container {
		let mut item = item.clone();

		canonicalize(&mut item);
		item.into_token_stream().to_string()
	} else {
		item.to_token_stream().to_string()
	}
}

/// Puts the items of the containers in and below `item` in their canonical order (see [`tie_text`]).
fn canonicalize(item: &mut syn::Item) {
	match item {
		syn::Item::Mod(module) => {
			if let Some((_, items)) = &mut module.content {
				items.iter_mut().for_each(canonicalize);
				sort_by_text(items);
			}
		}
		syn::Item::Impl(block) => sort_by_text(&mut block.items),
		syn::Item::Trait(block) => sort_by_text(&mut block.items),
		syn::Item::ForeignMod(block) => {
			if block.abi.name.is_none() {
				block.abi.name = Some(syn::LitStr::new("C", block.abi.extern_token.span));
			}

			sort_by_text(&mut block.items);
		}
		_ => {}
	}
}

fn sort_by_text<T: ToTokens>(items: &mut Vec<T>) {
	let mut keyed: Vec<(String, T)> =
		std::mem::take(items).into_iter().map(|item| (item.to_token_stream().to_string(), item)).collect();

	keyed.sort_by(|a, b| a.0.cmp(&b.0));
	items.extend(keyed.into_iter().map(|(_, item)| item));
}

/// Sorts the items of a module: its nested containers first (when sorting recursively), then the items themselves
/// when `reorder` is set.
fn sort_items(items: &mut Vec<syn::Item>, options: &SortOptions, reorder: bool) {
	if options.recursive {
		for item in items.iter_mut() {
			sort_nested(item, options);
		}
	}

	if reorder {
		reorder_items(items, options);
	}
}

/// Sorts the contents of a container item that is nested in a sorted container.
fn sort_nested(item: &mut syn::Item, options: &SortOptions) {
	match item {
		syn::Item::Mod(module) => {
			if let Some((_, items)) = &mut module.content {
				sort_items(items, options, options.inline_modules);
			}
		}
		syn::Item::Impl(block) if options.impl_items => {
			reorder(&mut block.items, cryotheum::plan_impl_items);
		}
		syn::Item::Trait(block) if options.trait_items => {
			reorder(&mut block.items, cryotheum::plan_trait_items);
		}
		syn::Item::ForeignMod(block) if options.foreign_items => {
			reorder(&mut block.items, cryotheum::plan_foreign_items);
		}
		_ => {}
	}
}

fn reorder_items(items: &mut Vec<syn::Item>, options: &SortOptions) {
	let ties: Vec<String> = items.iter().map(tie_text).collect();
	let mergeable = |_| options.merge_extern_blocks;
	let plan = cryotheum::plan_items(&items.iter().collect::<Vec<_>>(), &ties, &mergeable, options.style_edition);
	let mut slots: Vec<Option<syn::Item>> = std::mem::take(items).into_iter().map(Some).collect();

	for entry in plan.entries() {
		let Some(mut item) = slots.get_mut(entry.index).and_then(Option::take) else {
			continue;
		};

		if let syn::Item::ForeignMod(block) = &mut item
			&& !entry.merged.is_empty()
		{
			for &index in &entry.merged {
				if let Some(Some(syn::Item::ForeignMod(merged))) = slots.get_mut(index).map(Option::take) {
					block.items.extend(merged.items);
				}
			}

			// the merged items are sorted together, like the items of any nested `extern` block
			if options.recursive && options.foreign_items {
				reorder(&mut block.items, cryotheum::plan_foreign_items);
			}
		}

		items.push(item);
	}

	// every item is in the plan; this is only defensive
	items.extend(slots.into_iter().flatten());
}

/// Reorders the items of an `impl` block, `trait`, or `extern` block.
fn reorder<T: ToTokens>(items: &mut Vec<T>, plan: fn(&[&T], &[String]) -> Plan) {
	let ties = token_texts(items);
	let plan = plan(&items.iter().collect::<Vec<_>>(), &ties);
	let mut slots: Vec<Option<T>> = std::mem::take(items).into_iter().map(Some).collect();

	for entry in plan.entries() {
		if let Some(item) = slots.get_mut(entry.index).and_then(Option::take) {
			items.push(item);
		}
	}

	items.extend(slots.into_iter().flatten());
}

/// The normalized token text of each item.
pub(crate) fn token_texts<T: ToTokens>(items: &[T]) -> Vec<String> {
	items.iter().map(|item| item.to_token_stream().to_string()).collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn sorted(source: &str) -> String {
		sorted_with(source, &SortOptions::new())
	}

	/// Sorts, asserting that sorting again changes nothing.
	fn sorted_with(source: &str, options: &SortOptions) -> String {
		let tokens: TokenStream = source.parse().unwrap();
		let output = sort(tokens, options).unwrap();
		let again = sort(output.clone(), options).unwrap();

		assert_eq!(again.to_string(), output.to_string(), "not idempotent");

		output.to_string()
	}

	/// The token text of a file as syn prints it.
	fn tokens(source: &str) -> String {
		syn::parse_str::<syn::File>(source).unwrap().into_token_stream().to_string()
	}

	#[test]
	fn sorts_a_file() {
		assert_eq!(sorted("fn b() {} fn a() {}"), tokens("fn a() {} fn b() {}"));
		assert_eq!(
			sorted("fn f() {} struct S; use b; impl S { fn m(&self) {} fn new() -> Self { S } }"),
			tokens("use b; struct S; impl S { fn new() -> Self { S } fn m(&self) {} } fn f() {}")
		);
	}

	#[test]
	fn keeps_inner_attributes() {
		assert_eq!(
			sorted("#![allow(dead_code)] fn b() {} fn a() {}"),
			tokens("#![allow(dead_code)] fn a() {} fn b() {}")
		);
	}

	#[test]
	fn sorts_nested_containers() {
		let source = "mod m { fn b() {} fn a() {} impl X { fn b() {} fn a() {} } } trait T { fn b(); fn a(); }";

		assert_eq!(
			sorted(source),
			tokens("trait T { fn a(); fn b(); } mod m { impl X { fn a() {} fn b() {} } fn a() {} fn b() {} }")
		);
		assert_eq!(
			sorted_with(source, &SortOptions::new().recursive(false)),
			tokens("trait T { fn b(); fn a(); } mod m { fn b() {} fn a() {} impl X { fn b() {} fn a() {} } }")
		);
		assert_eq!(
			sorted_with(source, &SortOptions::new().inline_modules(false).trait_items(false)),
			tokens("trait T { fn b(); fn a(); } mod m { fn b() {} fn a() {} impl X { fn a() {} fn b() {} } }")
		);
	}

	#[test]
	fn merges_extern_blocks() {
		let source = r#"
			unsafe extern "C" { pub fn c(); }
			unsafe extern "C" { pub fn a(); pub static B: u8; }
			#[link(name = "z")] unsafe extern "C" { pub fn z(); }
			unsafe extern "C" { pub fn b(); }
		"#;

		assert_eq!(
			sorted(source),
			tokens(
				r#"
				unsafe extern "C" { pub static B: u8; pub fn a(); pub fn b(); pub fn c(); }
				#[link(name = "z")] unsafe extern "C" { pub fn z(); }
				"#
			)
		);
		// unmerged blocks are ordered by their token text with their items in text order
		assert_eq!(
			sorted_with(source, &SortOptions::new().merge_extern_blocks(false)),
			tokens(
				r#"
				unsafe extern "C" { pub static B: u8; pub fn a(); }
				unsafe extern "C" { pub fn b(); }
				unsafe extern "C" { pub fn c(); }
				#[link(name = "z")] unsafe extern "C" { pub fn z(); }
				"#
			)
		);

		// merged but not sorted: the items of each block follow in block order
		assert_eq!(
			sorted_with(source, &SortOptions::new().foreign_items(false)),
			tokens(
				r#"
				unsafe extern "C" { pub fn a(); pub static B: u8; pub fn b(); pub fn c(); }
				#[link(name = "z")] unsafe extern "C" { pub fn z(); }
				"#
			)
		);
	}

	#[test]
	fn ties_use_sorted_nested_contents() {
		// equal self types: the impls are ordered by their sorted contents, whatever the order of those contents
		let a = sorted("impl X { fn d() {} fn c() {} } impl X { fn b() {} fn a() {} }");
		let b = sorted("impl X { fn a() {} fn b() {} } impl X { fn c() {} fn d() {} }");

		assert_eq!(a, b);
		assert_eq!(a, tokens("impl X { fn a() {} fn b() {} } impl X { fn c() {} fn d() {} }"));
	}

	#[test]
	fn parse_errors() {
		let tokens: TokenStream = "fn a() {} struct".parse().unwrap();

		assert!(matches!(sort(tokens, &SortOptions::new()), Err(SortError::Parse { .. })));
	}
}
