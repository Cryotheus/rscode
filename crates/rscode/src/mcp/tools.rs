//! The work of every tool, run on a fresh thread per call (see [`super::worker`]).
//!
//! Each call loads the workspace from disk, so changes made by anything else are always seen. Failures become
//! messages for the client (tool errors), with hints for the common mistakes.

use super::params::AddImportParams;
use super::params::CreateModuleParams;
use super::params::EditItemParams;
use super::params::FindParams;
use super::params::FormatParams;
use super::params::InsertParams;
use super::params::ReferencesParams;
use super::params::RemoveParams;
use super::params::RenameParams;
use super::params::ReplaceParams;
use super::params::ViewParams;
use super::render;
use super::render::Page;
use super::retry;
use super::retry::Failure;
use super::retry::Search;
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
use crate::edit::ImportOptions;
use crate::model::UnloadedMember;
use crate::query;
use crate::query::ItemView;
use crate::query::ViewOptions;
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

/// What tells views of `view_items` apart: whether the view is from the wider workspace of a retry, the item whose
/// text it shows (see [`shown_item`]), and its path.
type ViewKey = (bool, ItemId, String);

/// How to search for the path of an item.
pub(super) const SEARCH_HINT: &str = "search with `find_items` (e.g. the pattern `*name*` with `ignore_case`, and \
	`include_imports` for imports) for the exact path";

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

/// `add_import`
pub(crate) fn add_import(load: &LoadOptions, params: &AddImportParams, permit: &Permit<'_>) -> Output {
	let imports: Vec<String> = (params.paths.iter())
		.map(|import| import.trim())
		.filter(|import| !import.is_empty())
		.map(str::to_owned)
		.collect();

	if imports.is_empty() {
		return Err("`paths` is empty: give the imports, such as `std::fs` or `crate::a::{B, C}`".to_owned());
	}

	let module = parse_path(&params.module)?;

	retry::run(load, &params.selection, Search::OneCrate, |_, workspace, resolver| {
		let plan = edit::add_imports(resolver, &module, &imports, &ImportOptions::default()).map_err(|error| {
			let hint = match &error {
				Error::Ambiguous { .. } if in_several_crates(resolver, &module) => SELECT_ONE_CRATE,
				Error::Collision { .. } => "import it under another name (`x::Y as Z`), or remove what has the name",
				Error::InvalidSource(_) => "each of `paths` is a `use` tree: `std::fs`, `crate::a::{B, C}`, or `x::Y as Z`",
				_ => return retry::fail(error, resolver),
			};

			Failure::Message(format!("{error}\nhint: {hint}"))
		})?;
		let root = workspace.root();
		let mut text = render::import_addition(root, &plan, params.dry_run);

		finish(root, &plan.edits, params.dry_run, permit, &mut text)?;
		Ok(text)
	})
}

