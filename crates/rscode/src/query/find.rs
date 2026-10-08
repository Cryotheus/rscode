//! Searching for items by name or path pattern.

use crate::Error;
use crate::cfg::Tristate;
use crate::model::ItemData;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::Workspace;
use crate::path::Anchor;
use crate::path::CanonicalPath;
use crate::pattern::MatchOptions;
use crate::pattern::PathPattern;
use crate::pattern::SegmentPattern;
use crate::resolve::Res;
use crate::resolve::Resolver;
use crate::resolve::Viewpoint;
use crate::source::LineCol;
use crate::source::TextRange;
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;

/// A builder for searching items.
///
/// Items are matched by their canonical path ([`Resolver::canonical_path`]): every `cfg` variant of an item is a match
/// of its own. Only crates that are part of the selection are searched, except by `::name` patterns, which also search
/// the other loaded crates. `impl` blocks are only matched by qualified patterns (`impl Trait for Type`,
/// `<Type as Trait>`), and qualified patterns only match `impl` blocks and their items.
///
/// Like paths everywhere else, patterns without wildcards also match what they name through re-exports
/// (`crate::Error` finds `my_crate::error::Error` when the crate root re-exports it), and so do the types of qualified
/// patterns (`<crate::Error as *>::*`).
///
/// ```no_run
/// # fn main() -> Result<(), rscode::Error> {
/// # let workspace: rscode::Workspace = todo!();
/// let matches = rscode::Find::new().pattern("*Error")?.kind(rscode::ItemKind::Enum).run(&workspace)?;
/// # Ok(()) }
/// ```
#[derive(Debug, Default, Clone)]
pub struct Find {
	options: FindOptions,
	match_options: MatchOptions,
}

impl Find {
	/// A search without patterns (matching every item) and default options.
	pub fn new() -> Self {
		Self::default()
	}

	/// A search with the given options.
	pub fn with_options(options: FindOptions) -> Self {
		Self {
			options,
			match_options: MatchOptions::default(),
		}
	}

	/// See [`FindOptions::active_only`].
	pub fn active_only(mut self, active_only: bool) -> Self {
		self.options.active_only = active_only;
		self
	}

	/// The match of an item if it is of a wanted kind, matches a pattern of the crate (`patterns`), is named by the path
	/// of a pattern (`named`), or the search has no patterns, and is not definitely inactive when only active items are
	/// wanted.
	fn find_item(
		&self,
		resolver: &Resolver<'_>,
		item: ItemId,
		data: &ItemData,
		patterns: &[(&PathPattern, &Resolved)],
		named: &HashSet<ItemId>,
	) -> Option<FindMatch> {
		let workspace = resolver.workspace();

		// (a path without wildcards names fields like other items, `Type::name` included, and macro invocations)
		let named_field = matches!(data.kind, ItemKind::Field | ItemKind::MacroCall) && self.options.kinds.is_empty() && named.contains(&item);
		let name = candidate_name(data).filter(|_| self.options.wants_kind(data.kind) || named_field)?;
		let path = resolver.canonical_path(item);
		let selected = workspace.krate(item.krate()).is_selected();

		// imports searched only for `use` patterns are only matched by those, fields for patterns with fields, and macro
		// invocations for patterns with `!`
		let use_patterns_only = data.kind == ItemKind::Import && !self.options.searches_imports();
		let field_patterns_only = data.kind == ItemKind::Field && !self.options.kinds.contains(&ItemKind::Field);
		let macro_patterns_only = data.kind == ItemKind::MacroCall && !self.options.kinds.contains(&ItemKind::MacroCall);

		let matched = self.options.patterns.is_empty()
			|| named.contains(&item)
			|| (patterns.iter())
				.filter(|(pattern, _)| (pattern.is_import() || !use_patterns_only) && (pattern.is_field() || !field_patterns_only))
				.filter(|(pattern, _)| pattern.is_macro_call() || !macro_patterns_only)
				.filter(|(pattern, _)| may_match(pattern, data.kind, name))
				.any(|(pattern, resolved)| {
					matches(workspace, pattern, item, &path, selected) || resolved.matches_owner(resolver, item, &path, selected)
				});

		if !matched {
			return None;
		}

		let active = workspace.is_active(item);

		if self.options.active_only && active == Tristate::False {
			return None;
		}

		Some(self.found(resolver, item, &path, active))
	}

