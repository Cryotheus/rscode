//! Rendering the difference between original and formatted sources.
//!
//! - [`unified_diff`]: a unified diff, like `rustfmt --check`.
//! - [`json`]: the output of `rustfmt --emit json`.
//! - [`checkstyle`]: the output of `rustfmt --emit checkstyle`.

mod lines;

use lines::Line;
use serde::Serialize;
use similar::TextDiff;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// A file whose contents changed (or would change) by formatting.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct FileChange {
	/// The path of the file, shown as given.
	pub path: PathBuf,

	/// The contents before formatting.
	pub original: String,

	/// The contents after formatting.
	pub formatted: String,
}

impl FileChange {
	/// Whether formatting changes the contents.
	pub fn is_changed(&self) -> bool {
		self.original != self.formatted
	}
}

/// A block of consecutive changed lines, as reported by `rustfmt --emit json`.
///
/// Lines are 1-based and split like [`str::lines`] does (a `\r` before a `\n` is not part of the line).
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct Mismatch {
	/// The line of the original text where the block starts.
	pub original_begin_line: usize,

	/// The last line of the block in the original text, or [`Mismatch::original_begin_line`] if the block only
	/// inserts lines.
	pub original_end_line: usize,

	/// The line of the formatted text where the block starts.
	pub expected_begin_line: usize,

	/// The last line of the block in the formatted text, or [`Mismatch::expected_begin_line`] if the block only
	/// removes lines.
	pub expected_end_line: usize,

	/// The original lines of the block, each followed by `\n`.
	pub original: String,

	/// The formatted lines of the block, each followed by `\n`.
	pub expected: String,
}

impl Mismatch {
	fn new(original_line: usize, expected_line: usize) -> Self {
		Self {
			original_begin_line: original_line,
			original_end_line: original_line,
			expected_begin_line: expected_line,
			expected_end_line: expected_line,
			original: String::new(),
			expected: String::new(),
		}
	}

	/// The formatted lines of the block, with their line numbers.
	fn expected_lines(&self) -> impl Iterator<Item = (usize, &str)> {
		(self.expected_begin_line..).zip(self.expected.split_terminator('\n'))
	}
}

/// The blocks of changed lines between an original and a formatted text, split and numbered exactly like rustfmt's
/// `json` and `checkstyle` output (a missing final line break counts as a changed, empty last line).
pub fn mismatches(original: &str, formatted: &str) -> Vec<Mismatch> {
	let mut mismatches = Vec::new();
	let mut current: Option<Mismatch> = None;
	let mut original_line = 1;
	let mut expected_line = 1;

	for line in lines::diff(original, formatted) {
		match line {
			Line::Both(_) => {
				mismatches.extend(current.take());
				original_line += 1;
				expected_line += 1;
			}
			Line::Original(text) => {
				let mismatch = current.get_or_insert_with(|| Mismatch::new(original_line, expected_line));

				mismatch.original_end_line = original_line;
				mismatch.original.push_str(text);
				mismatch.original.push('\n');
				original_line += 1;
			}
			Line::Formatted(text) => {
				let mismatch = current.get_or_insert_with(|| Mismatch::new(original_line, expected_line));

				mismatch.expected_end_line = expected_line;
				mismatch.expected.push_str(text);
				mismatch.expected.push('\n');
				expected_line += 1;
			}
		}
	}

	mismatches.extend(current);
	mismatches
}

/// A unified diff (`--- a/path`, `+++ b/path` headers) of every changed file, with `context` lines of context.
///
/// Relative paths are written with `/` on every platform, as `git apply` and `patch -p1` expect. Paths with a root or a
/// drive (absolute paths, and on Windows also `\path` and `C:path`) are written as they are, without a prefix
/// (`--- /path`, `+++ /path`).
pub fn unified_diff(changes: &[FileChange], context: usize) -> String {
	let mut diff = String::new();

	for change in changes.iter().filter(|change| change.is_changed()) {
		let (old, new) = match relative_with_slashes(&change.path) {
			Some(path) => (format!("a/{path}"), format!("b/{path}")),
			None => (change.path.display().to_string(), change.path.display().to_string()),
		};
		let text_diff = TextDiff::from_lines(change.original.as_str(), change.formatted.as_str());
		let unified = text_diff.unified_diff().context_radius(context).header(&old, &new).to_string();

		diff.push_str(&unified);
	}

	diff
}

