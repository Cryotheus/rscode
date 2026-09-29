//! Formatting of Rust source files and of individual items within them.
//!
//! Configure which formatter to use (rustfmt or prettyplease) and optionally sort items with [`rscode_sort`]
//! before formatting.
//!
//! - [`Formatter::format_str`] formats a whole file.
//! - [`Formatter::format_items`] formats only selected items of a file, leaving the rest of the text untouched.
//! - [`Formatter::format_tokens`] formats a [`proc_macro2::TokenStream`], such as generated `bindgen` output.
//! - [`emit`] renders the difference between original and formatted sources (diff, JSON, checkstyle).
//!
//! ```
//! use rscode_fmt::FormatOptions;
//! use rscode_fmt::FormatTarget;
//! use rscode_fmt::Formatter;
//! use rscode_fmt::RsFormatter;
//!
//! let formatter = Formatter::new(FormatOptions::new().formatter(RsFormatter::PrettyPlease));
//! let source = "fn  a( ) {}\nfn  b( ) {}\n";
//!
//! assert_eq!(formatter.format_str(source).unwrap(), "fn a() {}\nfn b() {}\n");
//!
//! // only `b` is formatted
//! let b = source.find("fn  b").unwrap();
//!
//! assert_eq!(formatter.format_items(source, &[FormatTarget::Item(b)]).unwrap(), "fn  a( ) {}\nfn b() {}\n");
//! ```
//!
//! # Threads
//!
//! Parsing uses `proc_macro2`'s thread-local source map, which grows with every parse: long-running processes should
//! format on short-lived threads (or call `proc_macro2::extra::invalidate_current_thread_spans`).
//!
//! Parsing, formatting, and dropping syntax trees recurse once per level of nesting of the source, and running out of
//! stack aborts the process (it is not a panic that can be caught). A thread's stack must therefore be large enough
//! for the most deeply nested code to be formatted: the 2 MiB of spawned threads is exhausted by expressions nested
//! only several hundred levels deep. Run formatting on a thread with a stack of [`RECOMMENDED_STACK_SIZE`]:
//!
//! ```
//! # use rscode_fmt::FormatOptions;
//! # use rscode_fmt::Formatter;
//! # use rscode_fmt::RsFormatter;
//! let formatter = Formatter::new(FormatOptions::new().formatter(RsFormatter::PrettyPlease));
//! let thread = std::thread::Builder::new().stack_size(rscode_fmt::RECOMMENDED_STACK_SIZE);
//! let formatted = thread.spawn(move || formatter.format_str("fn  a( ) {}\n")).unwrap().join().unwrap();
//!
//! assert_eq!(formatted.unwrap(), "fn a() {}\n");
//! ```

#![warn(missing_docs)]

mod config;
pub mod emit;
mod equivalence;
mod items;
mod prettyplease_fmt;
mod rustfmt;
mod source;
mod tokens;
mod tree;
mod trivia;

use proc_macro2::TokenStream;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;

pub use rscode_sort;
pub use rscode_sort::SortOptions;
pub use trivia::contains_comments;

/// The stack size recommended for threads that format (see [Threads](crate#threads)): 64 MiB.
///
/// This is address space reserved for the stack; memory is only committed as the stack grows. It is enough for
/// expressions nested thousands of levels deep, even in debug builds.
pub const RECOMMENDED_STACK_SIZE: usize = 64 * 1024 * 1024;

/// A Rust edition.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum Edition {
	/// Rust 2015.
	#[serde(rename = "2015")]
	E2015,

	/// Rust 2018.
	#[serde(rename = "2018")]
	E2018,

	/// Rust 2021.
	#[serde(rename = "2021")]
	E2021,

	/// Rust 2024.
	#[default]
	#[serde(rename = "2024")]
	E2024,
}

impl Edition {
	/// Every edition, oldest first.
	pub const ALL: &'static [Self] = &[Self::E2015, Self::E2018, Self::E2021, Self::E2024];

	/// The year of the edition, as accepted by [`str::parse`].
	pub fn as_str(self) -> &'static str {
		match self {
			Self::E2015 => "2015",
			Self::E2018 => "2018",
			Self::E2021 => "2021",
			Self::E2024 => "2024",
		}
	}
}

impl std::fmt::Display for Edition {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.as_str())
	}
}

