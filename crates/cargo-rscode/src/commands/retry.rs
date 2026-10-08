//! Searching the workspace members that the command line did not select when a path names nothing in the selected
//! ones, and suggesting what such a path may have meant.
//!
//! Without `-p` or `--workspace`, cargo's default members are loaded (in this repository, only `cargo-rscode`). When a
//! path names nothing in them, the command searches the other members (the one whose crate the path starts with, or
//! else all of them) and runs again with them selected, noting where the path was found. Reading commands also take
//! the only item whose path ends like a plain path that names nothing (`Type::method`). What still names nothing gets
//! the items named like it as suggestions.

use crate::render::PathDisplay;
use crate::ui::Ui;
use rscode::ItemId;
use rscode::ItemPath;
use rscode::LoadOptions;
use rscode::Resolver;
use rscode::Workspace;
use std::collections::BTreeSet;

/// Why an attempt of a command's operation failed.
#[derive(Debug)]
pub(super) enum Failure {
	/// A path names nothing ([`rscode::Error::NotFound`]): searching more workspace members may find it.
	NotFound(rscode::Error),

	/// Any other failure.
	Other(anyhow::Error),
}

impl From<anyhow::Error> for Failure {
	fn from(error: anyhow::Error) -> Self {
		Self::Other(error)
	}
}

/// What an operation may do with what a search of more workspace members finds.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum Search {
	/// Use whatever the path names (for reading).
	Everything,

	/// Use items of one crate only: a path naming items of several crates is ambiguous (for editing).
	OneCrate,

	/// Search nothing beyond the selection: more selected crates could make the other paths name more items (for
	/// edits of several paths).
	Selected,
}

/// For reading: when `path` names nothing, the only item whose path ends like it (see [`rscode::query::suggest`]),
/// as a path to use instead, with a note saying so.
pub(super) fn by_suffix(resolver: &Resolver<'_>, path: &ItemPath) -> Option<(ItemPath, String)> {
	let found = rscode::query::suggest(resolver, path).unique_suffix?;
	let note = format!("no item found for `{path}`; using `{found}`, the only item whose path ends like it");

	Some((ItemPath::parse(&found).ok()?, note))
}

/// An item for messages, like rscode's candidates of ambiguous paths: its path, kind, and location.
fn describe(resolver: &Resolver<'_>, paths: &PathDisplay, item: ItemId) -> String {
	let workspace = resolver.workspace();
	let file = workspace.file_of(item);
	let start = file.line_col(workspace.item(item).range.start);

	format!(
		"`{}` ({}) at {}:{start}",
		resolver.canonical_path(item).distinct(),
		workspace.item(item).kind,
		paths.display(file.path())
	)
}

/// A failure of an operation: a path that names nothing as such (see [`Failure::NotFound`]), anything else with the
/// hints of [`super::hint`].
pub(super) fn fail(error: rscode::Error, resolver: &Resolver<'_>) -> Failure {
	match error {
		rscode::Error::NotFound(_) => Failure::NotFound(error),
		error => Failure::Other(super::hinted(error, resolver)),
	}
}

/// The workspace members among `members` that `items` are in.
pub(super) fn members_of<'a>(workspace: &Workspace, items: impl IntoIterator<Item = ItemId>, members: &'a [impl AsRef<str>]) -> Vec<&'a str> {
	let packages: BTreeSet<&str> = (items.into_iter())
		.filter_map(|item| workspace.krate(item.krate()).package())
		.map(|package| workspace.package(package).name.as_str())
		.collect();

	members.iter().map(AsRef::as_ref).filter(|member| packages.contains(member)).collect()
}

/// The note on the workspace members that the command line did not select, which a path was found in (`found`), or
/// which were searched too.
pub(super) fn note(members: &[impl AsRef<str>], found: bool) -> String {
	let names: Vec<String> = members.iter().map(|member| format!("`{}`", member.as_ref())).collect();
	let (members, are) = match names.as_slice() {
		[name] => (format!("member {name}"), "is"),
		_ => (format!("members {}", names.join(", ")), "are"),
	};
	let what = if found { "found in" } else { "also searched" };

	format!("{what} workspace {members}, which {are} not selected by default (pass `-p` or `--workspace` to skip this search)")
}

