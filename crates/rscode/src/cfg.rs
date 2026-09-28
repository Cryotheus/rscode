//! `cfg` predicates and their (three-valued) evaluation.

use proc_macro2::Delimiter;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use serde::Serialize;
use smol_str::SmolStr;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::process::Command;

/// Names and keys whose complete set of values rustc reports (or that are well-known to be unset) when
/// configuration comes from `rustc --print cfg`.
const WELL_KNOWN: &[&str] = &[
	"clippy",
	"contract_checks",
	"debug_assertions",
	"doc",
	"doctest",
	"docsrs",
	"emscripten_wasm_eh",
	"fmt_debug",
	"miri",
	"overflow_checks",
	"panic",
	"proc_macro",
	"relocation_model",
	"rustfmt",
	"sanitize",
	"sanitizer_cfi_generalize_pointers",
	"sanitizer_cfi_normalize_integers",
	"target_abi",
	"target_arch",
	"target_endian",
	"target_env",
	"target_family",
	"target_feature",
	"target_has_atomic",
	"target_has_atomic_equal_alignment",
	"target_has_atomic_load_store",
	"target_os",
	"target_pointer_width",
	"target_thread_local",
	"target_vendor",
	"test",
	"ub_checks",
	"unix",
	"windows",
];

/// A `cfg` predicate, as found in `#[cfg(...)]` and `#[cfg_attr(...)]`.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum CfgExpr {
	/// `true` or `false`.
	Bool(bool),

	/// `unix`, `test`, ...
	Name(SmolStr),

	/// `feature = "foo"`, `target_os = "linux"`, ...
	KeyValue(SmolStr, SmolStr),

	/// `all(..)`: every predicate holds (true when empty).
	All(Vec<CfgExpr>),

	/// `any(..)`: some predicate holds (false when empty).
	Any(Vec<CfgExpr>),

	/// `not(..)`
	Not(Box<CfgExpr>),

	/// A predicate which is not understood (e.g. `version("1.80")`, `accessible(...)`), kept verbatim.
	/// Always evaluates to [`Tristate::Unknown`].
	Other(String),
}

impl CfgExpr {
	/// Parses the contents of a `cfg(...)`, e.g. `all(unix, feature = "foo")`.
	pub fn parse(text: &str) -> Result<Self, CfgParseError> {
		let tokens: TokenStream = text.parse().map_err(|error: proc_macro2::LexError| CfgParseError {
			text: text.to_owned(),
			message: error.to_string(),
		})?;

		Self::from_tokens(tokens)
	}

	/// Parses the tokens inside of `cfg(...)`.
	pub fn from_tokens(tokens: TokenStream) -> Result<Self, CfgParseError> {
		let tokens: Vec<TokenTree> = tokens.into_iter().collect();
		let mut predicates = parse_list(&tokens)?;

		match predicates.len() {
			1 => Ok(predicates.pop().unwrap()),
			0 => Err(CfgParseError::new(&tokens, "expected a cfg predicate")),
			_ => Err(CfgParseError::new(&tokens, "expected a single cfg predicate")),
		}
	}

	/// Parses the tokens inside of `cfg_attr(...)` into the predicate and the attributes' token streams.
	pub fn parse_cfg_attr(tokens: TokenStream) -> Result<(Self, Vec<TokenStream>), CfgParseError> {
		let tokens: Vec<TokenTree> = tokens.into_iter().collect();
		let mut parts = split_commas(&tokens).into_iter();
		let predicate = parts.next().filter(|part| !part.is_empty()).ok_or_else(|| CfgParseError::new(&tokens, "expected a cfg predicate"))?;
		let predicate = parse_predicate(predicate)?;
		let attrs = parts.filter(|part| !part.is_empty()).map(|part| part.iter().cloned().collect()).collect();

		Ok((predicate, attrs))
	}

	/// Combines predicates with `all(...)`, flattening nested `all`s and removing duplicates.
	/// Returns `None` for an empty input.
	pub fn all(exprs: impl IntoIterator<Item = CfgExpr>) -> Option<CfgExpr> {
		let mut flat = Vec::new();

		for expr in exprs {
			match expr {
				CfgExpr::All(inner) => flat.extend(inner),
				other => flat.push(other),
			}
		}

		let mut seen = BTreeSet::new();

		flat.retain(|expr| seen.insert(expr.clone()));

		match flat.len() {
			0 => None,
			1 => flat.pop(),
			_ => Some(CfgExpr::All(flat)),
		}
	}