	fn found(&self, resolver: &Resolver<'_>, item: ItemId, path: &CanonicalPath, active: Tristate) -> FindMatch {
		let workspace = resolver.workspace();
		let data = workspace.item(item);
		let krate = workspace.krate(item.krate());
		let file = workspace.file_of(item);
		let (start, end) = file.locate(data.range);

		FindMatch {
			item,
			path: path.to_string(),
			kind: data.kind,
			krate: krate.name().to_string(),
			package: krate.package().map(|package| workspace.package(package).name.to_string()),
			file: workspace.display_path(file.path()).to_path_buf(),
			start,
			end,
			range: data.range,
			visibility: data.vis.to_string(),
			cfg: workspace.effective_cfg(item).map(|cfg| cfg.to_string()),
			active,
			usable_paths: (self.options.from)
				.map(|viewpoint| resolver.usable_paths(item, viewpoint))
				.unwrap_or_default(),
			import_targets: match data.kind {
				ItemKind::Import => import_targets(resolver, item),
				_ => Vec::new(),
			},
			thread_local: data.is_thread_local(),
			entry_macro: workspace.entry_macro(item).map(str::to_owned),
		}
	}

	/// See [`FindOptions::from`].
	pub fn from(mut self, viewpoint: Viewpoint) -> Self {
		self.options.from = Some(viewpoint);
		self
	}

	/// Affects patterns added after this call.
	pub fn ignore_case(mut self, ignore_case: bool) -> Self {
		self.match_options.ignore_case = ignore_case;
		self
	}

	/// See [`FindOptions::imports`].
	pub fn imports(mut self, imports: bool) -> Self {
		self.options.imports = imports;
		self
	}

	/// Restricts the search to a kind of item (in addition to kinds added before).
	pub fn kind(mut self, kind: ItemKind) -> Self {
		self.options.kinds.push(kind);
		self
	}

	/// See [`FindOptions::limit`].
	pub fn limit(mut self, limit: usize) -> Self {
		self.options.limit = Some(limit);
		self
	}

	/// The options of the search.
	pub fn options(&self) -> &FindOptions {
		&self.options
	}

	/// Adds a parsed path pattern.
	pub fn path_pattern(mut self, pattern: PathPattern) -> Self {
		self.options.patterns.push(pattern);
		self
	}

	/// Adds a path pattern (see [`crate::pattern`]).
	pub fn pattern(mut self, pattern: &str) -> Result<Self, Error> {
		self.options.patterns.push(PathPattern::parse(pattern, self.match_options)?);
		Ok(self)
	}

	/// Searches the workspace, building a [`Resolver`].
	pub fn run(&self, workspace: &Workspace) -> Result<Vec<FindMatch>, Error> {
		self.run_with(&Resolver::new(workspace))
	}

	/// Searches with an existing [`Resolver`]. Matches are ordered by crate, file, then position.
	pub fn run_with(&self, resolver: &Resolver<'_>) -> Result<Vec<FindMatch>, Error> {
		let workspace = resolver.workspace();
		let resolved: Vec<Resolved> = self.options.patterns.iter().map(|pattern| Resolved::new(resolver, pattern)).collect();
		let named: HashSet<ItemId> = resolved.iter().flat_map(|resolved| resolved.items.iter().copied()).collect();
		let mut found = Vec::new();

		for krate in workspace.crates() {
			let selected = krate.is_selected();
			let patterns: Vec<(&PathPattern, &Resolved)> = (self.options.patterns.iter().zip(&resolved))
				.filter(|(pattern, _)| selected || searches_other_crates(pattern))
				.collect();
			let names_items = named.iter().any(|item| item.krate() == krate.id());

			// without patterns, every item of the selected crates matches
			if patterns.is_empty() && !names_items && !(selected && self.options.patterns.is_empty()) {
				continue;
			}

			for (item, data) in krate.items() {
				if let Some(matched) = self.find_item(resolver, item, data, &patterns, &named) {
					found.push(((item.krate(), workspace.file_of(item).path(), data.range.start, item), matched));
				}
			}
		}

		found.sort_by_key(|(key, _)| *key);

		let mut found: Vec<FindMatch> = found.into_iter().map(|(_, found)| found).collect();

		if let Some(limit) = self.options.limit {
			found.truncate(limit);
		}

		Ok(found)
	}
}

