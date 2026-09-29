//! `fmt` and `sort`: sorting (with the Cryotheum ordering schema) and formatting items.
//!
//! Targets are path patterns (globs allowed); `crate` is the crate root of every selected crate. Like rustfmt there
//! is `--skip-children`, `--check`, and `--emit files|stdout|diff|json|checkstyle` (the json and checkstyle output
//! is rustfmt's).

// rustfmt also has `--emit coverage`; a `coverage-json` might be nice

use crate::args::Emit;
use crate::args::FmtArgs;
use crate::args::MessageFormat;
use crate::args::OutputArgs;
use crate::render;
use crate::report::FormatReport;
use crate::ui;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::MatchOptions;
use rscode::PathPattern;
use rscode::Resolver;
use rscode::edit::FileChange;
use rscode::rscode_fmt::emit;
use std::process::ExitCode;

/// The warning for `--message-format json` with an `--emit` mode that prints something else: only `--emit files` has
/// a JSON report of rscode's own, and `--emit json` is rustfmt's JSON.
fn json_warning(format: MessageFormat, emit: Emit) -> Option<String> {
	(format == MessageFormat::Json && matches!(emit, Emit::Stdout | Emit::Diff | Emit::Checkstyle)).then(|| {
		format!(
			"`--message-format json` does not apply to `--emit {}` (`--emit json` prints rustfmt's JSON)",
			emit.name()
		)
	})
}

/// Ends non-empty text with a line break.
fn line(mut text: String) -> String {
	if !text.is_empty() && !text.ends_with('\n') {
		text.push('\n');
	}

	text
}

/// `sort_only`: run `sort` rather than `fmt`.
pub(super) fn run(matches: &ArgMatches, ui: &Ui, sort_only: bool) -> anyhow::Result<ExitCode> {
	let args = FmtArgs::from_matches(matches, sort_only)?;
	let output = OutputArgs::from_matches(matches);

	if let Some(warning) = json_warning(output.format, args.emit) {
		ui.warn(warning);
	}

	let (workspace, paths) = super::load(ui, &args.load_options(matches)?, output.absolute_paths)?;
	let resolver = Resolver::new(&workspace);
	let targets = args
		.targets
		.iter()
		.map(|target| PathPattern::parse(target, MatchOptions::default()))
		.collect::<Result<Vec<_>, _>>()?;
	let formatting = rscode::edit::format(&resolver, &targets, &args.options)?;

	// the JSON report of written files carries the warnings itself
	if !(args.emit == Emit::Files && output.format == MessageFormat::Json) {
		for warning in &formatting.warnings {
			ui.warn(warning);
		}
	}

	if formatting.changes.is_empty() {
		ui.note("no files matched the targets");
	}

	// every processed file: the emitters skip unchanged files where rustfmt does
	let changes = render::display_changes(&formatting.changes, &paths);

	// decided before printing: how much of the output is read (`| head`) does not change the verdict
	let code = match args.check && changes.iter().any(FileChange::is_changed) {
		true => ExitCode::FAILURE,
		false => ExitCode::SUCCESS,
	};

	match args.emit {
		Emit::Files => {
			let applied = formatting.edits.apply()?;
			let report = FormatReport {
				files: applied.written.iter().map(|file| paths.display(file)).collect(),
				warnings: formatting.warnings.clone(),
				sort_only,
			};

			match output.format {
				MessageFormat::Json => ui::print(&render::json_line(&report)?)?,
				_ if ui.quiet() => {}
				_ => ui::print(&report.summary())?,
			}
		}

		Emit::Stdout => ui::print(&render::formatted_contents(&changes))?,
		Emit::Diff => ui::print(&render::unified_diff(&formatting.changes, &paths))?,
		Emit::Json => ui::print(&line(emit::json(&changes)))?,
		Emit::Checkstyle => ui::print(&line(emit::checkstyle(&changes)))?,
	}

	Ok(code)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ends_lines() {
		assert_eq!(line("[]".to_owned()), "[]\n");
		assert_eq!(line("x\n".to_owned()), "x\n");
		assert_eq!(line(String::new()), "");
	}

	#[test]
	fn warns_about_json_that_is_not_printed() {
		assert_eq!(
			json_warning(MessageFormat::Json, Emit::Diff).as_deref(),
			Some("`--message-format json` does not apply to `--emit diff` (`--emit json` prints rustfmt's JSON)")
		);
		assert!(json_warning(MessageFormat::Json, Emit::Stdout).is_some());
		assert!(json_warning(MessageFormat::Json, Emit::Checkstyle).is_some());
		assert_eq!(json_warning(MessageFormat::Json, Emit::Files), None);
		assert_eq!(json_warning(MessageFormat::Json, Emit::Json), None);
		assert_eq!(json_warning(MessageFormat::Human, Emit::Diff), None);
	}
}