	/// Evaluates the predicate.
	pub fn eval(&self, context: &CfgContext) -> Tristate {
		context.eval(self)
	}

	/// Names of every feature mentioned by `feature = "..."`.
	pub fn features(&self) -> BTreeSet<&str> {
		let mut features = BTreeSet::new();

		self.collect_features(&mut features);
		features
	}

	fn collect_features<'a>(&'a self, features: &mut BTreeSet<&'a str>) {
		match self {
			Self::KeyValue(key, value) if key == "feature" => {
				features.insert(value.as_str());
			}

			Self::All(exprs) | Self::Any(exprs) => exprs.iter().for_each(|expr| expr.collect_features(features)),
			Self::Not(expr) => expr.collect_features(features),
			_ => {}
		}
	}
}

/// Formats as Rust syntax, e.g. `all(feature = "foo", not(unix))`.
impl std::fmt::Display for CfgExpr {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		fn list(f: &mut std::fmt::Formatter<'_>, name: &str, exprs: &[CfgExpr]) -> std::fmt::Result {
			write!(f, "{name}(")?;

			for (index, expr) in exprs.iter().enumerate() {
				if index > 0 {
					f.write_str(", ")?;
				}

				write!(f, "{expr}")?;
			}

			f.write_str(")")
		}

		match self {
			Self::Bool(value) => write!(f, "{value}"),
			Self::Name(name) => f.write_str(name),
			Self::KeyValue(key, value) => write!(f, "{key} = {value:?}"),
			Self::All(exprs) => list(f, "all", exprs),
			Self::Any(exprs) => list(f, "any", exprs),
			Self::Not(expr) => write!(f, "not({expr})"),
			Self::Other(text) => f.write_str(text),
		}
	}
}

impl Serialize for CfgExpr {
	fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.collect_str(self)
	}
}

fn split_commas(tokens: &[TokenTree]) -> Vec<&[TokenTree]> {
	let mut parts = Vec::new();
	let mut start = 0;

	for (index, token) in tokens.iter().enumerate() {
		if let TokenTree::Punct(punct) = token
			&& punct.as_char() == ','
		{
			parts.push(&tokens[start..index]);
			start = index + 1;
		}
	}

	parts.push(&tokens[start..]);
	parts
}

/// Parses a comma-separated list of predicates (a trailing comma is allowed).
fn parse_list(tokens: &[TokenTree]) -> Result<Vec<CfgExpr>, CfgParseError> {
	let mut parts = split_commas(tokens);

	if parts.last().is_some_and(|part| part.is_empty()) {
		parts.pop();
	}

	parts.into_iter().map(parse_predicate).collect()
}

fn parse_predicate(tokens: &[TokenTree]) -> Result<CfgExpr, CfgParseError> {
	let Some(TokenTree::Ident(ident)) = tokens.first() else {
		return Err(CfgParseError::new(tokens, "expected an identifier"));
	};

	let text = ident.to_string();
	let raw = text.starts_with("r#");
	let name = text.strip_prefix("r#").unwrap_or(&text);

	match &tokens[1..] {
		[] => Ok(match name {
			"true" if !raw => CfgExpr::Bool(true),
			"false" if !raw => CfgExpr::Bool(false),
			_ => CfgExpr::Name(name.into()),
		}),

		[TokenTree::Punct(eq), TokenTree::Literal(literal)] if eq.as_char() == '=' => match string_literal(&literal.to_string()) {
			Some(value) => Ok(CfgExpr::KeyValue(name.into(), value.into())),
			None => Err(CfgParseError::new(tokens, "expected a string literal")),
		},

		[TokenTree::Group(group)] if group.delimiter() == Delimiter::Parenthesis => {
			let inner: Vec<TokenTree> = group.stream().into_iter().collect();

			match name {
				"all" => Ok(CfgExpr::All(parse_list(&inner)?)),
				"any" => Ok(CfgExpr::Any(parse_list(&inner)?)),

				"not" => {
					let mut list = parse_list(&inner)?;

					match list.len() {
						1 => Ok(CfgExpr::Not(Box::new(list.pop().unwrap()))),
						_ => Err(CfgParseError::new(tokens, "`not` takes exactly one predicate")),
					}
				}

				_ => Ok(CfgExpr::Other(tokens_text(tokens))),
			}
		}

		_ => Err(CfgParseError::new(tokens, "expected `name`, `key = \"value\"`, or `op(...)`")),
	}
}

