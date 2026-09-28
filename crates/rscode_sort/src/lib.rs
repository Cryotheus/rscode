//! Deterministic sorting of the items in Rust source files.
//!
//! Sorting the same items produces the same output whatever order they started in, so two runs of a code generator
//! (such as `bindgen`) that emit the same items in different orders produce byte-identical files.
//!
//! Sorting is offered in two flavors:
//! - [`Sorter::sort_str`] and friends operate on source text.
//!   Only whole items are moved around, along with the comments attached to them, so comments and formatting are
//!   preserved.
//! - [`Sorter::sort_tokens`] operates on a [`proc_macro2::TokenStream`], such as generated `bindgen` output.
//!
//! Contents of expressions, function bodies, and macro invocations are never sorted, nor are enum variants, struct
//! fields, or items inside function bodies.
//!
//! Sorting parses on the calling thread, and `proc_macro2` keeps the text of everything parsed in a thread-local
//! source map for the lifetime of the thread. Long-running processes should sort on short-lived threads (or call
//! `proc_macro2::extra::invalidate_current_thread_spans` once no syntax trees of the thread are in use).
//!
//! ```
//! let sorted = rscode_sort::sort_str("fn b() {}\nfn a() {}\n").unwrap();
//! assert_eq!(sorted, "fn a() {}\n\nfn b() {}\n");
//! ```
//!
//! # The Cryotheum ordering
//!
//! The items of a module (a file, or an inline `mod foo { ... }`) are grouped, in this order:
//!
//! 1. `extern crate` items.
//! 2. `mod foo;` declarations: without `cfg`, then with a `cfg`, then with `#[cfg(test)]`.
//! 3. `use` items, ordered like rustfmt orders them (in the [`StyleEdition`] of [`SortOptions::style_edition`]).
//! 4. Re-exports: `pub use`, `pub(crate) use`, and so on.
//! 5. Type aliases.
//! 6. Constants.
//! 7. Statics.
//! 8. Mutable statics.
//! 9. Data types (structs, enums, unions, traits, and trait aliases), each followed by its `impl` blocks: inherent
//!    impls first, then trait impls.
//! 10. `impl` blocks of types that are not defined in the module.
//! 11. `extern` blocks. Blocks with the same safety, ABI, and attributes merge into one (`extern {}` counts as
//!     `extern "C" {}`).
//! 12. Functions.
//! 13. Inline modules: without `cfg`, then with a `cfg`, then with `#[cfg(test)]`.
//! 14. Anything else (syntax `syn` does not model), which keeps its relative order.
//!
//! The items of `impl` blocks and traits are grouped as associated types, associated constants, `fn new`, `fn _new`,
//! other functions without a receiver, then methods. The items of `extern` blocks are grouped as types, statics, then
//! functions.
//!
//! Within a group, items are ordered by name with [`version_cmp`], comparing names without their leading
//! underscores first so `_mike` directly follows `mike`. `use` items, `mod foo;` declarations, and `extern crate`
//! items are ordered exactly like rustfmt orders them instead (by bytes for `mod` and `extern crate`), so rustfmt
//! leaves the order alone, provided that [`SortOptions::style_edition`] is rustfmt's style edition: the two order
//! `use` items differently. Remaining ties are broken by the items' token text, in which the items of nested
//! containers count in a canonical order.
//!
//! ## Macros are barriers
//!
//! `macro_rules!` definitions, macro invocations in item position (including `include!`), and items with
//! `#[macro_use]` (modules and `extern crate` items) never move, and no item moves across them: they split the items
//! of their container into segments, and each segment is sorted on its own. Macros are textually scoped, may be
//! redefined, and invocations can expand to anything (such as `cfg_if! { macro_rules! .. }`), so moving items
//! across them could change what the code means or break it. Macros at the very top of a file stay there, and the
//! items after them are sorted as usual.
//!
//! Sorted source looks like this:
//!
//! ```text
//! // macros are barriers: they stay in place, and the items after them are sorted on their own
//! macro_rules! papa {
//!     () => {};
//! }
//!
//! mod alfa;
//!
//! use bravo::Charlie;
//! use bravo::delta::{Echo, Foxtrot};
//!
//! pub use Golf;
//! pub use hotel::India;
//!
//! type Juliet = Kilo;
//!
//! // enum, union, and struct are all sorted together in the same group
//! struct Lima;
//!
//! // impl items follow immediately after the type they target (if it is defined in the same file)
//! impl Lima {
//!     // new goes at the top
//!     fn new() -> Option<Self> {
//!         /* ... code ... */
//!     }
//!
//!     // other non-method functions follow
//!     unsafe fn new_unchecked() -> Option<Self> {
//!         /* ... code ... */
//!     }
//!
//!     // typical methods follow
//!     fn mike(&self) {
//!         /* ... code ... */
//!     }
//!
//!     // underscore-prefixed function names (implementation helpers) have the same sorting as their non-underscore-prefixed names
//!     // but they are always after the non-underscore-prefixed name
//!     fn _mike(&self) {
//!         /* ... code ... */
//!     }
//! }
//!
//! impl November {
//!     // the definition doesn't exist here
//!     //so it goes after all data types (enum, union, struct)
//! }
//!
//! unsafe extern "C" {
//!     //extern blocks have their items sorted and combined too
//!     //always make sure to check the attributes before merging!
//! }
//!
//! mod oscar {
//!     //inline modules are sorted the same as a file is
//! }
//!
//! #[cfg(test)]
//! mod tests {
//!     //modules with the `#[cfg(test)]` attribute are sorted in a group after normal modules
//! }
//! ```
//!
//! # Comments and layout
//!
//! The text engine moves each item together with the comments directly above it (including comment blocks separated
//! from it by blank lines, which act as section headers) and the comment trailing it on its last line. Comments
//! separated from the first item of a container by a blank line (such as a license header) stay in place, as does
//! everything after the last item. When `extern` blocks merge, the comments of a merged-away block (including those
//! among its attributes and keywords) move above its first item.
//!
//! Every item of a sorted container goes on its own line (a container with a single item is left as it is). Items of
//! different groups are separated by one blank line, except that consecutive macros keep the blank line between
//! them, or the lack of one, unless one of them has attributes, doc comments, or comments above it. Within a group:
//! - `use` items, `mod foo;` declarations, `extern crate` items, type aliases, constants, statics, associated types
//!   and constants, and the items of `extern` blocks are separated by a line break, or by a blank line next to an
//!   item with attributes, doc comments, or comments above it,
//! - other items are separated by a blank line.
//!
//! None of this depends on how many lines an item spans, so running rustfmt after sorting leaves nothing for
//! sorting to change again.

