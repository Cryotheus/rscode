//! Searching the workspace members that the command line did not select when a path names nothing in the selected
//! ones, and suggesting what such a path may have meant.
//!
//! Without `-p` or `--workspace`, cargo's default members are loaded (in this repository, only `cargo-rscode`). When a
//! path names nothing in them, the command searches the other members (the one whose crate the path starts with, or
//! else all of them) and runs again with the members it was found in selected too (so that the other paths of the
//! command line, like `crate::m`, name what they named before, unless those members have them too), noting where the
//! path was found. Reading commands also take the only item whose path ends like a plain path that names nothing
//! (`Type::method`). What still names nothing gets the items named like it as suggestions.

use crate::render::PathDisplay;
use crate::ui::Ui;
use rscode::ItemId;
use rscode::ItemPath;
use rscode::LoadOptions;
use rscode::Resolver;
use rscode::Workspace;
use rscode::model::CrateId;
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

/// How a command loads workspaces: with its load options, and how it shows paths.
#[derive(Clone, Copy)]
pub(super) struct Load<'a> {
    /// Where notes go.
    pub(super) ui: &'a Ui,

    /// The options of the command line.
    pub(super) options: &'a LoadOptions,

    /// Whether paths are shown absolute (`--absolute-paths`).
    pub(super) absolute_paths: bool,
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

