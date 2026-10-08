//! Parsing source text given by users into the items of a container, and telling what those items are.
//!
//! Items are classified the way the loader classifies them ([`ItemKind`]s depend on the container: a `fn` is an
//! [`ItemKind::Fn`] in a module and an [`ItemKind::AssocFn`] in an `impl` block), including syntax that `syn` only
//! tokenizes (`Verbatim` items such as `fn f();` or `const trait T {}`).

use crate::Error;
use crate::load::macro_entries::Entry;
use crate::load::thread_local;
use crate::load::thread_local::Declaration;
use crate::model::ItemKind;
use crate::path::is_keyword;
use crate::resolve::Namespace;
use proc_macro2::Delimiter;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use quote::ToTokens;
use smol_str::SmolStr;
use syn::Attribute;
use syn::Field;
use syn::Fields;
use syn::ForeignItem;
use syn::Ident;
use syn::ImplItem;
use syn::Item;
use syn::Token;
use syn::TraitItem;
use syn::UseTree;
use syn::Variant;
use syn::ext::IdentExt;
use syn::parse::Parse;
use syn::parse::ParseStream;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;

/// Parsed items, the end of their last token, and the range of a trailing comma to remove.
type Parsed = (Vec<NewItem>, usize, Option<std::ops::Range<usize>>);

const MACRO: &[Namespace] = &[Namespace::Macro];
const TYPE: &[Namespace] = &[Namespace::Type];
const TYPE_AND_VALUE: &[Namespace] = &[Namespace::Type, Namespace::Value];
const VALUE: &[Namespace] = &[Namespace::Value];

/// What holds items, which decides which items are valid in it.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub(super) enum Container {
	Module,
	Impl,
	Trait,
	Extern,
	Enum,

	/// A struct, union, or variant, whose fields are named (`name: Type`) or, in tuple structs and variants, not
	/// (`Type`).
	Fields,

	/// A `thread_local!` invocation, whose declarations are statics.
	ThreadLocal,

	/// Another macro invocation whose body is entries (`static NAME = value;`), which are statics.
	Entries,
}

impl Container {
	/// The container that items of `kind` hold their children in, if any.
	pub(super) fn of(kind: ItemKind) -> Option<Self> {
		match kind {
			ItemKind::Module => Some(Self::Module),
			ItemKind::Impl => Some(Self::Impl),
			ItemKind::Trait => Some(Self::Trait),
			ItemKind::ExternBlock => Some(Self::Extern),
			ItemKind::Enum => Some(Self::Enum),
			ItemKind::Struct | ItemKind::Union | ItemKind::Variant => Some(Self::Fields),

			// the macro calls with children (whose entries are `Entries`, see `Container::of_item`)
			ItemKind::MacroCall => Some(Self::ThreadLocal),

			_ => None,
		}
	}

	/// The container that `item` (of `ws`) holds its children in, if any: [`Container::of`] its kind, except for the
	/// entries of macro invocations other than `thread_local!`.
	pub(super) fn of_item(ws: &crate::model::Workspace, item: crate::model::ItemId) -> Option<Self> {
		let entries = ws.children(item).next().is_some_and(|child| ws.entry_macro(child).is_some());

		match Self::of(ws.item(item).kind)? {
			Self::ThreadLocal if entries => Some(Self::Entries),
			container => Some(container),
		}
	}

	fn const_kind(self) -> Option<ItemKind> {
		match self {
			Self::Module => Some(ItemKind::Const),
			Self::Impl | Self::Trait => Some(ItemKind::AssocConst),
			Self::Extern | Self::Enum | Self::Fields | Self::ThreadLocal | Self::Entries => None,
		}
	}

	fn fn_kind(self) -> ItemKind {
		match self {
			Self::Module | Self::Enum | Self::Fields | Self::ThreadLocal | Self::Entries => ItemKind::Fn,
			Self::Impl | Self::Trait => ItemKind::AssocFn,
			Self::Extern => ItemKind::ForeignFn,
		}
	}

