//! Sources: cargo workspaces and packages a session works on besides the server's own, attached by name when their
//! directories are exposed ([`ServerOptions::exposed`](super::ServerOptions::exposed)).
//!
//! Attaching only records a name for a canonical `Cargo.toml` (after checking that cargo can plan it), so it is
//! cheap: every tool call loads its source from disk anyway. The names belong to the session that attached them.

use super::render;
use crate::edit::EditSet;
use crate::load::without_verbatim_prefix;
use crate::workspace::LoadOptions;
use crate::workspace::plan_workspace;
use glob::MatchOptions;
use glob::Pattern;
use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::path::Component;
use std::path::MAIN_SEPARATOR;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

/// The attached sources of a session, by name.
pub(crate) type Sources = BTreeMap<String, Source>;

/// How exposed directories are matched: `*` and `?` stay within a component, and wildcards skip hidden names.
const MATCHING: MatchOptions = MatchOptions {
	case_sensitive: !cfg!(windows),
	require_literal_separator: true,
	require_literal_leading_dot: true,
};

/// Longest name of a source.
const MAX_NAME_LEN: usize = 64;

/// How the workspaces and packages of exposed directories can be used.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Access {
	/// Their files can be read: by the query tools, and by the editing tools' previews (`dry_run`, `check`).
	Read,

	/// Their files can also be written by the editing tools. Takes precedence over [`Access::Read`].
	Write,
}

impl Access {
	/// How a client is told about the access.
	pub(crate) fn describe(self) -> &'static str {
		match self {
			Self::Read => "read-only",
			Self::Write => "read and write",
		}
	}
}

impl fmt::Display for Access {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.pad(match self {
			Self::Read => "read",
			Self::Write => "write",
		})
	}
}

impl FromStr for Access {
	type Err = ExposureError;

	fn from_str(text: &str) -> Result<Self, Self::Err> {
		match text {
			"read" => Ok(Self::Read),
			"write" => Ok(Self::Write),
			_ => Err(ExposureError(format!("unknown access `{text}`: expected `read` or `write`"))),
		}
	}
}

/// Directories whose cargo workspaces and packages clients may attach, as a glob pattern, with the access they get.
///
/// A workspace or package can be attached when the directory of its `Cargo.toml` matches the pattern: `*` matches
/// within one path component, `?` one character, `[abc]` one of the characters, and `**` any number of components.
/// `/refs/*` matches `/refs/log` but neither `/refs` nor `/refs/misc/log`; `/refs/misc/**` matches `/refs/misc` and
/// every directory below it. Like in shells, wildcards do not match names starting with `.`. Case matters, except on
/// Windows.
///
/// Paths are compared with symbolic links resolved: a relative pattern is relative to the working directory, and the
/// part of the pattern before its first wildcard is canonicalized as far as it exists.
///
/// When they edit an attached source, the editing tools only write files and directories that are below a directory
/// matching an [`Access::Write`] pattern: the directory of a `Cargo.toml` attached for writing, and everything in it.
/// The server's own workspace is not restricted, like on the command line.
#[derive(Debug, Clone)]
pub struct Exposure {
	access: Access,

	/// The pattern as given.
	text: String,

	/// The pattern with an absolute, resolved literal prefix.
	pattern: Pattern,

	/// For a pattern ending in `**`, the directory the `**` is in, which the pattern itself does not match.
	base: Option<Pattern>,
}

impl Exposure {
	/// Exposes the directories matching `pattern` (see [`Exposure`]).
	///
	/// Fails for an empty or invalid pattern, and for `.` or `..` after a wildcard.
	pub fn new(access: Access, pattern: &str) -> Result<Self, ExposureError> {
		let (compiled, base) = compile(pattern).map_err(|reason| ExposureError(format!("`{pattern}`: {reason}")))?;

		Ok(Self {
			access,
			text: pattern.to_owned(),
			pattern: compiled,
			base,
		})
	}

	/// The access the directories get.
	pub fn access(&self) -> Access {
		self.access
	}

	/// Whether the pattern matches a directory, given with symbolic links resolved.
	pub fn matches(&self, directory: &Path) -> bool {
		self.pattern.matches_path_with(directory, MATCHING) || self.base.as_ref().is_some_and(|base| base.matches_path_with(directory, MATCHING))
	}