#![warn(missing_docs)]

mod cryotheum;
mod imports;
mod text;
mod tokens;
mod version;

pub use version::version_cmp;

use proc_macro2::TokenStream;
use serde::Deserialize;
use serde::Serialize;

/// Sorts all containers of a source file with the default [`SortOptions`].
pub fn sort_str(source: &str) -> Result<String, SortError> {
	Sorter::default().sort_str(source)
}

/// Sorts all containers of a token stream (a whole file's worth of items) with the default [`SortOptions`].
pub fn sort_tokens(tokens: TokenStream) -> Result<TokenStream, SortError> {
	Sorter::default().sort_tokens(tokens)
}

/// A scheme for ordering items.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OrderingSchema {
	/// Groups items by kind (modules, imports, re-exports, type aliases, constants, statics, data types with their
	/// `impl` blocks, loose `impl` blocks, `extern` blocks, functions, then inline modules), and orders each group by
	/// name using [`version_cmp`]. Macros are barriers that never move, and that no item moves across.
	#[default]
	Cryotheum,
}

impl OrderingSchema {
	/// All available schemas.
	pub const ALL: &'static [Self] = &[Self::Cryotheum];

	/// The kebab-case name of the schema.
	pub fn name(self) -> &'static str {
		match self {
			Self::Cryotheum => "cryotheum",
		}
	}
}

impl std::fmt::Display for OrderingSchema {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.name())
	}
}

impl std::str::FromStr for OrderingSchema {
	type Err = SortError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Self::ALL
			.iter()
			.copied()
			.find(|schema| schema.name().eq_ignore_ascii_case(s))
			.ok_or_else(|| SortError::UnknownSchema(s.to_owned()))
	}
}

#[cfg(feature = "clap")]
impl clap::ValueEnum for OrderingSchema {
	fn value_variants<'a>() -> &'a [Self] {
		Self::ALL
	}

	fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
		Some(clap::builder::PossibleValue::new(self.name()))
	}
}

/// A style edition of rustfmt (its `style_edition` option), which decides how rustfmt orders `use` items.
///
/// Style editions 2015, 2018, and 2021 order them alike: within a path segment, `snake_case` names sort before
/// `CamelCase` names, which sort before `UPPER_SNAKE_CASE` names, and names of the same kind by their bytes. Style
/// edition 2024 compares names by their bytes, except that numbers compare by value: `Zeta` sorts before `alpha`, and
/// `x9` before `x10`.
///
/// Unless its configuration sets a style edition, rustfmt uses the edition it formats for (and 2015 without one). In
/// the 2015 edition, rustfmt also drops the leading `::` of `use` paths before comparing them, which sorting does not.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum StyleEdition {
	/// Style edition 2015.
	#[serde(rename = "2015")]
	E2015,

	/// Style edition 2018.
	#[serde(rename = "2018")]
	E2018,

	/// Style edition 2021.
	#[serde(rename = "2021")]
	E2021,

	/// Style edition 2024.
	#[default]
	#[serde(rename = "2024")]
	E2024,
}

