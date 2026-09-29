//! End-to-end tests: the built `cargo-rscode` binary on the fixture crate `tests/fixtures/demo` (a lib and a bin).
//!
//! Commands that modify files run on temporary copies of the fixture. Tests that need rscode's library (loading,
//! resolving, and editing crates) are ignored until it is implemented.

use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

const BIN: &str = env!("CARGO_BIN_EXE_cargo-rscode");

fn fixture() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/demo")
}

/// `text` with `/` replaced by the platform's path separator, which the paths in the output have (except in the
/// headers of diffs).
fn native(text: &str) -> String {
	text.replace('/', std::path::MAIN_SEPARATOR_STR)
}

/// The outcome of running the binary.
#[derive(Debug)]
struct Run {
	code: Option<i32>,
	stdout: String,
	stderr: String,
}

impl Run {
	#[track_caller]
	fn success(self) -> Self {
		assert_eq!(self.code, Some(0), "{self:#?}");
		self
	}

	fn lines(&self) -> Vec<&str> {
		self.stdout.lines().collect()
	}
}

fn command(dir: &Path, args: &[&str]) -> Command {
	let mut command = Command::new(BIN);

	command.args(args).current_dir(dir).env("CARGO_TERM_COLOR", "never").env_remove("COMPLETE");
	command
}

fn run(dir: &Path, args: &[&str]) -> Run {
	finish(command(dir, args), None)
}

fn run_with_stdin(dir: &Path, args: &[&str], stdin: &str) -> Run {
	finish(command(dir, args), Some(stdin))
}

fn finish(mut command: Command, stdin: Option<&str>) -> Run {
	command.stdout(Stdio::piped()).stderr(Stdio::piped());
	command.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });

	let mut child = command.spawn().expect("cargo-rscode starts");

	if let Some(stdin) = stdin {
		child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
	}

	let output = child.wait_with_output().unwrap();

	Run {
		code: output.status.code(),
		stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
		stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
	}
}

/// Asks for the completions of the last word, like the bash script does.
fn completion(dir: &Path, words: &[&str]) -> Run {
	let mut command = Command::new(BIN);

	command
		.arg("--")
		.args(words)
		.current_dir(dir)
		.env("COMPLETE", "bash")
		.env("_CLAP_COMPLETE_INDEX", (words.len() - 1).to_string())
		.env("_CLAP_IFS", "\n");

	finish(command, None)
}

/// The distinct completion candidates for the last word.
fn complete(dir: &Path, words: &[&str]) -> Vec<String> {
	let run = completion(dir, words).success();
	let mut seen = BTreeSet::new();

	run.stdout.lines().filter(|line| !line.is_empty() && seen.insert(*line)).map(str::to_owned).collect()
}

/// The script `COMPLETE=<shell> cargo-rscode` prints.
fn registration(shell: &str) -> Run {
	let mut command = Command::new(BIN);

	command.env("COMPLETE", shell);
	finish(command, None)
}

/// Whether `bash` can run the completion script (bash 4 or newer) and the binary. (On Windows, `Command` finds WSL's
/// `bash` before others, and it cannot run Windows binaries.)
fn has_bash() -> bool {
	let script = format!("(( BASH_VERSINFO[0] >= 4 )) && {} --version", sh_quote(BIN));

	Command::new("bash").args(["-c", &script]).output().is_ok_and(|output| output.status.success())
}

fn sh_quote(text: &str) -> String {
	format!("'{}'", text.replace('\'', r"'\''"))
}