	/// The pattern as given.
	pub fn pattern(&self) -> &str {
		&self.text
	}

	/// The pattern that paths are matched against: absolute, with the part before its first wildcard resolved.
	pub fn resolved_pattern(&self) -> &str {
		self.pattern.as_str()
	}
}

impl fmt::Display for Exposure {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}={}", self.access, self.text)
	}
}

/// `ACCESS=PATTERN`, like `write=/abs/path/project` or `read=/abs/path/references/*`.
impl FromStr for Exposure {
	type Err = ExposureError;

	fn from_str(text: &str) -> Result<Self, Self::Err> {
		let Some((access, pattern)) = text.split_once('=') else {
			return Err(ExposureError(format!(
				"`{text}` is not `read=<directory glob>` or `write=<directory glob>`"
			)));
		};

		Self::new(access.trim().parse()?, pattern)
	}
}

/// An invalid [`Exposure`] or [`Access`].
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct ExposureError(String);

/// A workspace or package attached under a name.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct Source {
	/// Its `Cargo.toml`, canonical.
	pub(crate) manifest: PathBuf,

	pub(crate) access: Access,
}

impl Source {
	/// Checks that the source's manifest is still where it was attached (e.g. that no symbolic link now leads
	/// elsewhere), so that it is still exposed.
	pub(crate) fn check(&self, name: &str) -> Result<(), String> {
		match std::fs::canonicalize(&self.manifest).map(without_verbatim_prefix) {
			Ok(path) if path == self.manifest => Ok(()),

			Ok(path) => Err(format!(
				"the path of source `{name}` ({}) now leads to {} (through a symbolic link): attach it again",
				self.manifest.display(),
				path.display()
			)),

			Err(error) => Err(format!("source `{name}`: {}: {error}", self.manifest.display())),
		}
	}
}

/// Where the edits of a tool call may be written.
#[derive(Debug, Clone)]
pub(crate) enum WriteScope {
	/// Anywhere, like the command line: the server's own workspace.
	Anywhere,

	/// In the directories exposed for writing: a source attached for writing.
	Exposed { source: String, exposed: Arc<[Exposure]> },

	/// Nowhere: a source attached read-only.
	Nowhere { source: String },
}

impl WriteScope {
	/// Checks that every file and directory the edits change, move (from and to), or delete may be written.
	pub(crate) fn check(&self, root: &Path, edits: &EditSet) -> Result<(), String> {
		let (source, exposed) = match self {
			Self::Anywhere => return Ok(()),
			Self::Nowhere { source } => return Err(format!("source `{source}` is read-only; nothing was written")),
			Self::Exposed { source, exposed } => (source, exposed),
		};

		let moved = edits.moves().iter().flat_map(|(from, to)| [from, to]);
		let entries = moved.chain(edits.deletions()).map(PathBuf::as_path);
		let denied = edits
			.edited_files()
			.find(|path| !writable(exposed, path))
			.or_else(|| entries.into_iter().find(|path| !entry_writable(exposed, path)));

		match denied {
			None => Ok(()),

			Some(path) => Err(format!(
				"the edit of source `{source}` would change {}, which is not in a directory exposed for writing; \
				 nothing was written",
				render::relative(root, path).display()
			)),
		}
	}
}

/// The best access the exposures give the workspace or package whose manifest is in `directory` (resolved).
pub(crate) fn access(exposed: &[Exposure], directory: &Path) -> Option<Access> {
	exposed.iter().filter(|exposure| exposure.matches(directory)).map(Exposure::access).max()
}