/// A relative path (without a root or a drive) written with `/`.
fn relative_with_slashes(path: &Path) -> Option<String> {
	let mut components = Vec::new();

	for component in path.components() {
		match component {
			Component::Prefix(_) | Component::RootDir => return None,
			component => components.push(component.as_os_str().to_string_lossy()),
		}
	}

	Some(components.join("/"))
}

/// JSON in the format of `rustfmt --emit json`: an array with an entry for every file with [`mismatches`],
/// followed by a line break.
pub fn json(changes: &[FileChange]) -> String {
	#[derive(Serialize)]
	struct MismatchedFile {
		name: String,
		mismatches: Vec<Mismatch>,
	}

	let files: Vec<MismatchedFile> = changes
		.iter()
		.filter_map(|change| {
			let mismatches = mismatches(&change.original, &change.formatted);

			(!mismatches.is_empty()).then(|| MismatchedFile {
				name: change.path.display().to_string(),
				mismatches,
			})
		})
		.collect();

	let mut json = serde_json::to_string(&files).expect("strings and integers always serialize");

	json.push('\n');
	json
}

/// XML in the format of `rustfmt --emit checkstyle`: an element for every file (files without changes have no
/// errors), with an error for every formatted line of every [`Mismatch`], followed by a line break.
///
/// Unlike rustfmt, file names are escaped too, so the output is always well-formed.
pub fn checkstyle(changes: &[FileChange]) -> String {
	let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<checkstyle version=\"4.3\">");

	for change in changes {
		xml.push_str("<file name=\"");
		push_escaped(&mut xml, &change.path.display().to_string());
		xml.push_str("\">");

		for mismatch in mismatches(&change.original, &change.formatted) {
			for (line, text) in mismatch.expected_lines() {
				xml.push_str("<error line=\"");
				xml.push_str(&line.to_string());
				xml.push_str("\" severity=\"warning\" message=\"Should be `");
				push_escaped(&mut xml, text);
				xml.push_str("`\" />");
			}
		}

		xml.push_str("</file>");
	}

	xml.push_str("</checkstyle>\n");
	xml
}

