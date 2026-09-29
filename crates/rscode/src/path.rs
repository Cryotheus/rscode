//! Item paths given by users (`crate::foo::Bar`, `::dep::Baz`, `<Foo as Display>::fmt`) and
//! canonical paths of loaded items.

use serde::Serialize;
use smol_str::SmolStr;
use std::borrow::Cow;
use std::fmt::Formatter;

/// The strict and reserved keywords of Rust 2024.
const KEYWORDS: &[&str] = &[
	"Self", "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "crate", "do", "dyn", "else", "enum", "extern",
	"false", "final", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub",
	"ref", "return", "self", "static", "struct", "super", "trait", "true", "try", "type", "typeof", "unsafe", "unsized", "use", "virtual", "where",
	"while", "yield",
];

/// Where a path starts.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "count")]
pub enum Anchor {
	/// No anchor (`foo::Bar`): "presumed absolute", i.e. tried as `crate::foo::Bar`
	/// and, when `foo` names a loaded crate, as `::foo::Bar`.
	#[default]
	None,

	/// `crate::`
	Crate,

	/// `::` (the first segment names a crate).
	Global,

	/// `self::`
	SelfModule,

	/// `super::` (repeated `count` times).
	Super(u32),
}

/// The definition path of a loaded item: `crate_name::module::Item`, with an optional `impl` qualifier.
///
/// Displays like:
/// - `my_crate::a::Foo`, `my_crate::a::Foo::new` (inherent associated item), `my_crate::a::Trait::method`,
///   `my_crate::a::Enum::Variant`;
/// - `<my_crate::a::Foo as Display>::fmt` (trait `impl` item), `impl Display for my_crate::a::Foo` (`impl` block);
/// - `my_crate::a::<impl Trait for Vec<u8>>::method` when the self type is not a loaded item.
///
/// The fields combine as follows:
///
/// | item | `segments` | `impl_trait` | `unresolved_self_ty` | `is_impl` | `name` |
/// |---|---|---|---|---|---|
/// | crate root | empty | | | | crate name |
/// | module item, trait item, variant | crate, modules (, trait or enum) | | | | name |
/// | inherent `impl` item | crate, modules, type | | | | name |
/// | trait `impl` item | crate, modules, type | trait | | | name |
/// | `impl` block | crate, modules, type | trait, if any | | `true` | |
/// | item of an `impl` of an unloaded type | crate, modules | trait, if any | type | | name |
/// | `impl` block of an unloaded type | crate, modules | trait, if any | type | `true` | |
///
/// Names that are keywords display with `r#`. The trait and self type are displayed as written, and so are the generic
/// arguments of a loaded self type in the qualified forms (`<my_crate::a::Foo<T> as From<T>>::from`,
/// `impl my_crate::a::Foo<u8>`), which tell apart the `impl` blocks of one type and trait.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
pub struct CanonicalPath {
	/// Module path segments, starting with the crate name, followed by the owner (type or trait) if any.
	pub segments: Vec<SmolStr>,

	/// For `impl` blocks and their items: the implemented trait as written (`None` for inherent `impl`s).
	pub impl_trait: Option<String>,

	/// For `impl` blocks and their items whose self type is a loaded item: the generic arguments of the self type as
	/// written (`<T, u8>`), if it has any. Paths of items of inherent `impl`s do not show them (`Type::name`), see
	/// [`CanonicalPath::distinct`].
	pub self_ty_arguments: Option<String>,

	/// For `impl` blocks and their items whose self type is not a loaded item: the self type as written.
	/// The `impl` then acts as an anonymous segment after [`CanonicalPath::segments`].
	pub unresolved_self_ty: Option<String>,

	/// Whether the path denotes an `impl` block (rather than an item).
	pub is_impl: bool,

	/// The associated item's name (for `impl`/trait items), or the item's name (last segment) otherwise.
	pub name: Option<SmolStr>,

	/// Whether the path denotes an import (a leaf of a `use` item): its module's path and the name it binds (`*` for
	/// glob imports, `_` for underscore imports). It displays with `use ` in front (`use my_crate::a::Foo`), a path that
	/// names the import itself (see [`ItemPath::import`]).
	pub is_import: bool,
}

impl CanonicalPath {
	/// The path for messages that list several items: its [`Display`](std::fmt::Display), except that an item of an
	/// inherent `impl` whose type has generic arguments shows them, as a path naming it (`<my_crate::Wrapper<u8>>::get`):
	/// the items of `impl Wrapper<u8>` and `impl Wrapper<u16>` have the same path otherwise.
	pub fn distinct(&self) -> String {
		let inherent_item = !self.is_impl && self.impl_trait.is_none() && self.unresolved_self_ty.is_none();

		match (&self.self_ty_arguments, &self.name) {
			(Some(arguments), Some(name)) if inherent_item => {
				let path = |segments: &[SmolStr], name: Option<&SmolStr>| CanonicalPath {
					segments: segments.to_vec(),
					impl_trait: None,
					self_ty_arguments: None,
					unresolved_self_ty: None,
					is_impl: false,
					is_import: false,
					name: name.cloned(),
				};

				format!("<{}{arguments}>::{}", path(&self.segments, None), path(&[], Some(name)))
			}

			_ => self.to_string(),
		}
	}

	/// All matchable segments: `segments`, then an `<impl Trait for Type>` pseudo-segment when the self type is
	/// unresolved, then `name` (qualifiers of resolved `impl`s are flattened away).
	///
	/// `<my_crate::a::Foo as Display>::fmt` gives `["my_crate", "a", "Foo", "fmt"]`, and
	/// `my_crate::a::<impl Trait for Vec<u8>>::method` gives `["my_crate", "a", "<impl Trait for Vec<u8>>", "method"]`.
	pub fn flat_segments(&self) -> Vec<Cow<'_, str>> {
		let mut flat: Vec<Cow<'_, str>> = self.segments.iter().map(|segment| Cow::Borrowed(segment.as_str())).collect();

		flat.extend(self.impl_segment().map(Cow::Owned));
		flat.extend(self.name.as_deref().map(Cow::Borrowed));
		flat
	}

	/// The `<impl Trait for Type>` pseudo-segment of an `impl` whose self type is unresolved.
	fn impl_segment(&self) -> Option<String> {
		let self_ty = self.unresolved_self_ty.as_deref()?;

		Some(match &self.impl_trait {
			Some(trait_text) => format!("<impl {trait_text} for {self_ty}>"),
			None => format!("<impl {self_ty}>"),
		})
	}
}

impl std::fmt::Display for CanonicalPath {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		if self.is_import {
			f.write_str("use ")?;
		}

