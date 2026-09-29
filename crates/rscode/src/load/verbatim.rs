//! Classifying `Verbatim` syntax: items that syn tokenizes but does not model.
//!
//! syn produces `Verbatim` items (and impl/trait/foreign items) for syntax rustc's parser accepts but syn has no
//! syntax tree for, e.g. `fn f();` in a module or `impl` block, `static S: u8;`, `const C<T>: u8 = 1;`,
//! `type T: Bound;`, `const trait T {}`, `const impl T for S {}`, `impl(crate) trait T {}`, `macro m() {}`,
//! functions with bodies or statics with values in `extern` blocks, and `use {::a, b};` (and `use {a, {::b}};`).

use super::Container;
use super::syntax;
use crate::model::Receiver;
use proc_macro2::Delimiter;
use proc_macro2::Ident;
use proc_macro2::Span;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use proc_macro2::extra::DelimSpan;
use quote::ToTokens;
use syn::Attribute;
use syn::Item;
use syn::ItemImpl;
use syn::ItemTrait;
use syn::ItemTraitAlias;
use syn::Signature;
use syn::Token;
use syn::UseTree;
use syn::parse::ParseStream;
use syn::parse::Parser;

/// Keywords that cannot name an item (unless written as raw identifiers).
const KEYWORDS: &[&str] = &[
	"as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let",
	"loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
	"use", "where", "while",
];

/// A `Verbatim` item split into its outer attributes, its visibility, and the remaining tokens.
pub(super) struct Parts {
	pub(super) attrs: Vec<Attribute>,
	pub(super) vis: syn::Visibility,
	pub(super) rest: Vec<TokenTree>,
}

impl Parts {
	pub(super) fn split(tokens: &TokenStream) -> Self {
		let parser = |input: ParseStream| {
			Ok(Self {
				attrs: input.call(Attribute::parse_outer)?,
				vis: input.parse()?,
				rest: input.parse::<TokenStream>()?.into_iter().collect(),
			})
		};

		parser.parse2(tokens.clone()).unwrap_or_else(|_: syn::Error| Self {
			attrs: Vec::new(),
			vis: syn::Visibility::Inherited,
			rest: tokens.clone().into_iter().collect(),
		})
	}
}

/// What a `Verbatim` item is.
pub(super) enum Shape {
	/// A trait with modifiers syn does not support (`const trait`, `impl(crate) trait`), re-parsed without them.
	Trait(Box<ItemTrait>),

	/// A trait alias with modifiers syn does not support, re-parsed without them.
	TraitAlias(Box<ItemTraitAlias>),

	/// An `impl` block with modifiers syn does not support (`const impl`), re-parsed without them.
	Impl(Box<ItemImpl>),

	/// A `use` item whose top-level group, or a group nested in it through groups only, has elements starting with
	/// `::`. The elements of those groups, in source order (the groups themselves are not elements).
	Use(Vec<UseElement>),

	Fn {
		name: Ident,
		receiver: Option<Receiver>,
		is_const: bool,
		is_async: bool,
		is_unsafe: bool,

		/// The body, including its braces.
		body: Option<Span>,
	},

	Const {
		name: Ident,
	},

	Static {
		name: Ident,
		mutable: bool,
	},

	Type {
		name: Ident,
	},

	/// A declarative macro 2.0: `macro name(..) { .. }` or `macro name { .. }`.
	Macro {
		name: Ident,

		/// The last delimited group.
		body: Option<DelimSpan>,
	},

	/// Anything else.
	Unknown {
		/// The last delimited group.
		body: Option<DelimSpan>,
	},
}

/// An element of a group of a `use` item reached from the start of the item through groups only, which may start
/// with `::` (`::a` and `::c` in `use {::a, b, {::c}};`).
pub(super) struct UseElement {
	pub(super) leading_colon: Option<Token![::]>,
	pub(super) tree: UseTree,
}

/// Classifies a `Verbatim` item by its leading tokens (after attributes and visibility).
///
/// Traits, trait aliases, `impl` blocks, and `use` items are only recognized in modules.
pub(super) fn classify(parts: &Parts, container: Container) -> Shape {
	let rest = parts.rest.as_slice();

	if let Some(shape) = function(rest) {
		return shape;
	}

	if container == Container::Module
		&& let Some(shape) = reparse(parts).or_else(|| rooted_use(rest))
	{
		return shape;
	}

	let stripped = strip(rest, &["default"]);

	match (stripped.first(), stripped.get(1).and_then(name)) {
		(Some(keyword), Some(name)) if is(keyword, "const") => Shape::Const { name: name.clone() },
		(Some(keyword), Some(name)) if is(keyword, "type") => Shape::Type { name: name.clone() },

		(Some(keyword), Some(name)) if is(keyword, "macro") => Shape::Macro {
			name: name.clone(),
			body: last_group(rest),
		},

		_ => static_item(stripped).unwrap_or_else(|| Shape::Unknown { body: last_group(rest) }),
	}
}

