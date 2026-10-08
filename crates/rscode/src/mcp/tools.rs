//! The work of every tool, run on a fresh thread per call (see [`super::worker`]).
//!
//! Each call loads the workspace from disk, so changes made by anything else are always seen. Failures become
//! messages for the client (tool errors), with hints for the common mistakes.

use super::params::FindParams;
use super::params::FormatParams;
use super::params::InsertParams;
use super::params::RemoveParams;
use super::params::RenameParams;
use super::params::ReplaceParams;
use super::params::ViewParams;
use super::render;
use super::render::Page;
use super::sources::WriteScope;
use crate::Error;
use crate::Find;
use crate::FindMatch;
use crate::ItemId;
use crate::ItemKind;
use crate::ItemPath;
use crate::MatchOptions;
use crate::PathPattern;
use crate::Resolver;
use crate::Tristate;
use crate::View;
use crate::Viewpoint;
use crate::Workspace;
use crate::edit;
use crate::edit::Applied;
use crate::edit::EditSet;
use crate::edit::FileChange;
use crate::edit::FmtOptions;
use crate::model::UnloadedMember;
use crate::query::shown_item;
use crate::rscode_fmt::FormatOptions;
use crate::rscode_fmt::RsFormatter;
use crate::workspace::LoadOptions;
use crate::workspace::load_workspace;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

/// A tool's result: text for the client, or an error message.
pub(crate) type Output = Result<String, String>;

/// How to pick one of several crates that a path names items of.
const SELECT_ONE_CRATE: &str = "the path names items of several crates (such as the library and a binary of a \
	package, whose roots are both `crate`): select one with `lib` (the library) or `bin` (binaries, by name)";

const UNFORMATTED_DIFF: &str = "note: `format` only applies when writing; the diff is not formatted\n";

/// What an editing call may write, checked right before writing: a cancelled edit, or one that would change files
/// outside of its scope, writes nothing.
pub(crate) struct Permit<'a> {
	/// Whether the client cancelled the request.
	pub(crate) cancelled: &'a dyn Fn() -> bool,

	/// Where the edits may be written.
	pub(crate) scope: &'a WriteScope,
}

/// Where `find_items` computes usable paths from.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum UsableFrom {
	/// The root module of each item's own crate.
	CrateRoot,

	Viewpoint(Viewpoint),
}

impl UsableFrom {
	fn viewpoint(self, item: ItemId) -> Viewpoint {
		match self {
			Self::CrateRoot => Viewpoint::Module(ItemId::crate_root(item.krate())),
			Self::Viewpoint(viewpoint) => viewpoint,
		}
	}
}

/// A message for the client, with a hint on how to go on after common mistakes.
fn describe(error: &Error) -> String {
	let hint = match error {
		Error::NotFound(_) => {
			"search with `find_items` (e.g. the pattern `*name*` with `ignore_case`, and `include_imports` for imports) \
			 for the exact path"
		}

		// several imports of a module (`use` paths are never ambiguous through imports)
		Error::Ambiguous { path, .. } if path.starts_with("use ") => {
			"the `use` path names several imports of its module: remove them with `remove_items`, and insert the new \
			 `use` item with `insert_items`"
		}

		// a path through a private import (see `edit::remove`)
		Error::Ambiguous { candidates, .. } if candidates.iter().any(|candidate| candidate.starts_with("`use ")) => {
			"the path names an item through a private import: pass the import's `use` path to name the import itself, or \
			 the item's own path"
		}

		Error::Ambiguous { .. } => "use one of the candidates' paths",
		Error::Collision { .. } => "set `force` to proceed anyway",

		Error::PathParse(_) => {
			"paths look like `crate::a::Item`, `::crate_name::Item`, `a::Item`, `Type::method`, `<Type as Trait>::method`, \
			 `impl Trait for Type`, `impl Type[method]` (one of several blocks), `Type.field`, `a::macro_name!`, or \
			 `use crate::a::Name` (an import itself); patterns may use `*` and `**`"
		}

		_ => return error.to_string(),
	};

	format!("{error}\nhint: {hint}")
}