/// An item found by [`Find`].
#[derive(Debug, Clone, Serialize)]
pub struct FindMatch {
	/// The item.
	#[serde(skip)]
	pub item: ItemId,

	/// Canonical path (see [`crate::path::CanonicalPath`]); for imports, a `use` path (`use my_crate::a::Name`) that
	/// names the import itself.
	pub path: String,

	/// The kind of the item.
	pub kind: ItemKind,

	/// Crate (target) name.
	#[serde(rename = "crate")]
	pub krate: String,

	/// Package name.
	pub package: Option<String>,

	/// Path of the file, relative to the workspace root when possible.
	pub file: PathBuf,

	/// Start of the item (including attributes and doc comments).
	pub start: LineCol,

	/// End of the item (exclusive).
	pub end: LineCol,

	/// Byte range of the item in its file.
	#[serde(skip)]
	pub range: TextRange,

	/// The declared visibility (`pub`, `pub(crate)`, `private`, `inherited`, ...).
	pub visibility: String,

	/// The effective `cfg` predicate (the item's and its ancestors'), if any.
	pub cfg: Option<String>,

	/// Evaluation of [`FindMatch::cfg`].
	pub active: Tristate,

	/// Paths usable from the requested viewpoint (empty when none was requested or none is visible).
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub usable_paths: Vec<String>,

	/// Whether the item is a static declared by `thread_local!` (a `LocalKey` of its declared type).
	#[serde(skip_serializing_if = "std::ops::Not::not")]
	pub thread_local: bool,

	/// For a static declared by an entry of a macro invocation other than `thread_local!`: the macro's name.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub entry_macro: Option<String>,

	/// For imports: the canonical paths of what they import (for glob imports, of the modules and enums they import
	/// from), and paths outside of the loaded crates as written.
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub import_targets: Vec<String>,
}

/// Options for [`Find`].
#[derive(Debug, Default, Clone)]
pub struct FindOptions {
	/// An item matches when it matches any pattern (every item matches when there are none).
	pub patterns: Vec<PathPattern>,

	/// Only these kinds (all kinds when empty). [`ItemKind::Import`]s are included when asked for here, by
	/// [`FindOptions::imports`], or by a `use` pattern, [`ItemKind::Field`]s when asked for here or by a pattern with a
	/// field (`Point.*`), and [`ItemKind::MacroCall`]s (in modules) when asked for here or by a pattern ending with `!`;
	/// `use` items, other macro invocations, and `extern` blocks never match.
	pub kinds: Vec<ItemKind>,

	/// Exclude items whose `cfg` is definitely disabled.
	pub active_only: bool,

	/// Compute [`FindMatch::usable_paths`] from this viewpoint.
	pub from: Option<Viewpoint>,

	/// Include [`ItemKind::Import`]s (with their resolved targets). Their paths are `use` paths
	/// (`use my_crate::a::Name`), which name the imports themselves. `use` patterns search imports anyway.
	pub imports: bool,

	/// Keep only the first this many matches (in order).
	pub limit: Option<usize>,
}

impl FindOptions {
	/// Whether imports are asked for (by [`FindOptions::imports`] or the kinds), for every pattern.
	fn searches_imports(&self) -> bool {
		self.imports || self.kinds.contains(&ItemKind::Import)
	}

