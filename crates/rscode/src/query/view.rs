//! Viewing the source (or an outline) of items.

use super::outline::Source;
use super::outline::item_snippet;
use super::snippet::Line;
use super::snippet::Syntax;
use crate::Error;
use crate::cfg::Tristate;
use crate::model::CrateId;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::Workspace;
use crate::path::ItemPath;
use crate::resolve::Resolver;
use crate::source::FileId;
use crate::source::LineCol;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

/// The rendered view of an item.
#[derive(Debug, Clone, Serialize)]
pub struct ItemView {
	/// The item.
	#[serde(skip)]
	pub item: ItemId,

	/// Canonical path (see [`crate::path::CanonicalPath`]); for imports, a `use` path (`use my_crate::a::Name`), whose
	/// view is that of their `use` item.
	pub path: String,

	/// The kind of the item.
	pub kind: ItemKind,

	/// Path of the file containing the item (for out-of-line modules: the file declaring them), relative to the
	/// workspace root when possible.
	pub file: PathBuf,

	/// Start of the item (including attributes and doc comments).
	pub start: LineCol,

	/// End of the item (exclusive).
	pub end: LineCol,

	/// The effective `cfg` predicate (the item's and its ancestors'), if any.
	pub cfg: Option<String>,

	/// Evaluation of [`ItemView::cfg`].
	pub active: Tristate,

	/// Whether the item is a static declared by `thread_local!` (a `LocalKey` of its declared type).
	#[serde(skip_serializing_if = "std::ops::Not::not")]
	pub thread_local: bool,

	/// For a static declared by an entry of a macro invocation other than `thread_local!`: the macro's name.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub entry_macro: Option<String>,

	/// The rendered text.
	pub text: String,

	/// Views of `impl` blocks (with [`ViewOptions::impls`]).
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub impls: Vec<ItemView>,
}

/// When views prefix their lines with line numbers.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineNumbers {
	/// Never.
	#[default]
	Never,

	/// Always.
	Always,

	/// Only when the lines shown are not the consecutive lines of the item (or file) from its first line on, as when
	/// bodies are elided or doc comments left out: otherwise the first line's number tells every line's.
	Auto,
}

impl From<bool> for LineNumbers {
	fn from(line_numbers: bool) -> Self {
		match line_numbers {
			true => Self::Always,
			false => Self::Never,
		}
	}
}

/// Renders views, parsing every file once.
struct Renderer<'a, 'ws> {
	resolver: &'a Resolver<'ws>,
	options: &'a ViewOptions,
	syntax: HashMap<(CrateId, FileId), Syntax>,
}

impl Renderer<'_, '_> {
	fn is_wanted(&self, item: ItemId) -> bool {
		!self.options.active_only || self.resolver.workspace().is_active(item) != Tristate::False
	}

	fn render(&mut self, item: ItemId, mode: ViewMode) -> ItemView {
		let workspace = self.resolver.workspace();
		let data = workspace.item(item);

		let shown = shown_item(workspace, item);
		let file = workspace.file_of(shown);
		let (start, end) = file.locate(workspace.item(shown).range);

		ItemView {
			item,
			path: self.resolver.canonical_path(item).to_string(),
			kind: data.kind,
			file: workspace.display_path(file.path()).to_path_buf(),
			start,
			end,
			cfg: workspace.effective_cfg(item).map(|cfg| cfg.to_string()),
			active: workspace.is_active(item),
			thread_local: data.is_thread_local(),
			entry_macro: workspace.entry_macro(item).map(str::to_owned),
			text: self.text(shown, mode),
			impls: Vec::new(),
		}
	}

	fn text(&mut self, item: ItemId, mode: ViewMode) -> String {
		let workspace = self.resolver.workspace();
		let source = Source::of(workspace, item);

		let outline = match mode {
			ViewMode::Auto => workspace.item(item).kind == ItemKind::Module,
			ViewMode::Full => false,
			ViewMode::Outline => true,
		};

		let syntax = (self.syntax.entry((item.krate(), source.file_id))).or_insert_with(|| Syntax::of(source.file.text()));
		let mut snippet = item_snippet(workspace, item, &source, syntax, outline, self.options.docs, self.options.imports);
		let first_line = source.file.line_col(source.region.start).line;
		let line_numbers = match self.options.line_numbers {
			LineNumbers::Never => false,
			LineNumbers::Always => true,
			LineNumbers::Auto => !snippet.is_consecutive_from(first_line),
		};

		snippet.dedent();

		// the text is not where the view's location is
		if source.module_file {
			let path = workspace.display_path(source.file.path());

			snippet.lines.insert(0, Line::synthetic(format!("// file: {}", path.display())));
		}

		snippet.render(line_numbers)
	}

	/// The view of an item, with its `impl` blocks if requested.
	fn view(&mut self, item: ItemId) -> ItemView {
		let mode = self.options.mode;
		let mut view = self.render(item, mode);

		if self.options.impls && has_impls(view.kind) {
			let mode = match mode {
				ViewMode::Full => ViewMode::Full,
				ViewMode::Auto | ViewMode::Outline => ViewMode::Outline,
			};

			for impl_block in self.resolver.impls_of(item) {
				if self.is_wanted(impl_block) {
					view.impls.push(self.render(impl_block, mode));
				}
			}
		}

		view
	}
}