/// [`describe`], for an error of an operation: when a path names nothing, workspace members that are not loaded may
/// have it, or it may name imports (of items that are not loaded) or be a `use` path.
fn describe_in(error: &Error, resolver: &Resolver<'_>) -> String {
	let Error::NotFound(path) = error else {
		return describe(error);
	};

	let workspace = resolver.workspace();
	let path = ItemPath::parse(path).ok();
	let member = path.as_ref().and_then(|path| workspace.unloaded_member_of(path));

	// a selector that picks none of the blocks a loaded header names: the members do not matter
	if let Some(hint) = path.as_ref().and_then(|path| resolver.selector_hint(path)) {
		return format!("{error}\nhint: {hint}");
	}

	match (member, unloaded_hint(workspace, member)) {
		// the crate is known: searching the loaded crates would not help
		(Some(_), Some(hint)) => format!("{error}\nhint: {hint}"),

		(None, Some(hint)) => format!("{}\nhint: {hint}", describe(error)),

		(_, None) => match path.and_then(|path| resolver.import_hint(&path)) {
			Some(hint) => format!("{error}\nhint: {hint}"),
			None => describe(error),
		},
	}
}

/// Fails when a file no longer has the contents the edits were planned on.
fn ensure_unchanged(root: &Path, changes: &[FileChange]) -> Result<(), String> {
	for change in changes {
		let current = std::fs::read_to_string(&change.path).ok();

		if current.as_deref() != Some(change.original.as_str()) {
			return Err(format!(
				"{} changed on disk while the edit was being planned; nothing was written (try again)",
				render::relative(root, &change.path).display()
			));
		}
	}

	Ok(())
}

/// `find_items`
pub(crate) fn find(load: &LoadOptions, params: &FindParams) -> Output {
	let kinds = params.kinds()?;
	let mut find = Find::new()
		.ignore_case(params.ignore_case)
		.active_only(params.active_only)
		.imports(params.imports(&kinds))
		.pattern(&params.pattern)
		.map_err(|error| describe(&error))?;

	for &kind in &kinds {
		find = find.kind(kind);
	}

	let workspace = self::load(load)?;
	let resolver = Resolver::new(&workspace);
	let from = params
		.from
		.as_deref()
		.map(str::trim)
		.filter(|from| !from.is_empty())
		.map(|from| usable_from(&resolver, from))
		.transpose()?;
	let found = find.run_with(&resolver).map_err(|error| describe(&error))?;
	let total = found.len();
	let mut page: Vec<FindMatch> = found.into_iter().skip(params.offset).take(params.limit).collect();

	// only for the matches shown: finding usable paths searches the module graph
	if let Some(from) = from {
		for found in &mut page {
			found.usable_paths = resolver.usable_paths(found.item, from.viewpoint(found.item));
		}
	}

	let page_info = Page {
		offset: params.offset,
		limit: params.limit,
		total,
	};
	let mut text = render::find(workspace.root(), &params.pattern, &page, page_info, from.is_some());

	let member = pattern_crate(&params.pattern).and_then(|krate| workspace.unloaded_member_with_crate(krate));

	if total == 0
		&& let Some(hint) = unloaded_hint(&workspace, member)
	{
		writeln!(text, "hint: {hint}").unwrap();
	}

	text.push_str(&render::load_errors_note(&workspace));
	Ok(text)
}

/// Appends the diff for a dry run, or writes the edits.
fn finish(root: &Path, edits: &EditSet, dry_run: bool, permit: &Permit<'_>, text: &mut String) -> Result<(), String> {
	match dry_run {
		true => {
			let changes = edits.preview().map_err(|error| describe(&error))?;
			let diff = render::diff(root, &changes, edits.moves(), edits.deletions());

			text.push_str("nothing was written (dry run)\n");

			if !diff.is_empty() {
				text.push('\n');
				text.push_str(&diff);
			}
		}

		false => {
			for warning in write(root, edits, permit)?.warnings {
				writeln!(text, "warning: {warning}").unwrap();
			}
		}
	}

	Ok(())
}

/// `format_items`
pub(crate) fn format(load: &LoadOptions, params: &FormatParams, permit: &Permit<'_>) -> Output {
	let options = params.options()?;
	let targets = params
		.targets()
		.iter()
		.map(|target| PathPattern::parse(target, MatchOptions::default()).map_err(|error| describe(&error.into())))
		.collect::<Result<Vec<_>, _>>()?;
	let workspace = self::load(load)?;
	let resolver = Resolver::new(&workspace);
	let formatting = edit::format(&resolver, &targets, &options).map_err(|error| describe(&error))?;
	let root = workspace.root();

	if params.check {
		let mut text = render::format_check(root, &formatting.changes, &formatting.warnings);
		let diff = render::diff(root, &formatting.changes, &[], &[]);

		if !diff.is_empty() {
			text.push('\n');
			text.push_str(&diff);
		}

		return Ok(text);
	}

	let applied = write(root, &formatting.edits, permit)?;

	Ok(render::format_written(
		root,
		formatting.changes.len(),
		&applied.written,
		&formatting.warnings,
	))
}

