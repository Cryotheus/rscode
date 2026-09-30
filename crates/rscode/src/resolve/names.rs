//! Names that resolve without a definition in the loaded crates, and the namespaces items are bound in.

use super::text;
use crate::model::DataShape;
use crate::model::ItemData;
use crate::model::ItemDetail;
use crate::model::ItemKind;
use crate::resolve::Namespace;
use std::borrow::Cow;

/// Macros implemented by the compiler (macro namespace).
const BUILTIN_MACROS: &[&str] = &[
	"cfg",
	"column",
	"compile_error",
	"concat",
	"env",
	"file",
	"format_args",
	"include",
	"include_bytes",
	"include_str",
	"line",
	"macro_rules",
	"module_path",
	"option_env",
	"stringify",
];

/// Strict and reserved keywords that need `r#` to be used as identifiers (the set of [`crate::path::is_keyword`]).
const KEYWORDS: &[&str] = &[
	"Self", "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "crate", "do", "dyn", "else", "enum", "extern",
	"false", "final", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub",
	"ref", "return", "self", "static", "struct", "super", "trait", "true", "try", "type", "typeof", "unsafe", "unsized", "use", "virtual", "where",
	"while", "yield",
];

/// Derive macros of the standard library prelude and macros exported by `std` (macro namespace).
const PRELUDE_MACROS: &[&str] = &[
	"Clone",
	"Copy",
	"Debug",
	"Default",
	"Eq",
	"Hash",
	"Ord",
	"PartialEq",
	"PartialOrd",
	"assert",
	"assert_eq",
	"assert_ne",
	"dbg",
	"debug_assert",
	"debug_assert_eq",
	"debug_assert_ne",
	"eprint",
	"eprintln",
	"format",
	"matches",
	"panic",
	"print",
	"println",
	"thread_local",
	"todo",
	"unimplemented",
	"unreachable",
	"vec",
	"write",
	"writeln",
];

/// Types, traits, and variants of the standard library prelude (type namespace).
const PRELUDE_TYPES: &[&str] = &[
	"AsMut",
	"AsRef",
	"AsyncFn",
	"AsyncFnMut",
	"AsyncFnOnce",
	"Box",
	"Clone",
	"Copy",
	"Default",
	"DoubleEndedIterator",
	"Drop",
	"Eq",
	"Err",
	"ExactSizeIterator",
	"Extend",
	"Fn",
	"FnMut",
	"FnOnce",
	"From",
	"FromIterator",
	"Into",
	"IntoIterator",
	"Iterator",
	"None",
	"Ok",
	"Option",
	"Ord",
	"PartialEq",
	"PartialOrd",
	"Result",
	"Send",
	"Sized",
	"Some",
	"String",
	"Sync",
	"ToOwned",
	"ToString",
	"TryFrom",
	"TryInto",
	"Unpin",
	"Vec",
];

/// Functions and tuple/unit variants of the standard library prelude (value namespace).
const PRELUDE_VALUES: &[&str] = &["Err", "None", "Ok", "Some", "drop"];

/// Primitive types (type namespace).
const PRIMITIVES: &[&str] = &[
	"bool", "char", "f128", "f16", "f32", "f64", "i128", "i16", "i32", "i64", "i8", "isize", "str", "u128", "u16", "u32", "u64", "u8", "usize",
];

/// Crates in the extern prelude of every crate, whether or not they are declared dependencies.
pub(super) const SYSROOT_CRATES: &[&str] = &["std", "core", "alloc", "proc_macro"];

/// What an unbound name falls back to.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum Fallback {
	/// A name of the standard library prelude.
	External,

	/// A primitive type or compiler built-in macro.
	Builtin,
}

/// What a name that is bound nowhere resolves to in a namespace, if it is well-known.
pub(super) fn fallback(name: &str, namespace: Namespace) -> Option<Fallback> {
	let (prelude, builtin) = match namespace {
		Namespace::Type => (PRELUDE_TYPES, PRIMITIVES),
		Namespace::Value => (PRELUDE_VALUES, &[][..]),
		Namespace::Macro => (PRELUDE_MACROS, BUILTIN_MACROS),
	};

	if prelude.contains(&name) {
		Some(Fallback::External)
	} else if builtin.contains(&name) {
		Some(Fallback::Builtin)
	} else {
		None
	}
}

/// An identifier as written in a path: `r#`-prefixed if it is a keyword.
pub(super) fn ident_text(name: &str) -> Cow<'_, str> {
	if KEYWORDS.contains(&name) && !is_path_keyword(name) {
		Cow::Owned(format!("r#{name}"))
	} else {
		Cow::Borrowed(name)
	}
}

/// The crates rustc injects into a crate root as `extern crate` items, given the root's source: `std`, or `core` for
/// `#![no_std]` crates (both when `no_std` depends on a `cfg_attr`).
pub(super) fn injected_crates(root_source: &str) -> &'static [&'static str] {
	let mut injected: &[&str] = &["std"];

	for attribute in text::inner_attributes(root_source) {
		let words = text::words(attribute);

		match words.first().copied() {
			Some("no_std") => return &["core"],
			Some("cfg_attr") if words.contains(&"no_std") => injected = &["std", "core"],
			_ => {}
		}
	}

	injected
}

