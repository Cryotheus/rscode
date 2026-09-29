//! Terminal output: results on stdout; errors, warnings, notes, and status messages on stderr.

use anyhow::Context as _;
use cargo::util::command_prelude::ArgMatchesExt as _;
use cargo::util::style;
use clap::ArgMatches;
use clap::builder::styling::Style;
use std::fmt::Display;
use std::io::ErrorKind;
use std::io::IsTerminal as _;
use std::io::Write;

/// The kind of a message on stderr.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum Level {
	Error,
	Warning,
	Note,
}

impl Level {
	fn label(self) -> &'static str {
		match self {
			Self::Error => "error",
			Self::Warning => "warning",
			Self::Note => "note",
		}
	}

	fn style(self) -> Style {
		match self {
			Self::Error => style::ERROR,
			Self::Warning => style::WARN,
			Self::Note => style::NOTE,
		}
	}
}

/// Verbosity and colors of the stderr output (`-q`, `-v`, `--color`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Ui {
	quiet: bool,
	verbose: u32,
	color: bool,
}

impl Ui {
	/// From a subcommand's matches (every subcommand has `-q`, `-v`, and `--color`).
	pub(crate) fn new(matches: &ArgMatches) -> Self {
		Self {
			quiet: matches.flag("quiet"),
			verbose: matches.verbose(),
			color: stderr_color(matches._value_of("color")),
		}
	}

	pub(crate) fn error(&self, message: impl Display) {
		self.write(&format_message(Level::Error, &message.to_string(), self.color));
	}

	/// A warning or note (nothing with `-q`), or an error.
	pub(crate) fn message(&self, level: Level, message: impl Display) {
		if level == Level::Error || !self.quiet {
			self.write(&format_message(level, &message.to_string(), self.color));
		}
	}

	pub(crate) fn note(&self, message: impl Display) {
		self.message(Level::Note, message);
	}

	/// `-q`: only errors (and results) are printed.
	pub(crate) fn quiet(&self) -> bool {
		self.quiet
	}

	/// Plain status lines on stderr (nothing with `-q`), for summaries that must not mix with results on stdout.
	pub(crate) fn status(&self, text: &str) {
		if !self.quiet && !text.is_empty() {
			self.write(text.strip_suffix('\n').unwrap_or(text));
		}
	}

	/// The number of `-v`s.
	pub(crate) fn verbose(&self) -> u32 {
		self.verbose
	}

	pub(crate) fn warn(&self, message: impl Display) {
		self.message(Level::Warning, message);
	}

	fn write(&self, line: &str) {
		// nothing sensible is left to do when stderr is gone
		let _ = writeln!(std::io::stderr().lock(), "{line}");
	}
}

/// A `level: message` line, with the label colored like cargo's.
pub(crate) fn format_message(level: Level, message: &str, color: bool) -> String {
	let label = level.label();

	if color {
		let style = level.style();

		format!("{style}{label}{style:#}: {message}")
	} else {
		format!("{label}: {message}")
	}
}

/// Writes results to stdout.
///
/// When stdout is a pipe that was closed (`cargo rscode find x | head -1`), the results are dropped: the reader
/// wanted no more, and the command still ends with its own exit code (`fmt --check` fails when files would change).
pub(crate) fn print(text: &str) -> anyhow::Result<()> {
	write_results(&mut std::io::stdout().lock(), text).context("failed to write to stdout")
}

/// Whether to color stderr: `--color`, else cargo's `CARGO_TERM_COLOR`, else when stderr is a terminal and
/// `NO_COLOR` is not set.
fn stderr_color(choice: Option<&str>) -> bool {
	let choice = choice.map(str::to_owned).or_else(|| std::env::var("CARGO_TERM_COLOR").ok());

	match choice.map(|choice| choice.to_ascii_lowercase()).as_deref() {
		Some("always") => true,
		Some("never") => false,
		_ => std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()),
	}
}

/// Writes and flushes `text`, taking a closed pipe for success (see [`print`]).
fn write_results(out: &mut impl Write, text: &str) -> std::io::Result<()> {
	match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
		Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
		result => result,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A writer that accepts `capacity` bytes, then fails like a pipe whose reader is gone, or with `error`.
	struct Pipe {
		written: Vec<u8>,
		capacity: usize,
		error: ErrorKind,
	}

	impl Write for Pipe {
		fn flush(&mut self) -> std::io::Result<()> {
			Ok(())
		}

		fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
			let room = self.capacity - self.written.len();

			if room == 0 {
				return Err(self.error.into());
			}

			let count = room.min(bytes.len());

			self.written.extend_from_slice(&bytes[..count]);
			Ok(count)
		}
	}

	#[test]
	fn a_closed_pipe_is_not_an_error() {
		// `cargo rscode fmt --check | head -n 3`: the reader leaves before the diff is written, and `fmt` must still
		// exit with 1, so the write cannot fail
		let mut out = pipe(4, ErrorKind::BrokenPipe);

		write_results(&mut out, "--- a/src/big.rs\n+++ b/src/big.rs\n").unwrap();
		assert_eq!(out.written, b"--- ");
		write_results(&mut out, "more\n").unwrap();
	}

	#[test]
	fn color_choice() {
		assert!(stderr_color(Some("always")));
		assert!(stderr_color(Some("ALWAYS")));
		assert!(!stderr_color(Some("never")));
	}

	#[test]
	fn formats_messages() {
		assert_eq!(format_message(Level::Error, "oops", false), "error: oops");
		assert_eq!(format_message(Level::Warning, "hmm", false), "warning: hmm");
		assert_eq!(format_message(Level::Note, "fyi", false), "note: fyi");

		let colored = format_message(Level::Error, "oops", true);

		assert!(colored.starts_with('\u{1b}'), "{colored:?}");
		assert!(colored.contains("error\u{1b}[0m: oops"), "{colored:?}");
	}

	#[test]
	fn other_write_errors_are_errors() {
		let mut out = pipe(0, ErrorKind::StorageFull);
		let error = write_results(&mut out, "x").unwrap_err();

		assert_eq!(error.kind(), ErrorKind::StorageFull);
	}

	fn pipe(capacity: usize, error: ErrorKind) -> Pipe {
		Pipe {
			written: Vec::new(),
			capacity,
			error,
		}
	}

	#[test]
	fn reads_verbosity_from_the_arguments() {
		let matches = crate::cli::cli()
			.try_get_matches_from(["cargo-rscode", "view", "x", "-vv", "--color", "never"])
			.unwrap();
		let ui = Ui::new(matches.subcommand().unwrap().1);

		assert_eq!(ui.verbose(), 2);
		assert!(!ui.quiet());
		assert!(!ui.color);

		let matches = crate::cli::cli().try_get_matches_from(["cargo-rscode", "view", "x", "-q"]).unwrap();

		assert!(Ui::new(matches.subcommand().unwrap().1).quiet());
	}

	#[test]
	fn writes_results() {
		let mut out = pipe(100, ErrorKind::BrokenPipe);

		write_results(&mut out, "a\n").unwrap();
		write_results(&mut out, "b\n").unwrap();
		assert_eq!(out.written, b"a\nb\n");
	}
}
