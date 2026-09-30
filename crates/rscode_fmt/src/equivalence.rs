//! Checking that formatting did not change the meaning of a file.
//!
//! The syntax trees of a file before and after formatting are normalized, then compared token by token. The
//! normalization removes the differences a formatter may make without changing meaning:
//!
//! - spans, whitespace, and the spacing of punctuation that does not form an operator with the next one;
//! - parentheses: the structure of expressions, types, and patterns is compared instead, so parentheses that change
//!   how an expression is grouped are a difference;
//! - optional punctuation: trailing commas and `+`, the leading `|` of or-patterns, the commas after match arms, braces
//!   around a single import, and `::` before generic arguments in types;
//! - braces around the body of a match arm or closure that is a single expression;
//! - optional semicolons: after macro invocations that are not the last statement of a block and after item macros,
//!   after `if` without `else` and loops, after `return`, `break`, `continue`, and assignments, and empty statements;
//! - how doc comments are written (`///`, `/** */`, or `#[doc = ".."]`), and spaces at the ends of their lines;
//! - the delimiters of the standard library macros prettyplease prints its own way, such as `vec!`, and of the
//!   expansions of `macro_rules!` rules, and the semicolon after the last rule of `macro_rules!` and after the last
//!   declaration of `thread_local!`;
//! - `dyn` on trait objects, and `pub(in crate)`, `pub(in self)`, and `pub(in super)`.
//!
//! Everything else is a difference, such as a keyword prettyplease does not print.

use crate::prettyplease_fmt::REPRINTED_MACROS;
use crate::tree::drop_redundant_in;
use proc_macro2::Delimiter;
use proc_macro2::Group;
use proc_macro2::Spacing;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use quote::ToTokens;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::visit_mut;
use syn::visit_mut::VisitMut;

/// How many tokens around a difference are shown on each side.
const CONTEXT: usize = 8;

/// Pairs of punctuation characters that form (part of) an operator when joined, such as `&&` (not `& &`) or `..=`.
const OPERATOR_PAIRS: &[[char; 2]] = &[
	['&', '&'],
	['|', '|'],
	[':', ':'],
	['-', '>'],
	['=', '>'],
	['=', '='],
	['!', '='],
	['<', '='],
	['>', '='],
	['<', '<'],
	['>', '>'],
	['+', '='],
	['-', '='],
	['*', '='],
	['/', '='],
	['%', '='],
	['^', '='],
	['&', '='],
	['|', '='],
	['.', '.'],
	['.', '='],
];

/// Normalizes a syntax tree (see the module documentation).
struct Normalize;

impl VisitMut for Normalize {
	fn visit_angle_bracketed_generic_arguments_mut(&mut self, arguments: &mut syn::AngleBracketedGenericArguments) {
		// `::<` is optional in types (and required in expressions, which parse differently without it)
		arguments.colon2_token = None;
		untrail(&mut arguments.args);
		visit_mut::visit_angle_bracketed_generic_arguments_mut(self, arguments);
	}

	fn visit_arm_mut(&mut self, arm: &mut syn::Arm) {
		arm.comma = Some(Default::default());
		unwrap_block(&mut arm.body);
		visit_mut::visit_arm_mut(self, arm);
	}

	fn visit_attribute_mut(&mut self, attribute: &mut syn::Attribute) {
		normalize_doc(attribute);
		visit_mut::visit_attribute_mut(self, attribute);
	}

	fn visit_block_mut(&mut self, block: &mut syn::Block) {
		block.stmts.retain(|statement| !is_empty_statement(statement));

		let last = block.stmts.len().saturating_sub(1);

		for (index, statement) in block.stmts.iter_mut().enumerate() {
			normalize_semicolon(statement, index == last);
		}

		visit_mut::visit_block_mut(self, block);
	}

	fn visit_expr_closure_mut(&mut self, closure: &mut syn::ExprClosure) {
		untrail(&mut closure.inputs);

		// the body of a closure with a return type must be a block
		if matches!(closure.output, syn::ReturnType::Default) {
			unwrap_block(&mut closure.body);
		}

		visit_mut::visit_expr_closure_mut(self, closure);
	}