	/// Whether items of a kind are searched.
	fn wants_kind(&self, kind: ItemKind) -> bool {
		match kind {
			ItemKind::Import => self.searches_imports() || self.patterns.iter().any(PathPattern::is_import),
			ItemKind::Field => self.kinds.contains(&kind) || self.patterns.iter().any(PathPattern::is_field),
			ItemKind::MacroCall => self.kinds.contains(&kind) || self.patterns.iter().any(PathPattern::is_macro_call),
			kind => self.kinds.is_empty() || self.kinds.contains(&kind),
		}
	}
}

/// What a pattern names as a path, which it matches besides the canonical paths it matches: paths go through
/// re-exports.
struct Resolved {
	/// For a pattern without wildcards (or ignored case): the items its path names.
	items: Vec<ItemId>,

	/// For a qualified pattern whose type has no wildcards: the types that type names, and the pattern with any type,
	/// which matches the rest.
	owners: Option<(Vec<ItemId>, PathPattern)>,
}

impl Resolved {
	fn new(resolver: &Resolver<'_>, pattern: &PathPattern) -> Self {
		let items = pattern.to_item_path().map(|path| resolver.resolve_item_path(&path)).unwrap_or_default();
		let owners = (pattern.qualifier.as_ref())
			.and_then(|(self_ty, _)| self_ty.to_item_path())
			.map(|self_ty| {
				let mut any_type = pattern.clone();

				// (the type's generic arguments still have to match)
				if let Some((self_ty, _)) = &mut any_type.qualifier {
					self_ty.anchor = Anchor::None;
					self_ty.segments = vec![SegmentPattern::AnyDepth];
				}

				(resolver.resolve_item_path(&self_ty), any_type)
			});

		Self { items, owners }
	}

	/// Whether an `impl` block, or an item of one, is of a type the qualified pattern names (and matches the rest).
	fn matches_owner(&self, resolver: &Resolver<'_>, item: ItemId, path: &CanonicalPath, selected: bool) -> bool {
		let Some((owners, any_type)) = &self.owners else {
			return false;
		};

		let workspace = resolver.workspace();
		let impl_block = match workspace.item(item).kind {
			ItemKind::Impl => item,

			_ => match workspace.parent(item) {
				Some(parent) if workspace.item(parent).kind == ItemKind::Impl => parent,
				_ => return false,
			},
		};

		resolver.impl_self_types(impl_block).iter().any(|ty| owners.contains(ty)) && any_type.matches(path, selected)
	}
}

/// The name an item is matched by (the last segment of its canonical path), or `None` if it is never found: `use`
/// items, macro invocations outside of modules (and of macros that were not understood), `extern` blocks, and unnamed
/// items (`const _`). `impl` blocks have an empty name, glob imports `*`, underscore imports `_`, and macro
/// invocations the name of their macro.
fn candidate_name(data: &ItemData) -> Option<&str> {
	match data.kind {
		ItemKind::Impl => Some(""),
		ItemKind::MacroCall => data.macro_name(),

		// like `ImportInfo::path_name`
		ItemKind::Import => match &data.name {
			Some(name) => Some(name),
			None if data.import_info().is_some_and(|info| info.glob) => Some("*"),
			None => Some("_"),
		},

		kind if kind.is_nameable() => data.name.as_deref(),
		_ => None,
	}
}

/// What an import imports, as canonical paths (paths outside of the loaded crates as they are), without duplicates.
fn import_targets(resolver: &Resolver<'_>, import: ItemId) -> Vec<String> {
	let mut targets: Vec<String> = Vec::new();

	for res in resolver.import_targets(import) {
		let target = match res {
			Res::Item(target) => resolver.canonical_path(target).to_string(),
			Res::External(path) | Res::Builtin(path) => path.to_string(),
		};

		if !targets.contains(&target) {
			targets.push(target);
		}
	}

	targets
}

/// Whether an item with a canonical path matches a pattern. Qualified patterns only match `impl` blocks and their
/// items.
fn matches(workspace: &Workspace, pattern: &PathPattern, item: ItemId, path: &CanonicalPath, selected: bool) -> bool {
	let in_impl =
		|| workspace.item(item).kind == ItemKind::Impl || workspace.parent(item).is_some_and(|parent| workspace.item(parent).kind == ItemKind::Impl);

	(!pattern.is_qualified() || in_impl()) && pattern.matches(path, selected)
}

