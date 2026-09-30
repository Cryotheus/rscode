//! Loading a crate: parsing its files, following `mod` declarations, and extracting items into the model.
//!
//! # Module files
//!
//! Module files are found the way rustc finds them. Every module has a directory that `#[path]` attributes are
//! relative to: the directory of its file for the crate root and out-of-line modules, and the enclosing module's
//! child directory plus the module's name for inline modules. A module loaded from a non-`mod.rs` file (`a.rs`,
//! declared as `mod a;`) keeps the child modules it declares in a subdirectory named after it (`a/b.rs`); the crate
//! root, `mod.rs` files, and files loaded with `#[path]` keep them next to themselves.
//!
//! - `mod name;` loads `name.rs`, or else `name/mod.rs`. When both exist rustc reports an error; here `name.rs` is
//!   used (with an error diagnostic).
//! - `#[path = "p"] mod name;` loads `p`, relative to the module's directory (absolute paths are used as they are).
//!   On an inline module, `#[path = "p"]` names the directory of the module's children instead.
//! - Of several `#[path]` attributes, the first applies, as in rustc. `#[cfg_attr(pred, path = "p")]` counts when
//!   `pred` is true, and also (with a warning) when it cannot be evaluated.
//! - Modules whose `cfg` is definitely false are loaded too; problems with their files are warnings rather than
//!   errors. A module that would include one of its ancestors' files (a `#[path]` cycle) is not loaded.
//!
//! Paths are absolute and normalized: `.` components are removed, and `..` components remove the preceding
//! component, unless that is a symbolic link: like the operating system (and so rustc), `..` then leads to the parent
//! of the link's target, and the path continues from the target's canonical path. Other symbolic links are kept, so
//! a file reached through different symbolic links is loaded more than once.
//!
//! # Items
//!
//! The items of modules, `impl` blocks, traits, and `extern` blocks, the declarations of `thread_local!` invocations
//! in modules (as [`ItemKind::Static`] children of the [`ItemKind::MacroCall`], see [`thread_local`]), the variants
//! of enums, and the leaves of `use` trees become [`ItemData`]; items inside function bodies and other expressions do
//! not.
//!
//! Syntax that syn only tokenizes (`Verbatim` items, e.g. `fn f();`, `const trait T {}`, `impl(crate) trait T {}`,
//! `macro m() {}`, `static S: u8;`) is classified by its leading tokens. Traits and `impl` blocks with modifiers syn
//! does not support are re-parsed without them, so they get their details and associated items. Anything
//! unrecognized becomes a macro call ([`ItemKind::MacroCall`], [`ItemKind::AssocMacro`], or
//! [`ItemKind::ForeignMacro`]) whose [`ItemDetail::Macro`] path is empty.
//!
//! All parsing happens on the thread that calls [`load_crate`], and `proc_macro2`'s thread-local source map keeps a
//! copy of every parsed file (see [`crate::source::ParsedFile`]); [`Workspace::load_crate`](crate::Workspace::load_crate)
//! runs it on a thread of its own.

mod attrs;
mod items;
mod syntax;
pub(crate) mod thread_local;
mod verbatim;

use crate::CfgExpr;
use crate::Tristate;
use crate::model::Crate;
use crate::model::CrateId;
use crate::model::CrateSpec;
use crate::model::Diagnostic;
use crate::model::ItemAttrs;
use crate::model::ItemData;
use crate::model::ItemDetail;
use crate::model::ItemKind;
use crate::model::ModuleInfo;
use crate::model::Severity;
use crate::model::Visibility;
use crate::source::FileId;
use crate::source::LineCol;
use crate::source::ParsedFile;
use crate::source::SourceFile;
use crate::source::TextRange;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use smol_str::SmolStr;
use std::collections::HashMap;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::path::Prefix;
use std::sync::Arc;

/// Index of the crate root module in [`Crate::items`].
const ROOT: u32 = 0;

/// The kind of item whose body an item is in.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Container {
	Module,
	Impl { of_trait: bool },
	Trait,
	Extern,
}

impl Container {
	fn const_kind(self) -> Option<ItemKind> {
		match self {
			Self::Module => Some(ItemKind::Const),
			Self::Impl { .. } | Self::Trait => Some(ItemKind::AssocConst),
			Self::Extern => None,
		}
	}

