//! The loaded representation of crates: module trees of items with source locations.
//!
//! The model is `Send + Sync` and holds no `syn` values; syntax is re-parsed from [`SourceFile`]s on demand.

use crate::CfgContext;
use crate::CfgExpr;
use crate::Tristate;
use crate::path::Anchor;
use crate::path::ItemPath;
use crate::source::FileId;
use crate::source::LineCol;
use crate::source::SourceFile;
use crate::source::TextRange;
use rscode_fmt::Edition;
use serde::Serialize;
use smol_str::SmolStr;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

/// Index of a [`Crate`] within its [`Workspace`].
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
pub struct CrateId(pub(crate) u32);

impl CrateId {
	/// The index of the crate in [`Workspace::crates`].
	pub fn index(self) -> usize {
		self.0 as usize
	}
}

/// A handle to an item of a crate in a [`Workspace`].
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
pub struct ItemId {
	pub(crate) krate: CrateId,
	pub(crate) index: u32,
}

impl ItemId {
	pub(crate) fn new(krate: CrateId, index: u32) -> Self {
		Self { krate, index }
	}

	/// The root module of a crate.
	pub fn crate_root(krate: CrateId) -> Self {
		Self { krate, index: 0 }
	}

	/// The crate of the item.
	pub fn krate(self) -> CrateId {
		self.krate
	}

	/// The index of the item in [`Crate::items`].
	pub fn index(self) -> usize {
		self.index as usize
	}

	/// Whether the item is the root module of its crate.
	pub fn is_crate_root(self) -> bool {
		self.index == 0
	}
}

/// Index of a [`Package`] within its [`Workspace`].
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
pub struct PackageId(pub(crate) u32);

impl PackageId {
	/// The index of the package in [`Workspace::packages`].
	pub fn index(self) -> usize {
		self.0 as usize
	}
}

/// A cargo package.
#[derive(Debug, Clone, Serialize)]
pub struct Package {
	/// The package name.
	pub name: SmolStr,

	/// The version.
	pub version: String,

	/// Its `Cargo.toml`.
	pub manifest_path: PathBuf,

	/// The `[features]` table.
	pub features: BTreeMap<SmolStr, Vec<SmolStr>>,

	/// Features enabled for this build configuration.
	pub enabled_features: BTreeSet<SmolStr>,

	/// Whether the package is a member of the workspace.
	pub is_member: bool,
}

/// A workspace member that is not selected, and none of whose crates are loaded (see
/// [`Workspace::unloaded_members`]).
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct UnloadedMember {
	/// The package name.
	pub name: SmolStr,

	/// The version.
	pub version: String,

	/// Its `Cargo.toml`.
	pub manifest_path: PathBuf,

	/// The names of its crates (its library first, if it has one), as `::name::…` paths name them.
	pub crate_names: Vec<SmolStr>,
}

/// The kind of a cargo target (crate).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
	/// A library.
	Lib,

	/// A procedural macro library.
	ProcMacro,

	/// A binary.
	Bin,

	/// An example.
	Example,

	/// An integration test.
	Test,

	/// A benchmark.
	Bench,

	/// A build script.
	BuildScript,
}

impl TargetKind {
	/// The kind as cargo names it (`lib`, `proc-macro`, `bin`, `example`, `test`, `bench`, `build-script`).
	pub fn name(self) -> &'static str {
		match self {
			Self::Lib => "lib",
			Self::ProcMacro => "proc-macro",
			Self::Bin => "bin",
			Self::Example => "example",
			Self::Test => "test",
			Self::Bench => "bench",
			Self::BuildScript => "build-script",
		}
	}

	/// Whether other crates can depend on it (a library or procedural macro library).
	pub fn is_lib(self) -> bool {
		matches!(self, Self::Lib | Self::ProcMacro)
	}
}

impl std::fmt::Display for TargetKind {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.name())
	}
}

/// A dependency as seen from a crate: the name usable in paths (`::name`), and the crate it refers to.
#[derive(Debug, Clone, Serialize)]
pub struct Dependency {
	/// The name in the extern prelude (after renames, with `-` replaced by `_`).
	pub name: SmolStr,

	/// The crate name of the dependency's library target.
	pub crate_name: SmolStr,

	/// The dependency's package name, if known.
	pub package: Option<SmolStr>,

