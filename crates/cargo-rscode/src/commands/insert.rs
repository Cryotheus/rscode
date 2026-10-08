//! `insert`: items into a module, `impl` block, or trait.

use super::format_edited;
use super::retry;
use super::retry::Failure;
use super::retry::Search;
use crate::args;
use crate::args::InsertArgs;
use crate::args::OutputArgs;
use crate::render;
use crate::report::InsertionReport;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::ItemPath;
use std::process::ExitCode;

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	let args = InsertArgs::from_matches(matches)?;
	let output = OutputArgs::from_matches(matches);
	let options = args::load_options(matches)?;
	let source = super::read_source(&args.source, ui)?;
	let parent = args.parent.as_deref().map(ItemPath::parse).transpose()?;
	let report = retry::run(ui, &options, output.absolute_paths, Search::OneCrate, |options, _, paths, resolver| {
		let plan = rscode::edit::insert(resolver, parent.as_ref(), &source, &args.options).map_err(|error| match (&error, &parent) {
			// several containers have the anchor
			(rscode::Error::Ambiguous { .. }, None) => {
				Failure::Other(super::with_hint(error, Some("pass one of the candidates as PARENT".to_owned())))
			}

			_ => retry::fail(error, resolver),
		})?;
		let mut report = InsertionReport::new(&plan, args.parent.as_deref().unwrap_or(&plan.parent), args.dry_run, paths);

		if args.dry_run {
			report.diff = Some(render::edit_diff(&plan.edits, paths).map_err(anyhow::Error::from)?);

			if args.format {
				report.warnings.push("the diff shows the items before `--fmt` formats them".to_owned());
			}
		} else {
			// the container is resolved before the edit changes it
			let parent = match &parent {
				Some(parent) => parent.clone(),
				None => ItemPath::parse(&plan.parent).map_err(anyhow::Error::from)?,
			};
			let targets = args
				.format
				.then(|| format_edited::inserted(resolver, &parent, &plan.file, &plan.inserted, &plan.imports));

			report.warnings.extend(plan.edits.apply().map_err(anyhow::Error::from)?.warnings);

			if let Some(targets) = targets {
				let (formatted, warnings) = format_edited::format(options, targets, paths);

				report.formatted = formatted;
				report.warnings.extend(warnings);
			}
		}

		Ok(report)
	})?;

	super::print_report(ui, output.format, &report)?;
	Ok(ExitCode::SUCCESS)
}
