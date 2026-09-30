//! Glob-like patterns for identifiers and item paths. No regex.
//!
//! Identifier patterns use `*` as a wildcard for any (possibly empty) run of characters:
//!
//! | pattern | matches |
//! |---|---|
//! | `foo` | exactly `foo` (raw identifiers compare unraw'd: `r#type` = `type`) |
//! | `foo*` | starts with `foo` |
//! | `*foo` | ends with `foo` |
//! | `*foo*` | contains `foo` |
//! | `foo*bar` | starts with `foo`, ends with `bar` |
//! | `*foo*bar*` | contains `foo`, and `bar` after it |
//!
//! Path patterns are identifier patterns separated by `::`.
//! A `**` segment matches any number of whole segments:
//! `foo::*` matches the items directly in `foo`, `foo::**` everything below `foo`,
//! `**::Blam` any `Blam`, `a::**::b` any `b` below `a`.
//! `foo**` and `**foo` are shorthand for `foo::**` and `**::foo`.
//! A `**` at the end matches at least one segment (`foo::**` does not match `foo` itself); elsewhere it may match
//! none (`a::**::b` matches `a::b`).
//!
//! Patterns without an anchor (`crate::`, `::`, `self::`, `super::`) are unanchored: they match any suffix of an
//! item's canonical path (`crate_name::module::Item`). A pattern of a single identifier pattern therefore
//! matches item names anywhere. `crate::` patterns match from the root of each selected crate, and `::name`
//! patterns from the root of the crate `name`. `self::` and `super::` are not supported in patterns.
//!
//! Qualified patterns `<TypePattern as TraitPattern>::name` match associated items of `impl` blocks, and
//! `<TypePattern>::name` those of inherent `impl`s. Without trailing segments (`<Foo as Display>`), or written as
//! `impl TraitPattern for TypePattern` / `impl TypePattern`, they match the `impl` blocks themselves.
//! Unqualified patterns match `impl` items through their owner (`Foo::fmt` matches `<Foo as Display>::fmt`), but
//! never `impl` blocks.
//!
//! `use` patterns (`use crate::a::*`, `use Circle`) only match imports (the leaves of `use` items), whose paths are
//! those of their modules followed by the names they bind (`*` for glob imports, `_` for underscore imports). Other
//! patterns match imports too, when the caller searches imports.

use crate::path::Anchor;
use crate::path::CanonicalPath;
use crate::path::ItemPath;
use crate::path::PathParseError;
use crate::path::Qualifier;
use crate::path::generic_arguments_len;
use crate::path::is_ident_continuation;
use crate::path::is_ident_lexeme;
use crate::path::is_keyword;
use crate::path::last_segment_arguments;
use crate::path::normalize_arguments;
use serde::Deserialize;
use serde::Serialize;
use smol_str::SmolStr;

/// The error for `use` without a pattern after it (usually a shell splitting an unquoted pattern).
const EXPECTED_AFTER_USE: &str = "expected a pattern after `use` (quote the whole pattern, `use` included)";

/// The error for generic arguments where patterns cannot have them.
const ONLY_QUALIFIER_ARGUMENTS: &str =
	"generic arguments are only supported in the type and trait of `<Type as Trait>` and `impl Trait for Type` patterns";

/// A pattern for a single identifier.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct IdentPattern {
	/// Literal parts between `*`s (unraw'd; lowercased when ignoring case). Empty parts are omitted.
	pub(crate) parts: Vec<String>,
	pub(crate) anchored_start: bool,
	pub(crate) anchored_end: bool,
	pub(crate) ignore_case: bool,
}

impl IdentPattern {
	/// A lone `*`.
	pub fn any(options: MatchOptions) -> Self {
		Self::literal("", options, false, false)
	}

	/// Contains `text` (taken literally, without wildcards; a leading `r#` is ignored).
	pub fn contains(text: &str, options: MatchOptions) -> Self {
		Self::literal(text, options, false, false)
	}

	/// Ends with `text` (taken literally, without wildcards; a leading `r#` is ignored).
	pub fn ends_with(text: &str, options: MatchOptions) -> Self {
		Self::literal(text, options, false, true)
	}

	/// Exactly `text` (taken literally, without wildcards; a leading `r#` is ignored).
	pub fn exact(text: &str, options: MatchOptions) -> Self {
		Self::literal(text, options, true, true)
	}

	/// A pattern from `*`-separated parts, without validation.
	fn glob(text: &str, options: MatchOptions) -> Self {
		Self {
			parts: text
				.split('*')
				.filter(|part| !part.is_empty())
				.map(|part| normalize(part, options))
				.collect(),
			anchored_start: !text.starts_with('*'),
			anchored_end: !text.ends_with('*'),
			ignore_case: options.ignore_case,
		}
	}

	fn literal(text: &str, options: MatchOptions, anchored_start: bool, anchored_end: bool) -> Self {
		let text = text.strip_prefix("r#").unwrap_or(text);

		Self {
			parts: if text.is_empty() { Vec::new() } else { vec![normalize(text, options)] },
			anchored_start,
			anchored_end,
			ignore_case: options.ignore_case,
		}
	}

	/// Parses an identifier pattern: identifier characters and `*` wildcards, optionally prefixed with `r#`.
	pub fn parse(pattern: &str, options: MatchOptions) -> Result<Self, PathParseError> {
		parse_ident_pattern(pattern.trim(), options).map_err(|message| PathParseError {
			text: pattern.to_owned(),
			message,
		})
	}

	/// Starts with `text` (taken literally, without wildcards; a leading `r#` is ignored).
	pub fn starts_with(text: &str, options: MatchOptions) -> Self {
		Self::literal(text, options, true, false)
	}

	/// The identifier an exact, case-sensitive pattern matches, if it can name an item.
	fn exact_ident(&self) -> Option<SmolStr> {
		let [part] = self.parts.as_slice() else {
			return None;
		};
		let nameable = is_ident_lexeme(part) && !is_anchor_word(part) && part != "_";

		(self.is_exact() && !self.ignore_case && nameable).then(|| part.into())
	}

	/// Whether the pattern ignores case.
	pub fn ignores_case(&self) -> bool {
		self.ignore_case
	}

	/// Whether the pattern is a lone `*`.
	pub fn is_any(&self) -> bool {
		!self.anchored_start && !self.anchored_end && self.parts.iter().all(String::is_empty)
	}

	/// Whether the pattern has no wildcards.
	pub fn is_exact(&self) -> bool {
		self.anchored_start && self.anchored_end && self.parts.len() <= 1
	}

	/// Whether the pattern is exactly `_` (the name of underscore imports).
	fn is_exact_underscore(&self) -> bool {
		self.is_exact() && self.parts.as_slice() == ["_"]
	}

	/// Whether `ident` (compared unraw'd) matches.
	pub fn matches(&self, ident: &str) -> bool {
		let ident = ident.strip_prefix("r#").unwrap_or(ident);

		// ASCII without uppercase letters is already lowercase
		if self.ignore_case && (!ident.is_ascii() || ident.bytes().any(|byte| byte.is_ascii_uppercase())) {
			return self.matches_normalized(&ident.to_lowercase());
		}

		self.matches_normalized(ident)
	}

	/// Matches a candidate that is already unraw'd (and lowercased when ignoring case).
	///
	/// Greedy leftmost matching is exact for `*`-only globs: the first part must be a prefix (when anchored), the last
	/// a suffix (when anchored), and the others must occur in order, without overlapping.
	fn matches_normalized(&self, candidate: &str) -> bool {
		if self.is_exact() {
			return match self.parts.first() {
				Some(part) => candidate == part,
				None => candidate.is_empty(),
			};
		}

		let mut parts = self.parts.as_slice();
		let mut rest = candidate;

		if self.anchored_start
			&& let Some((first, others)) = parts.split_first()
		{
			let Some(after) = rest.strip_prefix(first.as_str()) else {
				return false;
			};

			rest = after;
			parts = others;
		}

		if self.anchored_end
			&& let Some((last, others)) = parts.split_last()
		{
			let Some(before) = rest.strip_suffix(last.as_str()) else {
				return false;
			};

			rest = before;
			parts = others;
		}

		for part in parts {
			let Some(index) = rest.find(part.as_str()) else {
				return false;
			};

			rest = &rest[index + part.len()..];
		}

		true
	}
}

