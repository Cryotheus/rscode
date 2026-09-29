//! Macro invocations and `macro_rules!` definitions.
//!
//! A macro body is walked like code when it parses as comma-separated expressions (with special care for the named and
//! inline arguments of the standard library's formatting macros), as statements, or in the shape of `vec![x; n]` or
//! `matches!(x, pattern)`. Other bodies, and the transcribers of `macro_rules!` definitions, are scanned for identifier
//! tokens named like a target, by their role: paths (`a::b`, `::a::b`, `name!`) are resolved where the tokens are
//! (`$crate::path` from the crate root), and a `macro_rules!` macro invoking itself refers to itself; identifiers after
//! `.` may be method calls; other identifiers are uncertain references, except function names after `fn` when no target
//! is a function.

use super::ReferenceKind;
use super::docs::DocStyle;
use super::format_str;
use super::paths::Locals;
use super::paths::PathRes;
use super::paths::ident_name;
use super::walker::FileWalker;
use super::walker::statement_items;
use crate::load::thread_local;
use crate::load::thread_local::Declaration;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::PathRef;
use crate::model::PathSegmentRef;
use crate::resolve::Namespace;
use crate::resolve::PathKind;
use crate::resolve::Res;
use crate::source::TextRange;
use proc_macro2::Ident;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use smol_str::SmolStr;
use syn::Block;
use syn::Expr;
use syn::ExprLit;
use syn::Lit;
use syn::LitStr;
use syn::Macro;
use syn::Pat;
use syn::Stmt;
use syn::Token;
use syn::parse::ParseStream;
use syn::punctuated::Punctuated;
use syn::visit::Visit;

/// Comma-separated expressions.
type Arguments = Punctuated<Expr, Token![,]>;

/// Standard library macros that format their arguments, with the index of their format string.
const FORMAT_MACROS: &[(&str, usize)] = &[
	("assert", 1),
	("assert_eq", 2),
	("assert_ne", 2),
	("debug_assert", 1),
	("debug_assert_eq", 2),
	("debug_assert_ne", 2),
	("eprint", 0),
	("eprintln", 0),
	("format", 0),
	("format_args", 0),
	("panic", 0),
	("print", 0),
	("println", 0),
	("todo", 0),
	("unimplemented", 0),
	("unreachable", 0),
	("write", 1),
	("writeln", 1),
];

impl FileWalker<'_, '_> {
	/// Resolves `$crate::a::b` (starting with the `crate` token at `start`) from the crate root, returning the index
	/// after the path.
	fn dollar_crate_path(&mut self, tokens: &[TokenTree], start: usize) -> usize {
		let TokenTree::Ident(krate) = &tokens[start] else {
			return start + 1;
		};

		let mut segments = vec![PathSegmentRef {
			name: "$crate".into(),
			range: self.parsed.range(krate.span()),
			has_arguments: false,
		}];

		let mut index = start + 1;

		while let Some(TokenTree::Ident(ident)) = tokens.get(index + 2)
			&& is_punct(tokens.get(index), ':')
			&& is_punct(tokens.get(index + 1), ':')
		{
			segments.push(self.segment(ident));
			index += 3;
		}

		let path = PathRef {
			leading_colon: false,
			segments,
		};

		if path.segments.iter().any(|segment| self.targets.named_str(&segment.name).is_some()) {
			let root = ItemId::crate_root(self.krate);
			let res = PathRes::Segments(self.module_path(root, &path, None, PathKind::Code));

			self.report_path(&path, &res, ReferenceKind::MacroToken);
		}

		index
	}

	/// For a formatting macro of the standard library: the index of its format string, and whether it certainly is the
	/// standard library's macro (not one of the loaded crates with the same name).
	fn format_macro(&mut self, mac: &Macro) -> Option<(usize, bool)> {
		let last = mac.path.segments.last()?;
		let &(_, index) = FORMAT_MACROS.iter().find(|(name, _)| last.ident == name)?;
		let path = self.path_ref(&mac.path);
		let res = self.resolve_path(&path, Namespace::Macro, Locals::All);
		let loaded = res == PathRes::Local || res.last().iter().any(|res| matches!(res, Res::Item(_)));

		Some((index, !loaded))
	}