/// Appends text with XML's special characters replaced by entities.
fn push_escaped(xml: &mut String, text: &str) {
	for character in text.chars() {
		match character {
			'<' => xml.push_str("&lt;"),
			'>' => xml.push_str("&gt;"),
			'"' => xml.push_str("&quot;"),
			'\'' => xml.push_str("&apos;"),
			'&' => xml.push_str("&amp;"),
			_ => xml.push(character),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn change(path: &str, original: &str, formatted: &str) -> FileChange {
		FileChange {
			path: PathBuf::from(path),
			original: original.to_owned(),
			formatted: formatted.to_owned(),
		}
	}

	/// The sample from rustfmt's own emitter tests, and a rustfmt run on it.
	const ORIGINAL: &str = "fn a( ){let x=1;}\n\n// keep\nfn b() {\n    let y = 2;\n}\nfn c( ){let z=3;\nlet w = 4;}\nstruct S{a:u8,b:u16}\n";
	const FORMATTED: &str = "fn a() {\n    let x = 1;\n}\n\n// keep\nfn b() {\n    let y = 2;\n}\nfn c() {\n    let z = 3;\n    let w = 4;\n}\nstruct S {\n    a: u8,\n    b: u16,\n}\n";

	#[test]
	fn mismatch_blocks() {
		let mismatches = mismatches(ORIGINAL, FORMATTED);

		assert_eq!(mismatches, [
			Mismatch {
				original_begin_line: 1,
				original_end_line: 1,
				expected_begin_line: 1,
				expected_end_line: 3,
				original: "fn a( ){let x=1;}\n".to_owned(),
				expected: "fn a() {\n    let x = 1;\n}\n".to_owned(),
			},
			Mismatch {
				original_begin_line: 7,
				original_end_line: 9,
				expected_begin_line: 9,
				expected_end_line: 16,
				original: "fn c( ){let z=3;\nlet w = 4;}\nstruct S{a:u8,b:u16}\n".to_owned(),
				expected: "fn c() {\n    let z = 3;\n    let w = 4;\n}\nstruct S {\n    a: u8,\n    b: u16,\n}\n".to_owned(),
			},
		]);
	}

	#[test]
	fn mismatch_edge_cases() {
		// only removed lines
		let removed = mismatches("fn a() {}\n\n\n\nfn b() {}\n", "fn a() {}\n\nfn b() {}\n");

		assert_eq!(removed, [Mismatch {
			original_begin_line: 3,
			original_end_line: 4,
			expected_begin_line: 3,
			expected_end_line: 3,
			original: "\n\n".to_owned(),
			expected: String::new(),
		}]);

		// a missing final line break
		let added = mismatches("fn a() {}", "fn a() {}\n");

		assert_eq!(added, [Mismatch {
			original_begin_line: 2,
			original_end_line: 2,
			expected_begin_line: 2,
			expected_end_line: 2,
			original: String::new(),
			expected: "\n".to_owned(),
		}]);

		// line breaks are not part of lines
		assert!(mismatches("fn a() {}\r\n", "fn a() {}\n").is_empty());
		assert!(mismatches("", "").is_empty());
		assert!(mismatches(ORIGINAL, ORIGINAL).is_empty());
	}

	#[test]
	fn json_output() {
		let changes = [change("src/lib.rs", ORIGINAL, FORMATTED), change("src/ok.rs", "fn ok() {}\n", "fn ok() {}\n")];
		let expected = concat!(
			r#"[{"name":"src/lib.rs","mismatches":["#,
			r#"{"original_begin_line":1,"original_end_line":1,"expected_begin_line":1,"expected_end_line":3,"original":"fn a( ){let x=1;}\n","expected":"fn a() {\n    let x = 1;\n}\n"},"#,
			r#"{"original_begin_line":7,"original_end_line":9,"expected_begin_line":9,"expected_end_line":16,"original":"fn c( ){let z=3;\nlet w = 4;}\nstruct S{a:u8,b:u16}\n","expected":"fn c() {\n    let z = 3;\n    let w = 4;\n}\nstruct S {\n    a: u8,\n    b: u16,\n}\n"}"#,
			"]}]\n",
		);

		assert_eq!(json(&changes), expected);
		assert_eq!(json(&changes[1..]), "[]\n");
		assert_eq!(json(&[]), "[]\n");
	}

	#[test]
	fn checkstyle_output() {
		let changes = [change("src/lib.rs", ORIGINAL, FORMATTED), change("src/ok.rs", "fn ok() {}\n", "fn ok() {}\n")];
		let expected = concat!(
			"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n",
			"<checkstyle version=\"4.3\">",
			"<file name=\"src/lib.rs\">",
			"<error line=\"1\" severity=\"warning\" message=\"Should be `fn a() {`\" />",
			"<error line=\"2\" severity=\"warning\" message=\"Should be `    let x = 1;`\" />",
			"<error line=\"3\" severity=\"warning\" message=\"Should be `}`\" />",
			"<error line=\"9\" severity=\"warning\" message=\"Should be `fn c() {`\" />",
			"<error line=\"10\" severity=\"warning\" message=\"Should be `    let z = 3;`\" />",
			"<error line=\"11\" severity=\"warning\" message=\"Should be `    let w = 4;`\" />",
			"<error line=\"12\" severity=\"warning\" message=\"Should be `}`\" />",
			"<error line=\"13\" severity=\"warning\" message=\"Should be `struct S {`\" />",
			"<error line=\"14\" severity=\"warning\" message=\"Should be `    a: u8,`\" />",
			"<error line=\"15\" severity=\"warning\" message=\"Should be `    b: u16,`\" />",
			"<error line=\"16\" severity=\"warning\" message=\"Should be `}`\" />",
			"</file>",
			"<file name=\"src/ok.rs\"></file>",
			"</checkstyle>\n",
		);

		assert_eq!(checkstyle(&changes), expected);
		assert_eq!(checkstyle(&[]), "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<checkstyle version=\"4.3\"></checkstyle>\n");
	}

	#[test]
	fn checkstyle_escapes() {
		let changes = [change("a&\"b\".rs", "fn a() { let s = \"<&>'\";}\n", "fn a() {\n    let s = \"<&>'\";\n}\n")];
		let xml = checkstyle(&changes);

		assert!(xml.contains("<file name=\"a&amp;&quot;b&quot;.rs\">"), "{xml}");
		assert!(xml.contains("message=\"Should be `    let s = &quot;&lt;&amp;&gt;&apos;&quot;;`\""), "{xml}");
	}

	#[test]
	fn checkstyle_ignores_removed_lines() {
		let changes = [change("a.rs", "fn a() {}\n\n\n\nfn b() {}\n", "fn a() {}\n\nfn b() {}\n")];

		assert_eq!(
			checkstyle(&changes),
			"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<checkstyle version=\"4.3\"><file name=\"a.rs\"></file></checkstyle>\n",
		);
	}

	#[test]
	fn unified_diff_output() {
		let changes = [
			change("src/a.rs", "fn a( ) {}\nfn b() {}\n", "fn a() {}\nfn b() {}\n"),
			change("src/unchanged.rs", "fn c() {}\n", "fn c() {}\n"),
			change("src/b.rs", "fn d() {}", "fn d() {}\n"),
		];

		assert_eq!(
			unified_diff(&changes, 3),
			concat!(
				"--- a/src/a.rs\n",
				"+++ b/src/a.rs\n",
				"@@ -1,2 +1,2 @@\n",
				"-fn a( ) {}\n",
				"+fn a() {}\n",
				" fn b() {}\n",
				"--- a/src/b.rs\n",
				"+++ b/src/b.rs\n",
				"@@ -1 +1 @@\n",
				"-fn d() {}\n",
				"\\ No newline at end of file\n",
				"+fn d() {}\n",
			),
		);
		assert_eq!(unified_diff(&changes[1..2], 3), "");
	}

	#[test]
	fn unified_diff_context() {
		let original: String = (1..=20).map(|line| format!("line {line}\n")).collect();
		let formatted = original.replace("line 10\n", "line ten\n");
		let changes = [change("f.rs", &original, &formatted)];

		assert_eq!(
			unified_diff(&changes, 1),
			"--- a/f.rs\n+++ b/f.rs\n@@ -9,3 +9,3 @@\n line 9\n-line 10\n+line ten\n line 11\n",
		);
		assert_eq!(unified_diff(&changes, 0), "--- a/f.rs\n+++ b/f.rs\n@@ -10 +10 @@\n-line 10\n+line ten\n");
	}

	#[test]
	fn unified_diff_of_absolute_paths() {
		let path = std::env::temp_dir().join("f.rs");
		let changes = [FileChange { path: path.clone(), original: "a\n".to_owned(), formatted: "b\n".to_owned() }];
		let path = path.display();

		assert_eq!(unified_diff(&changes, 0), format!("--- {path}\n+++ {path}\n@@ -1 +1 @@\n-a\n+b\n"));

		// on Windows, a path with a root but no drive is not absolute, but it is not relative either
		let path = PathBuf::from("/x.rs");
		let changes = [FileChange { path, original: "a\n".to_owned(), formatted: "b\n".to_owned() }];

		assert_eq!(unified_diff(&changes, 0), "--- /x.rs\n+++ /x.rs\n@@ -1 +1 @@\n-a\n+b\n");
	}

	#[test]
	fn unified_diff_of_native_paths() {
		// `src\a.rs` on Windows
		let path = Path::new("src").join("a.rs");
		let changes = [FileChange { path, original: "a\n".to_owned(), formatted: "b\n".to_owned() }];

		assert_eq!(unified_diff(&changes, 0), "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-a\n+b\n");
	}

	#[test]
	fn file_change() {
		assert!(change("a.rs", "a", "b").is_changed());
		assert!(!change("a.rs", "a", "a").is_changed());
	}
}