/// Formats in the pattern syntax. Exact keyword patterns are written with `r#`.
impl std::fmt::Display for IdentPattern {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if self.parts.is_empty() {
			return f.write_str(if self.is_exact() { "" } else { "*" });
		}

		if self.is_exact() && is_keyword(&self.parts[0]) && !is_anchor_word(&self.parts[0]) && self.parts[0] != "Self" {
			f.write_str("r#")?;
		}

		if !self.anchored_start {
			f.write_str("*")?;
		}

		f.write_str(&self.parts.join("*"))?;

		if !self.anchored_end {
			f.write_str("*")?;
		}

		Ok(())
	}
}

/// Options shared by all patterns.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct MatchOptions {
	/// Compare identifiers case-insensitively (Unicode lowercase).
	pub ignore_case: bool,
}

/// A pattern for item paths.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct PathPattern {
	/// Where the pattern starts. Always [`Anchor::None`] for qualified patterns.
	pub anchor: Anchor,

	/// `<SelfTy as Trait>` qualifier patterns.
	pub qualifier: Option<(Box<PathPattern>, Option<Box<PathPattern>>)>,

	/// The segments after the anchor or qualifier.
	pub segments: Vec<SegmentPattern>,

	/// The generic arguments of the last segment (of the type or trait of a qualifier), normalized like
	/// [`ItemPath::arguments`]: only `impl` blocks whose header has them match.
	pub arguments: Option<String>,

	/// Whether the pattern is a `use` pattern, which only matches imports (see [`CanonicalPath::is_import`]).
	pub import: bool,
}

impl PathPattern {
	/// A pattern matching exactly what an item path names (its segments are compared literally: `use a::*` matches the
	/// glob imports of `a` only, unlike the parsed pattern).
	pub fn exact(path: &ItemPath) -> Self {
		let exact = |path: &ItemPath| Box::new(Self::exact(path));

		Self {
			anchor: path.anchor,
			qualifier: (path.qualifier.as_ref()).map(|qualifier| (exact(&qualifier.self_ty), qualifier.trait_path.as_deref().map(exact))),
			segments: (path.segments.iter())
				.map(|segment| SegmentPattern::Ident(IdentPattern::exact(segment, MatchOptions::default())))
				.collect(),
			arguments: path.arguments.clone(),
			import: path.import,
		}
	}

	/// A pattern matching items named by `name` anywhere.
	pub fn from_ident(name: IdentPattern) -> Self {
		Self {
			anchor: Anchor::None,
			qualifier: None,
			segments: vec![SegmentPattern::Ident(name)],
			arguments: None,
			import: false,
		}
	}

	/// Parses a path pattern in the syntax described in the [module docs](self).
	pub fn parse(pattern: &str, options: MatchOptions) -> Result<Self, PathParseError> {
		parse_path_pattern(pattern.trim(), options, false).map_err(|message| PathParseError {
			text: pattern.to_owned(),
			message,
		})
	}

	/// Whether the pattern is a `use` pattern, which only matches imports.
	pub fn is_import(&self) -> bool {
		self.import
	}

	/// Whether the pattern has an `impl` qualifier (`<Type as Trait>`), and so only matches `impl` blocks and their
	/// items.
	///
	/// A [`CanonicalPath`] of an inherent associated item (`a::Type::new`) looks like the path of any other item, so
	/// [`PathPattern::matches`] cannot tell whether an item matched by `<Type>::new` comes from an `impl` block.
	/// Callers that know the item's kind should additionally require it to be an `impl` block or `impl` item.
	pub fn is_qualified(&self) -> bool {
		self.qualifier.is_some()
	}

	/// Matches a canonical path, honoring the anchor and `impl` qualifiers.
	///
	/// `is_selected` tells whether the crate the path belongs to is part of the user's selection (only selected
	/// crates match `crate::` anchors). See [`PathPattern::is_qualified`] for a limitation of qualified patterns.
	pub fn matches(&self, path: &CanonicalPath, is_selected: bool) -> bool {
		if self.import && !path.is_import {
			return false;
		}

		match &self.qualifier {
			Some((self_ty, trait_pattern)) => self.matches_qualified(path, is_selected, self_ty, trait_pattern.as_deref()),
			None if path.is_impl => false,

			None => {
				// most candidates fail on their name, which needs no flattening
				if let (Some(SegmentPattern::Ident(last)), Some(name)) = (self.segments.last(), &path.name)
					&& !last.matches(name)
				{
					return false;
				}

				let flat = path.flat_segments();
				let flat: Vec<&str> = flat.iter().map(AsRef::as_ref).collect();

				self.matches_from_crate(&flat, is_selected)
			}
		}
	}

	/// Whether the generic arguments of a type or trait as written (or just its generic arguments) are the pattern's,
	/// if it has any.
	fn matches_arguments(&self, written: Option<&str>) -> bool {
		self.arguments
			.as_ref()
			.is_none_or(|arguments| written.and_then(last_segment_arguments).as_ref() == Some(arguments))
	}

	/// Matches segments starting with a crate name, honoring the anchor.
	fn matches_from_crate(&self, segments: &[&str], is_selected: bool) -> bool {
		match self.anchor {
			Anchor::None | Anchor::Global => self.matches_segments(segments),
			Anchor::Crate => is_selected && segments.split_first().is_some_and(|(_, rest)| self.matches_segments(rest)),
			Anchor::SelfModule | Anchor::Super(_) => false,
		}
	}

	fn matches_qualified(&self, path: &CanonicalPath, is_selected: bool, self_ty: &PathPattern, trait_pattern: Option<&PathPattern>) -> bool {
		let trait_matches = match (trait_pattern, &path.impl_trait) {
			(Some(trait_pattern), Some(trait_text)) => trait_pattern.matches_written(trait_text) && trait_pattern.matches_arguments(Some(trait_text)),
			(None, None) => true,
			_ => false,
		};

		if !trait_matches {
			return false;
		}

		let owner_matches = match &path.unresolved_self_ty {
			Some(self_ty_text) => self_ty.matches_written(self_ty_text) && self_ty.matches_arguments(Some(self_ty_text)),

			None => {
				let owner: Vec<&str> = path.segments.iter().map(SmolStr::as_str).collect();

				self_ty.matches_from_crate(&owner, is_selected) && self_ty.matches_arguments(path.self_ty_arguments.as_deref())
			}
		};

		if !owner_matches {
			return false;
		}

		match (self.segments.is_empty(), &path.name) {
			(true, _) => path.is_impl,
			(false, Some(name)) => !path.is_impl && match_segments(&self.segments, &[name.as_str()], false),
			(false, None) => false,
		}
	}

	/// Matches a sequence of segments.
	///
	/// With [`Anchor::None`] the pattern may match any suffix of `segments`. Other anchors are matched from the
	/// start of `segments`, which the caller must have already stripped of the anchor's prefix
	/// (e.g. the crate name for [`Anchor::Crate`]). The qualifier is ignored.
	pub fn matches_segments(&self, segments: &[&str]) -> bool {
		match_segments(&self.segments, segments, self.anchor == Anchor::None)
	}

	/// Matches a path as written in source (an `impl`'s trait or unresolved self type), ignoring generic arguments,
	/// references, and anchors (`crate`, `self`, `super`, `::`).
	///
	/// Written paths may be relative to imports, so the pattern matches when it matches a suffix of the written path,
	/// or when the written path matches a suffix of the pattern (`std::fmt::Display` matches `fmt::Display`).
	fn matches_written(&self, text: &str) -> bool {
		let simplified = simplify_written_path(text);
		let segments = written_segments(&simplified, text);

		match_segments(&self.segments, &segments, true)
			|| (1..self.segments.len()).any(|skip| match_segments(&self.segments[skip..], &segments, false))
	}