	/// The loaded crate, if the dependency is part of the [`Workspace`] (filled by [`Workspace::link`]).
	pub krate: Option<CrateId>,
}

/// Everything needed to load a crate.
#[derive(Debug, Clone)]
pub struct CrateSpec {
	/// The crate name (a valid identifier, e.g. `cargo_rscode`).
	pub name: SmolStr,

	/// The crate root source file.
	pub root: PathBuf,

	/// The kind of the crate.
	pub kind: TargetKind,

	/// The edition, which decides how paths resolve.
	pub edition: Edition,

	/// The package the crate belongs to.
	pub package: Option<PackageId>,

	/// The configuration `cfg` attributes are evaluated against (including enabled features).
	pub cfg: CfgContext,

	/// The extern prelude.
	pub dependencies: Vec<Dependency>,

	/// Whether the crate is part of the user's selection.
	/// Unselected crates are loaded only so that references in them can be found (e.g. by renames).
	pub selected: bool,
}

impl CrateSpec {
	/// A spec for a standalone crate root file, with an empty (all unknown) cfg context.
	pub fn new(name: impl Into<SmolStr>, root: impl Into<PathBuf>) -> Self {
		Self {
			name: name.into(),
			root: root.into(),
			kind: TargetKind::Lib,
			edition: Edition::default(),
			package: None,
			cfg: CfgContext::new(),
			dependencies: Vec::new(),
			selected: true,
		}
	}
}

/// A problem encountered while loading.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
	/// How bad it is.
	pub severity: Severity,

	/// What the problem is.
	pub message: String,

	/// The file it is about, if any.
	pub file: Option<PathBuf>,

	/// Where in the file, if known.
	pub location: Option<LineCol>,
}

/// How bad a [`Diagnostic`] is.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
	/// A problem that does not affect the build as configured, such as in code under a disabled `cfg`.
	Warning,

	/// A problem that rustc would report too, such as a file that does not parse or a missing module file.
	Error,
}

/// A collection of loaded crates.
#[derive(Debug, Clone)]
pub struct Workspace {
	pub(crate) root: PathBuf,
	pub(crate) packages: Vec<Package>,
	pub(crate) crates: Vec<Crate>,
	pub(crate) unloaded_members: Vec<UnloadedMember>,
}

impl Workspace {
	/// An empty workspace. `root` is the directory paths are displayed relative to.
	pub fn new(root: impl Into<PathBuf>) -> Self {
		Self {
			root: root.into(),
			packages: Vec::new(),
			crates: Vec::new(),
			unloaded_members: Vec::new(),
		}
	}

	/// The directory paths are displayed relative to (the cargo workspace root, when loaded by cargo).
	pub fn root(&self) -> &Path {
		&self.root
	}

	/// Adds a package (for [`CrateSpec::package`] to refer to by the returned id).
	pub fn add_package(&mut self, package: Package) -> PackageId {
		self.packages.push(package);

		PackageId(self.packages.len() as u32 - 1)
	}

	/// Loads a crate: parses its root file and every module file it declares.
	///
	/// Problems (unparsable files, missing module files) are recorded as [`Crate::diagnostics`], not errors. The files
	/// are parsed on a thread of its own (see [Threads](crate#threads)).
	pub fn load_crate(&mut self, spec: CrateSpec) -> CrateId {
		let id = CrateId(self.crates.len() as u32);
		let krate = crate::source::isolated(|| crate::load::load_crate(id, spec));

		self.crates.push(krate);
		id
	}

	/// Connects [`Dependency::krate`] of every crate to the loaded crates they name.
	/// Call after loading all crates.
	pub fn link(&mut self) {
		let libs: BTreeMap<(Option<SmolStr>, SmolStr), CrateId> = self
			.crates
			.iter()
			.filter(|krate| krate.kind().is_lib())
			.map(|krate| {
				let package = krate.package().map(|package| self.packages[package.index()].name.clone());

				((package, krate.name().clone()), krate.id())
			})
			.collect();

		for krate in &mut self.crates {
			for dependency in &mut krate.spec.dependencies {
				dependency.krate = libs
					.get(&(dependency.package.clone(), dependency.crate_name.clone()))
					.or_else(|| libs.iter().find(|((_, name), _)| *name == dependency.crate_name).map(|(_, id)| id))
					.copied();
			}
		}
	}