impl StyleEdition {
	/// Every style edition, oldest first.
	pub const ALL: &'static [Self] = &[Self::E2015, Self::E2018, Self::E2021, Self::E2024];

	/// The year of the style edition.
	pub fn as_str(self) -> &'static str {
		match self {
			Self::E2015 => "2015",
			Self::E2018 => "2018",
			Self::E2021 => "2021",
			Self::E2024 => "2024",
		}
	}
}

impl std::fmt::Display for StyleEdition {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.as_str())
	}
}

/// Which containers get sorted.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct SortOptions {
	/// The ordering scheme.
	pub schema: OrderingSchema,

	/// Order `use` items like rustfmt does in this style edition, so that rustfmt leaves their order alone.
	pub style_edition: StyleEdition,

	/// Also sort containers nested in the targeted containers.
	///
	/// When `false`, only the targeted containers themselves are sorted (rustfmt's `--skip-children` analog).
	pub recursive: bool,

	/// Sort the items of inline modules (`mod foo { ... }`).
	pub inline_modules: bool,

	/// Sort the items of `impl` blocks.
	pub impl_items: bool,

	/// Sort the items of `trait` definitions.
	pub trait_items: bool,

	/// Sort the items of `extern` blocks.
	pub foreign_items: bool,

	/// Merge sibling `extern` blocks which have identical ABIs, safety, and attributes.
	pub merge_extern_blocks: bool,
}

impl SortOptions {
	/// The default options: everything is sorted, recursively, with `use` items ordered for style edition 2024.
	pub fn new() -> Self {
		Self {
			schema: OrderingSchema::Cryotheum,
			style_edition: StyleEdition::E2024,
			recursive: true,
			inline_modules: true,
			impl_items: true,
			trait_items: true,
			foreign_items: true,
			merge_extern_blocks: true,
		}
	}

	/// Sets [`SortOptions::schema`].
	pub fn schema(mut self, schema: OrderingSchema) -> Self {
		self.schema = schema;
		self
	}

	/// Sets [`SortOptions::style_edition`].
	pub fn style_edition(mut self, style_edition: StyleEdition) -> Self {
		self.style_edition = style_edition;
		self
	}

	/// Sets [`SortOptions::recursive`].
	pub fn recursive(mut self, recursive: bool) -> Self {
		self.recursive = recursive;
		self
	}

	/// Sets [`SortOptions::inline_modules`].
	pub fn inline_modules(mut self, inline_modules: bool) -> Self {
		self.inline_modules = inline_modules;
		self
	}

	/// Sets [`SortOptions::impl_items`].
	pub fn impl_items(mut self, impl_items: bool) -> Self {
		self.impl_items = impl_items;
		self
	}

	/// Sets [`SortOptions::trait_items`].
	pub fn trait_items(mut self, trait_items: bool) -> Self {
		self.trait_items = trait_items;
		self
	}

	/// Sets [`SortOptions::foreign_items`].
	pub fn foreign_items(mut self, foreign_items: bool) -> Self {
		self.foreign_items = foreign_items;
		self
	}

	/// Sets [`SortOptions::merge_extern_blocks`].
	pub fn merge_extern_blocks(mut self, merge_extern_blocks: bool) -> Self {
		self.merge_extern_blocks = merge_extern_blocks;
		self
	}
}

impl Default for SortOptions {
	fn default() -> Self {
		Self::new()
	}
}

/// A container whose items should be sorted.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum SortTarget {
	/// The items at the root of the file.
	File,

	/// The item (inline module, `impl` block, `trait`, or `extern` block) whose first token
	/// (including outer attributes and doc comments) starts at this byte offset.
	Item(usize),
}

/// Sorts items according to [`SortOptions`].
#[derive(Debug, Default, Clone)]
pub struct Sorter {
	options: SortOptions,
}

impl Sorter {
	/// A sorter using `options`.
	pub fn new(options: SortOptions) -> Self {
		Self { options }
	}

	/// The options this sorter uses.
	pub fn options(&self) -> &SortOptions {
		&self.options
	}

	/// Sorts every container of a source file.
	pub fn sort_str(&self, source: &str) -> Result<String, SortError> {
		self.sort_str_within(source, &[SortTarget::File])
	}

	/// Sorts only the targeted containers (and their nested containers when [`SortOptions::recursive`] is set).
	///
	/// A targeted container whose kind is disabled by the options (for example an `impl` block when
	/// [`SortOptions::impl_items`] is `false`) is left as is, but containers nested in it are still sorted when
	/// [`SortOptions::recursive`] is set.
	pub fn sort_str_within(&self, source: &str, targets: &[SortTarget]) -> Result<String, SortError> {
		match self.options.schema {
			OrderingSchema::Cryotheum => text::sort(source, targets, &self.options),
		}
	}