		if let Some(impl_segment) = self.impl_segment() {
			write_segments(f, &self.segments)?;

			if !self.segments.is_empty() {
				f.write_str("::")?;
			}

			f.write_str(&impl_segment)?;

			if let Some(name) = self.name.as_deref().filter(|_| !self.is_impl) {
				f.write_str("::")?;
				write_ident(f, name)?;
			}

			return Ok(());
		}

		let arguments = self.self_ty_arguments.as_deref().unwrap_or_default();

		if self.is_impl {
			f.write_str("impl ")?;

			if let Some(trait_text) = &self.impl_trait {
				write!(f, "{trait_text} for ")?;
			}

			write_segments(f, &self.segments)?;
			return f.write_str(arguments);
		}

		let mut separate = !self.segments.is_empty();

		match &self.impl_trait {
			Some(trait_text) => {
				f.write_str("<")?;
				write_segments(f, &self.segments)?;
				write!(f, "{arguments} as {trait_text}>")?;
				separate = true;
			}
			None => write_segments(f, &self.segments)?,
		}

		if let Some(name) = &self.name {
			if separate {
				f.write_str("::")?;
			}

			write_ident(f, name)?;
		}

		Ok(())
	}
}

/// A path to an item, as given by a user.
///
/// Grammar:
/// - `[::]segment(::segment)*` where a segment is an identifier (`r#` allowed) or `crate`/`self`/`super`
///   at the start;
/// - `<Path [as Path]>(::segment)*`: segments after the qualifier name associated items of the `impl`
///   (with no segments, the `impl` block itself);
/// - `impl [Trait for] Type`: sugar for `<Type as Trait>` / `<Type>`;
/// - `use [::]segment(::segment)*`: the imports (leaves of `use` items) of the module named by all but the last
///   segment that bind the last segment, which may be `*` (glob imports) or `_` (underscore imports). Other paths go
///   through imports to what they import.
///
/// Details:
/// - Whitespace is allowed around `::`, `<`, `>`, `as`, and `for`, and around the whole path.
/// - `crate` alone names the crate root. `self` and `super` may be followed by more `super`s
///   (`self::super::super::x` is `super::super::x`).
/// - Keywords must be written as raw identifiers (`r#type`) to be used as segments; `Self` and `_` never name items
///   (except for `_` at the end of a `use` path).
/// - The type and trait of a qualifier are plain (unqualified) paths with at least one segment, whose last segment
///   may have generic arguments (`<Wrapper<u8> as From<io::Error>>`), which select the `impl` blocks whose header has
///   the same arguments (as written, whatever the whitespace). Other paths have no generic arguments.
///
/// Segments are stored unraw'd.
#[derive(Debug, Default, Clone, Eq, PartialEq, Hash, Serialize)]
pub struct ItemPath {
	/// Where the path starts. Always [`Anchor::None`] for qualified paths (the qualifier's paths have anchors).
	pub anchor: Anchor,

	/// The `impl` qualifier (`<Type as Trait>`), if any.
	pub qualifier: Option<Qualifier>,

	/// The identifiers after the anchor or qualifier, unraw'd.
	pub segments: Vec<SmolStr>,

	/// The generic arguments of the last segment (of the type or trait of a qualifier), with their angle brackets, as
	/// normalized by [`normalize_arguments`].
	pub arguments: Option<String>,

	/// Whether the path is a `use` path, naming imports rather than what they import: the last segment is the name
	/// they bind (`*` for glob imports, `_` for underscore imports) in the module the other segments name. Such a path
	/// has no qualifier and no generic arguments.
	#[serde(skip_serializing_if = "std::ops::Not::not")]
	pub import: bool,
}

impl ItemPath {
	/// A path of plain identifiers with no anchor.
	pub fn from_segments<S: Into<SmolStr>>(segments: impl IntoIterator<Item = S>) -> Self {
		Self {
			segments: segments.into_iter().map(Into::into).collect(),
			..Self::default()
		}
	}

	/// Parses a path in the syntax described on [`ItemPath`].
	pub fn parse(text: &str) -> Result<Self, PathParseError> {
		let error = |message: String| PathParseError {
			text: text.to_owned(),
			message,
		};

		let tokens = tokenize(text).map_err(error)?;

		Parser {
			tokens,
			index: 0,
			import_name: false,
		}
		.parse()
		.map_err(error)
	}

	/// The last segment.
	pub fn name(&self) -> Option<&SmolStr> {
		self.segments.last()
	}
}

/// Formats in the input syntax (identifiers that are keywords get `r#`).
///
/// `impl` sugar is written as the equivalent qualifier: `impl Display for Foo` displays as `<Foo as Display>`.
impl std::fmt::Display for ItemPath {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		if self.import {
			f.write_str("use ")?;
		}

		if let Some(qualifier) = &self.qualifier {
			write!(f, "<{}", qualifier.self_ty)?;

			if let Some(trait_path) = &qualifier.trait_path {
				write!(f, " as {trait_path}")?;
			}

			f.write_str(">")?;

			for segment in &self.segments {
				f.write_str("::")?;
				write_ident(f, segment)?;
			}

			return Ok(());
		}

		// whether a `::` is needed before the next segment
		let mut separate = match self.anchor {
			Anchor::None => false,
			Anchor::Crate => {
				f.write_str("crate")?;
				true
			}
			Anchor::Global => {
				f.write_str("::")?;
				false
			}
			Anchor::SelfModule => {
				f.write_str("self")?;
				true
			}
			Anchor::Super(count) => {
				for index in 0..count {
					f.write_str(if index == 0 { "super" } else { "::super" })?;
				}

				count > 0
			}
		};

		for segment in &self.segments {
			if separate {
				f.write_str("::")?;
			}

			write_ident(f, segment)?;
			separate = true;
		}

		if let Some(arguments) = &self.arguments {
			f.write_str(arguments)?;
		}

		Ok(())
	}
}

impl std::str::FromStr for ItemPath {
	type Err = PathParseError;

	fn from_str(text: &str) -> Result<Self, Self::Err> {
		Self::parse(text)
	}
}

/// A recursive descent parser over [`Token`]s. Errors are plain messages.
struct Parser<'a> {
	tokens: Vec<Token<'a>>,
	index: usize,

	/// Whether a `use` path is being parsed, whose last segment may be `*` or `_`.
	import_name: bool,
}

impl<'a> Parser<'a> {
	fn bump(&mut self) -> Option<Token<'a>> {
		let token = self.peek();