impl std::str::FromStr for Edition {
	type Err = FormatError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Self::ALL
			.iter()
			.copied()
			.find(|edition| edition.as_str() == s)
			.ok_or_else(|| FormatError::UnknownEdition(s.to_owned()))
	}
}

#[cfg(feature = "clap")]
impl clap::ValueEnum for Edition {
	fn value_variants<'a>() -> &'a [Self] {
		Self::ALL
	}

	fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
		Some(clap::builder::PossibleValue::new(self.as_str()))
	}
}

/// Errors produced while formatting.
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
	/// The source is not valid Rust.
	#[error("failed to parse Rust source at {line}:{column}: {message}")]
	Parse {
		/// The parser's message.
		message: String,

		/// 1-based line.
		line: usize,

		/// 1-based column, in characters.
		column: usize,
	},

	/// Sorting failed.
	#[error(transparent)]
	Sort(#[from] rscode_sort::SortError),

	/// rustfmt could not be run, or communicating with it failed.
	#[error("failed to run rustfmt (`{program}`): {source}")]
	RustFmtSpawn {
		/// The program that was run.
		program: String,

		/// The underlying error.
		source: std::io::Error,
	},

	/// rustfmt failed, such as on invalid source or configuration.
	#[error("rustfmt failed: {stderr}")]
	RustFmt {
		/// What rustfmt printed to stderr (trimmed).
		stderr: String,
	},

	/// A `--config` override for rustfmt cannot be passed on the command line.
	#[error("invalid rustfmt configuration override `{0}`: keys and values cannot contain `,`, and keys cannot contain `=`")]
	InvalidRustFmtConfig(String),

	/// prettyplease cannot print the source, such as syntax `syn` does not model, or would change its meaning, such as
	/// by leaving out syntax it does not support.
	#[error("prettyplease cannot format the source: {0}")]
	PrettyPlease(String),

	/// prettyplease would discard non-doc comments (see [`FormatOptions::allow_comment_loss`]).
	#[error("prettyplease would discard comments; enable `allow_comment_loss` to format anyway")]
	CommentsWouldBeLost,

	/// No item starts at the byte offset of a [`FormatTarget::Item`].
	#[error("no item starts at byte {0}")]
	NoItem(usize),

	/// The output of sorting or formatting cannot be matched with the original source.
	#[error("the formatted output no longer matches the structure of the original source: {0}")]
	StructureMismatch(String),

	/// An unknown formatter name was parsed.
	#[error("unknown formatter `{0}`")]
	UnknownFormatter(String),

	/// An unknown edition was parsed.
	#[error("unknown edition `{0}`")]
	UnknownEdition(String),
}

impl FormatError {
	/// The error of parsing a token stream.
	///
	/// Must be called on the thread that parsed: spans live in a thread-local source map.
	pub(crate) fn from_syn(error: &syn::Error) -> Self {
		let start = error.span().start();

		Self::Parse {
			message: error.to_string(),
			line: start.line,
			column: start.column + 1,
		}
	}

	/// The error of parsing `source` with `syn::parse_file`, which skips a byte order mark before parsing.
	///
	/// Must be called on the thread that parsed: spans live in a thread-local source map.
	pub(crate) fn from_syn_in(error: &syn::Error, source: &str) -> Self {
		let start = error.span().start();
		let bom = start.line == 1 && source.starts_with(source::BOM);

		Self::Parse {
			message: error.to_string(),
			line: start.line,
			column: start.column + 1 + usize::from(bom),
		}
	}
}

/// Options for formatting.
#[derive(Debug, Default, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct FormatOptions {
	/// The formatter to run.
	pub formatter: RsFormatter,

	/// Options for [`RsFormatter::RustFmt`].
	pub rustfmt: RustFmtOptions,

	/// Sort items before formatting. `None` disables sorting.
	pub sort: Option<SortOptions>,

	/// Allow prettyplease to discard non-doc comments instead of failing with [`FormatError::CommentsWouldBeLost`].
	pub allow_comment_loss: bool,
}

impl FormatOptions {
	/// The default options: rustfmt, without sorting.
	pub fn new() -> Self {
		Self::default()
	}

	/// Sets [`FormatOptions::allow_comment_loss`].
	pub fn allow_comment_loss(mut self, allow: bool) -> Self {
		self.allow_comment_loss = allow;
		self
	}