	/// The visibility of items without a visibility keyword.
	fn default_vis(self) -> Visibility {
		match self {
			Self::Module | Self::Extern | Self::Impl { of_trait: false } => Visibility::Private,
			Self::Impl { of_trait: true } | Self::Trait => Visibility::Inherited,
		}
	}

	fn fn_kind(self) -> ItemKind {
		match self {
			Self::Module => ItemKind::Fn,
			Self::Impl { .. } | Self::Trait => ItemKind::AssocFn,
			Self::Extern => ItemKind::ForeignFn,
		}
	}

	/// The kind of a macro invocation (other than a `macro_rules!` definition).
	fn macro_kind(self) -> ItemKind {
		match self {
			Self::Module => ItemKind::MacroCall,
			Self::Impl { .. } | Self::Trait => ItemKind::AssocMacro,
			Self::Extern => ItemKind::ForeignMacro,
		}
	}

	fn static_kind(self) -> Option<ItemKind> {
		match self {
			Self::Module => Some(ItemKind::Static),
			Self::Extern => Some(ItemKind::ForeignStatic),
			Self::Impl { .. } | Self::Trait => None,
		}
	}

	fn type_kind(self) -> ItemKind {
		match self {
			Self::Module => ItemKind::TypeAlias,
			Self::Impl { .. } | Self::Trait => ItemKind::AssocType,
			Self::Extern => ItemKind::ForeignType,
		}
	}
}

/// The state of loading one crate.
struct Loader {
	spec: CrateSpec,
	files: Vec<SourceFile>,
	file_ids: HashMap<PathBuf, FileId>,
	items: Vec<ItemData>,
	diagnostics: Vec<Diagnostic>,

	/// The files of the out-of-line modules being loaded, outermost first (to detect `#[path]` cycles).
	file_stack: Vec<PathBuf>,
}

impl Loader {
	fn new(spec: CrateSpec) -> Self {
		Self {
			spec,
			files: Vec::new(),
			file_ids: HashMap::new(),
			items: Vec::new(),
			diagnostics: Vec::new(),
			file_stack: Vec::new(),
		}
	}

	fn add_file(&mut self, path: PathBuf, text: String) -> FileId {
		let id = FileId(self.files.len() as u32);

		self.file_ids.insert(path.clone(), id);
		self.files.push(SourceFile::new(path, text));
		id
	}

	fn diagnostic_at(&mut self, severity: Severity, file: FileId, offset: usize, message: String) {
		let source = &self.files[file.index()];

		self.diagnostics.push(Diagnostic {
			severity,
			message,
			file: Some(source.path().to_owned()),
			location: Some(source.line_col(offset)),
		});
	}

	fn eval(&self, cfg: Option<&CfgExpr>) -> Tristate {
		cfg.map_or(Tristate::True, |cfg| self.spec.cfg.eval(cfg))
	}

	/// Parses the file of a module (the crate root or an out-of-line module) and adds its items to the module.
	///
	/// `active`: whether the module is active, as far as known before reading its inner attributes.
	fn load_module_file(&mut self, module: u32, file: FileId, dir: &ModDir, active: Tristate) {
		let text = Arc::clone(self.files[file.index()].shared_text());

		match ParsedFile::parse(&text) {
			Ok(parsed) => Walker::new(self, &parsed, file).module_file(module, dir, active),

			Err(error) => {
				let source = &self.files[file.index()];
				let message = format!("cannot parse file: {error}");
				let location = parse_error_location(source, &error);
				let path = source.path().to_owned();

				self.set_load_error(module, message.clone());

				self.diagnostics.push(Diagnostic {
					severity: severity(active),
					message,
					file: Some(path),
					location: Some(location),
				});
			}
		}
	}

