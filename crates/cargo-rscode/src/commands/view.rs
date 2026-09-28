//! `view`: the source, or an outline, of items.

use crate::args;
use crate::args::MessageFormat;
use crate::args::OutputArgs;
use crate::args::ViewArgs;
use crate::render;
use crate::render::ViewRow;
use crate::ui;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::Resolver;
use rscode::View;
use std::process::ExitCode;

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	let args = ViewArgs::from_matches(matches);
	let output = OutputArgs::from_matches(matches);
	let (workspace, paths) = super::load(ui, &args::load_options(matches)?, output.absolute_paths)?;
	let resolver = Resolver::new(&workspace);
	let mut view = View::with_options(args.options);

	for path in &args.paths {
		view = view.path(path)?;
	}

	let views = view.run_with(&resolver).map_err(|error| super::hinted(error, &resolver))?;
	let text = match output.format {
		MessageFormat::Json => render::view_json(&views, &paths)?,

		MessageFormat::Human | MessageFormat::FileLines => {
			let rows: Vec<ViewRow> = views.iter().map(|view| ViewRow::new(view, &paths)).collect();

			render::view_human(&rows)
		}
	};

	ui::print(&text)?;
	Ok(ExitCode::SUCCESS)
}
