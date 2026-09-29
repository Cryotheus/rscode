//! Formatting with rustfmt, run as a subprocess.

use crate::FormatError;
use crate::RustFmtOptions;
use crate::config;
use crate::source::BOM;
use crate::source::uses_crlf;
use crate::source::with_line_breaks;
use std::borrow::Cow;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::thread;

/// Configuration keeping rustfmt from merging, splitting, regrouping, or reordering items, so that the items of its
/// output correspond one-to-one to the items of its input.
///
/// `reorder_imports` is left alone: without it, rustfmt does not wrap long import lists. It only reorders adjacent
/// `use` (and `extern crate`) items among each other, which [`crate::items`] accounts for.
const PRESERVE_ITEMS: &[(&str, &str)] = &[
	("reorder_modules", "false"),
	("reorder_impl_items", "false"),
	("imports_granularity", "Preserve"),
	("group_imports", "Preserve"),
];

/// Runs the command, writing `input` to its stdin and collecting its output.
///
/// Stdin is written on a separate thread, as rustfmt may fill its stdout pipe before it has read all of its input.
fn communicate(mut command: Command, input: &str) -> io::Result<Output> {
	let mut child = command.spawn()?;
	let stdin = child.stdin.take();

	let (output, written) = thread::scope(|scope| {
		let writer = scope.spawn(move || match stdin {
			// dropping stdin closes it, signaling the end of the input
			Some(mut stdin) => stdin.write_all(input.as_bytes()),
			None => Ok(()),
		});

		let output = child.wait_with_output();
		let written = writer.join().unwrap_or_else(|_| Err(io::Error::other("writing to stdin panicked")));

		(output, written)
	});

	let output = output?;

	// a failing rustfmt may exit before reading its input: report its error rather than the broken pipe
	if output.status.success() {
		written?;
	}

	Ok(output)
}

/// Joins the `--config` overrides into one argument, validating them.
///
/// Overrides of the `forced` keys are dropped, and the forced values are appended.
fn config_arg(overrides: &[(String, String)], forced: &[(&str, &str)]) -> Result<Option<String>, FormatError> {
	let mut pairs = Vec::with_capacity(overrides.len() + forced.len());

	for (key, value) in overrides {
		// rustfmt splits the argument at commas, and each pair at its first `=`
		if key.is_empty() || key.contains(['=', ',']) || value.contains(',') {
			return Err(FormatError::InvalidRustFmtConfig(format!("{key}={value}")));
		}

		if !forced.iter().any(|&(forced_key, _)| forced_key == key) {
			pairs.push(format!("{key}={value}"));
		}
	}

	pairs.extend(forced.iter().map(|(key, value)| format!("{key}={value}")));

	Ok((!pairs.is_empty()).then(|| pairs.join(",")))
}

fn failure_message(output: &Output) -> String {
	let stderr = String::from_utf8_lossy(&output.stderr);
	let stderr = stderr.trim();

	if stderr.is_empty() {
		format!("rustfmt exited with {}", output.status)
	} else {
		stderr.to_owned()
	}
}

/// Formats a whole file's worth of source text with rustfmt (source passed over stdin). A byte order mark is kept.
pub(crate) fn format(source: &str, options: &RustFmtOptions) -> Result<String, FormatError> {
	run(source, options, &[])
}

/// Like [`format()`], but rustfmt only reorders adjacent `use` and `extern crate` items, and never merges or splits
/// them or reorders other items.
pub(crate) fn format_preserving_items(source: &str, options: &RustFmtOptions) -> Result<String, FormatError> {
	run(source, options, PRESERVE_ITEMS)
}

/// Whether a program path is relative and has a directory component (so it is not looked up in `PATH`).
fn is_relative_path(program: &Path) -> bool {
	program.is_relative() && program.parent().is_some_and(|parent| !parent.as_os_str().is_empty())
}

/// The rustfmt executable: [`RustFmtOptions::program`], else `$RUSTFMT`, else `rustfmt` from `PATH`.
pub(crate) fn program(options: &RustFmtOptions) -> PathBuf {
	if let Some(program) = &options.program {
		return program.clone();
	}

	match std::env::var_os("RUSTFMT") {
		Some(program) if !program.is_empty() => PathBuf::from(program),
		_ => PathBuf::from("rustfmt"),
	}
}