	/// Loads the crate root file (always `FileId(0)`, empty if unreadable) and, through it, every module.
	fn load_root(&mut self) {
		let path = normalize_path(&self.spec.root);

		let (text, error) = match std::fs::read_to_string(&path) {
			Ok(text) => (text, None),
			Err(error) => (String::new(), Some(format!("cannot read the crate root: {error}"))),
		};

		let file = self.add_file(path.clone(), text);
		let len = self.files[file.index()].text().len();

		self.items.push(ItemData {
			kind: ItemKind::Module,
			name: Some(self.spec.name.clone()),
			parent: None,
			children: Vec::new(),
			file,
			range: TextRange::new(0, len),
			name_range: None,
			vis: Visibility::Public,
			cfg: None,
			attrs: ItemAttrs::default(),
			detail: ItemDetail::Module(ModuleInfo {
				inline: false,
				body: None,
				file: Some(file),
				file_path: Some(path.clone()),
				dir_owner: true,
				load_error: error.clone(),
			}),
		});

		if let Some(message) = error {
			self.diagnostics.push(Diagnostic {
				severity: Severity::Error,
				message,
				file: Some(path),
				location: None,
			});

			return;
		}

		let dir = ModDir::of_file(&path, None);

		self.file_stack.push(path);
		self.load_module_file(ROOT, file, &dir, Tristate::True);
		self.file_stack.pop();
	}

	/// Adds an item as the last child of `parent`, returning its index.
	fn push(&mut self, parent: u32, mut item: ItemData) -> u32 {
		let index = self.items.len() as u32;

		item.parent = Some(parent);
		self.items.push(item);
		self.items[parent as usize].children.push(index);
		index
	}

	/// Reads a file, or returns its id if the crate already loaded it.
	fn read_file(&mut self, path: &Path) -> std::io::Result<FileId> {
		if let Some(&id) = self.file_ids.get(path) {
			return Ok(id);
		}

		let text = std::fs::read_to_string(path)?;

		Ok(self.add_file(path.to_owned(), text))
	}

	fn set_load_error(&mut self, module: u32, error: String) {
		if let ItemDetail::Module(info) = &mut self.items[module as usize].detail {
			info.load_error = Some(error);
		}
	}

	fn set_module_file(&mut self, module: u32, file: FileId) {
		if let ItemDetail::Module(info) = &mut self.items[module as usize].detail {
			info.file = Some(file);
		}
	}
}

/// Where the files of a module's out-of-line child modules are (rustc's module `dir_path` and `DirOwnership`).
#[derive(Debug, Clone, Eq, PartialEq)]
struct ModDir {
	/// The directory `#[path]` attributes are relative to.
	dir: PathBuf,

	/// For a module loaded from a non-`mod.rs` file (`a.rs`): its name. The files of its child modules are in the
	/// subdirectory of that name (`a/b.rs`).
	relative: Option<SmolStr>,
}

impl ModDir {
	/// The module directory of a module loaded from `file`.
	fn of_file(file: &Path, relative: Option<SmolStr>) -> Self {
		Self {
			dir: file.parent().map(Path::to_owned).unwrap_or_default(),
			relative,
		}
	}

	/// The directory containing the files of child modules declared without `#[path]`.
	fn child_dir(&self) -> PathBuf {
		match &self.relative {
			Some(name) => self.dir.join(name.as_str()),
			None => self.dir.clone(),
		}
	}

	/// The module directory of an inline child module `mod name { .. }`.
	fn inline(&self, name: &str, path_attr: Option<&str>) -> Self {
		let dir = match path_attr {
			Some(path) => normalize_path(&self.dir.join(path)),
			None => self.child_dir().join(name),
		};

		Self { dir, relative: None }
	}

	/// The file of an out-of-line child module `mod name;`.
	fn resolve(&self, name: &str, path_attr: Option<&str>) -> ModFile {
		if let Some(path) = path_attr {
			let path = normalize_path(&self.dir.join(path));

			return ModFile {
				exists: path.is_file(),
				dir: ModDir::of_file(&path, None),
				path,
				mod_rs: None,
				ambiguous: false,
			};
		}

		let child_dir = self.child_dir();
		let file = child_dir.join(format!("{name}.rs"));
		let mod_rs = child_dir.join(name).join("mod.rs");

		match (file.is_file(), mod_rs.is_file()) {
			(false, true) => ModFile {
				exists: true,
				dir: ModDir::of_file(&mod_rs, None),
				path: mod_rs,
				mod_rs: None,
				ambiguous: false,
			},

			(exists, ambiguous) => ModFile {
				exists,
				dir: ModDir::of_file(&file, Some(name.into())),
				path: file,
				mod_rs: Some(mod_rs),
				ambiguous,
			},
		}
	}
}

/// The file an out-of-line module resolves to.
#[derive(Debug, Clone, Eq, PartialEq)]
struct ModFile {
	path: PathBuf,
	exists: bool,