	/// Sets the edition passed to rustfmt ([`RustFmtOptions::edition`]).
	pub fn edition(mut self, edition: Edition) -> Self {
		self.rustfmt.edition = Some(edition);
		self
	}

	/// Sets [`FormatOptions::formatter`].
	pub fn formatter(mut self, formatter: RsFormatter) -> Self {
		self.formatter = formatter;
		self
	}

	/// Sets [`FormatOptions::rustfmt`].
	pub fn rustfmt(mut self, rustfmt: RustFmtOptions) -> Self {
		self.rustfmt = rustfmt;
		self
	}

	/// Sets [`FormatOptions::sort`].
	pub fn sort(mut self, sort: Option<SortOptions>) -> Self {
		self.sort = sort;
		self
	}
}

/// An item or file to format with [`Formatter::format_items`].
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum FormatTarget {
	/// The whole file.
	File,

	/// The item whose first token (including outer attributes and doc comments) starts at this byte offset.
	///
	/// Items at every nesting level can be targeted: items of the file and of inline modules, and the items of
	/// `impl` blocks, traits, and `extern` blocks. Items inside function bodies cannot.
	Item(usize),
}

/// Sorts (optionally) and formats Rust source.
#[derive(Debug, Default, Clone)]
pub struct Formatter {
	options: FormatOptions,
}

impl Formatter {
	/// A formatter with the given options.
	pub fn new(options: FormatOptions) -> Self {
		Self { options }
	}

	/// Sorts (if enabled) then formats only the targeted items of a source file.
	///
	/// Text outside of the targets is left byte-for-byte untouched: only the text of each targeted item (from its
	/// first attribute or doc comment to its last token, plus the indentation before it) is replaced. Comments
	/// directly above an item are not part of it. The formatted text gets the file's line endings.
	///
	/// Targets nested inside other targets are covered by them. Sorting applies to the targeted containers (inline
	/// modules, `impl` blocks, traits, and `extern` blocks); other targeted items are only formatted.
	/// [`FormatTarget::File`] formats the whole file, like [`Formatter::format_str`].
	///
	/// Fails with [`FormatError::NoItem`] if a target is not the start of an item, and with
	/// [`FormatError::StructureMismatch`] if the output of sorting or formatting cannot be matched with the source.
	pub fn format_items(&self, source: &str, targets: &[FormatTarget]) -> Result<String, FormatError> {
		items::format_items(source, targets, &self.options)
	}

	/// Sorts (if enabled) then formats a whole source file.
	///
	/// A shebang and a byte order mark are preserved, as are `\r\n` line breaks (going by the first line break) unless
	/// rustfmt's `newline_style` is set to something other than `Auto`. Otherwise, the output of rustfmt is exactly
	/// what rustfmt prints for the source.
	pub fn format_str(&self, source: &str) -> Result<String, FormatError> {
		self.format_items(source, &[FormatTarget::File])
	}

	/// Sorts (if enabled) then formats a token stream containing a whole file's worth of items.
	///
	/// Without a formatter ([`RsFormatter::None`]), the tokens are printed on a single line.
	///
	/// Groups without delimiters ([`proc_macro2::Delimiter::None`], such as around an expression interpolated by a
	/// `macro_rules!` macro) have no text, and are printed as parentheses where they group expressions or types that
	/// would otherwise be read differently: `⟦a + b⟧ * 2` becomes `(a + b) * 2`. Those in the arguments of macros and
	/// attributes are printed as parentheses when they contain more than one token tree.
	pub fn format_tokens(&self, tokens: TokenStream) -> Result<String, FormatError> {
		let tokens = match &self.options.sort {
			Some(sort) => rscode_sort::Sorter::new(sort.clone()).sort_tokens(tokens)?,
			None => tokens,
		};

		match self.options.formatter {
			RsFormatter::RustFmt => {
				let formatted = rustfmt::format(&tokens::to_source(tokens)?, &self.options.rustfmt)?;

				source::ensure_parses(&formatted, "rustfmt")?;

				Ok(formatted)
			}
			RsFormatter::PrettyPlease => prettyplease_fmt::format_tokens(tokens),
			RsFormatter::None => tokens::to_source(tokens),
		}
	}

	/// The options of the formatter.
	pub fn options(&self) -> &FormatOptions {
		&self.options
	}
}

