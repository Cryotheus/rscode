//! The subcommands.

mod find;
mod fmt;
mod format_edited;
mod insert;
#[cfg(feature = "mcp")]
mod mcp;
mod remove;
mod rename;
mod replace;
mod view;

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

/// Runs the subcommand `name`.
pub(crate) fn run(name: &str, matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	match name {
		"find" => find::run(matches, ui),
		"view" => view::run(matches, ui),
		"fmt" => fmt::run(matches, ui, false),
		"sort" => fmt::run(matches, ui, true),
		"rename" => rename::run(matches, ui),
		"remove" => remove::run(matches, ui),
		"replace" => replace::run(matches, ui),
		"insert" => insert::run(matches, ui),
		#[cfg(feature = "mcp")]
		"mcp" => mcp::run(matches),
		_ => anyhow::bail!("unknown subcommand `{name}`"),
	}
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

/// How to pick one of several crates that a path names items of.
const SELECT_ONE_CRATE: &str = "the path names items of several crates (such as the library and a binary of a \
	package, whose roots are both `crate`): select one with `--lib` or `--bin NAME`";

/// How to name an import, or what it imports, when a path names both.
const THROUGH_IMPORT: &str = "the path names an item through a private import: name the import with its `use` path \
	(quoted as one argument, like `'use crate::a::Name'`), or the item with its own path";

/// How to replace one of several imports that a `use` path names.
const SEVERAL_IMPORTS: &str = "the `use` path names several imports of its module: remove them and insert the new \
	`use` item instead";

/// An error of an operation, with a hint naming the options that get past it (see [`hint`]).
fn hinted(error: rscode::Error, resolver: &Resolver<'_>) -> anyhow::Error {
	let hint = hint(&error, resolver);

	with_hint(error, hint)
}

/// A hint naming the options that get past an error of an operation.
fn hint(error: &rscode::Error, resolver: &Resolver<'_>) -> Option<String> {
	let workspace = resolver.workspace();

	match error {
		rscode::Error::Collision { .. } => Some("pass `--force` to proceed anyway".to_owned()),
		rscode::Error::Ambiguous { path, .. } if in_several_crates(resolver, path) => Some(SELECT_ONE_CRATE.to_owned()),

		// several imports of a module (`use` paths are never ambiguous through imports)
		rscode::Error::Ambiguous { path, .. } if path.starts_with("use ") => Some(SEVERAL_IMPORTS.to_owned()),

		// a path through a private import (see `rscode::edit::remove`)
		rscode::Error::Ambiguous { candidates, .. } if candidates.iter().any(|candidate| candidate.starts_with("`use ")) => {
			Some(THROUGH_IMPORT.to_owned())
		}

		rscode::Error::NotFound(path) => {
			let path = ItemPath::parse(path).ok();
			let member = path.as_ref().and_then(|path| workspace.unloaded_member_of(path));

			unloaded_hint(workspace, member).or_else(|| path.and_then(|path| resolver.import_hint(&path)))
		}

		_ => None,
	}
}

fn with_hint(error: rscode::Error, hint: Option<String>) -> anyhow::Error {
	match hint {
		Some(hint) => anyhow::anyhow!("{error}\nhint: {hint}"),
		None => error.into(),
	}
}

/// Whether the items a path names are in several crates.
fn in_several_crates(resolver: &Resolver<'_>, path: &str) -> bool {
	let Ok(path) = ItemPath::parse(path) else {
		return false;
	};

	let crates: BTreeSet<_> = resolver.resolve_item_path(&path).iter().map(|item| item.krate()).collect();

	crates.len() > 1
}

/// How to include the workspace members that are not loaded: `member` (whose crate was asked for) if given, or else
/// all of them.
fn unloaded_hint(workspace: &Workspace, member: Option<&UnloadedMember>) -> Option<String> {
	if let Some(member) = member {
		return Some(format!(
			"`{0}` is a workspace member that is not selected, so its crates are not loaded: pass `-p {0}` or \
			 `--workspace`",
			member.name
		));
	}

	let names: Vec<&str> = workspace.unloaded_members().iter().map(|member| member.name.as_str()).collect();

	match names.as_slice() {
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
	}
}

/// Reads new source code from a file or stdin.
fn read_source(source: &SourceArg, ui: &Ui) -> anyhow::Result<String> {
	match source {
		SourceArg::Stdin => {
			let mut stdin = std::io::stdin().lock();

			if stdin.is_terminal() {
				ui.note("reading the source from stdin (end it with Ctrl-D)");
			}

			let mut text = String::new();

			stdin.read_to_string(&mut text).context("failed to read the source from stdin")?;
			Ok(text)
		}

		SourceArg::File(path) => {
			std::fs::read_to_string(path).with_context(|| format!("failed to read `{}`", path.display()))
		}
	}
}
