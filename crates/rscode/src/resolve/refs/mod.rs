//! Finding references to items by walking the syntax of every loaded file.
//!
//! Every file that mentions a target's name is parsed again (once per crate that loads it) and walked with a syntax
//! visitor that tracks the module the code is in, what `Self` refers to, and the bindings of the local scopes: local
//! variables, generic parameters, and the items and imports of blocks. Paths are resolved with the [`Resolver`] one
//! prefix at a time, and every segment that names a target by the target's own name is a reference.
//!
//! - `use` trees: segments of imported paths ([`ReferenceKind::Import`]).
//! - Paths in types, expressions, patterns, trait bounds, `impl` headers, visibilities (`pub(in path)`), and macro
//!   invocations ([`ReferenceKind::Path`]), including `Self::item`, qualified paths (`<T as Trait>::item`), associated
//!   item bindings (`Iterator<Item = T>`), and identifier patterns that name constants or unit structs and variants.
//!   Like in Rust, `Type::name` names an item of an inherent `impl` rather than one of a trait `impl`, and otherwise
//!   also the provided items of the traits the type implements.
//! - Macro bodies that parse as expressions or statements are walked like code, including the inline arguments of
//!   format strings (`println!("{name}")`). In other macro bodies and the transcribers of `macro_rules!` definitions,
//!   paths (`a::b`, `name!`) are resolved where the tokens are (`$crate::...` from the crate root), which is where a
//!   macro's own paths usually resolve too, and other identifiers named like a target are uncertain
//!   ([`ReferenceKind::MacroToken`]; after `.`, [`ReferenceKind::MethodCall`]).
//! - Paths through generic parameters (`T::method`) are resolved through the traits the parameter is bounded by (in
//!   its declaration, and in the `where` clauses of its item and the items inside of it). Those that the bounds do not
//!   resolve (such as through supertraits), and method calls ([`ReferenceKind::MethodCall`]), depend on types, which
//!   are not known, so they are uncertain.
//! - Intra-doc links in doc comments ([`ReferenceKind::DocLink`]).
//!
//! Aliases are not references: `use a::Old as New;` refers to `Old`, but uses of `New` do not name it (the alias of
//! `use a::Old as Old;` does). Items the model does not know (inside of function bodies and macro invocations) are never
//! targets, but trait `impl`s there implement target trait items ([`ReferenceKind::Definition`]).
//!
//! Files are walked on worker threads, so their syntax trees never grow the calling thread's span source map.

mod docs;
mod format_str;
mod macros;
mod paths;
mod scope;
mod walker;

use super::Resolver;
use super::DeadName;
use super::LostBinding;
use super::fxhash::FxHashMap;
use super::fxhash::FxHashSet;
use crate::model::CrateId;
use crate::model::ItemData;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::Workspace;
use crate::resolve::Res;
use crate::source::FileId;
use crate::source::LineCol;
use crate::source::TextRange;
use proc_macro2::Ident;
use serde::Deserialize;
use serde::Serialize;
use smol_str::SmolStr;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use syn::visit::Visit;
use walker::FileWalker;

/// Stack size of the threads walking syntax trees (deeply nested code recurses deeply, especially in debug builds).
const WORKER_STACK_SIZE: usize = 64 << 20;

/// What to look for besides certain references (definitions, imports, and paths).
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ReferenceOptions {
	/// Method call expressions (`x.name(...)`) with the target's name, when a target is a method.
	/// These cannot be resolved without type inference, so they are reported as uncertain, and so are paths through
	/// generic parameters (`T::name`) that the parameter's bounds do not resolve.
	pub method_calls: bool,

	/// Occurrences of the target's name inside of macro invocations that could not be parsed as expressions or
	/// statements, and inside of `macro_rules!` transcribers, that are not in paths resolved where they are (and not
	/// after `.`, which [`ReferenceOptions::method_calls`] covers). Reported as uncertain.
	pub macro_tokens: bool,

	/// Intra-doc links (``[`Name`]``, `[Name]`, `[path::Name]`, `[text](path::Name)`) in doc comments.
	pub doc_links: bool,
}

impl ReferenceOptions {
	/// Everything.
	pub fn all() -> Self {
		Self {
			method_calls: true,
			macro_tokens: true,
			doc_links: true,
		}
	}
}

