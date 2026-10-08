//! `remove`: items, with their attached comments, and the files of out-of-line modules.

use super::retry;
use super::retry::Search;
use crate::args;
use crate::args::OutputArgs;
use crate::args::RemoveArgs;
use crate::render;
use crate::report::RemovalReport;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::ItemPath;
use std::process::ExitCode;

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
    let args = RemoveArgs::from_matches(matches);
    let output = OutputArgs::from_matches(matches);
    let mut options = args::load_options(matches)?;

    // dangling references (and imports to prune) are searched in every crate of the workspace
    options.load_all_members = true;

    let targets = args
        .paths
        .iter()
        .map(|path| ItemPath::parse(path))
        .collect::<Result<Vec<_>, _>>()?;

    // (selecting more crates could make the other paths name more items, which would be removed too)
    let search = match targets.len() {
        1 => Search::OneCrate,
        _ => Search::Selected,
    };
    let report = retry::run(
        ui,
        &options,
        output.absolute_paths,
        search,
        |_, _, paths, resolver| {
            let plan = rscode::edit::remove(resolver, &targets, &args.options)
                .map_err(|error| retry::fail(error, resolver))?;
            let mut report = RemovalReport::new(&plan, args.dry_run, paths);

            match args.dry_run {
                true => {
                    report.diff =
                        Some(render::edit_diff(&plan.edits, paths).map_err(anyhow::Error::from)?)
                }

                false => report
                    .warnings
                    .extend(plan.edits.apply().map_err(anyhow::Error::from)?.warnings),
            }

            Ok(report)
        },
    )?;

    super::print_report(ui, output.format, &report)?;
    Ok(ExitCode::SUCCESS)
}