	/// The module directory of the module loaded from the file.
	dir: ModDir,

	/// When `path` is the default `name.rs`: the other candidate, `name/mod.rs`.
	mod_rs: Option<PathBuf>,

	/// Whether both `name.rs` and `name/mod.rs` exist (which rustc rejects).
	ambiguous: bool,
}

/// Extracts the items of one parsed file into the crate being loaded.
struct Walker<'l, 'p, 't> {
	loader: &'l mut Loader,
	parsed: &'p ParsedFile<'t>,
	file: FileId,
}

impl<'l, 'p, 't> Walker<'l, 'p, 't> {
	fn new(loader: &'l mut Loader, parsed: &'p ParsedFile<'t>, file: FileId) -> Self {
		Self { loader, parsed, file }
	}

	/// Loads the file of an out-of-line module (`mod name;`) whose item was just added.
	///
	/// `declaration`: the offset of the module's name, for diagnostics.
	fn load_out_of_line(&mut self, module: u32, name: &str, file: ModFile, declaration: usize, active: Tristate) {
		let severity = severity(active);

		if file.ambiguous
			&& let Some(mod_rs) = &file.mod_rs
		{
			let message = format!(
				"file for module `{name}` found at both `{}` and `{}`; using the former",
				file.path.display(),
				mod_rs.display(),
			);

			self.loader.diagnostic_at(severity, self.file, declaration, message);
		}

		let error = if !file.exists {
			match &file.mod_rs {
				Some(mod_rs) => format!(
					"file not found for module `{name}`: neither `{}` nor `{}` exists",
					file.path.display(),
					mod_rs.display(),
				),

				None => format!("file not found for module `{name}`: `{}` does not exist", file.path.display()),
			}
		} else if let Some(start) = self.loader.file_stack.iter().position(|path| *path == file.path) {
			let chain: Vec<String> = self.loader.file_stack[start..]
				.iter()
				.chain([&file.path])
				.map(|path| format!("`{}`", path.display()))
				.collect();

			format!("circular modules: {}", chain.join(" -> "))
		} else {
			match self.loader.read_file(&file.path) {
				Ok(id) => {
					self.loader.set_module_file(module, id);
					self.loader.file_stack.push(file.path);
					self.loader.load_module_file(module, id, &file.dir, active);
					self.loader.file_stack.pop();

					return;
				}

				Err(error) => format!("cannot read file `{}` of module `{name}`: {error}", file.path.display()),
			}
		};

		self.loader.set_load_error(module, error.clone());
		self.loader.diagnostic_at(severity, self.file, declaration, error);
	}

	/// Adds the inner attributes and the items of the parsed file to `module`.
	fn module_file(&mut self, module: u32, dir: &ModDir, active: Tristate) {
		let parsed = self.parsed;
		let inner = self.attributes(&parsed.file.attrs, true);
		let inner_cfg = CfgExpr::all(inner.cfgs);
		let active = active.and(self.loader.eval(inner_cfg.as_ref()));
		let item = &mut self.loader.items[module as usize];

		item.cfg = CfgExpr::all(item.cfg.take().into_iter().chain(inner_cfg));
		item.attrs.doc_hidden |= inner.attrs.doc_hidden;
		item.attrs.macro_export |= inner.attrs.macro_export;
		item.attrs.macro_use |= inner.attrs.macro_use;
		item.attrs.test |= inner.attrs.test;

		self.module_items(module, &parsed.file.items, dir, active);
	}

	fn warning(&mut self, offset: usize, message: String) {
		self.loader.diagnostic_at(Severity::Warning, self.file, offset, message);
	}
}

/// Whether a file name means the same in a path without `\\?\`: Windows takes names like `NUL` and `nul.txt` for
/// devices, drops the `.` and ` ` that names end with, and does not allow some characters in them.
fn is_plain_name(name: &str) -> bool {
	// (the name before its first `.`, without the spaces it ends with: `nul .tar.gz` names `NUL` too)
	let base = name.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_uppercase();
	let numbered = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "\u{b9}", "\u{b2}", "\u{b3}"];
	let device = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
		|| ((base.starts_with("COM") || base.starts_with("LPT")) && numbered.contains(&&base[3..]));
	let invalid = |char: char| char < ' ' || "<>:\"/\\|?*".contains(char);

	!(device || name.is_empty() || name.ends_with(['.', ' ']) || name.contains(invalid))
}