/// How a reference refers to its target.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReferenceKind {
	/// The identifier of a definition (a target itself, or an item implementing a target trait item in an `impl` the
	/// model does not know).
	Definition,

	/// A segment of a `use` path (or the alias of `use a::Name as Name;`).
	Import,

	/// A segment of a path in code (types, expressions, patterns, bounds, visibilities, macro paths), or an inline
	/// argument of a format string.
	Path,

	/// A method call (`x.name()`).
	MethodCall,

	/// An identifier token inside of a macro invocation or `macro_rules!` transcriber.
	MacroToken,

	/// An intra-doc link.
	DocLink,
}

/// An occurrence of a target's name.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
pub struct Reference {
	/// The item referred to.
	pub target: ItemId,

	/// How the reference refers to it.
	pub kind: ReferenceKind,

	/// The crate whose code contains the reference (a file loaded by several crates is searched in each).
	pub krate: CrateId,

	/// The file within [`Reference::krate`].
	pub file: FileId,

	/// The file's path.
	pub path: PathBuf,

	/// The whole identifier token (including any `r#`) to replace when renaming.
	pub range: TextRange,

	/// Where the range starts.
	pub start: LineCol,

	/// Whether the occurrence certainly refers to the target.
	pub certain: bool,
}

/// All occurrences found.
#[derive(Debug, Default, Clone, Serialize)]
pub struct References {
	/// Sorted by file path and position, with at most one reference per range.
	pub references: Vec<Reference>,

	/// Human-readable notes about places that could not be analyzed (unparsable files, ...).
	pub notes: Vec<String>,
}

impl References {
	/// For renaming the targets to `new_name`: the references, and the references that local bindings named `new_name`
	/// would capture (`let new = 1; old()` would become `new()`, calling the variable), with the module of the code
	/// containing them and what the binding is (`local variable`, ...).
	pub(crate) fn for_rename(
		resolver: &Resolver<'_>,
		targets: &[ItemId],
		options: &ReferenceOptions,
		new_name: &str,
	) -> (Self, Vec<(Reference, ItemId, &'static str)>) {
		let mut targets = Targets::new(resolver.workspace(), targets);

		targets.shadow = Some(TargetName::new(new_name.into()));

		let (references, captures) = search_targets(resolver, &targets, options);
		let captures = captures.into_iter().map(|capture| (capture.reference, capture.module, capture.binding)).collect();

		(references, captures)
	}
}

/// A reference that a local binding named like the new name of its target would capture.
#[derive(Debug, Clone)]
pub(super) struct Capture {
	pub(super) reference: Reference,

	/// The module of the code containing the reference.
	pub(super) module: ItemId,

	/// What the binding is (`local variable`, ...).
	pub(super) binding: &'static str,
}

pub(crate) fn find_references(resolver: &Resolver<'_>, targets: &[ItemId], options: &ReferenceOptions) -> References {
	search_targets(resolver, &Targets::new(resolver.workspace(), targets), options).0
}

/// References to the targets, and references through lost bindings and to dead names (see [`Targets::lost`]), whose
/// target is the import of the binding.
pub(crate) fn find_references_through(
	resolver: &Resolver<'_>,
	targets: &[ItemId],
	lost: &[LostBinding],
	dead: &[DeadName],
	options: &ReferenceOptions,
) -> References {
	search_targets(resolver, &Targets::new(resolver.workspace(), targets).with_lost(lost, dead), options).0
}

fn search_targets(resolver: &Resolver<'_>, targets: &Targets, options: &ReferenceOptions) -> (References, Vec<Capture>) {
	let ws = resolver.workspace();
	let mut references = definitions(ws, targets);
	let mut notes = Vec::new();
	let mut captures = Vec::new();

	for outcome in search(resolver, targets, options, &jobs(ws, targets)) {
		references.extend(outcome.references);
		notes.extend(outcome.note);
		captures.extend(outcome.captures);
	}

	notes.sort();
	notes.dedup();

	// (files loaded by several crates are walked once per crate)
	captures.sort_by(|a, b| (&a.reference.path, a.reference.range.start).cmp(&(&b.reference.path, b.reference.range.start)));
	captures.dedup_by(|later, kept| later.reference.path == kept.reference.path && later.reference.range == kept.reference.range);

	let references = References {
		references: sorted(references),
		notes,
	};

	(references, captures)
}

/// The targets, grouped by name.
#[derive(Debug, Default)]
pub(super) struct Targets {
	names: Vec<TargetName>,

