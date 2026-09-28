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

/// How much of an item to show.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewMode {
	/// Outline for modules, full source for everything else.
	#[default]
	Auto,

	/// The exact source text.
	Full,

	/// Function, method, and macro bodies elided as `{ ... }`; for modules, their items' outlines
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

	/// Prefix every line with its line number.
	pub line_numbers: bool,

	/// After types and traits, also show their `impl` blocks (outlined unless the mode is [`ViewMode::Full`]).
	pub impls: bool,

	/// Skip items whose `cfg` is definitely disabled.
	pub active_only: bool,
}

impl Default for ViewOptions {
	fn default() -> Self {
		Self { mode: ViewMode::Auto, docs: true, line_numbers: false, impls: false, active_only: false }
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

	/// Adds a path to view (see [`ItemPath`]).
	pub fn path(mut self, path: &str) -> Result<Self, Error> {
		self.paths.push(ItemPath::parse(path)?);
		Ok(self)
	}

	/// Adds a parsed path to view.
	pub fn item_path(mut self, path: ItemPath) -> Self {
		self.paths.push(path);
		self
	}

	/// See [`ViewOptions::mode`].
	pub fn mode(mut self, mode: ViewMode) -> Self {
		self.options.mode = mode;
		self
	}

	/// See [`ViewOptions::docs`].
	pub fn docs(mut self, docs: bool) -> Self {
		self.options.docs = docs;
		self
	}

	/// See [`ViewOptions::line_numbers`].
	pub fn line_numbers(mut self, line_numbers: bool) -> Self {
		self.options.line_numbers = line_numbers;
		self
	}

	/// See [`ViewOptions::impls`].
	pub fn impls(mut self, impls: bool) -> Self {
		self.options.impls = impls;
		self
	}

	/// See [`ViewOptions::active_only`].
	pub fn active_only(mut self, active_only: bool) -> Self {
		self.options.active_only = active_only;
		self
	}

	/// The options of the view.
	pub fn options(&self) -> &ViewOptions {
		&self.options
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

	/// Views specific items, in order, without duplicates.
	pub fn items(&self, resolver: &Resolver<'_>, items: &[ItemId]) -> Result<Vec<ItemView>, Error> {
		let mut renderer = Renderer { resolver, options: &self.options, syntax: HashMap::new() };
		let mut seen = HashSet::new();
		let mut views = Vec::with_capacity(items.len());

		for &item in items {
			if seen.insert(item) && renderer.is_wanted(item) {
				views.push(renderer.view(item));
			}
		}

		Ok(views)
	}
}

/// The rendered view of an item.
#[derive(Debug, Clone, Serialize)]
pub struct ItemView {
	/// The item.
	#[serde(skip)]
	pub item: ItemId,

	/// Canonical path (see [`crate::path::CanonicalPath`]).
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

	/// The rendered text.
	pub text: String,

	/// Views of `impl` blocks (with [`ViewOptions::impls`]).
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub impls: Vec<ItemView>,
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

	fn render(&mut self, item: ItemId, mode: ViewMode) -> ItemView {
		let workspace = self.resolver.workspace();
		let data = workspace.item(item);
		let file = workspace.file_of(item);
		let (start, end) = file.locate(data.range);

		ItemView {
			item,
			path: self.resolver.canonical_path(item).to_string(),
			kind: data.kind,
			file: workspace.display_path(file.path()).to_path_buf(),
			start,
			end,
			cfg: workspace.effective_cfg(item).map(|cfg| cfg.to_string()),
			active: workspace.is_active(item),
			text: self.text(item, mode),
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

		let syntax =
			(self.syntax.entry((item.krate(), source.file_id))).or_insert_with(|| Syntax::of(source.file.text()));
		let mut snippet = item_snippet(workspace, item, &source, syntax, outline, self.options.docs);

		snippet.dedent();

		// the text is not where the view's location is
		if source.module_file {
			let path = workspace.display_path(source.file.path());

			snippet.lines.insert(0, Line::synthetic(format!("// file: {}", path.display())));
		}

		snippet.render(self.options.line_numbers)
	}
}

/// Whether `impl` blocks can be for (or, for traits, of) items of a kind.
fn has_impls(kind: ItemKind) -> bool {
	matches!(
		kind,
		ItemKind::Struct
			| ItemKind::Enum
			| ItemKind::Union
			| ItemKind::Trait
			| ItemKind::TypeAlias
			| ItemKind::ForeignType
	)
}