	fn visit_expr_mut(&mut self, expr: &mut syn::Expr) {
		loop {
			let inner = match expr {
				syn::Expr::Paren(paren) if paren.attrs.is_empty() => &mut paren.expr,
				syn::Expr::Group(group) if group.attrs.is_empty() => &mut group.expr,
				_ => break,
			};

			*expr = std::mem::replace(inner.as_mut(), syn::Expr::PLACEHOLDER);
		}

		visit_mut::visit_expr_mut(self, expr);
		*expr = syn::Expr::Verbatim(grouped(&*expr));
	}

	fn visit_foreign_item_macro_mut(&mut self, item: &mut syn::ForeignItemMacro) {
		item.semi_token = Some(Default::default());
		visit_mut::visit_foreign_item_macro_mut(self, item);
	}

	fn visit_generics_mut(&mut self, generics: &mut syn::Generics) {
		untrail(&mut generics.params);

		if let Some(where_clause) = &mut generics.where_clause {
			untrail(&mut where_clause.predicates);
		}

		visit_mut::visit_generics_mut(self, generics);
	}

	fn visit_impl_item_macro_mut(&mut self, item: &mut syn::ImplItemMacro) {
		item.semi_token = Some(Default::default());
		visit_mut::visit_impl_item_macro_mut(self, item);
	}

	fn visit_item_macro_mut(&mut self, item: &mut syn::ItemMacro) {
		item.semi_token = Some(Default::default());

		if item.ident.is_some() && item.mac.path.is_ident("macro_rules") {
			item.mac.delimiter = syn::MacroDelimiter::Brace(Default::default());
			item.mac.tokens = without_trailing_semicolon(normalize_rules(std::mem::take(&mut item.mac.tokens)));
		}

		visit_mut::visit_item_macro_mut(self, item);
	}

	fn visit_item_trait_mut(&mut self, item: &mut syn::ItemTrait) {
		untrail(&mut item.supertraits);
		visit_mut::visit_item_trait_mut(self, item);
	}

	fn visit_macro_mut(&mut self, mac: &mut syn::Macro) {
		let reprinted = mac
			.path
			.segments
			.last()
			.is_some_and(|segment| segment.ident == "thread_local" || REPRINTED_MACROS.iter().any(|name| segment.ident == name));

		if is_thread_local(mac) {
			mac.tokens = without_trailing_semicolon(std::mem::take(&mut mac.tokens));
		}

		if reprinted {
			mac.delimiter = syn::MacroDelimiter::Paren(Default::default());

			// prettyplease prints the arguments as expressions, when they are
			if let Ok(mut arguments) = Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated.parse2(mac.tokens.clone()) {
				for argument in &mut arguments {
					self.visit_expr_mut(argument);
				}

				untrail(&mut arguments);
				mac.tokens = arguments.into_token_stream();
			}
		}

		visit_mut::visit_macro_mut(self, mac);
	}

	fn visit_pat_mut(&mut self, pat: &mut syn::Pat) {
		loop {
			let inner = match pat {
				syn::Pat::Paren(paren) if paren.attrs.is_empty() => &mut paren.pat,
				_ => break,
			};

			*pat = std::mem::replace(inner.as_mut(), syn::Pat::Verbatim(TokenStream::new()));
		}

		visit_mut::visit_pat_mut(self, pat);
		*pat = syn::Pat::Verbatim(grouped(&*pat));
	}

	fn visit_pat_or_mut(&mut self, pat: &mut syn::PatOr) {
		pat.leading_vert = None;
		visit_mut::visit_pat_or_mut(self, pat);
	}

	fn visit_predicate_type_mut(&mut self, predicate: &mut syn::PredicateType) {
		untrail(&mut predicate.bounds);
		visit_mut::visit_predicate_type_mut(self, predicate);
	}

