//! Reading the attributes rscode cares about: `cfg`, `cfg_attr`, `path`, docs, and a few flags.

use super::Walker;
use crate::CfgExpr;
use crate::Tristate;
use crate::model::ItemAttrs;
use crate::source::TextRange;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use quote::ToTokens;
use syn::AttrStyle;
use syn::Attribute;
use syn::Expr;
use syn::ExprLit;
use syn::Lit;
use syn::Meta;

/// What rscode reads from an item's attributes.
#[derive(Debug, Default)]
pub(super) struct AttrSummary {
	/// Every `cfg` predicate, including those produced by `cfg_attr`s.
	pub(super) cfgs: Vec<CfgExpr>,

	/// Everything but [`ItemAttrs::after_attrs`].
	pub(super) attrs: ItemAttrs,

	/// Whether [`ItemAttrs::path`] applies regardless of `cfg_attr` predicates that could not be evaluated.
	path_is_definite: bool,
}

/// Where a meta item was found.
struct MetaContext {
	outer: bool,

	/// Whether the attributes belong to a module (only modules have `#[path]`).
	module: bool,

	/// The predicates of the enclosing `cfg_attr`s that could not be evaluated.
	conditions: Vec<CfgExpr>,
}

impl Walker<'_, '_, '_> {
	/// Reads attributes (outer and inner) of an item.
	pub(super) fn attributes(&mut self, attrs: &[Attribute], module: bool) -> AttrSummary {
		let mut summary = AttrSummary::default();

		for attr in attrs {
			let outer = matches!(attr.style, AttrStyle::Outer);

			if outer && is_doc_comment(&attr.meta) {
				let range = self.doc_range(attr);

				summary.attrs.docs = Some(summary.attrs.docs.map_or(range, |docs| docs.cover(range)));
			}

			let mut context = MetaContext {
				outer,
				module,
				conditions: Vec::new(),
			};

			self.meta(&attr.meta, &mut context, &mut summary);
		}

		summary
	}

	/// The range of a doc attribute. (proc-macro2 includes the `\r` of a CRLF line break in a `///` comment's span.)
	fn doc_range(&self, attr: &Attribute) -> TextRange {
		let range = self.range_of(attr);
		let text = self.parsed.slice(range);

		TextRange::new(range.start, range.start + text.trim_end_matches('\r').len())
	}

	fn meta(&mut self, meta: &Meta, context: &mut MetaContext, summary: &mut AttrSummary) {
		let Some(name) = meta.path().get_ident() else {
			return;
		};

		if name == "cfg" {
			let cfg = self.cfg(meta);

			summary.cfgs.push(conditional(&context.conditions, cfg));
		} else if name == "cfg_attr" {
			self.cfg_attr(meta, context, summary);
		} else if name == "path" {
			if context.outer && context.module {
				self.path_attr(meta, context, summary);
			}
		} else if name == "doc" {
			summary.attrs.doc_hidden |= matches!(meta, Meta::List(list) if has_word(&list.tokens, "hidden"));
		} else if name == "macro_export" {
			summary.attrs.macro_export = true;
		} else if name == "macro_use" {
			summary.attrs.macro_use = true;
		} else if name == "test" {
			summary.attrs.test = true;
		}
	}

	/// The predicate of `#[cfg(...)]`; unparsable predicates are kept as [`CfgExpr::Other`] with a warning.
	fn cfg(&mut self, meta: &Meta) -> CfgExpr {
		let error = match meta {
			Meta::List(list) => match CfgExpr::from_tokens(list.tokens.clone()) {
				Ok(cfg) => return cfg,
				Err(error) => error.to_string(),
			},

			_ => "expected `cfg(predicate)`".to_owned(),
		};

		let text = match meta {
			Meta::List(list) => self.compact_text(list.tokens.clone()),
			_ => self.compact_text(meta.to_token_stream()),
		};

		self.warning(self.range_of(meta).start, error);
		CfgExpr::Other(text)
	}

	/// Applies the attributes of `#[cfg_attr(predicate, attrs...)]` unless the predicate is false.
	fn cfg_attr(&mut self, meta: &Meta, context: &mut MetaContext, summary: &mut AttrSummary) {
		let parsed = match meta {
			Meta::List(list) => CfgExpr::parse_cfg_attr(list.tokens.clone()).map_err(|error| error.to_string()),
			_ => Err("expected `cfg_attr(predicate, attributes...)`".to_owned()),
		};

		let (predicate, attrs) = match parsed {
			Ok(parsed) => parsed,

			Err(error) => {
				self.warning(self.range_of(meta).start, error);
				return;
			}
		};

		let unknown = match self.loader.spec.cfg.eval(&predicate) {
			Tristate::False => return,
			Tristate::True => false,
			Tristate::Unknown => true,
		};

		if unknown {
			context.conditions.push(predicate);
		}

		for tokens in attrs {
			// attributes that are not meta items (`cfg_attr(x, some tokens)`) are none of rscode's business
			if let Ok(meta) = syn::parse2::<Meta>(tokens) {
				self.meta(&meta, context, summary);
			}
		}

		if unknown {
			context.conditions.pop();
		}
	}

	/// `#[path = "..."]`: the first one applies (like rustc, which warns about the others).
	fn path_attr(&mut self, meta: &Meta, context: &MetaContext, summary: &mut AttrSummary) {
		let offset = self.range_of(meta).start;

		let Some(path) = string_value(meta) else {
			self.warning(offset, "expected `path = \"file.rs\"`".to_owned());
			return;
		};

		match &summary.attrs.path {
			None => {
				if let Some(predicate) = all(&context.conditions) {
					self.warning(offset, format!("cannot tell whether `cfg_attr({predicate}, path = {path:?})` applies; assuming it does"));
				}

				summary.path_is_definite = context.conditions.is_empty();
				summary.attrs.path = Some(path);
			}

			Some(first) if summary.path_is_definite => {
				let message = format!("unused `path` attribute: the first one (`{first}`) applies");

				self.warning(offset, message);
			}

			Some(_) => {}
		}
	}
}

