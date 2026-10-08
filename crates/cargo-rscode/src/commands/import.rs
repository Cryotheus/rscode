//! `import`: into a module, as new `use` items or merged into its `use` items.

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
	let (workspace, paths) = super::load(ui, &options, output.absolute_paths)?;
	let resolver = Resolver::new(&workspace);
	let plan = rscode::edit::add_imports(&resolver, &module, &args.paths, &ImportOptions::default())
		.map_err(|error| hinted(error, &resolver))?;
	let mut report = ImportReport::new(&plan, &args.module, args.dry_run, &paths);

	match args.dry_run {
		true => report.diff = Some(render::edit_diff(&plan.edits, &paths)?),
		false => report.warnings.extend(plan.edits.apply()?.warnings),
	}

	super::print_report(ui, output.format, &report)?;
	Ok(ExitCode::SUCCESS)
}