	fn visit_trait_item_macro_mut(&mut self, item: &mut syn::TraitItemMacro) {
		item.semi_token = Some(Default::default());
		visit_mut::visit_trait_item_macro_mut(self, item);
	}

	fn visit_type_impl_trait_mut(&mut self, ty: &mut syn::TypeImplTrait) {
		untrail(&mut ty.bounds);
		visit_mut::visit_type_impl_trait_mut(self, ty);
	}

	fn visit_type_mut(&mut self, ty: &mut syn::Type) {
		loop {
			let inner = match ty {
				syn::Type::Paren(paren) if paren.attrs.is_empty() => &mut paren.elem,
				syn::Type::Group(group) if group.attrs.is_empty() => &mut group.elem,
				_ => break,
			};

			*ty = std::mem::replace(inner.as_mut(), syn::Type::Verbatim(TokenStream::new()));
		}

		visit_mut::visit_type_mut(self, ty);
		*ty = syn::Type::Verbatim(grouped(&*ty));
	}

	fn visit_type_param_mut(&mut self, param: &mut syn::TypeParam) {
		untrail(&mut param.bounds);
		visit_mut::visit_type_param_mut(self, param);
	}

	fn visit_type_trait_object_mut(&mut self, object: &mut syn::TypeTraitObject) {
		object.dyn_token = Some(Default::default());
		untrail(&mut object.bounds);
		visit_mut::visit_type_trait_object_mut(self, object);
	}

	fn visit_use_tree_mut(&mut self, tree: &mut syn::UseTree) {
		// braces around one import other than `self`
		while let syn::UseTree::Group(group) = tree
			&& group.items.len() == 1
			&& !imports_self(&group.items[0])
		{
			match group.items.pop() {
				Some(item) => *tree = item,
				None => break,
			}
		}

		visit_mut::visit_use_tree_mut(self, tree);
	}

	fn visit_vis_restricted_mut(&mut self, restricted: &mut syn::VisRestricted) {
		drop_redundant_in(restricted);
		visit_mut::visit_vis_restricted_mut(self, restricted);
	}
}

/// A token of a normalized token stream.
#[derive(Debug, PartialEq, Eq)]
enum Token {
	Open(Delimiter),
	Close(Delimiter),
	Ident(String),

	/// A punctuation character, and whether it is joined with the next one to form an operator.
	Punct(char, bool),

	Literal(String),
}

impl Token {
	fn text(&self) -> String {
		let delimiter = |delimiter: &Delimiter, open: bool| {
			let (open_text, close_text) = match delimiter {
				Delimiter::Parenthesis => ("(", ")"),
				Delimiter::Brace => ("{", "}"),
				Delimiter::Bracket => ("[", "]"),
				Delimiter::None => ("", ""),
			};

			if open { open_text } else { close_text }.to_owned()
		};

		match self {
			Self::Open(open) => delimiter(open, true),
			Self::Close(close) => delimiter(close, false),
			Self::Ident(text) | Self::Literal(text) => text.clone(),
			Self::Punct(character, _) => character.to_string(),
		}
	}
}

/// Checks that `after` has the same meaning as `before`. On a difference, describes it: "`..` became `..`".
pub(crate) fn check(mut before: syn::File, mut after: syn::File) -> Result<(), String> {
	if before.shebang != after.shebang {
		return Err(format!("the shebang {:?} became {:?}", before.shebang, after.shebang));
	}

	Normalize.visit_file_mut(&mut before);
	Normalize.visit_file_mut(&mut after);

	let mut before_tokens = Vec::new();
	let mut after_tokens = Vec::new();

	flatten(before.into_token_stream(), &mut before_tokens);
	flatten(after.into_token_stream(), &mut after_tokens);

	let difference = before_tokens.iter().zip(&after_tokens).position(|(before, after)| before != after);

	match difference {
		None if before_tokens.len() == after_tokens.len() => Ok(()),

		difference => {
			let at = difference.unwrap_or_else(|| before_tokens.len().min(after_tokens.len()));

			Err(format!("`{}` became `{}`", excerpt(&before_tokens, at), excerpt(&after_tokens, at)))
		}
	}
}