	/// Reports the inline arguments of a format string (`"{name}"`) that name targets, except named arguments.
	fn format_string(&mut self, format: &LitStr, named: &[SmolStr], certain: bool) {
		let range = self.parsed.range(format.span());

		let Some((content, start, raw)) = self.parsed.text.get(range.as_range()).and_then(format_str::literal_content) else {
			return;
		};

		let targets = self.targets;

		for (offset, name) in format_str::inline_arguments(content, raw) {
			let Some(target) = targets.named_str(name).filter(|_| !named.iter().any(|argument| argument == name)) else {
				continue;
			};

			let at = range.start + start + offset;

			let path = PathRef {
				leading_colon: false,
				segments: vec![PathSegmentRef {
					name: name.into(),
					range: TextRange::new(at, at + name.len()),
					has_arguments: false,
				}],
			};

			let res = self.resolve_path(&path, Namespace::Value, Locals::All);

			if certain {
				self.report_path(&path, &res, ReferenceKind::Path);
				self.check_capture(&path, &res, Namespace::Value);
			} else if self.options.macro_tokens
				&& let Some(found) = target.find(res.last())
			{
				self.report(found, ReferenceKind::MacroToken, path.segments[0].range, false);
			}
		}
	}

	/// The arguments of a macro: expressions, except for the named arguments of formatting macros (`name = value`), and
	/// the inline arguments of their format strings.
	fn macro_arguments(&mut self, mac: &Macro, arguments: &Arguments) {
		let format = self.format_macro(mac);
		let mut named = Vec::new();

		for (index, argument) in arguments.iter().enumerate() {
			if let Some((format_index, _)) = format
				&& index > format_index
				&& let Some((name, value)) = named_argument(argument)
			{
				named.push(name);
				self.visit_expr(value);
				continue;
			}

			self.visit_expr(argument);
		}

		if let Some((index, certain)) = format
			&& let Some(Expr::Lit(ExprLit { lit: Lit::Str(format), .. })) = arguments.iter().nth(index)
		{
			self.format_string(format, &named, certain);
		}
	}

	/// A macro's body; `declarations`: those of a `thread_local!`.
	fn macro_body(&mut self, mac: &Macro, declarations: Option<&[Declaration]>) {
		// declarations of statics (definitions, not references)
		if let Some(declarations) = declarations {
			for declaration in declarations {
				self.visit_item_static(&declaration.to_item());
			}
		} else if let Ok(arguments) = mac.parse_body_with(Arguments::parse_terminated) {
			self.macro_arguments(mac, &arguments);
		} else if let Ok(stmts) = mac.parse_body_with(Block::parse_within) {
			self.statements(&stmts);
		} else if !self.special_macro(mac) && self.options.macro_tokens {
			self.scan_tokens(&mac.tokens, None);
		}
	}

	/// A macro invocation: its path (in the macro namespace), and its body.
	pub(super) fn macro_call(&mut self, mac: &Macro) {
		self.code_path(None, &mac.path, Namespace::Macro);

		if self.tokens_mention_target(&mac.tokens) {
			let declarations = thread_local::declarations(mac);

			// the statics a `thread_local!` declares are documented like the items around it (doc comments inside of
			// bodies are not searched)
			for declaration in declarations.iter().flatten() {
				self.doc_comments(&declaration.attrs, DocStyle::Outer);
			}

			self.body_depth += 1;
			self.macro_body(mac, declarations.as_deref());
			self.body_depth -= 1;
		}
	}

	/// A `macro_rules!` definition: its transcribers are scanned (its matchers are patterns of tokens, not code).
	pub(super) fn macro_rules(&mut self, name: &Ident, mac: &Macro) {
		if !self.tokens_mention_target(&mac.tokens) {
			return;
		}

		let own = self.loaded_item(name, ItemKind::MacroRules);
		let rules: Vec<TokenTree> = mac.tokens.clone().into_iter().collect();

		self.body_depth += 1;

		for (index, token) in rules.iter().enumerate().skip(2) {
			if let TokenTree::Group(transcriber) = token
				&& is_punct(rules.get(index - 1), '>')
				&& is_punct(rules.get(index - 2), '=')
			{
				self.scan_tokens(&transcriber.stream(), own);
			}
		}

		self.body_depth -= 1;
	}

	/// An identifier token that is not (the start of) a path, at `index` of `tokens`.
	fn macro_token(&mut self, ident: &Ident, tokens: &[TokenTree], index: usize) {
		let previous = index.checked_sub(1).and_then(|index| tokens.get(index));
		let targets = self.targets;

		let Some(target) = targets.named(ident) else {
			return;
		};

		// a method call or a field
		if is_punct(previous, '.') {
			return self.method_call(ident);
		}

		// the name of a function the macro defines
		if matches!(previous, Some(TokenTree::Ident(keyword)) if keyword == "fn")
			&& !target
				.items
				.iter()
				.any(|&item| matches!(self.ws.item(item).kind, ItemKind::Fn | ItemKind::AssocFn | ItemKind::ForeignFn))
		{
			return;
		}

		self.uncertain_token(ident);
	}