	/// The equivalent [`ItemPath`], if the pattern has no wildcards and is case-sensitive.
	///
	/// Note that unanchored patterns match anywhere, while unanchored item paths are resolved from crate roots.
	pub fn to_item_path(&self) -> Option<ItemPath> {
		let anchor = match self.anchor {
			Anchor::None | Anchor::Crate | Anchor::Global => self.anchor,
			Anchor::SelfModule | Anchor::Super(_) => return None,
		};
		let last = self.segments.len().saturating_sub(1);
		let segments = self
			.segments
			.iter()
			.enumerate()
			.map(|(index, segment)| match segment {
				// the name of underscore imports (a `*` stays a wildcard: it matches any import)
				SegmentPattern::Ident(pattern) if self.import && index == last && pattern.is_exact_underscore() => Some(SmolStr::new_static("_")),

				SegmentPattern::Ident(pattern) => pattern.exact_ident(),
				SegmentPattern::AnyDepth => None,
			})
			.collect::<Option<Vec<SmolStr>>>()?;
		let qualifier = match &self.qualifier {
			None => None,

			Some((self_ty, trait_pattern)) => Some(Qualifier {
				self_ty: Box::new(self_ty.to_item_path()?),
				trait_path: match trait_pattern {
					Some(trait_pattern) => Some(Box::new(trait_pattern.to_item_path()?)),
					None => None,
				},
			}),
		};

		Some(ItemPath {
			anchor,
			qualifier,
			segments,
			arguments: self.arguments.clone(),
			import: self.import,
		})
	}
}

/// Formats in the pattern syntax (`impl` sugar is written as the equivalent qualifier).
impl std::fmt::Display for PathPattern {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if self.import {
			f.write_str("use ")?;
		}

		if let Some((self_ty, trait_pattern)) = &self.qualifier {
			write!(f, "<{self_ty}")?;

			if let Some(trait_pattern) = trait_pattern {
				write!(f, " as {trait_pattern}")?;
			}

			f.write_str(">")?;

			for segment in &self.segments {
				write!(f, "::{segment}")?;
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

			write!(f, "{segment}")?;
			separate = true;
		}

		if let Some(arguments) = &self.arguments {
			f.write_str(arguments)?;
		}

		Ok(())
	}
}

/// One segment of a [`PathPattern`].
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum SegmentPattern {
	/// One segment matching an identifier pattern.
	Ident(IdentPattern),

	/// `**`
	AnyDepth,
}

impl std::fmt::Display for SegmentPattern {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Ident(pattern) => pattern.fmt(f),
			Self::AnyDepth => f.write_str("**"),
		}
	}
}

/// `crate`, `self`, and `super`, which are anchors rather than names.
fn is_anchor_word(text: &str) -> bool {
	matches!(text, "crate" | "self" | "super")
}

/// Whether `pattern` matches all of `segments`, or with `suffix`, any suffix of them.
///
/// A `**` matches zero or more segments, except at the end, where it matches one or more. Computed by dynamic
/// programming over (pattern position, segment position), so many `**`s cannot cause exponential backtracking.
fn match_segments(pattern: &[SegmentPattern], segments: &[&str], suffix: bool) -> bool {
	let count = segments.len();

	// row[j]: whether the pattern from the current position matches `segments[j..]`
	let mut row = vec![false; count + 1];

	row[count] = true;

	for (index, segment_pattern) in pattern.iter().enumerate().rev() {
		let mut next = vec![false; count + 1];

		match segment_pattern {
			SegmentPattern::AnyDepth if index + 1 == pattern.len() => next[..count].fill(true),

			SegmentPattern::AnyDepth => {
				next[count] = row[count];

				for position in (0..count).rev() {
					next[position] = row[position] || next[position + 1];
				}
			}

			SegmentPattern::Ident(ident) => {
				for position in 0..count {
					next[position] = row[position + 1] && ident.matches(segments[position]);
				}
			}
		}

		row = next;
	}

	if suffix { row.contains(&true) } else { row[0] }
}

/// Lowercases when ignoring case.
fn normalize(text: &str, options: MatchOptions) -> String {
	if options.ignore_case { text.to_lowercase() } else { text.to_owned() }
}

fn parse_ident_pattern(text: &str, options: MatchOptions) -> Result<IdentPattern, String> {
	let glob = text.strip_prefix("r#").unwrap_or(text);

	if glob.is_empty() {
		return Err("expected an identifier pattern".to_owned());
	}

	for part in glob.split('*') {
		if !is_ident_continuation(part) {
			let invalid = part.chars().find(|&c| !is_ident_continuation(c.encode_utf8(&mut [0; 4]))).unwrap_or('?');

			return Err(format!(
				"{invalid:?} is not allowed in identifier patterns (only identifier characters and `*`)"
			));
		}
	}

	Ok(IdentPattern::glob(glob, options))
}

/// A pattern inside of a qualifier.
fn parse_nested(text: &str, options: MatchOptions, expected: &str) -> Result<Box<PathPattern>, String> {
	let text = text.trim();

	if text.is_empty() {
		return Err(format!("expected {expected}"));
	}

	Ok(Box::new(parse_path_pattern(text, options, true)?))
}

/// Parses a path pattern; `nested` patterns (inside a qualifier) cannot be qualified and need a segment.
fn parse_path_pattern(text: &str, options: MatchOptions, nested: bool) -> Result<PathPattern, String> {
	if text.is_empty() {
		return Err("the pattern is empty".to_owned());
	}

	// (items named `use` are matched by `r#use`)
	if text == "use" {
		return Err(EXPECTED_AFTER_USE.to_owned());
	}

	if let Some(rest) = strip_word(text, "use") {
		if nested {
			return Err("`use` cannot appear inside of a qualifier".to_owned());
		}

		let rest = rest.trim();

		if rest.is_empty() {
			return Err(EXPECTED_AFTER_USE.to_owned());
		}

		let mut pattern = parse_path_pattern(rest, options, false)?;

		if pattern.qualifier.is_some() || pattern.import {
			return Err("`use` patterns match the imports of modules, and cannot be qualified".to_owned());
		}

		if pattern.segments.is_empty() {
			return Err(format!("expected a pattern of the names imports bind after `use {rest}`"));
		}

		pattern.import = true;
		return Ok(pattern);
	}

	if let Some(rest) = strip_word(text, "impl") {
		if nested {
			return Err("`impl` cannot appear inside of a qualifier".to_owned());
		}

		let (trait_text, self_text) = match split_word(rest, "for") {
			Some((trait_text, self_text)) => (Some(trait_text), self_text),
			None => (None, rest),
		};
		let (self_ty, trait_pattern) = match trait_text {
			Some(trait_text) => (
				parse_nested(self_text, options, "a type pattern after `for`")?,
				Some(parse_nested(trait_text, options, "a trait pattern after `impl`")?),
			),

			None => (parse_nested(self_text, options, "a type pattern after `impl`")?, None),
		};

		return Ok(PathPattern {
			anchor: Anchor::None,
			qualifier: Some((self_ty, trait_pattern)),
			segments: Vec::new(),
			arguments: None,
			import: false,
		});
	}

	if text.starts_with('<') {
		if nested {
			return Err("nested qualified patterns are not supported".to_owned());
		}

		let Some(length) = generic_arguments_len(text) else {
			return Err("expected `>` to close `<`".to_owned());
		};
		let inside = &text[1..length - 1];
		let after = text[length..].trim_start();

		if after.contains(['<', '>']) {
			return Err(ONLY_QUALIFIER_ARGUMENTS.to_owned());
		}

		let (self_ty, trait_pattern) = match split_word(inside, "as") {
			Some((self_text, trait_text)) => (
				parse_nested(self_text, options, "a type pattern after `<`")?,
				Some(parse_nested(trait_text, options, "a trait pattern after `as`")?),
			),

			None => (parse_nested(inside, options, "a type pattern after `<`")?, None),
		};
		let mut segments = Vec::new();

		if !after.is_empty() {
			let Some(after) = after.strip_prefix("::") else {
				return Err(format!("expected `::` after `>`, found `{after}`"));
			};

			for raw in after.split("::") {
				parse_segment(raw, options, &mut segments)?;
			}
		}

		return Ok(PathPattern {
			anchor: Anchor::None,
			qualifier: Some((self_ty, trait_pattern)),
			segments,
			arguments: None,
			import: false,
		});
	}

	// the type and trait of a qualifier may end with generic arguments
	let (text, arguments) = match text.find('<') {
		Some(start) if nested => match generic_arguments_len(&text[start..]) {
			Some(length) if start + length == text.len() => (text[..start].trim_end(), Some(normalize_arguments(&text[start..]))),
			Some(_) => return Err("generic arguments are only supported on the last segment of a type or trait".to_owned()),
			None => return Err("expected `>` to close the generic arguments".to_owned()),
		},

		Some(_) => return Err(ONLY_QUALIFIER_ARGUMENTS.to_owned()),
		None if text.contains('>') => return Err(format!("unexpected `>` in `{text}`")),
		None => (text, None),
	};

	let mut raw_segments = text.split("::").map(str::trim);
	let mut segments = Vec::new();
	let first = raw_segments.next().unwrap_or_default();

	let anchor = match first {
		"" => {
			let Some(crate_name) = raw_segments.next() else {
				return Err("the pattern is empty".to_owned());
			};

			if crate_name.is_empty() {
				return Err("expected a crate name pattern after `::`".to_owned());
			}

			parse_segment(crate_name, options, &mut segments)?;
			Anchor::Global
		}

		"crate" => Anchor::Crate,

		"crate**" => {
			segments.push(SegmentPattern::AnyDepth);
			Anchor::Crate
		}

		"self" | "self**" => return Err("`self` is not supported in patterns".to_owned()),
		"super" | "super**" => return Err("`super` is not supported in patterns".to_owned()),

		first => {
			parse_segment(first, options, &mut segments)?;
			Anchor::None
		}
	};

	for raw in raw_segments {
		parse_segment(raw, options, &mut segments)?;
	}

	if nested && segments.is_empty() {
		return Err(format!("expected a pattern with at least one segment, found `{text}`"));
	}

	Ok(PathPattern {
		anchor,
		qualifier: None,
		segments,
		arguments,
		import: false,
	})
}

