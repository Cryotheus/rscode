//! The declarations of `thread_local!` invocations, which declare statics of the enclosing module.
//!
//! `thread_local! { [attributes] [visibility] static NAME: Type = [const] initializer; ... }` declares one
//! `static NAME: LocalKey<Type>` per declaration (the `;` after the last one is optional). Rather than expanding the
//! macro, its declarations are parsed like `static` items, so they can be found, viewed, and edited where they are
//! written.

use proc_macro2::TokenStream;
use quote::ToTokens;
use syn::Attribute;
use syn::Expr;
use syn::Ident;
use syn::ItemStatic;
use syn::Macro;
use syn::StaticMutability;
use syn::Token;
use syn::Type;
use syn::Visibility;
use syn::parse::Parse;
use syn::parse::ParseStream;

/// One declaration of a `thread_local!` invocation.
pub(crate) struct Declaration {
	pub(crate) attrs: Vec<Attribute>,
	pub(crate) vis: Visibility,
	pub(crate) static_token: Token![static],
	pub(crate) ident: Ident,
	pub(crate) colon_token: Token![:],
	pub(crate) ty: Type,
	pub(crate) eq_token: Token![=],

	/// The initializer: an expression, or a `const { ... }` block (which parses as an expression).
	pub(crate) expr: Expr,

	/// Optional after the last declaration.
	pub(crate) semi_token: Option<Token![;]>,
}

impl Declaration {
	/// The declaration as the `static` item it is written like (with a `;` that has no location if it has none).
	pub(crate) fn to_item(&self) -> ItemStatic {
		ItemStatic {
			attrs: self.attrs.clone(),
			vis: self.vis.clone(),
			static_token: self.static_token,
			mutability: StaticMutability::None,
			ident: self.ident.clone(),
			colon_token: self.colon_token,
			ty: Box::new(self.ty.clone()),
			eq_token: self.eq_token,
			expr: Box::new(self.expr.clone()),
			semi_token: self.semi_token.unwrap_or_default(),
		}
	}
}

impl Parse for Declaration {
	fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
		Ok(Self {
			attrs: input.call(Attribute::parse_outer)?,
			vis: input.parse()?,
			static_token: input.parse()?,
			ident: input.parse()?,
			colon_token: input.parse()?,
			ty: input.parse()?,
			eq_token: input.parse()?,
			expr: input.parse()?,
			semi_token: match input.is_empty() {
				true => input.parse()?,
				false => Some(input.parse()?),
			},
		})
	}
}

/// Only the tokens that were written: no `;` after a last declaration without one.
impl ToTokens for Declaration {
	fn to_tokens(&self, tokens: &mut TokenStream) {
		tokens.extend(self.attrs.iter().map(ToTokens::to_token_stream));
		self.vis.to_tokens(tokens);
		self.static_token.to_tokens(tokens);
		self.ident.to_tokens(tokens);
		self.colon_token.to_tokens(tokens);
		self.ty.to_tokens(tokens);
		self.eq_token.to_tokens(tokens);
		self.expr.to_tokens(tokens);
		self.semi_token.to_tokens(tokens);
	}
}

/// Parses the declarations of a list (a `thread_local!` body, or source replacing one declaration).
pub(crate) fn parse_declarations(input: ParseStream<'_>) -> syn::Result<Vec<Declaration>> {
	let mut declarations = Vec::new();

	while !input.is_empty() {
		declarations.push(input.parse()?);
	}

	Ok(declarations)
}

/// Whether a macro path names the standard library's `thread_local!`: `thread_local`, `std::thread_local`, or
/// `::std::thread_local`.
pub(crate) fn is_thread_local(mac: &Macro) -> bool {
	let segments: Vec<&syn::PathSegment> = mac.path.segments.iter().collect();

	match segments.as_slice() {
		[name] => mac.path.leading_colon.is_none() && name.ident == "thread_local",
		[krate, name] => krate.ident == "std" && name.ident == "thread_local",
		_ => false,
	}
}

/// The declarations of a `thread_local!` invocation, or `None` for another macro (or a body that does not parse).
pub(crate) fn declarations(mac: &Macro) -> Option<Vec<Declaration>> {
	match is_thread_local(mac) {
		true => mac.parse_body_with(parse_declarations).ok(),
		false => None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(source: &str) -> Option<Vec<Declaration>> {
		let item: syn::ItemMacro = syn::parse_str(source).unwrap();

		declarations(&item.mac)
	}

	fn names(source: &str) -> Option<Vec<String>> {
		parse(source).map(|declarations| declarations.iter().map(|declaration| declaration.ident.to_string()).collect())
	}

	#[test]
	fn parses_declarations() {
		let source = "thread_local! {
			/// Docs.
			#[allow(unused)]
			pub static A: Cell<u8> = Cell::new(0);
			static B: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
			pub(crate) static C: u8 = 1
		}";

		assert_eq!(names(source).unwrap(), ["A", "B", "C"]);

		let declarations = parse(source).unwrap();

		assert_eq!(declarations[0].attrs.len(), 2);
		assert!(matches!(declarations[0].vis, Visibility::Public(_)));
		assert!(matches!(declarations[1].expr, Expr::Const(_)));
		assert!(declarations[1].semi_token.is_some() && declarations[2].semi_token.is_none());
		assert_eq!(declarations[2].to_token_stream().to_string(), "pub (crate) static C : u8 = 1");
		assert_eq!(declarations[2].to_item().to_token_stream().to_string(), "pub (crate) static C : u8 = 1 ;");
	}

	#[test]
	fn only_the_standard_macro() {
		assert_eq!(names("std::thread_local!(static A: u8 = 0);").unwrap(), ["A"]);
		assert_eq!(names("::std::thread_local!(static A: u8 = 0;);").unwrap(), ["A"]);
		assert_eq!(names("thread_local!();").unwrap(), Vec::<String>::new());
		assert!(names("other::thread_local!(static A: u8 = 0);").is_none());
		assert!(names("my_thread_local!(static A: u8 = 0);").is_none());
		assert!(names("thread_local!(static A: u8 = 0 static B: u8 = 1);").is_none());
		assert!(names("thread_local!(static mut A: u8 = 0;);").is_none());
	}
}
