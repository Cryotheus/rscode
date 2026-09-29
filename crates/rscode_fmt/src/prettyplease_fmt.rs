//! Formatting with prettyplease.

use crate::FormatError;
use crate::contains_comments;
use crate::equivalence;
use crate::source::BOM;
use crate::source::uses_crlf;
use crate::source::with_line_breaks;
use crate::tokens;
use proc_macro2::Punct;
use proc_macro2::Spacing;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use std::panic::AssertUnwindSafe;
use syn::visit;
use syn::visit::Visit;
use syn::visit_mut;
use syn::visit_mut::VisitMut;

/// Macros that prettyplease prints its own way, with parentheses (brackets for `vec!`), when their arguments parse.
/// `thread_local!` is printed with braces and left out.
pub(crate) const REPRINTED_MACROS: &[&str] = &[
	"addr_of",
	"addr_of_mut",
	"assert",
	"assert_eq",
	"assert_ne",
	"cfg",
	"compile_error",
	"concat",
	"concat_bytes",
	"const_format_args",
	"dbg",
	"debug_assert",
	"debug_assert_eq",
	"debug_assert_ne",
	"env",
	"eprint",
	"eprintln",
	"format",
	"format_args",
	"format_args_nl",
	"include",
	"include_bytes",
	"include_str",
	"matches",
	"option_env",
	"panic",
	"print",
	"println",
	"todo",
	"unimplemented",
	"unreachable",
	"vec",
	"write",
	"writeln",
];

/// Works around prettyplease's printing of some macro invocations, without changing their meaning:
///
/// - Brace-delimited invocations of well-known macros (`compile_error! { .. }`) are printed with parentheses, but
///   without the semicolon parentheses require after an item or statement. The delimiters of a macro invocation do
///   not change its meaning, so such invocations get parentheses beforehand, and prettyplease prints the semicolon.
/// - The last declaration of a `thread_local!` invocation is left out when no semicolon follows it. `thread_local!`
///   accepts a semicolon after its last declaration, so one is added.
struct FixMacros;

impl FixMacros {
	fn parenthesize(mac: &mut syn::Macro) {
		let reprinted = |segment: &syn::PathSegment| REPRINTED_MACROS.iter().any(|name| segment.ident == name);

		if matches!(mac.delimiter, syn::MacroDelimiter::Brace(_)) && mac.path.segments.last().is_some_and(reprinted) {
			mac.delimiter = syn::MacroDelimiter::Paren(syn::token::Paren::default());
		}
	}
}

impl VisitMut for FixMacros {
	fn visit_foreign_item_macro_mut(&mut self, item: &mut syn::ForeignItemMacro) {
		Self::parenthesize(&mut item.mac);
		visit_mut::visit_foreign_item_macro_mut(self, item);
	}

	fn visit_impl_item_macro_mut(&mut self, item: &mut syn::ImplItemMacro) {
		Self::parenthesize(&mut item.mac);
		visit_mut::visit_impl_item_macro_mut(self, item);
	}

	fn visit_item_macro_mut(&mut self, item: &mut syn::ItemMacro) {
		// `macro_rules!` definitions have a name
		if item.ident.is_none() {
			Self::parenthesize(&mut item.mac);
		}

		visit_mut::visit_item_macro_mut(self, item);
	}

	fn visit_macro_mut(&mut self, mac: &mut syn::Macro) {
		let thread_local = mac.path.segments.last().is_some_and(|segment| segment.ident == "thread_local");
		let terminated = match mac.tokens.clone().into_iter().last() {
			Some(TokenTree::Punct(punct)) => punct.as_char() == ';',
			Some(_) => false,
			None => true,
		};

		if thread_local && !terminated {
			mac.tokens.extend([TokenTree::Punct(Punct::new(';', Spacing::Alone))]);
		}

		visit_mut::visit_macro_mut(self, mac);
	}

