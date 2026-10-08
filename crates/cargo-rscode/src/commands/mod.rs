//! The subcommands.

mod create_module;
mod edit;
mod find;
mod fmt;
mod format_edited;
mod import;
mod insert;
mod refs;
mod remove;
mod rename;
mod replace;
mod retry;
mod view;

#[cfg(feature = "mcp")]
mod mcp;

use crate::args::MessageFormat;
use crate::args::SourceArg;
use crate::render;
use crate::render::PathDisplay;
use crate::report::EditReport;
use crate::ui;
use crate::ui::Ui;
use anyhow::Context as _;
use clap::ArgMatches;
use rscode::ItemPath;
use rscode::LoadOptions;
use rscode::Resolver;
use rscode::Workspace;
use rscode::model::Severity;
use rscode::model::UnloadedMember;
use std::collections::BTreeSet;
use std::io::IsTerminal as _;
use std::io::Read as _;
use std::process::ExitCode;

/// How to pick one of several crates that a path names items of.
const SELECT_ONE_CRATE: &str = "the path names items of several crates (such as the library and a binary of a \
	package, whose roots are both `crate`): select one with `--lib` or `--bin NAME`";

/// How to replace one of several imports that a `use` path names.
const SEVERAL_IMPORTS: &str = "the `use` path names several imports of its module: remove them and insert the new \
	`use` item instead";

/// How to name an import, or what it imports, when a path names both.
const THROUGH_IMPORT: &str = "the path names an item through a private import: name the import with its `use` path \
	(quoted as one argument, like `'use crate::a::Name'`), or the item with its own path";

/// A hint naming the options that get past an error of an operation.
fn hint(error: &rscode::Error, resolver: &Resolver<'_>) -> Option<String> {
	let workspace = resolver.workspace();

	match error {
		rscode::Error::Collision { .. } => Some("pass `--force` to proceed anyway".to_owned()),
		rscode::Error::Ambiguous { path, .. } if let Some(hint) = select_one_crate(resolver, path) => Some(hint),

		// several imports of a module (`use` paths are never ambiguous through imports)
		rscode::Error::Ambiguous { path, .. } if path.starts_with("use ") => Some(SEVERAL_IMPORTS.to_owned()),

		// a path through a private import (see `rscode::edit::remove`)
		rscode::Error::Ambiguous { candidates, .. } if candidates.iter().any(|candidate| candidate.starts_with("`use ")) => {
			Some(THROUGH_IMPORT.to_owned())
		}

		rscode::Error::NotFound(path) => {
			let path = ItemPath::parse(path).ok();
			let member = path.as_ref().and_then(|path| workspace.unloaded_member_of(path));

			// (a selector that picks none of the blocks a loaded header names: the members do not matter)
			if let Some(selector) = path.as_ref().and_then(|path| resolver.selector_hint(path)) {
				return Some(selector);
			}

			// the crate is known: searching the loaded crates would not help
			if member.is_some() {
				return unloaded_hint(workspace, member);
			}

			let hint = (path.as_ref().and_then(|path| resolver.import_hint(path)))
				.or_else(|| retry::suggestion(resolver, path.as_ref()));

			match (hint, unloaded_hint(workspace, None)) {
				(Some(hint), Some(unloaded)) => Some(format!("{hint}\nhint: {unloaded}")),
				(hint, unloaded) => hint.or(unloaded),
			}
		}

		_ => None,
	}
}

/// An error of an operation, with a hint naming the options that get past it (see [`hint`]).
fn hinted(error: rscode::Error, resolver: &Resolver<'_>) -> anyhow::Error {
	let hint = hint(&error, resolver);

	with_hint(error, hint)
}

/// Loads the workspace, reporting load problems (errors, and with `-v` warnings) as warnings.
fn load(ui: &Ui, options: &LoadOptions, absolute_paths: bool) -> anyhow::Result<(Workspace, PathDisplay)> {
	let workspace = rscode::load_workspace(options)?;
	let paths = PathDisplay::new(workspace.root(), absolute_paths);

	if !ui.quiet() {
		// files loaded by several crates report the same problems
		let mut seen = BTreeSet::new();

		for krate in workspace.crates() {
			for problem in krate.diagnostics() {
				if problem.severity == Severity::Error || ui.verbose() > 0 {
					let line = render::diagnostic(problem, &paths);

					if seen.insert(line.clone()) {
						ui.warn(line);
					}
				}
			}
		}
	}

	Ok((workspace, paths))
}

/// Prints an edit report: JSON, or its warnings and notes, then its summary (on stdout) or, for dry runs, its diff
/// (on stdout) and summary (on stderr, so that stdout is a clean diff).
fn print_report(ui: &Ui, format: MessageFormat, report: &impl EditReport) -> anyhow::Result<()> {
	if format == MessageFormat::Json {
		ui::print(&render::json_line(report)?)?;
		return Ok(());
	}

	for (level, message) in report.messages() {
		ui.message(level, message);
	}

	if report.dry_run() {
		ui::print(report.diff().unwrap_or_default())?;
		ui.status(&report.summary());
	} else if !ui.quiet() {
		ui::print(&report.summary())?;
	}

	Ok(())
}