	/// The packages, in the order they were added.
	pub fn packages(&self) -> &[Package] {
		&self.packages
	}

	/// Records a workspace member whose crates are not loaded, so that failing to find something can point to it.
	pub fn add_unloaded_member(&mut self, member: UnloadedMember) {
		self.unloaded_members.push(member);
	}

	/// The workspace members whose crates are not loaded, since they are not selected.
	pub fn unloaded_members(&self) -> &[UnloadedMember] {
		&self.unloaded_members
	}

	/// The unloaded workspace member with a crate named `name` (see [`Workspace::unloaded_members`]).
	pub fn unloaded_member_with_crate(&self, name: &str) -> Option<&UnloadedMember> {
		self.unloaded_members.iter().find(|member| member.crate_names.iter().any(|krate| krate == name))
	}

	/// The unloaded workspace member with the crate that `path` starts from (`::name::…`, or `name::…`, also as the
	/// type of a qualifier), which is where the path would find its item if the member was selected.
	pub fn unloaded_member_of(&self, path: &ItemPath) -> Option<&UnloadedMember> {
		if let Some(qualifier) = &path.qualifier {
			return self.unloaded_member_of(&qualifier.self_ty);
		}

		match path.anchor {
			Anchor::None | Anchor::Global => self.unloaded_member_with_crate(path.segments.first()?),
			_ => None,
		}
	}

	/// A package by id.
	pub fn package(&self, id: PackageId) -> &Package {
		&self.packages[id.index()]
	}

	/// The loaded crates, in the order they were loaded.
	pub fn crates(&self) -> &[Crate] {
		&self.crates
	}

	/// A crate by id.
	pub fn krate(&self, id: CrateId) -> &Crate {
		&self.crates[id.index()]
	}

	/// Crates that are part of the user's selection.
	pub fn selected_crates(&self) -> impl Iterator<Item = &Crate> {
		self.crates.iter().filter(|krate| krate.is_selected())
	}

	/// An item by id.
	pub fn item(&self, id: ItemId) -> &ItemData {
		&self.krate(id.krate).items[id.index()]
	}

	/// The file an item is defined in.
	pub fn file_of(&self, id: ItemId) -> &SourceFile {
		self.krate(id.krate).file(self.item(id).file)
	}

	/// The source text of an item (including outer attributes and doc comments).
	pub fn item_text(&self, id: ItemId) -> &str {
		self.file_of(id).slice(self.item(id).range)
	}

	/// The syntactic parent of an item (`None` for crate roots).
	pub fn parent(&self, id: ItemId) -> Option<ItemId> {
		self.item(id).parent.map(|index| ItemId::new(id.krate, index))
	}

	/// The syntactic children of an item, in source order.
	pub fn children(&self, id: ItemId) -> impl Iterator<Item = ItemId> + '_ {
		self.item(id).children.iter().map(move |&index| ItemId::new(id.krate, index))
	}

	/// The nearest enclosing module (the item itself if it is a module).
	pub fn module_of(&self, id: ItemId) -> ItemId {
		let mut current = id;

		loop {
			if self.item(current).kind == ItemKind::Module {
				return current;
			}

			current = self.parent(current).expect("every item is inside of a module");
		}
	}

	/// Chain of ancestors from the item's parent up to the crate root.
	pub fn ancestors(&self, id: ItemId) -> impl Iterator<Item = ItemId> + '_ {
		std::iter::successors(self.parent(id), move |&id| self.parent(id))
	}

	/// The conjunction of the item's own `cfg` and those of all its ancestors.
	pub fn effective_cfg(&self, id: ItemId) -> Option<CfgExpr> {
		let own = self.item(id).cfg.iter();
		let inherited = self.ancestors(id).filter_map(|ancestor| self.item(ancestor).cfg.as_ref());

		CfgExpr::all(own.chain(inherited).cloned())
	}

	/// Evaluates [`Workspace::effective_cfg`] in the crate's cfg context.
	pub fn is_active(&self, id: ItemId) -> Tristate {
		match self.effective_cfg(id) {
			Some(cfg) => self.krate(id.krate).spec.cfg.eval(&cfg),
			None => Tristate::True,
		}
	}

	/// Displays a path relative to [`Workspace::root`] when possible.
	pub fn display_path<'a>(&self, path: &'a Path) -> &'a Path {
		path.strip_prefix(&self.root).unwrap_or(path)
	}
}

