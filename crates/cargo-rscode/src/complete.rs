//! Dynamic shell completion (`COMPLETE=<shell> cargo-rscode`, see [`clap_complete::CompleteEnv`]).
//!
//! Completers run in a fresh process on every TAB, with the shell's working directory, and their stdout is the
//! reply to the shell. So they must be quick, must never print (errors and panics yield no candidates), and must
//! filter by the typed prefix themselves (clap does that only for `ArgValueCandidates`).
//!
//! A completer only receives the word being completed, but in completion mode `argv` is
//! `[completer, "--", words...]`, so options typed earlier (`--manifest-path`, `-p`) are read from there.

use cargo::util::command_prelude::new_gctx_for_completions;
use cargo::util::command_prelude::root_manifest;
use clap_complete::CompletionCandidate;
use rscode::ItemId;
use rscode::ItemKind;
use rscode::LoadOptions;
use rscode::Resolver;
use rscode::Workspace;
use rscode::model::ImportInfo;
use rscode::model::Visibility;
use rscode::resolve::Namespace;
use rscode::resolve::Res;
use rscode::workspace::TargetSelection;
use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::iter::Peekable;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::path::PathBuf;

/// The environment variable that activates completion.
pub(crate) const COMPLETE_VAR: &str = "COMPLETE";

/// At most this many path candidates are offered.
const MAX_CANDIDATES: usize = 200;

/// A named child of a node of a [`PathTree`].
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct Child<N> {
	/// The name as written in paths (`r#type` for keywords).
	pub(crate) name: String,

	pub(crate) kind: ItemKind,
	pub(crate) node: N,
}

/// Which items are offered.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum Filter {
	Any,
	Modules,
}

impl Filter {
	fn accepts(self, kind: ItemKind) -> bool {
		match self {
			Self::Any => true,
			Self::Modules => kind == ItemKind::Module,
		}
	}
}

/// Nameable items to complete paths among.
pub(crate) trait PathTree {
	type Node: Copy;

	/// The items nameable as `<node path>::<name>`.
	fn children(&self, node: Self::Node) -> Vec<Child<Self::Node>>;

	/// The selected crates: their names and roots.
	fn crates(&self) -> Vec<(String, Self::Node)>;

	/// For a struct, union, or variant: the names (or indexes) of its fields, nameable as `<node path>.<name>`.
	fn fields(&self, node: Self::Node) -> Vec<String>;

	/// For a module: the names its imports bind, as written in `use` paths (`*` for glob imports, `_` for `as _` ones),
	/// whatever their visibility.
	fn imports(&self, node: Self::Node) -> Vec<String>;
}

/// The options among completion words that affect which crates are completed: the workspace, the packages, and
/// their targets (`--example demo` completes the paths of that example).
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub(crate) struct TypedArgs {
	pub(crate) manifest_path: Option<PathBuf>,
	pub(crate) packages: Vec<String>,
	pub(crate) workspace: bool,
	pub(crate) exclude: Vec<String>,
	pub(crate) targets: TargetSelection,
}

impl TypedArgs {
	pub(crate) fn parse(words: &[OsString]) -> Self {
		let mut typed = Self::default();
		let mut words = words.iter().filter_map(|word| word.to_str()).peekable();

		while let Some(word) = words.next() {
			let targets = &mut typed.targets;

			match word {
				"--" => break,
				"--workspace" | "--all" => typed.workspace = true,
				"--lib" => targets.lib = true,
				"--bins" => targets.all_bins = true,
				"--examples" => targets.all_examples = true,
				"--tests" => targets.all_tests = true,
				"--benches" => targets.all_benches = true,
				"--all-targets" => targets.all_targets = true,

				_ => {
					let values = [
						("--manifest-path", Some('m')),
						("--package", Some('p')),
						("--exclude", None),
						("--bin", None),
						("--example", None),
						("--test", None),
						("--bench", None),
					];
					let Some((option, value)) = values
						.into_iter()
						.find_map(|(long, short)| Some((long, option_value(word, long, short, &mut words)?)))
					else {
						continue;
					};

					let value = value.map(str::to_owned);

					match option {
						"--manifest-path" => typed.manifest_path = value.map(PathBuf::from),
						"--package" => typed.packages.extend(value),
						"--exclude" => typed.exclude.extend(value),
						"--bin" => targets.bins.extend(value),
						"--example" => targets.examples.extend(value),
						"--test" => targets.tests.extend(value),
						_ => targets.benches.extend(value),
					}
				}
			}
		}

		typed
	}