/// The start of the last token of `text` (of the closing delimiter, for a group), or the end of its last
/// non-whitespace character if it cannot be tokenized.
fn last_token_start(text: &str) -> usize {
	let bom = if text.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
	let tokens = text[bom..].parse::<TokenStream>().ok();

	let span = tokens.and_then(|tokens| tokens.into_iter().last()).map(|token| match token {
		TokenTree::Group(group) => group.span_close(),
		token => token.span(),
	});

	span.map_or_else(|| text.trim_end().len(), |span| bom + span.byte_range().start)
}

/// Loads a crate from its spec. Never fails: problems become [`Crate::diagnostics`](crate::Crate::diagnostics).
pub(crate) fn load_crate(id: CrateId, spec: CrateSpec) -> Crate {
	let mut loader = Loader::new(spec);

	loader.load_root();

	Crate {
		id,
		spec: loader.spec,
		files: loader.files,
		items: loader.items,
		diagnostics: loader.diagnostics,
	}
}

/// An absolute, normalized path naming the same file as `path`: `.` components are removed, and `..` components
/// remove the preceding component (see [`parent`]). Symbolic links are only resolved where `..` needs it.
fn normalize_path(path: &Path) -> PathBuf {
	let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
	let mut normalized = PathBuf::new();

	for component in absolute.components() {
		match component {
			Component::CurDir => {}
			Component::ParentDir => parent(&mut normalized),
			other => normalized.push(other.as_os_str()),
		}
	}

	normalized
}

/// Applies a `..` component to a normalized path. The operating system resolves `..` after a symbolic link from the
/// link's target, not lexically, so for a symbolic link this is the parent of its target's canonical path.
fn parent(path: &mut PathBuf) {
	match path.components().next_back() {
		Some(Component::Normal(_)) => {
			if path.is_symlink()
				&& let Ok(target) = std::fs::canonicalize(&path)
			{
				*path = without_verbatim_prefix(target);
			}

			path.pop();
		}

		// `/..` is `/`
		Some(Component::RootDir | Component::Prefix(_)) => {}

		_ => path.push(".."),
	}
}

/// The location of a parse error of a file. syn places errors at the end of the file at the call site (the start of
/// the file); they are placed at the last token instead, as rustc does.
fn parse_error_location(source: &SourceFile, error: &syn::Error) -> LineCol {
	let at_end = error.span().byte_range() == (0..0) && error.to_string().starts_with("unexpected end of input");

	if at_end {
		source.line_col(last_token_start(source.text()))
	} else {
		source.error_location(error)
	}
}