/// A loaded crate (a cargo target).
#[derive(Debug, Clone)]
pub struct Crate {
	pub(crate) id: CrateId,
	pub(crate) spec: CrateSpec,
	pub(crate) files: Vec<SourceFile>,
	pub(crate) items: Vec<ItemData>,
	pub(crate) diagnostics: Vec<Diagnostic>,
}

impl Crate {
	/// The id of the crate in its [`Workspace`].
	pub fn id(&self) -> CrateId {
		self.id
	}

	/// The crate name, as other crates name it in paths.
	pub fn name(&self) -> &SmolStr {
		&self.spec.name
	}

	/// The kind of target.
	pub fn kind(&self) -> TargetKind {
		self.spec.kind
	}

	/// The edition.
	pub fn edition(&self) -> Edition {
		self.spec.edition
	}

	/// The package the crate belongs to, if known.
	pub fn package(&self) -> Option<PackageId> {
		self.spec.package
	}

	/// The spec the crate was loaded from.
	pub fn spec(&self) -> &CrateSpec {
		&self.spec
	}

	/// The configuration its `cfg`s are evaluated against.
	pub fn cfg(&self) -> &CfgContext {
		&self.spec.cfg
	}

	/// Its extern prelude.
	pub fn dependencies(&self) -> &[Dependency] {
		&self.spec.dependencies
	}

	/// Whether the crate is part of the user's selection (see [`CrateSpec::selected`]).
	pub fn is_selected(&self) -> bool {
		self.spec.selected
	}

	/// The crate root module.
	pub fn root_module(&self) -> ItemId {
		ItemId::crate_root(self.id)
	}

	/// The crate root file (empty if it could not be read).
	pub fn root_file(&self) -> &SourceFile {
		&self.files[0]
	}

	/// Every loaded file, the root file first.
	pub fn files(&self) -> &[SourceFile] {
		&self.files
	}

	/// A file by id.
	pub fn file(&self, id: FileId) -> &SourceFile {
		&self.files[id.index()]
	}

	/// The id of the file loaded from `path`, if any.
	pub fn file_id(&self, path: &Path) -> Option<FileId> {
		self.files.iter().position(|file| file.path == path).map(|index| FileId(index as u32))
	}

	/// Every item, the crate root first, with its id.
	pub fn items(&self) -> impl Iterator<Item = (ItemId, &ItemData)> {
		self.items.iter().enumerate().map(|(index, item)| (ItemId::new(self.id, index as u32), item))
	}

	/// An item of this crate by id.
	pub fn item(&self, id: ItemId) -> &ItemData {
		debug_assert_eq!(id.krate, self.id);

		&self.items[id.index()]
	}

	/// The problems found while loading.
	pub fn diagnostics(&self) -> &[Diagnostic] {
		&self.diagnostics
	}
}

/// The kind of an item.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ItemKind {
	/// `mod name;` or `mod name { ... }`, and crate roots.
	Module,

	/// `struct`
	Struct,

	/// `enum`
	Enum,

	/// `union`
	Union,

	/// `trait`
	Trait,

	/// `trait Name = Bounds;`
	TraitAlias,

	/// `type Name = Type;` in a module.
	TypeAlias,

	/// `fn` in a module.
	Fn,

	/// `const` in a module.
	Const,

	/// `static` in a module.
	Static,

	/// `macro_rules! name { ... }`
	MacroRules,

	/// An item-position macro invocation (`foo! { ... }`).
	/// `macro_rules!` definitions are [`ItemKind::MacroRules`] instead.
	MacroCall,

	/// A whole `use` item. Its children are [`ItemKind::Import`]s.
	Use,

	/// One leaf of a `use` tree (`a::B`, `a::B as C`, `a::*`, `a::{self}`). Its range is the leaf's element of the
	/// innermost enclosing group, see [`ItemData::range`].
	Import,

	/// `extern crate name;`
	ExternCrate,

	/// `extern "C" { ... }`. Transparent for paths: its children live in the enclosing module.
	ExternBlock,

	/// `fn` in an `extern` block.
	ForeignFn,

	/// `static` in an `extern` block.
	ForeignStatic,

	/// `type` in an `extern` block.
	ForeignType,

	/// Macro invocation in an `extern` block.
	ForeignMacro,

	/// `impl [Trait for] Type { ... }`.
	Impl,

	/// `fn` in an `impl` block or `trait`.
	AssocFn,

	/// `const` in an `impl` block or `trait`.
	AssocConst,

	/// `type` in an `impl` block or `trait`.
	AssocType,

	/// Macro invocation in an `impl` block or `trait`.
	AssocMacro,

	/// Enum variant.
	Variant,
}