/// Whether an expression has the same meaning with or without a semicolon after it at the end of a block: it
/// assigns, or it leaves the block.
fn ends_statement(expr: &syn::Expr) -> bool {
	match expr {
		syn::Expr::Assign(_) | syn::Expr::Break(_) | syn::Expr::Continue(_) | syn::Expr::Return(_) | syn::Expr::Yield(_) => true,

		syn::Expr::Binary(binary) => matches!(
			binary.op,
			syn::BinOp::AddAssign(_)
				| syn::BinOp::SubAssign(_)
				| syn::BinOp::MulAssign(_)
				| syn::BinOp::DivAssign(_)
				| syn::BinOp::RemAssign(_)
				| syn::BinOp::BitXorAssign(_)
				| syn::BinOp::BitAndAssign(_)
				| syn::BinOp::BitOrAssign(_)
				| syn::BinOp::ShlAssign(_)
				| syn::BinOp::ShrAssign(_)
		),

		syn::Expr::Group(group) => ends_statement(&group.expr),
		_ => false,
	}
}

/// The tokens around `at`, as text.
fn excerpt(tokens: &[Token], at: usize) -> String {
	let start = at.saturating_sub(CONTEXT);
	let end = (at + CONTEXT + 1).min(tokens.len());
	let mut text = if start > 0 { "..".to_owned() } else { String::new() };
	let mut joined = true;

	for token in &tokens[start..end] {
		let token_text = token.text();

		if token_text.is_empty() {
			continue;
		}

		if !joined {
			text.push(' ');
		}

		text.push_str(&token_text);
		joined = matches!(token, Token::Punct(_, true));
	}

	if end < tokens.len() {
		text.push_str(" ..");
	}

	text
}

/// Flattens a token stream into normalized tokens.
///
/// Groups without delimiters around a single token tree are left out, as they group nothing. A trailing comma in a
/// group is left out. The spacing of punctuation is kept only when it joins an operator.
fn flatten(stream: TokenStream, tokens: &mut Vec<Token>) {
	let mut trees = stream.into_iter().peekable();

	while let Some(tree) = trees.next() {
		match tree {
			TokenTree::Group(group) => {
				let delimiter = group.delimiter();
				let inner = group.stream();

				if delimiter == Delimiter::None && inner.clone().into_iter().nth(1).is_none() {
					flatten(inner, tokens);
					continue;
				}

				tokens.push(Token::Open(delimiter));

				let start = tokens.len();

				flatten(inner, tokens);

				if tokens.len() > start && tokens.last() == Some(&Token::Punct(',', false)) {
					tokens.pop();
				}

				tokens.push(Token::Close(delimiter));
			}

			TokenTree::Ident(ident) => tokens.push(Token::Ident(ident.to_string())),

			TokenTree::Punct(punct) => {
				let character = punct.as_char();
				let joined = punct.spacing() == Spacing::Joint
					&& matches!(trees.peek(), Some(TokenTree::Punct(next)) if OPERATOR_PAIRS.contains(&[character, next.as_char()]));

				tokens.push(Token::Punct(character, joined));
			}

			TokenTree::Literal(literal) => tokens.push(Token::Literal(literal.to_string())),
		}
	}
}

/// The tokens of a node in a group without delimiters, which makes the structure of the syntax tree part of the
/// tokens: `a + b * c` and `(a + b) * c` differ.
fn grouped(node: &impl ToTokens) -> TokenStream {
	TokenTree::Group(Group::new(Delimiter::None, node.to_token_stream())).into()
}

/// Whether a use tree is `self` or `self as name`, which needs braces around it.
fn imports_self(tree: &syn::UseTree) -> bool {
	match tree {
		syn::UseTree::Name(name) => name.ident == "self",
		syn::UseTree::Rename(rename) => rename.ident == "self",
		_ => false,
	}
}