	/// What the items of the container are called, for messages.
	pub(super) fn items(self) -> &'static str {
		match self {
			Self::Module => "module items",
			Self::Impl => "associated items of an `impl` block",
			Self::Trait => "trait items",
			Self::Extern => "items of an `extern` block",
			Self::Enum => "enum variants",
			Self::Fields => "fields (`name: Type`, or `Type` in a tuple struct or variant)",
			Self::ThreadLocal => "declarations of a `thread_local!` (`static NAME: Type = initializer;`)",
			Self::Entries => "entries of a macro invocation (`static NAME = value;`)",
		}
	}

	/// The kind of a macro invocation (and of syntax that is not understood).
	fn macro_kind(self) -> ItemKind {
		match self {
			Self::Module | Self::Enum | Self::Fields | Self::ThreadLocal | Self::Entries => ItemKind::MacroCall,
			Self::Impl | Self::Trait => ItemKind::AssocMacro,
			Self::Extern => ItemKind::ForeignMacro,
		}
	}

	fn static_kind(self) -> Option<ItemKind> {
		match self {
			Self::Module | Self::ThreadLocal | Self::Entries => Some(ItemKind::Static),
			Self::Extern => Some(ItemKind::ForeignStatic),
			Self::Impl | Self::Trait | Self::Enum | Self::Fields => None,
		}
	}

	fn type_kind(self) -> ItemKind {
		match self {
			Self::Module | Self::Enum | Self::Fields | Self::ThreadLocal | Self::Entries => ItemKind::TypeAlias,
			Self::Impl | Self::Trait => ItemKind::AssocType,
			Self::Extern => ItemKind::ForeignType,
		}
	}
}

/// The path of a `use` leaf.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct ImportPath {
	pub(super) leading_colon: bool,

	/// Unraw'd segments, including `crate`, `self`, and `super`.
	pub(super) segments: Vec<SmolStr>,
}

/// A name that a new item binds.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct NewBinding {
	pub(super) name: SmolStr,

	/// The namespaces the name is bound in. For named imports: every namespace their path may name something in.
	pub(super) namespaces: &'static [Namespace],

	/// For named imports: the imported path, whose resolution tells the namespaces actually bound.
	pub(super) import: Option<ImportPath>,
}

/// An item of parsed source.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct NewItem {
	pub(super) kind: ItemKind,

	/// The unraw'd name, `None` for unnamed items (and `_`).
	pub(super) name: Option<SmolStr>,

	/// The names the item binds in its container: its own name, the leaves of a `use`, or the items of an `extern`
	/// block.
	pub(super) bindings: Vec<NewBinding>,
}

impl NewItem {
	fn named(kind: ItemKind, ident: &Ident, namespaces: &'static [Namespace]) -> Self {
		Self::with_name(kind, ident_name(ident), namespaces)
	}

	fn unnamed(kind: ItemKind) -> Self {
		Self {
			kind,
			name: None,
			bindings: Vec::new(),
		}
	}

	fn with_name(kind: ItemKind, name: Option<SmolStr>, namespaces: &'static [Namespace]) -> Self {
		let bindings = name
			.iter()
			.map(|name| NewBinding {
				name: name.clone(),
				namespaces,
				import: None,
			})
			.collect();

		Self { kind, name, bindings }
	}
}

/// Source text parsed as items of a container.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct ParsedSource {
	pub(super) items: Vec<NewItem>,

	/// The text of the items: the source without a byte order mark and, for enum variants and fields, without a
	/// trailing comma (the variant or field being replaced keeps the comma after it).
	pub(super) text: String,

	/// Where the code ends in [`ParsedSource::text`]: a trailing comment follows, if any.
	pub(super) code_end: usize,

	/// Whether a comment follows the last token (it would comment out code following the text on its line).
	pub(super) trailing_comment: bool,

	/// Whether the last token is a `;`.
	pub(super) ends_with_semicolon: bool,

	/// Whether an item has `cfg` (or `cfg_attr`) attributes.
	pub(super) has_cfg: bool,
}

/// The `cfg` and `cfg_attr` attributes among the outer attributes (and doc comments) of an item, as written.
pub(super) fn cfg_attributes(attributes: &str) -> Vec<String> {
	crate::source::isolated(|| {
		let Ok(parsed) = Attribute::parse_outer.parse_str(attributes) else {
			return Vec::new();
		};

		(parsed.iter())
			.filter(|attribute| is_cfg_attribute(attribute.meta.path().to_token_stream()))
			.filter_map(|attribute| attributes.get(attribute.span().byte_range()).map(str::to_owned))
			.collect()
	})
}

/// Tuple and unit structs (and variants) are also constructors.
fn data_namespaces(fields: &Fields) -> &'static [Namespace] {
	match fields {
		Fields::Named(_) => TYPE,
		Fields::Unnamed(_) | Fields::Unit => TYPE_AND_VALUE,
	}
}

/// The 1-based line and column of the end of a text.
fn end_location(text: &str) -> (usize, usize) {
	let line_start = text.rfind('\n').map_or(0, |index| index + 1);

	(text.matches('\n').count() + 1, text[line_start..].chars().count() + 1)
}