impl ItemKind {
	/// Every kind.
	pub const ALL: &'static [Self] = &[
		Self::Module,
		Self::Struct,
		Self::Enum,
		Self::Union,
		Self::Trait,
		Self::TraitAlias,
		Self::TypeAlias,
		Self::Fn,
		Self::Const,
		Self::Static,
		Self::MacroRules,
		Self::MacroCall,
		Self::Use,
		Self::Import,
		Self::ExternCrate,
		Self::ExternBlock,
		Self::ForeignFn,
		Self::ForeignStatic,
		Self::ForeignType,
		Self::ForeignMacro,
		Self::Impl,
		Self::AssocFn,
		Self::AssocConst,
		Self::AssocType,
		Self::AssocMacro,
		Self::Variant,
	];

	/// A short, kebab-case name (`mod`, `struct`, `fn`, `assoc-fn`, ...).
	pub fn name(self) -> &'static str {
		match self {
			Self::Module => "mod",
			Self::Struct => "struct",
			Self::Enum => "enum",
			Self::Union => "union",
			Self::Trait => "trait",
			Self::TraitAlias => "trait-alias",
			Self::TypeAlias => "type",
			Self::Fn => "fn",
			Self::Const => "const",
			Self::Static => "static",
			Self::MacroRules => "macro-rules",
			Self::MacroCall => "macro-call",
			Self::Use => "use",
			Self::Import => "import",
			Self::ExternCrate => "extern-crate",
			Self::ExternBlock => "extern-block",
			Self::ForeignFn => "foreign-fn",
			Self::ForeignStatic => "foreign-static",
			Self::ForeignType => "foreign-type",
			Self::ForeignMacro => "foreign-macro",
			Self::Impl => "impl",
			Self::AssocFn => "assoc-fn",
			Self::AssocConst => "assoc-const",
			Self::AssocType => "assoc-type",
			Self::AssocMacro => "assoc-macro",
			Self::Variant => "variant",
		}
	}

	/// Whether items of this kind can be named by a path.
	pub fn is_nameable(self) -> bool {
		!matches!(
			self,
			Self::MacroCall | Self::Use | Self::ExternBlock | Self::Impl | Self::ForeignMacro | Self::AssocMacro
		)
	}

	/// Whether the item holds other items (modules, `impl` blocks, `trait`s, `extern` blocks, `enum`s, `use`s).
	pub fn is_container(self) -> bool {
		matches!(self, Self::Module | Self::Impl | Self::Trait | Self::ExternBlock | Self::Enum | Self::Use)
	}

	/// Whether the item is inside of an `impl` block or `trait`.
	pub fn is_associated(self) -> bool {
		matches!(self, Self::AssocFn | Self::AssocConst | Self::AssocType | Self::AssocMacro)
	}
}

impl std::fmt::Display for ItemKind {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.name())
	}
}

impl std::str::FromStr for ItemKind {
	type Err = crate::Error;

	/// Accepts [`ItemKind::name`]s and common aliases (`module`, `function`, `type-alias`, `macro`, ...).
	fn from_str(s: &str) -> Result<Self, Self::Err> {
		let normalized = s.trim().to_ascii_lowercase().replace('_', "-");

		let kind = match normalized.as_str() {
			"module" => Self::Module,
			"function" => Self::Fn,
			"type-alias" | "alias" => Self::TypeAlias,
			"macro" | "macro_rules" => Self::MacroRules,
			"method" | "assoc-function" => Self::AssocFn,
			"extern" => Self::ExternBlock,
			"variant" | "enum-variant" => Self::Variant,
			other => return Self::ALL.iter().copied().find(|kind| kind.name() == other).ok_or_else(|| crate::Error::UnknownItemKind(s.to_owned())),
		};

		Ok(kind)
	}
}

/// Declared visibility of an item.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "path")]
pub enum Visibility {
	/// `pub`
	Public,

	/// `pub(crate)` (or 2015's `crate`)
	Crate,

	/// `pub(super)`
	Super,