/// The value of a (possibly raw) string literal's source text, if it is one without escapes needing interpretation
/// beyond the simple ones.
fn string_literal(text: &str) -> Option<String> {
	if let Some(raw) = text.strip_prefix('r') {
		let hashes = raw.len() - raw.trim_start_matches('#').len();
		let inner = raw.get(hashes..raw.len().checked_sub(hashes)?)?;

		return inner.strip_prefix('"')?.strip_suffix('"').map(str::to_owned);
	}

	let inner = text.strip_prefix('"')?.strip_suffix('"')?;
	let mut value = String::with_capacity(inner.len());
	let mut chars = inner.chars();

	while let Some(char) = chars.next() {
		if char != '\\' {
			value.push(char);
			continue;
		}

		match chars.next()? {
			'n' => value.push('\n'),
			't' => value.push('\t'),
			'r' => value.push('\r'),
			'0' => value.push('\0'),
			'\\' => value.push('\\'),
			'"' => value.push('"'),
			'\'' => value.push('\''),
			_ => return None,
		}
	}

	Some(value)
}

fn tokens_text(tokens: &[TokenTree]) -> String {
	tokens.iter().cloned().collect::<TokenStream>().to_string()
}

/// Error parsing a `cfg` predicate.
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
#[error("invalid cfg predicate `{text}`: {message}")]
pub struct CfgParseError {
	/// The predicate (or the part of it that is invalid).
	pub text: String,

	/// What is wrong with it.
	pub message: String,
}

impl CfgParseError {
	fn new(tokens: &[TokenTree], message: &str) -> Self {
		Self {
			text: tokens_text(tokens),
			message: message.to_owned(),
		}
	}
}

/// The result of evaluating a `cfg` predicate when some configuration may be unknown
/// (e.g. `cfg`s set by build scripts or `RUSTFLAGS`).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tristate {
	/// Definitely true.
	True,

	/// Definitely false.
	False,

	/// Either, depending on configuration that is not known.
	Unknown,
}

impl Tristate {
	/// Three-valued conjunction: false if either is false, true if both are true.
	pub fn and(self, other: Self) -> Self {
		match (self, other) {
			(Self::False, _) | (_, Self::False) => Self::False,
			(Self::True, Self::True) => Self::True,
			_ => Self::Unknown,
		}
	}

	/// Three-valued disjunction: true if either is true, false if both are false.
	pub fn or(self, other: Self) -> Self {
		match (self, other) {
			(Self::True, _) | (_, Self::True) => Self::True,
			(Self::False, Self::False) => Self::False,
			_ => Self::Unknown,
		}
	}

	/// Three-valued negation: unknown stays unknown.
	#[expect(clippy::should_implement_trait)]
	pub fn not(self) -> Self {
		match self {
			Self::True => Self::False,
			Self::False => Self::True,
			Self::Unknown => Self::Unknown,
		}
	}

	/// `true` unless definitely `False`.
	pub fn is_possible(self) -> bool {
		self != Self::False
	}

	/// `true`, `false`, or `unknown`.
	pub fn name(self) -> &'static str {
		match self {
			Self::True => "true",
			Self::False => "false",
			Self::Unknown => "unknown",
		}
	}
}

impl From<bool> for Tristate {
	fn from(value: bool) -> Self {
		if value { Self::True } else { Self::False }
	}
}

impl std::fmt::Display for Tristate {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.name())
	}
}

/// The configuration `cfg` predicates are evaluated against.
///
/// A name or key is *definite* when the context knows its complete set of values;
/// predicates on definite names that are not set evaluate to [`Tristate::False`],
/// and predicates on other names evaluate to [`Tristate::Unknown`].
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct CfgContext {
	pub(crate) names: BTreeSet<SmolStr>,
	pub(crate) values: BTreeMap<SmolStr, BTreeSet<SmolStr>>,
	pub(crate) definite: BTreeSet<SmolStr>,
}

impl CfgContext {
	/// An empty context in which everything is unknown.
	pub fn new() -> Self {
		Self::default()
	}

	/// The host's configuration, from `rustc --print cfg` (honoring `$RUSTC`), plus `debug_assertions`.
	/// `test`, `doc`, `docsrs`, `miri`, and `proc_macro` are definitely unset; features are unknown until
	/// [`CfgContext::with_features`] is called.
	pub fn host() -> Result<Self, crate::Error> {
		Self::for_target(None)
	}

