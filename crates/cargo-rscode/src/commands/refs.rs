//! `refs`: the references to items, in every crate of the workspace.
//!
//! Every `cfg` variant of an item is searched, and for items of traits also the items implementing them (and the
//! other way around), since a call through either names both. Certain references are always printed; method calls,
//! names inside of macro bodies, and doc links on request. Paths that name nothing in the selected packages are
//! searched in the other workspace members (see [`super::retry`]).

use super::retry;
use super::retry::Failure;
use super::retry::Search;
use crate::args;
use crate::args::MessageFormat;
use crate::args::OutputArgs;
use crate::args::RefsArgs;
use crate::render;
use crate::render::ReferenceRow;
use crate::ui;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::ItemPath;
use std::process::ExitCode;

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	let args = RefsArgs::from_matches(matches);
	let output = OutputArgs::from_matches(matches);
	let mut options = args::load_options(matches)?;

	// references in every crate of the workspace are found
	options.load_all_members = true;

	let targets = args.paths.iter().map(|path| ItemPath::parse(path)).collect::<Result<Vec<_>, _>>()?;
	let (text, report, notes) = retry::run(ui, &options, output.absolute_paths, Search::Everything, |_, _, paths, resolver| {
		let mut found = Vec::new();
		let mut notes = Vec::new();

		for path in &targets {
			// a plain path that names nothing stands for the only item whose path ends like it
			match resolver.resolve_item_path(path).is_empty() {
				false => found.push(path.clone()),

				true => match retry::by_suffix(resolver, path) {
					Some((path, note)) => {
						notes.push(note);
						found.push(path);
					}

					None => return Err(Failure::NotFound(rscode::Error::NotFound(path.to_string()))),
				},
			}
		}

		let report =
			rscode::query::find_references(resolver, &found, &args.options).map_err(|error| retry::fail(error, resolver))?;
		let rows: Vec<ReferenceRow> = report.references.iter().map(|found| ReferenceRow::new(found, paths)).collect();
		let text = match output.format {
			MessageFormat::Json => render::json_line(&rows).map_err(anyhow::Error::from)?,
			MessageFormat::Human | MessageFormat::FileLines => render::references_human(&rows),
		};

		Ok((text, report, notes))
	})?;

	for note in notes {
		ui.note(note);
	}

	ui::print(&text)?;

	for note in &report.notes {
		ui.note(note);
	}

	if report.references.is_empty() {
		ui.note("no references found");
	}

	Ok(ExitCode::SUCCESS)
}