	/// Reports the identifiers named like targets in tokens that are not parsed as code, by their role:
	/// - in a path (`a::b`, `::a::b`, `name!`, `$crate::a`) that resolves where the tokens are (or from the crate root
	///   for `$crate`), by what it resolves to (certain);
	/// - after `.`, as method calls (uncertain, see [`ReferenceOptions::method_calls`](super::ReferenceOptions));
	/// - otherwise, as uncertain tokens, unless their role rules the targets out (a name after `fn` only defines
	///   functions).
	///
	/// `own`: the `macro_rules!` macro whose transcriber the tokens are.
	pub(super) fn scan_tokens(&mut self, tokens: &TokenStream, own: Option<ItemId>) {
		let tokens: Vec<TokenTree> = tokens.clone().into_iter().collect();
		let mut index = 0;

		while index < tokens.len() {
			match &tokens[index] {
				TokenTree::Group(group) => self.scan_tokens(&group.stream(), own),

				TokenTree::Punct(punct) if punct.as_char() == '$' => match tokens.get(index + 1) {
					Some(TokenTree::Ident(ident)) if ident == "crate" => {
						index = self.dollar_crate_path(&tokens, index + 1);
						continue;
					}

					// a metavariable
					Some(TokenTree::Ident(_)) => index += 1,
					_ => {}
				},

				TokenTree::Ident(_) if starts_path(&tokens, index) => {
					index = self.token_path(&tokens, index, own);
					continue;
				}

				TokenTree::Ident(ident) => self.macro_token(ident, &tokens, index),
				_ => {}
			}

			index += 1;
		}
	}

	/// `vec![value; count]` and `matches!(value, pattern [if guard])` (whose patterns do not parse as expressions).
	fn special_macro(&mut self, mac: &Macro) -> bool {
		let Some(name) = mac.path.segments.last().map(|segment| &segment.ident) else {
			return false;
		};

		if name == "vec"
			&& let Ok((value, count)) = mac.parse_body_with(parse_repeat)
		{
			self.visit_expr(&value);
			self.visit_expr(&count);
			return true;
		}

		if (name == "matches" || name == "assert_matches" || name == "debug_assert_matches")
			&& let Ok((value, pattern, guard, rest)) = mac.parse_body_with(parse_matches)
		{
			self.visit_expr(&value);
			self.scopes.push(Default::default());
			self.pattern(&pattern);

			if let Some(guard) = &guard {
				self.visit_expr(guard);
			}

			self.scopes.pop();

			for argument in &rest {
				self.visit_expr(argument);
			}

			return true;
		}

		false
	}

	/// Statements parsed from a macro body, in a scope of their own.
	fn statements(&mut self, stmts: &[Stmt]) {
		let items = statement_items(stmts);
		let scope = self.local_items(items.iter().map(AsRef::as_ref));

		self.scopes.push(scope);

		for stmt in stmts {
			self.visit_stmt(stmt);
		}

		self.scopes.pop();
	}

	/// A path of tokens (`a::b`, `::a::b`, or `name!`) starting with the identifier at `start`, resolved where the
	/// tokens are. Returns the index after the path.
	fn token_path(&mut self, tokens: &[TokenTree], start: usize, own: Option<ItemId>) -> usize {
		let TokenTree::Ident(first) = &tokens[start] else {
			return start + 1;
		};

		let mut idents = vec![first];
		let mut index = start + 1;

		while let Some(TokenTree::Ident(ident)) = tokens.get(index + 2)
			&& is_punct(tokens.get(index), ':')
			&& is_punct(tokens.get(index + 1), ':')
		{
			idents.push(ident);
			index += 3;
		}

		let targets = self.targets;

		if !idents.iter().any(|ident| targets.named(ident).is_some()) {
			return index;
		}

		let is_macro = is_punct(tokens.get(index), '!');
		let path = PathRef {
			leading_colon: leading_colon_before(tokens, start),
			segments: idents.iter().map(|ident| self.segment(ident)).collect(),
		};

		// a `macro_rules!` macro invoking itself
		if let (Some(own), [ident], true) = (own, idents.as_slice(), is_macro)
			&& targets.named(ident).is_some_and(|target| target.contains(own))
		{
			self.report(own, ReferenceKind::MacroToken, path.segments[0].range, true);
			return index;
		}

		match self.token_path_res(&path, is_macro.then_some(Namespace::Macro)) {
			// resolved from where the tokens are: what the path names in the code a macro expands to, most likely
			PathRes::Segments(segments) if segments.first().is_some_and(|first| !first.is_empty()) => {
				self.report_path(&path, &PathRes::Segments(segments), ReferenceKind::MacroToken);
			}

			// local to a body: not a target
			PathRes::Local => {}

			// starting with a name that only the expansion site may know
			_ => {
				for ident in idents {
					self.uncertain_token(ident);
				}
			}
		}

		index
	}