/// `[default] [const] [async] [safe | unsafe] [extern ["abi"]] fn name ...`
fn function(rest: &[TokenTree]) -> Option<Shape> {
	let fn_index = rest.iter().position(|token| is(token, "fn"))?;
	let mut modifiers = rest[..fn_index].iter().peekable();
	let (mut is_const, mut is_async, mut is_unsafe) = (false, false, false);

	while let Some(token) = modifiers.next() {
		match token {
			TokenTree::Ident(ident) if ident == "const" => is_const = true,
			TokenTree::Ident(ident) if ident == "async" => is_async = true,
			TokenTree::Ident(ident) if ident == "unsafe" => is_unsafe = true,
			TokenTree::Ident(ident) if ident == "default" || ident == "safe" => {}

			TokenTree::Ident(ident) if ident == "extern" => {
				modifiers.next_if(|token| matches!(token, TokenTree::Literal(_)));
			}

			_ => return None,
		}
	}

	let name = rest.get(fn_index + 1).and_then(name)?;

	let (end, body) = match rest.last() {
		Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Brace => (rest.len() - 1, Some(group.span())),
		Some(TokenTree::Punct(punct)) if punct.as_char() == ';' => (rest.len() - 1, None),
		_ => (rest.len(), None),
	};

	// syn's `Signature` knows neither `default` nor `safe`
	let signature: TokenStream = rest[..fn_index]
		.iter()
		.filter(|token| !is(token, "default") && !is(token, "safe"))
		.chain(&rest[fn_index..end.max(fn_index)])
		.cloned()
		.collect();

	let receiver = syn::parse2::<Signature>(signature)
		.ok()
		.and_then(|signature| signature.receiver().map(syntax::receiver));

	Some(Shape::Fn {
		name: name.clone(),
		receiver,
		is_const,
		is_async,
		is_unsafe,
		body,
	})
}

/// Whether a token is the identifier (or keyword) `keyword`.
fn is(token: &TokenTree, keyword: &str) -> bool {
	matches!(token, TokenTree::Ident(ident) if ident == keyword)
}

fn last_group(tokens: &[TokenTree]) -> Option<DelimSpan> {
	tokens.iter().rev().find_map(|token| match token {
		TokenTree::Group(group) if group.delimiter() != Delimiter::None => Some(group.delim_span()),
		_ => None,
	})
}

/// An identifier token that can be the name of an item.
fn name(token: &TokenTree) -> Option<&Ident> {
	match token {
		TokenTree::Ident(ident) if !KEYWORDS.iter().any(|keyword| ident == keyword) => Some(ident),
		_ => None,
	}
}

/// Re-parses traits and `impl` blocks without the modifiers syn rejects: `impl(..)` restrictions and `const`.
fn reparse(parts: &Parts) -> Option<Shape> {
	let kept = match parts.rest.as_slice() {
		[restriction, TokenTree::Group(group), rest @ ..] if is(restriction, "impl") && group.delimiter() == Delimiter::Parenthesis => {
			strip(rest, &["const"])
		}

		[constness, rest @ ..] if is(constness, "const") => rest,
		_ => return None,
	};

	let is_trait_or_impl = kept
		.first()
		.is_some_and(|token| ["trait", "unsafe", "auto", "impl"].iter().any(|keyword| is(token, keyword)));

	if !is_trait_or_impl {
		return None;
	}

	let mut tokens = TokenStream::new();

	for attr in &parts.attrs {
		attr.to_tokens(&mut tokens);
	}

	parts.vis.to_tokens(&mut tokens);
	tokens.extend(kept.iter().cloned());

	match syn::parse2::<Item>(tokens).ok()? {
		Item::Trait(item) => Some(Shape::Trait(Box::new(item))),
		Item::TraitAlias(item) => Some(Shape::TraitAlias(Box::new(item))),
		Item::Impl(item) => Some(Shape::Impl(Box::new(item))),
		_ => None,
	}
}