		self.index += usize::from(token.is_some());
		token
	}

	/// Consumes the next token if it is the keyword `keyword`.
	fn eat_keyword(&mut self, keyword: &str) -> bool {
		let found = self.peek().is_some_and(|token| token.is_word(keyword));

		self.index += usize::from(found);
		found
	}

	fn parse(mut self) -> Result<ItemPath, String> {
		let path = match self.peek() {
			None => return Err("the path is empty".to_owned()),
			Some(token) if token.is_word("impl") => self.parse_impl()?,
			Some(token) if token.is_word("use") => self.parse_use()?,
			Some(Token::Lt) => self.parse_qualified()?,
			Some(_) => self.parse_plain()?,
		};

		match self.bump() {
			None => Ok(path),

			Some(Token::Arguments(_)) => {
				Err("generic arguments are only supported in the type and trait of `<Type as Trait>` and `impl Trait for Type`".to_owned())
			}

			Some(token) => Err(format!("unexpected `{token}`")),
		}
	}

	/// `impl [Trait for] Type`
	fn parse_impl(&mut self) -> Result<ItemPath, String> {
		self.bump();

		let first = self.parse_nested("a type after `impl`")?;

		let qualifier = if self.eat_keyword("for") {
			Qualifier {
				self_ty: self.parse_nested("a type after `for`")?,
				trait_path: Some(first),
			}
		} else {
			Qualifier {
				self_ty: first,
				trait_path: None,
			}
		};

		Ok(ItemPath {
			qualifier: Some(qualifier),
			..ItemPath::default()
		})
	}

	/// A plain path with at least one segment, inside a qualifier.
	fn parse_nested(&mut self, expected: &str) -> Result<Box<ItemPath>, String> {
		match self.peek() {
			None | Some(Token::Gt) => return Err(format!("expected {expected}")),
			Some(Token::Lt) => return Err("nested qualified paths are not supported".to_owned()),
			Some(_) => {}
		}

		let mut path = self.parse_plain()?;

		if path.segments.is_empty() {
			return Err(format!("expected {expected}, found `{path}`"));
		}

		if let Some(Token::Arguments(arguments)) = self.peek() {
			self.bump();
			path.arguments = Some(normalize_arguments(arguments));

			if self.peek() == Some(Token::PathSep) {
				return Err("generic arguments are only supported on the last segment of a type or trait".to_owned());
			}
		}

		Ok(Box::new(path))
	}

	/// `[::]segment(::segment)*`, with `crate`, `self`, and `super` anchors.
	fn parse_plain(&mut self) -> Result<ItemPath, String> {
		let mut path = ItemPath::default();

		match self.peek() {
			Some(Token::PathSep) => {
				self.bump();
				path.anchor = Anchor::Global;
				path.segments.push(self.parse_segment("a crate name after `::`")?);
			}
			Some(token) if token.is_word("crate") => {
				self.bump();
				path.anchor = Anchor::Crate;
			}
			Some(token) if token.is_word("self") => {
				self.bump();
				path.anchor = Anchor::SelfModule;
			}
			Some(token) if token.is_word("super") => {
				self.bump();
				path.anchor = Anchor::Super(1);
			}
			_ => path.segments.push(self.parse_segment("an identifier")?),
		}

		while self.peek() == Some(Token::PathSep) {
			self.bump();

			// `self::super` and `super::super` extend the anchor
			if path.segments.is_empty() && self.peek().is_some_and(|token| token.is_word("super")) {
				path.anchor = match path.anchor {
					Anchor::SelfModule => Anchor::Super(1),
					Anchor::Super(count) => Anchor::Super(count.saturating_add(1)),
					anchor => anchor,
				};

				if matches!(path.anchor, Anchor::Super(_)) {
					self.bump();
					continue;
				}
			}

			path.segments.push(self.parse_segment("an identifier after `::`")?);
		}

		Ok(path)
	}

	/// `<Type [as Trait]>(::segment)*`
	fn parse_qualified(&mut self) -> Result<ItemPath, String> {
		self.bump();

		let self_ty = self.parse_nested("a type after `<`")?;
		let trait_path = if self.eat_keyword("as") {
			Some(self.parse_nested("a trait after `as`")?)
		} else {
			None
		};

		match self.bump() {
			Some(Token::Gt) => {}
			Some(token) => return Err(format!("expected `as` or `>`, found `{token}`")),
			None => return Err("expected `>`".to_owned()),
		}

		let mut segments = Vec::new();

		while self.peek() == Some(Token::PathSep) {
			self.bump();
			segments.push(self.parse_segment("an identifier after `::`")?);
		}

		Ok(ItemPath {
			qualifier: Some(Qualifier { self_ty, trait_path }),
			segments,
			..ItemPath::default()
		})
	}

	/// An identifier that is not an anchor keyword (or `*` or `_` ending a `use` path).
	fn parse_segment(&mut self, expected: &str) -> Result<SmolStr, String> {
		let token = self.bump();
		let last_of_use = self.import_name && self.peek().is_none();

		match token {
			Some(Token::Star) if last_of_use => Ok("*".into()),
			Some(Token::Star) => Err("`*` can only end a `use` path".to_owned()),
			Some(Token::Ident { text: "_", raw: false }) if last_of_use => Ok("_".into()),
			Some(Token::Ident { text, raw: true }) => Ok(text.into()),
			Some(Token::Ident { text, raw: false }) => match text {
				"crate" => Err("`crate` can only start a path".to_owned()),
				"self" => Err("`self` can only start a path".to_owned()),
				"super" => Err("`super` can only start a path or follow `self` or `super`".to_owned()),
				"Self" => Err("`Self` is not supported; name the type instead".to_owned()),
				"_" => Err("`_` does not name an item".to_owned()),
				_ if is_keyword(text) => Err(format!("`{text}` is a keyword; write `r#{text}` for an item named `{text}`")),
				_ => Ok(text.into()),
			},
			Some(token) => Err(format!("expected {expected}, found `{token}`")),
			None => Err(format!("expected {expected}")),
		}
	}

	/// `use [::]segment(::segment)*`, whose last segment may be `*` or `_`.
	fn parse_use(&mut self) -> Result<ItemPath, String> {
		self.bump();

		match self.peek() {
			None => return Err("expected a path after `use` (quote the whole path, `use` included)".to_owned()),
			Some(Token::Lt) => return Err("`use` paths name the imports of modules, and cannot be qualified".to_owned()),
			Some(_) => {}
		}

		self.import_name = true;

		let mut path = self.parse_plain()?;

		match (path.anchor, path.segments.len()) {
			(_, 0) => return Err(format!("expected the name the imports bind after `{path}`")),
			(Anchor::Global, 1) => return Err(format!("expected the name the imports bind after `{path}`")),
			_ => {}
		}

		path.import = true;
		Ok(path)
	}

	fn peek(&self) -> Option<Token<'a>> {
		self.tokens.get(self.index).copied()
	}
}

/// Error parsing an [`ItemPath`] or a pattern.
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
#[error("invalid path `{text}`: {message}")]
pub struct PathParseError {
	/// The text that failed to parse.
	pub text: String,