/// Runs `op` on the workspace loaded with `options`. When it fails because a path names nothing, and the command line
/// named no packages, searches the workspace members it did not select (see [`LoadOptions::widened`]), as `search`
/// allows, and runs `op` again with them selected, noting where the path was found. A path that still names nothing
/// gets suggestions (see [`super::hint`]).
pub(super) fn run<T>(
	ui: &Ui,
	options: &LoadOptions,
	absolute_paths: bool,
	search: Search,
	mut op: impl FnMut(&LoadOptions, &Workspace, &PathDisplay, &Resolver<'_>) -> Result<T, Failure>,
) -> anyhow::Result<T> {
	let (workspace, paths) = super::load(ui, options, absolute_paths)?;
	let resolver = Resolver::new(&workspace);

	let error = match op(options, &workspace, &paths, &resolver) {
		Ok(done) => return Ok(done),
		Err(Failure::Other(error)) => return Err(error),
		Err(Failure::NotFound(error)) => error,
	};

	let path = match &error {
		rscode::Error::NotFound(text) => ItemPath::parse(text).ok(),
		_ => None,
	};
	let named_packages = options.workspace || !options.packages.is_empty();
	let widening = match (search, named_packages) {
		(Search::Selected, _) | (_, true) => None,
		_ => options.widened(&workspace, path.as_ref()),
	};

	let Some(widening) = widening else {
		return Err(super::hinted(error, &resolver));
	};

	let (wider, paths) = super::load(ui, &widening.options, absolute_paths)?;
	let resolver = Resolver::new(&wider);
	let found = path.as_ref().map(|path| resolver.resolve_item_path(path)).unwrap_or_default();
	let crates: BTreeSet<_> = found.iter().map(|item| item.krate()).collect();

	match search {
		Search::OneCrate if found.is_empty() => return Err(super::hinted(error, &resolver)),

		Search::OneCrate if crates.len() > 1 => {
			let candidates = found.iter().map(|&item| describe(&resolver, &paths, item)).collect();
			let error = rscode::Error::Ambiguous {
				path: path.map(|path| path.to_string()).unwrap_or_default(),
				candidates,
			};
			let hint = "the path names nothing in the selected packages, but items of several crates of the other \
				workspace members: select one with `-p NAME` (and `--lib` or `--bin NAME`)";

			return Err(anyhow::anyhow!("{error}\nhint: {hint}"));
		}

		_ => {}
	}

	let members = members_of(&wider, found, &widening.members);

	match members.is_empty() {
		true => ui.note(note(&widening.members, false)),
		false => ui.note(note(&members, true)),
	}

	match op(&widening.options, &wider, &paths, &resolver) {
		Ok(done) => Ok(done),
		Err(Failure::Other(error)) => Err(error),
		Err(Failure::NotFound(error)) => Err(super::hinted(error, &resolver)),
	}
}

/// What a path that names nothing may have meant: the only item whose path ends like it, or the items named like it.
pub(super) fn suggestion(resolver: &Resolver<'_>, path: Option<&ItemPath>) -> Option<String> {
	let suggestions = rscode::query::suggest(resolver, path?);

	if let Some(path) = &suggestions.unique_suffix {
		return Some(format!("did you mean `{path}`?"));
	}

	let mut hint = match suggestions.similar.as_slice() {
		[] => return None,
		[path] => format!("did you mean `{path}`?"),

		[paths @ .., last] => {
			let paths: Vec<String> = paths.iter().map(|path| format!("`{path}`")).collect();

			format!("did you mean {}, or `{last}`?", paths.join(", "))
		}
	};

	if suggestions.named > suggestions.similar.len() {
		hint.push_str(&format!(" ({} items have that name: see `find`)", suggestions.named));
	}

	Some(hint)
}