/// The end of a node's last token (`fallback` for nodes without tokens).
fn end_of(node: &impl Spanned, fallback: usize) -> usize {
	let span = node.span();

	match span.source_text() {
		Some(_) => span.byte_range().end,
		None => fallback,
	}
}

fn foreign_item(item: &ForeignItem) -> NewItem {
	match item {
		ForeignItem::Fn(item) => NewItem::named(ItemKind::ForeignFn, &item.sig.ident, VALUE),
		ForeignItem::Static(item) => NewItem::named(ItemKind::ForeignStatic, &item.ident, VALUE),
		ForeignItem::Type(item) => NewItem::named(ItemKind::ForeignType, &item.ident, TYPE),
		ForeignItem::Macro(_) => NewItem::unnamed(ItemKind::ForeignMacro),
		ForeignItem::Verbatim(tokens) => verbatim(tokens, Container::Extern),
		item => verbatim(&item.to_token_stream(), Container::Extern),
	}
}

/// `[default] [const] [async] [safe | unsafe] [extern ["abi"]] fn name ...`
fn function_name(rest: &[TokenTree]) -> Option<Option<SmolStr>> {
	let fn_index = rest.iter().position(|token| is(token, "fn"))?;
	let mut modifiers = rest[..fn_index].iter().peekable();

	while let Some(token) = modifiers.next() {
		match token {
			TokenTree::Ident(ident) if ["const", "async", "unsafe", "default", "safe"].iter().any(|word| ident == word) => {}

			TokenTree::Ident(ident) if ident == "extern" => {
				modifiers.next_if(|token| matches!(token, TokenTree::Literal(_)));
			}

			_ => return None,
		}
	}

	Some(rest.get(fn_index + 1).and_then(token_name))
}

/// Whether an outer attribute of an item of parsed source (at the top level of its tokens) is a `cfg` or `cfg_attr`.
fn has_cfg_attributes(text: &str) -> bool {
	let Ok(tokens) = text.parse::<TokenStream>() else {
		return false;
	};

	let tokens: Vec<TokenTree> = tokens.into_iter().collect();

	tokens.windows(2).any(|pair| match pair {
		[TokenTree::Punct(pound), TokenTree::Group(group)] if pound.as_char() == '#' => {
			group.delimiter() == Delimiter::Bracket && is_cfg_attribute(group.stream())
		}

		_ => false,
	})
}

/// The unraw'd name of an identifier, `None` for `_`.
fn ident_name(ident: &Ident) -> Option<SmolStr> {
	let name = ident.unraw().to_string();

	(name != "_").then(|| name.into())
}

fn impl_item(item: &ImplItem) -> NewItem {
	match item {
		ImplItem::Const(item) => NewItem::named(ItemKind::AssocConst, &item.ident, VALUE),
		ImplItem::Fn(item) => NewItem::named(ItemKind::AssocFn, &item.sig.ident, VALUE),
		ImplItem::Type(item) => NewItem::named(ItemKind::AssocType, &item.ident, TYPE),
		ImplItem::Macro(_) => NewItem::unnamed(ItemKind::AssocMacro),
		ImplItem::Verbatim(tokens) => verbatim(tokens, Container::Impl),
		item => verbatim(&item.to_token_stream(), Container::Impl),
	}
}

/// The error for source that does not parse, located in the source (must be called on the parsing thread).
fn invalid_source(error: &syn::Error, text: &str, container: Container) -> Error {
	let span = error.span();

	// errors at the end of the input have no location: they are at the end of the text
	let (line, column) = match span.source_text() {
		Some(_) => (span.start().line, span.start().column + 1),
		None => end_location(text),
	};

	Error::InvalidSource(format!("the source does not parse as {} at {line}:{column}: {error}", container.items()))
}

/// Whether a token is the identifier (or keyword) `word`.
fn is(token: &TokenTree, word: &str) -> bool {
	matches!(token, TokenTree::Ident(ident) if ident == word)
}

/// Whether the contents of an attribute's brackets are a `cfg` or `cfg_attr` attribute.
fn is_cfg_attribute(tokens: TokenStream) -> bool {
	matches!(tokens.into_iter().next(), Some(TokenTree::Ident(ident)) if ident == "cfg" || ident == "cfg_attr")
}