/// Checks that a client may attach the workspace or package of `manifest` (as given), with write access if `write`,
/// and that cargo can plan loading it. Returns the source and a summary of its packages. `cap` is the most access the
/// server gives, for messages.
pub(crate) fn attach(manifest: &str, write: bool, exposed: &[Exposure], cap: Access, own: &LoadOptions) -> Result<(Source, String), String> {
	let manifest = self::manifest(manifest)?;
	let directory = manifest.parent().unwrap_or(Path::new(""));
	let matching: Vec<String> = exposed
		.iter()
		.filter(|exposure| exposure.matches(directory))
		.map(ToString::to_string)
		.collect();

	match access(exposed, directory) {
		None => {
			return Err(format!(
				"{} is not in a directory exposed by this server\n{}",
				directory.display(),
				exposed_list(exposed, cap)
			));
		}

		Some(Access::Read) if write => {
			return Err(format!(
				"{} is only exposed for reading ({}): attach it without `write` to read it",
				directory.display(),
				matching.join(", ")
			));
		}

		Some(_) => {}
	}

	let access = if write { Access::Write } else { Access::Read };
	let plan = plan_workspace(&load_options(own, &manifest)).map_err(|error| format!("failed to load {}: {error}", manifest.display()))?;
	let mut summary = format!("workspace root: {}\n", plan.root.display());
	let packages: Vec<String> = plan
		.packages
		.iter()
		.filter(|package| package.is_member)
		.map(|package| format!("{} {}", package.name, package.version))
		.collect();

	writeln!(summary, "packages loaded by default: {}", list_or_none(&packages)).unwrap();

	if !plan.unloaded_members.is_empty() {
		let names: Vec<&str> = plan.unloaded_members.iter().map(|member| member.name.as_str()).collect();

		writeln!(
			summary,
			"other workspace members (select them with `packages` or `workspace`): {}",
			names.join(", ")
		)
		.unwrap();
	}

	Ok((Source { manifest, access }, summary))
}

/// Checks a name for a source.
pub(crate) fn check_name(name: &str) -> Result<(), String> {
	let valid = name.chars().all(|char| char.is_ascii_alphanumeric() || matches!(char, '_' | '-' | '.'));

	match name {
		"" => Err("`name` is empty: choose a name to refer to the source by".to_owned()),
		_ if !valid || name.len() > MAX_NAME_LEN => Err(format!(
			"invalid name `{name}`: use up to {MAX_NAME_LEN} ASCII letters, digits, `_`, `-`, and `.`"
		)),
		_ => Ok(()),
	}
}

/// Compiles an exposure's pattern, and for a pattern ending in `**`, the pattern of the directory the `**` is in.
fn compile(text: &str) -> Result<(Pattern, Option<Pattern>), String> {
	if text.trim().is_empty() {
		return Err("the pattern is empty".to_owned());
	}

	let path = Path::new(text);
	let absolute = match path.is_absolute() {
		true => path.to_path_buf(),
		false => std::env::current_dir()
			.map_err(|error| format!("the working directory is unknown: {error}"))?
			.join(path),
	};
	let mut literal = PathBuf::new();
	let mut wild: Vec<&str> = Vec::new();

	for component in absolute.components() {
		match component {
			Component::Normal(name) if wild.is_empty() && !has_wildcard(name.as_encoded_bytes()) => literal.push(name),

			Component::Normal(name) => {
				wild.push(name.to_str().ok_or("the pattern is not valid UTF-8")?);
			}

			Component::CurDir | Component::ParentDir if !wild.is_empty() => {
				return Err("`.` and `..` cannot follow a wildcard".to_owned());
			}

			component => literal.push(component),
		}
	}

	let literal = resolve(&literal);
	let literal = Pattern::escape(literal.to_str().ok_or("the pattern is not valid UTF-8")?);
	let join = |wild: &[&str]| {
		let mut pattern = literal.clone();

		for component in wild {
			if !pattern.ends_with(MAIN_SEPARATOR) {
				pattern.push(MAIN_SEPARATOR);
			}

			pattern.push_str(component);
		}

		Pattern::new(&pattern).map_err(|error| format!("{} (at `{pattern}`)", error.msg))
	};
	let base = match wild.split_last() {
		Some((&"**", rest)) => Some(join(rest)?),
		_ => None,
	};

	Ok((join(&wild)?, base))
}

/// Whether the editing tools may create, move, or delete the entry `path`: like [`writable`], but a symbolic link at
/// `path` itself is not followed, since moving or deleting it moves or deletes the link.
pub(crate) fn entry_writable(exposed: &[Exposure], path: &Path) -> bool {
	let entry = match (path.parent(), path.file_name()) {
		(Some(parent), Some(name)) => resolve(parent).join(name),
		_ => resolve(path),
	};

	in_write_area(exposed, &entry)
}