/// `create_module`
pub(crate) fn create_module(load: &LoadOptions, params: &CreateModuleParams, permit: &Permit<'_>) -> Output {
	let name = params.name.trim();

	if name.is_empty() {
		return Err("`name` is empty: give the name of the new module (an identifier)".to_owned());
	}

	let parent = parse_path(&params.parent)?;

	retry::run(load, &params.selection, Search::OneCrate, |_, workspace, resolver| {
		let plan = edit::create_module(resolver, &parent, name, &params.source, &params.options()).map_err(|error| {
			let hint = match &error {
				Error::Ambiguous { .. } if in_several_crates(resolver, &parent) => SELECT_ONE_CRATE.to_owned(),
				Error::InvalidIdent(_) => "`name` is the new module's identifier alone (e.g. `render`), not a path".to_owned(),
				Error::Collision { .. } => "choose another `name`".to_owned(),

				Error::Io { source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists => format!(
					"declare the existing file with `insert_items` (`source` `mod {name};`), or choose another `name`"
				),

				Error::InvalidSource(message) if message.starts_with("the source") => {
					"`source` is the whole file of the module: items, after `//!` docs and inner attributes if any".to_owned()
				}

				Error::InvalidSource(_) => {
					"`vis` is `pub`, `pub(crate)`, `pub(super)`, or `pub(in path)`; leave it out for a private module".to_owned()
				}

				_ => return retry::fail(error, resolver),
			};

			Failure::Message(format!("{error}\nhint: {hint}"))
		})?;
		let root = workspace.root();
		let mut text = render::module_creation(root, &plan, &params.parent, params.dry_run);

		finish(root, &plan.edits, params.dry_run, permit, &mut text)?;
		Ok(text)
	})
}

/// A message for the client, with a hint on how to go on after common mistakes.
pub(super) fn describe(error: &Error) -> String {
	let hint = match error {
		Error::NotFound(_) => SEARCH_HINT,

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

/// [`describe`], for an error of an operation: a path that names nothing gets the hints of [`retry::not_found`] (what
/// it may have meant, workspace members that are not loaded, imports of items that are not loaded).
pub(super) fn describe_in(error: &Error, resolver: &Resolver<'_>) -> String {
	match error {
		Error::NotFound(_) => retry::not_found(error, resolver),
		_ => describe(error),
	}
}

/// `edit_item`
pub(crate) fn edit_item(load: &LoadOptions, params: &EditItemParams, permit: &Permit<'_>) -> Output {
	let edit = params.edit()?;
	let path = parse_path(&params.path)?;

	retry::run(load, &params.selection, Search::OneCrate, |load, workspace, resolver| {
		let plan = edit::edit_item(resolver, &path, &edit, &params.options()).map_err(|error| {
			let all_variants = matches!(error, Error::Ambiguous { .. }) && edit::replaces_all_variants(resolver, &path);
			let hint = match error {
				Error::Ambiguous { .. } if all_variants && in_several_crates(resolver, &path) => {
					format!("{SELECT_ONE_CRATE}, or set `all_variants` to edit every one of them")
				}

				Error::Ambiguous { .. } if all_variants => {
					"give `old` text that only one of them has, or set `all_variants` to edit every one of them".to_owned()
				}

				Error::TextMismatch { ref lines, .. } if lines.is_empty() => {
					"copy `old` exactly from the output of `view_items` (with or without its line numbers)".to_owned()
				}

				Error::TextMismatch { .. } => {
					"include more of the surrounding text in `old` so that it occurs once, or copy it with the line numbers \
					 of `view_items` (`line_numbers`) to pick one"
						.to_owned()
				}

				error => return retry::fail(error, resolver),
			};

			Failure::Message(format!("{error}\nhint: {hint}"))
		})?;
		let root = workspace.root();
		let mut text = render::item_edit(root, &plan, params.dry_run);

		if params.format && params.dry_run {
			text.push_str(UNFORMATTED_DIFF);
		}

		finish(root, &plan.edits, params.dry_run, permit, &mut text)?;

		if params.format && !params.dry_run && !plan.spans.is_empty() {
			text.push_str(&format_written(load, permit, &[(params.path.clone(), path.clone())]));
		}

		Ok(text)
	})
}

/// Fails when a file no longer has the contents the edits were planned on, or a file that `edits` creates exists.
fn ensure_unchanged(root: &Path, changes: &[FileChange], edits: &EditSet) -> Result<(), String> {
	let created: HashSet<&Path> = edits.created().collect();

	for change in changes {
		let unchanged = match created.contains(change.path.as_path()) {
			true => std::fs::symlink_metadata(&change.path).is_err(),
			false => std::fs::read_to_string(&change.path).ok().as_deref() == Some(change.original.as_str()),
		};

		if !unchanged {
			return Err(format!(
				"{} changed on disk while the edit was being planned; nothing was written (try again)",
				render::relative(root, &change.path).display()
			));
		}
	}

	Ok(())
}

/// `find_items`: when nothing matches and the call names no packages, the workspace members it did not select are
/// searched too (see [`retry`]).
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
	let found = search(&resolver, &find, params)?;
	let path = pattern_crate(&params.pattern).and_then(|krate| ItemPath::parse(krate).ok());
	let widening = (found.0.is_empty() && !params.selection.names_packages())
		.then(|| load.widened(&workspace, path.as_ref()))
		.flatten();

	let Some(widening) = widening else {
		return Ok(found_text(&workspace, &resolver, params, found));
	};

	let wider = self::load(&widening.options)?;
	let resolver = Resolver::new(&wider);
	let found = search(&resolver, &find, params)?;
	let members = retry::members_of(&wider, found.0.iter().map(|found| found.item), &widening.members);
	// (a pattern that starts with the crate of a member asks for that member: finding it there is no news)
	let asked = path.as_ref().is_some_and(|path| workspace.unloaded_member_of(path).is_some());
	let note = match members.is_empty() {
		true => format!("note: {}\n", retry::note(&widening.members, false)),
		false if asked => String::new(),
		false => format!("note: {}\n", retry::note(&members, true)),
	};

	Ok(note + &found_text(&wider, &resolver, params, found))
}

/// Appends the diff for a dry run, or writes the edits.
fn finish(root: &Path, edits: &EditSet, dry_run: bool, permit: &Permit<'_>, text: &mut String) -> Result<(), String> {
	match dry_run {
		true => {
			let changes = edits.preview().map_err(|error| describe(&error))?;
			let diff = render::diff(root, &changes, edits);

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
		let diff = render::diff(root, &formatting.changes, &formatting.edits);

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

/// The text of `find_items`: the matches of `params`' page, and hints.
fn found_text(
	workspace: &Workspace,
	resolver: &Resolver<'_>,
	params: &FindParams,
	found: (Vec<FindMatch>, Option<UsableFrom>),
) -> String {
	let (found, from) = found;
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

	let unloaded: Vec<&str> = workspace.unloaded_members().iter().map(|member| member.name.as_str()).collect();

	match total {
		0 => {
			if let Some(hint) = unloaded_hint(workspace, member) {
				writeln!(text, "hint: {hint}").unwrap();
			}
		}

		// (only when the call left the selection to the server)
		_ if !unloaded.is_empty() && !params.selection.names_packages() => {
			writeln!(text, "note: not searched: {} (members not selected; see `packages`)", unloaded.join(", ")).unwrap();
		}

		_ => {}
	}

	text.push_str(&render::load_errors_note(workspace));
	text
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

	retry::run(load, &params.selection, Search::OneCrate, |load, workspace, resolver| {
		let plan = edit::insert(resolver, parent.as_ref(), &params.source, &options).map_err(|error| match (&error, &parent) {
			(Error::Ambiguous { .. }, Some(parent)) if in_several_crates(resolver, parent) => {
				Failure::Message(format!("{error}\nhint: {SELECT_ONE_CRATE}"))
			}

			// several containers have the anchor
			(Error::Ambiguous { .. }, None) => Failure::Message(format!("{error}\nhint: pass one of the candidates as `parent`")),

			_ => retry::fail(error, resolver),
		})?;
		let root = workspace.root();
		let mut text = render::insertion(root, &plan, params.parent().unwrap_or(&plan.parent), params.dry_run);

		if params.format && params.dry_run {
			text.push_str(UNFORMATTED_DIFF);
		}

		finish(root, &plan.edits, params.dry_run, permit, &mut text)?;

		if params.format && !params.dry_run {
			// (the canonical path of the container found from the anchor is a path to it)
			let parent = match &parent {
				Some(parent) => parent.clone(),
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
				text.push_str(
					"note: unnamed items (like `impl` blocks) are not formatted; use `format_items` on the parent\n",
				);
			}

			text.push_str(&format_written(load, permit, &targets));
		}

		Ok(text)
	})
}

/// Loads the workspace, with a hint when there is no `Cargo.toml` to load it from.
pub(super) fn load(options: &LoadOptions) -> Result<Workspace, String> {
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

/// `find_references`
pub(crate) fn references(load: &LoadOptions, params: &ReferencesParams) -> Output {
	let path = parse_path(&params.path)?;

	retry::run(load, &params.selection, Search::Everything, |_, workspace, resolver| {
		// a plain path that names nothing stands for the only item whose path ends like it
		let (path, mut text) = match resolver.resolve_item_path(&path).is_empty() {
			false => (path.clone(), String::new()),

			true => match retry::by_suffix(resolver, &path) {
				Some((found, note)) => (found, format!("note: {note}\n")),
				None => return Err(Failure::NotFound(Error::NotFound(path.to_string()))),
			},
		};
		let report =
			query::find_references(resolver, &[path], &params.options()).map_err(|error| retry::fail(error, resolver))?;
		let page = Page {
			offset: params.offset,
			limit: params.limit,
			total: report.references.len(),
		};

		text.push_str(&render::references(workspace.root(), &params.path, &report, page, params.searches_everything()));
		text.push_str(&render::load_errors_note(workspace));
		Ok(text)
	})
}

/// `remove_items`
pub(crate) fn remove(load: &LoadOptions, params: &RemoveParams, permit: &Permit<'_>) -> Output {
	if params.paths.is_empty() {
		return Err("`paths` is empty: give the paths of the items to remove".to_owned());
	}

	let paths = params.paths.iter().map(|path| parse_path(path)).collect::<Result<Vec<_>, _>>()?;

	// (selecting more crates could make the other paths name more items, which would be removed too)
	let search = match paths.len() {
		1 => Search::OneCrate,
		_ => Search::Selected,
	};

	retry::run(load, &params.selection, search, |_, workspace, resolver| {
		let plan = edit::remove(resolver, &paths, &params.options()).map_err(|error| retry::fail(error, resolver))?;
		let root = workspace.root();
		let mut text = render::removal(root, &plan, params.dry_run);

		finish(root, &plan.edits, params.dry_run, permit, &mut text)?;
		Ok(text)
	})
}

/// `rename_item`
pub(crate) fn rename(load: &LoadOptions, params: &RenameParams, permit: &Permit<'_>) -> Output {
	let path = parse_path(&params.path)?;

	retry::run(load, &params.selection, Search::OneCrate, |_, workspace, resolver| {
		let plan = edit::rename(resolver, &path, params.new_name.trim(), &params.options()).map_err(|error| match error {
			Error::InvalidIdent(_) => Failure::Message(format!(
				"{error}\nhint: `new_name` is the new identifier alone (e.g. `parse_config`), not a path"
			)),

			error => retry::fail(error, resolver),
		})?;
		let root = workspace.root();
		let mut text = render::rename(root, &plan, params.new_name.trim(), params.dry_run);

		finish(root, &plan.edits, params.dry_run, permit, &mut text)?;
		Ok(text)
	})
}

/// `replace_item`
pub(crate) fn replace(load: &LoadOptions, params: &ReplaceParams, permit: &Permit<'_>) -> Output {
	let path = parse_path(&params.path)?;

	retry::run(load, &params.selection, Search::OneCrate, |load, workspace, resolver| {
		let plan = edit::replace(resolver, &path, &params.source, &params.options()).map_err(|error| {
			let all_variants = matches!(error, Error::Ambiguous { .. }) && edit::replaces_all_variants(resolver, &path);

			match error {
				Error::Ambiguous { .. } if all_variants && in_several_crates(resolver, &path) => Failure::Message(format!(
					"{error}\nhint: {SELECT_ONE_CRATE}, or set `all_variants` to replace every one of them"
				)),

				Error::Ambiguous { .. } if all_variants => {
					Failure::Message(format!("{error}\nhint: set `all_variants` to replace every one of them"))
				}

				// `Type::name` names a method, and the source is the field `Type.name`
				Error::InvalidSource(_) if let Some(hint) = resolver.field_hint(&path) => {
					Failure::Message(format!("{error}\nhint: {hint}"))
				}

				error => retry::fail(error, resolver),
			}
		})?;
		let root = workspace.root();
		let mut text = render::replacement(root, &plan, params.dry_run);

		if params.format && params.dry_run {
			text.push_str(UNFORMATTED_DIFF);
		}

		finish(root, &plan.edits, params.dry_run, permit, &mut text)?;

		if params.format && !params.dry_run {
			text.push_str(&format_written(load, permit, &[(params.path.clone(), path.clone())]));
		}

		Ok(text)
	})
}

/// The matches of `find_items`' search, and the viewpoint of its `from`, in `resolver`'s workspace.
fn search(
	resolver: &Resolver<'_>,
	find: &Find,
	params: &FindParams,
) -> Result<(Vec<FindMatch>, Option<UsableFrom>), String> {
	let from = params
		.from
		.as_deref()
		.map(str::trim)
		.filter(|from| !from.is_empty())
		.map(|from| usable_from(resolver, from))
		.transpose()?;
	let found = find.run_with(resolver).map_err(|error| describe(&error))?;

	Ok((found, from))
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
pub(super) fn unloaded_hint(workspace: &Workspace, member: Option<&UnloadedMember>) -> Option<String> {
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

/// `view_items`: a path that names nothing is searched again in the workspace members that the call did not select
/// (see [`retry`]), and a plain path that names nothing stands for the only item whose path ends like it.
pub(crate) fn view(load: &LoadOptions, params: &ViewParams) -> Output {
	if params.paths.is_empty() {
		return Err("`paths` is empty: give the paths of the items to show".to_owned());
	}

	let workspace = self::load(load)?;
	let resolver = Resolver::new(&workspace);
	let options = params.options();
	let mut notes = Vec::new();

	// one path at a time, so that one bad path does not hide the others; each view with what tells duplicates apart
	// (several paths, or imports shown as their `use` item), also when found in a wider workspace
	let mut viewed: Vec<Result<Vec<(ViewKey, ItemView)>, String>> = Vec::new();
	let mut missing = Vec::new();
	let keyed = |workspace: &Workspace, wider: bool, views: Vec<ItemView>| -> Vec<_> {
		(views.into_iter())
			.map(|view| ((wider, shown_item(workspace, view.item), view.path.clone()), view))
			.collect()
	};

	for (index, text) in params.paths.iter().enumerate() {
		match view_path(&resolver, &options, text) {
			Ok((views, note)) => {
				notes.extend(note);
				viewed.push(Ok(keyed(&workspace, false, views)));
			}

			Err(Failure::Message(message)) => viewed.push(Err(message)),

			Err(Failure::NotFound(error)) => {
				missing.push((index, error));
				viewed.push(Ok(Vec::new()));
			}
		}
	}

	// one member is searched when every path that names nothing starts with the name of one of its crates
	let member_of = |index: usize| {
		let path = ItemPath::parse(&params.paths[index]).ok()?;

		workspace.unloaded_member_of(&path).map(|member| member.name.clone())
	};
	let member_path = (missing.first())
		.map(|&(first, _)| first)
		.filter(|&first| {
			member_of(first).is_some() && missing.iter().all(|&(index, _)| member_of(index) == member_of(first))
		})
		.and_then(|first| ItemPath::parse(&params.paths[first]).ok());
	let widening = (!missing.is_empty() && !params.selection.names_packages())
		.then(|| load.widened(&workspace, member_path.as_ref()))
		.flatten();

	match widening {
		None => {
			for (index, error) in missing {
				viewed[index] = Err(describe_in(&error, &resolver));
			}
		}

		Some(widening) => {
			let wider = self::load(&widening.options)?;
			let resolver = Resolver::new(&wider);
			let mut found = Vec::new();

			for (index, _) in missing {
				viewed[index] = match view_path(&resolver, &options, &params.paths[index]) {
					Ok((views, note)) => {
						notes.extend(note);
						found.extend(views.iter().map(|view| view.item));
						Ok(keyed(&wider, true, views))
					}

					Err(Failure::Message(message)) => Err(message),
					Err(Failure::NotFound(error)) => Err(describe_in(&error, &resolver)),
				};
			}

			let members = retry::members_of(&wider, found, &widening.members);

			// (paths that start with the crate of a member ask for that member: finding them there is no news)
			if !members.is_empty() && member_path.is_none() {
				notes.insert(0, retry::note(&members, true));
			}
		}
	}

	let mut seen = HashSet::new();
	let mut views = Vec::new();
	let mut errors = Vec::new();

	for result in viewed {
		match result {
			Ok(viewed) => {
				views.extend(viewed.into_iter().filter(|(key, _)| seen.insert(key.clone())).map(|(_, view)| view));
			}

			Err(error) => errors.push(error),
		}
	}

	if views.is_empty() {
		return Err(errors.join("\n"));
	}

	let mut text = String::new();

	for (label, message) in (notes.iter().map(|note| ("note", note))).chain(errors.iter().map(|error| ("error", error))) {
		for line in message.lines() {
			writeln!(text, "// {label}: {line}").unwrap();
		}
	}

	text.push_str(&render::views(workspace.root(), &views));
	text.push_str(&render::load_errors_note(&workspace));
	Ok(text)
}

/// The views of the items that one path of `view_items` names; a plain path that names nothing stands for the only
/// item whose path ends like it (with a note saying so).
fn view_path(
	resolver: &Resolver<'_>,
	options: &ViewOptions,
	text: &str,
) -> Result<(Vec<ItemView>, Option<String>), Failure> {
	let path = parse_path(text)?;
	let view = |path: ItemPath| View::with_options(options.clone()).item_path(path).run_with(resolver);

	match view(path.clone()) {
		Ok(views) => Ok((views, None)),

		Err(error @ Error::NotFound(_)) => match retry::by_suffix(resolver, &path) {
			Some((found, note)) => {
				(view(found).map(|views| (views, Some(note)))).map_err(|error| retry::fail(error, resolver))
			}

			None => Err(Failure::NotFound(error)),
		},

		Err(error) => Err(retry::fail(error, resolver)),
	}
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

	ensure_unchanged(root, &changes, edits)?;
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

		let edit: EditItemParams = serde_json::from_value(serde_json::json!({ "path": "crate::f", "old": "a" })).unwrap();

		assert!(self::find(&load, &find).unwrap_err().starts_with("unknown item kind `nope`"));
		assert!(self::edit_item(&load, &edit, &PERMIT).unwrap_err().starts_with("`old` needs `new`"));
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

		let edits = EditSet::new();

		assert_eq!(ensure_unchanged(&dir, &[change("fn a() {}\n")], &edits), Ok(()));
		assert_eq!(
			ensure_unchanged(&dir, &[change("fn z() {}\n")], &edits),
			Err("lib.rs changed on disk while the edit was being planned; nothing was written (try again)".to_owned())
		);

		// a file to create must not exist
		let mut creating = EditSet::new();
		let created = FileChange {
			path: dir.join("new.rs"),
			original: String::new(),
			formatted: "fn n() {}\n".to_owned(),
		};

		creating.create_file(dir.join("new.rs"), "fn n() {}\n");
		assert_eq!(ensure_unchanged(&dir, std::slice::from_ref(&created), &creating), Ok(()));
		std::fs::write(dir.join("new.rs"), "").unwrap();
		assert!(ensure_unchanged(&dir, &[created], &creating).is_err());

		std::fs::remove_dir_all(&dir).unwrap();

		// a deleted file changed too
		assert!(ensure_unchanged(&dir, &[change("fn a() {}\n")], &edits).is_err());
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
