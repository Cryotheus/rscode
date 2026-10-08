//! `view`: the source, or an outline, of items.
//!
//! Paths that name nothing in the selected packages are searched in the other workspace members (see
//! [`super::retry`]), and what they name there is shown after the rest; a plain path that names nothing stands for the
//! only item whose path ends like it.

use super::retry;
use super::retry::Failure;
use super::retry::Search;
use crate::args;
use crate::args::MessageFormat;
use crate::args::OutputArgs;
use crate::args::ViewArgs;
use crate::render;
use crate::render::ViewRow;
use crate::ui;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::ItemPath;
use rscode::Resolver;
use rscode::View;
use rscode::query::ItemView;
use std::process::ExitCode;

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
    let args = ViewArgs::from_matches(matches);
    let output = OutputArgs::from_matches(matches);
    let options = args::load_options(matches)?;
    let targets = args
        .paths
        .iter()
        .map(|path| ItemPath::parse(path))
        .collect::<Result<Vec<_>, _>>()?;
    let (workspace, paths) = super::load(ui, &options, output.absolute_paths)?;
    let resolver = Resolver::new(&workspace);
    let mut notes = Vec::new();
    let (found, missing) = retry::split(&resolver, &targets, &mut notes);
    let mut views =
        view(&args, &resolver, &found).map_err(|error| super::hinted(error, &resolver))?;

    // (only the paths that name nothing: with more crates selected, the others could name more items)
    if let Some(first) = missing.first() {
        let load = retry::Load {
            ui,
            options: &options,
            absolute_paths: output.absolute_paths,
        };
        let error = rscode::Error::NotFound(first.to_string());

        views.extend(retry::again(
            load,
            Search::Everything,
            &resolver,
            error,
            &missing,
            |_, _, _, resolver| {
                let (found, missing) = retry::split(resolver, &missing, &mut notes);

                if let Some(first) = missing.first() {
                    return Err(Failure::NotFound(rscode::Error::NotFound(
                        first.to_string(),
                    )));
                }

                view(&args, resolver, &found).map_err(|error| retry::fail(error, resolver))
            },
        )?);
    }

    let text = match output.format {
        MessageFormat::Json => render::view_json(&views, &paths)?,

        MessageFormat::Human | MessageFormat::FileLines => {
            let rows: Vec<ViewRow> = views
                .iter()
                .map(|view| ViewRow::new(view, &paths))
                .collect();

            render::view_human(&rows)
        }
    };

    for note in notes {
        ui.note(note);
    }

    ui::print(&text)?;
    Ok(ExitCode::SUCCESS)
}

/// The views of the items that `paths` name in `resolver`'s workspace.
fn view(
    args: &ViewArgs,
    resolver: &Resolver<'_>,
    paths: &[ItemPath],
) -> Result<Vec<ItemView>, rscode::Error> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    let mut view = View::with_options(args.options.clone());

    for path in paths {
        view = view.item_path(path.clone());
    }

    view.run_with(resolver)
}