/// Which formatter to run.
///
/// Serialized by [`RsFormatter::name`].
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum RsFormatter {
	/// `rustfmt`, run as a subprocess. Preserves comments and honors `rustfmt.toml`.
	#[default]
	#[serde(rename = "rustfmt")]
	RustFmt,

	/// [`prettyplease`]: fast and dependency-free, but discards non-doc comments.
	/// Intended for generated code.
	#[serde(rename = "prettyplease")]
	PrettyPlease,

	/// Do not format (only sort, if sorting is enabled).
	#[serde(rename = "none")]
	None,
}

impl RsFormatter {
	/// Every formatter.
	pub const ALL: &'static [Self] = &[Self::RustFmt, Self::PrettyPlease, Self::None];

	/// The name of the formatter, as accepted by [`str::parse`].
	pub fn name(self) -> &'static str {
		match self {
			Self::RustFmt => "rustfmt",
			Self::PrettyPlease => "prettyplease",
			Self::None => "none",
		}
	}
}

impl std::fmt::Display for RsFormatter {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.name())
	}
}

impl std::str::FromStr for RsFormatter {
	type Err = FormatError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Self::ALL
			.iter()
			.copied()
			.find(|formatter| formatter.name().eq_ignore_ascii_case(s))
			.ok_or_else(|| FormatError::UnknownFormatter(s.to_owned()))
	}
}

#[cfg(feature = "clap")]
impl clap::ValueEnum for RsFormatter {
	fn value_variants<'a>() -> &'a [Self] {
		Self::ALL
	}

	fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
		Some(clap::builder::PossibleValue::new(self.name()))
	}
}

/// Options for running `rustfmt`.
#[derive(Debug, Default, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct RustFmtOptions {
	/// The `rustfmt` executable. Defaults to `$RUSTFMT`, then `rustfmt` from `PATH`.
	pub program: Option<PathBuf>,

	/// `--edition`, [`Edition::default`] (the latest) when `None`.
	///
	/// Like `cargo fmt`, which passes each crate's edition, this overrides an `edition` of rustfmt's configuration
	/// (without an edition, rustfmt would assume 2015).
	pub edition: Option<Edition>,

	/// `--style-edition`. Without one, rustfmt's configuration decides, and then the edition (see
	/// [`RustFmtOptions::style_edition_in_effect`]).
	pub style_edition: Option<Edition>,

	/// Where rustfmt's configuration comes from.
	///
	/// - A file: the configuration file (`--config-path`).
	/// - A directory: rustfmt searches it and its ancestors for `rustfmt.toml` or `.rustfmt.toml`, then falls back to
	///   the user's global configuration, as it does for a file in that directory.
	/// - `None`: the same search, from the current working directory.
	///
	/// Source is passed to rustfmt over stdin, so rustfmt cannot find the configuration of the file being formatted on
	/// its own. Set this to the directory of the file to honor the project's configuration.
	pub config_path: Option<PathBuf>,

	/// `--config key=value` overrides. Keys and values cannot contain `,`, and keys cannot contain `=`.
	pub config: Vec<(String, String)>,
}

impl RustFmtOptions {
	/// The style edition rustfmt formats with, given these options (it decides how rustfmt orders `use` items).
	///
	/// That is (highest precedence first) a `style_edition` in [`RustFmtOptions::config`], the
	/// [`RustFmtOptions::style_edition`], the `style_edition` (or the deprecated `version`) of rustfmt's configuration
	/// file, or else the [`RustFmtOptions::edition`]. The configuration file is found like rustfmt finds it (see
	/// [`RustFmtOptions::config_path`]), and read every time.
	pub fn style_edition_in_effect(&self) -> Edition {
		config::style_edition(self)
	}
}

impl From<Edition> for rscode_sort::StyleEdition {
	fn from(edition: Edition) -> Self {
		match edition {
			Edition::E2015 => Self::E2015,
			Edition::E2018 => Self::E2018,
			Edition::E2021 => Self::E2021,
			Edition::E2024 => Self::E2024,
		}
	}
}