	/// Sorts every container of a token stream containing a whole file's worth of items.
	///
	/// Token streams carry no comments, so the output is re-assembled from the syntax tree.
	pub fn sort_tokens(&self, tokens: TokenStream) -> Result<TokenStream, SortError> {
		match self.options.schema {
			OrderingSchema::Cryotheum => tokens::sort(tokens, &self.options),
		}
	}
}

/// Errors produced while sorting.
#[derive(Debug, thiserror::Error)]
pub enum SortError {
	/// The source does not parse as a Rust file.
	#[error("failed to parse Rust source at {line}:{column}: {message}")]
	Parse {
		/// The parser's message.
		message: String,

		/// 1-based line.
		line: usize,

		/// 1-based column, in characters.
		column: usize,
	},

	/// A [`SortTarget::Item`] does not name a container.
	#[error("no sortable container (inline module, impl, trait, or extern block) starts at byte {0}")]
	NoContainer(usize),

	/// An [`OrderingSchema`] name was not recognized.
	#[error("unknown ordering schema `{0}`")]
	UnknownSchema(String),

	/// Sorting failed because of a bug in this crate (for example, its output would not parse). The source is left
	/// as it is.
	#[error("internal error while sorting at {line}:{column}: {message} (this is a bug in rscode_sort)")]
	Internal {
		/// What went wrong.
		message: String,

		/// 1-based line in the source.
		line: usize,

		/// 1-based column in the source, in characters.
		column: usize,
	},
}

impl SortError {
	/// Must be called on the thread that parsed: spans live in a thread-local source map.
	pub(crate) fn from_syn(error: &syn::Error) -> Self {
		let start = error.span().start();

		Self::Parse { message: error.to_string(), line: start.line, column: start.column + 1 }
	}

	/// Like [`SortError::from_syn`] for an error of `syn::parse_file(source)`, which strips a byte order mark before
	/// parsing: the byte order mark counts as a character of the first line, like in `rscode` locations.
	pub(crate) fn from_syn_in(error: &syn::Error, source: &str) -> Self {
		let start = error.span().start();

		Self::Parse {
			message: error.to_string(),
			line: start.line,
			column: start.column + 1 + usize::from(start.line == 1 && source.starts_with('\u{feff}')),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn options_serialize_in_kebab_case_with_defaults() {
		let options: SortOptions =
			serde_json::from_str(r#"{ "recursive": false, "merge-extern-blocks": false }"#).unwrap();

		assert_eq!(options, SortOptions::new().recursive(false).merge_extern_blocks(false));
		assert_eq!(serde_json::from_str::<SortOptions>("{}").unwrap(), SortOptions::default());

		let json = serde_json::to_value(SortOptions::new()).unwrap();

		assert_eq!(json["schema"], "cryotheum");
		assert_eq!(json["inline-modules"], true);
		assert_eq!(json["foreign-items"], true);
	}

	#[test]
	fn schema_names() {
		assert_eq!("cryotheum".parse::<OrderingSchema>().unwrap(), OrderingSchema::Cryotheum);
		assert_eq!("Cryotheum".parse::<OrderingSchema>().unwrap(), OrderingSchema::Cryotheum);
		assert!(
			matches!("rustfmt".parse::<OrderingSchema>(), Err(SortError::UnknownSchema(name)) if name == "rustfmt")
		);
		assert_eq!(OrderingSchema::Cryotheum.to_string(), "cryotheum");
		assert_eq!(serde_json::to_string(&OrderingSchema::Cryotheum).unwrap(), r#""cryotheum""#);
	}

	#[cfg(feature = "clap")]
	#[test]
	fn schema_value_enum() {
		use clap::ValueEnum;

		assert_eq!(<OrderingSchema as ValueEnum>::from_str("cryotheum", true).unwrap(), OrderingSchema::Cryotheum);
		assert_eq!(OrderingSchema::value_variants(), OrderingSchema::ALL);
	}

	#[test]
	fn sorter_entry_points() {
		let sorter = Sorter::new(SortOptions::new().impl_items(false));

		assert!(!sorter.options().impl_items);
		assert_eq!(
			sorter.sort_str("impl X {\n\tfn b() {}\n\tfn a() {}\n}\n").unwrap(),
			"impl X {\n\tfn b() {}\n\tfn a() {}\n}\n"
		);
		assert_eq!(sort_tokens("fn b() {} fn a() {}".parse().unwrap()).unwrap().to_string(), "fn a () { } fn b () { }");
		assert!(matches!(sort_str("fn"), Err(SortError::Parse { line: 1, .. })));
	}
}
