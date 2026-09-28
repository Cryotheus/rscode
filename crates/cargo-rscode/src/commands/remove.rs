//! `remove`: items, with their attached comments, and the files of out-of-line modules.

use crate::args;
use crate::args::OutputArgs;
use crate::args::RemoveArgs;
use crate::render;
use crate::report::RemovalReport;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::ItemPath;
use rscode::Resolver;
use std::process::ExitCode;

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	let args = RemoveArgs::from_matches(matches);
	let output = OutputArgs::from_matches(matches);
	let mut options = args::load_options(matches)?;

	// dangling references (and imports to prune) are searched in every crate of the workspace
	options.load_all_members = true;

	let (workspace, paths) = super::load(ui, &options, output.absolute_paths)?;
	let resolver = Resolver::new(&workspace);
	let targets = args.paths.iter().map(|path| ItemPath::parse(path)).collect::<Result<Vec<_>, _>>()?;
	let plan = rscode::edit::remove(&resolver, &targets, &args.options).map_err(|error| super::hinted(error, &resolver))?;
	let mut report = RemovalReport::new(&plan, args.dry_run, &paths);

	match args.dry_run {
		true => report.diff = Some(render::edit_diff(&plan.edits, &paths)?),
		false => drop(plan.edits.apply()?),
	}

	super::print_report(ui, output.format, &report)?;
	Ok(ExitCode::SUCCESS)
}