/// Parses one `::`-separated segment, which may be `**`-fused (`foo**`, `**foo`), into `segments`.
fn parse_segment(raw: &str, options: MatchOptions, segments: &mut Vec<SegmentPattern>) -> Result<(), String> {
	let text = raw.trim();

	if text.is_empty() {
		return Err("expected a segment after `::`".to_owned());
	}

	if text.bytes().all(|byte| byte == b'*') {
		match text.len() {
			1 => segments.push(SegmentPattern::Ident(IdentPattern::any(options))),
			2 => segments.push(SegmentPattern::AnyDepth),

			_ => {
				return Err(format!(
					"`{text}` is not a segment pattern; use `*` for one segment or `**` for any number"
				));
			}
		}

		return Ok(());
	}

	let (leading, core) = match text.strip_prefix("**") {
		Some(core) => (true, core),
		None => (false, text),
	};
	let (core, trailing) = match core.strip_suffix("**") {
		Some(core) => (core, true),
		None => (core, false),
	};

	if (leading && core.starts_with('*')) || (trailing && core.ends_with('*')) || core.contains("**") {
		return Err(format!(
			"`{text}` is not a segment pattern; `**` must be a whole segment, or at the start or end of one"
		));
	}

	// ignoring case, `SELF` would match the same names as `self`
	match normalize(core, options).as_str() {
		"crate" | "self" | "super" => return Err(format!("`{core}` can only start a pattern")),
		"Self" => return Err("`Self` is not supported in patterns".to_owned()),
		"r#crate" | "r#self" | "r#super" | "r#Self" => return Err(format!("`{core}` is not a valid identifier")),
		_ => {}
	}

	if leading {
		segments.push(SegmentPattern::AnyDepth);
	}

	segments.push(SegmentPattern::Ident(parse_ident_pattern(core, options)?));

	if trailing {
		segments.push(SegmentPattern::AnyDepth);
	}

	Ok(())
}

/// Reduces a type or trait as written to its path: generic and parenthesized arguments, return types, references,
/// pointers, lifetimes, `dyn`/`impl`, extra bounds, and `!`/`?` modifiers are removed.
fn simplify_written_path(text: &str) -> String {
	let mut rest = text.trim();

	loop {
		let before = rest;

		for prefix in ["&", "*const ", "*mut ", "mut ", "dyn ", "impl ", "!", "?"] {
			if let Some(stripped) = rest.strip_prefix(prefix) {
				rest = stripped.trim_start();
			}
		}

		if let Some(lifetime) = rest.strip_prefix('\'') {
			rest = lifetime.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_').trim_start();
		}

		// `for<'a>` binders
		if let Some(binder) = rest.strip_prefix("for")
			&& let Some(generics) = binder.trim_start().strip_prefix('<')
			&& let Some(close) = generics.find('>')
		{
			rest = generics[close + 1..].trim_start();
		}

		if rest == before {
			break;
		}
	}

	let mut simplified = String::with_capacity(rest.len());
	let mut depth = 0_usize;
	let mut chars = rest.chars().peekable();

	while let Some(c) = chars.next() {
		match c {
			'-' if chars.peek() == Some(&'>') => {
				chars.next();

				if depth == 0 {
					break;
				}
			}

			'+' if depth == 0 => break,
			'<' | '(' => depth += 1,
			'>' | ')' => depth = depth.saturating_sub(1),
			_ if depth == 0 => simplified.push(c),
			_ => {}
		}
	}

	simplified.truncate(simplified.trim_end().len());
	simplified
}

/// Splits `text` at the first whitespace-separated occurrence of `word`, trimming both sides.
fn split_word<'a>(text: &'a str, word: &str) -> Option<(&'a str, &'a str)> {
	let mut offset = 0;

	for candidate in text.split_whitespace() {
		// words contain no whitespace, so the first occurrence after the previous word is this word
		let start = offset + text[offset..].find(candidate)?;

		if candidate == word {
			return Some((text[..start].trim(), text[start + word.len()..].trim()));
		}

		offset = start + candidate.len();
	}

	None
}

/// The text after a leading `word` that is followed by whitespace.
fn strip_word<'a>(text: &'a str, word: &str) -> Option<&'a str> {
	let rest = text.strip_prefix(word)?;

	rest.starts_with(char::is_whitespace).then_some(rest)
}

/// The segments of a simplified written path, without anchors. Types that are not paths (tuples, slices) become one
/// opaque segment of their original text.
fn written_segments<'a>(simplified: &'a str, original: &'a str) -> Vec<&'a str> {
	let mut segments: Vec<&str> = simplified.split("::").map(str::trim).filter(|segment| !segment.is_empty()).collect();
	let anchors = segments.iter().take_while(|segment| is_anchor_word(segment)).count();

	segments.drain(..anchors);

	if segments.is_empty() { vec![original.trim()] } else { segments }
}

#[cfg(test)]
mod tests {
	use super::*;

	const CASE: MatchOptions = MatchOptions { ignore_case: false };
	const NO_CASE: MatchOptions = MatchOptions { ignore_case: true };

	#[test]
	fn converts_exact_patterns_to_item_paths() {
		let path = |text: &str| pattern(text).to_item_path().map(|path| path.to_string());

		assert_eq!(path("crate"), Some("crate".to_owned()));
		assert_eq!(path("crate::a::Foo"), Some("crate::a::Foo".to_owned()));
		assert_eq!(path("::dep::Foo"), Some("::dep::Foo".to_owned()));
		assert_eq!(path("a::r#type"), Some("a::r#type".to_owned()));
		assert_eq!(path("<Foo as Display>::fmt"), Some("<Foo as Display>::fmt".to_owned()));
		assert_eq!(path("impl Foo"), Some("<Foo>".to_owned()));
		assert_eq!(path("a::*"), None);
		assert_eq!(path("a::**"), None);
		assert_eq!(path("Foo*"), None);
		assert_eq!(path("<Foo as *>::fmt"), None);
		assert_eq!(path("a::1x"), None);
		assert_eq!(PathPattern::parse("Foo", NO_CASE).unwrap().to_item_path(), None);
		assert_eq!(pattern("crate::a").to_item_path(), Some(ItemPath::parse("crate::a").unwrap()));
	}

