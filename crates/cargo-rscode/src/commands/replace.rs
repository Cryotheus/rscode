//! `replace`: the source of an item.

use super::format_edited;
use super::retry;
use super::retry::Failure;
use super::retry::Search;
use crate::args;
use crate::args::OutputArgs;
use crate::args::ReplaceArgs;
use crate::render;
use crate::report::ReplacementReport;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::ItemPath;
use std::process::ExitCode;

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
    let args = ReplaceArgs::from_matches(matches);
    let output = OutputArgs::from_matches(matches);
    let options = args::load_options(matches)?;
    let source = super::read_source(&args.source, ui)?;
    let path = ItemPath::parse(&args.path)?;
    let report = retry::run(
        ui,
        &options,
        output.absolute_paths,
        Search::OneCrate,
        |options, _, paths, resolver| {
            let plan = rscode::edit::replace(resolver, &path, &source, &args.options).map_err(
                |error| {
                    if matches!(error, rscode::Error::NotFound(_)) {
                        return Failure::NotFound(error);
                    }

                    // (`Type::name` names a method, and the source is the field `Type.name`)
                    let field = matches!(error, rscode::Error::InvalidSource(_))
                        .then(|| resolver.field_hint(&path))
                        .flatten();
                    let hint = super::hint(&error, resolver).or(field);
                    let all_variants = matches!(error, rscode::Error::Ambiguous { .. })
                        && rscode::edit::replaces_all_variants(resolver, &path);
                    let hint = match (hint, all_variants) {
                        // (`cfg` variants of an import: removing and inserting is not the better way)
                        (Some(_), true) if path.import => {
                            Some("pass `--all-variants` to replace every one of them".to_owned())
                        }

                        (Some(hint), true) => {
                            Some(format!("{hint}, or `--all-variants` to replace all"))
                        }

                        (None, true) => {
                            Some("pass `--all-variants` to replace every one of them".to_owned())
                        }

                        (hint, false) => hint,
                    };

                    Failure::Other(super::with_hint(error, hint))
                },
            )?;
            let mut report = ReplacementReport::new(&plan, args.dry_run, paths);

            if args.dry_run {
                report.diff =
                    Some(render::edit_diff(&plan.edits, paths).map_err(anyhow::Error::from)?);

                if args.format {
                    report.warnings.push(
                        "the diff shows the replacement before `--fmt` formats it".to_owned(),
                    );
                }
            } else {
                // what to format is decided before the edit changes the items
                let targets = args
                    .format
                    .then(|| format_edited::replaced(resolver, &path, &plan.files));

                report
                    .warnings
                    .extend(plan.edits.apply().map_err(anyhow::Error::from)?.warnings);

                if let Some(targets) = targets {
                    let (formatted, warnings) = format_edited::format(options, targets, paths);

                    report.formatted = formatted;
                    report.warnings.extend(warnings);
                }
            }

            Ok(report)
        },
    )?;

    super::print_report(ui, output.format, &report)?;
    Ok(ExitCode::SUCCESS)
}