	fn visit_stmt_macro_mut(&mut self, statement: &mut syn::StmtMacro) {
		Self::parenthesize(&mut statement.mac);
		visit_mut::visit_stmt_macro_mut(self, statement);
	}

	fn visit_trait_item_macro_mut(&mut self, item: &mut syn::TraitItemMacro) {
		Self::parenthesize(&mut item.mac);
		visit_mut::visit_trait_item_macro_mut(self, item);
	}
}

/// Finds syntax that prettyplease cannot print: nodes `syn` does not model (`Verbatim`), and `macro_rules!`
/// definitions whose rules are not `(matcher) => {expansion};` sequences.
#[derive(Default)]
struct Unsupported {
	found: Option<String>,
}

impl Unsupported {
	fn found(&mut self, tokens: &TokenStream) {
		self.found.get_or_insert_with(|| tokens.to_string());
	}
}

impl<'ast> Visit<'ast> for Unsupported {
	fn visit_expr(&mut self, expr: &'ast syn::Expr) {
		match expr {
			// empty statements (`;`) are empty tokens, which prettyplease leaves out
			syn::Expr::Verbatim(tokens) if !tokens.is_empty() => self.found(tokens),
			_ => visit::visit_expr(self, expr),
		}
	}

	fn visit_foreign_item(&mut self, item: &'ast syn::ForeignItem) {
		match item {
			syn::ForeignItem::Verbatim(tokens) => self.found(tokens),
			_ => visit::visit_foreign_item(self, item),
		}
	}

	fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
		match item {
			syn::ImplItem::Verbatim(tokens) => self.found(tokens),
			_ => visit::visit_impl_item(self, item),
		}
	}

	fn visit_item(&mut self, item: &'ast syn::Item) {
		match item {
			syn::Item::Verbatim(tokens) => self.found(tokens),
			syn::Item::Macro(definition)
				if definition.ident.is_some() && definition.mac.path.is_ident("macro_rules") && !is_printable_macro_rules(&definition.mac.tokens) =>
			{
				self.found(&quote::ToTokens::to_token_stream(definition));
			}
			_ => visit::visit_item(self, item),
		}
	}

	fn visit_pat(&mut self, pat: &'ast syn::Pat) {
		match pat {
			syn::Pat::Verbatim(tokens) => self.found(tokens),
			_ => visit::visit_pat(self, pat),
		}
	}

	fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
		match item {
			syn::TraitItem::Verbatim(tokens) => self.found(tokens),
			_ => visit::visit_trait_item(self, item),
		}
	}

	fn visit_type(&mut self, ty: &'ast syn::Type) {
		match ty {
			syn::Type::Verbatim(tokens) => self.found(tokens),
			_ => visit::visit_type(self, ty),
		}
	}

	fn visit_type_param_bound(&mut self, bound: &'ast syn::TypeParamBound) {
		match bound {
			syn::TypeParamBound::Verbatim(tokens) => self.found(tokens),
			_ => visit::visit_type_param_bound(self, bound),
		}
	}
}

/// Formats a whole source file. A shebang, a byte order mark, and `\r\n` line breaks (going by the first line break)
/// are preserved.
///
/// Fails with [`FormatError::CommentsWouldBeLost`] if the source has non-doc comments, unless `allow_comment_loss`.
pub(crate) fn format_str(source: &str, allow_comment_loss: bool) -> Result<String, FormatError> {
	if !allow_comment_loss && contains_comments(source) {
		return Err(FormatError::CommentsWouldBeLost);
	}

	// `syn::File` keeps the shebang, and prettyplease prints it
	let file = syn::parse_file(source).map_err(|error| FormatError::from_syn_in(&error, source))?;
	let formatted = unparse(file)?;
	let mut formatted = if uses_crlf(source) {
		with_line_breaks(&formatted, true).into_owned()
	} else {
		formatted
	};

	if source.starts_with(BOM) {
		formatted.insert_str(0, BOM);
	}

	Ok(formatted)
}