/// Parses a group whose elements may start with `::`, adding its elements to `elements`; elements that are groups
/// themselves are parsed the same way (like syn's `parse_use_tree` with `allow_crate_root_in_path`).
fn rooted_group(input: ParseStream, elements: &mut Vec<UseElement>) -> syn::Result<()> {
	let content;

	syn::braced!(content in input);

	while !content.is_empty() {
		let leading_colon: Option<Token![::]> = content.parse()?;

		if leading_colon.is_none() && content.peek(syn::token::Brace) {
			rooted_group(&content, elements)?;
		} else {
			elements.push(UseElement {
				leading_colon,
				tree: content.parse()?,
			});
		}

		if !content.is_empty() {
			content.parse::<Token![,]>()?;
		}
	}

	Ok(())
}

/// `use {::a, b};`, `use {a, {::b}};`: syn gives up on `::` at the start of group elements.
fn rooted_use(rest: &[TokenTree]) -> Option<Shape> {
	if !rest.first().is_some_and(|token| is(token, "use")) {
		return None;
	}

	let parser = |input: ParseStream| {
		let mut elements = Vec::new();

		input.parse::<Token![use]>()?;
		rooted_group(input, &mut elements)?;
		input.parse::<Token![;]>()?;
		Ok(elements)
	};

	parser.parse2(rest.iter().cloned().collect()).ok().map(Shape::Use)
}

/// `[safe | unsafe] static [mut] name ...`
fn static_item(rest: &[TokenTree]) -> Option<Shape> {
	let rest = strip(rest, &["safe", "unsafe"]);
	let mutable = rest.get(1).is_some_and(|token| is(token, "mut"));
	let name = rest.get(if mutable { 2 } else { 1 }).and_then(name)?;

	rest.first()
		.filter(|keyword| is(keyword, "static"))
		.map(|_| Shape::Static { name: name.clone(), mutable })
}

/// Skips leading tokens that are any of `keywords`.
fn strip<'a>(mut tokens: &'a [TokenTree], keywords: &[&str]) -> &'a [TokenTree] {
	while let [first, rest @ ..] = tokens
		&& keywords.iter().any(|keyword| is(first, keyword))
	{
		tokens = rest;
	}

	tokens
}

#[cfg(test)]
mod tests {
	use super::*;
	use syn::ForeignItem;
	use syn::ImplItem;
	use syn::TraitItem;

	#[test]
	fn classifies_associated_and_foreign_items() {
		let inherent = Container::Impl { of_trait: false };

		assert_eq!(
			describe("fn f(&mut self);", inherent),
			"fn f receiver=Some(Receiver { reference: true, mutable: true, typed: false }) const=false async=false unsafe=false body=false"
		);

		assert_eq!(
			describe("default async fn f(self: Box<Self>);", inherent),
			"fn f receiver=Some(Receiver { reference: false, mutable: false, typed: true }) const=false async=true unsafe=false body=false"
		);

		assert_eq!(describe("const C: u8;", inherent), "const C");
		assert_eq!(describe("default type T;", inherent), "type T");
		assert_eq!(describe("type T: Clone = u8;", inherent), "type T");
		assert_eq!(
			describe("pub fn f();", Container::Trait),
			"fn f receiver=None const=false async=false unsafe=false body=false"
		);
		assert_eq!(describe("const C<T>: u8;", Container::Trait), "const C");
		assert_eq!(describe("pub type T;", Container::Trait), "type T");
		assert_eq!(
			describe("safe fn f() {}", Container::Extern),
			"fn f receiver=None const=false async=false unsafe=false body=true"
		);
		assert_eq!(describe("unsafe static mut S: u8 = 1;", Container::Extern), "static S mut=true");
		assert_eq!(describe("type T = u8;", Container::Extern), "type T");
	}

