//! Searching the workspace members a call did not select when a path names nothing in the selected ones, and
//! suggesting what a path that names nothing may have meant.
//!
//! A call that names no packages works on the server's selection (cargo's default members, usually). When one of its
//! paths names nothing there, the tool searches the other members (the one whose crate the path starts with, or else
//! all of them), runs again with the members it was found in selected too (so that the call's other paths, like
//! `crate::m`, name what they named before, unless those members have them too), and starts its text with a note
//! saying where the path was found. Read tools also take the only item whose path ends like a plain path that names
//! nothing (`Type::method`). What still names nothing gets the items named like it as suggestions.

use super::params::Selection;
use super::tools;
use super::tools::Output;
use crate::Error;
use crate::ItemId;
use crate::ItemPath;
use crate::Resolver;
use crate::Workspace;
use crate::edit;
use crate::model::CrateId;
use crate::query;
use crate::workspace::LoadOptions;
use smol_str::SmolStr;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::fmt::Write as _;

/// Why an attempt of an operation failed.
#[derive(Debug)]
pub(crate) enum Failure {
	/// A path names nothing ([`Error::NotFound`]): searching more workspace members may find it.
	NotFound(Error),

	/// Any other failure, as the message for the client.
	Message(String),
}

impl From<String> for Failure {
	fn from(message: String) -> Self {
		Self::Message(message)
	}
}

/// What an operation may do with what a search of more workspace members finds.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum Search {
	/// Use whatever the path names (for reading).
	Everything,

	/// Use items of one crate only: a path naming items of several crates is ambiguous (for editing).
	OneCrate,

	/// Search nothing beyond the selection: more selected crates could make the other paths of the call name more
	/// items (for edits of several paths).
	Selected,
}

/// For reading: when `path` names nothing, the only item whose path ends like it (see [`query::suggest`]), as a path
/// to use instead, with a note line saying so.
pub(crate) fn by_suffix(resolver: &Resolver<'_>, path: &ItemPath) -> Option<(ItemPath, String)> {
	let found = query::suggest(resolver, path).unique_suffix?;
	let note = format!("no item found for `{path}`; using `{found}`, the only item whose path ends like it");

	Some((ItemPath::parse(&found).ok()?, note))
}

/// A failure of an operation: a path that names nothing as such (see [`Failure::NotFound`]), anything else described
/// with hints.
pub(crate) fn fail(error: Error, resolver: &Resolver<'_>) -> Failure {
	match error {
		Error::NotFound(_) => Failure::NotFound(error),
		error => Failure::Message(tools::describe_in(&error, resolver)),
	}
}

/// The items that `path` names in `resolver`'s workspace, or for reading (see [`Search::Everything`]), when it names
/// nothing, the only item whose path ends like it (see [`by_suffix`]).
fn found_items(resolver: &Resolver<'_>, path: &ItemPath, search: Search) -> Vec<ItemId> {
	let found = resolver.resolve_item_path(path);

	match (found.is_empty(), search) {
		(true, Search::Everything) => (by_suffix(resolver, path))
			.map(|(path, _)| resolver.resolve_item_path(&path))
			.unwrap_or_default(),

		_ => found,
	}
}

/// Whether `items` are in several crates, counting the copies of an item once that several crates load from one file
/// (a library and a binary of a package, usually), since edits change them together.
fn in_several_crates(workspace: &Workspace, items: &[ItemId]) -> bool {
	let mut copies = HashSet::new();
	let crates: BTreeSet<CrateId> = (items.iter())
		.filter(|&&item| copies.insert((workspace.file_of(item).path(), workspace.item(item).range)))
		.map(|item| item.krate())
		.collect();

	crates.len() > 1
}

/// The workspace members among `members` that `items` are in.
pub(crate) fn members_of(
	workspace: &Workspace,
	items: impl IntoIterator<Item = ItemId>,
	members: &[SmolStr],
) -> Vec<SmolStr> {
	let packages: BTreeSet<&SmolStr> = (items.into_iter())
		.filter_map(|item| workspace.krate(item.krate()).package())
		.map(|package| &workspace.package(package).name)
		.collect();

	members.iter().filter(|member| packages.contains(member)).cloned().collect()
}

/// The message for a path that names nothing (any other error is described as usual), with hints: the workspace
/// member to select when the path starts with the name of a crate of one that is not loaded; else what the path may
/// have meant (see [`query::suggest`]) or how to search for it, and the workspace members that are not loaded.
pub(crate) fn not_found(error: &Error, resolver: &Resolver<'_>) -> String {
	let Error::NotFound(text) = error else {
		return tools::describe(error);
	};

	let workspace = resolver.workspace();
	let path = ItemPath::parse(text).ok();
	let member = path.as_ref().and_then(|path| workspace.unloaded_member_of(path));

	// a selector that picks none of the blocks a loaded header names: the members do not matter
	if let Some(hint) = path.as_ref().and_then(|path| resolver.selector_hint(path)) {
		return format!("{error}\nhint: {hint}");
	}

	// the crate is known: searching the loaded crates would not help
	if let Some(hint) = tools::unloaded_hint(workspace, member).filter(|_| member.is_some()) {
		return format!("{error}\nhint: {hint}");
	}

	let hint = match path.as_ref().and_then(|path| resolver.import_hint(path)) {
		Some(hint) => hint,
		None => suggestion(resolver, path.as_ref()),
	};
	let mut message = format!("{error}\nhint: {hint}");

	if let Some(hint) = tools::unloaded_hint(workspace, None) {
		write!(message, "\nhint: {hint}").unwrap();
	}

	message
}