/// A doc comment (`///`, `/** */`) or `#[doc = ...]`.
fn is_doc_comment(meta: &Meta) -> bool {
	matches!(meta, Meta::NameValue(name_value) if name_value.path.is_ident("doc"))
}

/// A `cfg` that holds when either `conditions` do not or `cfg` does.
fn conditional(conditions: &[CfgExpr], cfg: CfgExpr) -> CfgExpr {
	match all(conditions) {
		Some(condition) => CfgExpr::Any(vec![CfgExpr::Not(Box::new(condition)), cfg]),
		None => cfg,
	}
}

fn all(conditions: &[CfgExpr]) -> Option<CfgExpr> {
	match conditions {
		[] => None,
		[condition] => Some(condition.clone()),
		conditions => Some(CfgExpr::All(conditions.to_vec())),
	}
}

/// The value of `name = "value"`.
fn string_value(meta: &Meta) -> Option<String> {
	match meta {
		Meta::NameValue(name_value) => match &name_value.value {
			Expr::Lit(ExprLit { lit: Lit::Str(value), .. }) => Some(value.value()),
			_ => None,
		},

		_ => None,
	}
}

/// Whether a comma-separated list contains `word` on its own (`hidden` in `doc(hidden, alias = "x")`).
fn has_word(tokens: &TokenStream, word: &str) -> bool {
	let tokens: Vec<TokenTree> = tokens.clone().into_iter().collect();

	tokens
		.split(|token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == ','))
		.any(|part| matches!(part, [TokenTree::Ident(ident)] if ident == word))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn finds_words_in_lists() {
		let tokens: TokenStream = "hidden, alias = \"x\"".parse().unwrap();

		assert!(has_word(&tokens, "hidden"));
		assert!(!has_word(&"alias = \"hidden\"".parse().unwrap(), "hidden"));
		assert!(!has_word(&"hidden = 1".parse().unwrap(), "hidden"));
	}

	#[test]
	fn builds_conditional_cfgs() {
		let unix = CfgExpr::Name("unix".into());
		let test = CfgExpr::Name("test".into());
		let feature = CfgExpr::KeyValue("feature".into(), "x".into());

		assert_eq!(conditional(&[], unix.clone()), unix);
		assert_eq!(conditional(std::slice::from_ref(&test), unix.clone()).to_string(), "any(not(test), unix)");
		assert_eq!(conditional(&[test, feature], unix).to_string(), r#"any(not(all(test, feature = "x")), unix)"#);
	}
}