	/// A name whose local bindings are tracked too (the new name of a rename, without targets), to find references they
	/// would capture.
	pub(super) shadow: Option<TargetName>,

	/// Bindings of module scopes that come through removed imports (see [`Resolver::lost_bindings`]), by name: path
	/// segments that name something through them are references too.
	pub(super) lost: FxHashMap<SmolStr, Vec<LostBinding>>,

	/// Names that removed imports that resolve to nothing bound in modules (see [`Resolver::dead_names`]), by name:
	/// path segments that fail to resolve at them are references too.
	pub(super) dead: FxHashMap<SmolStr, Vec<DeadName>>,
}

impl Targets {
	fn new(ws: &Workspace, items: &[ItemId]) -> Self {
		let mut names: Vec<TargetName> = Vec::new();
		let mut seen = FxHashSet::default();

		for &item in items {
			let data = ws.item(item);

			let Some(name) = data.name.as_ref().filter(|name| *name != "_") else {
				continue;
			};

			if !seen.insert(item) {
				continue;
			}

			let index = match names.iter().position(|target| target.name == *name) {
				Some(index) => index,

				None => {
					names.push(TargetName::new(name.clone()));
					names.len() - 1
				}
			};

			let target = &mut names[index];

			target.items.push(item);

			if is_method(data) {
				target.methods.push(item);
			}

			if is_trait_item(ws, item) {
				target.trait_items.push(item);
			}
		}

		for target in &mut names {
			target.items.sort();
			target.methods.sort();
			target.trait_items.sort();
		}

		Self { names, shadow: None, lost: FxHashMap::default(), dead: FxHashMap::default() }
	}

	/// Also finds references through `lost` bindings, and to `dead` names.
	fn with_lost(mut self, lost: &[LostBinding], dead: &[DeadName]) -> Self {
		let names = lost.iter().map(|binding| &binding.name).chain(dead.iter().map(|dead| &dead.name));

		for name in names {
			if !self.names.iter().any(|target| target.name == *name) {
				self.names.push(TargetName::new(name.clone()));
			}
		}

		for binding in lost {
			self.lost.entry(binding.name.clone()).or_default().push(binding.clone());
		}

		for dead in dead {
			self.dead.entry(dead.name.clone()).or_default().push(dead.clone());
		}

		self
	}

	/// Whether local bindings named like the identifier are tracked: it is named like a target, or like
	/// [`Targets::shadow`].
	pub(super) fn tracks(&self, ident: &Ident) -> bool {
		self.named(ident).is_some() || self.shadow.as_ref().is_some_and(|shadow| shadow.matches(ident))
	}

	/// The targets named like an identifier (compared without `r#`).
	pub(super) fn named(&self, ident: &Ident) -> Option<&TargetName> {
		self.names.iter().find(|target| target.matches(ident))
	}

	/// The targets named `name` (unraw'd).
	pub(super) fn named_str(&self, name: &str) -> Option<&TargetName> {
		self.names.iter().find(|target| target.name == name)
	}

	/// Whether a text contains a target's name at all.
	pub(super) fn mentioned_in(&self, text: &str) -> bool {
		self.names.iter().any(|target| text.contains(target.name.as_str()))
	}

	pub(super) fn names(&self) -> impl Iterator<Item = &TargetName> {
		self.names.iter()
	}
}

/// The targets with one name.
#[derive(Debug)]
pub(super) struct TargetName {
	pub(super) name: SmolStr,

	/// `r#name`, which a raw identifier compares equal to.
	raw: String,

	/// Every target with the name, sorted.
	pub(super) items: Vec<ItemId>,

	/// Targets that are methods (associated functions with a `self` parameter).
	pub(super) methods: Vec<ItemId>,

	/// Targets that are items of traits or of trait `impl`s.
	pub(super) trait_items: Vec<ItemId>,
}

impl TargetName {
	fn new(name: SmolStr) -> Self {
		Self {
			raw: format!("r#{name}"),
			name,
			items: Vec::new(),
			methods: Vec::new(),
			trait_items: Vec::new(),
		}
	}