	/// Like [`CfgContext::host`] but for a target triple (`rustc --print cfg --target <triple>`).
	pub fn for_target(target: Option<&str>) -> Result<Self, crate::Error> {
		let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
		let mut command = Command::new(&rustc);

		command.args(["--print", "cfg"]);

		if let Some(target) = target {
			command.args(["--target", target]);
		}

		let output = command.output().map_err(|error| crate::Error::Rustc(format!("failed to run `{}`: {error}", rustc.to_string_lossy())))?;

		if !output.status.success() {
			return Err(crate::Error::Rustc(String::from_utf8_lossy(&output.stderr).trim().to_owned()));
		}

		let mut context = Self::from_rustc_print_cfg(&String::from_utf8_lossy(&output.stdout));

		// `rustc --print cfg` reflects a release-like invocation; cargo's default dev profile enables these
		context.set_name("debug_assertions", true);
		context.set_name("overflow_checks", true);

		Ok(context)
	}

	/// Parses the output of `rustc --print cfg` (one `name` or `key="value"` per line).
	/// Every name and key it mentions becomes definite, as do the well-known names it may omit
	/// (`unix`, `windows`, `test`, `debug_assertions`, `doc`, `docsrs`, `miri`, `proc_macro`, ...).
	pub fn from_rustc_print_cfg(output: &str) -> Self {
		let mut context = Self::new();

		context.definite.extend(WELL_KNOWN.iter().map(|&name| SmolStr::new(name)));

		for line in output.lines().map(str::trim).filter(|line| !line.is_empty()) {
			match CfgExpr::parse(line) {
				Ok(CfgExpr::Name(name)) => {
					context.names.insert(name.clone());
					context.definite.insert(name);
				}

				Ok(CfgExpr::KeyValue(key, value)) => {
					context.values.entry(key.clone()).or_default().insert(value);
					context.definite.insert(key);
				}

				_ => {}
			}
		}

		context
	}

	/// Sets the complete set of enabled features (making `feature` definite).
	pub fn with_features<S: Into<SmolStr>>(mut self, features: impl IntoIterator<Item = S>) -> Self {
		self.set_features(features);
		self
	}

	/// Sets the complete set of enabled features (making `feature` definite).
	pub fn set_features<S: Into<SmolStr>>(&mut self, features: impl IntoIterator<Item = S>) {
		self.values.insert("feature".into(), features.into_iter().map(Into::into).collect());
		self.definite.insert("feature".into());
	}

	/// The enabled features, if known.
	pub fn features(&self) -> Option<&BTreeSet<SmolStr>> {
		self.definite.contains("feature").then(|| self.values.get("feature")).flatten()
	}

	/// Enables a name (`test`) or key-value pair (`feature="x"`), given in `--cfg` syntax, making it definite.
	///
	/// A key-value pair adds to the key's values rather than replacing them.
	pub fn enable(&mut self, spec: &str) -> Result<(), CfgParseError> {
		match CfgExpr::parse(spec)? {
			CfgExpr::Name(name) => {
				self.names.insert(name.clone());
				self.definite.insert(name);
			}

			CfgExpr::KeyValue(key, value) => {
				self.values.entry(key.clone()).or_default().insert(value);
				self.definite.insert(key);
			}

			_ => {
				return Err(CfgParseError {
					text: spec.to_owned(),
					message: "expected `name` or `key=\"value\"`".to_owned(),
				});
			}
		}

		Ok(())
	}

	/// Sets a name as definitely enabled or disabled.
	pub fn set_name(&mut self, name: &str, enabled: bool) {
		if enabled {
			self.names.insert(name.into());
		} else {
			self.names.remove(name);
		}

		self.definite.insert(name.into());
	}