fn module_item(item: &Item) -> NewItem {
	match item {
		Item::Const(item) => NewItem::named(ItemKind::Const, &item.ident, VALUE),
		Item::Enum(item) => NewItem::named(ItemKind::Enum, &item.ident, TYPE),

		Item::ExternCrate(item) => {
			let bound = item.rename.as_ref().map_or(&item.ident, |(_, alias)| alias);

			NewItem::named(ItemKind::ExternCrate, bound, TYPE)
		}

		Item::Fn(item) => NewItem::named(ItemKind::Fn, &item.sig.ident, VALUE),

		Item::ForeignMod(block) => NewItem {
			kind: ItemKind::ExternBlock,
			name: None,
			bindings: block.items.iter().flat_map(|item| foreign_item(item).bindings).collect(),
		},

		Item::Impl(_) => NewItem::unnamed(ItemKind::Impl),

		Item::Macro(item) => match &item.ident {
			Some(ident) if item.mac.path.is_ident("macro_rules") => NewItem::named(ItemKind::MacroRules, ident, MACRO),

			// the statics of a `thread_local!` are bound in the module, like the items of an `extern` block
			_ => NewItem {
				kind: ItemKind::MacroCall,
				name: None,
				bindings: (thread_local::declarations(&item.mac).into_iter().flatten())
					.flat_map(|declaration| NewItem::named(ItemKind::Static, &declaration.ident, VALUE).bindings)
					.collect(),
			},
		},

		Item::Mod(item) => NewItem::named(ItemKind::Module, &item.ident, TYPE),
		Item::Static(item) => NewItem::named(ItemKind::Static, &item.ident, VALUE),
		Item::Struct(item) => NewItem::named(ItemKind::Struct, &item.ident, data_namespaces(&item.fields)),
		Item::Trait(item) => NewItem::named(ItemKind::Trait, &item.ident, TYPE),
		Item::TraitAlias(item) => NewItem::named(ItemKind::TraitAlias, &item.ident, TYPE),
		Item::Type(item) => NewItem::named(ItemKind::TypeAlias, &item.ident, TYPE),
		Item::Union(item) => NewItem::named(ItemKind::Union, &item.ident, TYPE),

		Item::Use(item) => {
			let mut bindings = Vec::new();
			let path = ImportPath {
				leading_colon: item.leading_colon.is_some(),
				segments: Vec::new(),
			};

			use_bindings(&item.tree, path, &mut bindings);

			NewItem {
				kind: ItemKind::Use,
				name: None,
				bindings,
			}
		}

		Item::Verbatim(tokens) => verbatim(tokens, Container::Module),
		item => verbatim(&item.to_token_stream(), Container::Module),
	}
}

/// Traits and `impl` blocks with modifiers syn does not support (`const trait`, `impl(crate) trait`, `const impl`),
/// and `use` items with `::` at the start of group elements.
fn module_verbatim(rest: &[TokenTree]) -> Option<NewItem> {
	let rest = match rest {
		[restriction, TokenTree::Group(group), rest @ ..] if is(restriction, "impl") && group.delimiter() == Delimiter::Parenthesis => rest,
		rest => rest,
	};

	match strip(rest, &["const", "unsafe", "auto"]) {
		[keyword, name, rest @ ..] if is(keyword, "trait") => {
			let name = token_name(name)?;
			let body = rest
				.iter()
				.any(|token| matches!(token, TokenTree::Group(group) if group.delimiter() == Delimiter::Brace));
			let alias = !body
				&& rest
					.iter()
					.any(|token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == '='));
			let kind = if alias { ItemKind::TraitAlias } else { ItemKind::Trait };

			Some(NewItem::with_name(kind, Some(name), TYPE))
		}

		[keyword, ..] if is(keyword, "impl") => Some(NewItem::unnamed(ItemKind::Impl)),
		[keyword, ..] if is(keyword, "use") => Some(NewItem::unnamed(ItemKind::Use)),
		_ => None,
	}
}

/// Named fields (`a: u8, pub b: u16`), or else unnamed ones (`u8, pub u16`), whose trailing comma is removed (the field
/// being replaced keeps the comma after it). Unnamed fields have no name (their index depends on their place).
fn parse_fields(text: &str) -> syn::Result<Parsed> {
	let parser = |named: bool| {
		move |input: ParseStream<'_>| {
			reject_inner_attributes(input)?;

			let fields = match named {
				true => Punctuated::<Field, Token![,]>::parse_terminated_with(input, Field::parse_named)?,
				false => Punctuated::<Field, Token![,]>::parse_terminated_with(input, Field::parse_unnamed)?,
			};
			let mut end = fields.last().map_or(0, |field| end_of(field, 0));
			let trailing_comma = (fields.pairs().next_back()).and_then(|pair| pair.punct().map(|comma| comma.span.byte_range()));

			if let Some(comma) = &trailing_comma {
				// the comma is removed, and what follows it moves back
				end = comma.start;
			}

			let items = fields
				.iter()
				.map(|field| NewItem::with_name(ItemKind::Field, field.ident.as_ref().and_then(ident_name), &[]))
				.collect();

			Ok((items, end, trailing_comma))
		}
	};

	parser(true).parse_str(text).or_else(|named| parser(false).parse_str(text).map_err(|_| named))
}