	fn matches(&self, ident: &Ident) -> bool {
		*ident == self.name || *ident == self.raw
	}

	/// Whether `item` is one of the targets.
	pub(super) fn contains(&self, item: ItemId) -> bool {
		self.items.binary_search(&item).is_ok()
	}

	/// The first target among resolutions.
	pub(super) fn find(&self, resolutions: &[Res]) -> Option<ItemId> {
		resolutions.iter().find_map(|res| match res {
			Res::Item(item) if self.contains(*item) => Some(*item),
			_ => None,
		})
	}

	/// The first target (there is always one).
	pub(super) fn first(&self) -> ItemId {
		self.items[0]
	}
}

fn is_method(data: &ItemData) -> bool {
	data.kind == ItemKind::AssocFn && data.fn_info().is_some_and(|info| info.receiver.is_some())
}

/// Whether an item is an item of a trait or of a trait `impl`.
fn is_trait_item(ws: &Workspace, item: ItemId) -> bool {
	if !ws.item(item).kind.is_associated() {
		return false;
	}

	ws.parent(item).is_some_and(|parent| {
		let data = ws.item(parent);

		data.kind == ItemKind::Trait || data.impl_info().is_some_and(|info| info.trait_path.is_some())
	})
}

/// The identifiers of the targets' definitions.
fn definitions(ws: &Workspace, targets: &Targets) -> Vec<Reference> {
	let mut references = Vec::new();

	for target in targets.names() {
		for &item in &target.items {
			let data = ws.item(item);
			let source = ws.file_of(item);

			let Some(range) = data.name_range.filter(|range| is_identifier(source.text().get(range.as_range()), &target.name)) else {
				continue;
			};

			references.push(Reference {
				target: item,
				kind: ReferenceKind::Definition,
				krate: item.krate(),
				file: data.file,
				path: source.path().to_path_buf(),
				range,
				start: source.line_col(range.start),
				certain: true,
			});
		}
	}

	references
}

/// Whether a text is the identifier `name`, possibly raw.
pub(super) fn is_identifier(text: Option<&str>, name: &str) -> bool {
	text.is_some_and(|text| text == name || text.strip_prefix("r#") == Some(name))
}

/// A file to walk, as the file of one or more modules of a crate.
#[derive(Debug)]
struct Job {
	krate: CrateId,
	file: FileId,
	modules: Vec<ItemId>,
	size: usize,
}

/// The files that mention a target's name (others cannot contain references), largest first.
fn jobs(ws: &Workspace, targets: &Targets) -> Vec<Job> {
	let mut jobs = Vec::new();

	for krate in ws.crates() {
		let mut by_file: BTreeMap<FileId, Vec<ItemId>> = BTreeMap::new();

		for (id, data) in krate.items() {
			if let Some(file) = data.module_info().and_then(|info| info.file) {
				by_file.entry(file).or_default().push(id);
			}
		}

		for (file, modules) in by_file {
			let text = krate.file(file).text();

			if targets.mentioned_in(text) {
				jobs.push(Job {
					krate: krate.id(),
					file,
					modules,
					size: text.len(),
				});
			}
		}
	}

	jobs.sort_by_key(|job| std::cmp::Reverse(job.size));
	jobs
}

/// What walking a file found.
#[derive(Debug, Default)]
struct Outcome {
	references: Vec<Reference>,
	captures: Vec<Capture>,
	note: Option<String>,
}

/// Walks the files of `jobs` on worker threads.
fn search(resolver: &Resolver<'_>, targets: &Targets, options: &ReferenceOptions, jobs: &[Job]) -> Vec<Outcome> {
	if jobs.is_empty() {
		return Vec::new();
	}

	let workers = std::thread::available_parallelism().map_or(1, usize::from).min(jobs.len());
	let next = AtomicUsize::new(0);

	let work = || {
		let mut outcomes = Vec::new();

		while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
			outcomes.push(walk(resolver, targets, options, job));
		}

		outcomes
	};