	/// `pub(self)`
	SelfModule,

	/// `pub(in path)`
	InPath(PathRef),

	/// No visibility keyword on an item that is private by default.
	Private,

	/// No visibility keyword on an item whose visibility comes from its parent
	/// (trait items, items of trait `impl`s, enum variants).
	Inherited,
}

impl std::fmt::Display for Visibility {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Public => f.write_str("pub"),
			Self::Crate => f.write_str("pub(crate)"),
			Self::Super => f.write_str("pub(super)"),
			Self::SelfModule => f.write_str("pub(self)"),
			Self::InPath(path) => write!(f, "pub(in {path})"),
			Self::Private => f.write_str("private"),
			Self::Inherited => f.write_str("inherited"),
		}
	}
}

/// A path as written in source, with the location of every segment.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
pub struct PathRef {
	/// Whether the path starts with `::`.
	pub leading_colon: bool,

	/// The segments, in order.
	pub segments: Vec<PathSegmentRef>,
}

impl PathRef {
	/// The names of the segments.
	pub fn names(&self) -> impl Iterator<Item = &str> {
		self.segments.iter().map(|segment| segment.name.as_str())
	}

	/// The last segment.
	pub fn last(&self) -> Option<&PathSegmentRef> {
		self.segments.last()
	}
}

impl std::fmt::Display for PathRef {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if self.leading_colon {
			f.write_str("::")?;
		}

		for (index, segment) in self.segments.iter().enumerate() {
			if index > 0 {
				f.write_str("::")?;
			}

			f.write_str(&segment.name)?;
		}

		Ok(())
	}
}

/// One segment of a [`PathRef`].
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
pub struct PathSegmentRef {
	/// The unraw'd identifier, or a keyword (`crate`, `self`, `super`, `Self`).
	pub name: SmolStr,

	/// Location of the whole identifier token (including any `r#`).
	pub range: TextRange,

	/// Whether the segment has generic arguments (`Foo<T>`, `Fn(A) -> B`).
	pub has_arguments: bool,
}

/// A type as written in source, simplified to what name resolution needs.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum TypeRef {
	/// A path type without a qualified self (`Foo`, `a::Foo<T>`).
	Path {
		/// The path.
		path: PathRef,
	},

	/// `&T`, `&mut T`, `*const T`, `*mut T`, `(T)`, `[T]`, `[T; N]`: the referenced type.
	Indirect {
		/// The referenced type.
		inner: Box<TypeRef>,
	},

	/// Anything else (tuples, `dyn Trait`, `impl Trait`, fn pointers, `<T as Trait>::Assoc`, ...).
	Other,
}

impl TypeRef {
	/// The path of the type after stripping indirections, if any.
	pub fn base_path(&self) -> Option<&PathRef> {
		match self {
			Self::Path { path } => Some(path),
			Self::Indirect { inner } => inner.base_path(),
			Self::Other => None,
		}
	}
}

/// Attributes of an item that rscode cares about.
#[derive(Debug, Default, Clone, Eq, PartialEq, Hash, Serialize)]
pub struct ItemAttrs {
	/// `#[doc(hidden)]`
	pub doc_hidden: bool,

	/// `#[macro_export]`
	pub macro_export: bool,

	/// `#[macro_use]`
	pub macro_use: bool,

	/// `#[path = "..."]` (the one in effect after evaluating `cfg_attr`s: the first, as rustc ignores later ones).
	pub path: Option<String>,

	/// `#[test]`
	pub test: bool,

	/// Range covering the outer doc comments / `#[doc = ...]` attributes, if any.
	pub docs: Option<TextRange>,

	/// Byte offset where the item proper starts, after its outer attributes and doc comments.
	pub after_attrs: usize,
}

/// A function signature summary.
#[derive(Debug, Default, Clone, Eq, PartialEq, Hash, Serialize)]
pub struct FnInfo {
	/// The `self` parameter of a method.
	pub receiver: Option<Receiver>,

	/// `const fn`
	pub is_const: bool,

	/// `async fn`
	pub is_async: bool,

	/// `unsafe fn`
	pub is_unsafe: bool,

	/// The body block including its braces, if present (absent for trait methods without a default, and foreign
	/// functions).
	pub body: Option<TextRange>,
}

/// The `self` parameter of a method.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize)]
pub struct Receiver {
	/// `&self` / `&mut self`
	pub reference: bool,