fn run(source: &str, options: &RustFmtOptions, forced: &[(&str, &str)]) -> Result<String, FormatError> {
	let config = config_arg(&options.config, forced)?;
	let mut program = program(options);
	let mut working_directory = None;
	let mut config_path_arg = None;

	if let Some(config_path) = &options.config_path {
		if config_path.is_dir() {
			// reading stdin, rustfmt searches for its configuration upwards from its working directory
			// (`--config-path <dir>` would only look inside the directory itself)
			working_directory = Some(config_path);

			// a relative program path would be resolved against the new working directory
			if is_relative_path(&program)
				&& let Ok(absolute) = std::path::absolute(&program)
			{
				program = absolute;
			}
		} else {
			config_path_arg = Some(config_path);
		}
	}

	let mut command = Command::new(&program);

	command.args(["--emit", "stdout", "--color", "never"]);

	command.args(["--edition", options.edition.unwrap_or_default().as_str()]);

	if let Some(style_edition) = options.style_edition {
		command.args(["--style-edition", style_edition.as_str()]);
	}

	if let Some(config_path) = config_path_arg {
		command.arg("--config-path").arg(config_path);
	}

	if let Some(config) = config {
		command.arg("--config").arg(config);
	}

	if let Some(directory) = working_directory {
		command.current_dir(directory);
	}

	command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

	let io_error = |source: io::Error| FormatError::RustFmtSpawn {
		program: program.display().to_string(),
		source,
	};

	let output = communicate(command, source).map_err(io_error)?;

	if !output.status.success() {
		return Err(FormatError::RustFmt {
			stderr: failure_message(&output),
		});
	}

	let mut formatted = String::from_utf8(output.stdout).map_err(|_| FormatError::RustFmt {
		stderr: "rustfmt printed invalid UTF-8".to_owned(),
	})?;

	// rustfmt drops a byte order mark from its input, but keeps it when it formats files
	if source.starts_with(BOM) && !formatted.starts_with(BOM) {
		formatted.insert_str(0, BOM);
	}

	// `newline_style = "Auto"` means the line breaks of the input, but rustfmt sees its input with `\r\n` turned into
	// `\n`, and gives input without line breaks those of the platform: keep the input's (`\n` if it has none, so that
	// the output is the same on every platform)
	if config::newline_style_is_auto(options)
		&& let Cow::Owned(converted) = with_line_breaks(&formatted, uses_crlf(source))
	{
		formatted = converted;
	}

	Ok(formatted)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::Edition;
	use std::sync::OnceLock;

	#[test]
	fn config_args() {
		let overrides = vec![
			("max_width".to_owned(), "80".to_owned()),
			("reorder_imports".to_owned(), "true".to_owned()),
		];

		assert_eq!(config_arg(&[], &[]).unwrap(), None);
		assert_eq!(config_arg(&overrides, &[]).unwrap().as_deref(), Some("max_width=80,reorder_imports=true"));
		assert_eq!(
			config_arg(&overrides, &[("reorder_imports", "false"), ("reorder_modules", "false")])
				.unwrap()
				.as_deref(),
			Some("max_width=80,reorder_imports=false,reorder_modules=false"),
		);

		for (key, value) in [("", "1"), ("a=b", "1"), ("a,b", "1"), ("max_width", "1,hard_tabs=true")] {
			let error = config_arg(&[(key.to_owned(), value.to_owned())], &[]).unwrap_err();

			assert!(matches!(error, FormatError::InvalidRustFmtConfig(_)), "{key}={value}: {error:?}");
		}

		// `=` is fine in values: rustfmt splits at the first one
		assert!(config_arg(&[("key".to_owned(), "a=b".to_owned())], &[]).is_ok());
	}

	/// Configuration with rustfmt's defaults, so tests do not depend on configuration files around the repository.
	fn default_config() -> PathBuf {
		PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rustfmt/default/rustfmt.toml"))
	}

	#[test]
	fn formats_large_input_without_deadlocking() {
		if !rustfmt_available() {
			return;
		}

		// larger than any pipe buffer, in both directions
		let source: String = (0..20_000).map(|index| format!("fn  f{index}( ){{let x=1;}}\n")).collect();
		let formatted = format(&source, &options()).unwrap();

		assert_eq!(formatted.lines().count(), 20_000 * 3);
		assert!(formatted.ends_with("fn f19999() {\n    let x = 1;\n}\n"));
	}

	#[test]
	fn formats_through_stdin() {
		if !rustfmt_available() {
			return;
		}

		assert_eq!(format("fn  main( ){let x=1;}", &options()).unwrap(), "fn main() {\n    let x = 1;\n}\n");
	}

	fn options() -> RustFmtOptions {
		RustFmtOptions {
			edition: Some(Edition::E2024),
			config_path: Some(default_config()),
			..RustFmtOptions::default()
		}
	}

	#[test]
	fn preserving_items_disables_reordering() {
		if !rustfmt_available() {
			return;
		}

		let source = "use b::c;\nuse a::b;\nmod z;\nmod y;\nimpl S {\n    fn f() {}\n    type T = u8;\n}\n";

		assert_eq!(
			format(source, &options()).unwrap(),
			"use a::b;\nuse b::c;\nmod y;\nmod z;\nimpl S {\n    fn f() {}\n    type T = u8;\n}\n",
		);

		// only imports are still reordered
		let preserved = "use a::b;\nuse b::c;\nmod z;\nmod y;\nimpl S {\n    fn f() {}\n    type T = u8;\n}\n";

		assert_eq!(format_preserving_items(source, &options()).unwrap(), preserved);

		// even when the configuration asks for more
		let mut options = options();

		options.config.push(("reorder_modules".to_owned(), "true".to_owned()));
		options.config.push(("reorder_impl_items".to_owned(), "true".to_owned()));
		options.config.push(("imports_granularity".to_owned(), "Crate".to_owned()));

		assert_eq!(format_preserving_items(source, &options).unwrap(), preserved);
		assert_eq!(
			format_preserving_items("use a::b;\nuse a::c;\n", &options).unwrap(),
			"use a::b;\nuse a::c;\n"
		);
	}

	#[test]
	fn preserving_items_keeps_wrapping_imports() {
		if !rustfmt_available() {
			return;
		}

		let source = "use crate::expr::{ExprBreak, ExprRange, ExprRawAddr, ExprReference, ExprReturn, ExprUnary, ExprYield};\n";
		let wrapped = "use crate::expr::{\n    ExprBreak, ExprRange, ExprRawAddr, ExprReference, ExprReturn, ExprUnary, ExprYield,\n};\n";

		assert_eq!(format_preserving_items(source, &options()).unwrap(), wrapped);
	}

	#[test]
	fn program_prefers_the_option() {
		let options = RustFmtOptions {
			program: Some(PathBuf::from("/opt/rustfmt")),
			..RustFmtOptions::default()
		};

		assert_eq!(program(&options), Path::new("/opt/rustfmt"));
	}

	#[test]
	fn relative_program_paths() {
		assert!(!is_relative_path(Path::new("rustfmt")));
		assert!(is_relative_path(Path::new("./rustfmt")));
		assert!(is_relative_path(Path::new("bin/rustfmt")));
		assert!(!is_relative_path(&std::path::absolute("bin/rustfmt").unwrap()));
	}

	#[test]
	fn reports_rustfmt_errors() {
		if !rustfmt_available() {
			return;
		}

		match format("fn main( {", &options()) {
			Err(FormatError::RustFmt { stderr }) => {
				assert!(stderr.contains("<stdin>"), "{stderr}");
				assert_eq!(stderr, stderr.trim());
				assert!(!stderr.contains('\u{1b}'), "no color codes: {stderr:?}");
			}
			other => panic!("unexpected result: {other:?}"),
		}
	}

	#[test]
	fn reports_spawn_errors() {
		let options = RustFmtOptions {
			program: Some(PathBuf::from("/nonexistent/rustfmt")),
			..RustFmtOptions::default()
		};

		match format("fn main() {}", &options) {
			Err(FormatError::RustFmtSpawn { program, source }) => {
				assert_eq!(program, "/nonexistent/rustfmt");
				assert_eq!(source.kind(), io::ErrorKind::NotFound);
			}
			other => panic!("unexpected result: {other:?}"),
		}
	}

	fn rustfmt_available() -> bool {
		static AVAILABLE: OnceLock<bool> = OnceLock::new();

		*AVAILABLE.get_or_init(|| {
			let available = Command::new(program(&RustFmtOptions::default()))
				.arg("--version")
				.output()
				.is_ok_and(|output| output.status.success());

			if !available {
				eprintln!("rustfmt is not available: skipping tests that run it");
			}

			available
		})
	}
}