	#[test]
	fn displays_ident_patterns() {
		for (text, displayed) in [
			("foo", "foo"),
			("foo*", "foo*"),
			("*foo", "*foo"),
			("*a*b*", "*a*b*"),
			("a*b", "a*b"),
			("*", "*"),
			("**", "*"),
			("r#type", "r#type"),
			("type", "r#type"),
			("r#union", "union"),
		] {
			let parsed = ident(text);

			assert_eq!(parsed.to_string(), displayed);
			assert_eq!(ident(displayed), parsed);
		}

		assert_eq!(IdentPattern::parse("Foo*", NO_CASE).unwrap().to_string(), "foo*");
		assert_eq!(IdentPattern::exact("", CASE).to_string(), "");
	}

	#[test]
	fn displays_path_patterns() {
		let cases = [
			("foo", "foo"),
			("crate", "crate"),
			("crate::a::*", "crate::a::*"),
			("::dep::**", "::dep::**"),
			("foo**", "foo::**"),
			("**foo", "**::foo"),
			("a::**::b", "a::**::b"),
			("<Foo as Display>::fmt", "<Foo as Display>::fmt"),
			("impl Display for crate::Foo", "<crate::Foo as Display>"),
			("impl *", "<*>"),
			("<* as ::std::fmt::*>::*", "<* as ::std::fmt::*>::*"),
			("r#type::r#fn", "r#type::r#fn"),
			("use crate::a::*", "use crate::a::*"),
			("use  Foo*", "use Foo*"),
			("use **::_", "use **::_"),
		];

		for (text, displayed) in cases {
			let parsed = pattern(text);

			assert_eq!(parsed.to_string(), displayed, "display of `{text}`");
			assert_eq!(pattern(displayed), parsed, "`{displayed}` does not round-trip");
		}
	}

	#[test]
	fn generic_arguments_select_impl_blocks() {
		let wrapper = |arguments: Option<&str>, trait_text: &str| CanonicalPath {
			self_ty_arguments: arguments.map(str::to_owned),
			..trait_item(&["my_crate", "Wrapper"], trait_text, "from")
		};
		let pattern = |text: &str| pattern(text);

		assert!(pattern("<Wrapper as From<u8>>::from").matches(&wrapper(None, "From<u8>"), true));
		assert!(!pattern("<Wrapper as From<u8>>::from").matches(&wrapper(None, "From<u16>"), true));
		assert!(!pattern("<Wrapper as From<u8>>::from").matches(&wrapper(None, "From"), true));
		assert!(pattern("<Wrapper as From< u8 >>::*").matches(&wrapper(None, "From<u8>"), true));
		assert!(pattern("<Wrapper as From>::from").matches(&wrapper(None, "From<u16>"), true));

		// of the type
		assert!(pattern("<Wrapper<T, bool> as *>::from").matches(&wrapper(Some("<T, bool>"), "From<u8>"), true));
		assert!(!pattern("<Wrapper<T, bool> as *>::from").matches(&wrapper(Some("<T, u8>"), "From<u8>"), true));
		assert!(!pattern("<Wrapper<T, bool> as *>::from").matches(&wrapper(None, "From<u8>"), true));
		assert!(pattern("impl From<u8> for Wrapper<T,bool>").matches(
			&CanonicalPath {
				is_impl: true,
				name: None,
				..wrapper(Some("<T, bool>"), "From<u8>")
			},
			true
		));
		assert!(pattern("<*<u8> as *>").matches(
			&CanonicalPath {
				is_impl: true,
				name: None,
				..unresolved(&["my_crate"], "Vec<u8>", Some("X"), None)
			},
			true
		));

		assert_eq!(
			pattern("<Wrapper<T,  bool> as From<u8>>::from").to_string(),
			"<Wrapper<T,bool> as From<u8>>::from"
		);
		assert_eq!(
			pattern("<crate::Wrapper<u8> as From<u16>>::from").to_item_path().unwrap(),
			ItemPath::parse("<crate::Wrapper<u8> as From<u16>>::from").unwrap()
		);
	}

	fn ident(pattern: &str) -> IdentPattern {
		IdentPattern::parse(pattern, CASE).unwrap_or_else(|error| panic!("{error}"))
	}

	#[test]
	fn ident_pattern_shapes() {
		assert!(ident("foo").is_exact());
		assert!(!ident("foo*").is_exact());
		assert!(ident("*").is_any());
		assert!(ident("**").is_any());
		assert!(!ident("*a*").is_any());
		assert_eq!(ident("*foo*bar*").parts, ["foo", "bar"]);
		assert_eq!(IdentPattern::parse("  Foo*  ", NO_CASE).unwrap().parts, ["foo"]);
		assert!(IdentPattern::any(CASE).is_any());
	}

	#[test]
	fn ident_patterns_ignore_case() {
		let pattern = IdentPattern::parse("*Error", NO_CASE).unwrap();

		assert!(pattern.matches("IoError"));
		assert!(pattern.matches("ioerror"));
		assert!(pattern.matches("IOERROR"));
		assert!(!pattern.matches("Errors"));
		assert!(pattern.ignores_case());
		assert!(!ident("*Error").matches("ioerror"));

		let unicode = IdentPattern::parse("STRASSE*", NO_CASE).unwrap();

		assert!(unicode.matches("strasse_x"));

		let accented = IdentPattern::parse("Écl*", NO_CASE).unwrap();

		assert!(accented.matches("éclair"));
		assert!(accented.matches("ÉCLAIR"));
		assert!(IdentPattern::exact("MyType", NO_CASE).matches("MYTYPE"));
	}

	#[test]
	fn ident_patterns_match() {
		let cases: &[(&str, &[&str], &[&str])] = &[
			("foo", &["foo", "r#foo"], &["Foo", "fo", "foox", "xfoo", ""]),
			("r#type", &["type", "r#type"], &["types"]),
			("foo*", &["foo", "foobar", "r#foo_x"], &["xfoo", "fo"]),
			("*foo", &["foo", "xfoo", "r#afoo"], &["foox", "oo"]),
			("*foo*", &["foo", "afoob", "xxfoo", "foo_"], &["fo_o", "FOO"]),
			("foo*bar", &["foobar", "foo_bar", "fooxbar"], &["fooba", "foobarx", "xfoobar", "foobr"]),
			("foo*foo", &["foofoo", "foo_foo"], &["foo", "foofo"]),
			("*foo*bar*", &["foobar", "xfooybarz", "barfoobar"], &["barfoo", "fobar"]),
			("*a*b*c*", &["abc", "xaybzc", "aabbcc"], &["acb", "cba", "ab"]),
			("a*b*c", &["abc", "a_b_c", "abbc"], &["abcd", "bc", "ac"]),
			("*", &["", "x", "anything"], &[]),
			("**", &["x"], &[]),
			("a**b", &["ab", "axb"], &["ba"]),
			("é*", &["école"], &["ecole"]),
		];

		for &(text, matching, not_matching) in cases {
			let pattern = ident(text);

			for candidate in matching {
				assert!(pattern.matches(candidate), "`{text}` should match `{candidate}`");
			}

			for candidate in not_matching {
				assert!(!pattern.matches(candidate), "`{text}` should not match `{candidate}`");
			}
		}
	}

	fn impl_block(owner: &[&str], trait_text: Option<&str>) -> CanonicalPath {
		CanonicalPath {
			segments: owner.iter().copied().map(SmolStr::from).collect(),
			impl_trait: trait_text.map(str::to_owned),
			self_ty_arguments: None,
			unresolved_self_ty: None,
			is_impl: true,
			is_import: false,
			name: None,
		}
	}

