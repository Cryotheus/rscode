//! `create-module`: a module's file, and its `mod` declaration.

use super::retry;
use super::retry::Failure;
use super::retry::Search;
use crate::args;
use crate::args::CreateModuleArgs;
use crate::args::OutputArgs;
use crate::render;
use crate::report::ModuleReport;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::Error;
use rscode::ItemPath;
use rscode::Resolver;
use std::process::ExitCode;

/// An error of [`rscode::edit::create_module`], with a hint naming the argument or option that gets past it.
fn hinted(error: Error, resolver: &Resolver<'_>, name: &str) -> anyhow::Error {
    let hint = match &error {
		Error::Collision { .. } => "choose another NAME".to_owned(),
		Error::InvalidIdent(_) => "NAME is the identifier of the new module alone (such as `render`), not a path".to_owned(),

		Error::Io { source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists => {
			format!("declare the existing file with `cargo rscode insert PARENT` and the source `mod {name};`, or choose another NAME")
		}

		Error::InvalidSource(message) if message.starts_with("the source") => {
			"SOURCE is the whole file of the module: items, after `//!` docs and inner attributes if any".to_owned()
		}

		Error::InvalidSource(_) => {
			"--vis takes `pub`, `pub(crate)`, `pub(super)`, or `pub(in path)`; leave it out for a private module".to_owned()
		}

		_ => return super::hinted(error, resolver),
	};

    super::with_hint(error, Some(hint))
}

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
    let args = CreateModuleArgs::from_matches(matches);
    let output = OutputArgs::from_matches(matches);
    let options = args::load_options(matches)?;
    let source = match &args.source {
        Some(source) => super::read_source(source, ui)?,
        None => String::new(),
    };
    let parent = ItemPath::parse(&args.parent)?;
    let report = retry::run(
        ui,
        &options,
        output.absolute_paths,
        Search::OneCrate,
        |_, _, paths, resolver| {
            let plan =
                rscode::edit::create_module(resolver, &parent, &args.name, &source, &args.options)
                    .map_err(|error| match error {
                        Error::NotFound(_) => Failure::NotFound(error),
                        error => Failure::Other(hinted(error, resolver, &args.name)),
                    })?;
            let mut report = ModuleReport::new(&plan, &args.parent, args.dry_run, paths);

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