/// Formats items with rustfmt (without sorting) after an edit wrote them, loading the workspace again to see the
/// edit. `targets` pairs a label for messages with each item's path.
///
/// Returns lines for the report; the edit is already written, so failing to format it is only a warning.
fn format_written(load: &LoadOptions, permit: &Permit<'_>, targets: &[(String, ItemPath)]) -> String {
	match try_format_written(load, permit, targets) {
		Ok(report) => report,
		Err(error) => format!("warning: the edit was written, but formatting it failed: {error}\n"),
	}
}

/// Whether the items `path` names are in several crates.
fn in_several_crates(resolver: &Resolver<'_>, path: &ItemPath) -> bool {
	let crates: HashSet<_> = resolver.resolve_item_path(path).iter().map(|item| item.krate()).collect();

	crates.len() > 1
}

/// `insert_items`
pub(crate) fn insert(load: &LoadOptions, params: &InsertParams, permit: &Permit<'_>) -> Output {
	let options = params.options()?;
	let parent = params.parent().map(parse_path).transpose()?;
	let workspace = self::load(load)?;
	let resolver = Resolver::new(&workspace);
	let plan = edit::insert(&resolver, parent.as_ref(), &params.source, &options).map_err(|error| match (&error, &parent) {
		(Error::Ambiguous { .. }, Some(parent)) if in_several_crates(&resolver, parent) => {
			format!("{error}\nhint: {SELECT_ONE_CRATE}")
		}

		// several containers have the anchor
		(Error::Ambiguous { .. }, None) => format!("{error}\nhint: pass one of the candidates as `parent`"),

		_ => describe_in(&error, &resolver),
	})?;
	let root = workspace.root();
	let mut text = render::insertion(root, &plan, params.parent().unwrap_or(&plan.parent), params.dry_run);

	if params.format && params.dry_run {
		text.push_str(UNFORMATTED_DIFF);
	}

	finish(root, &plan.edits, params.dry_run, permit, &mut text)?;

	if params.format && !params.dry_run {
		// (the canonical path of the container found from the anchor is a path to it)
		let parent = match parent {
			Some(parent) => parent,
			None => parse_path(&plan.parent)?,
		};
		let child = |name: &str, import: bool| {
			let mut path = ItemPath { import, ..parent.clone() };

			path.segments.push(name.into());

			let label = if import { format!("use {name}") } else { name.to_owned() };

			(label, path)
		};

		let named = plan
			.inserted
			.iter()
			.filter_map(|(_, name)| name.as_deref())
			.map(|name| child(name, false));

		// `use` items, by their imports (not in `impl` blocks: `use` items are not items of those)
		let imports = plan
			.imports
			.iter()
			.filter(|_| parent.qualifier.is_none())
			.map(|(_, name)| child(name, true));
		let targets: Vec<(String, ItemPath)> = named.chain(imports).collect();

		let unnamed = (plan.inserted.iter().enumerate())
			.filter(|(index, (_, name))| name.is_none() && !plan.imports.iter().any(|(import_of, _)| import_of == index))
			.count();

		if unnamed > 0 {
			text.push_str("note: unnamed items (like `impl` blocks) are not formatted; use `format_items` on the parent\n");
		}

		text.push_str(&format_written(load, permit, &targets));
	}

	Ok(text)
}

fn load(options: &LoadOptions) -> Result<Workspace, String> {
	load_workspace(options).map_err(|error| {
		let mut message = format!("failed to load the workspace: {error}");

		if options.manifest_path.is_none()
			&& let Ok(directory) = std::env::current_dir()
			&& !directory.ancestors().any(|directory| directory.join("Cargo.toml").is_file())
		{
			write!(
				message,
				"\nhint: the server looks for Cargo.toml from its working directory ({}) upwards; start it with \
				 `--manifest-path /path/to/Cargo.toml`",
				directory.display()
			)
			.unwrap();
		}

		message
	})
}

fn parse_path(text: &str) -> Result<ItemPath, String> {
	ItemPath::parse(text).map_err(|error| describe(&error.into()))
}

