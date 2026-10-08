//! `view`: the source, or an outline, of items.
//!
//! A path that names nothing in the selected packages is searched in the other workspace members (see
//! [`super::retry`]), and a plain path that names nothing stands for the only item whose path ends like it.

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
use rscode::View;
use std::process::ExitCode;

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	let args = ViewArgs::from_matches(matches);
	let output = OutputArgs::from_matches(matches);
	let options = args::load_options(matches)?;
	let targets = args.paths.iter().map(|path| ItemPath::parse(path)).collect::<Result<Vec<_>, _>>()?;
	let (text, notes) = retry::run(ui, &options, output.absolute_paths, Search::Everything, |_, _, paths, resolver| {
		let mut view = View::with_options(args.options.clone());
		let mut notes = Vec::new();

		for path in &targets {
			// a plain path that names nothing stands for the only item whose path ends like it
			let path = match resolver.resolve_item_path(path).is_empty() {
				false => path.clone(),

				true => match retry::by_suffix(resolver, path) {
					Some((found, note)) => {
						notes.push(note);
						found
					}

					None => return Err(Failure::NotFound(rscode::Error::NotFound(path.to_string()))),
				},
			};

			view = view.item_path(path);
		}

		let views = view.run_with(resolver).map_err(|error| retry::fail(error, resolver))?;
		let text = match output.format {
			MessageFormat::Json => render::view_json(&views, paths).map_err(anyhow::Error::from)?,

			MessageFormat::Human | MessageFormat::FileLines => {
				let rows: Vec<ViewRow> = views.iter().map(|view| ViewRow::new(view, paths)).collect();

				render::view_human(&rows)
			}
		};

		Ok((text, notes))
	})?;

	for note in notes {
		ui.note(note);
	}

	ui::print(&text)?;
	Ok(ExitCode::SUCCESS)
}
