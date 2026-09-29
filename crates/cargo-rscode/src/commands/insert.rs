//! `insert`: items into a module, `impl` block, or trait.

use super::format_edited;
use crate::args;
use crate::args::InsertArgs;
use crate::args::OutputArgs;
use crate::render;
use crate::report::InsertionReport;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::ItemPath;
use rscode::Resolver;
use std::process::ExitCode;

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	let args = InsertArgs::from_matches(matches)?;
	let output = OutputArgs::from_matches(matches);
	let options = args::load_options(matches)?;
	let source = super::read_source(&args.source, ui)?;
	let parent = ItemPath::parse(&args.parent)?;
	let (workspace, paths) = super::load(ui, &options, output.absolute_paths)?;
	let resolver = Resolver::new(&workspace);
	let plan = rscode::edit::insert(&resolver, &parent, &source, &args.options).map_err(|error| super::hinted(error, &resolver))?;
	let mut report = InsertionReport::new(&plan, &args.parent, args.dry_run, &paths);

	if args.dry_run {
		report.diff = Some(render::edit_diff(&plan.edits, &paths)?);

		if args.format {
			report.warnings.push("the diff shows the items before `--fmt` formats them".to_owned());
		}
	} else {
		// the container is resolved before the edit changes it
		let targets = args.format.then(|| format_edited::inserted(&resolver, &parent, &plan.file, &plan.inserted, &plan.imports));

		plan.edits.apply()?;

		if let Some(targets) = targets {
			let (formatted, warnings) = format_edited::format(&options, targets, &paths);

			report.formatted = formatted;
			report.warnings.extend(warnings);
		}
	}

	super::print_report(ui, output.format, &report)?;
	Ok(ExitCode::SUCCESS)
}