fn parse_list<T: Parse + ToTokens>(text: &str, classify: impl Fn(&T) -> NewItem) -> syn::Result<Parsed> {
	let parser = |input: ParseStream<'_>| {
		reject_inner_attributes(input)?;

		let mut items = Vec::new();
		let mut end = 0;

		while !input.is_empty() {
			let item: T = input.parse()?;

			end = end_of(&item, end);
			items.push(classify(&item));
		}

		Ok((items, end, None))
	};

	parser.parse_str(text)
}

/// Parses `source` as zero or more items of `container`.
///
/// Fails with [`Error::InvalidSource`] (with a location relative to `source`) when it does not parse, or when it has
/// inner attributes (`#![...]`, `//!`), which would apply to the container.
pub(super) fn parse_source(source: &str, container: Container) -> Result<ParsedSource, Error> {
	crate::source::isolated(|| parse_source_here(source, container))
}

/// [`parse_source`] on the calling thread.
fn parse_source_here(source: &str, container: Container) -> Result<ParsedSource, Error> {
	let text = source.strip_prefix('\u{feff}').unwrap_or(source);

	let parsed = match container {
		Container::Module => parse_list(text, |item: &Item| module_item(item)),
		Container::Impl => parse_list(text, |item: &ImplItem| impl_item(item)),
		Container::Trait => parse_list(text, |item: &TraitItem| trait_item(item)),
		Container::Extern => parse_list(text, |item: &ForeignItem| foreign_item(item)),
		Container::Enum => parse_variants(text),
		Container::Fields => parse_fields(text),

		Container::ThreadLocal => parse_list(text, |declaration: &Declaration| {
			NewItem::named(ItemKind::Static, &declaration.ident, VALUE)
		}),

		// (entries are bound nowhere)
		Container::Entries => parse_list(text, |entry: &Entry| {
			NewItem::with_name(ItemKind::Static, ident_name(&entry.ident), &[])
		}),
	};

	let (items, end, trailing_comma) = parsed.map_err(|error| invalid_source(&error, text, container))?;
	let mut text = text.to_owned();

	if let Some(comma) = trailing_comma {
		text.replace_range(comma, "");
	}

	let code_end = end.min(text.len());
	let trailing_comment = !text.get(code_end..).unwrap_or_default().trim().is_empty();
	let ends_with_semicolon = text.get(..code_end).is_some_and(|code| code.ends_with(';'));
	let has_cfg = has_cfg_attributes(&text);

	Ok(ParsedSource {
		items,
		text,
		code_end,
		trailing_comment,
		ends_with_semicolon,
		has_cfg,
	})
}

fn parse_variants(text: &str) -> syn::Result<Parsed> {
	let parser = |input: ParseStream<'_>| {
		reject_inner_attributes(input)?;

		let variants = Punctuated::<Variant, Token![,]>::parse_terminated(input)?;
		let mut end = variants.last().map_or(0, |variant| end_of(variant, 0));
		let trailing_comma = variants
			.pairs()
			.next_back()
			.and_then(|pair| pair.punct().map(|comma| comma.span.byte_range()));

		if let Some(comma) = &trailing_comma {
			// the comma is removed, and what follows it moves back
			end = comma.start;
		}

		Ok((variants.iter().map(variant).collect(), end, trailing_comma))
	};

	parser.parse_str(text)
}

fn reject_inner_attributes(input: ParseStream<'_>) -> syn::Result<()> {
	let inner = input.call(Attribute::parse_inner)?;

	match inner.first() {
		Some(attribute) => Err(syn::Error::new_spanned(
			attribute,
			"inner attributes (`#![...]`) and inner doc comments (`//!`) are not allowed here, as they would apply to \
			 the container",
		)),

		None => Ok(()),
	}
}