/// Lines listing the exposed directories, with at most the access `cap`.
pub(crate) fn exposed_list(exposed: &[Exposure], cap: Access) -> String {
	let mut text = "directories whose workspaces and packages can be attached (the directory of the Cargo.toml must \
	                match):\n"
		.to_owned();

	for exposure in exposed {
		writeln!(text, "  {:5}  {}", exposure.access.min(cap), exposure.resolved_pattern()).unwrap();
	}

	text
}

/// Whether a path component has glob wildcards.
fn has_wildcard(name: &[u8]) -> bool {
	name.iter().any(|byte| matches!(byte, b'*' | b'?' | b'['))
}

/// Whether a directory above `path` (resolved) matches a pattern exposed for writing.
fn in_write_area(exposed: &[Exposure], path: &Path) -> bool {
	path.ancestors().skip(1).any(|directory| {
		exposed
			.iter()
			.any(|exposure| exposure.access == Access::Write && exposure.matches(directory))
	})
}

fn list_or_none(items: &[String]) -> String {
	match items.is_empty() {
		true => "none".to_owned(),
		false => items.join(", "),
	}
}

/// The load options of an attached source: its manifest, with the server's options that are not about the server's
/// own workspace (cargo's configuration, the target platform, and `cfg`s).
pub(crate) fn load_options(own: &LoadOptions, manifest: &Path) -> LoadOptions {
	LoadOptions {
		manifest_path: Some(manifest.to_path_buf()),
		target: own.target.clone(),
		cfgs: own.cfgs.clone(),
		exact_features: own.exact_features,
		offline: own.offline,
		locked: own.locked,
		frozen: own.frozen,
		config: own.config.clone(),
		silent: own.silent,
		..LoadOptions::default()
	}
}

/// The canonical `Cargo.toml` a client names: its path, or its directory's (relative to the working directory).
pub(crate) fn manifest(text: &str) -> Result<PathBuf, String> {
	let text = text.trim();

	if text.is_empty() {
		return Err("`manifest_path` is empty: give the path of a Cargo.toml".to_owned());
	}

	let path = Path::new(text);
	let path = match path.is_dir() {
		true => path.join("Cargo.toml"),
		false => path.to_path_buf(),
	};
	let manifest = std::fs::canonicalize(&path)
		.map(without_verbatim_prefix)
		.map_err(|error| format!("{}: {error}", path.display()))?;

	match manifest.is_file() && manifest.file_name().is_some_and(|name| name == "Cargo.toml") {
		true => Ok(manifest),
		false => Err(format!("{} is not a Cargo.toml (nor a directory with one)", manifest.display())),
	}
}