	/// What a path of tokens resolves to, with its last segment in `namespace` (or in any namespace).
	fn token_path_res(&mut self, path: &PathRef, namespace: Option<Namespace>) -> PathRes {
		if let Some(namespace) = namespace {
			return self.resolve_path(path, namespace, Locals::Items);
		}

		let mut res = self.resolve_path(path, Namespace::Type, Locals::Items);

		if let (PathRes::Segments(segments), PathRes::Segments(values)) = (&mut res, self.resolve_path(path, Namespace::Value, Locals::Items))
			&& let (Some(last), Some(value_last)) = (segments.last_mut(), values.last())
		{
			last.extend(value_last.iter().cloned());
			last.sort();
			last.dedup();
		}

		res
	}

	/// Whether tokens contain an identifier named like a target, or a string literal mentioning one (a format string).
	pub(super) fn tokens_mention_target(&self, tokens: &TokenStream) -> bool {
		tokens.clone().into_iter().any(|token| match token {
			TokenTree::Ident(ident) => self.targets.named(&ident).is_some(),
			TokenTree::Group(group) => self.tokens_mention_target(&group.stream()),
			TokenTree::Literal(literal) => self.targets.mentioned_in(&literal.to_string()),
			TokenTree::Punct(_) => false,
		})
	}

	/// Reports an identifier named like a target as an uncertain token.
	fn uncertain_token(&mut self, ident: &Ident) {
		let targets = self.targets;

		if self.options.macro_tokens
			&& let Some(target) = targets.named(ident)
		{
			self.report(target.first(), ReferenceKind::MacroToken, self.parsed.range(ident.span()), false);
		}
	}

	/// Syntax syn does not model (verbatim items, types, expressions, and patterns).
	pub(super) fn verbatim(&mut self, tokens: &TokenStream) {
		if self.options.macro_tokens && self.tokens_mention_target(tokens) {
			self.scan_tokens(tokens, None);
		}
	}
}

fn is_punct(token: Option<&TokenTree>, char: char) -> bool {
	matches!(token, Some(TokenTree::Punct(punct)) if punct.as_char() == char)
}

/// Whether the identifier at `index` follows a leading `::` (one that follows neither a path segment nor generic
/// arguments).
fn leading_colon_before(tokens: &[TokenTree], index: usize) -> bool {
	let before = |back: usize| index.checked_sub(back).and_then(|index| tokens.get(index));

	is_punct(before(1), ':') && is_punct(before(2), ':') && !matches!(before(3), Some(TokenTree::Ident(_))) && !is_punct(before(3), '>')
}

/// `name = value` (a named argument of a formatting macro).
fn named_argument(argument: &Expr) -> Option<(SmolStr, &Expr)> {
	let Expr::Assign(assign) = argument else {
		return None;
	};

	let Expr::Path(left) = &*assign.left else {
		return None;
	};

	let ident = left.path.get_ident().filter(|_| left.qself.is_none())?;

	Some((ident_name(ident), &assign.right))
}

/// `matches!` arguments: a value, a pattern with an optional guard, and optional further arguments.
fn parse_matches(input: ParseStream) -> syn::Result<(Expr, Pat, Option<Expr>, Arguments)> {
	let value: Expr = input.parse()?;

	input.parse::<Token![,]>()?;

	let pattern = Pat::parse_multi_with_leading_vert(input)?;
	let guard = match input.parse::<Option<Token![if]>>()? {
		Some(_) => Some(input.parse()?),
		None => None,
	};

	let rest = match input.parse::<Option<Token![,]>>()? {
		Some(_) => Arguments::parse_terminated(input)?,
		None => Arguments::new(),
	};

	Ok((value, pattern, guard, rest))
}

/// `vec!` arguments of the form `value; count`.
fn parse_repeat(input: ParseStream) -> syn::Result<(Expr, Expr)> {
	let value = input.parse()?;

	input.parse::<Token![;]>()?;
	Ok((value, input.parse()?))
}

/// Whether the identifier at `index` starts a path of tokens: `name!`, or `name::` followed by an identifier, and not
/// after `.`, `::` (unless that is a leading `::`), or a `$` (of a metavariable).
fn starts_path(tokens: &[TokenTree], index: usize) -> bool {
	let before = |back: usize| index.checked_sub(back).and_then(|index| tokens.get(index));
	let separated = is_punct(before(1), ':') && is_punct(before(2), ':');

	if is_punct(before(1), '.') || is_punct(before(1), '$') || (separated && !leading_colon_before(tokens, index)) {
		return false;
	}

	let after = |ahead: usize| tokens.get(index + ahead);

	is_punct(after(1), '!') || (is_punct(after(1), ':') && is_punct(after(2), ':') && matches!(after(3), Some(TokenTree::Ident(_))))
}