/// `[safe | unsafe] static [mut] name ...`
fn static_item(rest: &[TokenTree], container: Container) -> Option<NewItem> {
	let [keyword, rest @ ..] = strip(rest, &["safe", "unsafe"]) else {
		return None;
	};

	if !is(keyword, "static") {
		return None;
	}

	let name = strip(rest, &["mut"]).first().and_then(token_name)?;

	container.static_kind().map(|kind| NewItem::with_name(kind, Some(name), VALUE))
}

/// Skips leading tokens that are any of `words`.
fn strip<'a>(mut tokens: &'a [TokenTree], words: &[&str]) -> &'a [TokenTree] {
	while let [first, rest @ ..] = tokens
		&& words.iter().any(|word| is(first, word))
	{
		tokens = rest;
	}

	tokens
}

fn strip_attributes_and_visibility(tokens: &TokenStream) -> Vec<TokenTree> {
	let parser = |input: ParseStream<'_>| {
		input.call(Attribute::parse_outer)?;
		input.parse::<syn::Visibility>()?;
		input.parse::<TokenStream>()
	};

	parser.parse2(tokens.clone()).unwrap_or_else(|_| tokens.clone()).into_iter().collect()
}

/// The unraw'd name of an identifier token that can name an item.
fn token_name(token: &TokenTree) -> Option<SmolStr> {
	match token {
		TokenTree::Ident(ident) if !is_keyword(&ident.to_string()) => ident_name(ident),
		_ => None,
	}
}

fn trait_item(item: &TraitItem) -> NewItem {
	match item {
		TraitItem::Const(item) => NewItem::named(ItemKind::AssocConst, &item.ident, VALUE),
		TraitItem::Fn(item) => NewItem::named(ItemKind::AssocFn, &item.sig.ident, VALUE),
		TraitItem::Type(item) => NewItem::named(ItemKind::AssocType, &item.ident, TYPE),
		TraitItem::Macro(_) => NewItem::unnamed(ItemKind::AssocMacro),
		TraitItem::Verbatim(tokens) => verbatim(tokens, Container::Trait),
		item => verbatim(&item.to_token_stream(), Container::Trait),
	}
}

/// Adds the names bound by the leaves of a `use` tree, below the path `prefix`.
fn use_bindings(tree: &UseTree, mut prefix: ImportPath, bindings: &mut Vec<NewBinding>) {
	let mut leaf = |ident: &Ident, alias: Option<&Ident>| {
		let is_self = ident == "self";
		let mut path = prefix.clone();

		if !is_self {
			path.segments.push(ident.unraw().to_string().into());
		}

		let bound = match alias {
			Some(alias) => ident_name(alias),
			None if is_self => path.segments.last().cloned(),
			None => ident_name(ident),
		};

		let Some(name) = bound.filter(|name| !matches!(name.as_str(), "crate" | "self" | "super")) else {
			return;
		};

		// `a::{self}` only imports the module (or type) `a`
		let (namespaces, import) = if is_self { (TYPE, None) } else { (&Namespace::ALL[..], Some(path)) };

		bindings.push(NewBinding { name, namespaces, import });
	};

	match tree {
		UseTree::Path(path) => {
			prefix.segments.push(path.ident.unraw().to_string().into());
			use_bindings(&path.tree, prefix, bindings);
		}

		UseTree::Name(name) => leaf(&name.ident, None),
		UseTree::Rename(rename) => leaf(&rename.ident, Some(&rename.rename)),
		UseTree::Glob(_) => {}

		UseTree::Group(group) => {
			for tree in &group.items {
				use_bindings(tree, prefix.clone(), bindings);
			}
		}
	}
}

fn variant(variant: &Variant) -> NewItem {
	NewItem::named(ItemKind::Variant, &variant.ident, data_namespaces(&variant.fields))
}