	/// What is wrong with it.
	pub message: String,
}

/// An `impl` qualifier: `<Type as Trait>` or `<Type>`.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
pub struct Qualifier {
	/// The implementing type.
	pub self_ty: Box<ItemPath>,

	/// The implemented trait (`None` for inherent `impl`s).
	pub trait_path: Option<Box<ItemPath>>,
}

/// A token of an [`ItemPath`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Token<'a> {
	/// An identifier or keyword; `text` excludes the `r#` of raw identifiers.
	Ident { text: &'a str, raw: bool },

	/// `::`
	PathSep,

	/// `<`
	Lt,

	/// `>`
	Gt,

	/// Generic arguments after an identifier, with their angle brackets, as written.
	Arguments(&'a str),

	/// `*`, the name of glob imports at the end of a `use` path.
	Star,
}

impl Token<'_> {
	/// Whether the token is the non-raw identifier or keyword `keyword`.
	fn is_word(self, keyword: &str) -> bool {
		matches!(self, Token::Ident { text, raw: false } if text == keyword)
	}
}

impl std::fmt::Display for Token<'_> {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			Token::Ident { text, raw: true } => write!(f, "r#{text}"),
			Token::Ident { text, raw: false } => f.write_str(text),
			Token::PathSep => f.write_str("::"),
			Token::Lt => f.write_str("<"),
			Token::Gt => f.write_str(">"),
			Token::Arguments(text) => f.write_str(text),
			Token::Star => f.write_str("*"),
		}
	}
}

/// The length of the generic arguments at the start of `text` (which starts with `<`), up to their closing `>` (not
/// counting the `>` of `->`).
pub(crate) fn generic_arguments_len(text: &str) -> Option<usize> {
	let mut depth = 0usize;
	let mut previous = None;

	for (index, c) in text.char_indices() {
		match c {
			'<' => depth += 1,
			'>' if previous == Some('-') => {}

			'>' => {
				depth = depth.checked_sub(1)?;

				if depth == 0 {
					return Some(index + 1);
				}
			}

			_ => {}
		}

		previous = Some(c);
	}

	None
}

/// Whether all characters of `text` may continue an identifier (`XID_Continue`).
pub(crate) fn is_ident_continuation(text: &str) -> bool {
	text.is_empty() || is_ident_lexeme(&format!("_{text}"))
}

/// Whether `text` lexes as a single identifier or keyword: an `XID_Start` character or `_`, followed by
/// `XID_Continue` characters. `r#` is not accepted; keywords and `_` are.
pub(crate) fn is_ident_lexeme(text: &str) -> bool {
	let Some(first) = text.chars().next() else {
		return false;
	};

	if text.is_ascii() {
		return (first.is_ascii_alphabetic() || first == '_') && text.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
	}

	// Whitespace, comments, and punctuation would be skipped or split by the lexer below.
	if text.chars().any(|c| c.is_ascii() && !(c.is_ascii_alphanumeric() || c == '_')) {
		return false;
	}

	// The Unicode XID tables of proc-macro2's lexer decide non-ASCII identifiers.
	let Ok(tokens) = text.parse::<proc_macro2::TokenStream>() else {
		return false;
	};
	let mut tokens = tokens.into_iter();

	match (tokens.next(), tokens.next()) {
		(Some(proc_macro2::TokenTree::Ident(ident)), None) => ident == text,
		_ => false,
	}
}

/// Whether an identifier is a (strict or reserved) keyword that needs `r#` to be used as an identifier.
///
/// These are the keywords of Rust 2024, including reserved ones (`abstract`, `gen`, ...). Contextual keywords
/// (`union`, `auto`, `default`, `safe`, `raw`, `macro_rules`) are not keywords. `_` is not a keyword.
pub fn is_keyword(ident: &str) -> bool {
	KEYWORDS.contains(&ident)
}

/// Keywords that cannot be written as raw identifiers.
fn is_unrawable(name: &str) -> bool {
	matches!(name, "crate" | "self" | "super" | "Self" | "_")
}

/// Whether a string is a valid identifier (optionally `r#`-prefixed), including non-ASCII identifiers.
///
/// Identifiers that cannot name items are invalid: `_`, keywords without `r#` (see [`is_keyword`]), and the
/// keywords that cannot be raw (`r#crate`, `r#self`, `r#super`, `r#Self`).
pub fn is_valid_ident(ident: &str) -> bool {
	let (raw, name) = match ident.strip_prefix("r#") {
		Some(name) => (true, name),
		None => (false, ident),
	};

	if name == "_" || !is_ident_lexeme(name) {
		return false;
	}

	if raw { !is_unrawable(name) } else { !is_keyword(name) }
}

/// Whitespace, including the left-to-right and right-to-left marks that Rust treats as whitespace.
pub(crate) fn is_whitespace(c: char) -> bool {
	c.is_whitespace() || matches!(c, '\u{200e}' | '\u{200f}')
}

/// Characters that may be part of an identifier-like run of text (validated separately).
pub(crate) fn is_word_char(c: char) -> bool {
	c.is_ascii_alphanumeric() || c == '_' || (!c.is_ascii() && !is_whitespace(c))
}

/// The generic arguments of the last segment of a type or trait as written (`<T>` in `&'a Foo<T>` and in
/// `a::Foo<T>`), normalized (see [`normalize_arguments`]). `None` without any, or when they are not the end of the
/// text (`a::B<T>::C`).
pub fn last_segment_arguments(text: &str) -> Option<String> {
	written_arguments(text).map(normalize_arguments)
}

/// Normalizes generic arguments (including their angle brackets) for comparisons: without whitespace, except for a
/// single space between two words (`<&'a mut T, dyn Fn(u8) -> u8>` becomes `<&'a mut T,dyn Fn(u8)->u8>`).
pub fn normalize_arguments(text: &str) -> String {
	let mut normalized = String::with_capacity(text.len());
	let mut space = false;

	for c in text.chars() {
		if is_whitespace(c) {
			space = true;
			continue;
		}

		if space && normalized.ends_with(|last: char| is_word_char(last) || last == '\'') && is_word_char(c) {
			normalized.push(' ');
		}

		normalized.push(c);
		space = false;
	}

	normalized
}