/// The path that a [`Error::NotFound`] is about.
fn not_found_path(error: &Error) -> Option<ItemPath> {
	match error {
		Error::NotFound(text) => ItemPath::parse(text).ok(),
		_ => None,
	}
}

/// The note on the workspace members that a call did not select, which a path was found in (`found`), or which were
/// searched too.
pub(crate) fn note(members: &[SmolStr], found: bool) -> String {
	let names: Vec<String> = members.iter().map(|member| format!("`{member}`")).collect();
	let members = match names.as_slice() {
		[name] => format!("member {name}"),
		_ => format!("members {}", names.join(", ")),
	};
	let what = if found { "found in" } else { "also searched" };

	format!("{what} unselected workspace {members}")
}

/// The result of a run of an operation after a search of more workspace members: its text after the `note` on the
/// search, or its error (after the note too, unless a path still names nothing).
fn noted(note: &str, result: Result<String, Failure>, resolver: &Resolver<'_>) -> Output {
	match result {
		Ok(text) => Ok(format!("{note}{text}")),
		Err(Failure::Message(message)) => Err(format!("{note}{message}")),
		Err(Failure::NotFound(error)) => Err(not_found(&error, resolver)),
	}
}

/// Runs `op` on the workspace loaded with `load`. When it fails because a path names nothing, and the call named no
/// packages (`selection`), searches the workspace members it did not select (see [`LoadOptions::widened`]), as
/// `search` allows, and runs `op` again with the members the path was found in selected too (all those searched when
/// it was not found), starting its text with a note line on where the path was found. A path that still names nothing
/// gets suggestions (see [`not_found`]).
pub(crate) fn run(
	load: &LoadOptions,
	selection: &Selection,
	search: Search,
	mut op: impl FnMut(&LoadOptions, &Workspace, &Resolver<'_>) -> Result<String, Failure>,
) -> Output {
	let workspace = tools::load(load)?;
	let resolver = Resolver::new(&workspace);

	let error = match op(load, &workspace, &resolver) {
		Ok(text) => return Ok(text),
		Err(Failure::Message(message)) => return Err(message),
		Err(Failure::NotFound(error)) => error,
	};

	let path = not_found_path(&error);
	// (a path that starts with the crate of a member asks for that member: finding it there is no news)
	let asked = path.as_ref().is_some_and(|path| workspace.unloaded_member_of(path).is_some());
	let widening = match (search, selection.names_packages()) {
		(Search::Selected, _) | (_, true) => None,
		_ => load.widened(&workspace, path.as_ref()),
	};

	let Some(widening) = widening else {
		return Err(not_found(&error, &resolver));
	};

	let wider = tools::load(&widening.options)?;
	let searched = widening.searched(&wider);

	// (`lib` or `bin` left out the crates of every member to search)
	if searched.is_empty() {
		return Err(not_found(&error, &resolver));
	}

	let wide = Resolver::new(&wider);
	let found = path.as_ref().map(|path| found_items(&wide, path, search)).unwrap_or_default();

	if search == Search::OneCrate && found.is_empty() {
		return Err(not_found(&error, &wide));
	}

	if search == Search::OneCrate && in_several_crates(&wider, &found) {
		return Err(several_crates(&wide, path.as_ref(), &found));
	}

	let members = members_of(&wider, found, &searched);
	let note = match members.is_empty() {
		true => format!("note: {}\n", self::note(&searched, false)),
		false if asked => String::new(),
		false => format!("note: {}\n", self::note(&members, true)),
	};

	// with only the members the path was found in added to the selection, the call's other paths (`crate::...`) name
	// what they named in it, unless those members have them too
	if members.is_empty() || members.len() == searched.len() {
		return noted(&note, op(&widening.options, &wider, &wide), &wide);
	}

	let options = load.with_members(&workspace, &members);
	let narrower = tools::load(&options)?;
	let resolver = Resolver::new(&narrower);

	noted(&note, op(&options, &narrower, &resolver), &resolver)
}

/// The error for an edit whose path names nothing in the selected crates, but items of several crates of the
/// members it did not select.
fn several_crates(resolver: &Resolver<'_>, path: Option<&ItemPath>, items: &[ItemId]) -> String {
	let path = path.map(ToString::to_string).unwrap_or_default();
	let mut candidates: Vec<String> = items.iter().map(|&item| edit::describe(resolver, item)).collect();
	let mut seen = HashSet::new();

	// (copies of an item that several crates load from one file)
	candidates.retain(|candidate| seen.insert(candidate.clone()));

	let error = Error::Ambiguous {
		path: path.clone(),
		candidates,
	};

	format!(
		"{error}\nhint: `{path}` names nothing in the selected packages, but items of several crates of the other \
		 workspace members: select one with `packages` (and `lib` or `bin`)"
	)
}

/// What a path that names nothing may have meant: the only item whose path ends like it, or the items named like it,
/// or else how to search for it.
fn suggestion(resolver: &Resolver<'_>, path: Option<&ItemPath>) -> String {
	let suggestions = path.map(|path| query::suggest(resolver, path)).unwrap_or_default();

	if let Some(path) = &suggestions.unique_suffix {
		return format!("did you mean `{path}`?");
	}

	let mut hint = match suggestions.similar.as_slice() {
		[] => return tools::SEARCH_HINT.to_owned(),
		[path] => format!("did you mean `{path}`?"),

		[paths @ .., last] => {
			let paths: Vec<String> = paths.iter().map(|path| format!("`{path}`")).collect();

			format!("did you mean {}, or `{last}`?", paths.join(", "))
		}
	};

	if suggestions.named > suggestions.similar.len() {
		write!(hint, " ({} items have that name: see `find_items`)", suggestions.named).unwrap();
	}

	hint
}
