//! `edit`: text inside an item, and its attributes, doc comment, and visibility, in place.

use super::format_edited;
use super::retry;
use super::retry::Failure;
use super::retry::Search;
use crate::args;
use crate::args::EditArgs;
use crate::args::OutputArgs;
use crate::args::TextArg;
use crate::render;
use crate::report::ItemEditReport;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::ItemPath;
use rscode::edit::ItemEdit;
use rscode::edit::TextReplacement;
use std::process::ExitCode;

/// How to go on when the edited item would become another kind of item, or several items.
const KIND_CHANGE: &str = "pass `--allow-kind-change` to let the item become another kind of item, or several items";

/// How to go on when `--old` does not occur in the item.
const NOT_FOUND: &str = "copy `--old` exactly from the output of `cargo rscode view` (with or without its `-n` line numbers)";

/// How to go on when `--old` occurs several times in the item.
const SEVERAL: &str = "include more of the surrounding text in `--old` so that it occurs once, or copy it with the \
	line numbers of `cargo rscode view -n` to pick one";

/// How to go on when the path names several `cfg` variants.
const VARIANTS: &str = "give `--old` text that only one of them has, or pass `--all-variants` to edit every one of them";

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	let args = EditArgs::from_matches(matches)?;
	let output = OutputArgs::from_matches(matches);
	let options = args::load_options(matches)?;
	let read = |text: &TextArg| match text {
		TextArg::Inline(text) => Ok(text.clone()),
		TextArg::Read(source) => super::read_source(source, ui),
	};
	let edit = ItemEdit {
		replacements: (args.replacements.iter())
			.map(|(old, new)| Ok(TextReplacement { old: read(old)?, new: read(new)? }))
			.collect::<anyhow::Result<_>>()?,
		remove_attributes: args.remove_attributes.clone(),
		add_attributes: args.add_attributes.clone(),
		doc: args.doc.as_ref().map(read).transpose()?,
		visibility: args.visibility.clone(),
	};

	if edit.is_empty() {
		anyhow::bail!("nothing to change: pass `--old` and `--new`, `--vis`, `--doc`, `--add-attr`, or `--remove-attr`");
	}

	edit.check()?;

	let path = ItemPath::parse(&args.path)?;
	let report = retry::run(ui, &options, output.absolute_paths, Search::OneCrate, |options, _, paths, resolver| {
		let plan = rscode::edit::edit_item(resolver, &path, &edit, &args.options).map_err(|error| {
			if matches!(error, rscode::Error::NotFound(_)) {
				return Failure::NotFound(error);
			}

			let hint = super::hint(&error, resolver);
			let all_variants =
				matches!(error, rscode::Error::Ambiguous { .. }) && rscode::edit::replaces_all_variants(resolver, &path);

			let hint = match (&error, hint, all_variants) {
				(rscode::Error::KindChange(_), ..) => Some(KIND_CHANGE.to_owned()),
				(rscode::Error::TextMismatch { lines, .. }, ..) if lines.is_empty() => Some(NOT_FOUND.to_owned()),
				(rscode::Error::TextMismatch { .. }, ..) => Some(SEVERAL.to_owned()),
				(_, Some(hint), true) => Some(format!("{hint}, or `--all-variants` to edit all")),
				(_, None, true) => Some(VARIANTS.to_owned()),
				(_, hint, false) => hint,
			};

			Failure::Other(super::with_hint(error, hint))
		})?;
		let mut report = ItemEditReport::new(&plan, args.dry_run, paths);

		if args.dry_run {
			report.diff = Some(render::edit_diff(&plan.edits, paths).map_err(anyhow::Error::from)?);

			if args.format {
				report.warnings.push("the diff shows the edit before `--fmt` formats it".to_owned());
			}
		} else {
			// what to format is decided before the edit changes the items
			let targets =
				(args.format && !plan.spans.is_empty()).then(|| format_edited::replaced(resolver, &path, &plan.files));

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