	#[test]
	fn invalid_ident_patterns() {
		for text in ["", "r#", "foo bar", "a-b", "a::b", "a.b", "a😀", "#"] {
			assert!(IdentPattern::parse(text, CASE).is_err(), "`{text}`");
		}

		assert_eq!(
			IdentPattern::parse("foo-bar", CASE).unwrap_err().message,
			"'-' is not allowed in identifier patterns (only identifier characters and `*`)"
		);
	}

	fn item(segments: &[&str], name: &str) -> CanonicalPath {
		CanonicalPath {
			segments: segments.iter().copied().map(SmolStr::from).collect(),
			impl_trait: None,
			self_ty_arguments: None,
			unresolved_self_ty: None,
			is_impl: false,
			is_import: false,
			name: Some(name.into()),
		}
	}

	#[test]
	fn literal_ident_patterns() {
		assert!(IdentPattern::exact("foo", CASE).matches("foo"));
		assert!(!IdentPattern::exact("foo", CASE).matches("foox"));
		assert!(IdentPattern::exact("r#type", CASE).matches("type"));
		assert!(IdentPattern::exact("", CASE).matches(""));
		assert!(!IdentPattern::exact("", CASE).matches("a"));
		assert!(IdentPattern::contains("oo", CASE).matches("foo"));
		assert!(IdentPattern::contains("", CASE).matches("anything"));
		assert!(IdentPattern::contains("a*b", CASE).matches("xa*by"));
		assert!(!IdentPattern::contains("a*b", CASE).matches("xaby"));
		assert!(IdentPattern::starts_with("fo", CASE).matches("foo"));
		assert!(!IdentPattern::starts_with("oo", CASE).matches("foo"));
		assert!(IdentPattern::starts_with("", CASE).matches("foo"));
		assert!(IdentPattern::ends_with("oo", CASE).matches("foo"));
		assert!(!IdentPattern::ends_with("fo", CASE).matches("foo"));
		assert!(IdentPattern::ends_with("", CASE).matches("foo"));
		assert!(IdentPattern::starts_with("Get", NO_CASE).matches("get_x"));
	}

	#[test]
	fn many_any_depth_segments_are_fast() {
		let text = std::iter::repeat_n("**", 40).chain(["x"]).collect::<Vec<_>>().join("::");
		let segments = vec!["a"; 60];
		let started = std::time::Instant::now();

		assert!(!pattern(&text).matches_segments(&segments));
		assert!(started.elapsed() < std::time::Duration::from_secs(1));
	}

	#[test]
	fn matches_canonical_paths() {
		let foo = item(&["my_crate", "a"], "Foo");
		let root = CanonicalPath {
			segments: Vec::new(),
			impl_trait: None,
			self_ty_arguments: None,
			unresolved_self_ty: None,
			is_impl: false,
			is_import: false,
			name: Some("my_crate".into()),
		};

		assert!(pattern("Foo").matches(&foo, true));
		assert!(pattern("a::Foo").matches(&foo, true));
		assert!(pattern("my_crate::a::Foo").matches(&foo, false));
		assert!(pattern("F*").matches(&foo, true));
		assert!(!pattern("Bar").matches(&foo, true));
		assert!(!pattern("a").matches(&foo, true));

		// `crate::` only matches selected crates, from their root
		assert!(pattern("crate::a::Foo").matches(&foo, true));
		assert!(!pattern("crate::a::Foo").matches(&foo, false));
		assert!(!pattern("crate::Foo").matches(&foo, true));
		assert!(pattern("crate::**").matches(&foo, true));
		assert!(pattern("crate::*::Foo").matches(&foo, true));
		assert!(pattern("crate").matches(&root, true));
		assert!(!pattern("crate").matches(&foo, true));
		assert!(!pattern("crate::**").matches(&root, true));

		// `::name` anchors at the crate name
		assert!(pattern("::my_crate::a::Foo").matches(&foo, false));
		assert!(pattern("::my_*::**").matches(&foo, false));
		assert!(!pattern("::a::Foo").matches(&foo, true));
		assert!(pattern("::my_crate").matches(&root, false));
		assert!(pattern("my_crate").matches(&root, false));

		// case
		assert!(PathPattern::parse("A::FOO", NO_CASE).unwrap().matches(&foo, true));
		assert!(!pattern("A::FOO").matches(&foo, true));

		// raw identifiers compare unraw'd
		let raw = item(&["my_crate", "type"], "fn");

		assert!(pattern("r#type::r#fn").matches(&raw, true));
		assert!(pattern("type::fn").matches(&raw, true));
	}

	#[test]
	fn matches_segments_with_wildcards() {
		let cases: &[(&str, &[&str], bool)] = &[
			// unanchored: suffix semantics
			("Blam", &["c", "a", "Blam"], true),
			("Blam", &["c", "a", "Blam", "x"], false),
			("a::Blam", &["c", "a", "Blam"], true),
			("b::Blam", &["c", "a", "Blam"], false),
			("foo::*", &["c", "foo", "x"], true),
			("foo::*", &["c", "foo", "x", "y"], false),
			("foo::*", &["c", "foo"], false),
			("foo::**", &["c", "foo", "x"], true),
			("foo::**", &["c", "foo", "x", "y"], true),
			("foo::**", &["c", "foo"], false),
			("**::Blam", &["c", "Blam"], true),
			("**::Blam", &["c", "a", "b", "Blam"], true),
			("a::**::b", &["c", "a", "b"], true),
			("a::**::b", &["c", "a", "x", "y", "b"], true),
			("a::**::b", &["c", "a", "x", "b", "z"], false),
			("**", &["c"], true),
			("**", &["c", "x"], true),
			("*", &["c"], true),
			("**::**", &["c"], true),
			("a::**::**", &["c", "a", "x"], true),
			("a::**::**", &["c", "a"], false),
			("*::*::*", &["c", "a"], false),
			("*::*::*", &["c", "a", "b"], true),
		];

		for &(text, segments, expected) in cases {
			assert_eq!(pattern(text).matches_segments(segments), expected, "`{text}` against {segments:?}");
		}

		// anchored (the anchor's prefix already stripped by the caller)
		let crate_pattern = pattern("crate::a::**");

		assert!(crate_pattern.matches_segments(&["a", "b"]));
		assert!(!crate_pattern.matches_segments(&["x", "a", "b"]));
		assert!(!crate_pattern.matches_segments(&["a"]));
		assert!(pattern("crate").matches_segments(&[]));
		assert!(!pattern("crate").matches_segments(&["a"]));
	}

	#[test]
	fn parses_path_patterns() {
		let p = pattern("crate::a::*");

		assert_eq!(p.anchor, Anchor::Crate);
		assert_eq!(p.segments.len(), 2);

		assert_eq!(pattern("crate").segments, []);
		assert_eq!(pattern("crate").anchor, Anchor::Crate);
		assert_eq!(pattern("::dep::x").anchor, Anchor::Global);
		assert_eq!(pattern("foo").anchor, Anchor::None);
		assert_eq!(pattern(" a :: b ").segments, pattern("a::b").segments);
		assert_eq!(pattern("foo**").segments, pattern("foo::**").segments);
		assert_eq!(pattern("**foo").segments, pattern("**::foo").segments);
		assert_eq!(pattern("**foo**").segments, pattern("**::foo::**").segments);
		assert_eq!(pattern("crate**"), pattern("crate::**"));
		assert_eq!(pattern("a::r#type"), pattern("a::type"));
		assert!(matches!(pattern("a::**").segments[1], SegmentPattern::AnyDepth));
	}