	/// `&mut self` / `mut self`
	pub mutable: bool,

	/// `self: Type`
	pub typed: bool,
}

/// Details of a module.
#[derive(Debug, Default, Clone, Eq, PartialEq, Hash, Serialize)]
pub struct ModuleInfo {
	/// `mod foo { ... }` rather than `mod foo;`.
	pub inline: bool,

	/// Range inside the braces of an inline module.
	pub body: Option<TextRange>,

	/// The loaded file of an out-of-line module (and of the crate root, which always has one: `FileId(0)`, empty
	/// if it could not be read).
	pub file: Option<FileId>,

	/// The file an out-of-line module resolves to (or the candidates, when none exists).
	pub file_path: Option<PathBuf>,

	/// Whether child modules declared in the module's file are looked up in the file's own directory
	/// (crate roots, `mod.rs` files, and files loaded with `#[path]`). `false` for inline modules.
	pub dir_owner: bool,

	/// Why the module's file could not be loaded or parsed.
	pub load_error: Option<String>,
}

/// Details of one leaf of a `use` tree.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
pub struct ImportInfo {
	/// The full imported path, including the prefix contributed by enclosing groups.
	/// For `a::{self}` the path is `a`, for globs the path is the glob's parent.
	pub path: PathRef,

	/// `as name` / `as _`. Stored unraw'd; `_` for underscore imports.
	pub alias: Option<SmolStr>,

	/// Location of the alias identifier.
	pub alias_range: Option<TextRange>,

	/// `a::*`
	pub glob: bool,

	/// `a::{self}` / `a::{self as b}`
	pub is_self: bool,
}

impl ImportInfo {
	/// The name the import binds, or `None` for globs and `_` imports.
	pub fn binding_name(&self) -> Option<&SmolStr> {
		if self.glob {
			return None;
		}

		match &self.alias {
			Some(alias) if alias == "_" => None,
			Some(alias) => Some(alias),
			None => self.path.last().map(|segment| &segment.name),
		}
	}
}

/// Details of an `impl` block.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
pub struct ImplInfo {
	/// The implementing type.
	pub self_ty: TypeRef,

	/// The self type as written (normalized token text, e.g. `Foo<T>`).
	pub self_ty_text: String,

	/// The implemented trait, if any.
	pub trait_path: Option<PathRef>,

	/// The trait as written (normalized token text), if any.
	pub trait_text: Option<String>,

	/// `impl !Trait for Type`
	pub negative: bool,

	/// `unsafe impl`
	pub is_unsafe: bool,

	/// Range inside the braces.
	pub body: TextRange,
}

/// Kind-specific details of an item.
#[derive(Debug, Default, Clone, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum ItemDetail {
	/// No details (for the kinds without any).
	#[default]
	None,

	/// A module.
	Module(ModuleInfo),

	/// A leaf of a `use` tree.
	Import(ImportInfo),

	/// An `impl` block.
	Impl(ImplInfo),

	/// A function (in a module, `impl` block, trait, or `extern` block).
	Fn(FnInfo),

	/// An `extern crate` item.
	ExternCrate {
		/// The crate being imported (`self` for `extern crate self as foo;`).
		crate_name: SmolStr,

		/// `as name`, unraw'd.
		alias: Option<SmolStr>,
	},

	/// An `extern` block.
	ExternBlock {
		/// The ABI string (`"C"`), if written.
		abi: Option<String>,

		/// `unsafe extern`
		is_unsafe: bool,

		/// Range inside the braces.
		body: TextRange,
	},

	/// A `static` (in a module or an `extern` block).
	Static {
		/// `static mut`
		mutable: bool,
	},

	/// Trait (or trait alias) details.
	Trait {
		/// `unsafe trait`
		is_unsafe: bool,

		/// `auto trait`
		is_auto: bool,

		/// Range inside the braces (`None` for trait aliases).
		body: Option<TextRange>,
	},

	/// Struct, union, enum, or variant fields.
	Data {
		/// `{ ... }` fields (or enum variants' braces), `( ... )` fields, or none (unit).
		shape: DataShape,

		/// Range inside the braces/parentheses.
		body: Option<TextRange>,
	},

	/// A macro invocation or definition.
	Macro {
		/// The invoked macro's path as written (`macro_rules`, `foo::bar`); `macro` for declarative macros 2.0
		/// (`macro m() {}`), and empty for syntax that was not understood.
		path: String,

		/// Range inside the delimiters (of the last delimited group, for macros 2.0 and syntax that was not
		/// understood; an empty range at the end of the item if there is none).
		body: TextRange,
	},
}

