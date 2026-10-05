//! The emit formats, compared with rustfmt's own output. rustfmt's `json` and `checkstyle` emit modes are unstable, so
//! the comparisons need a nightly rustfmt, and are skipped (with a message) without one.

mod common;

use common::fixture;
use common::rustfmt;
use common::rustfmt_available;
use common::rustfmt_unstable_emit;
use rscode_fmt::Formatter;
use rscode_fmt::emit;
use rscode_fmt::emit::FileChange;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Writes `original` to a temporary file and returns its path.
fn write_sample(name: &str, original: &str) -> PathBuf {
	let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("emit");

	std::fs::create_dir_all(&directory).unwrap();

	let path = directory.join(format!("{name}.rs"));

	std::fs::write(&path, original).unwrap();
	path
}

/// Runs `rustfmt --emit <emit>` on files (which it does not modify) and returns what it prints.
fn rustfmt_emit(emit: &str, paths: &[PathBuf]) -> String {
	let output = Command::new(common::rustfmt_program())
		.args(["--edition", "2024", "--emit", emit, "--config-path"])
		.arg(fixture("rustfmt/default/rustfmt.toml"))
		.args(paths)
		.output()
		.unwrap();

	assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

	String::from_utf8(output.stdout).unwrap()
}

/// The change of a sample file as rscode_fmt computes it, named like rustfmt names it: canonicalized (on Windows,
/// `\\?\C:\...`).
fn change(path: &Path, original: &str) -> FileChange {
	FileChange {
		path: std::fs::canonicalize(path).unwrap(),
		original: original.to_owned(),
		formatted: Formatter::new(rustfmt()).format_str(original).unwrap(),
	}
}

/// Asserts that `json` and `checkstyle` print exactly what rustfmt prints for the samples.
fn assert_like_rustfmt(samples: &[(&str, &str)]) {
	let paths: Vec<PathBuf> = samples.iter().map(|(name, original)| write_sample(name, original)).collect();
	let changes: Vec<FileChange> = paths.iter().zip(samples).map(|(path, (_, original))| change(path, original)).collect();

	assert_eq!(emit::json(&changes), rustfmt_emit("json", &paths));
	assert_eq!(emit::checkstyle(&changes), rustfmt_emit("checkstyle", &paths));
}

#[test]
fn several_blocks() {
	if !rustfmt_unstable_emit() {
		return;
	}

	assert_like_rustfmt(&[(
		"several_blocks",
		"fn a( ){let x=1;}\n\n// keep\nfn b() {\n    let y = 2;\n}\nfn c( ){let z=3;\nlet w = 4;}\nstruct S{a:u8,b:u16}\n",
	)]);
}

#[test]
fn removed_lines() {
	if !rustfmt_unstable_emit() {
		return;
	}

	assert_like_rustfmt(&[
		("removed_blank_lines", "fn a() {}\n\n\n\nfn b() {}\n"),
		("removed_final_lines", "fn a() {}\n\n\n"),
	]);
}

#[test]
fn final_line_breaks() {
	if !rustfmt_unstable_emit() {
		return;
	}

	assert_like_rustfmt(&[("missing_final_line_break", "fn a() {}"), ("crlf", "fn a( ) {}\r\nfn b() {}\r\n")]);
}

#[test]
fn escaping() {
	if !rustfmt_unstable_emit() {
		return;
	}

	assert_like_rustfmt(&[("escaping", "fn a() { let s = \"<&>\\\"'\";let t=1; }\n")]);
}

#[test]
fn unchanged_files() {
	if !rustfmt_unstable_emit() {
		return;
	}

	// omitted from the json, present but empty in the checkstyle
	assert_like_rustfmt(&[("formatted", "fn a() {}\n"), ("unformatted", "fn  b( ) {}\n"), ("formatted_too", "struct S;\n")]);
	assert_like_rustfmt(&[("only_formatted", "fn a() {}\n")]);
}

#[test]
fn many_blocks() {
	if !rustfmt_unstable_emit() {
		return;
	}

	// many similar lines (`}`, blank lines) make the alignment of lines ambiguous
	let original: String = (0..300)
		.map(|index| match index % 7 {
			0 => format!("fn f{index}() {{\n    let x = {index};\n}}\n"),
			1 => format!("fn  f{index}( ){{let x={index};}}\n"),
			2 => format!("\n\n\nfn f{index}() {{}}\n"),
			3 => format!("struct S{index}{{a:u8,b:u8}}\n"),
			4 => format!("struct S{index} {{\n    a: u8,\n}}\n"),
			5 => format!("// c{index}\nfn f{index}() {{ }}\n"),
			_ => format!("impl S{index} {{\n    fn a(&self) {{}}\n  fn b(&self){{}}\n}}\n\n"),
		})
		.collect();

	assert_like_rustfmt(&[("many_blocks", &original)]);
}

#[test]
fn unified_diff_of_a_rustfmt_run() {
	if !rustfmt_available() {
		return;
	}

	let path = write_sample("unified", "fn  a( ) {}\nfn b() {}\n");
	let change = change(&path, "fn  a( ) {}\nfn b() {}\n");
	// an absolute path is not prefixed
	let expected = format!(
		"--- {path}\n+++ {path}\n@@ -1,2 +1,2 @@\n-fn  a( ) {{}}\n+fn a() {{}}\n fn b() {{}}\n",
		path = change.path.display(),
	);

	assert_eq!(emit::unified_diff(&[change], 3), expected);
}
