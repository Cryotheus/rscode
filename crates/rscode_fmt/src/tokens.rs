//! Printing token streams as source text.
//!
//! Token streams can contain groups without delimiters ([`Delimiter::None`]), such as around an expression
//! interpolated by a `macro_rules!` macro. They group their tokens like parentheses do, but have no text: printing
//! `⟦a + b⟧ * 2` as `a + b * 2` would change its meaning. Such groups are given parentheses where they matter.

use crate::FormatError;
use crate::source::ensure_parses;
use proc_macro2::Delimiter;
use proc_macro2::Group;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use quote::ToTokens;
use syn::visit_mut;
use syn::visit_mut::VisitMut;

/// Prints a token stream containing a whole file's worth of items as source text, on a single line.
///
/// Groups without delimiters around expressions and types become parentheses unless the parentheses would be
/// redundant; the other such groups (as in the arguments of macros) become parentheses when they contain more than one
/// token tree.
pub(crate) fn to_source(tokens: TokenStream) -> Result<String, FormatError> {
	let mut file: syn::File = syn::parse2(tokens).map_err(|error| FormatError::from_syn(&error))?;

	Delimit {
		expressions: true,
	}
	.visit_file_mut(&mut file);

	let source = delimit(file.into_token_stream()).to_string();

	ensure_parses(&source, "printing tokens")?;

	Ok(source)
}

/// Gives parentheses to the groups without delimiters in the parts of a file that are kept as tokens (the arguments
/// of macros and attributes), and in types. Expressions are left alone: prettyplease parenthesizes them itself.
pub(crate) fn delimit_for_prettyplease(file: &mut syn::File) {
	Delimit {
		expressions: false,
	}
	.visit_file_mut(file);
}

/// Replaces the groups without delimiters of a syntax tree (see [`to_source`]).
struct Delimit {
	/// Whether to replace them around expressions.
	expressions: bool,
}

impl VisitMut for Delimit {
	fn visit_expr_mut(&mut self, expr: &mut syn::Expr) {
		visit_mut::visit_expr_mut(self, expr);

		if !self.expressions {
			return;
		}

		if let syn::Expr::Group(group) = expr {
			let inner = std::mem::replace(group.expr.as_mut(), syn::Expr::PLACEHOLDER);

			*expr = if group.attrs.is_empty() && is_atomic_expr(&inner) {
				inner
			} else {
				syn::Expr::Paren(syn::ExprParen {
					attrs: std::mem::take(&mut group.attrs),
					paren_token: Default::default(),
					expr: Box::new(inner),
				})
			};
		}
	}

	fn visit_type_mut(&mut self, ty: &mut syn::Type) {
		visit_mut::visit_type_mut(self, ty);

		if let syn::Type::Group(group) = ty {
			let inner = std::mem::replace(group.elem.as_mut(), syn::Type::Verbatim(TokenStream::new()));

			*ty = if group.attrs.is_empty() && is_atomic_type(&inner) {
				inner
			} else {
				syn::Type::Paren(syn::TypeParen {
					attrs: std::mem::take(&mut group.attrs),
					paren_token: Default::default(),
					elem: Box::new(inner),
				})
			};
		}
	}

	fn visit_macro_mut(&mut self, mac: &mut syn::Macro) {
		visit_mut::visit_macro_mut(self, mac);
		mac.tokens = delimit(std::mem::take(&mut mac.tokens));
	}

	fn visit_meta_list_mut(&mut self, meta: &mut syn::MetaList) {
		visit_mut::visit_meta_list_mut(self, meta);
		meta.tokens = delimit(std::mem::take(&mut meta.tokens));
	}
}

/// Whether an expression never needs parentheses to keep its meaning wherever it is.
fn is_atomic_expr(expr: &syn::Expr) -> bool {
	matches!(
		expr,
		syn::Expr::Array(_) | syn::Expr::Lit(_) | syn::Expr::Paren(_) | syn::Expr::Path(_) | syn::Expr::Repeat(_) | syn::Expr::Tuple(_)
	)
}

/// Whether a type never needs parentheses to keep its meaning wherever it is.
fn is_atomic_type(ty: &syn::Type) -> bool {
	matches!(
		ty,
		syn::Type::Array(_)
			| syn::Type::Infer(_)
			| syn::Type::Never(_)
			| syn::Type::Paren(_)
			| syn::Type::Path(_)
			| syn::Type::Slice(_)
			| syn::Type::Tuple(_)
	)
}

/// Replaces the groups without delimiters of a token stream: a group of a single token tree by that tree, and other
/// groups by parenthesized groups.
fn delimit(stream: TokenStream) -> TokenStream {
	stream
		.into_iter()
		.map(|tree| match tree {
			TokenTree::Group(group) => {
				let inner = delimit(group.stream());

				match group.delimiter() {
					Delimiter::None if inner.clone().into_iter().nth(1).is_none() => inner,
					Delimiter::None => TokenTree::Group(Group::new(Delimiter::Parenthesis, inner)).into(),
					delimiter => {
						let mut delimited = Group::new(delimiter, inner);

						delimited.set_span(group.span());
						TokenTree::Group(delimited).into()
					}
				}
			}
			tree => TokenStream::from(tree),
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use quote::quote;

	fn none(tokens: TokenStream) -> Group {
		Group::new(Delimiter::None, tokens)
	}

	#[test]
	fn parenthesizes_groups_without_delimiters_where_needed() {
		let sum = none(quote!(a + b));
		let variable = none(quote!(a));
		let object = none(quote!(dyn A + Send));
		let tokens = quote! {
			fn f(x: &#object) -> u8 {
				let y = #variable * 2;
				m!(#sum * 2);
				#sum * 2
			}
		};

		assert_eq!(
			to_source(tokens).unwrap(),
			"fn f (x : & (dyn A + Send)) -> u8 { let y = a * 2 ; m ! ((a + b) * 2) ; (a + b) * 2 }",
		);
	}

	#[test]
	fn delimits_only_what_prettyplease_does_not() {
		let sum = none(quote!(a + b));
		let mut file: syn::File = syn::parse2(quote!(fn f() { m!(#sum * 2); #sum * 2 })).unwrap();

		delimit_for_prettyplease(&mut file);

		let syn::Item::Fn(function) = &file.items[0] else { unreachable!() };

		assert!(matches!(&function.block.stmts[0], syn::Stmt::Macro(statement) if statement.mac.tokens.to_string() == "(a + b) * 2"));
		assert!(matches!(&function.block.stmts[1], syn::Stmt::Expr(syn::Expr::Binary(binary), None) if matches!(*binary.left, syn::Expr::Group(_))));
	}

	#[test]
	fn reports_invalid_tokens() {
		assert!(matches!(to_source(quote!(fn missing_body())), Err(FormatError::Parse { .. })));
	}
}
