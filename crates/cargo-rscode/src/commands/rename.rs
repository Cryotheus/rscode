//! `rename`: an item, with its references everywhere in the workspace.
//!
//! Every `cfg` variant is renamed (items with one path under different `cfg`s are one item in intent). A collision
//! with an existing name refuses the rename, unless `--force`.

use crate::args;
use crate::args::OutputArgs;
use crate::args::RenameArgs;
use crate::render;
use crate::report::RenameReport;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::ItemPath;
use rscode::Resolver;
use std::process::ExitCode;

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	let args = RenameArgs::from_matches(matches);
	let output = OutputArgs::from_matches(matches);
	let mut options = args::load_options(matches)?;

	// references in every crate of the workspace are updated
	options.load_all_members = true;

	let (workspace, paths) = super::load(ui, &options, output.absolute_paths)?;
	let resolver = Resolver::new(&workspace);
	let plan = rscode::edit::rename(&resolver, &ItemPath::parse(&args.path)?, &args.new_name, &args.options)
		.map_err(|error| super::hinted(error, &resolver))?;
	let mut report = RenameReport::new(&plan, &args.new_name, args.dry_run, &paths);

	match args.dry_run {
		true => report.diff = Some(render::edit_diff(&plan.edits, &paths)?),
		false => report.warnings.extend(plan.edits.apply()?.warnings),
	}

	super::print_report(ui, output.format, &report)?;
	Ok(ExitCode::SUCCESS)
}
