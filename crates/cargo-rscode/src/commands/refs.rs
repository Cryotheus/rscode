//! `refs`: the references to items, in every crate of the workspace.
//!
//! Every `cfg` variant of an item is searched, and for items of traits also the items implementing them (and the
//! other way around), since a call through either names both. Certain references are always printed; method calls,
//! names inside of macro bodies, and doc links on request. Paths that name nothing in the selected packages are
//! searched in the other workspace members (see [`super::retry`]), and a plain path that names nothing stands for the
//! only item whose path ends like it.

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
use rscode::Resolver;
use rscode::query::FoundReference;
use rscode::query::ReferenceReport;
use rscode::query::find_references;
use std::process::ExitCode;

/// What the search for references does not find.
const UNSEARCHED: &str = "uses in attributes (derives, attribute macro arguments, and paths in strings like \
	`#[serde(default = \"name\")]`) are not searched";

/// Adds the references of `more`, found in another load of the workspace, to `report`.
fn merge(report: &mut ReferenceReport, more: ReferenceReport) {
	let at = |found: &FoundReference| (found.reference.path.clone(), found.reference.range);

	report.targets.extend(more.targets);
	report.targets.sort();
	report.targets.dedup();
	report.references.extend(more.references);
	report.references.sort_by_key(at);
	report.references.dedup_by(|later, kept| at(later) == at(kept));

	for note in more.notes {
		if !report.notes.contains(&note) {
			report.notes.push(note);
		}
	}
}

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	let args = RefsArgs::from_matches(matches);
	let output = OutputArgs::from_matches(matches);
	let mut options = args::load_options(matches)?;

	// references in every crate of the workspace are found
	options.load_all_members = true;

	let targets = args.paths.iter().map(|path| ItemPath::parse(path)).collect::<Result<Vec<_>, _>>()?;
	let (workspace, paths) = super::load(ui, &options, output.absolute_paths)?;
	let resolver = Resolver::new(&workspace);
	let mut notes = Vec::new();
	let (found, missing) = retry::split(&resolver, &targets, &mut notes);
	let mut report = match found.is_empty() {
		true => ReferenceReport::default(),
		false => find_references(&resolver, &found, &args.options).map_err(|error| super::hinted(error, &resolver))?,
	};

	// (only the paths that name nothing: with more crates selected, the others could name more items)
	if let Some(first) = missing.first() {
		let load = retry::Load {
			ui,
			options: &options,
			absolute_paths: output.absolute_paths,
		};
		let error = rscode::Error::NotFound(first.to_string());
		let more = retry::again(load, Search::Everything, &resolver, error, &missing, |_, _, _, resolver| {
			let (found, missing) = retry::split(resolver, &missing, &mut notes);

			if let Some(first) = missing.first() {
				return Err(Failure::NotFound(rscode::Error::NotFound(first.to_string())));
			}

			find_references(resolver, &found, &args.options).map_err(|error| retry::fail(error, resolver))
		})?;

		merge(&mut report, more);
	}

	let rows: Vec<ReferenceRow> = report.references.iter().map(|found| ReferenceRow::new(found, &paths)).collect();
	let text = match output.format {
		MessageFormat::Json => render::json_line(&rows)?,
		MessageFormat::Human | MessageFormat::FileLines => render::references_human(&rows),
	};

	for note in notes {
		ui.note(note);
	}

	ui::print(&text)?;

	for note in &report.notes {
		ui.note(note);
	}

	// (nothing found is no proof that nothing uses it)
	if report.references.is_empty() {
		ui.note("no references found");
		ui.note(UNSEARCHED);
	}

	Ok(ExitCode::SUCCESS)
}