	std::thread::scope(|scope| {
		let handles: Vec<_> = (0..workers)
			.filter_map(|_| std::thread::Builder::new().stack_size(WORKER_STACK_SIZE).spawn_scoped(scope, work).ok())
			.collect();

		// without threads (when none could be started), everything is done here
		let mut outcomes = if handles.is_empty() { work() } else { Vec::new() };

		for handle in handles {
			match handle.join() {
				Ok(found) => outcomes.extend(found),
				Err(panic) => std::panic::resume_unwind(panic),
			}
		}

		outcomes
	})
}

/// Parses a file and walks it as each of its modules.
fn walk(resolver: &Resolver<'_>, targets: &Targets, options: &ReferenceOptions, job: &Job) -> Outcome {
	let ws = resolver.workspace();
	let source = ws.krate(job.krate).file(job.file);

	let parsed = match source.parse() {
		Ok(parsed) => parsed,

		Err(error) => {
			let path = ws.display_path(source.path()).display();
			let location = source.error_location(&error);

			return Outcome {
				references: Vec::new(),
				captures: Vec::new(),
				note: Some(format!("{path}:{location}: references in this file were not searched, since it does not parse: {error}")),
			};
		}
	};

	let mut outcome = Outcome::default();

	for &module in &job.modules {
		let mut walker = FileWalker::new(resolver, targets, options, &parsed, job.krate, job.file, module);

		walker.visit_file(&parsed.file);
		outcome.references.append(&mut walker.out);
		outcome.captures.append(&mut walker.captures);
	}

	outcome
}

/// Sorts references by path and position, keeping one reference per range: a certain one, of the first kind.
fn sorted(mut references: Vec<Reference>) -> Vec<Reference> {
	references.sort_by(|a, b| {
		let key = |reference: &Reference| (reference.range.start, reference.range.end, !reference.certain, reference.kind, reference.target);

		a.path.cmp(&b.path).then_with(|| key(a).cmp(&key(b)))
	});

	references.dedup_by(|later, kept| later.path == kept.path && later.range == kept.range);
	references
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::model::CrateSpec;

	fn reference(path: &str, start: usize, kind: ReferenceKind, certain: bool) -> Reference {
		Reference {
			target: ItemId::crate_root(CrateId(0)),
			kind,
			krate: CrateId(0),
			file: FileId(0),
			path: PathBuf::from(path),
			range: TextRange::new(start, start + 3),
			start: LineCol::default(),
			certain,
		}
	}

	#[test]
	fn sorts_and_keeps_the_best_reference_per_range() {
		let references = sorted(vec![
			reference("/b.rs", 4, ReferenceKind::Path, true),
			reference("/a.rs", 9, ReferenceKind::MacroToken, false),
			reference("/a.rs", 9, ReferenceKind::Path, true),
			reference("/a.rs", 1, ReferenceKind::MethodCall, false),
			reference("/b.rs", 4, ReferenceKind::Import, true),
		]);

		let summary: Vec<(&str, usize, ReferenceKind, bool)> = references
			.iter()
			.map(|reference| (reference.path.to_str().unwrap(), reference.range.start, reference.kind, reference.certain))
			.collect();

		assert_eq!(
			summary,
			[
				("/a.rs", 1, ReferenceKind::MethodCall, false),
				("/a.rs", 9, ReferenceKind::Path, true),
				("/b.rs", 4, ReferenceKind::Import, true),
			]
		);
	}

	#[test]
	fn identifiers_may_be_raw() {
		assert!(is_identifier(Some("type"), "type"));
		assert!(is_identifier(Some("r#type"), "type"));
		assert!(!is_identifier(Some("r#types"), "type"));
		assert!(!is_identifier(Some("Type"), "type"));
		assert!(!is_identifier(None, "type"));
	}

	#[test]
	fn no_targets_find_nothing() {
		let mut ws = Workspace::new("/nowhere");

		ws.load_crate(CrateSpec::new("missing", "/nowhere/src/lib.rs"));

		let resolver = Resolver::new(&ws);
		let found = find_references(&resolver, &[], &ReferenceOptions::all());

		assert!(found.references.is_empty() && found.notes.is_empty());

		// the crate root is named like its crate, but its (empty) file never mentions it
		let root = ws.crates()[0].root_module();
		let found = find_references(&resolver, &[root], &ReferenceOptions::all());

		assert!(found.references.is_empty() && found.notes.is_empty(), "{found:?}");
	}
}