/// Formats a whole source file with rustfmt for the latest edition ([`Edition::default`]), without sorting.
pub fn format_str(source: &str) -> Result<String, FormatError> {
	Formatter::new(FormatOptions::new().edition(Edition::default())).format_str(source)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn builder() {
		let sort = SortOptions::new().recursive(false);
		let options = FormatOptions::new()
			.formatter(RsFormatter::None)
			.rustfmt(RustFmtOptions {
				program: Some(PathBuf::from("rustfmt")),
				..RustFmtOptions::default()
			})
			.edition(Edition::E2018)
			.sort(Some(sort.clone()))
			.allow_comment_loss(true);

		assert_eq!(options.formatter, RsFormatter::None);
		assert_eq!(options.rustfmt.program.as_deref(), Some(std::path::Path::new("rustfmt")));
		assert_eq!(options.rustfmt.edition, Some(Edition::E2018));
		assert_eq!(options.sort, Some(sort));
		assert!(options.allow_comment_loss);
		assert_eq!(Formatter::new(options.clone()).options(), &options);
	}

	#[test]
	fn formats_sorted_tokens() {
		let formatter = Formatter::new(FormatOptions::new().formatter(RsFormatter::PrettyPlease).sort(Some(SortOptions::new())));
		let formatted = formatter
			.format_tokens(quote::quote!(
				fn b() {}
				fn a() {}
			))
			.unwrap();

		assert!(formatted.find("fn a").unwrap() < formatted.find("fn b").unwrap(), "{formatted}");
	}

	#[test]
	fn formats_tokens_keeping_the_grouping_of_groups_without_delimiters() {
		let sum = proc_macro2::Group::new(proc_macro2::Delimiter::None, quote::quote!(a + b));
		let tokens = quote::quote!(fn f(a: u8, b: u8) -> u8 { m!(#sum * 2); #sum * 2 });
		let format = |formatter: RsFormatter| Formatter::new(FormatOptions::new().formatter(formatter)).format_tokens(tokens.clone());

		assert_eq!(
			format(RsFormatter::None).unwrap(),
			"fn f (a : u8 , b : u8) -> u8 { m ! ((a + b) * 2) ; (a + b) * 2 }"
		);
		assert_eq!(
			format(RsFormatter::PrettyPlease).unwrap(),
			"fn f(a: u8, b: u8) -> u8 {\n    m!((a + b) * 2);\n    (a + b) * 2\n}\n"
		);
	}

	#[test]
	fn formats_tokens_with_prettyplease() {
		let formatter = Formatter::new(FormatOptions::new().formatter(RsFormatter::PrettyPlease));

		assert_eq!(
			formatter
				.format_tokens(quote::quote!(
					fn a() {}
				))
				.unwrap(),
			"fn a() {}\n"
		);
	}

	#[test]
	fn formats_tokens_without_a_formatter() {
		let formatter = Formatter::new(FormatOptions::new().formatter(RsFormatter::None));

		assert_eq!(
			formatter
				.format_tokens(quote::quote!(
					fn a() {}
				))
				.unwrap(),
			"fn a () { }"
		);
	}

	#[test]
	fn names_round_trip() {
		for &formatter in RsFormatter::ALL {
			assert_eq!(formatter.name().parse::<RsFormatter>().unwrap(), formatter);
			assert_eq!(formatter.to_string(), formatter.name());
			assert_eq!(serde_json::to_value(formatter).unwrap(), formatter.name());
		}

		for &edition in Edition::ALL {
			assert_eq!(serde_json::to_value(edition).unwrap(), edition.as_str());
		}

		for &edition in Edition::ALL {
			assert_eq!(edition.as_str().parse::<Edition>().unwrap(), edition);
		}

		assert_eq!("RustFmt".parse::<RsFormatter>().unwrap(), RsFormatter::RustFmt);
		assert!(matches!("black".parse::<RsFormatter>(), Err(FormatError::UnknownFormatter(_))));
		assert!(matches!("2027".parse::<Edition>(), Err(FormatError::UnknownEdition(_))));
	}

	#[test]
	fn options_serialize_in_kebab_case() {
		let options = FormatOptions::new()
			.formatter(RsFormatter::PrettyPlease)
			.edition(Edition::E2021)
			.allow_comment_loss(true);
		let json = serde_json::to_value(&options).unwrap();

		assert_eq!(json["formatter"], "prettyplease");
		assert_eq!(json["rustfmt"]["edition"], "2021");
		assert_eq!(json["allow-comment-loss"], true);
		assert_eq!(serde_json::from_value::<FormatOptions>(json).unwrap(), options);
		assert_eq!(serde_json::from_str::<FormatOptions>("{}").unwrap(), FormatOptions::default());
	}
}