/// Reads new source code from a file or stdin.
fn read_source(source: &SourceArg, ui: &Ui) -> anyhow::Result<String> {
	match source {
		SourceArg::Stdin => {
			let mut stdin = std::io::stdin().lock();

			if stdin.is_terminal() {
				let end = if cfg!(windows) { "Ctrl-Z" } else { "Ctrl-D" };

				ui.note(format!("reading the source from stdin (end it with {end})"));
			}

			let mut text = String::new();

			stdin.read_to_string(&mut text).context("failed to read the source from stdin")?;
			Ok(text)
		}

		SourceArg::File(path) => std::fs::read_to_string(path).with_context(|| format!("failed to read `{}`", path.display())),
	}
}

/// Runs the subcommand `name`.
pub(crate) fn run(name: &str, matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	match name {
		"find" => find::run(matches, ui),
		"view" => view::run(matches, ui),
		"refs" => refs::run(matches, ui),
		"fmt" => fmt::run(matches, ui, false),
		"sort" => fmt::run(matches, ui, true),
		"rename" => rename::run(matches, ui),
		"remove" => remove::run(matches, ui),
		"replace" => replace::run(matches, ui),
		"edit" => edit::run(matches, ui),
		"insert" => insert::run(matches, ui),
		"create-module" => create_module::run(matches, ui),
		"import" => import::run(matches, ui),

		#[cfg(feature = "mcp")]
		"mcp" => mcp::run(matches),

		_ => anyhow::bail!("unknown subcommand `{name}`"),
	}
}

/// How to pick one of the several crates that the items a path names are in, if they are: by target (see
/// [`SELECT_ONE_CRATE`]) when they are crates of one package, or else by crate name or package.
fn select_one_crate(resolver: &Resolver<'_>, path: &str) -> Option<String> {
	let workspace = resolver.workspace();
	let path = ItemPath::parse(path).ok()?;
	let crates: BTreeSet<_> = resolver.resolve_item_path(&path).iter().map(|item| item.krate()).collect();
	let packages: BTreeSet<_> = crates.iter().map(|&krate| workspace.krate(krate).package()).collect();

	match (crates.len(), packages.len()) {
		(0 | 1, _) => None,
		(_, 1) => Some(SELECT_ONE_CRATE.to_owned()),

		_ => {
			let mut names: Vec<&str> = crates.iter().map(|&krate| workspace.krate(krate).name().as_str()).collect();
			let mut seen = BTreeSet::new();

			names.retain(|name| seen.insert(*name));

			Some(format!(
				"the path names items of crates of several packages (`{}`): start it with the name of one of these \
				 crates rather than `crate`, or select one package with `-p NAME`",
				names.join("`, `")
			))
		}
	}
}

/// How to include the workspace members that are not loaded: `member` (whose crate was asked for) if given, or else
/// all of them. Members that are selected, but none of whose crates the target options (`--lib`, `--bin`, ...) select,
/// are not loaded either.
fn unloaded_hint(workspace: &Workspace, member: Option<&UnloadedMember>) -> Option<String> {
	let is_selected = |member: &UnloadedMember| workspace.packages().iter().any(|package| package.name == member.name);

	if let Some(member) = member {
		let hint = match is_selected(member) {
			true => format!(
				"`{}` is selected, but the target options (like `--lib` and `--bin`) leave out all of its crates",
				member.name
			),

			false => format!(
				"`{0}` is a workspace member that is not selected, so its crates are not loaded: pass `-p {0}` or \
				 `--workspace`",
				member.name
			),
		};

		return Some(hint);
	}

	let (selected, names): (Vec<&UnloadedMember>, Vec<&UnloadedMember>) =
		workspace.unloaded_members().iter().partition(|member| is_selected(member));
	let names: Vec<&str> = names.iter().map(|member| member.name.as_str()).collect();
	let selected: Vec<String> = selected.iter().map(|member| format!("`{}`", member.name)).collect();

	let unselected = match names.as_slice() {
		[] => None,

		[name] => Some(format!(
			"the workspace member `{name}` is not selected, so its crates were not searched: pass `-p {name}` or \
			 `--workspace`"
		)),

		_ => Some(format!(
			"{} workspace members are not selected, so their crates were not searched ({}): pass `-p NAME` or \
			 `--workspace`",
			names.len(),
			names.join(", ")
		)),
	};
	let filtered = (!selected.is_empty()).then(|| {
		format!(
			"the target options (like `--lib` and `--bin`) leave out all crates of the selected workspace {} {}",
			if selected.len() == 1 { "member" } else { "members" },
			selected.join(", ")
		)
	});

	match (unselected, filtered) {
		(Some(unselected), Some(filtered)) => Some(format!("{unselected}\nhint: {filtered}")),
		(unselected, filtered) => unselected.or(filtered),
	}
}

fn with_hint(error: rscode::Error, hint: Option<String>) -> anyhow::Error {
	match hint {
		Some(hint) => anyhow::anyhow!("{error}\nhint: {hint}"),
		None => error.into(),
	}
}
