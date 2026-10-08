//! `import`: into a module, as new `use` items or merged into its `use` items.

use super::retry;
use super::retry::Failure;
use super::retry::Search;
use crate::args;
use crate::args::ImportArgs;
use crate::args::OutputArgs;
use crate::render;
use crate::report::ImportReport;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::Error;
use rscode::ItemPath;
use rscode::Resolver;
use rscode::edit::ImportOptions;
use std::process::ExitCode;

/// An error of [`rscode::edit::add_imports`], with a hint naming the argument that gets past it.
fn hinted(error: Error, resolver: &Resolver<'_>) -> anyhow::Error {
	let hint = match &error {
		Error::Collision { .. } => "import it under another name ('x::Y as Z'), or remove what has the name",
		Error::InvalidSource(_) => "each PATH is a `use` tree, such as `std::fs`, 'crate::a::{B, C}', or 'x::Y as Z'",
		_ => return super::hinted(error, resolver),
	};

	super::with_hint(error, Some(hint.to_owned()))
}

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	let args = ImportArgs::from_matches(matches);
	let output = OutputArgs::from_matches(matches);
	let options = args::load_options(matches)?;
	let module = ItemPath::parse(&args.module)?;
	let report = retry::run(ui, &options, output.absolute_paths, Search::OneCrate, |_, _, paths, resolver| {
		let plan = rscode::edit::add_imports(resolver, &module, &args.paths, &ImportOptions::default()).map_err(|error| match error {
			Error::NotFound(_) => Failure::NotFound(error),
			error => Failure::Other(hinted(error, resolver)),
		})?;
		let mut report = ImportReport::new(&plan, &args.module, args.dry_run, paths);

		match args.dry_run {
			true => report.diff = Some(render::edit_diff(&plan.edits, paths).map_err(anyhow::Error::from)?),
			false => report.warnings.extend(plan.edits.apply().map_err(anyhow::Error::from)?.warnings),
		}

		Ok(report)
	})?;

	super::print_report(ui, output.format, &report)?;
	Ok(ExitCode::SUCCESS)
}