fn normalized(mut base: PathBuf, components: &[Component<'_>]) -> PathBuf {
	for component in components {
		match component {
			Component::CurDir => {}

			Component::ParentDir => {
				base.pop();
			}

			component => base.push(component),
		}
	}

	base
}

/// `path` (absolute) with symbolic links resolved: canonicalized as far as it exists, with the rest appended (and
/// `.` and `..` in it resolved textually, which is exact where nothing exists).
pub(crate) fn resolve(path: &Path) -> PathBuf {
	let components: Vec<Component<'_>> = path.components().collect();

	for split in (1..=components.len()).rev() {
		let prefix: PathBuf = components[..split].iter().collect();

		if let Ok(canonical) = std::fs::canonicalize(&prefix) {
			return normalized(without_verbatim_prefix(canonical), &components[split..]);
		}
	}

	normalized(PathBuf::new(), &components)
}

/// The error for a name that no source has.
pub(crate) fn unknown(name: &str, sources: &Sources) -> String {
	let mut message = format!("no source is attached as `{name}`");

	match sources.is_empty() {
		true => message.push_str("\nhint: attach one with `attach_source`"),
		false => {
			let names: Vec<String> = sources.keys().map(|name| format!("`{name}`")).collect();

			write!(message, "\nhint: the attached sources are {}", names.join(", ")).unwrap();
		}
	}

	message
}

/// Whether the editing tools may write to the file `path`: whether a directory above it matches a pattern exposed for
/// writing, with symbolic links resolved, including one at `path` itself (writing to a link writes its target).
pub(crate) fn writable(exposed: &[Exposure], path: &Path) -> bool {
	in_write_area(exposed, &resolve(path))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A temporary directory tree, deleted on drop.
	struct Tree(PathBuf);

	impl Tree {
		fn new(name: &str, directories: &[&str]) -> Self {
			let root = std::env::temp_dir().join(format!("rscode-sources-{name}-{}", std::process::id()));
			let _ = std::fs::remove_dir_all(&root);

			std::fs::create_dir_all(&root).unwrap();

			for directory in directories {
				std::fs::create_dir_all(root.join(directory)).unwrap();
			}

			// resolved like the paths the functions compare: on Windows, `C:\...` rather than `\\?\C:\...`
			Self(resolve(&root))
		}

		fn exposure(&self, access: Access, pattern: &str) -> Exposure {
			Exposure::new(access, self.path(pattern).to_str().unwrap()).unwrap()
		}

		fn path(&self, path: &str) -> PathBuf {
			self.0.join(path)
		}
	}

	impl Drop for Tree {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}

	/// Moving or deleting a link affects the link, wherever it leads.
	#[cfg(unix)]
	#[test]
	fn entries_are_not_followed() {
		let tree = Tree::new("entries", &["project/src", "refs/log/src"]);
		let exposed = [tree.exposure(Access::Write, "project"), tree.exposure(Access::Read, "refs/*")];

		std::fs::write(tree.path("project/src/real.rs"), "").unwrap();
		std::os::unix::fs::symlink(tree.path("project/src/real.rs"), tree.path("refs/log/src/link.rs")).unwrap();
		std::os::unix::fs::symlink(tree.path("refs/log/src"), tree.path("project/src/outside")).unwrap();

		// a link outside, leading in: its target may be written, but the link may not be deleted
		assert!(writable(&exposed, &tree.path("refs/log/src/link.rs")));
		assert!(!entry_writable(&exposed, &tree.path("refs/log/src/link.rs")));

		// a link inside, leading out: the link may be deleted, but not what it leads to
		assert!(entry_writable(&exposed, &tree.path("project/src/outside")));
		assert!(!writable(&exposed, &tree.path("project/src/outside")));
		assert!(!entry_writable(&exposed, &tree.path("project/src/outside/lib.rs")));

		let mut edits = EditSet::new();

		edits.delete_path(tree.path("refs/log/src/link.rs"));

		let scope = WriteScope::Exposed {
			source: "project".to_owned(),
			exposed: exposed.into(),
		};
		let error = scope.check(&tree.path("project"), &edits).unwrap_err();

		assert!(error.contains("link.rs, which is not in a directory exposed for writing"), "{error}");
	}

	#[test]
	fn manifests_are_found_in_directories() {
		let tree = Tree::new("manifests", &["package/src"]);

		std::fs::write(tree.path("package/Cargo.toml"), "").unwrap();

		let manifest = tree.path("package/Cargo.toml");

		assert_eq!(self::manifest(tree.path("package").to_str().unwrap()), Ok(manifest.clone()));
		assert_eq!(self::manifest(manifest.to_str().unwrap()), Ok(manifest));
		assert!(self::manifest(tree.path("package/src").to_str().unwrap()).is_err());
		assert!(self::manifest(tree.path("missing/Cargo.toml").to_str().unwrap()).is_err());
		assert!(self::manifest(" ").unwrap_err().contains("empty"));
	}

	#[test]
	fn names() {
		for name in ["sdk", "rust_source_sdk_2013", "refs.log-2"] {
			assert_eq!(check_name(name), Ok(()));
		}

		for name in ["", "a b", "a/b", "ü", &"a".repeat(MAX_NAME_LEN + 1)] {
			assert!(check_name(name).is_err(), "{name}");
		}
	}

	#[test]
	fn parses_access_and_pattern() {
		let exposure: Exposure = "write=/abs/project".parse().unwrap();

		assert_eq!(exposure.access(), Access::Write);
		assert_eq!(exposure.pattern(), "/abs/project");
		assert_eq!(exposure.to_string(), "write=/abs/project");
		assert_eq!("read=/abs/refs/*".parse::<Exposure>().unwrap().access(), Access::Read);

		for (text, message) in [
			("/abs/project", "is not `read=<directory glob>`"),
			("rw=/abs/project", "unknown access `rw`"),
			("read=", "the pattern is empty"),
			("read=/abs/a**", "recursive wildcards must form a single path component"),
			("read=/abs/[a", "invalid range pattern"),
			("read=/abs/*/../etc", "cannot follow a wildcard"),
		] {
			let error = text.parse::<Exposure>().unwrap_err().to_string();

			assert!(error.contains(message), "{text}: {error}");
		}
	}

	#[test]
	fn relative_patterns_are_absolute() {
		let exposure = Exposure::new(Access::Read, "references/*").unwrap();
		let expected = resolve(&std::env::current_dir().unwrap().join("references"));

		assert!(exposure.matches(&expected.join("log")), "{}", exposure.resolved_pattern());
		assert!(Path::new(exposure.resolved_pattern()).is_absolute());
	}

	/// Resolving the literal prefix can bring in wildcard characters that were not written: they match literally.
	#[cfg(unix)]
	#[test]
	fn resolved_prefixes_are_escaped() {
		let tree = Tree::new("escaped", &["real[1]/project", "real1/project"]);

		std::os::unix::fs::symlink(tree.path("real[1]"), tree.path("link")).unwrap();

		let exposure = tree.exposure(Access::Read, "link/*");

		assert!(exposure.matches(&tree.path("real[1]/project")), "{}", exposure.resolved_pattern());
		assert!(!exposure.matches(&tree.path("real1/project")));
	}

	#[cfg(unix)]
	#[test]
	fn symbolic_links_are_resolved() {
		let tree = Tree::new("links", &["real/refs/log", "other"]);

		std::os::unix::fs::symlink(tree.path("real"), tree.path("link")).unwrap();
		std::os::unix::fs::symlink(tree.path("other"), tree.path("real/refs/escape")).unwrap();

		// a pattern through a link matches the real directories
		let exposure = tree.exposure(Access::Write, "link/refs/*");

		assert!(exposure.matches(&tree.path("real/refs/log")));
		assert!(writable(std::slice::from_ref(&exposure), &tree.path("real/refs/log/src/lib.rs")));

		// a link in an exposed directory does not expose what it leads to
		assert!(!writable(&[exposure], &tree.path("real/refs/escape/lib.rs")));
	}

	#[test]
	fn wildcards() {
		let tree = Tree::new("wildcards", &[]);
		let one = tree.exposure(Access::Read, "refs/*");
		let any = tree.exposure(Access::Read, "refs/misc/**");
		let exact = tree.exposure(Access::Write, "project");

		assert!(one.matches(&tree.path("refs/log")));
		assert!(!one.matches(&tree.path("refs")));
		assert!(!one.matches(&tree.path("refs/misc/log")));
		assert!(!one.matches(&tree.path("refs/.hidden")));

		assert!(any.matches(&tree.path("refs/misc")));
		assert!(any.matches(&tree.path("refs/misc/a/b")));
		assert!(!any.matches(&tree.path("refs/misc/.git")));
		assert!(!any.matches(&tree.path("refs/miscellaneous")));

		assert!(exact.matches(&tree.path("project")));
		assert!(!exact.matches(&tree.path("project/crates/a")));
		assert!(!exact.matches(&tree.path("project2")));
	}

	#[test]
	fn write_takes_precedence() {
		let tree = Tree::new("precedence", &[]);
		let exposed = [tree.exposure(Access::Read, "**"), tree.exposure(Access::Write, "project")];

		assert_eq!(access(&exposed, &tree.path("project")), Some(Access::Write));
		assert_eq!(access(&exposed, &tree.path("refs")), Some(Access::Read));
		assert_eq!(access(&exposed[1..], &tree.path("refs")), None);
	}

	#[test]
	fn writing_needs_a_directory_exposed_for_writing_above() {
		let tree = Tree::new("writable", &["project/src", "refs/log/src"]);
		let exposed = [tree.exposure(Access::Write, "project"), tree.exposure(Access::Read, "refs/*")];

		assert!(writable(&exposed, &tree.path("project/src/lib.rs")));
		assert!(writable(&exposed, &tree.path("project/src/new/deep.rs")));
		assert!(writable(&exposed, &tree.path("project/Cargo.toml")));
		assert!(!writable(&exposed, &tree.path("project")));
		assert!(!writable(&exposed, &tree.path("refs/log/src/lib.rs")));
		assert!(!writable(&exposed, &tree.path("project/../refs/log/src/lib.rs")));
		assert!(!writable(&exposed, &tree.path("elsewhere.rs")));
	}
}