	#[test]
	fn parses_qualified_patterns() {
		let p = pattern("<Foo as Display>::fmt");
		let (self_ty, trait_pattern) = p.qualifier.clone().unwrap();

		assert_eq!(*self_ty, pattern("Foo"));
		assert_eq!(trait_pattern.as_deref(), Some(&pattern("Display")));
		assert_eq!(p.segments, pattern("fmt").segments);
		assert!(p.is_qualified());

		assert_eq!(pattern(" < Foo  as  Display > :: fmt "), p);
		assert_eq!(pattern("impl Display for Foo"), pattern("<Foo as Display>"));
		assert_eq!(pattern("impl Foo"), pattern("<Foo>"));
		assert_eq!(pattern("impl * for crate::**"), pattern("<crate::** as *>"));
		assert_eq!(pattern("<*>::new").qualifier.unwrap().1, None);
		assert!(!pattern("Foo::new").is_qualified());
	}

	fn pattern(text: &str) -> PathPattern {
		PathPattern::parse(text, CASE).unwrap_or_else(|error| panic!("{error}"))
	}

	fn pattern_error(text: &str) -> String {
		match PathPattern::parse(text, CASE) {
			Ok(pattern) => panic!("`{text}` parsed as {pattern:?}"),

			Err(error) => {
				assert_eq!(error.text, text);
				error.message
			}
		}
	}

	#[test]
	fn qualified_patterns_match_impl_items() {
		let display_fmt = trait_item(&["c", "a", "Foo"], "Display", "fmt");
		let debug_fmt = trait_item(&["c", "a", "Foo"], "fmt::Debug", "fmt");
		let generic = trait_item(&["c", "a", "Foo"], "From<u8>", "from");
		let new = item(&["c", "a", "Foo"], "new");

		assert!(pattern("<Foo as Display>::fmt").matches(&display_fmt, true));
		assert!(!pattern("<Foo as Display>::fmt").matches(&debug_fmt, true));
		assert!(pattern("<Foo as Debug>::fmt").matches(&debug_fmt, true));
		assert!(pattern("<Foo as fmt::Debug>::fmt").matches(&debug_fmt, true));
		assert!(pattern("<Foo as std::fmt::Debug>::fmt").matches(&debug_fmt, true));
		assert!(pattern("<Foo as ::core::fmt::Debug>::*").matches(&debug_fmt, true));
		assert!(!pattern("<Foo as io::Debug>::fmt").matches(&debug_fmt, true));
		assert!(pattern("<Foo as *>::fmt").matches(&display_fmt, true));
		assert!(pattern("<* as *>::*").matches(&display_fmt, true));
		assert!(pattern("<Foo as From>::from").matches(&generic, true));
		assert!(!pattern("<Foo as Display>::new").matches(&display_fmt, true));
		assert!(!pattern("<Bar as Display>::fmt").matches(&display_fmt, true));

		// owners honor anchors
		assert!(pattern("<crate::a::Foo as Display>::fmt").matches(&display_fmt, true));
		assert!(!pattern("<crate::a::Foo as Display>::fmt").matches(&display_fmt, false));
		assert!(!pattern("<crate::Foo as Display>::fmt").matches(&display_fmt, true));
		assert!(pattern("<::c::a::Foo as Display>::fmt").matches(&display_fmt, false));
		assert!(pattern("<a::Foo as Display>::fmt").matches(&display_fmt, true));

		// inherent qualifiers do not match trait impl items, and vice versa
		assert!(pattern("<Foo>::new").matches(&new, true));
		assert!(!pattern("<Foo>::fmt").matches(&display_fmt, true));
		assert!(!pattern("<Foo as *>::new").matches(&new, true));

		// qualified patterns without segments match impl blocks only
		assert!(!pattern("<Foo as Display>").matches(&display_fmt, true));
		assert!(pattern("<Foo as Display>").matches(&impl_block(&["c", "a", "Foo"], Some("Display")), true));
		assert!(pattern("impl Display for Foo").matches(&impl_block(&["c", "a", "Foo"], Some("std::fmt::Display")), true));
		assert!(!pattern("<Foo as Display>").matches(&impl_block(&["c", "a", "Foo"], None), true));
		assert!(pattern("<Foo>").matches(&impl_block(&["c", "a", "Foo"], None), true));
		assert!(pattern("impl Foo").matches(&impl_block(&["c", "a", "Foo"], None), true));
		assert!(!pattern("<Foo>::*").matches(&impl_block(&["c", "a", "Foo"], None), true));
		assert!(!pattern("<Foo as Display>::fmt::x").matches(&display_fmt, true));
		assert!(pattern("<Foo as Display>::**").matches(&display_fmt, true));
	}

	#[test]
	fn qualified_patterns_match_unresolved_self_types() {
		let method = unresolved(&["c", "m"], "Vec<u8>", Some("Tr"), Some("required"));
		let block = unresolved(&["c", "m"], "Vec<u8>", Some("Tr"), None);
		let reference = unresolved(&["c", "m"], "&'a mut [u8]", Some("Tr"), Some("x"));
		let str_ref = unresolved(&["c", "m"], "&'static str", None, Some("len2"));
		let boxed = unresolved(&["c", "m"], "Box<dyn Fn(u8) -> u8 + Send>", Some("for<'a> Callback<'a>"), Some("call"));

		assert!(pattern("<Vec as Tr>::required").matches(&method, true));
		assert!(pattern("<std::vec::Vec as Tr>::required").matches(&method, true));
		assert!(pattern("<* as Tr>::*").matches(&method, true));
		assert!(!pattern("<Vec as Tr>").matches(&method, true));
		assert!(pattern("<Vec as Tr>").matches(&block, true));
		assert!(pattern("impl Tr for Vec").matches(&block, true));
		assert!(!pattern("<Vec>").matches(&block, true));
		assert!(!pattern("<String as Tr>::required").matches(&method, true));
		assert!(pattern("<*u8* as Tr>::x").matches(&reference, true));
		assert!(pattern("<str>::len2").matches(&str_ref, true));
		assert!(pattern("<Box as Callback>::call").matches(&boxed, true));

		// unqualified patterns see the `<impl ..>` pseudo-segment
		assert!(pattern("m::*::required").matches(&method, true));
		assert!(pattern("crate::m::**").matches(&method, true));
		assert!(!pattern("m::required").matches(&method, true));
		assert!(pattern("*Vec*::required").matches(&method, true));
	}

	/// Random inputs never panic, whatever parses displays as text that parses to the same pattern, and matching
	/// random paths never panics.
	#[test]
	fn random_patterns_round_trip() {
		const PIECES: &[&str] = &[
			"::", ":", "<", ">", " as ", " for ", "impl ", "use ", "crate", "self", "super", "Self", "r#", "a", "b", "type", "é", "_", "1", " ", "*",
			"**", "***", "😀", "-", "#", "É",
		];

		// xorshift, for reproducible inputs without dependencies
		let mut state = 0x9e37_79b9_7f4a_7c15_u64;
		let mut random = |bound: usize| {
			state ^= state << 13;
			state ^= state >> 7;
			state ^= state << 17;
			(state % bound as u64) as usize
		};
		let paths = [
			item(&["c", "a"], "b"),
			item(&["c", "a", "type"], "É"),
			trait_item(&["c", "a"], "b::B<u8>", "a"),
			impl_block(&["c", "b"], None),
			unresolved(&["c"], "&'a [a]", Some("a"), Some("b")),
			unresolved(&["c"], "(a, b)", None, None),
		];
		let mut parsed = 0;

		for _ in 0..20_000 {
			let length = random(8);
			let text: String = (0..length).map(|_| PIECES[random(PIECES.len())]).collect();
			let options = MatchOptions { ignore_case: random(2) == 0 };

			let _ = IdentPattern::parse(&text, options).map(|pattern| pattern.matches(&text));

			let Ok(pattern) = PathPattern::parse(&text, options) else {
				continue;
			};
			let displayed = pattern.to_string();

			parsed += 1;
			assert_eq!(
				PathPattern::parse(&displayed, options).as_ref(),
				Ok(&pattern),
				"`{text}` displayed as `{displayed}`"
			);

			for path in &paths {
				let _ = pattern.matches(path, random(2) == 0);
			}

			let _ = pattern.to_item_path();
		}

		assert!(parsed > 500, "only {parsed} random patterns parsed");
	}