	/// How to load the workspace: quietly, and without network access.
	pub(crate) fn load_options(&self) -> LoadOptions {
		LoadOptions {
			manifest_path: self.manifest_path.clone(),
			packages: self.packages.clone(),
			workspace: self.workspace,
			exclude: self.exclude.clone(),
			targets: self.targets.clone(),
			offline: true,
			silent: true,
			..LoadOptions::default()
		}
	}
}

/// The loaded workspace as a [`PathTree`]. Associated items of types need name resolution, which is only done when
/// a typed path reaches below a type.
pub(crate) struct WorkspaceTree<'ws> {
	workspace: &'ws Workspace,
	resolver: OnceCell<Resolver<'ws>>,
}

impl<'ws> WorkspaceTree<'ws> {
	pub(crate) fn new(workspace: &'ws Workspace) -> Self {
		Self {
			workspace,
			resolver: OnceCell::new(),
		}
	}

	fn associated_items(&self, owner: ItemId, children: &mut Vec<Child<ItemId>>) {
		let items = self.resolver().associated_items(owner);

		children.extend(
			items
				.into_iter()
				.filter(|&id| self.workspace.item(id).kind.is_nameable())
				.filter_map(|id| self.child(id)),
		);
	}

	fn child(&self, id: ItemId) -> Option<Child<ItemId>> {
		let item = self.workspace.item(id);
		let name = item.name()?;
		let name = if rscode::path::is_keyword(name) {
			format!("r#{name}")
		} else {
			name.to_owned()
		};

		Some(Child {
			name,
			kind: item.kind,
			node: id,
		})
	}

	/// The items of a module: its nameable items, the items of its `extern` blocks and the statics of its
	/// `thread_local!` invocations, and its re-exports.
	fn module_children(&self, module: ItemId, children: &mut Vec<Child<ItemId>>) {
		for id in self.workspace.children(module) {
			let item = self.workspace.item(id);

			match item.kind {
				// transparent: their items live in the module (only `thread_local!` invocations have children)
				ItemKind::ExternBlock | ItemKind::MacroCall => self.module_children(id, children),

				ItemKind::Use => {
					let reexports = self
						.workspace
						.children(id)
						.filter(|&import| self.workspace.item(import).vis != Visibility::Private);

					children.extend(reexports.filter_map(|import| self.child(import)));
				}

				kind if kind.is_nameable() => children.extend(self.child(id)),
				_ => {}
			}
		}
	}

	/// The items a re-export names in the type namespace, the one of the items with children (imports are followed
	/// to their final targets).
	fn reexported(&self, import: ItemId) -> Vec<ItemId> {
		let Some(name) = self.workspace.item(import).name() else {
			return Vec::new();
		};

		let module = self.workspace.module_of(import);

		self.resolver()
			.bindings(module, name, Namespace::Type)
			.into_iter()
			.filter(|binding| binding.import == Some(import))
			.filter_map(|binding| match binding.res {
				Res::Item(item) => Some(item),
				_ => None,
			})
			.filter(|&item| self.workspace.item(item).kind != ItemKind::Import)
			.collect()
	}

	fn resolver(&self) -> &Resolver<'ws> {
		self.resolver.get_or_init(|| Resolver::new(self.workspace))
	}
}

impl PathTree for WorkspaceTree<'_> {
	type Node = ItemId;

	fn children(&self, node: ItemId) -> Vec<Child<ItemId>> {
		let mut children = Vec::new();

		match self.workspace.item(node).kind {
			ItemKind::Module => self.module_children(node, &mut children),

			ItemKind::Trait => {
				let items = self.workspace.children(node).filter(|&id| self.workspace.item(id).kind.is_nameable());

				children.extend(items.filter_map(|id| self.child(id)));
			}

			ItemKind::Enum => {
				let variants = self
					.workspace
					.children(node)
					.filter(|&id| self.workspace.item(id).kind == ItemKind::Variant);

				children.extend(variants.filter_map(|id| self.child(id)));
				self.associated_items(node, &mut children);
			}

			ItemKind::Struct | ItemKind::Union | ItemKind::TypeAlias | ItemKind::ForeignType => {
				self.associated_items(node, &mut children);
			}

			// `crate::Circle::new` through `pub use shapes::Circle;`
			ItemKind::Import => {
				for target in self.reexported(node) {
					children.extend(self.children(target));
				}
			}

			_ => {}
		}

		children
	}

	fn crates(&self) -> Vec<(String, ItemId)> {
		self.workspace
			.selected_crates()
			.map(|krate| (krate.name().to_string(), krate.root_module()))
			.collect()
	}

	fn fields(&self, node: ItemId) -> Vec<String> {
		(self.workspace.children(node))
			.filter(|&id| self.workspace.item(id).kind == ItemKind::Field)
			.filter_map(|id| self.child(id))
			.map(|child| child.name)
			.collect()
	}

	fn imports(&self, node: ItemId) -> Vec<String> {
		if self.workspace.item(node).kind != ItemKind::Module {
			return Vec::new();
		}

		let uses = self.workspace.children(node).filter(|&id| self.workspace.item(id).kind == ItemKind::Use);
		let imports = uses.flat_map(|id| self.workspace.children(id));
		let mut names: Vec<String> = imports
			.filter_map(|import| self.workspace.item(import).import_info().map(ImportInfo::path_name))
			.map(|name| {
				if rscode::path::is_keyword(&name) {
					format!("r#{name}")
				} else {
					name.to_string()
				}
			})
			.collect();

		names.sort();
		names.dedup();
		names
	}
}