/// Whether a statement is an empty statement (a lone `;`).
fn is_empty_statement(statement: &syn::Stmt) -> bool {
	matches!(statement, syn::Stmt::Expr(syn::Expr::Verbatim(tokens), Some(_)) if tokens.is_empty())
}

fn is_thread_local(mac: &syn::Macro) -> bool {
	mac.path.segments.last().is_some_and(|segment| segment.ident == "thread_local")
}

/// Whether an expression is a block-like expression that is always of the unit type (a loop other than `loop`, or
/// an `if` without `else`), so a semicolon after it changes nothing.
fn is_unit_block(expr: &syn::Expr) -> bool {
	match expr {
		syn::Expr::ForLoop(_) | syn::Expr::While(_) => true,

		syn::Expr::If(expr_if) => match &expr_if.else_branch {
			Some((_, else_branch)) => is_unit_block(else_branch),
			None => true,
		},

		syn::Expr::Group(group) => is_unit_block(&group.expr),
		_ => false,
	}
}

/// Writes the value of a doc comment attribute as a plain string literal, without spaces at the ends of its lines.
fn normalize_doc(attribute: &mut syn::Attribute) {
	let syn::Meta::NameValue(meta) = &mut attribute.meta else {
		return;
	};

	if !meta.path.is_ident("doc") {
		return;
	}

	if let syn::Expr::Lit(syn::ExprLit {
		attrs,
		lit: syn::Lit::Str(doc),
	}) = &mut meta.value
		&& attrs.is_empty()
	{
		let value = doc
			.value()
			.split('\n')
			.map(|line| line.trim_end_matches(' '))
			.collect::<Vec<_>>()
			.join("\n");

		*doc = syn::LitStr::new(&value, doc.span());
	}
}

/// Puts the expansions of the rules of a `macro_rules!` definition in braces, as prettyplease prints them.
fn normalize_rules(rules: TokenStream) -> TokenStream {
	let mut trees: Vec<TokenTree> = rules.into_iter().collect();

	for index in 2..trees.len() {
		let expansion = matches!(&trees[index - 2], TokenTree::Punct(punct) if punct.as_char() == '=')
			&& matches!(&trees[index - 1], TokenTree::Punct(punct) if punct.as_char() == '>');

		if expansion && let TokenTree::Group(group) = &trees[index] {
			trees[index] = TokenTree::Group(Group::new(Delimiter::Brace, group.stream()));
		}
	}

	trees.into_iter().collect()
}

/// Adds or removes semicolons that do not change the meaning of a statement (`last` in its block).
fn normalize_semicolon(statement: &mut syn::Stmt, last: bool) {
	match statement {
		syn::Stmt::Expr(expr, semicolon @ None) if ends_statement(expr) => *semicolon = Some(Default::default()),
		syn::Stmt::Expr(expr, semicolon @ Some(_)) if is_unit_block(expr) => *semicolon = None,

		// `thread_local!` declares items, so it has no value even at the end of a block
		syn::Stmt::Macro(statement) if !last || is_thread_local(&statement.mac) => statement.semi_token = Some(Default::default()),

		_ => {}
	}
}

/// Removes trailing punctuation.
fn untrail<T, P>(punctuated: &mut Punctuated<T, P>) {
	if punctuated.trailing_punct() {
		punctuated.pop_punct();
	}
}

/// Replaces a block that only has a tail expression (or a statement whose semicolon is implied, or a macro
/// invocation without a semicolon), such as `{ a + b }`, by the expression, and an empty block by `()`. Where a block
/// is optional, as the body of a match arm or closure, both mean the same.
fn unwrap_block(expr: &mut syn::Expr) {
	while let syn::Expr::Block(block) = expr
		&& block.attrs.is_empty()
		&& block.label.is_none()
	{
		let [statement] = block.block.stmts.as_mut_slice() else {
			// `{}` is `()`
			if block.block.stmts.is_empty() {
				*expr = syn::Expr::Tuple(syn::ExprTuple {
					attrs: Vec::new(),
					paren_token: Default::default(),
					elems: Punctuated::new(),
				});
			}

			return;
		};

		let inner = match statement {
			syn::Stmt::Expr(inner, None) => std::mem::replace(inner, syn::Expr::PLACEHOLDER),
			syn::Stmt::Expr(inner, Some(_)) if ends_statement(inner) => std::mem::replace(inner, syn::Expr::PLACEHOLDER),

			syn::Stmt::Macro(statement) if statement.semi_token.is_none() => syn::Expr::Macro(syn::ExprMacro {
				attrs: std::mem::take(&mut statement.attrs),
				mac: statement.mac.clone(),
			}),

			_ => return,
		};

		*expr = inner;
	}
}