/// Formats a token stream containing a whole file's worth of items.
///
/// Groups without delimiters are parenthesized where needed (see [`tokens`]).
pub(crate) fn format_tokens(tokens: TokenStream) -> Result<String, FormatError> {
	let mut file: syn::File = syn::parse2(tokens).map_err(|error| FormatError::from_syn(&error))?;

	tokens::delimit_for_prettyplease(&mut file);
	unparse(file)
}

/// Whether prettyplease can print the rules of a `macro_rules!` definition: a sequence of
/// `(matcher) => {expansion}` rules separated by `;`.
fn is_printable_macro_rules(rules: &TokenStream) -> bool {
	#[derive(Clone, Copy)]
	enum State {
		Start,
		Matcher,
		Equal,
		Greater,
		Expander,
	}

	let mut state = State::Start;

	for token in rules.clone() {
		let group = matches!(token, TokenTree::Group(_));
		let punct = |character: char, spacing: Spacing| matches!(&token, TokenTree::Punct(punct) if punct.as_char() == character && punct.spacing() == spacing);

		state = match state {
			State::Start if group => State::Matcher,
			State::Matcher if punct('=', Spacing::Joint) => State::Equal,
			State::Equal if punct('>', Spacing::Alone) => State::Greater,
			State::Greater if group => State::Expander,
			State::Expander if punct(';', Spacing::Alone) => State::Start,
			_ => return false,
		};
	}

	true
}