	#[test]
	fn rejects_invalid_path_patterns() {
		let cases = [
			("", "the pattern is empty"),
			("::", "expected a crate name pattern after `::`"),
			("crate::", "expected a segment after `::`"),
			("a::::b", "expected a segment after `::`"),
			("a::", "expected a segment after `::`"),
			("self::a", "`self` is not supported in patterns"),
			("super::a", "`super` is not supported in patterns"),
			("a::crate", "`crate` can only start a pattern"),
			("::crate::a", "`crate` can only start a pattern"),
			("a::self", "`self` can only start a pattern"),
			("Self::new", "`Self` is not supported in patterns"),
			("***", "`***` is not a segment pattern; use `*` for one segment or `**` for any number"),
			("a::***", "`***` is not a segment pattern; use `*` for one segment or `**` for any number"),
			(
				"a**b",
				"`a**b` is not a segment pattern; `**` must be a whole segment, or at the start or end of one",
			),
			(
				"***a",
				"`***a` is not a segment pattern; `**` must be a whole segment, or at the start or end of one",
			),
			(
				"a***",
				"`a***` is not a segment pattern; `**` must be a whole segment, or at the start or end of one",
			),
			("Vec<u8>", ONLY_QUALIFIER_ARGUMENTS),
			("a>", "unexpected `>` in `a>`"),
			(
				"<Vec<u8>::X as Y>",
				"generic arguments are only supported on the last segment of a type or trait",
			),
			("<Vec<u8 as Y>", "expected `>` to close `<`"),
			("<Foo", "expected `>` to close `<`"),
			("<>", "expected a type pattern after `<`"),
			("<Foo as>", "expected a trait pattern after `as`"),
			("<Foo>fmt", "expected `::` after `>`, found `fmt`"),
			("<Foo>::from<T>", ONLY_QUALIFIER_ARGUMENTS),
			("r#self::a", "`r#self` is not a valid identifier"),
			("a::r#crate", "`r#crate` is not a valid identifier"),
			("**r#Self", "`r#Self` is not a valid identifier"),
			("<Foo>::", "expected a segment after `::`"),
			("<crate as X>", "expected a pattern with at least one segment, found `crate`"),
			("<impl X as Y>", "`impl` cannot appear inside of a qualifier"),
			("impl for Foo", "expected a trait pattern after `impl`"),
			("impl Foo for", "expected a type pattern after `for`"),
			("a b", "' ' is not allowed in identifier patterns (only identifier characters and `*`)"),
			("a:b", "':' is not allowed in identifier patterns (only identifier characters and `*`)"),
		];

		for (text, message) in cases {
			assert_eq!(pattern_error(text), message, "`{text}`");
		}

		let error = |text: &str| PathPattern::parse(text, NO_CASE).unwrap_err().message;

		assert_eq!(error("a::SELF"), "`SELF` can only start a pattern");
		assert_eq!(error("a::R#Crate"), "`R#Crate` is not a valid identifier");
		assert!(PathPattern::parse("a::SELF", CASE).is_ok());
	}

	#[test]
	fn simplifies_written_paths() {
		let cases = [
			("Vec<u8>", "Vec"),
			("std::vec::Vec<Option<u8>>", "std::vec::Vec"),
			("&'a mut [u8]", "[u8]"),
			("&'static str", "str"),
			("*const T", "T"),
			("dyn Fn(u8) -> Vec<u8> + Send", "Fn"),
			("Box<dyn Fn() -> u8>", "Box"),
			("for<'a> Callback<'a>", "Callback"),
			("!Send", "Send"),
			("fmt :: Display", "fmt :: Display"),
			("(A, B)", ""),
		];

		for (text, simplified) in cases {
			assert_eq!(simplify_written_path(text), simplified, "`{text}`");
		}

		assert_eq!(written_segments("fmt :: Display", "fmt :: Display"), ["fmt", "Display"]);
		assert_eq!(written_segments("crate::super::a::B", ""), ["a", "B"]);
		assert_eq!(written_segments("", "(A, B)"), ["(A, B)"]);
	}

	fn trait_item(owner: &[&str], trait_text: &str, name: &str) -> CanonicalPath {
		CanonicalPath {
			impl_trait: Some(trait_text.to_owned()),
			..item(owner, name)
		}
	}

	#[test]
	fn unqualified_patterns_match_impl_items_but_not_impl_blocks() {
		let display_fmt = trait_item(&["c", "a", "Foo"], "Display", "fmt");
		let debug_fmt = trait_item(&["c", "a", "Foo"], "fmt::Debug", "fmt");
		let new = item(&["c", "a", "Foo"], "new");

		for path in [&display_fmt, &debug_fmt] {
			assert!(pattern("Foo::fmt").matches(path, true));
			assert!(pattern("fmt").matches(path, true));
			assert!(pattern("crate::a::Foo::*").matches(path, true));
		}

		assert!(pattern("Foo::new").matches(&new, true));
		assert!(!pattern("Foo").matches(&impl_block(&["c", "a", "Foo"], Some("Display")), true));
		assert!(!pattern("**").matches(&impl_block(&["c", "a", "Foo"], None), true));
		assert!(!pattern("*").matches(&unresolved(&["c", "m"], "Vec<u8>", None, None), true));
	}

	fn unresolved(module: &[&str], self_ty: &str, trait_text: Option<&str>, name: Option<&str>) -> CanonicalPath {
		CanonicalPath {
			segments: module.iter().copied().map(SmolStr::from).collect(),
			impl_trait: trait_text.map(str::to_owned),
			self_ty_arguments: None,
			unresolved_self_ty: Some(self_ty.to_owned()),
			is_impl: name.is_none(),
			is_import: false,
			name: name.map(SmolStr::from),
		}
	}

	#[test]
	fn use_patterns_match_imports_only() {
		let import = |segments: &[&str], name: &str| CanonicalPath {
			segments: segments.iter().copied().map(SmolStr::new).collect(),
			impl_trait: None,
			self_ty_arguments: None,
			unresolved_self_ty: None,
			is_impl: false,
			name: Some(name.into()),
			is_import: true,
		};
		let item = |segments: &[&str], name: &str| CanonicalPath {
			is_import: false,
			..import(segments, name)
		};

		assert!(pattern("use Foo").matches(&import(&["c", "a"], "Foo"), true));
		assert!(!pattern("use Foo").matches(&item(&["c", "a"], "Foo"), true));
		assert!(pattern("Foo").matches(&import(&["c", "a"], "Foo"), true));
		assert!(pattern("use crate::a::*").matches(&import(&["c", "a"], "*"), true));
		assert!(pattern("use crate::a::*").matches(&import(&["c", "a"], "Foo"), true));
		assert!(!pattern("use crate::a::*").matches(&import(&["c", "a", "b"], "Foo"), true));

		// exact `use` patterns are `use` paths: `_` names underscore imports, `*` stays a wildcard
		assert!(pattern("use crate::a::Foo").to_item_path().unwrap().import);
		assert_eq!(pattern("use crate::a::_").to_item_path().unwrap().to_string(), "use crate::a::_");
		assert_eq!(pattern("use crate::a::*").to_item_path(), None);
		assert_eq!(pattern("crate::a::_").to_item_path(), None);

		for (text, message) in [
			("use", "expected a pattern after `use` (quote the whole pattern, `use` included)"),
			("use <A as B>", "`use` patterns match the imports of modules, and cannot be qualified"),
			("use use a", "`use` patterns match the imports of modules, and cannot be qualified"),
			("use crate", "expected a pattern of the names imports bind after `use crate`"),
			("impl use a", "`use` cannot appear inside of a qualifier"),
			("impl use", "expected a pattern after `use` (quote the whole pattern, `use` included)"),
		] {
			match PathPattern::parse(text, MatchOptions::default()) {
				Ok(parsed) => panic!("`{text}` parsed as {parsed:?}"),
				Err(error) => assert_eq!(error.message, message, "{text}"),
			}
		}
	}
}