/// The shape of struct, union, or variant fields.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DataShape {
	/// `{ a: A, b: B }` (and the variants of enums).
	Named,

	/// `(A, B)`
	Tuple,

	/// No fields.
	Unit,
}

/// An item in a crate's item tree.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct ItemData {
	/// What the item is.
	pub kind: ItemKind,

	/// The unraw'd name. `None` for unnamed items (`impl`, `use`, `extern` blocks, macro calls, `const _`,
	/// glob and `_` imports).
	pub name: Option<SmolStr>,

	/// Syntactic parent (index into the same crate). `None` only for the crate root.
	pub(crate) parent: Option<u32>,

	/// Syntactic children (indices into the same crate), in source order.
	pub(crate) children: Vec<u32>,

	/// The file containing the item's text. For out-of-line modules this is the file with the `mod foo;`
	/// declaration; the module's own file is [`ModuleInfo::file`].
	pub file: FileId,

	/// The whole item, including outer attributes and doc comments
	/// (but not attached non-doc comments; see [`crate::edit`]).
	///
	/// For an [`ItemKind::Import`], the element of the innermost `{..}` group of the `use` tree that contains the
	/// leaf: the tokens between the group's `{` or `,` before the leaf and the next `,` or `}`, including a leading
	/// `::` (`B as C` in `use a::{B as C, D};`, `b::C` in `use a::{b::C, D};`, `::c::D` in `use {::c::D, e};`).
	/// For a leaf in no group, the whole tree between `use` and `;`, including a leading `::` (`::a::b::C` in
	/// `use ::a::b::C;`). In a group, removing the range and one adjacent comma removes just the leaf (with the path
	/// prefix only it uses) and leaves valid code. The leaf's own use tree (`C`, `B as C`, `*`, `self`) is a suffix
	/// of the range: it starts at the last segment of [`ImportInfo::path`] (at the `*` for globs, and at the `self`
	/// token for `self` imports).
	pub range: TextRange,

	/// The identifier token (including any `r#`) that names the item.
	pub name_range: Option<TextRange>,

	/// The visibility as written.
	pub vis: Visibility,

	/// The item's own `cfg` predicate (all `#[cfg]`s combined), excluding ancestors'.
	pub cfg: Option<CfgExpr>,

	/// The attributes rscode cares about.
	pub attrs: ItemAttrs,

	/// Details for the item's kind.
	pub detail: ItemDetail,
}

impl ItemData {
	/// The unraw'd name, if the item has one.
	pub fn name(&self) -> Option<&str> {
		self.name.as_deref()
	}

	/// The details of a module.
	pub fn module_info(&self) -> Option<&ModuleInfo> {
		match &self.detail {
			ItemDetail::Module(info) => Some(info),
			_ => None,
		}
	}

	/// The details of a leaf of a `use` tree.
	pub fn import_info(&self) -> Option<&ImportInfo> {
		match &self.detail {
			ItemDetail::Import(info) => Some(info),
			_ => None,
		}
	}

	/// The details of an `impl` block.
	pub fn impl_info(&self) -> Option<&ImplInfo> {
		match &self.detail {
			ItemDetail::Impl(info) => Some(info),
			_ => None,
		}
	}

	/// The details of a function.
	pub fn fn_info(&self) -> Option<&FnInfo> {
		match &self.detail {
			ItemDetail::Fn(info) => Some(info),
			_ => None,
		}
	}

	/// The range inside the item's delimiters (braces, or parentheses for tuple structs/variants and macros),
	/// for containers, data types, and macros. See [`FnInfo::body`] for function bodies.
	pub fn body(&self) -> Option<TextRange> {
		match &self.detail {
			ItemDetail::Module(info) => info.body,
			ItemDetail::Impl(info) => Some(info.body),
			ItemDetail::ExternBlock { body, .. } => Some(*body),
			ItemDetail::Trait { body, .. } => *body,
			ItemDetail::Data { body, .. } => *body,
			ItemDetail::Macro { body, .. } => Some(*body),
			_ => None,
		}
	}
}