/// The crate a pattern starts from (`::name::…`, or `name::…`), when that segment has no wildcards.
fn pattern_crate(pattern: &str) -> Option<&str> {
	let pattern = pattern.trim();
	let pattern = pattern
		.strip_prefix("use")
		.filter(|rest| rest.starts_with(char::is_whitespace))
		.unwrap_or(pattern)
		.trim();
	let first = pattern.strip_prefix("::").unwrap_or(pattern).split("::").next()?;

	(!first.is_empty() && !first.contains('*') && !pattern.starts_with('<')).then_some(first)
}

/// `remove_items`
pub(crate) fn remove(load: &LoadOptions, params: &RemoveParams, permit: &Permit<'_>) -> Output {
	if params.paths.is_empty() {
		return Err("`paths` is empty: give the paths of the items to remove".to_owned());
	}

	let paths = params.paths.iter().map(|path| parse_path(path)).collect::<Result<Vec<_>, _>>()?;
	let workspace = self::load(load)?;
	let resolver = Resolver::new(&workspace);
	let plan = edit::remove(&resolver, &paths, &params.options()).map_err(|error| describe_in(&error, &resolver))?;
	let root = workspace.root();
	let mut text = render::removal(root, &plan, params.dry_run);

	finish(root, &plan.edits, params.dry_run, permit, &mut text)?;
	Ok(text)
}

/// `rename_item`
pub(crate) fn rename(load: &LoadOptions, params: &RenameParams, permit: &Permit<'_>) -> Output {
	let path = parse_path(&params.path)?;
	let workspace = self::load(load)?;
	let resolver = Resolver::new(&workspace);
	let plan = edit::rename(&resolver, &path, params.new_name.trim(), &params.options()).map_err(|error| match error {
		Error::InvalidIdent(_) => {
			format!("{error}\nhint: `new_name` is the new identifier alone (e.g. `parse_config`), not a path")
		}

		error => describe_in(&error, &resolver),
	})?;
	let root = workspace.root();
	let mut text = render::rename(root, &plan, params.new_name.trim(), params.dry_run);

	finish(root, &plan.edits, params.dry_run, permit, &mut text)?;
	Ok(text)
}

/// `replace_item`
pub(crate) fn replace(load: &LoadOptions, params: &ReplaceParams, permit: &Permit<'_>) -> Output {
	let path = parse_path(&params.path)?;
	let workspace = self::load(load)?;
	let resolver = Resolver::new(&workspace);
	let plan = edit::replace(&resolver, &path, &params.source, &params.options()).map_err(|error| {
		let all_variants = matches!(error, Error::Ambiguous { .. }) && edit::replaces_all_variants(&resolver, &path);

		match error {
			Error::Ambiguous { .. } if all_variants && in_several_crates(&resolver, &path) => {
				format!("{error}\nhint: {SELECT_ONE_CRATE}, or set `all_variants` to replace every one of them")
			}

			Error::Ambiguous { .. } if all_variants => {
				format!("{error}\nhint: set `all_variants` to replace every one of them")
			}

			// `Type::name` names a method, and the source is the field `Type.name`
			Error::InvalidSource(_) if let Some(hint) = resolver.field_hint(&path) => format!("{error}\nhint: {hint}"),

			error => describe_in(&error, &resolver),
		}
	})?;
	let root = workspace.root();
	let mut text = render::replacement(root, &plan, params.dry_run);

	if params.format && params.dry_run {
		text.push_str(UNFORMATTED_DIFF);
	}

	finish(root, &plan.edits, params.dry_run, permit, &mut text)?;

	if params.format && !params.dry_run {
		text.push_str(&format_written(load, permit, &[(params.path.clone(), path)]));
	}

	Ok(text)
}

fn try_format_written(load: &LoadOptions, permit: &Permit<'_>, targets: &[(String, ItemPath)]) -> Result<String, String> {
	if targets.is_empty() {
		return Ok(String::new());
	}

	let workspace = self::load(load)?;
	let resolver = Resolver::new(&workspace);
	let mut report = String::new();
	let mut patterns = Vec::new();

	for (label, path) in targets {
		if resolver.resolve_item_path(path).is_empty() {
			writeln!(
				report,
				"note: `{label}` was not formatted: its path no longer names an item (was it renamed?)"
			)
			.unwrap();
			continue;
		}

		// (not a parsed pattern: `*` is a wildcard in those)
		patterns.push(PathPattern::exact(path));
	}

	if patterns.is_empty() {
		return Ok(report);
	}

	let options = FmtOptions {
		format: FormatOptions::new().formatter(RsFormatter::RustFmt).sort(None),
		skip_children: true,
		active_only: false,
	};
	let formatting = edit::format(&resolver, &patterns, &options).map_err(|error| error.to_string())?;
	let root = workspace.root();

	permit.scope.check(root, &formatting.edits)?;

	let applied = formatting.edits.apply().map_err(|error| error.to_string())?;

	for warning in &formatting.warnings {
		writeln!(report, "warning: {warning}").unwrap();
	}

	match applied.written.is_empty() {
		true => report.push_str("formatting changed nothing\n"),

		false => {
			let files: Vec<String> = applied
				.written
				.iter()
				.map(|file| render::relative(root, file).display().to_string())
				.collect();

			writeln!(report, "formatted {}", files.join(", ")).unwrap();
		}
	}

	Ok(report)
}

