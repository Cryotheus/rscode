//! `rename`: an item, with its references everywhere in the workspace.
//!
//! Every `cfg` variant is renamed (items with one path under different `cfg`s are one item in intent). A collision
//! with an existing name refuses the rename, unless `--force`.

use super::retry;
use super::retry::Search;
use crate::args;
use crate::args::OutputArgs;
use crate::args::RenameArgs;
use crate::render;
use crate::report::RenameReport;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::ItemPath;
use std::process::ExitCode;

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
    let args = RenameArgs::from_matches(matches);
    let output = OutputArgs::from_matches(matches);
    let mut options = args::load_options(matches)?;

    // references in every crate of the workspace are updated
    options.load_all_members = true;

    let path = ItemPath::parse(&args.path)?;
    let report = retry::run(
        ui,
        &options,
        output.absolute_paths,
        Search::OneCrate,
        |_, _, paths, resolver| {
            let plan = rscode::edit::rename(resolver, &path, &args.new_name, &args.options)
                .map_err(|error| retry::fail(error, resolver))?;
            let mut report = RenameReport::new(&plan, &args.new_name, args.dry_run, paths);

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