/// `path` without its `\\?\` (see [`without_verbatim_prefix`]), if it has one it can do without.
fn plain_form(path: &Path) -> Option<PathBuf> {
	let mut components = path.components();

	let Some(Component::Prefix(prefix)) = components.next() else {
		return None;
	};

	let prefix = match prefix.kind() {
		Prefix::VerbatimDisk(disk) => format!("{}:", char::from(disk)),
		Prefix::VerbatimUNC(server, share) => format!(r"\\{}\{}", server.to_str()?, share.to_str()?),
		_ => return None,
	};
	let mut names = Vec::new();

	for component in components {
		match component {
			Component::RootDir => {}
			Component::Normal(name) => names.push(name.to_str().filter(|name| is_plain_name(name))?),
			_ => return None,
		}
	}

	Some(PathBuf::from(format!(r"{prefix}\{}", names.join(r"\"))))
}

/// Problems with a module that is definitely inactive are only warnings: rustc never looks at it.
fn severity(active: Tristate) -> Severity {
	match active {
		Tristate::False => Severity::Warning,
		Tristate::True | Tristate::Unknown => Severity::Error,
	}
}

/// `path` without the `\\?\` that canonicalizing gives Windows paths, as they are usually written: `\\?\C:\a` as
/// `C:\a`, and `\\?\UNC\server\share\a` (on a network share) as `\\server\share\a`. Paths that mean something else
/// without it keep it (see [`is_plain_name`]).
pub(crate) fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
	plain_form(&path).unwrap_or(path)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// An absolute path on every platform: `/c` on Unix, `C:\c` (on the current drive) on Windows, where a path
	/// starting with `/` has no drive.
	fn abs(path: &str) -> PathBuf {
		std::path::absolute(path).unwrap()
	}

	#[test]
	fn module_directories_follow_rustc() {
		let root = ModDir::of_file(&abs("/c/src/lib.rs"), None);

		assert_eq!(root.child_dir(), abs("/c/src"));
		assert_eq!(root.inline("m", None).child_dir(), abs("/c/src/m"));
		assert_eq!(root.inline("m", Some("p")).child_dir(), abs("/c/src/p"));

		// `src/a.rs`, loaded by `mod a;`
		let a = ModDir::of_file(&abs("/c/src/a.rs"), Some("a".into()));

		assert_eq!(a.child_dir(), abs("/c/src/a"));
		assert_eq!(
			a.inline("inl", None),
			ModDir {
				dir: abs("/c/src/a/inl"),
				relative: None
			}
		);
		assert_eq!(
			a.inline("inl", Some("x")),
			ModDir {
				dir: abs("/c/src/x"),
				relative: None
			}
		);

		let missing = a.resolve("b", None);

		assert_eq!(missing.path, abs("/c/src/a/b.rs"));
		assert_eq!(
			missing.dir,
			ModDir {
				dir: abs("/c/src/a"),
				relative: Some("b".into())
			}
		);
		assert!(!missing.exists);
		assert_eq!(a.resolve("c", Some("c.rs")).path, abs("/c/src/c.rs"));
		assert_eq!(
			a.resolve("c", Some("c.rs")).dir,
			ModDir {
				dir: abs("/c/src"),
				relative: None
			}
		);
		assert_eq!(a.resolve("c", Some("/abs/c.rs")).path, abs("/abs/c.rs"));
		assert_eq!(a.resolve("c", Some("../up.rs")).path, abs("/c/up.rs"));
	}

	#[test]
	fn normalizes_paths_lexically() {
		assert_eq!(normalize_path(Path::new("/a/./b/../c.rs")), abs("/a/c.rs"));
		assert_eq!(normalize_path(Path::new("/a/b/../../../c")), abs("/c"));
		assert_eq!(normalize_path(Path::new("/a//b/")), abs("/a/b"));
		assert!(normalize_path(Path::new("relative/x.rs")).is_absolute());
		assert!(normalize_path(Path::new("relative/x.rs")).ends_with("relative/x.rs"));
	}

	#[cfg(not(windows))]
	#[test]
	fn paths_have_no_verbatim_prefixes() {
		assert_eq!(without_verbatim_prefix(PathBuf::from(r"/a/\\?\C:")), Path::new(r"/a/\\?\C:"));
	}

	#[test]
	fn plain_names() {
		for name in [
			"lib.rs",
			"a b.rs",
			"console.rs",
			"com10.rs",
			"comx.rs",
			"nul_check.rs",
			".hidden",
			"ünï.rs",
		] {
			assert!(is_plain_name(name), "{name}");
		}

		// devices, names that Windows drops the end of, and names it does not allow
		let devices = ["NUL", "nul.rs", "Con.tar.gz", "aux .rs", "COM1.rs", "lpt\u{b9}", "CONIN$"];

		for name in devices.into_iter().chain(["a.", "a ", ".", "..", "a:b", "a?"]) {
			assert!(!is_plain_name(name), "{name}");
		}
	}

	#[cfg(windows)]
	#[test]
	fn verbatim_prefixes_are_removed_where_they_change_nothing() {
		let plain = |path: &str| without_verbatim_prefix(PathBuf::from(path)).display().to_string();

		assert_eq!(plain(r"\\?\C:\a\b.rs"), r"C:\a\b.rs");
		assert_eq!(plain(r"\\?\c:\"), r"C:\");
		assert_eq!(plain(r"\\?\UNC\server\share\a\b.rs"), r"\\server\share\a\b.rs");
		assert_eq!(plain(r"\\?\UNC\server\share"), r"\\server\share\");

		// paths without it, and paths that mean something else without it
		for path in [
			r"C:\a",
			r"\\server\share\a",
			r"\\?\C:\a\nul.rs",
			r"\\?\C:\a.\b",
			r"\\?\UNC\server\share\aux",
			r"\\?\Volume{a2b8b2b6-0000-0000-0000-100000000000}\a",
			r"\\?\GLOBALROOT\Device\HarddiskVolume1\a",
		] {
			assert_eq!(plain(path), path);
		}
	}
}