/// `COMPREPLY` of the registered bash completion for `line` (with the cursor at its end), given `words`: the line
/// split like bash splits it for `COMP_WORDS`, at whitespace and at the characters of `COMP_WORDBREAKS` (so
/// `crate::a` is `crate` `::` `a`, and `--kind=fn` is `--kind` `=` `fn`).
fn bash_completion(dir: &Path, line: &str, words: &[&str]) -> Vec<String> {
	let script = registration("bash").success().stdout;
	let words: Vec<String> = words.iter().map(|word| sh_quote(word)).collect();
	let program = format!(
		r#"{script}
COMP_WORDBREAKS=$' \t\n"\'@><=;|&(:'
COMP_LINE={line}
COMP_POINT=${{#COMP_LINE}}
COMP_WORDS=({words})
COMP_CWORD=$(( ${{#COMP_WORDS[@]}} - 1 ))
_clap_complete_cargo_rscode cargo-rscode "${{COMP_WORDS[COMP_CWORD]}}" "${{COMP_WORDS[COMP_CWORD-1]}}"
for reply in "${{COMPREPLY[@]}}"; do printf '[%s]\n' "$reply"; done
"#,
		line = sh_quote(line),
		words = words.join(" "),
	);
	let output = Command::new("bash").arg("-c").arg(program).current_dir(dir).env_remove("COMPLETE").output().unwrap();

	assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

	String::from_utf8(output.stdout)
		.unwrap()
		.lines()
		.map(|reply| reply.strip_prefix('[').and_then(|reply| reply.strip_suffix(']')).unwrap().to_owned())
		.collect()
}

/// Runs the binary with its stdout a pipe that nobody reads (`cargo rscode ... | head -n 0`).
fn run_into_closed_pipe(dir: &Path, args: &[&str]) -> Option<i32> {
	let (reader, writer) = std::io::pipe().unwrap();

	drop(reader);

	command(dir, args).stdout(writer).stderr(Stdio::null()).stdin(Stdio::null()).status().unwrap().code()
}

/// A copy of the fixture in the temporary directory, removed when dropped.
struct TempCopy(PathBuf);

impl TempCopy {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir().join(format!("cargo-rscode-cli-{}-{name}", std::process::id()));

		let _ = std::fs::remove_dir_all(&dir);
		copy_dir(&fixture(), &dir);
		Self(dir)
	}

	/// A directory with the files, instead of a copy of the fixture.
	fn with_files(name: &str, files: &[(&str, &str)]) -> Self {
		let dir = std::env::temp_dir().join(format!("cargo-rscode-cli-{}-{name}", std::process::id()));

		let _ = std::fs::remove_dir_all(&dir);

		for (file, text) in files {
			let path = dir.join(file);

			std::fs::create_dir_all(path.parent().unwrap()).unwrap();
			std::fs::write(path, text).unwrap();
		}

		Self(dir)
	}

	fn path(&self) -> &Path {
		&self.0
	}

	fn read(&self, file: &str) -> String {
		std::fs::read_to_string(self.0.join(file)).unwrap()
	}
}

/// A workspace whose root package `a` has the member `b`, which calls `a::foo`.
fn two_packages(name: &str) -> TempCopy {
	let package = |name: &str, dependencies: &str| {
		format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{dependencies}")
	};

	TempCopy::with_files(
		name,
		&[
			("Cargo.toml", &package("a", "\n[workspace]\nmembers = [\"b\"]\n")),
			("src/lib.rs", "pub fn foo() {}\n"),
			("b/Cargo.toml", &package("b", "\n[dependencies]\na = { path = \"..\" }\n")),
			("b/src/lib.rs", "pub fn bar() {\n\ta::foo();\n}\n"),
		],
	)
}

impl Drop for TempCopy {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

fn copy_dir(from: &Path, to: &Path) {
	std::fs::create_dir_all(to).unwrap();

	for entry in std::fs::read_dir(from).unwrap() {
		let entry = entry.unwrap();
		let target = to.join(entry.file_name());

		if entry.file_type().unwrap().is_dir() {
			if entry.file_name() != "target" {
				copy_dir(&entry.path(), &target);
			}
		} else {
			std::fs::copy(entry.path(), target).unwrap();
		}
	}
}

/// Proves that an edited copy of the fixture still compiles.
#[track_caller]
fn cargo_check(dir: &Path) {
	let output = Command::new(env!("CARGO"))
		.args(["check", "--quiet", "--all-targets"])
		.current_dir(dir)
		.env("CARGO_TARGET_DIR", dir.join("target"))
		.output()
		.unwrap();

	assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

fn read_fixture(file: &str) -> String {
	std::fs::read_to_string(fixture().join(file)).unwrap()
}

#[test]
fn prints_help_and_version() {
	let help = run(&fixture(), &["--help"]).success();

	assert!(help.stdout.contains("Usage: cargo-rscode <COMMAND>"), "{}", help.stdout);

	for subcommand in ["find", "view", "fmt", "sort", "rename", "remove", "replace", "insert"] {
		assert!(help.stdout.contains(&format!("\n  {subcommand} ")), "{subcommand}: {}", help.stdout);
	}

	assert_eq!(help.stdout.contains("\n  mcp "), cfg!(feature = "mcp"), "{}", help.stdout);
	assert!(help.stdout.contains("source <(COMPLETE=bash cargo-rscode)"), "{}", help.stdout);
	assert_eq!(run(&fixture(), &["--version"]).success().stdout, "cargo-rscode 0.1.0\n");
	assert_eq!(run(&fixture(), &["rscode", "--version"]).success().stdout, "cargo-rscode 0.1.0\n");
}

#[test]
fn usage_names_the_invocation() {
	let direct = run(&fixture(), &["find", "--help"]).success();

	assert!(direct.stdout.contains("Usage: cargo-rscode find [OPTIONS]"), "{}", direct.stdout);

	let via_cargo = run(&fixture(), &["rscode", "find", "--help"]).success();

	assert!(via_cargo.stdout.contains("Usage: cargo rscode find [OPTIONS]"), "{}", via_cargo.stdout);
}

#[test]
fn usage_errors_exit_with_2() {
	let typo = run(&fixture(), &["rscode", "fnd"]);

	assert_eq!(typo.code, Some(2));
	assert!(typo.stderr.contains("unrecognized subcommand 'fnd'"), "{}", typo.stderr);
	assert!(typo.stderr.contains("Usage: cargo rscode <COMMAND>"), "{}", typo.stderr);

	for args in [
		&[][..],
		&["find"],
		&["view", "x", "--outline", "--full"],
		&["view", "x", "--cfg", "not(test)"],
		&["view", "x", "-v", "-q"],
		&["view", "x", "-s"],
		&["insert", "crate", "--position", "before"],
		&["find", "x", "--kind", "strukt"],
	] {
		assert_eq!(run(&fixture(), args).code, Some(2), "{args:?}");
	}
}

#[test]
fn errors_exit_with_1() {
	let exclude = run(&fixture(), &["view", "x", "--exclude", "demo"]);

	assert_eq!(exclude.code, Some(1));
	assert_eq!(exclude.stderr, "error: --exclude can only be used together with --workspace\n");

	let packages = run(&fixture(), &["view", "x", "-p"]);

	assert_eq!(packages.code, Some(1));
	assert!(packages.stderr.contains("Possible packages/workspace members:\n    demo\n"), "{}", packages.stderr);

	let bins = run(&fixture(), &["view", "x", "--bin"]);

	assert_eq!(bins.code, Some(1));
	assert_eq!(bins.stderr, "error: \"--bin\" takes one argument.\nAvailable binaries:\n    demo\n");

	let check = run(&fixture(), &["fmt", "--check", "--emit", "files"]);

	assert_eq!(check.code, Some(1));
	assert!(check.stderr.contains("cannot be used with `--emit files`"), "{}", check.stderr);

	let anchor = run(&fixture(), &["insert", "crate", "--anchor", "crate::add"]);

	assert_eq!(anchor.code, Some(1));
	assert!(anchor.stderr.contains("`--anchor` needs `--position before` or `--position after`"), "{}", anchor.stderr);

	let source = run(&fixture(), &["replace", "crate::add", "missing.rs"]);

	assert_eq!(source.code, Some(1));
	assert!(source.stderr.starts_with("error: failed to read `missing.rs`: "), "{}", source.stderr);
	assert_eq!(source.stdout, "");
}

#[test]
fn runs_as_a_cargo_subcommand() {
	// cargo looks for `cargo-rscode` in `PATH`, after `$CARGO_HOME/bin` unless that is in `PATH` already
	let bin_dir = Path::new(BIN).parent().unwrap().to_path_buf();
	let cargo_home = std::env::var_os("CARGO_HOME")
		.map(PathBuf::from)
		.or_else(|| std::env::home_dir().map(|home| home.join(".cargo")))
		.unwrap_or_default();
	let path = std::env::var_os("PATH").unwrap_or_default();
	let path = std::env::join_paths([bin_dir, cargo_home.join("bin")].into_iter().chain(std::env::split_paths(&path)))
		.unwrap();
	let cargo = |args: &[&str]| {
		let mut command = Command::new(env!("CARGO"));

		command.args(args).current_dir(fixture()).env("PATH", &path).env("CARGO_TERM_COLOR", "never");
		finish(command, None)
	};

	assert_eq!(cargo(&["rscode", "--version"]).success().stdout, "cargo-rscode 0.1.0\n");

	let help = cargo(&["rscode", "view", "--help"]).success();

	assert!(help.stdout.contains("Usage: cargo rscode view [OPTIONS] <PATH>..."), "{}", help.stdout);
	assert_eq!(cargo(&["rscode", "vew"]).code, Some(2));
}

#[test]
fn completes_subcommands_and_values() {
	let dir = fixture();

	assert_eq!(complete(&dir, &["cargo-rscode", "f"]), ["find", "fmt"]);
	assert_eq!(complete(&dir, &["cargo-rscode", "find", "x", "--kind", "stru"]), ["struct"]);
	assert_eq!(complete(&dir, &["cargo-rscode", "find", "x", "--show", "kind,sp"]), ["kind,span"]);
	assert_eq!(complete(&dir, &["cargo-rscode", "fmt", "--emit", "j"]), ["json"]);
	assert_eq!(complete(&dir, &["cargo-rscode", "fmt", "--formatter", "pr"]), ["prettyplease"]);

	// words as cargo's completion passes them on
	assert_eq!(complete(&dir, &["cargo", "rscode", "vi"]), ["view"]);
	assert_eq!(complete(&dir, &["rscode", "rem"]), ["remove"]);
}

#[test]
fn completes_packages() {
	assert_eq!(complete(&fixture(), &["cargo-rscode", "view", "-p", "d"]), ["demo"]);

	// once: the word is not also completed as a path (clap asks for both after an option with an optional value)
	for words in [["cargo-rscode", "view", "-p", "d"], ["cargo-rscode", "find", "--package", "d"]] {
		assert_eq!(completion(&fixture(), &words).success().lines(), ["demo"], "{words:?}");
	}

	// the members of the workspace of a typed `--manifest-path`
	let manifest = fixture().join("Cargo.toml");
	let words = ["cargo-rscode", "view", "--manifest-path", manifest.to_str().unwrap(), "--package", "de"];

	assert_eq!(complete(&std::env::temp_dir(), &words), ["demo"]);
}

#[test]
fn completers_never_print_errors() {
	for words in [
		&["cargo-rscode", "view", "crate::"][..],
		&["cargo-rscode", "view", "--manifest-path", "/nonexistent/Cargo.toml", "crate::"],
		&["cargo-rscode", "find", "x", "--from", "crate::"],
		&["cargo-rscode", "view", "-p", ""],
	] {
		let run = completion(&std::env::temp_dir(), words).success();

		assert_eq!(run.stderr, "", "{words:?}");
	}
}

#[test]
fn bash_completes_words_that_bash_splits() {
	if !has_bash() {
		eprintln!("skipped: no bash 4");
		return;
	}

	let dir = fixture();

	assert_eq!(bash_completion(&dir, "cargo-rscode f", &["cargo-rscode", "f"]), ["find", "fmt"]);

	// `,` is no word break: the candidate replaces the whole word
	let words = ["cargo-rscode", "find", "x", "-k", "fn,assoc-f"];

	assert_eq!(bash_completion(&dir, "cargo-rscode find x -k fn,assoc-f", &words), ["fn,assoc-fn"]);

	// bash replaces only the text after the last word break, so candidates are trimmed to that
	let words = ["cargo-rscode", "find", "x", "--kind", "=", "stru"];

	assert_eq!(bash_completion(&dir, "cargo-rscode find x --kind=stru", &words), ["struct"]);

	let words = ["cargo-rscode", "find", "x", "--from", "cr"];

	assert_eq!(bash_completion(&dir, "cargo-rscode find x --from cr", &words), ["crate"]);

	// `::` is complete, and bash keeps the word (outside of a workspace, the anchor is the only candidate)
	let words = ["cargo-rscode", "find", "x", "--from", "::"];

	assert_eq!(bash_completion(&std::env::temp_dir(), "cargo-rscode find x --from ::", &words), [""]);

	// forwarded by cargo's completion: the line starts with `rscode`, the words with `cargo-rscode`
	assert_eq!(bash_completion(&dir, "rscode vi", &["cargo-rscode", "vi"]), ["view"]);
	assert!(bash_completion(&dir, "cargo-rscode nothing-like-this", &["cargo-rscode", "nothing-like-this"]).is_empty());
}

#[test]
fn bash_completes_item_paths() {
	if !has_bash() {
		eprintln!("skipped: no bash 4");
		return;
	}

	let dir = fixture();

	// `crate::sh` reaches bash's completion function as `crate` `::` `sh`, and only `sh` is replaced
	let words = ["cargo-rscode", "view", "crate", "::", "sh"];

	assert_eq!(bash_completion(&dir, "cargo-rscode view crate::sh", &words), ["shapes"]);

	let words = ["cargo-rscode", "view", "crate", "::", "shapes", "::"];

	assert_eq!(bash_completion(&dir, "cargo-rscode view crate::shapes::", &words), ["Circle", "Kind", "Shape"]);

	let words = ["cargo-rscode", "find", "x", "--from", "=", "crate", "::", "u"];

	assert_eq!(bash_completion(&dir, "cargo-rscode find x --from=crate::u", &words), ["util"]);
}

#[test]
fn registers_completion_scripts() {
	let bash = registration("bash").success();

	assert!(bash.stdout.contains("-F _clap_complete_cargo_rscode cargo-rscode"), "{}", bash.stdout);
	// quoted when it has characters special to the shell (like `\` on Windows)
	let invocation = [format!("{BIN} -- "), format!("{} -- ", sh_quote(BIN))];

	assert!(invocation.iter().any(|invocation| bash.stdout.contains(invocation)), "{}", bash.stdout);

	let zsh = registration("zsh").success();

	assert!(zsh.stdout.contains("compdef _clap_dynamic_completer_cargo_rscode cargo-rscode"), "{}", zsh.stdout);
	assert!(zsh.stdout.contains("_cargo-rscode() { _clap_dynamic_completer_cargo_rscode \"$@\" }"), "{}", zsh.stdout);

	let fish = registration("fish").success();

	assert!(fish.stdout.contains("--command cargo --condition __cargo_rscode_args "), "{}", fish.stdout);

	let unknown = registration("tcsh");

	assert_eq!(unknown.code, Some(2));
	assert!(unknown.stderr.contains("unknown shell `tcsh`"), "{}", unknown.stderr);
	assert_eq!(unknown.stdout, "");

	// an empty or "0" `COMPLETE` runs normally
	assert_eq!(registration("").code, Some(2));
	assert_eq!(registration("0").code, Some(2));
}

/// Imports are named by `use` paths; a plain path through a private import is ambiguous for edits.
#[test]
fn names_imports_by_use_paths() {
	let view = run(&fixture(), &["view", "use demo::Circle"]).success();

	assert!(
		view.stdout.starts_with(&format!("// {}", native("use demo::Circle (import) src/lib.rs:6"))),
		"{}",
		view.stdout
	);
	assert!(view.stdout.contains("pub use shapes::Circle;"), "{}", view.stdout);

	let removal = run(&fixture(), &["remove", "--bin", "demo", "use crate::Shape", "--dry-run"]).success();

	assert!(removal.stdout.contains("-use demo::shapes::Shape;"), "{removal:#?}");
	assert!(removal.stderr.contains("use demo::Shape (import)"), "{removal:#?}");

	let ambiguous = run(&fixture(), &["remove", "--bin", "demo", "crate::Shape", "--dry-run"]);

	assert_ne!(ambiguous.code, Some(0), "{ambiguous:#?}");
	assert!(ambiguous.stderr.contains(&native("`use demo::Shape` (import) at src/main.rs:1:5")), "{ambiguous:#?}");
	assert!(ambiguous.stderr.contains(&native("`demo::shapes::Shape` (trait) at src/shapes.rs")), "{ambiguous:#?}");
	assert!(ambiguous.stderr.contains("hint: the path names an item through a private import"), "{ambiguous:#?}");

	// unquoted, the shell splits the path
	let split = run(&fixture(), &["view", "use", "demo::Circle"]);

	assert!(split.stderr.contains("expected a path after `use` (quote the whole path, `use` included)"), "{split:#?}");

	let split_pattern = run(&fixture(), &["find", "use", "Circle"]);

	assert!(split_pattern.stderr.contains("expected a pattern after `use` (quote the whole pattern"), "{split_pattern:#?}");

	// an import's own text is not the path of the import
	let pasted = run(&fixture(), &["remove", "--bin", "demo", "use demo::shapes::Shape", "--dry-run"]);

	assert!(pasted.stderr.contains("hint: a `use` path names the imports of the module"), "{pasted:#?}");
}

#[test]
fn finds_items() {
	let circle = run(&fixture(), &["find", "Circle"]).success();

	assert_eq!(circle.stdout, native("demo::shapes::Circle  struct  src/shapes.rs:3:1\n"));

	let imports = run(&fixture(), &["find", "Circle", "--imports", "--show", "kind"]).success();

	assert!(imports.lines().contains(&"demo::shapes::Circle  struct"), "{}", imports.stdout);
	assert!(imports.lines().contains(&"use demo::Circle  import  -> demo::shapes::Circle"), "{}", imports.stdout);

	// asking for the kind is asking for imports
	let only_imports = run(&fixture(), &["find", "*", "-k", "import", "--show", "kind"]).success();

	assert!(only_imports.lines().contains(&"use demo::Circle  import  -> demo::shapes::Circle"), "{}", only_imports.stdout);
	assert!(only_imports.lines().iter().all(|line| line.contains("  import")), "{}", only_imports.stdout);

	let methods = run(&fixture(), &["find", "--starts-with", "ar", "-k", "assoc-fn", "--show", "location"]).success();

	let area = native("demo::shapes::Shape::area  src/shapes.rs:30:2");

	assert!(methods.lines().contains(&area.as_str()), "{}", methods.stdout);

	let nothing = run(&fixture(), &["find", "Nothing*"]).success();

	assert_eq!(nothing.stdout, "");
	assert_eq!(nothing.stderr, "note: no items found\n");

	let limited = run(&fixture(), &["find", "crate::*", "--limit", "1", "-q"]).success();

	assert_eq!(limited.lines().len(), 1, "{}", limited.stdout);
}

#[test]
fn finds_cfg_variants() {
	let plain = run(&fixture(), &["find", "crate::extra", "--show", "cfg"]).success();

	assert_eq!(
		plain.stdout,
		"demo::extra  cfg: feature = \"extra\"  inactive\ndemo::extra  cfg: not(feature = \"extra\")\n"
	);

	let extra = run(&fixture(), &["find", "crate::extra", "--show", "cfg", "--features", "extra"]).success();

	assert_eq!(
		extra.stdout,
		"demo::extra  cfg: feature = \"extra\"\ndemo::extra  cfg: not(feature = \"extra\")  inactive\n"
	);

	let active = run(&fixture(), &["find", "crate::extra", "--active-only", "--show", "location"]).success();

	assert_eq!(active.stdout, native("demo::extra  src/lib.rs:19:1\n"));
}

#[test]
fn finds_usable_paths() {
	let foreign = run(&fixture(), &["find", "crate::shapes::Circle", "--from", "::", "--show", "kind"]).success();

	assert!(foreign.stdout.starts_with("demo::shapes::Circle  struct  usable: "), "{}", foreign.stdout);
	assert!(foreign.stdout.contains("::demo::Circle"), "{}", foreign.stdout);

	let local = run(&fixture(), &["find", "crate::shapes::Circle", "--from", "crate", "--show", "kind"]).success();

	assert!(local.stdout.contains("crate::Circle"), "{}", local.stdout);

	let missing = run(&fixture(), &["find", "Circle", "--from", "crate::add"]);

	assert_eq!(missing.code, Some(1));
	assert!(missing.stderr.contains("does not name a module"), "{}", missing.stderr);
}

#[test]
fn finds_as_json_and_file_lines() {
	let json: Value =
		serde_json::from_str(&run(&fixture(), &["find", "crate::add", "--message-format", "json"]).success().stdout)
			.unwrap();
	let found = json.as_array().unwrap();

	assert_eq!(found.len(), 1, "{json}");
	assert_eq!(found[0]["path"], "demo::add");
	assert_eq!(found[0]["kind"], "fn");
	assert_eq!(found[0]["crate"], "demo");
	assert_eq!(found[0]["file"], native("src/lib.rs"));
	assert_eq!(found[0]["start"], json!({"line": 8, "column": 1}));
	assert_eq!(found[0]["end"], json!({"line": 11, "column": 2}));
	assert_eq!(found[0]["visibility"], "pub");
	assert_eq!(found[0]["active"], "true");

	let lines = run(&fixture(), &["find", "crate::add", "crate::twice", "--message-format", "file-lines"]).success();
	let lib = serde_json::to_string(&native("src/lib.rs")).unwrap();

	assert_eq!(lines.stdout, format!("[{{\"file\":{lib},\"range\":[8,11]}},{{\"file\":{lib},\"range\":[25,28]}}]\n"));

	let absolute = run(&fixture(), &["find", "crate::add", "--absolute-paths"]).success();
	let printed = absolute.stdout.strip_prefix("demo::add  fn  ").and_then(|rest| rest.strip_suffix(":8:1\n"));
	let printed = Path::new(printed.unwrap_or_else(|| panic!("{}", absolute.stdout)));

	// compared canonicalized, which on Windows gives verbatim paths (`\\?\C:\...`), not how they are printed
	assert!(printed.is_absolute(), "{}", absolute.stdout);
	assert_eq!(printed.canonicalize().unwrap(), fixture().join("src/lib.rs").canonicalize().unwrap());

	// relative to the current directory when below it
	let from_src = run(&fixture().join("src"), &["find", "crate::add"]).success();

	assert_eq!(from_src.stdout, "demo::add  fn  lib.rs:8:1\n");
}

#[test]
fn views_items() {
	let add = run(&fixture(), &["view", "crate::add"]).success();

	assert_eq!(
		add.stdout,
		format!(
			"// {}\n/// Adds two numbers.\npub fn add(left: i32, right: i32) -> i32 {{\n\tleft + right\n}}\n",
			native("demo::add (fn) src/lib.rs:8-11")
		)
	);

	let no_docs = run(&fixture(), &["view", "crate::add", "--no-docs"]).success();

	assert!(!no_docs.stdout.contains("Adds two numbers"), "{}", no_docs.stdout);
	assert!(no_docs.stdout.contains("pub fn add(left: i32, right: i32) -> i32 {"), "{}", no_docs.stdout);

	let numbered = run(&fixture(), &["view", "crate::twice", "-n"]).success();

	assert!(
		numbered.lines().iter().any(|line| line.trim_start().starts_with("26 │ pub fn twice(value: i32) -> i32 {")),
		"{}",
		numbered.stdout
	);

	let variants = run(&fixture(), &["view", "crate::extra"]).success();

	assert!(variants.stdout.contains("[cfg: feature = \"extra\"] [inactive]\n"), "{}", variants.stdout);
	assert!(variants.stdout.contains("\"plain\""), "{}", variants.stdout);

	let json: Value =
		serde_json::from_str(&run(&fixture(), &["view", "crate::add", "--message-format", "json"]).success().stdout)
			.unwrap();

	assert_eq!(json[0]["path"], "demo::add");
	assert_eq!(json[0]["file"], native("src/lib.rs"));
	assert!(json[0]["text"].as_str().unwrap().contains("left + right"), "{json}");

	let missing = run(&fixture(), &["view", "crate::missing"]);

	assert_eq!(missing.code, Some(1));
	assert_eq!(missing.stderr, "error: no item found for `crate::missing`\n");
}

#[test]
fn views_outlines_and_impls() {
	let shapes = run(&fixture(), &["view", "crate::shapes"]).success();

	assert!(
		shapes.stdout.starts_with(&format!("// {}\n", native("demo::shapes (mod) src/lib.rs:3-3"))),
		"{}",
		shapes.stdout
	);
	assert!(shapes.stdout.contains("pub fn new(radius: f64) -> Self { ... }"), "{}", shapes.stdout);
	assert!(!shapes.stdout.contains("Self { radius }"), "{}", shapes.stdout);

	let full = run(&fixture(), &["view", "crate::shapes", "--full"]).success();

	assert!(full.stdout.contains("Self { radius }"), "{}", full.stdout);

	let outline = run(&fixture(), &["view", "crate::shapes::Circle", "--impls"]).success();

	let impls = [
		native("impl demo::shapes::Circle (impl) src/shapes.rs:9-14"),
		native("impl Shape for demo::shapes::Circle (impl) src/shapes.rs:16-20"),
	];

	for header in impls {
		assert!(outline.stdout.contains(&format!("// {header}\n")), "{}", outline.stdout);
	}

	assert!(outline.stdout.contains("fn area(&self) -> f64 { ... }"), "{}", outline.stdout);
}

#[test]
fn checks_formatting() {
	let check = run(&fixture(), &["fmt", "--check", "crate::util::alpha"]);

	assert_eq!(check.code, Some(1), "{check:#?}");
	assert!(check.stdout.contains("-pub(crate)   fn   alpha( )->u8{2}"), "{}", check.stdout);
	assert!(check.stdout.contains("+pub(crate) fn alpha() -> u8 {"), "{}", check.stdout);

	let clean = run(&fixture(), &["fmt", "--check", "--no-sort", "crate::add", "crate::shapes"]);

	assert_eq!(clean.code, Some(0), "{clean:#?}");
	assert_eq!(clean.stdout, "");

	let json = run(&fixture(), &["fmt", "--check", "--emit", "json", "crate::util::alpha"]);

	assert_eq!(json.code, Some(1));

	let mismatches: Value = serde_json::from_str(&json.stdout).unwrap();

	assert_eq!(mismatches[0]["name"], native("src/util.rs"), "{mismatches}");

	let stdout = run(&fixture(), &["fmt", "--emit", "stdout", "crate::util::alpha"]).success();

	assert!(stdout.stdout.starts_with("pub(crate) fn zeta() -> u8 {\n"), "{}", stdout.stdout);
	assert!(stdout.stdout.contains("pub(crate) fn alpha() -> u8 {\n\t2\n}\n"), "{}", stdout.stdout);
	assert_eq!(read_fixture("src/util.rs").lines().nth(4), Some("pub(crate)   fn   alpha( )->u8{2}"));
}

#[test]
fn check_results_survive_a_closed_stdout() {
	// `cargo rscode fmt --check | head -n 3` under `set -o pipefail` must still fail when files would change
	assert_eq!(run_into_closed_pipe(&fixture(), &["fmt", "--check", "crate::util::alpha"]), Some(1));
	assert_eq!(run_into_closed_pipe(&fixture(), &["fmt", "--check", "--emit", "json", "crate::util::alpha"]), Some(1));
	assert_eq!(run_into_closed_pipe(&fixture(), &["sort", "--check", "crate::util"]), Some(1));
	assert_eq!(run_into_closed_pipe(&fixture(), &["fmt", "--check", "--no-sort", "crate::add"]), Some(0));

	// the rest of the output is not wanted: that is no failure
	assert_eq!(run_into_closed_pipe(&fixture(), &["find", "*"]), Some(0));
	assert_eq!(run_into_closed_pipe(&fixture(), &["view", "crate::shapes", "--full"]), Some(0));
}

#[test]
fn formats_files() {
	let copy = TempCopy::new("fmt");
	let formatted = run(copy.path(), &["fmt", "crate::util::alpha"]).success();

	assert_eq!(formatted.stdout, native("formatted src/util.rs\n"));
	assert_eq!(
		copy.read("src/util.rs"),
		"pub(crate) fn zeta() -> u8 {\n\t1\n}\n\npub(crate) fn alpha() -> u8 {\n\t2\n}\n\npub(crate) fn double(value: i32) -> i32 {\n\tvalue * 2\n}\n"
	);

	// nothing left to do
	assert_eq!(run(copy.path(), &["fmt", "crate::util::alpha"]).success().stdout, "");
	cargo_check(copy.path());
}

#[test]
fn sorts_items() {
	let copy = TempCopy::new("sort");

	assert_eq!(run(copy.path(), &["sort", "--check", "crate::util"]).code, Some(1));
	assert_eq!(run(copy.path(), &["sort", "crate::util"]).success().stdout, native("sorted src/util.rs\n"));

	let util = copy.read("src/util.rs");
	let position = |text: &str| util.find(text).unwrap_or_else(|| panic!("{text}: {util}"));

	// sorted, and not formatted
	assert!(position("fn   alpha") < position("fn double"), "{util}");
	assert!(position("fn double") < position("fn zeta"), "{util}");
	assert_eq!(run(copy.path(), &["sort", "--check", "crate::util"]).code, Some(0));
	cargo_check(copy.path());
}

#[test]
fn previews_renames() {
	let lib = read_fixture("src/lib.rs");
	let rename = run(&fixture(), &["rename", "crate::add", "sum", "--dry-run"]).success();

	assert!(rename.stdout.contains("-pub fn add(left: i32, right: i32) -> i32 {"), "{}", rename.stdout);
	assert!(rename.stdout.contains("+pub fn sum(left: i32, right: i32) -> i32 {"), "{}", rename.stdout);
	assert!(
		rename.stdout.contains("+\tlet circle = demo::Circle::new(f64::from(demo::sum(1, 2)));"),
		"{}",
		rename.stdout
	);
	assert!(rename.stderr.contains("would rename 1 item, update 1 reference in 2 files"), "{}", rename.stderr);
	assert_eq!(read_fixture("src/lib.rs"), lib);

	let json: Value = serde_json::from_str(
		&run(&fixture(), &["rename", "crate::add", "sum", "-n", "--message-format", "json"]).success().stdout,
	)
	.unwrap();

	assert_eq!(json["renamed"], json!(["demo::add"]));
	assert_eq!(json["dry_run"], true);
	assert!(json["diff"].as_str().unwrap().contains("+pub fn sum("), "{json}");

	let collision = run(&fixture(), &["rename", "crate::add", "twice"]);

	assert_eq!(collision.code, Some(1));
	assert!(collision.stderr.contains("collides"), "{}", collision.stderr);
	assert_eq!(read_fixture("src/lib.rs"), lib);
}

#[test]
fn diffs_are_relative_to_the_workspace_root() {
	let workspace = two_packages("diff-root");
	let rename = run(&workspace.path().join("b"), &["rename", "::a::foo", "foo2", "--dry-run"]).success();
	let mut headers: Vec<&str> =
		rename.stdout.lines().filter(|line| line.starts_with("--- ") || line.starts_with("+++ ")).collect();

	headers.sort();

	assert_eq!(headers, ["+++ b/b/src/lib.rs", "+++ b/src/lib.rs", "--- a/b/src/lib.rs", "--- a/src/lib.rs"]);

	// so `patch -p1` applies it in the workspace root
	if Command::new("patch").arg("--version").output().is_ok_and(|output| output.status.success()) {
		let mut patch = Command::new("patch");

		patch.args(["-p1", "--dry-run", "--batch"]).current_dir(workspace.path());

		let patched = finish(patch, Some(&rename.stdout));

		assert_eq!(patched.code, Some(0), "{patched:#?}");
	}
}

#[test]
fn errors_name_the_options_that_get_past_them() {
	let collision = run(&fixture(), &["rename", "crate::add", "twice", "--dry-run"]);

	assert_eq!(collision.code, Some(1));
	assert!(collision.stderr.contains("collides with existing names:\n"), "{}", collision.stderr);
	assert!(collision.stderr.ends_with("hint: pass `--force` to proceed anyway\n"), "{}", collision.stderr);

	let source = "pub fn extra() -> &'static str {\n\t\"x\"\n}\n";
	let variants = run_with_stdin(&fixture(), &["replace", "crate::extra", "--dry-run"], source);

	assert_eq!(variants.code, Some(1));
	assert!(variants.stderr.contains("hint: pass `--all-variants`"), "{}", variants.stderr);

	// but not to `impl` blocks whose headers differ: those are no `cfg` variants
	let impls = TempCopy::with_files(
		"impls",
		&[
			("Cargo.toml", "[package]\nname = \"impls\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
			("src/lib.rs", "pub struct W<T>(T);\n\nimpl W<u8> {\n\tfn get() {}\n}\n\nimpl W<u16> {\n\tfn get() {}\n}"),
		],
	);
	let args = ["replace", "crate::W::get", "--dry-run", "--all-variants"];
	let variants = run_with_stdin(impls.path(), &args, "fn get() {}\n");

	assert_eq!(variants.code, Some(1));
	assert!(
		variants.stderr.contains(&native("`<impls::W<u16>>::get` (assoc-fn) at src/lib.rs:8:2")),
		"{}",
		variants.stderr
	);
	assert!(!variants.stderr.contains("hint"), "{}", variants.stderr);

	// `crate` is the root of the library and of the binary
	let crates = run_with_stdin(&fixture(), &["insert", "crate", "--dry-run"], "pub fn q() {}\n");

	assert_eq!(crates.code, Some(1));
	assert!(crates.stderr.contains(&native("`demo` (lib crate root) at src/lib.rs:1:1")), "{}", crates.stderr);
	assert!(crates.stderr.contains(&native("`demo` (bin crate root) at src/main.rs:1:1")), "{}", crates.stderr);
	assert!(crates.stderr.contains("select one with `--lib` or `--bin NAME`"), "{}", crates.stderr);

	let lib = run_with_stdin(&fixture(), &["insert", "crate", "--dry-run", "--lib"], "pub fn q() {}\n").success();

	assert!(lib.stdout.contains("+++ b/src/lib.rs"), "{}", lib.stdout);

	// members that are not selected are not loaded
	let workspace = two_packages("hints");
	let member = run(&workspace.path().join("b"), &["view", "::a::foo"]);

	assert_eq!(member.code, Some(1));
	assert!(member.stderr.contains("`a` is a workspace member that is not selected"), "{}", member.stderr);
	assert!(member.stderr.contains("pass `-p a` or `--workspace`"), "{}", member.stderr);

	let found = run(workspace.path(), &["find", "bar"]).success();

	assert!(found.stderr.contains("the workspace member `b` is not selected"), "{}", found.stderr);
	assert!(run(workspace.path(), &["view", "::b::bar", "-p", "b"]).success().stdout.contains("a::foo();"));
}

#[test]
fn renames_across_crates() {
	let copy = TempCopy::new("rename");
	let renamed = run(copy.path(), &["rename", "crate::shapes::Circle", "Disc"]).success();

	assert!(renamed.stdout.starts_with("renamed 1 item, updated "), "{}", renamed.stdout);
	assert!(renamed.stdout.ends_with(" in 3 files\n"), "{}", renamed.stdout);

	let shapes = copy.read("src/shapes.rs");

	assert!(shapes.contains("pub struct Disc {"), "{shapes}");
	assert!(shapes.contains("impl Disc {"), "{shapes}");
	assert!(shapes.contains("impl Shape for Disc {"), "{shapes}");
	assert!(copy.read("src/lib.rs").contains("pub use shapes::Disc;"));
	assert!(copy.read("src/main.rs").contains("demo::Disc::new("));
	cargo_check(copy.path());
}

#[test]
fn removes_items() {
	let dry = run(&fixture(), &["remove", "crate::twice", "--dry-run"]).success();

	assert!(dry.stdout.contains("-pub fn twice(value: i32) -> i32 {"), "{}", dry.stdout);
	assert!(dry.stderr.contains(&native("warning: dangling reference at src/main.rs:7:")), "{}", dry.stderr);
	assert!(dry.stderr.contains(&native("would remove demo::twice (fn) src/lib.rs:25:1")), "{}", dry.stderr);

	let copy = TempCopy::new("remove");
	let removed = run(copy.path(), &["remove", "crate::util::zeta"]).success();

	assert_eq!(removed.stdout, native("removed demo::util::zeta (fn) src/util.rs:1:1\n"));

	let util = copy.read("src/util.rs");

	assert!(!util.contains("zeta"), "{util}");
	assert!(util.trim_start().starts_with("pub(crate)   fn   alpha"), "{util}");
	cargo_check(copy.path());
}

#[test]
fn replaces_items() {
	let source = "/// Adds.\npub fn add(left: i32, right: i32) -> i32 {\n\tright + left\n}\n";
	let dry = run_with_stdin(&fixture(), &["replace", "crate::add", "--dry-run"], source).success();

	assert!(dry.stdout.contains("+\tright + left"), "{}", dry.stdout);
	assert!(dry.stderr.contains("would replace demo::add"), "{}", dry.stderr);

	let copy = TempCopy::new("replace");

	assert_eq!(
		run_with_stdin(copy.path(), &["replace", "crate::add"], source).success().stdout,
		"replaced demo::add\n"
	);

	let lib = copy.read("src/lib.rs");

	assert!(lib.contains("/// Adds.\npub fn add(left: i32, right: i32) -> i32 {\n\tright + left\n}\n"), "{lib}");
	assert!(!lib.contains("Adds two numbers"), "{lib}");

	// from a file, formatted afterwards
	std::fs::write(copy.path().join("new.rs"), "pub fn twice(value:i32)->i32{util::double(value)}").unwrap();

	let formatted = run(copy.path(), &["replace", "crate::twice", "new.rs", "--fmt"]).success();

	assert_eq!(formatted.stdout, native("replaced demo::twice\nformatted src/lib.rs\n"));
	assert!(copy.read("src/lib.rs").ends_with("pub fn twice(value: i32) -> i32 {\n\tutil::double(value)\n}\n"));
	cargo_check(copy.path());

	let wrong_kind = run_with_stdin(copy.path(), &["replace", "crate::add"], "pub struct Add;");

	assert_eq!(wrong_kind.code, Some(1));
}

#[test]
fn inserts_items() {
	let copy = TempCopy::new("insert");
	let first =
		run_with_stdin(copy.path(), &["insert", "crate::util", "--position", "start"], "pub(crate) fn first() {}\n")
			.success();

	assert_eq!(first.stdout, native("inserted fn first into crate::util (src/util.rs)\n"));
	assert!(copy.read("src/util.rs").starts_with("pub(crate) fn first() {}\n\npub(crate) fn zeta()"));

	let method = "pub fn unit() -> Self {\n\tSelf::new(1.0)\n}\n";
	let args = ["insert", "<crate::shapes::Circle>", "--position", "after", "--anchor", "<crate::shapes::Circle>::new"];

	run_with_stdin(copy.path(), &args, method).success();
	assert!(copy.read("src/shapes.rs").contains("\tpub fn unit() -> Self {\n\t\tSelf::new(1.0)\n\t}\n}"));

	let last =
		run_with_stdin(copy.path(), &["insert", "crate::util", "--fmt"], "pub(crate) fn last()->u8{3}").success();

	assert_eq!(last.stdout, native("inserted fn last into crate::util (src/util.rs)\nformatted src/util.rs\n"));
	assert!(copy.read("src/util.rs").ends_with("pub(crate) fn last() -> u8 {\n\t3\n}\n"));
	cargo_check(copy.path());

	let taken = run_with_stdin(copy.path(), &["insert", "crate::util"], "pub(crate) fn first() {}\n");

	assert_eq!(taken.code, Some(1));
	assert!(taken.stderr.contains("collides"), "{}", taken.stderr);
}

#[test]
fn formats_edited_items_by_their_canonical_paths() {
	let copy = TempCopy::new("fmt-edited");

	// another `Circle`, whose `new` is badly formatted: `--fmt` of `demo::shapes::Circle`'s items must not touch it
	let other = "pub struct Circle;\n\nimpl Circle {\n\tpub fn new( )->Self{Circle}\n}\n";

	std::fs::write(copy.path().join("src/other.rs"), other).unwrap();
	std::fs::write(copy.path().join("src/lib.rs"), copy.read("src/lib.rs") + "\npub mod other;\n").unwrap();

	// `crate::Circle` is a re-export of `demo::shapes::Circle`
	let unit = "pub fn unit()->Self{Self::new(1.0)}";
	let inserted = run_with_stdin(copy.path(), &["insert", "<crate::Circle>", "--fmt"], unit).success();

	assert_eq!(
		inserted.stdout,
		native("inserted assoc-fn unit into <crate::Circle> (src/shapes.rs)\nformatted src/shapes.rs\n"),
		"{inserted:#?}"
	);
	assert_eq!(inserted.stderr, "");
	assert!(copy.read("src/shapes.rs").contains("\tpub fn unit() -> Self {\n\t\tSelf::new(1.0)\n\t}\n"));

	// `Circle` names `crate::Circle` (paths are presumed absolute), not every `Circle`
	let new = "pub fn new(radius:f64)->Self{Self{radius}}";
	let replaced = run_with_stdin(copy.path(), &["replace", "<Circle>::new", "--fmt"], new).success();

	assert_eq!(
		replaced.stdout,
		native("replaced demo::shapes::Circle::new\nformatted src/shapes.rs\n"),
		"{replaced:#?}"
	);
	assert!(copy.read("src/shapes.rs").contains("\tpub fn new(radius: f64) -> Self {\n\t\tSelf { radius }\n\t}\n"));
	assert_eq!(copy.read("src/other.rs"), other);

	// a replacement with another name cannot be formatted by the replaced item's path: a warning says so
	let args = ["replace", "crate::twice", "--allow-kind-change", "--fmt"];
	let renamed = run_with_stdin(copy.path(), &args, "pub const TWICE:i32=2;").success();

	assert_eq!(renamed.stdout, "replaced demo::twice\n", "{renamed:#?}");
	assert!(renamed.stderr.contains("warning: `demo::twice` is gone after the edit"), "{}", renamed.stderr);
	assert_eq!(copy.read("src/other.rs"), other);
}

#[test]
fn completes_item_paths() {
	let dir = fixture();

	assert_eq!(complete(&dir, &["cargo-rscode", "view", "crate::sh"]), ["crate::shapes"]);
	assert_eq!(complete(&dir, &["cargo-rscode", "view", "crate::shapes::Ci"]), ["crate::shapes::Circle"]);
	assert_eq!(complete(&dir, &["cargo-rscode", "view", "demo::shapes::Circle::n"]), ["demo::shapes::Circle::new"]);
	assert_eq!(
		complete(&dir, &["cargo-rscode", "view", "crate::shapes::Kind::"]),
		["crate::shapes::Kind::Round", "crate::shapes::Kind::Square"]
	);
	assert_eq!(complete(&dir, &["cargo-rscode", "find", "x", "--from", "crate::"]), ["crate::shapes", "crate::util"]);
	assert_eq!(complete(&dir, &["cargo", "rscode", "rename", "crate::tw"]), ["crate::twice"]);

	// below a re-export (`pub use shapes::Circle;`)
	assert_eq!(
		complete(&dir, &["cargo-rscode", "view", "crate::Circle::"]),
		["crate::Circle::area", "crate::Circle::new"]
	);

	// a typed `--manifest-path` is honored
	let manifest = fixture().join("Cargo.toml");
	let words = ["cargo-rscode", "view", "--manifest-path", manifest.to_str().unwrap(), "crate::a"];

	assert_eq!(complete(&std::env::temp_dir(), &words), ["crate::add"]);
}

/// `cargo rscode mcp`
#[cfg(feature = "mcp")]
mod mcp {
	use super::*;
	use std::io::BufRead as _;
	use std::io::BufReader;
	use std::sync::mpsc;
	use std::time::Duration;
	use std::time::Instant;

	#[test]
	fn help_explains_registration() {
		let mcp = run(&fixture(), &["mcp", "--help"]).success();

		assert!(mcp.stdout.contains("claude mcp add rscode -- cargo rscode mcp"), "{}", mcp.stdout);
		assert!(mcp.stdout.contains("\"mcpServers\""), "{}", mcp.stdout);
	}

	/// The names of the tools the server lists, after checking its handshake.
	fn tools(extra: &[&str]) -> BTreeSet<String> {
		let manifest = fixture().join("Cargo.toml");
		let mut child = Command::new(BIN)
			.arg("mcp")
			.arg("--manifest-path")
			.arg(&manifest)
			.args(extra)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::null())
			.spawn()
			.unwrap();
		let mut stdin = child.stdin.take().unwrap();
		let stdout = child.stdout.take().unwrap();
		let (sender, receiver) = mpsc::channel();

		std::thread::spawn(move || {
			for line in BufReader::new(stdout).lines() {
				if sender.send(line).is_err() {
					break;
				}
			}
		});

		let messages = [
			json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
				"protocolVersion": "2025-11-25",
				"capabilities": {},
				"clientInfo": {"name": "cargo-rscode-tests", "version": "0.0.0"},
			}}),
			json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
			json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
		];

		for message in messages {
			writeln!(stdin, "{message}").unwrap();
		}

		stdin.flush().unwrap();

		let mut tools = None;

		while tools.is_none() {
			let line = receiver.recv_timeout(Duration::from_secs(60)).expect("a response").unwrap();

			// nothing but the protocol on stdout
			let message: Value = serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {line}"));

			match message["id"].as_i64() {
				Some(1) => assert_eq!(message["result"]["serverInfo"]["name"], "rscode", "{message}"),
				Some(2) => tools = message["result"]["tools"].as_array().cloned(),
				_ => {}
			}
		}

		// the server ends when the client disconnects
		drop(stdin);

		let deadline = Instant::now() + Duration::from_secs(30);

		loop {
			if let Some(status) = child.try_wait().unwrap() {
				assert!(status.success(), "{status}");
				break;
			}

			assert!(Instant::now() < deadline, "the server did not exit");
			std::thread::sleep(Duration::from_millis(20));
		}

		tools.unwrap_or_default().iter().filter_map(|tool| tool["name"].as_str()).map(str::to_owned).collect()
	}

	#[test]
	fn serves_tools_over_stdio() {
		let all = tools(&[]);

		for tool in [
			"workspace_info",
			"find_items",
			"view_items",
			"rename_item",
			"remove_items",
			"replace_item",
			"insert_items",
			"format_items",
		] {
			assert!(all.contains(tool), "{tool}: {all:?}");
		}

		let read_only = tools(&["--read-only"]);

		assert!(read_only.contains("find_items") && read_only.contains("view_items"), "{read_only:?}");
		assert!(!read_only.contains("rename_item") && !read_only.contains("remove_items"), "{read_only:?}");
	}
}
