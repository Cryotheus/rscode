//! `replace`: the source of an item.

use super::format_edited;
use crate::args;
use crate::args::OutputArgs;
use crate::args::ReplaceArgs;
use crate::render;
use crate::report::ReplacementReport;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::ItemPath;
use rscode::Resolver;
use std::process::ExitCode;

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	let args = ReplaceArgs::from_matches(matches);
	let output = OutputArgs::from_matches(matches);
	let options = args::load_options(matches)?;
	let source = super::read_source(&args.source, ui)?;
	let path = ItemPath::parse(&args.path)?;
	let (workspace, paths) = super::load(ui, &options, output.absolute_paths)?;
	let resolver = Resolver::new(&workspace);
	let plan = rscode::edit::replace(&resolver, &path, &source, &args.options).map_err(|error| {
		let hint = super::hint(&error, &resolver);
		let all_variants =
			matches!(error, rscode::Error::Ambiguous { .. }) && rscode::edit::replaces_all_variants(&resolver, &path);
		let hint = match (hint, all_variants) {
			// (`cfg` variants of an import: removing and inserting is not the better way)
			(Some(_), true) if path.import => Some("pass `--all-variants` to replace every one of them".to_owned()),
			(Some(hint), true) => Some(format!("{hint}, or `--all-variants` to replace all")),
			(None, true) => Some("pass `--all-variants` to replace every one of them".to_owned()),
			(hint, false) => hint,
		};

		super::with_hint(error, hint)
	})?;
	let mut report = ReplacementReport::new(&plan, args.dry_run, &paths);

	if args.dry_run {
		report.diff = Some(render::edit_diff(&plan.edits, &paths)?);

		if args.format {
			report.warnings.push("the diff shows the replacement before `--fmt` formats it".to_owned());
		}
	} else {
		// what to format is decided before the edit changes the items
		let targets = args.format.then(|| format_edited::replaced(&resolver, &path, &plan.files));

		report.warnings.extend(plan.edits.apply()?.warnings);

		if let Some(targets) = targets {
			let (formatted, warnings) = format_edited::format(&options, targets, &paths);

			report.formatted = formatted;
			report.warnings.extend(warnings);
		}
	}

	super::print_report(ui, output.format, &report)?;
	Ok(ExitCode::SUCCESS)
}