/// Removes a semicolon at the end of the tokens of a macro invocation, which `macro_rules!` and `thread_local!` accept
/// but do not need.
fn without_trailing_semicolon(tokens: TokenStream) -> TokenStream {
	let mut trees: Vec<TokenTree> = tokens.into_iter().collect();

	if matches!(trees.last(), Some(TokenTree::Punct(punct)) if punct.as_char() == ';') {
		trees.pop();
	}

	trees.into_iter().collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn accepts_changes_that_keep_the_meaning() {
		let cases = [
			("fn  f( a:u8, ) ->u8 {a}", "fn f(a: u8) -> u8 {\n    a\n}"),
			("struct S { a: u8 }", "struct S {\n    a: u8,\n}"),
			("/// Docs.  \n#[doc = r\"More.\"]\nfn f() {}", "#[doc = \" Docs.\"]\n///More.\nfn f() {}"),
			(
				"fn f() { match x { | A | B => {}, C => 1 } }",
				"fn f() { match x { A | B => (), C => 1, } }",
			),
			(
				"fn f() { match x { A => { a + 1 } B => { return; } C => { m!() } } }",
				"fn f() { match x { A => a + 1, B => return, C => m!(), } }",
			),
			(
				"fn f() { let g = |a| { a.b() }; let h = || -> u8 { 1 }; }",
				"fn f() { let g = |a| a.b(); let h = || -> u8 { 1 }; }",
			),
			(
				"fn f() { assert!(x.f(|a| a + 1, size_of::<A::<u8>>(),)); }",
				"fn f() { assert!(x.f(|a| { a + 1 }, size_of::<A<u8>>())); }",
			),
			("thread_local!(static X: u8 = 1);", "thread_local! { static X: u8 = 1; }"),
			(
				"macro_rules! m { () => ( 1 ); ($a:expr) => [ $a ] }",
				"macro_rules! m {\n    () => { 1 };\n    ($a:expr) => { $a };\n}",
			),
			(
				"fn f() { if a { b(); }; for x in y {}; x = 1 }",
				"fn f() { if a { b(); } for x in y {} x = 1; }",
			),
			("fn f() { a();; b(); }", "fn f() { a(); b(); }"),
			(
				"fn f() { println! { \"a\" } let v = vec!(1,); }",
				"fn f() { println!(\"a\"); let v = vec![1]; }",
			),
			("compile_error! { \"no\" }", "compile_error!(\"no\");"),
			("fn f<T: A +,>() where T: B, {}", "fn f<T: A>() where T: B {}"),
			("fn f(x: Box<dyn A + Send>) -> impl B + {}", "fn f(x: Box<dyn A + Send>) -> impl B {}"),
			("pub(in crate) fn f() {}", "pub(crate) fn f() {}"),
			("use a::{b::{c},};\nuse d::{self};", "use a::b::c;\nuse d::{self};"),
			("fn f() { let x = a==-1; let y = |a,| a; }", "fn f() { let x = a == -1; let y = |a| a; }"),
			("fn f() { m!(a,-1); assert_eq!(a,-1) }", "fn f() { m!(a, -1); assert_eq!(a, -1) }"),
			(
				"#!/bin/run\nfn f() { let x = (1 + 2) * 3; }",
				"#!/bin/run\nfn f() {\n    let x = (1 + 2) * 3;\n}",
			),
		];

		for (before, after) in cases {
			assert_eq!(check_str(before, after), Ok(()), "{before}");
		}
	}

	fn check_str(before: &str, after: &str) -> Result<(), String> {
		check(syn::parse_file(before).unwrap(), syn::parse_file(after).unwrap())
	}

	#[test]
	fn describes_the_difference() {
		let message = check_str(
			"unsafe extern \"C\" { pub safe static X: u8; }\nfn keep() {}",
			"unsafe extern \"C\" { pub static X: u8; }\nfn keep() {}",
		)
		.unwrap_err();

		assert!(
			message.contains("`unsafe extern \"C\" { pub safe static X : u8 ; } fn keep ..` became"),
			"{message}"
		);
		assert!(
			message.ends_with("`unsafe extern \"C\" { pub static X : u8 ; } fn keep ( ..`"),
			"{message}"
		);
	}

	#[test]
	fn detects_changes_of_meaning() {
		let cases = [
			// syntax prettyplease 0.3.0 does not print
			(
				"unsafe extern \"C\" { pub safe static X: u8; }",
				"unsafe extern \"C\" { pub static X: u8; }",
			),
			("struct S { a: u8 = 1 }", "struct S { a: u8 }"),
			("fn f() { S { .. }; }", "fn f() { S {}; }"),
			// grouping
			("fn f() { let x = (1 + 2) * 3; }", "fn f() { let x = 1 + 2 * 3; }"),
			// tuples of one element are not parenthesized values
			("fn f() { let x = (1,); }", "fn f() { let x = (1); }"),
			("fn f((a,): (u8,)) {}", "fn f((a): (u8)) {}"),
			// operators
			("fn f() { a && b; }", "fn f() { a & &b; }"),
			("fn f() { m!(a && b); }", "fn f() { m!(a & &b); }"),
			// arm bodies that are blocks with statements
			("fn f() { match x { A => { a(); b } } }", "fn f() { match x { A => b } }"),
			("fn f() { match x { A => { m!(); } } }", "fn f() { match x { A => m!() } }"),
			("fn f() { let g = || -> u8 { 1 }; }", "fn f() { let g = || -> u8 { { 1 } }; }"),
			// what prettyplease 0.3.0 prints for some macros
			("thread_local!(static X: u8 = { 1 });", "thread_local! {}"),
			(
				"thread_local!(static X: u8 = 1; static Y: u8 = 2);",
				"thread_local! { static X: u8 = 1; }",
			),
			("#[kani::ensures(|p| p == 0)]\nfn f() {}", "#[kani::ensures(|p| p = = 0)]\nfn f() {}"),
			("#[e(a = match b { _ => 1 })]\nstruct E;", "#[e(a = match b { _ = > 1 })]\nstruct E;"),
			// semicolons that change the value of a block
			("fn f() -> u8 { match x { _ => 1 } }", "fn f() -> u8 { match x { _ => 1 }; }"),
			("fn f() -> Vec<u8> { vec![1] }", "fn f() -> Vec<u8> { vec![1]; }"),
			// docs, other than trailing spaces
			("/// A.\nfn f() {}", "/// B.\nfn f() {}"),
			("#!/bin/a\nfn f() {}", "#!/bin/b\nfn f() {}"),
			("fn f() {}", "fn f() {}\nfn g() {}"),
			("use a::{b, c};", "use a::b;"),
		];

		for (before, after) in cases {
			assert!(check_str(before, after).is_err(), "{before} -> {after}");
		}
	}

	#[test]
	fn groups_without_delimiters_group_like_parentheses() {
		let group = Group::new(Delimiter::None, quote::quote!(a + b));
		let before: syn::File = syn::parse2(quote::quote!(fn f() -> u8 { #group * 2 })).unwrap();

		assert_eq!(check(before.clone(), syn::parse_str("fn f() -> u8 { (a + b) * 2 }").unwrap()), Ok(()));
		assert!(check(before, syn::parse_str("fn f() -> u8 { a + b * 2 }").unwrap()).is_err());
	}
}