/// How to include the workspace members that are not loaded: `member` (whose crate was asked for) if given, or else
/// all of them.
fn unloaded_hint(workspace: &Workspace, member: Option<&UnloadedMember>) -> Option<String> {
	if let Some(member) = member {
		return Some(format!(
			"`{}` is a workspace member that is not selected, so its crates are not loaded: add it to `packages`, or set \
			 `workspace` to true",
			member.name
		));
	}

	let names: Vec<&str> = workspace.unloaded_members().iter().map(|member| member.name.as_str()).collect();

	match names.as_slice() {
		[] => None,

		[name] => Some(format!(
			"the workspace member `{name}` is not selected, so its crates were not searched: add it to `packages`, or \
			 set `workspace` to true"
		)),

		_ => Some(format!(
			"{} workspace members are not selected, so their crates were not searched ({}): add them to `packages`, or \
			 set `workspace` to true",
			names.len(),
			names.join(", ")
		)),
	}
}

/// Parses `find_items`' `from`: `crate`, `::`, or the path of a module.
fn usable_from(resolver: &Resolver<'_>, text: &str) -> Result<UsableFrom, String> {
	match text {
		"crate" => return Ok(UsableFrom::CrateRoot),
		"::" => return Ok(UsableFrom::Viewpoint(Viewpoint::Foreign)),
		_ => {}
	}

	let path = parse_path(text)?;
	let workspace = resolver.workspace();
	let modules: Vec<ItemId> = resolver
		.resolve_item_path(&path)
		.into_iter()
		.filter(|&item| workspace.item(item).kind == ItemKind::Module)
		.collect();
	let active: Vec<ItemId> = modules
		.iter()
		.copied()
		.filter(|&module| workspace.is_active(module) != Tristate::False)
		.collect();

	match (modules.as_slice(), active.as_slice()) {
		([module], _) | (_, [module]) => Ok(UsableFrom::Viewpoint(Viewpoint::Module(*module))),

		([], _) => Err(format!(
			"`from`: `{text}` does not name a module (give `crate`, `::`, or the path of a module)"
		)),

		_ => {
			let candidates: Vec<String> = modules.iter().map(|&module| format!("  {}", resolver.canonical_path(module))).collect();

			Err(format!("`from`: `{text}` names several modules:\n{}", candidates.join("\n")))
		}
	}
}

/// `view_items`
pub(crate) fn view(load: &LoadOptions, params: &ViewParams) -> Output {
	if params.paths.is_empty() {
		return Err("`paths` is empty: give the paths of the items to show".to_owned());
	}

	let workspace = self::load(load)?;
	let resolver = Resolver::new(&workspace);
	let options = params.options();
	let mut seen = HashSet::new();
	let mut views = Vec::new();
	let mut errors = Vec::new();

	// one path at a time, so that one bad path does not hide the others
	for text in &params.paths {
		let viewed = ItemPath::parse(text)
			.map_err(Error::from)
			.and_then(|path| View::with_options(options.clone()).item_path(path).run_with(&resolver));

		match viewed {
			Ok(viewed) => {
				views.extend(
					viewed
						.into_iter()
						.filter(|view| seen.insert((shown_item(&workspace, view.item), view.path.clone()))),
				);
			}

			Err(error) => errors.push(describe_in(&error, &resolver)),
		}
	}

	if views.is_empty() {
		return Err(errors.join("\n"));
	}

	let mut text = String::new();

	for error in &errors {
		for line in error.lines() {
			writeln!(text, "// error: {line}").unwrap();
		}
	}

	text.push_str(&render::views(workspace.root(), &views));
	text.push_str(&render::load_errors_note(&workspace));
	Ok(text)
}