	#[test]
	fn classifies_module_items() {
		let module = Container::Module;

		assert_eq!(
			describe("fn f();", module),
			"fn f receiver=None const=false async=false unsafe=false body=false"
		);
		assert_eq!(
			describe("pub const unsafe extern \"C\" fn f<T: Fn(u8)>(x: T) -> u8 where T: Copy;", module),
			"fn f receiver=None const=true async=false unsafe=true body=false"
		);
		assert_eq!(describe("static S: u8;", module), "static S mut=false");
		assert_eq!(describe("static mut S = 1;", module), "static S mut=true");
		assert_eq!(describe("const C: u8;", module), "const C");
		assert_eq!(describe("const G<T>: u8 = 1;", module), "const G");
		assert_eq!(describe("type T: Bound = u8;", module), "type T");
		assert_eq!(describe("const trait T { fn f(); }", module), "trait T (1 items)");
		assert_eq!(describe("const unsafe trait T {}", module), "trait T (0 items)");
		assert_eq!(describe("pub impl(crate) const trait T { type A; }", module), "trait T (1 items)");
		assert_eq!(describe("const impl T for u8 { fn f() {} }", module), "impl u8 (1 items)");
		assert_eq!(describe("const unsafe impl T for u8 {}", module), "impl u8 (0 items)");
		assert_eq!(describe("macro m($x:expr) { $x }", module), "macro m body=true");
		assert_eq!(describe("pub macro m { () => {} }", module), "macro m body=true");
		assert_eq!(describe("use {::a::b, c, ::d};", module), "use (3 elements, 2 rooted)");
		assert_eq!(describe("use {c::d, {::std::fmt}};", module), "use (2 elements, 1 rooted)");
		assert_eq!(
			describe("pub use {{::a, {}, {b}}, ::c::{d, e}, {{::f as g}}};", module),
			"use (4 elements, 3 rooted)"
		);
	}

	fn describe(source: &str, container: Container) -> String {
		let parts = Parts::split(&verbatim(source, container));

		match classify(&parts, container) {
			Shape::Trait(item) => format!("trait {} ({} items)", item.ident, item.items.len()),
			Shape::TraitAlias(item) => format!("trait alias {}", item.ident),
			Shape::Impl(item) => format!("impl {} ({} items)", item.self_ty.to_token_stream(), item.items.len()),
			Shape::Use(elements) => {
				let rooted = elements.iter().filter(|element| element.leading_colon.is_some()).count();

				format!("use ({} elements, {rooted} rooted)", elements.len())
			}

			Shape::Fn {
				name,
				receiver,
				is_const,
				is_async,
				is_unsafe,
				body,
			} => format!(
				"fn {name} receiver={receiver:?} const={is_const} async={is_async} unsafe={is_unsafe} body={}",
				body.is_some()
			),

			Shape::Const { name } => format!("const {name}"),
			Shape::Static { name, mutable } => format!("static {name} mut={mutable}"),
			Shape::Type { name } => format!("type {name}"),
			Shape::Macro { name, body } => format!("macro {name} body={}", body.is_some()),
			Shape::Unknown { body } => format!("unknown body={}", body.is_some()),
		}
	}

	#[test]
	fn falls_back_to_unknown() {
		let parts = Parts::split(&"#[a] something weird ( x ) ;".parse().unwrap());

		assert_eq!(parts.attrs.len(), 1);
		assert!(matches!(classify(&parts, Container::Module), Shape::Unknown { body: Some(_) }));

		let parts = Parts::split(&"const impl".parse().unwrap());

		assert!(matches!(classify(&parts, Container::Module), Shape::Unknown { body: None }));
		assert!(matches!(
			classify(&Parts::split(&TokenStream::new()), Container::Trait),
			Shape::Unknown { body: None }
		));
	}

	/// The `Verbatim` tokens of the first item of `source` in a container.
	fn verbatim(source: &str, container: Container) -> TokenStream {
		let file = match container {
			Container::Module => source.to_owned(),
			Container::Impl { .. } => format!("impl X {{ {source} }}"),
			Container::Trait => format!("trait X {{ {source} }}"),
			Container::Extern => format!("extern {{ {source} }}"),
		};

		let file = syn::parse_file(&file).unwrap();

		match (&file.items[0], container) {
			(Item::Verbatim(tokens), Container::Module) => tokens.clone(),
			(Item::Impl(item), _) => match &item.items[0] {
				ImplItem::Verbatim(tokens) => tokens.clone(),
				other => panic!("not verbatim: {}", other.to_token_stream()),
			},
			(Item::Trait(item), _) => match &item.items[0] {
				TraitItem::Verbatim(tokens) => tokens.clone(),
				other => panic!("not verbatim: {}", other.to_token_stream()),
			},
			(Item::ForeignMod(item), _) => match &item.items[0] {
				ForeignItem::Verbatim(tokens) => tokens.clone(),
				other => panic!("not verbatim: {}", other.to_token_stream()),
			},
			(other, _) => panic!("not verbatim: {}", other.to_token_stream()),
		}
	}
}