/// Runs `op` again after it failed with `error` on the workspace of `resolver` (loaded with `load`), because the
/// paths `missing` name nothing there: when the command line named no packages, searches the workspace members it did
/// not select for them (see [`LoadOptions::widened`]), as `search` allows, and runs `op` with the members they were
/// found in selected too (all those searched, when they were not found), noting where they were found. A path that
/// still names nothing gets suggestions (see [`super::hint`]).
///
/// Only the members the paths were found in are added to the selection, so that the other paths of the command line
/// (`crate::...`) name what they named in it, unless those members have them too.
pub(super) fn again<T>(
    load: Load<'_>,
    search: Search,
    resolver: &Resolver<'_>,
    error: rscode::Error,
    missing: &[ItemPath],
    mut op: impl FnMut(&LoadOptions, &Workspace, &PathDisplay, &Resolver<'_>) -> Result<T, Failure>,
) -> anyhow::Result<T> {
    let workspace = resolver.workspace();

    // one member is searched when every path starts with the name of one of its crates
    let member_of = |path: &ItemPath| {
        workspace
            .unloaded_member_of(path)
            .map(|member| &member.name)
    };
    let member_path = (missing.first()).filter(|first| {
        member_of(first).is_some()
            && missing
                .iter()
                .all(|path| member_of(path) == member_of(first))
    });
    let widening = match (search, named_packages(load.options)) {
        (Search::Selected, _) | (_, true) => None,
        _ => load.options.widened(workspace, member_path),
    };

    let Some(widening) = widening else {
        return Err(super::hinted(error, resolver));
    };

    let (wider, paths) = super::load(load.ui, &widening.options, load.absolute_paths)?;
    let searched = widening.searched(&wider);

    // (the target options left out the crates of every member to search)
    if searched.is_empty() {
        return Err(super::hinted(error, resolver));
    }

    let wide = Resolver::new(&wider);
    let found: Vec<ItemId> = missing
        .iter()
        .flat_map(|path| found_items(&wide, path, search))
        .collect();

    match search {
        Search::OneCrate if found.is_empty() => return Err(super::hinted(error, &wide)),

        Search::OneCrate if in_several_crates(&wider, &found) => {
            let mut candidates: Vec<String> = found
                .iter()
                .map(|&item| describe(&wide, &paths, item))
                .collect();
            let mut seen = BTreeSet::new();

            // (copies of an item that several crates load from one file)
            candidates.retain(|candidate| seen.insert(candidate.clone()));

            let error = rscode::Error::Ambiguous {
                path: missing
                    .first()
                    .map(|path| path.to_string())
                    .unwrap_or_default(),
                candidates,
            };
            let hint = "the path names nothing in the selected packages, but items of several crates of the other \
				workspace members: select one with `-p NAME` (and `--lib` or `--bin NAME`)";

            return Err(anyhow::anyhow!("{error}\nhint: {hint}"));
        }

        _ => {}
    }

    let members = members_of(&wider, found, &searched);

    match members.is_empty() {
        true => load.ui.note(note(&searched, false)),
        false => load.ui.note(note(&members, true)),
    }

    if members.is_empty() || members.len() == searched.len() {
        return op(&widening.options, &wider, &paths, &wide)
            .map_err(|failure| failed(failure, &wide));
    }

    let options = load.options.with_members(workspace, &members);
    let (narrower, paths) = super::load(load.ui, &options, load.absolute_paths)?;
    let resolver = Resolver::new(&narrower);

    op(&options, &narrower, &paths, &resolver).map_err(|failure| failed(failure, &resolver))
}

/// For reading: when `path` names nothing, the only item whose path ends like it (see [`rscode::query::suggest`]),
/// as a path to use instead, with a note saying so.
pub(super) fn by_suffix(resolver: &Resolver<'_>, path: &ItemPath) -> Option<(ItemPath, String)> {
    let found = rscode::query::suggest(resolver, path).unique_suffix?;
    let note = format!(
        "no item found for `{path}`; using `{found}`, the only item whose path ends like it"
    );

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

/// The error of an operation run again after a search of more workspace members.
fn failed(failure: Failure, resolver: &Resolver<'_>) -> anyhow::Error {
    match failure {
        Failure::Other(error) => error,
        Failure::NotFound(error) => super::hinted(error, resolver),
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
    let mut copies = BTreeSet::new();
    let crates: BTreeSet<CrateId> = (items.iter())
        .filter(|&&item| {
            copies.insert((workspace.file_of(item).path(), workspace.item(item).range))
        })
        .map(|item| item.krate())
        .collect();

    crates.len() > 1
}

/// The workspace members among `members` that `items` are in.
pub(super) fn members_of<'a>(
    workspace: &Workspace,
    items: impl IntoIterator<Item = ItemId>,
    members: &'a [impl AsRef<str>],
) -> Vec<&'a str> {
    let packages: BTreeSet<&str> = (items.into_iter())
        .filter_map(|item| workspace.krate(item.krate()).package())
        .map(|package| workspace.package(package).name.as_str())
        .collect();

    members
        .iter()
        .map(AsRef::as_ref)
        .filter(|member| packages.contains(member))
        .collect()
}

/// Whether the command line named the packages to load (`-p`, `--workspace`), which turns off searching the other
/// workspace members.
pub(super) fn named_packages(options: &LoadOptions) -> bool {
    options.workspace || !options.packages.is_empty()
}

/// The note on the workspace members that the command line did not select, which a path was found in (`found`), or
/// which were searched too.
pub(super) fn note(members: &[impl AsRef<str>], found: bool) -> String {
    let names: Vec<String> = members
        .iter()
        .map(|member| format!("`{}`", member.as_ref()))
        .collect();
    let (members, are) = match names.as_slice() {
        [name] => (format!("member {name}"), "is"),
        _ => (format!("members {}", names.join(", ")), "are"),
    };
    let what = if found { "found in" } else { "also searched" };

    format!(
        "{what} workspace {members}, which {are} not selected by default (pass `-p` or `--workspace` to skip this search)"
    )
}

/// Runs `op` on the workspace loaded with `options`. When it fails because a path names nothing, runs it again with
/// more workspace members selected, as `search` allows (see [`again`]).
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

    let missing: Vec<ItemPath> = match &error {
        rscode::Error::NotFound(text) => ItemPath::parse(text).into_iter().collect(),
        _ => Vec::new(),
    };
    let load = Load {
        ui,
        options,
        absolute_paths,
    };

    again(load, search, &resolver, error, &missing, op)
}

/// The paths among `targets` that name something in `resolver`'s workspace (a plain path that names nothing replaced
/// by the only item whose path ends like it, with a note in `notes`; see [`by_suffix`]), and the paths that name
/// nothing.
pub(super) fn split(
    resolver: &Resolver<'_>,
    targets: &[ItemPath],
    notes: &mut Vec<String>,
) -> (Vec<ItemPath>, Vec<ItemPath>) {
    let mut found = Vec::new();
    let mut missing = Vec::new();

    for path in targets {
        if !resolver.resolve_item_path(path).is_empty() {
            found.push(path.clone());
            continue;
        }

        match by_suffix(resolver, path) {
            Some((path, note)) => {
                notes.push(note);
                found.push(path);
            }

            None => missing.push(path.clone()),
        }
    }

    (found, missing)
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
        hint.push_str(&format!(
            " ({} items have that name: see `find`)",
            suggestions.named
        ));
    }

    Some(hint)
}