/// Completion candidates, with their help, in order and capped at [`MAX_CANDIDATES`].
fn candidates(found: BTreeMap<String, &'static str>) -> Vec<CompletionCandidate> {
	found
		.into_iter()
		.take(MAX_CANDIDATES)
		.map(|(value, help)| CompletionCandidate::new(value).help(Some(help.into())))
		.collect()
}

/// Candidates for a typed path, one `::` segment at a time: `crate` and crate names first, then the children of the
/// items the typed segments name. Qualified paths and patterns (`<`, `*`) are not completed. `use` paths (typed as one
/// word, which shells may pass with its opening quote, or with escaped spaces) complete modules and imports.
pub(crate) fn complete_path<T: PathTree>(tree: &T, typed: &str, filter: Filter) -> Vec<CompletionCandidate> {
	let word = typed.trim_start_matches(['\'', '"']).replace("\\ ", " ");

	if filter == Filter::Any
		&& let Some(rest) = word.strip_prefix("use").filter(|rest| rest.starts_with(char::is_whitespace))
	{
		return complete_use_path(tree, rest.trim_start());
	}

	complete_plain_path(tree, typed, filter, |_| Vec::new())
}

/// [`complete_path`] of a path without `use`, offering `extra` names (and their help) of the nodes the head names too.
fn complete_plain_path<T: PathTree>(
	tree: &T,
	typed: &str,
	filter: Filter,
	extra: impl Fn(T::Node) -> Vec<(String, &'static str)>,
) -> Vec<CompletionCandidate> {
	if typed.contains(|char: char| char.is_whitespace() || matches!(char, '<' | '>' | '*')) {
		return Vec::new();
	}

	let (global, rest) = match typed.strip_prefix("::") {
		Some(rest) => (true, rest),
		None => (false, typed),
	};

	// a field after a `.`: those of the structs, unions, and variants that the path before it names
	if filter == Filter::Any
		&& let Some((owner, partial)) = rest.rsplit_once('.')
	{
		let prefix = &typed[..typed.len() - partial.len()];
		let fields = (nodes_named(tree, &tree.crates(), owner, global).into_iter())
			.flat_map(|node| tree.fields(node))
			.filter(|name| matches_prefix(name, partial))
			.map(|name| (format!("{prefix}{name}"), "field"));

		return candidates(fields.collect());
	}

	let (head, partial) = match rest.rsplit_once("::") {
		Some((head, partial)) => (Some(head), partial),
		None => (None, rest),
	};

	// everything before the partial segment, `::`s included
	let prefix = &typed[..typed.len() - partial.len()];
	let crates = tree.crates();
	let mut found: BTreeMap<String, &'static str> = BTreeMap::new();

	match head {
		None => {
			if !global && !crates.is_empty() && "crate".starts_with(partial) {
				found.insert("crate".to_owned(), "the crate root");
			}

			for (name, _) in &crates {
				if matches_prefix(name, partial) {
					found.entry(format!("{prefix}{name}")).or_insert("crate");
				}
			}
		}

		Some(head) => {
			let nodes = nodes_named(tree, &crates, head, global);

			for &node in &nodes {
				for (name, help) in extra(node) {
					if matches_prefix(&name, partial) {
						found.entry(format!("{prefix}{name}")).or_insert(help);
					}
				}
			}

			for child in nodes.into_iter().flat_map(|node| tree.children(node)) {
				if filter.accepts(child.kind) && matches_prefix(&child.name, partial) {
					found.entry(format!("{prefix}{}", child.name)).or_insert(child.kind.name());
				}
			}
		}
	}

	candidates(found)
}

/// Candidates for a `use` path: the modules on the way, and the names the imports of the last module bind.
fn complete_use_path<T: PathTree>(tree: &T, typed: &str) -> Vec<CompletionCandidate> {
	let imports = |node| tree.imports(node).into_iter().map(|name| (name, "import")).collect();

	(complete_plain_path(tree, typed, Filter::Modules, imports).into_iter())
		.map(|candidate| {
			let value = format!("use {}", candidate.get_value().to_string_lossy());

			CompletionCandidate::new(value).help(candidate.get_help().cloned())
		})
		.collect()
}

/// The words of the command line being completed, and the index of the word being completed.
fn completion_words() -> (Vec<OsString>, usize) {
	let words: Vec<OsString> = std::env::args_os().skip_while(|arg| arg != "--").skip(1).collect();

	// `_CLAP_COMPLETE_INDEX` comes from bash and zsh; fish completes the last word
	let current = std::env::var("_CLAP_COMPLETE_INDEX")
		.ok()
		.and_then(|index| index.parse().ok())
		.unwrap_or(words.len().saturating_sub(1));

	(words, current)
}

/// Whether the word at `current` is the value of an option whose value is optional (`-p`, `--bin`, ...).
fn follows_option_with_optional_value(words: &[OsString], current: usize) -> bool {
	let previous = current.checked_sub(1).and_then(|index| words.get(index)).and_then(|word| word.to_str());

	matches!(previous, Some("-p" | "--package" | "--bin" | "--example" | "--test" | "--bench"))
}

/// `find --from`: `crate`, `::`, or a module path.
pub(crate) fn from_paths(current: &OsStr) -> Vec<CompletionCandidate> {
	let Some(typed) = current.to_str() else {
		return Vec::new();
	};

	let anchors = [("crate", "from each item's crate root"), ("::", "from another crate")];
	let mut candidates: Vec<CompletionCandidate> = anchors
		.into_iter()
		.filter(|(anchor, _)| anchor.starts_with(typed))
		.map(|(anchor, help)| CompletionCandidate::new(anchor).help(Some(help.into())))
		.collect();

	candidates.extend(
		workspace_paths(current, Filter::Modules)
			.into_iter()
			.filter(|candidate| candidate.get_value() != "crate"),
	);
	candidates
}

/// Whether a shell started this process to complete a command line.
pub(crate) fn is_requested() -> bool {
	std::env::var_os(COMPLETE_VAR).is_some_and(|value| !value.is_empty() && value != "0")
}

/// Item paths (`view`, `rename`, `fmt` targets, ...).
pub(crate) fn item_paths(current: &OsStr) -> Vec<CompletionCandidate> {
	let (words, index) = completion_words();

	// clap also asks for positional values after an option with an optional value, though the word is its value
	if follows_option_with_optional_value(&words, index) {
		return Vec::new();
	}

	workspace_paths(current, Filter::Any)
}

/// `find --kind` values.
pub(crate) fn kind_candidates() -> Vec<CompletionCandidate> {
	crate::cli::find_kinds().map(|kind| CompletionCandidate::new(kind.name())).collect()
}

fn matches_prefix(name: &str, partial: &str) -> bool {
	name.starts_with(partial) || unraw(name).starts_with(partial)
}

/// Module paths (`create-module PARENT`).
pub(crate) fn module_paths(current: &OsStr) -> Vec<CompletionCandidate> {
	let (words, index) = completion_words();

	// clap also asks for positional values after an option with an optional value, though the word is its value
	if follows_option_with_optional_value(&words, index) {
		return Vec::new();
	}

	workspace_paths(current, Filter::Modules)
}

/// The nodes a typed path (without its leading `::`, `global`, and its partial segment) names: the crate roots of
/// `crate` or of a crate name, then the children named by each segment.
fn nodes_named<T: PathTree>(tree: &T, crates: &[(String, T::Node)], path: &str, global: bool) -> Vec<T::Node> {
	let mut segments = path.split("::");
	let first = segments.next().unwrap_or_default();
	let mut nodes: Vec<T::Node> = crates
		.iter()
		.filter(|(name, _)| (first == "crate" && !global) || same_ident(name, first))
		.map(|(_, node)| *node)
		.collect();

	for segment in segments {
		nodes = nodes
			.into_iter()
			.flat_map(|node| tree.children(node))
			.filter(|child| same_ident(&child.name, segment))
			.map(|child| child.node)
			.collect();
	}

	nodes
}

/// When `word` is the option (`--long VALUE`, `--long=VALUE`, and with a short form `-s VALUE`, `-sVALUE`, or
/// `-s=VALUE`): its value, if any (a following word that looks like an option is not a value).
fn option_value<'a>(word: &'a str, long: &str, short: Option<char>, rest: &mut Peekable<impl Iterator<Item = &'a str>>) -> Option<Option<&'a str>> {
	let short_text = short.map(|short| format!("-{short}"));

	if word == long || short_text.as_deref() == Some(word) {
		return Some(rest.next_if(|next| !next.starts_with('-')));
	}

	if let Some(value) = word.strip_prefix(long).and_then(|rest| rest.strip_prefix('=')) {
		return Some(Some(value));
	}

	if !word.starts_with("--")
		&& let Some(short) = &short_text
		&& let Some(value) = word.strip_prefix(short.as_str())
	{
		return Some(Some(value.strip_prefix('=').unwrap_or(value)));
	}

	None
}

/// `-p` values: the members of the workspace (of a typed `--manifest-path`, or of the current directory).
pub(crate) fn package_candidates() -> Vec<CompletionCandidate> {
	quietly(|| workspace_members(typed_args().manifest_path.as_deref()).ok())
}

/// Runs a completer's body: its failures and panics mean no candidates (the panic hook is silenced in completion
/// mode, see `main`).
fn quietly<T: Default>(body: impl FnOnce() -> Option<T>) -> T {
	std::panic::catch_unwind(AssertUnwindSafe(body)).ok().flatten().unwrap_or_default()
}

fn same_ident(a: &str, b: &str) -> bool {
	unraw(a) == unraw(b)
}

/// Options typed on the command line being completed.
fn typed_args() -> TypedArgs {
	let (words, current) = completion_words();

	// skip the command name and the word being completed
	TypedArgs::parse(words.get(1..current.min(words.len())).unwrap_or_default())
}

fn unraw(ident: &str) -> &str {
	ident.strip_prefix("r#").unwrap_or(ident)
}

fn workspace_members(manifest_path: Option<&Path>) -> anyhow::Result<Vec<CompletionCandidate>> {
	let gctx = new_gctx_for_completions()?;
	let manifest = root_manifest(manifest_path, &gctx)?;
	let workspace = cargo::workspace::Workspace::new(&manifest, &gctx)?;

	Ok(workspace
		.members()
		.map(|package| {
			let description = package.manifest().metadata().description.clone();

			CompletionCandidate::new(package.name().as_str()).help(description.map(Into::into))
		})
		.collect())
}

fn workspace_paths(current: &OsStr, filter: Filter) -> Vec<CompletionCandidate> {
	let Some(typed) = current.to_str() else {
		return Vec::new();
	};

	quietly(|| {
		let workspace = rscode::load_workspace(&typed_args().load_options()).ok()?;

		Some(complete_path(&WorkspaceTree::new(&workspace), typed, filter))
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::cli::cli;

	/// A tree of `(name, kind, children)` nodes.
	struct FakeTree {
		crates: Vec<(String, usize)>,
		nodes: Vec<(String, ItemKind, Vec<usize>)>,

		/// The names the imports of modules bind.
		imports: Vec<(usize, Vec<&'static str>)>,
	}

	impl FakeTree {
		/// ```text
		/// demo (lib)            demo (bin)
		/// ├── shapes            └── main
		/// │   ├── Circle { new, area, .radius }
		/// │   ├── Kind { Round, Square }
		/// │   └── Shape { area }
		/// ├── r#type (mod)
		/// ├── add
		/// └── Circle (re-export)
		/// ```
		fn demo() -> Self {
			let node = |name: &str, kind: ItemKind, children: &[usize]| (name.to_owned(), kind, children.to_vec());
			let nodes = vec![
				node("demo", ItemKind::Module, &[1, 12, 13, 14]),
				node("shapes", ItemKind::Module, &[2, 5, 8]),
				node("Circle", ItemKind::Struct, &[3, 4, 15]),
				node("new", ItemKind::AssocFn, &[]),
				node("area", ItemKind::AssocFn, &[]),
				node("Kind", ItemKind::Enum, &[6, 7]),
				node("Round", ItemKind::Variant, &[]),
				node("Square", ItemKind::Variant, &[]),
				node("Shape", ItemKind::Trait, &[9]),
				node("area", ItemKind::AssocFn, &[]),
				node("demo", ItemKind::Module, &[11]),
				node("main", ItemKind::Fn, &[]),
				node("r#type", ItemKind::Module, &[]),
				node("add", ItemKind::Fn, &[]),
				node("Circle", ItemKind::Import, &[]),
				node("radius", ItemKind::Field, &[]),
			];

			Self {
				crates: vec![("demo".to_owned(), 0), ("demo".to_owned(), 10)],
				nodes,
				imports: vec![(0, vec!["*", "Circle", "_"]), (1, vec!["Rc"])],
			}
		}
	}

	impl PathTree for FakeTree {
		type Node = usize;

		fn children(&self, node: usize) -> Vec<Child<usize>> {
			self.nodes[node]
				.2
				.iter()
				.filter(|&&child| self.nodes[child].1 != ItemKind::Field)
				.map(|&child| Child {
					name: self.nodes[child].0.clone(),
					kind: self.nodes[child].1,
					node: child,
				})
				.collect()
		}

		fn crates(&self) -> Vec<(String, usize)> {
			self.crates.clone()
		}

		fn fields(&self, node: usize) -> Vec<String> {
			(self.nodes[node].2.iter())
				.filter(|&&child| self.nodes[child].1 == ItemKind::Field)
				.map(|&child| self.nodes[child].0.clone())
				.collect()
		}

		fn imports(&self, node: usize) -> Vec<String> {
			(self.imports.iter())
				.filter(|(module, _)| *module == node)
				.flat_map(|(_, names)| names.iter().map(ToString::to_string))
				.collect()
		}
	}

	#[test]
	fn caps_the_number_of_candidates() {
		let mut tree = FakeTree {
			crates: vec![("big".to_owned(), 0)],
			nodes: vec![("big".to_owned(), ItemKind::Module, (1..=300).collect())],
			imports: Vec::new(),
		};

		tree.nodes
			.extend((1..=300).map(|index| (format!("f{index:03}"), ItemKind::Fn, Vec::new())));

		let candidates = values(&complete_path(&tree, "crate::", Filter::Any));

		assert_eq!(candidates.len(), MAX_CANDIDATES);
		assert_eq!(candidates[0], "crate::f001");
	}

	fn complete(typed: &str) -> Vec<String> {
		values(&complete_path(&FakeTree::demo(), typed, Filter::Any))
	}

	#[test]
	fn completer_failures_yield_nothing() {
		assert_eq!(quietly(|| None::<Vec<u8>>), Vec::<u8>::new());
		assert_eq!(quietly(|| Some(vec![1])), [1]);
		assert_eq!(quietly::<Vec<u8>>(|| panic!("no loader")), Vec::<u8>::new());
	}

	#[test]
	fn completes_from_anchors() {
		// without a loaded workspace only the anchors are offered
		let candidates = values(&from_paths(OsStr::new("")));

		assert!(candidates.starts_with(&["crate".to_owned(), "::".to_owned()]), "{candidates:?}");
		assert_eq!(values(&from_paths(OsStr::new("cr")))[0], "crate");
		assert!(values(&from_paths(OsStr::new("x"))).is_empty());
	}

	#[test]
	fn completes_one_segment_at_a_time() {
		// the lib's and the bin's items (both crates are called `demo`)
		assert_eq!(
			complete("crate::"),
			["crate::Circle", "crate::add", "crate::main", "crate::r#type", "crate::shapes"]
		);
		assert_eq!(complete("crate::sh"), ["crate::shapes"]);
		assert_eq!(
			complete("crate::shapes::"),
			["crate::shapes::Circle", "crate::shapes::Kind", "crate::shapes::Shape"]
		);
		assert_eq!(
			complete("crate::shapes::Circle::"),
			["crate::shapes::Circle::area", "crate::shapes::Circle::new"]
		);
		assert_eq!(complete("crate::shapes::Kind::S"), ["crate::shapes::Kind::Square"]);
		assert_eq!(complete("crate::shapes::Shape::"), ["crate::shapes::Shape::area"]);
		assert_eq!(complete("demo::shapes::C"), ["demo::shapes::Circle"]);
		assert_eq!(complete("::demo::a"), ["::demo::add"]);
		assert_eq!(complete("crate::nope::"), Vec::<String>::new());
		assert_eq!(complete("crate::add::"), Vec::<String>::new());

		// fields after a `.`
		assert_eq!(complete("crate::shapes::Circle."), ["crate::shapes::Circle.radius"]);
		assert_eq!(complete("::demo::shapes::Circle.r"), ["::demo::shapes::Circle.radius"]);
		assert_eq!(complete("crate::shapes::Circle.x"), Vec::<String>::new());
		assert_eq!(complete("crate::shapes::Kind."), Vec::<String>::new());
	}

	#[test]
	fn completes_raw_identifiers() {
		assert_eq!(complete("crate::ty"), ["crate::r#type"]);
		assert_eq!(complete("crate::r#t"), ["crate::r#type"]);
		assert_eq!(complete("crate::type::"), Vec::<String>::new());
		assert_eq!(complete("crate::r#type::"), Vec::<String>::new());
	}

	#[test]
	fn completes_subcommands_and_values() {
		assert_eq!(engine(&["cargo-rscode", "f"]), ["find", "fmt"]);
		assert_eq!(engine(&["cargo-rscode", "re"]), ["rename", "remove", "replace"]);
		assert_eq!(engine(&["cargo-rscode", "cr"]), ["create-module"]);
		assert_eq!(engine(&["cargo-rscode", "i"]), ["insert", "import"]);
		assert_eq!(engine(&["cargo-rscode", "find", "x", "--kind", "stru"]), ["struct"]);
		assert_eq!(
			engine(&["cargo-rscode", "find", "x", "-k", "fn,assoc-"]),
			["fn,assoc-fn", "fn,assoc-const", "fn,assoc-type"]
		);
		assert_eq!(engine(&["cargo-rscode", "find", "x", "--show", "kind,lo"]), ["kind,location"]);
		assert_eq!(engine(&["cargo-rscode", "fmt", "--emit", "j"]), ["json"]);
		assert_eq!(engine(&["cargo-rscode", "fmt", "--formatter", "p"]), ["prettyplease"]);
		assert_eq!(engine(&["cargo-rscode", "sort", "--schema", ""]), ["cryotheum"]);
		assert_eq!(engine(&["cargo-rscode", "fmt", "--edition", "202"]), ["2021", "2024"]);
		assert_eq!(engine(&["cargo-rscode", "insert", "crate", "--position", "a"]), ["after"]);
		assert_eq!(engine(&["cargo-rscode", "find", "x", "--message-format", "f"]), ["file-lines"]);
		assert_eq!(engine(&["cargo-rscode", "view", "x", "--color", "n"]), ["never"]);
	}

	#[test]
	fn completes_the_first_segment() {
		assert_eq!(complete(""), ["crate", "demo"]);
		assert_eq!(complete("c"), ["crate"]);
		assert_eq!(complete("de"), ["demo"]);
		assert_eq!(complete("x"), Vec::<String>::new());
		assert_eq!(complete("::"), ["::demo"]);
		assert_eq!(complete("::d"), ["::demo"]);
	}

	#[test]
	fn completes_through_cargo() {
		// word lists of `cargo rscode ...` (fish, and zsh through rustup's `_cargo`): `cargo` is taken for the binary
		// and `rscode` for a stray positional
		assert_eq!(engine(&["cargo", "rscode", "vi"]), ["view"]);
		assert_eq!(engine(&["cargo", "rscode", "fmt", "--emit", "c"]), ["checkstyle"]);
	}

	/// `use` paths complete the modules on the way and the imports of the last one (all of them, whatever their
	/// visibility), whether the shell passes the word with its opening quote or with escaped spaces.
	#[test]
	fn completes_use_paths() {
		assert_eq!(complete("use "), ["use crate", "use demo"]);
		assert_eq!(
			complete("use crate::"),
			[
				"use crate::*",
				"use crate::Circle",
				"use crate::_",
				"use crate::r#type",
				"use crate::shapes"
			]
		);
		assert_eq!(complete("'use crate::C"), ["use crate::Circle"]);
		assert_eq!(complete("use\\ crate::sh"), ["use crate::shapes"]);
		assert_eq!(complete("use crate::shapes::"), ["use crate::shapes::Rc"]);
		assert_eq!(complete("use crate::shapes::Circle::"), Vec::<String>::new());
		assert_eq!(complete("user"), Vec::<String>::new());
	}

	#[test]
	fn describes_candidates() {
		let candidates = complete_path(&FakeTree::demo(), "crate::shapes::", Filter::Any);
		let help: Vec<String> = candidates.iter().map(|candidate| candidate.get_help().unwrap().to_string()).collect();

		assert_eq!(help, ["struct", "enum", "trait"]);
	}

	#[test]
	fn does_not_complete_patterns_or_qualified_paths() {
		assert!(complete("crate::*").is_empty());
		assert!(complete("<Circle as Shape>::").is_empty());
		assert!(complete("crate:: shapes").is_empty());
	}

	/// Completion of the last word, like the shell adapters do it.
	fn engine(words: &[&str]) -> Vec<String> {
		let args: Vec<OsString> = words.iter().map(OsString::from).collect();
		let index = args.len() - 1;
		let candidates = clap_complete::engine::complete(&mut cli(), args, index, None).unwrap();

		values(&candidates)
	}

	#[test]
	fn filters_modules() {
		let modules = |typed: &str| values(&complete_path(&FakeTree::demo(), typed, Filter::Modules));

		assert_eq!(modules("crate::"), ["crate::r#type", "crate::shapes"]);
		assert_eq!(modules("crate::shapes::"), Vec::<String>::new());
	}

	#[test]
	fn knows_values_of_options_with_optional_values() {
		let line = words(&["cargo-rscode", "view", "-p", "d", "--bin", "x", "--kind", "crate::a"]);
		let follows = |index| follows_option_with_optional_value(&line, index);

		assert!(follows(3));
		assert!(follows(5));
		assert!(!follows(0));
		assert!(!follows(2));
		assert!(!follows(4));
		assert!(!follows(7));
		assert!(!follows(99));
	}

	#[test]
	fn loads_quietly_and_offline() {
		let typed = TypedArgs {
			manifest_path: Some(PathBuf::from("Cargo.toml")),
			packages: vec!["a".to_owned()],
			exclude: vec!["b".to_owned()],
			targets: TargetSelection {
				all_examples: true,
				..TargetSelection::default()
			},
			..TypedArgs::default()
		};
		let options = typed.load_options();

		assert!(options.silent && options.offline && !options.load_all_members);
		assert_eq!(options.packages, ["a"]);
		assert_eq!(options.exclude, ["b"]);
		assert!(options.targets.all_examples);
		assert_eq!(options.manifest_path, Some(PathBuf::from("Cargo.toml")));
	}

	#[test]
	fn offers_the_kinds_find_finds() {
		let kinds = values(&kind_candidates());

		for kind in ["mod", "struct", "assoc-fn", "variant", "field", "import", "impl", "foreign-fn", "macro-call"] {
			assert!(kinds.contains(&kind.to_owned()), "{kind}: {kinds:?}");
		}

		for kind in ["use", "foreign-macro", "assoc-macro", "extern-block"] {
			assert!(!kinds.contains(&kind.to_owned()), "{kind}: {kinds:?}");
		}
	}

	#[test]
	fn reads_typed_options() {
		assert_eq!(TypedArgs::parse(&words(&["find"])), TypedArgs::default());
		assert_eq!(
			TypedArgs::parse(&words(&[
				"find",
				"--manifest-path",
				"a/Cargo.toml",
				"-p",
				"x",
				"--package=y",
				"-pz",
				"--workspace"
			])),
			TypedArgs {
				manifest_path: Some(PathBuf::from("a/Cargo.toml")),
				packages: vec!["x".to_owned(), "y".to_owned(), "z".to_owned()],
				workspace: true,
				..TypedArgs::default()
			}
		);
		assert_eq!(
			TypedArgs::parse(&words(&["view", "-m", "b/Cargo.toml", "-p=w"])),
			TypedArgs {
				manifest_path: Some(PathBuf::from("b/Cargo.toml")),
				packages: vec!["w".to_owned()],
				..TypedArgs::default()
			}
		);
		assert_eq!(
			TypedArgs::parse(&words(&["view", "--manifest-path=c/Cargo.toml"])).manifest_path,
			Some(PathBuf::from("c/Cargo.toml"))
		);

		// `-p` without a value, options after `--`, and look-alikes
		assert_eq!(TypedArgs::parse(&words(&["view", "-p", "--workspace"])).packages, Vec::<String>::new());
		assert!(TypedArgs::parse(&words(&["view", "-p", "--workspace"])).workspace);
		assert_eq!(TypedArgs::parse(&words(&["view", "--", "-p", "x"])), TypedArgs::default());
		assert_eq!(TypedArgs::parse(&words(&["view", "--packages", "x"])), TypedArgs::default());
	}

	#[test]
	fn reads_typed_targets() {
		let typed = TypedArgs::parse(&words(&[
			"view",
			"--workspace",
			"--exclude",
			"big",
			"--example",
			"demo",
			"--test=it",
			"--bin",
			"cli",
			"--bench",
			"speed",
			"--lib",
			"--tests",
		]));

		assert_eq!(typed.exclude, ["big"]);
		assert_eq!(
			typed.targets,
			TargetSelection {
				lib: true,
				bins: vec!["cli".to_owned()],
				examples: vec!["demo".to_owned()],
				tests: vec!["it".to_owned()],
				all_tests: true,
				benches: vec!["speed".to_owned()],
				..TargetSelection::default()
			}
		);

		let all = TypedArgs::parse(&words(&["find", "--bins", "--examples", "--benches", "--all-targets"]));

		assert!(all.targets.all_bins && all.targets.all_examples && all.targets.all_benches && all.targets.all_targets);

		// a bare option takes no value, and `--binary` is not `--bin`
		assert_eq!(
			TypedArgs::parse(&words(&["view", "--example", "--lib"])).targets.examples,
			Vec::<String>::new()
		);
		assert_eq!(TypedArgs::parse(&words(&["view", "--binary", "x"])), TypedArgs::default());
	}

	fn values(candidates: &[CompletionCandidate]) -> Vec<String> {
		candidates
			.iter()
			.map(|candidate| candidate.get_value().to_string_lossy().into_owned())
			.collect()
	}

	fn words(words: &[&str]) -> Vec<OsString> {
		words.iter().map(OsString::from).collect()
	}
}