/// Splits a path into tokens, validating identifiers.
fn tokenize(text: &str) -> Result<Vec<Token<'_>>, String> {
	let mut tokens = Vec::new();
	let mut position = 0;

	while let Some(c) = text[position..].chars().next() {
		let rest = &text[position..];

		if is_whitespace(c) {
			position += c.len_utf8();
			continue;
		}

		let (token, length) = match c {
			':' if rest.starts_with("::") => (Token::PathSep, 2),
			':' => return Err("expected `::`, found a single `:`".to_owned()),

			// after an identifier (other than a keyword), generic arguments
			'<' if matches!(tokens.last(), Some(Token::Ident { text, raw }) if *raw || !matches!(*text, "as" | "for" | "impl" | "use")) => {
				let length = generic_arguments_len(rest).ok_or("expected `>` to close the generic arguments")?;

				(Token::Arguments(&rest[..length]), length)
			}

			'<' => (Token::Lt, 1),
			'>' => (Token::Gt, 1),
			_ if rest.starts_with("r#") => {
				let name = &rest[2..2 + word_len(&rest[2..])];

				if name.is_empty() {
					return Err("expected an identifier after `r#`".to_owned());
				}

				if !is_ident_lexeme(name) || is_unrawable(name) {
					return Err(format!("`r#{name}` is not a valid identifier"));
				}

				(Token::Ident { text: name, raw: true }, 2 + name.len())
			}
			_ if is_word_char(c) => {
				let word = &rest[..word_len(rest)];

				if !is_ident_lexeme(word) {
					return Err(format!("`{word}` is not a valid identifier"));
				}

				(Token::Ident { text: word, raw: false }, word.len())
			}
			// glob imports, named at the end of a `use` path
			'*' if tokens.first().is_some_and(|token| token.is_word("use")) => {
				if !matches!(tokens.last(), Some(Token::PathSep) | Some(Token::Ident { text: "use", raw: false })) {
					return Err("`*` can only end a `use` path".to_owned());
				}

				(Token::Star, 1)
			}

			'*' => return Err("wildcards are only supported in patterns (for example by `find`)".to_owned()),
			_ => return Err(format!("unexpected character `{c}`")),
		};

		tokens.push(token);
		position += length;
	}

	Ok(tokens)
}

/// The length of the run of [`is_word_char`]s at the start of `text`.
pub(crate) fn word_len(text: &str) -> usize {
	text.char_indices()
		.find(|&(_, c)| !is_word_char(c))
		.map_or(text.len(), |(index, _)| index)
}

/// Writes a name, with `r#` if it is a keyword.
fn write_ident(f: &mut Formatter<'_>, name: &str) -> std::fmt::Result {
	if is_keyword(name) && !is_unrawable(name) {
		f.write_str("r#")?;
	}

	f.write_str(name)
}

/// Writes names separated by `::`.
fn write_segments(f: &mut Formatter<'_>, segments: &[SmolStr]) -> std::fmt::Result {
	for (index, segment) in segments.iter().enumerate() {
		if index > 0 {
			f.write_str("::")?;
		}

		write_ident(f, segment)?;
	}

	Ok(())
}