/// Classifies syntax that syn only tokenizes by its leading tokens (after attributes and visibility), like the loader.
fn verbatim(tokens: &TokenStream, container: Container) -> NewItem {
	let rest = strip_attributes_and_visibility(tokens);
	let unknown = NewItem::unnamed(container.macro_kind());

	if let Some(name) = function_name(&rest) {
		return NewItem::with_name(container.fn_kind(), name, VALUE);
	}

	if container == Container::Module
		&& let Some(item) = module_verbatim(&rest)
	{
		return item;
	}

	let stripped = strip(&rest, &["default"]);

	match (stripped.first(), stripped.get(1).and_then(token_name)) {
		(Some(keyword), Some(name)) if is(keyword, "const") => match container.const_kind() {
			Some(kind) => NewItem::with_name(kind, Some(name), VALUE),
			None => unknown,
		},

		(Some(keyword), Some(name)) if is(keyword, "type") => NewItem::with_name(container.type_kind(), Some(name), TYPE),

		// a declarative macro 2.0 (`macro m() {}`)
		(Some(keyword), Some(name)) if is(keyword, "macro") && container == Container::Module => {
			NewItem::with_name(ItemKind::MacroRules, Some(name), MACRO)
		}

		_ => static_item(stripped, container).unwrap_or(unknown),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn classifies_associated_foreign_and_variant_items() {
		use ItemKind::*;

		assert_eq!(
			items("fn a(&self) {}\nconst B: u8 = 1;\ntype C = u8;\nmac!();", Container::Impl),
			expected(&[(AssocFn, Some("a")), (AssocConst, Some("B")), (AssocType, Some("C")), (AssocMacro, None)])
		);
		assert_eq!(
			items("fn a();\nconst B: u8;\ntype C;\nmac!();", Container::Trait),
			expected(&[(AssocFn, Some("a")), (AssocConst, Some("B")), (AssocType, Some("C")), (AssocMacro, None)])
		);
		assert_eq!(
			items("fn a();\nstatic B: u8;\ntype C;\nmac!();", Container::Extern),
			expected(&[
				(ForeignFn, Some("a")),
				(ForeignStatic, Some("B")),
				(ForeignType, Some("C")),
				(ForeignMacro, None)
			])
		);
		assert_eq!(
			items("A, B(u8), C { x: u8 } = 3", Container::Enum),
			expected(&[(Variant, Some("A")), (Variant, Some("B")), (Variant, Some("C"))])
		);
	}

	#[test]
	fn classifies_module_items() {
		use ItemKind::*;

		let source = "
			/// Docs.
			#[derive(Debug)]
			pub struct S(u8);
			enum E { A }
			union U { x: u8 }
			pub(super) fn f() {}
			const C: u8 = 1;
			const _: () = ();
			static S2: u8 = 1;
			type T = u8;
			trait Tr {}
			trait Alias = Tr;
			mod m {}
			mod n;
			impl Tr for S {}
			macro_rules! mac { () => {} }
			mac!();
			use a::b;
			extern crate alloc as heap;
			extern \"C\" { fn g(); }
			fn r#type() {}
		";

		assert_eq!(
			items(source, Container::Module),
			expected(&[
				(Struct, Some("S")),
				(Enum, Some("E")),
				(Union, Some("U")),
				(Fn, Some("f")),
				(Const, Some("C")),
				(Const, None),
				(Static, Some("S2")),
				(TypeAlias, Some("T")),
				(Trait, Some("Tr")),
				(TraitAlias, Some("Alias")),
				(Module, Some("m")),
				(Module, Some("n")),
				(Impl, None),
				(MacroRules, Some("mac")),
				(MacroCall, None),
				(Use, None),
				(ExternCrate, Some("heap")),
				(ExternBlock, None),
				(Fn, Some("type")),
			])
		);
	}

	#[test]
	fn classifies_verbatim_items_like_the_loader() {
		use ItemKind::*;

		let module = "
			fn no_body();
			static S: u8;
			const G<T>: u8 = 1;
			const trait CT { fn f(); }
			pub impl(crate) trait Restricted {}
			const impl CT for u8 { fn f() {} }
			macro m($x:expr) { $x }
			use {::a::b, c};
		";

		assert_eq!(
			items(module, Container::Module),
			expected(&[
				(Fn, Some("no_body")),
				(Static, Some("S")),
				(Const, Some("G")),
				(Trait, Some("CT")),
				(Trait, Some("Restricted")),
				(Impl, None),
				(MacroRules, Some("m")),
				(Use, None),
			])
		);
		assert_eq!(
			items("fn f(&self);\ndefault type T;\nconst C: u8;", Container::Impl),
			expected(&[(AssocFn, Some("f")), (AssocType, Some("T")), (AssocConst, Some("C"))])
		);
		assert_eq!(
			items("pub type T;\nconst C<T>: u8;", Container::Trait),
			expected(&[(AssocType, Some("T")), (AssocConst, Some("C"))])
		);
		assert_eq!(
			items("safe fn f() {}\nunsafe static mut S: u8 = 1;\ntype T = u8;", Container::Extern),
			expected(&[(ForeignFn, Some("f")), (ForeignStatic, Some("S")), (ForeignType, Some("T"))])
		);
	}

	fn error(source: &str, container: Container) -> String {
		match parse_source(source, container) {
			Err(Error::InvalidSource(message)) => message,
			other => panic!("expected an error for {source:?}, got {other:?}"),
		}
	}

	fn expected(items: &[(ItemKind, Option<&str>)]) -> Vec<(ItemKind, Option<String>)> {
		items.iter().map(|&(kind, name)| (kind, name.map(String::from))).collect()
	}

	/// The kinds and names of the items of `source`.
	fn items(source: &str, container: Container) -> Vec<(ItemKind, Option<String>)> {
		parse(source, container)
			.items
			.into_iter()
			.map(|item| (item.kind, item.name.map(String::from)))
			.collect()
	}

	#[test]
	fn keeps_the_text_and_notices_trailing_comments() {
		let parsed = parse("\u{feff}fn a() {}\n", Container::Module);

		assert_eq!(parsed.text, "fn a() {}\n");
		assert!(!parsed.trailing_comment);
		assert!(parse("fn a() {} // done\n", Container::Module).trailing_comment);
		assert!(parse("fn a() {} /* done */", Container::Module).trailing_comment);
		assert!(!parse("// before\nfn a() {}", Container::Module).trailing_comment);

		// a trailing comma of variants is removed
		let parsed = parse("B(u8), // two\n", Container::Enum);

		assert_eq!(parsed.text, "B(u8) // two\n");
		assert!(parsed.trailing_comment);
		assert_eq!(parse("A, B,", Container::Enum).text, "A, B");
		assert_eq!(parse("A", Container::Enum).text, "A");
		assert!(parse("", Container::Module).items.is_empty());
		assert!(parse("  // nothing\n", Container::Impl).items.is_empty());
	}

	fn parse(source: &str, container: Container) -> ParsedSource {
		parse_source(source, container).unwrap_or_else(|error| panic!("{error}"))
	}

	#[test]
	fn records_bindings() {
		let parsed = parse(
			"struct Unit;\nstruct Named {}\nuse a::{b as c, d::{self, e}, f::*, g as _};",
			Container::Module,
		);
		// name, namespaces, and imported path
		type Row<'a> = (&'a str, &'a [Namespace], Option<Vec<&'a str>>);

		let bindings: Vec<Row<'_>> = parsed
			.items
			.iter()
			.flat_map(|item| &item.bindings)
			.map(|binding| {
				let path = binding.import.as_ref().map(|path| path.segments.iter().map(SmolStr::as_str).collect());

				(binding.name.as_str(), binding.namespaces, path)
			})
			.collect();

		assert_eq!(
			bindings,
			[
				("Unit", TYPE_AND_VALUE, None),
				("Named", TYPE, None),
				("c", &Namespace::ALL[..], Some(vec!["a", "b"])),
				("d", TYPE, None),
				("e", &Namespace::ALL[..], Some(vec!["a", "d", "e"])),
			]
		);

		let parsed = parse("extern \"C\" { fn f(); static S: u8; type T; }", Container::Module);
		let names: Vec<&str> = parsed.items[0].bindings.iter().map(|binding| binding.name.as_str()).collect();

		assert_eq!(names, ["f", "S", "T"]);

		let parsed = parse("use ::dep::Item;", Container::Module);

		assert_eq!(
			parsed.items[0].bindings[0].import,
			Some(ImportPath {
				leading_colon: true,
				segments: vec!["dep".into(), "Item".into()]
			})
		);
	}

	#[test]
	fn reports_errors_with_locations() {
		assert_eq!(
			error("fn a() {}\nfn b( {}", Container::Module),
			"the source does not parse as module items at 2:5: cannot parse string into token stream"
		);
		assert_eq!(
			error("fn a() {}\nstruct", Container::Module),
			"the source does not parse as module items at 2:7: unexpected end of input, expected identifier"
		);

		// items of the wrong container
		let message = error("struct S;", Container::Impl);

		assert!(
			message.starts_with("the source does not parse as associated items of an `impl` block at 1:1"),
			"{message}"
		);
		assert!(error("struct S;", Container::Trait).contains("trait items at 1:1"));
		assert!(error("fn f() {}", Container::Enum).contains("enum variants at 1:1"));

		// inner attributes would apply to the container
		assert!(error("#![allow(dead_code)]\nfn f() {}", Container::Module).contains("at 1:1: inner attributes"));
		assert!(error("//! docs\nfn f() {}", Container::Impl).contains("inner attributes"));
		assert!(error("#![allow(x)]", Container::Enum).contains("inner attributes"));

		// a byte order mark does not count as a column
		assert!(error("\u{feff}fn 1() {}", Container::Module).contains("at 1:4:"));
	}
}