	/// Evaluates a predicate in this configuration.
	pub fn eval(&self, expr: &CfgExpr) -> Tristate {
		match expr {
			CfgExpr::Bool(value) => Tristate::from(*value),

			CfgExpr::Name(name) if self.names.contains(name) => Tristate::True,
			CfgExpr::Name(name) if self.definite.contains(name) => Tristate::False,
			CfgExpr::Name(_) => Tristate::Unknown,

			CfgExpr::KeyValue(key, value) if self.values.get(key).is_some_and(|values| values.contains(value)) => Tristate::True,
			CfgExpr::KeyValue(key, _) if self.definite.contains(key) => Tristate::False,
			CfgExpr::KeyValue(..) => Tristate::Unknown,

			CfgExpr::All(exprs) => exprs.iter().fold(Tristate::True, |result, expr| result.and(self.eval(expr))),
			CfgExpr::Any(exprs) => exprs.iter().fold(Tristate::False, |result, expr| result.or(self.eval(expr))),
			CfgExpr::Not(expr) => self.eval(expr).not(),
			CfgExpr::Other(_) => Tristate::Unknown,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_and_displays() {
		let expr = CfgExpr::parse(r##"all(unix, not(feature = "x"), any(target_os = "linux", r#true), true,)"##).unwrap();

		assert_eq!(expr.to_string(), r#"all(unix, not(feature = "x"), any(target_os = "linux", true), true)"#);
		assert_eq!(expr.features(), BTreeSet::from(["x"]));
		assert_eq!(CfgExpr::parse(r#"version("1.80")"#).unwrap(), CfgExpr::Other(r#"version ("1.80")"#.to_owned()));
		assert_eq!(CfgExpr::parse(r##"feature = r#"a"b"#"##).unwrap(), CfgExpr::KeyValue("feature".into(), "a\"b".into()));
		assert!(CfgExpr::parse("not(a, b)").is_err());
		assert!(CfgExpr::parse("a, b").is_err());
		assert!(CfgExpr::parse("").is_err());
		assert!(CfgExpr::parse("feature = 1").is_err());
	}

	#[test]
	fn parses_cfg_attr() {
		let (predicate, attrs) = CfgExpr::parse_cfg_attr(r#"feature = "serde", derive(Serialize), path = "a.rs""#.parse().unwrap()).unwrap();

		assert_eq!(predicate, CfgExpr::KeyValue("feature".into(), "serde".into()));
		assert_eq!(attrs.iter().map(ToString::to_string).collect::<Vec<_>>(), ["derive (Serialize)", "path = \"a.rs\""]);
	}

	#[test]
	fn all_flattens_and_dedups() {
		let a = CfgExpr::Name("a".into());
		let b = CfgExpr::Name("b".into());

		assert_eq!(CfgExpr::all([]), None);
		assert_eq!(CfgExpr::all([a.clone()]), Some(a.clone()));
		assert_eq!(CfgExpr::all([CfgExpr::All(vec![a.clone(), b.clone()]), a.clone()]), Some(CfgExpr::All(vec![a, b])));
	}

	#[test]
	fn evaluates_three_valued() {
		let mut context = CfgContext::from_rustc_print_cfg("unix\ntarget_os=\"linux\"\ntarget_pointer_width=\"64\"\n").with_features(["std"]);

		context.enable("my_cfg").unwrap();

		let eval = |text: &str| context.eval(&CfgExpr::parse(text).unwrap());

		assert_eq!(eval("unix"), Tristate::True);
		assert_eq!(eval("windows"), Tristate::False);
		assert_eq!(eval("test"), Tristate::False);
		assert_eq!(eval("my_cfg"), Tristate::True);
		assert_eq!(eval("from_build_script"), Tristate::Unknown);
		assert_eq!(eval(r#"target_os = "linux""#), Tristate::True);
		assert_eq!(eval(r#"target_os = "macos""#), Tristate::False);
		assert_eq!(eval(r#"feature = "std""#), Tristate::True);
		assert_eq!(eval(r#"feature = "alloc""#), Tristate::False);
		assert_eq!(eval("all(unix, from_build_script)"), Tristate::Unknown);
		assert_eq!(eval("all(windows, from_build_script)"), Tristate::False);
		assert_eq!(eval("any(unix, from_build_script)"), Tristate::True);
		assert_eq!(eval("not(from_build_script)"), Tristate::Unknown);
		assert_eq!(eval("any()"), Tristate::False);
		assert_eq!(eval("all()"), Tristate::True);
		assert_eq!(eval("false"), Tristate::False);
		assert_eq!(eval(r#"version("1.0")"#), Tristate::Unknown);
		assert_eq!(CfgContext::new().eval(&CfgExpr::parse("unix").unwrap()), Tristate::Unknown);
	}

	#[test]
	fn host_context() {
		let context = CfgContext::host().unwrap();

		assert_eq!(context.eval(&CfgExpr::parse("debug_assertions").unwrap()), Tristate::True);
		assert_eq!(context.eval(&CfgExpr::parse("test").unwrap()), Tristate::False);
		assert!(context.values.contains_key("target_os"));
	}
}