/// [`last_segment_arguments`] as written.
pub(crate) fn written_arguments(text: &str) -> Option<&str> {
	let start = text.find('<')?;
	let length = generic_arguments_len(&text[start..])?;

	text[start + length..].trim().is_empty().then(|| &text[start..start + length])
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn accepts_whitespace() {
		assert_eq!(parse("  crate :: a ::b  "), plain(Anchor::Crate, &["a", "b"]));
		assert_eq!(parse(":: dep"), plain(Anchor::Global, &["dep"]));
		assert_eq!(parse("\t< Foo  as  Display >  ::  fmt\n"), parse("<Foo as Display>::fmt"));
		assert_eq!(parse("impl  Display\tfor  Foo "), parse("<Foo as Display>"));
		assert_eq!(parse("a\u{200e}::b"), plain(Anchor::None, &["a", "b"]));
	}

	fn canonical(segments: &[&str], name: Option<&str>) -> CanonicalPath {
		CanonicalPath {
			segments: segments.iter().copied().map(SmolStr::from).collect(),
			impl_trait: None,
			self_ty_arguments: None,
			unresolved_self_ty: None,
			is_impl: false,
			is_import: false,
			name: name.map(SmolStr::from),
		}
	}

	#[test]
	fn displays_canonical_paths() {
		assert_eq!(canonical(&[], Some("my_crate")).to_string(), "my_crate");
		assert_eq!(canonical(&["my_crate", "a"], Some("Foo")).to_string(), "my_crate::a::Foo");
		assert_eq!(canonical(&["my_crate", "r#type"], Some("r#fn")).to_string(), "my_crate::r#type::r#fn");
		assert_eq!(canonical(&["my_crate", "type"], Some("match")).to_string(), "my_crate::r#type::r#match");
		assert_eq!(canonical(&["my_crate", "a"], Some("*")).to_string(), "my_crate::a::*");
		assert_eq!(canonical(&["my_crate", "a"], Some("_")).to_string(), "my_crate::a::_");

		let trait_item = CanonicalPath {
			impl_trait: Some("Display".to_owned()),
			..canonical(&["my_crate", "a", "Foo"], Some("fmt"))
		};

		assert_eq!(trait_item.to_string(), "<my_crate::a::Foo as Display>::fmt");

		let impl_block = CanonicalPath {
			is_impl: true,
			..trait_item.clone()
		};

		assert_eq!(
			CanonicalPath {
				name: None,
				..impl_block.clone()
			}
			.to_string(),
			"impl Display for my_crate::a::Foo"
		);
		assert_eq!(impl_block.to_string(), "impl Display for my_crate::a::Foo");

		let inherent_block = CanonicalPath {
			is_impl: true,
			..canonical(&["my_crate", "a", "Foo"], None)
		};

		assert_eq!(inherent_block.to_string(), "impl my_crate::a::Foo");

		// the generic arguments of the type, in the qualified forms
		let arguments = Some("<T, u8>".to_owned());

		assert_eq!(
			CanonicalPath {
				self_ty_arguments: arguments.clone(),
				..trait_item.clone()
			}
			.to_string(),
			"<my_crate::a::Foo<T, u8> as Display>::fmt"
		);
		assert_eq!(
			CanonicalPath {
				self_ty_arguments: arguments.clone(),
				..impl_block.clone()
			}
			.to_string(),
			"impl Display for my_crate::a::Foo<T, u8>"
		);
		assert_eq!(
			CanonicalPath {
				self_ty_arguments: arguments.clone(),
				..inherent_block
			}
			.to_string(),
			"impl my_crate::a::Foo<T, u8>"
		);

		// items of inherent `impl`s show them only to tell items apart
		let inherent_item = CanonicalPath {
			self_ty_arguments: arguments,
			..canonical(&["my_crate", "Foo"], Some("type"))
		};

		assert_eq!(inherent_item.to_string(), "my_crate::Foo::r#type");
		assert_eq!(inherent_item.distinct(), "<my_crate::Foo<T, u8>>::r#type");
		assert_eq!(
			ItemPath::parse(&inherent_item.distinct()).unwrap().to_string(),
			"<my_crate::Foo<T,u8>>::r#type"
		);
		assert_eq!(trait_item.distinct(), trait_item.to_string());

		let unresolved = CanonicalPath {
			impl_trait: Some("Trait".to_owned()),
			unresolved_self_ty: Some("Vec<u8>".to_owned()),
			..canonical(&["my_crate", "a"], Some("method"))
		};

		assert_eq!(unresolved.to_string(), "my_crate::a::<impl Trait for Vec<u8>>::method");
		assert_eq!(
			CanonicalPath {
				is_impl: true,
				name: None,
				..unresolved.clone()
			}
			.to_string(),
			"my_crate::a::<impl Trait for Vec<u8>>"
		);

		let unresolved_inherent = CanonicalPath {
			impl_trait: None,
			..unresolved.clone()
		};

		assert_eq!(unresolved_inherent.to_string(), "my_crate::a::<impl Vec<u8>>::method");
		assert_eq!(
			CanonicalPath {
				is_impl: true,
				name: None,
				..unresolved_inherent
			}
			.to_string(),
			"my_crate::a::<impl Vec<u8>>"
		);
	}

	#[test]
	fn displays_in_input_syntax() {
		let cases = [
			("foo", "foo"),
			("crate", "crate"),
			("crate :: a::b", "crate::a::b"),
			("::dep::X", "::dep::X"),
			("self::x", "self::x"),
			("self", "self"),
			("super", "super"),
			("self::super::super::x", "super::super::x"),
			("r#type::r#fn", "r#type::r#fn"),
			("r#union", "union"),
			("<Foo as Display>::fmt", "<Foo as Display>::fmt"),
			("impl Display for crate::a::Foo", "<crate::a::Foo as Display>"),
			("impl Foo", "<Foo>"),
			("<Foo>::r#type", "<Foo>::r#type"),
			("use crate::a::Foo", "use crate::a::Foo"),
			("use  Foo", "use Foo"),
			("use ::dep :: *", "use ::dep::*"),
			("use a::_", "use a::_"),
			("use *", "use *"),
			("use r#use::r#type", "use r#use::r#type"),
			("r#use", "r#use"),
		];

		for (input, displayed) in cases {
			let path = parse(input);

			assert_eq!(path.to_string(), displayed, "display of `{input}`");
			assert_eq!(parse(displayed), path, "`{displayed}` does not round-trip");
		}

		assert_eq!(ItemPath::default().to_string(), "");
		assert_eq!(ItemPath::from_segments(["a", "match"]).to_string(), "a::r#match");
	}

	#[test]
	fn finds_the_generic_arguments_of_written_paths() {
		assert_eq!(last_segment_arguments("From<io::Error>").as_deref(), Some("<io::Error>"));
		assert_eq!(last_segment_arguments("&'a Foo<T, U>").as_deref(), Some("<T,U>"));
		assert_eq!(last_segment_arguments("a::Foo<Vec<u8>>").as_deref(), Some("<Vec<u8>>"));
		assert_eq!(last_segment_arguments("Fn<(u8,)>").as_deref(), Some("<(u8,)>"));
		assert_eq!(last_segment_arguments("fmt::Display"), None);
		assert_eq!(last_segment_arguments("a::B<T>::C"), None);
		assert_eq!(last_segment_arguments("Foo<T"), None);
		assert_eq!(normalize_arguments("< dyn  Fn ( u8 )  ->  u8 + 'static >"), "<dyn Fn(u8)->u8+'static>");
	}

	#[test]
	fn flattens_canonical_paths() {
		let flat = |path: &CanonicalPath| path.flat_segments().iter().map(ToString::to_string).collect::<Vec<_>>();

		assert_eq!(flat(&canonical(&[], Some("my_crate"))), ["my_crate"]);
		assert_eq!(flat(&canonical(&["c", "a"], Some("Foo"))), ["c", "a", "Foo"]);

		let trait_item = CanonicalPath {
			impl_trait: Some("Display".to_owned()),
			..canonical(&["c", "a", "Foo"], Some("fmt"))
		};

		assert_eq!(flat(&trait_item), ["c", "a", "Foo", "fmt"]);
		assert_eq!(
			flat(&CanonicalPath {
				is_impl: true,
				name: None,
				..trait_item
			}),
			["c", "a", "Foo"]
		);

		let unresolved = CanonicalPath {
			impl_trait: Some("Trait".to_owned()),
			unresolved_self_ty: Some("Vec<u8>".to_owned()),
			..canonical(&["c", "a"], Some("method"))
		};

		assert_eq!(flat(&unresolved), ["c", "a", "<impl Trait for Vec<u8>>", "method"]);
		assert_eq!(
			flat(&CanonicalPath {
				is_impl: true,
				name: None,
				..unresolved
			}),
			["c", "a", "<impl Trait for Vec<u8>>"]
		);
	}

	#[test]
	fn keywords() {
		for keyword in [
			"as", "async", "await", "dyn", "gen", "try", "Self", "self", "crate", "super", "abstract", "yield", "macro",
		] {
			assert!(is_keyword(keyword), "{keyword}");
		}

		for ident in ["union", "auto", "default", "safe", "raw", "macro_rules", "_", "foo", "Type", "r#type"] {
			assert!(!is_keyword(ident), "{ident}");
		}
	}

	#[test]
	fn name_and_from_str() {
		let path: ItemPath = "crate::a::Foo".parse().unwrap();

		assert_eq!(path.name().map(SmolStr::as_str), Some("Foo"));
		assert_eq!(parse("crate").name(), None);
		assert!("crate::".parse::<ItemPath>().is_err());
		assert_eq!(ItemPath::parse("a b").unwrap_err().to_string(), "invalid path `a b`: unexpected `b`");
	}

	fn parse(text: &str) -> ItemPath {
		ItemPath::parse(text).unwrap_or_else(|error| panic!("{error}"))
	}

	fn parse_error(text: &str) -> String {
		match ItemPath::parse(text) {
			Ok(path) => panic!("`{text}` parsed as {path:?}"),
			Err(error) => {
				assert_eq!(error.text, text);
				error.message
			}
		}
	}

	#[test]
	fn parses_generic_arguments_of_qualifiers() {
		let from = |arguments: &str| with_arguments(plain(Anchor::None, &["From"]), arguments);
		let wrapper = with_arguments(plain(Anchor::Crate, &["Wrapper"]), "<u8>");

		assert_eq!(
			parse("<X as From<u8>>::from"),
			qualified(plain(Anchor::None, &["X"]), Some(from("<u8>")), &["from"])
		);
		assert_eq!(
			parse("impl From<io::Error> for crate::Wrapper<u8>"),
			qualified(wrapper.clone(), Some(from("<io::Error>")), &[])
		);
		assert_eq!(parse("<crate::Wrapper < u8 >>::new"), qualified(wrapper, None, &["new"]));

		// normalized
		assert_eq!(
			parse("<X as OrderingOrBool< L , R>>::left"),
			qualified(
				plain(Anchor::None, &["X"]),
				Some(with_arguments(plain(Anchor::None, &["OrderingOrBool"]), "<L,R>")),
				&["left"]
			)
		);
		assert_eq!(
			parse("impl Fn< (&'a  mut T, [u8; 2]) > for X")
				.qualifier
				.unwrap()
				.trait_path
				.unwrap()
				.arguments
				.as_deref(),
			Some("<(&'a mut T,[u8;2])>")
		);
		assert_eq!(parse("<X as A<fn() -> u8>>").to_string(), "<X as A<fn()->u8>>");
		assert_eq!(parse("<X as From < io :: Error >>::from").to_string(), "<X as From<io::Error>>::from");
		assert_eq!(
			parse("<X as r#as<u8>>").qualifier.unwrap().trait_path.unwrap().arguments.as_deref(),
			Some("<u8>")
		);
	}

	#[test]
	fn parses_plain_paths() {
		assert_eq!(parse("foo"), plain(Anchor::None, &["foo"]));
		assert_eq!(parse("foo::Bar"), plain(Anchor::None, &["foo", "Bar"]));
		assert_eq!(parse("crate"), plain(Anchor::Crate, &[]));
		assert_eq!(parse("crate::a::b"), plain(Anchor::Crate, &["a", "b"]));
		assert_eq!(parse("::dep::Item"), plain(Anchor::Global, &["dep", "Item"]));
		assert_eq!(parse("self::x"), plain(Anchor::SelfModule, &["x"]));
		assert_eq!(parse("self"), plain(Anchor::SelfModule, &[]));
		assert_eq!(parse("super"), plain(Anchor::Super(1), &[]));
		assert_eq!(parse("super::x"), plain(Anchor::Super(1), &["x"]));
		assert_eq!(parse("super::super::x"), plain(Anchor::Super(2), &["x"]));
		assert_eq!(parse("self::super::super::x"), plain(Anchor::Super(2), &["x"]));
	}

	#[test]
	fn parses_qualified_paths() {
		let foo = plain(Anchor::None, &["Foo"]);
		let display = plain(Anchor::None, &["Display"]);

		assert_eq!(parse("<Foo as Display>::fmt"), qualified(foo.clone(), Some(display.clone()), &["fmt"]));
		assert_eq!(parse("<Foo as Display>"), qualified(foo.clone(), Some(display.clone()), &[]));
		assert_eq!(parse("<Foo>::new"), qualified(foo.clone(), None, &["new"]));
		assert_eq!(parse("<Foo>"), qualified(foo.clone(), None, &[]));
		assert_eq!(
			parse("<Foo as Trait>::Assoc::x"),
			qualified(foo.clone(), Some(plain(Anchor::None, &["Trait"])), &["Assoc", "x"])
		);
		assert_eq!(parse("impl Display for Foo"), qualified(foo.clone(), Some(display.clone()), &[]));
		assert_eq!(parse("impl Foo"), qualified(foo, None, &[]));
		assert_eq!(
			parse("<crate::a::Foo as ::std::fmt::Display>::r#fmt"),
			qualified(
				plain(Anchor::Crate, &["a", "Foo"]),
				Some(plain(Anchor::Global, &["std", "fmt", "Display"])),
				&["fmt"]
			),
		);
		assert_eq!(
			parse("impl super::Tr for self::X"),
			qualified(plain(Anchor::SelfModule, &["X"]), Some(plain(Anchor::Super(1), &["Tr"])), &[]),
		);
	}

	#[test]
	fn parses_use_paths() {
		let path = parse("use crate::a::B");

		assert!(path.import && path.qualifier.is_none());
		assert_eq!(
			(path.anchor, path.segments.as_slice()),
			(Anchor::Crate, ["a", "B"].map(SmolStr::new).as_slice())
		);
		assert_eq!(parse("use a::*").segments.last().unwrap(), "*");
		assert_eq!(parse("use a::_").segments.last().unwrap(), "_");
		assert!(!parse("crate::a::B").import);
	}

	fn plain(anchor: Anchor, segments: &[&str]) -> ItemPath {
		ItemPath {
			anchor,
			segments: segments.iter().copied().map(SmolStr::from).collect(),
			..ItemPath::default()
		}
	}

	fn qualified(self_ty: ItemPath, trait_path: Option<ItemPath>, segments: &[&str]) -> ItemPath {
		ItemPath {
			qualifier: Some(Qualifier {
				self_ty: Box::new(self_ty),
				trait_path: trait_path.map(Box::new),
			}),
			segments: segments.iter().copied().map(SmolStr::from).collect(),
			..ItemPath::default()
		}
	}

	/// Random inputs never panic, and whatever parses displays as text that parses to the same path.
	#[test]
	fn random_paths_round_trip() {
		const PIECES: &[&str] = &[
			"::", ":", "<", ">", " as ", " for ", "impl ", "use ", "crate", "self", "super", "Self", "r#", "a", "b", "type", "é", "_", "1", " ",
			"\u{200e}", "😀", "-", "#", "*",
		];

		// xorshift, for reproducible inputs without dependencies
		let mut state = 0x2545_f491_4f6c_dd1d_u64;
		let mut random = |bound: usize| {
			state ^= state << 13;
			state ^= state >> 7;
			state ^= state << 17;
			(state % bound as u64) as usize
		};
		let mut parsed = 0;

		for _ in 0..20_000 {
			let length = random(9);
			let text: String = (0..length).map(|_| PIECES[random(PIECES.len())]).collect();

			let Ok(path) = ItemPath::parse(&text) else {
				continue;
			};
			let displayed = path.to_string();

			parsed += 1;
			assert_eq!(ItemPath::parse(&displayed).as_ref(), Ok(&path), "`{text}` displayed as `{displayed}`");
		}

		assert!(parsed > 500, "only {parsed} random paths parsed");
	}

	#[test]
	fn rejects_invalid_paths() {
		assert_eq!(parse_error(""), "the path is empty");
		assert_eq!(parse_error("   "), "the path is empty");
		assert_eq!(parse_error("::"), "expected a crate name after `::`");
		assert_eq!(parse_error("crate::"), "expected an identifier after `::`");
		assert_eq!(parse_error("a::::b"), "expected an identifier after `::`, found `::`");
		assert_eq!(parse_error("a:b"), "expected `::`, found a single `:`");
		assert_eq!(parse_error("a: :b"), "expected `::`, found a single `:`");
		assert_eq!(parse_error("a b"), "unexpected `b`");
		assert_eq!(parse_error("a::crate"), "`crate` can only start a path");
		assert_eq!(parse_error("::crate::a"), "`crate` can only start a path");
		assert_eq!(parse_error("a::self"), "`self` can only start a path");
		assert_eq!(parse_error("self::self"), "`self` can only start a path");
		assert_eq!(parse_error("a::super"), "`super` can only start a path or follow `self` or `super`");
		assert_eq!(
			parse_error("crate::super::a"),
			"`super` can only start a path or follow `self` or `super`"
		);
		assert_eq!(
			parse_error("super::a::super"),
			"`super` can only start a path or follow `self` or `super`"
		);
		assert_eq!(parse_error("Self::new"), "`Self` is not supported; name the type instead");
		assert_eq!(parse_error("a::_"), "`_` does not name an item");
		assert_eq!(parse_error("fn"), "`fn` is a keyword; write `r#fn` for an item named `fn`");
		assert_eq!(parse_error("a::type"), "`type` is a keyword; write `r#type` for an item named `type`");
		assert_eq!(parse_error("gen"), "`gen` is a keyword; write `r#gen` for an item named `gen`");
		assert_eq!(parse_error("r#crate"), "`r#crate` is not a valid identifier");
		assert_eq!(parse_error("r#self::a"), "`r#self` is not a valid identifier");
		assert_eq!(parse_error("r#Self"), "`r#Self` is not a valid identifier");
		assert_eq!(parse_error("r#_"), "`r#_` is not a valid identifier");
		assert_eq!(parse_error("r#"), "expected an identifier after `r#`");
		assert_eq!(parse_error("1abc"), "`1abc` is not a valid identifier");
		assert_eq!(parse_error("a-b"), "unexpected character `-`");
		assert_eq!(parse_error("a#b"), "unexpected character `#`");
		assert_eq!(parse_error("smile😀"), "`smile😀` is not a valid identifier");
		assert_eq!(parse_error("foo*"), "wildcards are only supported in patterns (for example by `find`)");

		// `use` paths
		assert_eq!(parse_error("use"), "expected a path after `use` (quote the whole path, `use` included)");
		assert_eq!(parse_error("use crate"), "expected the name the imports bind after `crate`");
		assert_eq!(parse_error("use ::dep"), "expected the name the imports bind after `::dep`");
		assert_eq!(
			parse_error("use <A as B>::c"),
			"`use` paths name the imports of modules, and cannot be qualified"
		);
		assert_eq!(parse_error("use a::*::b"), "`*` can only end a `use` path");
		assert_eq!(parse_error("use a::_::b"), "`_` does not name an item");
		assert_eq!(parse_error("use a::**"), "`*` can only end a `use` path");
		assert_eq!(parse_error("use use a"), "`use` is a keyword; write `r#use` for an item named `use`");
		assert_eq!(parse_error("a::*"), "wildcards are only supported in patterns (for example by `find`)");
		assert!(parse_error("use a::b<u8>").starts_with("generic arguments are only supported in the type and trait"));
		assert_eq!(parse_error("impl use a"), "`use` is a keyword; write `r#use` for an item named `use`");
	}

	#[test]
	fn rejects_invalid_qualified_paths() {
		let only_qualifiers = "generic arguments are only supported in the type and trait of `<Type as Trait>` and `impl Trait for Type`";

		assert_eq!(parse_error("Vec<u8>"), only_qualifiers);
		assert_eq!(parse_error("Vec<u8>::new"), only_qualifiers);
		assert_eq!(
			parse_error("<X as a::B<u8>::C>"),
			"generic arguments are only supported on the last segment of a type or trait"
		);
		assert_eq!(parse_error("<X as From<u8"), "expected `>` to close the generic arguments");
		assert_eq!(parse_error("<X as From<u8>"), "expected `>`");
		assert_eq!(parse_error("<<A as B>::C as D>"), "nested qualified paths are not supported");
		assert_eq!(parse_error("<>"), "expected a type after `<`");
		assert_eq!(parse_error("<Foo as>"), "expected a trait after `as`");
		assert_eq!(parse_error("<Foo"), "expected `>`");
		assert_eq!(parse_error("<Foo Bar>"), "expected `as` or `>`, found `Bar`");
		assert_eq!(parse_error("<Foo>fmt"), "unexpected `fmt`");
		assert_eq!(parse_error("<Foo>::crate"), "`crate` can only start a path");
		assert_eq!(parse_error("<crate as X>"), "expected a type after `<`, found `crate`");
		assert_eq!(parse_error("<Self as X>"), "`Self` is not supported; name the type instead");
		assert_eq!(parse_error("impl"), "expected a type after `impl`");
		assert_eq!(parse_error("impl X for"), "expected a type after `for`");
		assert_eq!(parse_error("impl X for Y::z::w a"), "unexpected `a`");
		assert_eq!(parse_error("impl X for Y::fmt>"), "unexpected `>`");
		assert_eq!(parse_error("a::<Foo>"), "expected an identifier after `::`, found `<`");
		assert_eq!(parse_error("x>"), "unexpected `>`");
	}

	#[test]
	fn unraws_segments() {
		assert_eq!(parse("r#type"), plain(Anchor::None, &["type"]));
		assert_eq!(parse("crate::r#fn::r#match"), plain(Anchor::Crate, &["fn", "match"]));
		assert_eq!(parse("r#union"), plain(Anchor::None, &["union"]));
		assert_eq!(parse("union::default::r#gen"), plain(Anchor::None, &["union", "default", "gen"]));
		assert_eq!(parse("über::Straße"), plain(Anchor::None, &["über", "Straße"]));
	}

	#[test]
	fn valid_identifiers() {
		let valid = [
			"foo", "_foo", "Foo1", "r#type", "r#gen", "r#union", "union", "é", "Straße", "a\u{301}", "_1",
		];

		for ident in valid.into_iter().chain(["r#r"]) {
			assert!(is_valid_ident(ident), "{ident}");
		}

		// not names, or keywords
		let keywords = [
			"", "_", "r#_", "r#", "r#crate", "r#self", "r#super", "r#Self", "type", "gen", "self", "crate",
		];

		// not identifiers
		let malformed = ["Self", "1a", "a-b", "a b", " a", "a ", "a::b", "r#r#a", "a/**/", "😀", "a😀", "a#", "'a"];

		for ident in keywords.into_iter().chain(malformed).chain(["\u{301}a", "\u{feff}a", "a\n"]) {
			assert!(!is_valid_ident(ident), "{ident:?}");
		}
	}

	fn with_arguments(mut path: ItemPath, arguments: &str) -> ItemPath {
		path.arguments = Some(arguments.to_owned());
		path
	}
}