/// Whether an item could match a pattern, judging by its kind and name only (before computing its canonical path).
fn may_match(pattern: &PathPattern, kind: ItemKind, name: &str) -> bool {
	let is_impl = kind == ItemKind::Impl;

	match pattern.segments.last() {
		// `<Type as Trait>` and `impl Trait for Type` match `impl` blocks, and only those
		None if pattern.is_qualified() => is_impl,

		_ if is_impl => false,

		// `use` patterns match imports, and only those, patterns with a field fields, and patterns with `!` macro
		// invocations
		_ if pattern.is_import() && kind != ItemKind::Import => false,
		_ if pattern.is_macro_call() && kind != ItemKind::MacroCall => false,
		_ if pattern.is_field() => kind == ItemKind::Field && pattern.field.as_ref().is_some_and(|field| field.matches(name)),

		Some(SegmentPattern::Ident(last)) => last.matches(name),
		_ => true,
	}
}

/// Whether a pattern searches crates outside of the selection: `::name` patterns, and qualified patterns whose type
/// is one.
fn searches_other_crates(pattern: &PathPattern) -> bool {
	match &pattern.qualifier {
		Some((self_ty, _)) => self_ty.anchor == Anchor::Global,
		None => pattern.anchor == Anchor::Global,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn kinds_include_imports_when_asked() {
		let mut options = FindOptions::default();

		assert!(options.wants_kind(ItemKind::Struct));
		assert!(!options.wants_kind(ItemKind::Import));

		options.imports = true;
		assert!(options.wants_kind(ItemKind::Import));

		options.imports = false;
		options.kinds = vec![ItemKind::Import];
		assert!(options.wants_kind(ItemKind::Import));
		assert!(!options.wants_kind(ItemKind::Struct));

		options.imports = true;
		options.kinds = vec![ItemKind::Fn];
		assert!(options.wants_kind(ItemKind::Import));
		assert!(options.wants_kind(ItemKind::Fn));
		assert!(!options.wants_kind(ItemKind::Struct));
	}

	#[test]
	fn other_crates_are_searched_by_global_patterns() {
		assert!(searches_other_crates(&pattern("::dep::Item")));
		assert!(searches_other_crates(&pattern("::*::**")));
		assert!(searches_other_crates(&pattern("<::dep::Type as *>::method")));
		assert!(!searches_other_crates(&pattern("Item")));
		assert!(!searches_other_crates(&pattern("crate::Item")));
		assert!(!searches_other_crates(&pattern("dep::Item")));
		assert!(!searches_other_crates(&pattern("<Type as ::dep::Trait>")));
	}

	fn pattern(text: &str) -> PathPattern {
		PathPattern::parse(text, MatchOptions::default()).unwrap()
	}

	#[test]
	fn prefilters_by_kind_and_name() {
		assert!(may_match(&pattern("Circle"), ItemKind::Struct, "Circle"));
		assert!(!may_match(&pattern("Circle"), ItemKind::Struct, "Square"));
		assert!(may_match(&pattern("C*"), ItemKind::Struct, "Circle"));
		assert!(may_match(&pattern("shapes::**"), ItemKind::Struct, "Circle"));
		assert!(may_match(&pattern("crate"), ItemKind::Module, "demo"));
		assert!(!may_match(&pattern("*"), ItemKind::Impl, ""));
		assert!(!may_match(&pattern("**"), ItemKind::Impl, ""));
		assert!(may_match(&pattern("impl Shape for *"), ItemKind::Impl, ""));
		assert!(!may_match(&pattern("impl Shape for *"), ItemKind::AssocFn, "area"));
		assert!(may_match(&pattern("<* as Shape>::area"), ItemKind::AssocFn, "area"));
		assert!(!may_match(&pattern("<* as Shape>::area"), ItemKind::Impl, ""));
		assert!(may_match(
			&PathPattern::parse("circle", MatchOptions { ignore_case: true }).unwrap(),
			ItemKind::Struct,
			"Circle"
		));
	}
}