/// A builder for viewing items by path.
///
/// The text of a view is taken from the source file and dedented, so that it starts at column 0 even for nested items.
/// Modules with a file of their own (out-of-line modules and crate roots) are shown from that file (in full or
/// outlined), after a `// file: <path>` line. Line breaks become `\n`, and the text does not end with one.
///
/// With line numbers, every line is prefixed with the number of the line of the file it comes from (lines of elided
/// bodies are skipped), right-aligned, and `│`: `  42 │ fn foo() {`.
///
/// ```no_run
/// # fn main() -> Result<(), rscode::Error> {
/// # let workspace: rscode::Workspace = todo!();
/// let views = rscode::View::new().path("crate::shapes::Circle")?.line_numbers(true).run(&workspace)?;
/// # Ok(()) }
/// ```
#[derive(Debug, Default, Clone)]
pub struct View {
	options: ViewOptions,
	paths: Vec<ItemPath>,
}

impl View {
	/// A view of no paths yet, with default options.
	pub fn new() -> Self {
		Self::default()
	}

	/// A view of no paths yet, with the given options.
	pub fn with_options(options: ViewOptions) -> Self {
		Self { options, paths: Vec::new() }
	}

	/// See [`ViewOptions::active_only`].
	pub fn active_only(mut self, active_only: bool) -> Self {
		self.options.active_only = active_only;
		self
	}

	/// See [`ViewOptions::docs`].
	pub fn docs(mut self, docs: bool) -> Self {
		self.options.docs = docs;
		self
	}

	/// See [`ViewOptions::impls`].
	pub fn impls(mut self, impls: bool) -> Self {
		self.options.impls = impls;
		self
	}

	/// See [`ViewOptions::imports`].
	pub fn imports(mut self, imports: bool) -> Self {
		self.options.imports = imports;
		self
	}

	/// Adds a parsed path to view.
	pub fn item_path(mut self, path: ItemPath) -> Self {
		self.paths.push(path);
		self
	}

	/// Views specific items, in order, without duplicates (imports of one `use` item with the same path, like its glob
	/// imports, are shown once).
	pub fn items(&self, resolver: &Resolver<'_>, items: &[ItemId]) -> Result<Vec<ItemView>, Error> {
		let mut renderer = Renderer {
			resolver,
			options: &self.options,
			syntax: HashMap::new(),
		};
		let mut seen = HashSet::new();
		let mut views = Vec::with_capacity(items.len());

		for &item in items {
			let key = (shown_item(resolver.workspace(), item), resolver.canonical_path(item));

			if seen.insert(key) && renderer.is_wanted(item) {
				views.push(renderer.view(item));
			}
		}

		Ok(views)
	}

	/// See [`ViewOptions::line_numbers`]; `true` for [`LineNumbers::Always`], `false` for [`LineNumbers::Never`].
	pub fn line_numbers(mut self, line_numbers: impl Into<LineNumbers>) -> Self {
		self.options.line_numbers = line_numbers.into();
		self
	}

	/// See [`ViewOptions::mode`].
	pub fn mode(mut self, mode: ViewMode) -> Self {
		self.options.mode = mode;
		self
	}

	/// The options of the view.
	pub fn options(&self) -> &ViewOptions {
		&self.options
	}

	/// Adds a path to view (see [`ItemPath`]).
	pub fn path(mut self, path: &str) -> Result<Self, Error> {
		self.paths.push(ItemPath::parse(path)?);
		Ok(self)
	}

	/// Views every item each path resolves to, building a [`Resolver`].
	pub fn run(&self, workspace: &Workspace) -> Result<Vec<ItemView>, Error> {
		self.run_with(&Resolver::new(workspace))
	}

	/// Views every item each path resolves to (every `cfg` variant), in the order of the paths, without duplicates.
	/// A path resolving to nothing is an [`Error::NotFound`].
	pub fn run_with(&self, resolver: &Resolver<'_>) -> Result<Vec<ItemView>, Error> {
		let mut items = Vec::new();

		for path in &self.paths {
			let resolved = resolver.resolve_item_path(path);

			if resolved.is_empty() {
				return Err(Error::NotFound(path.to_string()));
			}

			items.extend(resolved);
		}

		self.items(resolver, &items)
	}
}

/// How much of an item to show.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewMode {
	/// Outline for modules, full source for everything else.
	#[default]
	Auto,

	/// The exact source text.
	Full,

	/// Function, method, and macro bodies (other than of `thread_local!`, whose statics are items) elided as `{ ... }`;
	/// for modules, their items' outlines
	/// (with nested inline modules collapsed to `mod name { ... }`). See [`outline_text`](super::outline_text).
	Outline,
}

/// Options for [`View`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ViewOptions {
	/// How much of each item to show.
	pub mode: ViewMode,

	/// Include doc comments (and `#[doc = ...]` attributes).
	pub docs: bool,

	/// When to prefix the lines with their line numbers.
	pub line_numbers: LineNumbers,

	/// List the `use` items of modules in outlines; otherwise each run of several private `use` items becomes one line
	/// `use ...;`.
	pub imports: bool,

	/// After types and traits, also show their `impl` blocks (outlined unless the mode is [`ViewMode::Full`]).
	pub impls: bool,

	/// Skip items whose `cfg` is definitely disabled.
	pub active_only: bool,
}

impl Default for ViewOptions {
	fn default() -> Self {
		Self {
			mode: ViewMode::Auto,
			docs: true,
			line_numbers: LineNumbers::Never,
			imports: true,
			impls: false,
			active_only: false,
		}
	}
}

/// Whether `impl` blocks can be for (or, for traits, of) items of a kind.
fn has_impls(kind: ItemKind) -> bool {
	matches!(
		kind,
		ItemKind::Struct | ItemKind::Enum | ItemKind::Union | ItemKind::Trait | ItemKind::TypeAlias | ItemKind::ForeignType
	)
}

/// The item whose text is shown for an item: the item, or for an import its `use` item (of which its own text is
/// only a part).
pub(crate) fn shown_item(workspace: &Workspace, item: ItemId) -> ItemId {
	match workspace.item(item).kind {
		ItemKind::Import => workspace.parent(item).unwrap_or(item),
		_ => item,
	}
}