/// `workspace_info`
pub(crate) fn workspace_info(load: &LoadOptions) -> Output {
	let workspace = self::load(load)?;

	Ok(render::workspace_info(&workspace, load))
}

/// Writes edits, unless an edited file changed on disk since the workspace was loaded (the edits would undo those
/// changes), they would write outside of the call's scope, or the request was cancelled.
fn write(root: &Path, edits: &EditSet, permit: &Permit<'_>) -> Result<Applied, String> {
	let changes = edits.preview().map_err(|error| describe(&error))?;

	ensure_unchanged(root, &changes)?;
	permit.scope.check(root, edits)?;

	if (permit.cancelled)() {
		return Err("the request was cancelled; nothing was written".to_owned());
	}

	edits.apply().map_err(|error| describe(&error))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::path::PathParseError;

	/// Lets an edit write anywhere.
	const PERMIT: Permit<'static> = Permit {
		cancelled: &|| false,
		scope: &WriteScope::Anywhere,
	};

	#[test]
	fn empty_path_lists_are_refused_before_loading() {
		let load = LoadOptions::default();
		let view: ViewParams = serde_json::from_value(serde_json::json!({ "paths": [] })).unwrap();
		let remove: RemoveParams = serde_json::from_value(serde_json::json!({ "paths": [] })).unwrap();

		assert!(self::view(&load, &view).unwrap_err().starts_with("`paths` is empty"));
		assert!(self::remove(&load, &remove, &PERMIT).unwrap_err().starts_with("`paths` is empty"));
	}

	#[test]
	fn errors_come_with_hints() {
		let not_found = describe(&Error::NotFound("crate::nope".to_owned()));

		assert!(
			not_found.starts_with("no item found for `crate::nope`\nhint: search with `find_items`"),
			"{not_found}"
		);

		let parse = describe(&Error::PathParse(PathParseError {
			text: "a::".to_owned(),
			message: "expected an identifier".to_owned(),
		}));

		assert!(
			parse.starts_with("invalid path `a::`: expected an identifier\nhint: paths look like"),
			"{parse}"
		);

		// errors that explain themselves are left alone
		assert_eq!(describe(&Error::InvalidSource("expected an item".to_owned())), "expected an item");
	}

	#[test]
	fn invalid_options_are_refused_before_loading() {
		let load = LoadOptions::default();
		let find: FindParams = serde_json::from_value(serde_json::json!({ "pattern": "x", "kinds": ["fn", "nope"] })).unwrap();
		let insert: InsertParams =
			serde_json::from_value(serde_json::json!({ "parent": "crate", "source": "fn f() {}", "position": "after" })).unwrap();
		let format: FormatParams = serde_json::from_value(serde_json::json!({ "formatter": "none", "sort": false })).unwrap();

		assert!(self::find(&load, &find).unwrap_err().starts_with("unknown item kind `nope`"));
		assert!(self::insert(&load, &insert, &PERMIT).unwrap_err().starts_with("`anchor`"));
		assert!(self::format(&load, &format, &PERMIT).unwrap_err().starts_with("nothing to do"));
	}

	#[test]
	fn unchanged_files_pass_and_changed_files_fail() {
		let dir = std::env::temp_dir().join(format!("rscode-mcp-unchanged-{}", std::process::id()));

		std::fs::create_dir_all(&dir).unwrap();

		let file = dir.join("lib.rs");

		std::fs::write(&file, "fn a() {}\n").unwrap();

		let change = |original: &str| FileChange {
			path: file.clone(),
			original: original.to_owned(),
			formatted: "fn b() {}\n".to_owned(),
		};

		assert_eq!(ensure_unchanged(&dir, &[change("fn a() {}\n")]), Ok(()));
		assert_eq!(
			ensure_unchanged(&dir, &[change("fn z() {}\n")]),
			Err("lib.rs changed on disk while the edit was being planned; nothing was written (try again)".to_owned())
		);

		std::fs::remove_dir_all(&dir).unwrap();

		// a deleted file changed too
		assert!(ensure_unchanged(&dir, &[change("fn a() {}\n")]).is_err());
	}

	#[test]
	fn usable_from_crate_roots() {
		let item = ItemId::new(crate::CrateId(3), 17);

		assert_eq!(
			UsableFrom::CrateRoot.viewpoint(item),
			Viewpoint::Module(ItemId::crate_root(crate::CrateId(3)))
		);
		assert_eq!(UsableFrom::Viewpoint(Viewpoint::Foreign).viewpoint(item), Viewpoint::Foreign);
	}
}
