//! The entries of item-like macro invocations: bodies that are a sequence of `static` declarations, like those of
//! `lazy_static!` or of a project's own `commands!`.
//!
//! An entry is `[attributes] [visibility] static [ref] NAME [: Type] = value [;]`, where the value is the tokens up to a
//! `;` or to the start of the next entry (an outer attribute, a visibility, or `static`). A body is taken as entries
//! only when all of it is entries (all or nothing). The macro is not expanded: each entry is loaded as a `static` of
//! the invocation, so that it can be found, viewed, and edited where it is written, but it is bound in no scope (the
//! macro may declare anything).

use proc_macro2::Delimiter;
use proc_macro2::Spacing;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use quote::ToTokens;
use syn::Attribute;
use syn::Ident;
use syn::Macro;
use syn::Token;
use syn::Type;
use syn::Visibility;
use syn::parse::Parse;
use syn::parse::ParseStream;

/// One entry of an item-like macro invocation.
pub(crate) struct Entry {
	pub(crate) attrs: Vec<Attribute>,
	pub(crate) vis: Visibility,
	pub(crate) static_token: Token![static],

	/// `static ref`, as in `lazy_static!`.
	pub(crate) ref_token: Option<Token![ref]>,

	pub(crate) ident: Ident,

	/// `: Type`, if written.
	pub(crate) ty: Option<(Token![:], Type)>,

	pub(crate) eq_token: Token![=],

	/// The value: at least one token tree, up to a `;` or the start of the next entry.
	pub(crate) value: TokenStream,

	/// The `;` ending the value, if any.
	pub(crate) semi_token: Option<Token![;]>,
}

impl Parse for Entry {
	fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
		let attrs = input.call(Attribute::parse_outer)?;
		let vis = input.parse()?;
		let static_token = input.parse()?;
		let ref_token = input.parse()?;
		let ident = input.parse()?;
		let ty = match input.peek(Token![:]) {
			true => Some((input.parse()?, input.parse()?)),
			false => None,
		};
		let eq_token = input.parse()?;
		let value = input.step(|cursor| {
			let mut rest = *cursor;
			let mut value = TokenStream::new();
			let mut previous: Option<TokenTree> = None;

			while let Some((token, next)) = rest.token_tree() {
				let after = next.token_tree().map(|(after, _)| after);

				if is_semicolon(&token) || (!value.is_empty() && starts_entry(&token, previous.as_ref(), after)) {
					break;
				}

				value.extend([token.clone()]);
				previous = Some(token);
				rest = next;
			}

			Ok((value, rest))
		})?;

		if value.is_empty() {
			return Err(input.error("expected the value of the entry"));
		}

		Ok(Self {
			attrs,
			vis,
			static_token,
			ref_token,
			ident,
			ty,
			eq_token,
			value,
			semi_token: input.parse()?,
		})
	}
}

/// Only the tokens that were written: no `;` after an entry without one.
impl ToTokens for Entry {
	fn to_tokens(&self, tokens: &mut TokenStream) {
		tokens.extend(self.attrs.iter().map(ToTokens::to_token_stream));
		self.vis.to_tokens(tokens);
		self.static_token.to_tokens(tokens);
		self.ref_token.to_tokens(tokens);
		self.ident.to_tokens(tokens);

		if let Some((colon, ty)) = &self.ty {
			colon.to_tokens(tokens);
			ty.to_tokens(tokens);
		}

		self.eq_token.to_tokens(tokens);
		self.value.to_tokens(tokens);
		self.semi_token.to_tokens(tokens);
	}
}

/// The entries of a macro invocation (one or more), or `None` when its body is not entries through and through (or
/// it is a `macro_rules!` definition or a `thread_local!`, whose declarations are typed statics).
pub(crate) fn entries(mac: &Macro) -> Option<Vec<Entry>> {
	if mac.path.is_ident("macro_rules") || crate::load::thread_local::is_thread_local(mac) {
		return None;
	}

	mac.parse_body_with(parse_entries).ok().filter(|entries| !entries.is_empty())
}

/// Whether a token is a `;`, which ends a value.
fn is_semicolon(token: &TokenTree) -> bool {
	matches!(token, TokenTree::Punct(punct) if punct.as_char() == ';')
}

/// Parses a list of entries (a macro's body, or source replacing one entry).
pub(crate) fn parse_entries(input: ParseStream<'_>) -> syn::Result<Vec<Entry>> {
	let mut entries = Vec::new();

	while !input.is_empty() {
		entries.push(input.parse()?);
	}

	Ok(entries)
}

/// Whether a token of a value starts the next entry instead: `static` (not the lifetime `'static`), a visibility
/// (`pub`), or an outer attribute (`#` followed by brackets).
fn starts_entry(token: &TokenTree, previous: Option<&TokenTree>, next: Option<TokenTree>) -> bool {
	let after_quote =
		matches!(previous, Some(TokenTree::Punct(punct)) if punct.as_char() == '\'' && punct.spacing() == Spacing::Joint);

	match token {
		TokenTree::Ident(ident) => !after_quote && (ident == "static" || ident == "pub"),
		TokenTree::Punct(punct) if punct.as_char() == '#' => {
			matches!(next, Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Bracket)
		}
		_ => false,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn entries_end_before_the_next_one() {
		let mac: Macro = syn::parse_str("commands! { static A = x + y static B = 1; }").unwrap();
		let entries = entries(&mac).unwrap();

		assert_eq!(entries[0].value.to_string(), "x + y");
		assert!(entries[0].semi_token.is_none() && entries[1].semi_token.is_some());
	}

	/// The names of the entries of a `commands!` invocation with that body, if it has entries.
	fn names(body: &str) -> Option<Vec<String>> {
		let mac: Macro = syn::parse_str(&format!("commands! {{ {body} }}")).unwrap();

		entries(&mac).map(|entries| entries.iter().map(|entry| entry.ident.to_string()).collect())
	}

	#[test]
	fn other_bodies_have_no_entries() {
		assert_eq!(names(""), None);
		assert_eq!(names("fn f() {}"), None);
		assert_eq!(names("static A = 1; fn f() {}"), None);
		assert_eq!(names("static A ="), None);
		assert_eq!(names("static A = ; static B = 1;"), None);

		let thread_local: Macro = syn::parse_str("thread_local! { static A: u8 = 1; }").unwrap();

		assert!(entries(&thread_local).is_none());
	}

	#[test]
	fn parses_entries() {
		assert_eq!(names("static SAY = fn say() { x(); } static HELP = 2;"), Some(vec!["SAY".to_owned(), "HELP".to_owned()]));
		assert_eq!(names("pub static ref A: Vec<u8> = vec![]; static B: &'static str = \"b\""), Some(vec!["A".to_owned(), "B".to_owned()]));
		assert_eq!(names("/// Docs.\n#[cfg(x)] static C = 1 pub(crate) static D = 2"), Some(vec!["C".to_owned(), "D".to_owned()]));
		assert_eq!(names("static E = #[allow(x)] 1;"), Some(vec!["E".to_owned()]), "a value has at least one token");
		assert_eq!(names("static F = 1 #[cfg(x)] static G = 2"), Some(vec!["F".to_owned(), "G".to_owned()]));
	}
}