/// Whether an item is a `macro_rules!` macro (textually scoped), rather than a declarative macro 2.0
/// (`macro m() {}`, scoped like other items).
pub(super) fn is_macro_rules(item: &ItemData) -> bool {
	item.kind == ItemKind::MacroRules && !matches!(&item.detail, ItemDetail::Macro { path, .. } if path == "macro")
}

/// Path segments with a special meaning (`crate`, `self`, `super`, `Self`, `$crate`), which never name a binding.
pub(super) fn is_path_keyword(name: &str) -> bool {
	matches!(name, "crate" | "self" | "super" | "Self" | "$crate")
}

/// The namespaces an item is bound in when it is a member of a module (or an enum, for variants, or an owner,
/// for associated items).
pub(super) fn namespaces(item: &ItemData) -> &'static [Namespace] {
	const TYPE: &[Namespace] = &[Namespace::Type];
	const VALUE: &[Namespace] = &[Namespace::Value];
	const TYPE_AND_VALUE: &[Namespace] = &[Namespace::Type, Namespace::Value];
	const MACRO: &[Namespace] = &[Namespace::Macro];

	match item.kind {
		ItemKind::Module
		| ItemKind::Enum
		| ItemKind::Union
		| ItemKind::Trait
		| ItemKind::TraitAlias
		| ItemKind::TypeAlias
		| ItemKind::ExternCrate
		| ItemKind::ForeignType
		| ItemKind::AssocType => TYPE,

		// tuple and unit structs and variants are also constructors
		ItemKind::Struct | ItemKind::Variant => match item.detail {
			ItemDetail::Data {
				shape: DataShape::Tuple | DataShape::Unit,
				..
			} => TYPE_AND_VALUE,

			_ => TYPE,
		},

		ItemKind::Fn
		| ItemKind::Const
		| ItemKind::Static
		| ItemKind::ForeignFn
		| ItemKind::ForeignStatic
		| ItemKind::AssocFn
		| ItemKind::AssocConst => VALUE,

		ItemKind::MacroRules => MACRO,

		ItemKind::MacroCall
		| ItemKind::Use
		| ItemKind::Import
		| ItemKind::ExternBlock
		| ItemKind::ForeignMacro
		| ItemKind::Impl
		| ItemKind::AssocMacro => &[],
	}
}

/// The name of the macro a function of a proc-macro crate implements, given its outer attributes: its own name for
/// `#[proc_macro]` and `#[proc_macro_attribute]`, the derive's name for `#[proc_macro_derive(Name)]`.
pub(super) fn proc_macro_name<'a>(attributes: &[&'a str], function: &'a str) -> Option<&'a str> {
	attributes.iter().find_map(|attribute| {
		let words = text::words(attribute);

		match words.first().copied()? {
			"proc_macro" | "proc_macro_attribute" => Some(function),
			"proc_macro_derive" => words.get(1).copied(),
			_ => None,
		}
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn fallbacks_depend_on_namespace() {
		assert_eq!(fallback("Option", Namespace::Type), Some(Fallback::External));
		assert_eq!(fallback("Option", Namespace::Value), None);
		assert_eq!(fallback("Some", Namespace::Value), Some(Fallback::External));
		assert_eq!(fallback("u8", Namespace::Type), Some(Fallback::Builtin));
		assert_eq!(fallback("u8", Namespace::Value), None);
		assert_eq!(fallback("println", Namespace::Macro), Some(Fallback::External));
		assert_eq!(fallback("concat", Namespace::Macro), Some(Fallback::Builtin));
		assert_eq!(fallback("Debug", Namespace::Macro), Some(Fallback::External));
		assert_eq!(fallback("Debug", Namespace::Type), None);
		assert_eq!(fallback("Frobnicate", Namespace::Type), None);
	}

	#[test]
	fn injected_crates_depend_on_no_std() {
		assert_eq!(injected_crates("mod a;"), ["std"]);
		assert_eq!(injected_crates("//! Docs.\n#![no_std]\n"), ["core"]);
		assert_eq!(injected_crates("#![cfg_attr(not(feature = \"std\"), no_std)]"), ["std", "core"]);
		assert_eq!(injected_crates("#![cfg_attr(feature = \"no_std\", deny(warnings))]"), ["std"]);
		assert_eq!(injected_crates("mod a; #![no_std]"), ["std"], "inner attributes come first");
	}

	#[test]
	fn keywords_get_raw_prefix() {
		assert_eq!(ident_text("type"), "r#type");
		assert_eq!(ident_text("gen"), "r#gen");
		assert_eq!(ident_text("union"), "union");
		assert_eq!(ident_text("crate"), "crate");
		assert_eq!(ident_text("Foo"), "Foo");
	}

	#[test]
	fn proc_macro_names_come_from_attributes() {
		assert_eq!(proc_macro_name(&["doc = \"x\"", "proc_macro"], "make"), Some("make"));
		assert_eq!(proc_macro_name(&["proc_macro_attribute"], "route"), Some("route"));
		assert_eq!(
			proc_macro_name(&["proc_macro_derive(Thing, attributes(thing))"], "derive_thing"),
			Some("Thing")
		);
		assert_eq!(proc_macro_name(&["proc_macro_derive"], "broken"), None);
		assert_eq!(proc_macro_name(&["inline"], "helper"), None);
	}
}
