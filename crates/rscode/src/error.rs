use crate::cfg::CfgParseError;
use crate::path::PathParseError;
use crate::source::LineCol;
use std::path::PathBuf;

/// Errors produced by rscode operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
	/// Reading or writing a file failed.
	#[error("{}: {source}", path.display())]
	Io {
		/// The file.
		path: PathBuf,

		/// What failed.
		source: std::io::Error,
	},

	/// A file does not parse.
	#[error("{}:{location}: {message}", path.display())]
	Parse {
		/// The file.
		path: PathBuf,

		/// Where parsing failed.
		location: LineCol,

		/// The parser's message.
		message: String,
	},

	/// An item path is invalid.
	#[error(transparent)]
	PathParse(#[from] PathParseError),

	/// A `cfg` predicate is invalid.
	#[error(transparent)]
	CfgParse(#[from] CfgParseError),

	/// A path (shown as given) names no item.
	#[error("no item found for `{0}`")]
	NotFound(String),

	/// A path names several items where one is needed.
	#[error("`{path}` is ambiguous; candidates:\n{}", candidates.join("\n"))]
	Ambiguous {
		/// The path, as given.
		path: String,

		/// The items it names, one line each (canonical path, kind, location, and `cfg`).
		candidates: Vec<String>,
	},

	/// A new name is not an identifier.
	#[error("`{0}` is not a valid identifier")]
	InvalidIdent(String),

	/// A new name is already taken (see the `force` options of the operations).
	#[error("`{name}` collides with existing names:\n{}", collisions.join("\n"))]
	Collision {
		/// The new name (or names).
		name: String,

		/// What already has the name, one line each.
		collisions: Vec<String>,
	},

	/// Two different edits of a file change overlapping text.
	#[error("{}: overlapping edits at bytes {first:?} and {second:?}", path.display())]
	OverlappingEdits {
		/// The file.
		path: PathBuf,

		/// The byte range of one edit.
		first: std::ops::Range<usize>,

		/// The byte range of the other.
		second: std::ops::Range<usize>,
	},

	/// Applying an edit failed partway (see [`EditSet::apply`](crate::EditSet::apply)), such as on a file or directory
	/// that another process has open (on Windows). What the edit changed before was undone, except for what `kept`
	/// lists.
	#[error("{}: {source}; {}", path.display(), undo_summary(kept))]
	Apply {
		/// The file or directory whose change failed.
		path: PathBuf,

		/// What failed.
		source: std::io::Error,

		/// The changes made before the failure that could not be undone, one line each (with why).
		kept: Vec<String>,
	},

	/// An edited file would no longer parse, so nothing is written.
	#[error("the edit would leave {} unparsable ({location}: {message}); nothing was written", path.display())]
	EditBreaksSyntax {
		/// The file.
		path: PathBuf,

		/// Where the edited text fails to parse.
		location: LineCol,

		/// The parser's message.
		message: String,
	},

	/// New source is not valid for where it goes.
	#[error("{0}")]
	InvalidSource(String),

	/// An edited item would become another kind of item, or several items (see
	/// [`EditItemOptions::allow_kind_change`](crate::edit::EditItemOptions::allow_kind_change)).
	#[error("{0}")]
	KindChange(String),

	/// Text to replace inside of an item (see [`edit_item`](crate::edit::edit_item)) does not occur in it exactly once.
	#[error("{message}")]
	TextMismatch {
		/// Where the text was looked for, and where it occurs, or what comes closest.
		message: String,

		/// The (1-based) lines of the file where it occurs: none when it does not occur.
		lines: Vec<usize>,
	},

	/// The operation does not support what it was given.
	#[error("{0}")]
	Unsupported(String),

	/// An item kind name is not known.
	#[error("unknown item kind `{0}`")]
	UnknownItemKind(String),

	/// Formatting failed.
	#[error(transparent)]
	Format(#[from] rscode_fmt::FormatError),

	/// Sorting failed.
	#[error(transparent)]
	Sort(#[from] rscode_sort::SortError),

	/// rustc could not tell about the target (to evaluate `cfg`s).
	#[error("failed to query rustc: {0}")]
	Rustc(String),

	/// cargo failed (such as on an invalid manifest, or an unknown package or feature), with its message.
	#[error("cargo: {0}")]
	Cargo(String),
}

impl Error {
	pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
		Self::Io { path: path.into(), source }
	}
}

/// The end of the message of [`Error::Apply`].
fn undo_summary(kept: &[String]) -> String {
	match kept.is_empty() {
		true => "nothing was changed".to_owned(),
		false => format!("undoing the edit failed, so these changes stay:\n{}", kept.join("\n")),
	}
}