/// Prints a file with prettyplease, which panics on some syntax it does not support, and does not print some other
/// syntax.
///
/// Syntax that makes it panic is detected beforehand where possible; a panic is caught as a last resort (the panic
/// hook still runs). The output is checked to parse and to mean the same as the file (see [`equivalence`]).
fn unparse(mut file: syn::File) -> Result<String, FormatError> {
	let mut unsupported = Unsupported::default();

	unsupported.visit_file(&file);

	if let Some(tokens) = unsupported.found {
		return Err(FormatError::PrettyPlease(format!("unsupported syntax `{tokens}`")));
	}

	FixMacros.visit_file_mut(&mut file);

	let formatted = std::panic::catch_unwind(AssertUnwindSafe(|| prettyplease::unparse(&file))).map_err(|payload| {
		let message = match payload.downcast_ref::<&str>() {
			Some(message) => (*message).to_owned(),
			None => match payload.downcast_ref::<String>() {
				Some(message) => message.clone(),
				None => "prettyplease panicked".to_owned(),
			},
		};

		FormatError::PrettyPlease(message)
	})?;

	let reparsed = syn::parse_file(&formatted).map_err(|error| {
		let location = FormatError::from_syn(&error);

		FormatError::PrettyPlease(format!("prettyplease printed invalid code: {location}"))
	})?;

	equivalence::check(file, reparsed)
		.map_err(|difference| FormatError::PrettyPlease(format!("prettyplease would change the meaning of the code: {difference}")))?;

	Ok(formatted)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::str::FromStr;

	#[test]
	fn brace_delimited_known_macros_get_semicolons() {
		let cases = [
			("compile_error! { \"no\" }\nstruct S;\n", "compile_error!(\"no\");\nstruct S;\n"),
			(
				"fn f() {\n    println! { \"a\" }\n    let x = vec! { 1 };\n}\n",
				"fn f() {\n    println!(\"a\");\n    let x = vec![1];\n}\n",
			),
			(
				"impl S {\n    compile_error! { \"no\" }\n}\n",
				"impl S {\n    compile_error!(\"no\");\n}\n",
			),
			// other macros keep their braces
			("my_macro! { a b }\n", "my_macro! {\n    a b\n}\n"),
			("thread_local! { static X: u8 = 1; }\n", "thread_local! {\n    static X: u8 = 1;\n}\n"),
			// prettyplease leaves out a last declaration without a semicolon
			("thread_local!(static X: u8 = 1);\n", "thread_local! {\n    static X: u8 = 1;\n}\n"),
			(
				"fn f() {\n    thread_local!(static X: u8 = 1; static Y: u8 = 2);\n}\n",
				"fn f() {\n    thread_local! {\n        static X: u8 = 1;\n        static Y: u8 = 2;\n    }\n}\n",
			),
			("macro_rules! println {\n    () => {};\n}\n", "macro_rules! println {\n    () => {};\n}\n"),
		];

		for (source, expected) in cases {
			assert_eq!(format_str(source, false).unwrap(), expected, "{source}");
		}
	}

	#[test]
	fn doc_comments_are_kept() {
		let source = "//! Inner docs.\n\n/// Docs.\nfn  a( ) {}\n";

		assert_eq!(format_str(source, false).unwrap(), "//! Inner docs.\n/// Docs.\nfn a() {}\n");
	}

	#[test]
	fn formats_tokens() {
		let tokens = quote::quote! {
			pub struct Foo { a: u8 }
			impl Foo { pub fn new() -> Self { Self { a: 0 } } }
		};

		assert_eq!(
			format_tokens(tokens).unwrap(),
			"pub struct Foo {\n    a: u8,\n}\nimpl Foo {\n    pub fn new() -> Self {\n        Self { a: 0 }\n    }\n}\n",
		);
	}

	#[test]
	fn macro_rules_are_printed() {
		let source = "macro_rules! m {\n    () => {};\n    ($x:expr) => { $x };\n}\n";

		assert!(format_str(source, false).unwrap().starts_with("macro_rules! m {"));
	}

	#[test]
	fn macro_rules_validation() {
		let rules = |source: &str| is_printable_macro_rules(&TokenStream::from_str(source).unwrap());

		assert!(rules(""));
		assert!(rules("() => {}"));
		assert!(rules("() => {};"));
		assert!(rules("(a) => {}; [b] => (c); {d} => [e]"));
		assert!(!rules("() => {} () => {}"));
		assert!(!rules("() = > {}"));
		assert!(!rules("x"));

		// an incomplete last rule is printed as is
		assert!(rules("() =>"));
	}

	#[test]
	fn preserves_bom_and_shebang() {
		let source = "\u{feff}#!/usr/bin/env run-cargo-script\nfn  a( ) {}\n";

		assert_eq!(format_str(source, false).unwrap(), "\u{feff}#!/usr/bin/env run-cargo-script\nfn a() {}\n");
	}

	#[test]
	fn refuses_to_lose_comments() {
		let source = "// comment\nfn  a( ) {}\n";

		assert!(matches!(format_str(source, false), Err(FormatError::CommentsWouldBeLost)));
		assert_eq!(format_str(source, true).unwrap(), "fn a() {}\n");
	}

	#[test]
	fn refuses_unsupported_syntax() {
		// each of these would make prettyplease panic
		for source in [
			"const trait T {}",
			"impl(crate) trait R {}",
			"macro_rules! m { garbage }",
			"fn f() { macro_rules! m { () => {} () => {} } }",
		] {
			match format_str(source, true) {
				Err(FormatError::PrettyPlease(message)) => assert!(message.contains("unsupported syntax"), "{source}: {message}"),
				other => panic!("{source}: unexpected result {other:?}"),
			}
		}
	}

	#[test]
	fn reports_parse_errors_with_locations() {
		match format_str("fn a() {}\nfn b( {}\n", true) {
			Err(FormatError::Parse { line, .. }) => assert_eq!(line, 2),
			other => panic!("unexpected result: {other:?}"),
		}
	}

	#[test]
	fn token_parse_errors() {
		let tokens = TokenStream::from_str("fn missing_body()").unwrap();

		assert!(matches!(format_tokens(tokens), Err(FormatError::Parse { .. })));
	}
}
