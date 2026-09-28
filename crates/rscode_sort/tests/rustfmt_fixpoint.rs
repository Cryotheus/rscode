//! Sorting and rustfmt agree: rustfmt does not reorder what sorting ordered, and sorting after rustfmt changes nothing,
//! so `sort` then `rustfmt` settles in one pass.
//!
//! Skipped (with a message) when rustfmt is not installed. Set `RSCODE_SORT_FIXPOINT_DIR` to also check every `.rs`
//! file below a directory (such as copies of crates from `~/.cargo/registry/src`).

mod common;

use common::Rng;
use common::Rustfmt;
use std::path::Path;
use std::path::PathBuf;

/// `mod foo;` declarations and `extern crate` items whose Cryotheum order (by name, with version sorting and the
/// underscore rule) differs from rustfmt's (by bytes).
const DECLARATIONS: &[&str] = &[
	"mod v10;",
	"mod v2;",
	"mod v02;",
	"mod alpha;",
	"mod __private;",
	"mod _alpha;",
	"mod Beta;",
	"mod r#zz;",
	"mod s;",
	"mod a_b;",
	"mod ab;",
	"mod a1;",
	"mod x_8;",
	"mod é;",
	"pub mod z;",
	"/// Documented.\npub(crate) mod documented;",
];

const EXTERN_CRATES: &[&str] = &[
	"extern crate zeta as alpha_z;",
	"extern crate beta;",
	"extern crate beta as abc;",
	"extern crate beta as _;",
	"extern crate Zed;",
	"extern crate r#async;",
	"extern crate asynd;",
	"extern crate core2;",
	"extern crate core10;",
	"extern crate self as this;",
];

#[test]
fn declarations_are_ordered_like_rustfmt() {
	let Some(rustfmt) = Rustfmt::find() else {
		eprintln!("skipped: rustfmt is not installed");
		return;
	};
	let mut rng = Rng::new(0x0dec_1a2e);

	for round in 0..6 {
		let mut declarations = DECLARATIONS.to_vec();
		let mut crates = EXTERN_CRATES.to_vec();

		rng.shuffle(&mut declarations);
		rng.shuffle(&mut crates);

		let source = format!("{}\n{}\n", declarations.join("\n"), crates.join("\n"));
		let sorted = rscode_sort::sort_str(&source).unwrap();
		let formatted = rustfmt.format(&sorted).unwrap();

		assert_eq!(formatted, sorted, "round {round}: rustfmt reordered sorted items\n--- source:\n{source}");
	}
}

/// Inputs where rustfmt changes the layout in ways that sorting must not undo.
const LAYOUTS: &[&str] = &[
	// rustfmt wraps long items of compact groups
	"use zeta::Zeta;\nuse alpha::{AlphaOne, AlphaTwo, AlphaThree, AlphaFour, AlphaFive, AlphaSix, AlphaSeven, AlphaEight};\nuse beta::Beta;\n",
	"const Z: u8 = 0;\nconst A: [&str; 8] = [\"alpha\", \"beta\", \"gamma\", \"delta\", \"epsilon\", \"zeta\", \"eta\", \"theta\"];\nconst B: u8 = 0;\n",
	"static Z: u8 = 0;\ntype A = std::collections::HashMap<std::string::String, std::vec::Vec<std::collections::BTreeMap<u8, u8>>>;\nstatic B: u8 = 0;\n",
	// rustfmt puts attributes on their own lines
	"#[cfg(x)] const B: u8 = 0;\nconst A: u8 = 0;\n#[cfg(y)] use b;\nuse a;\n#[cfg(z)] mod c; #[cfg(z)] mod b;\n",
	// rustfmt splits single-line containers and items sharing a line
	"mod m { fn b() {} fn a() {} }\nimpl X { const B: u8 = 0; const A: u8 = 0; fn f() {} }\nstruct B; struct A;\n",
	"trait T { fn b(); fn a(); type X; }\nextern \"C\" { fn b(); static A: u8; fn a(); }\n",
	// rustfmt writes `extern "C"` for `extern`
	"extern { fn b(); }\nextern \"C\" { fn a(); }\nunsafe extern { fn d(); }\nextern \"C\" { #![allow(x)] fn c(); }\n",
	// macros
	"a!(); b!();\nfn z() {}\nfn y() {}\nmacro_rules! m { () => {} }\nuse b; use a;\nc! { x }\nd!(y);\n",
	// comments and section headers
	"// header\n\nuse b; // trailing\n// about a\nuse a;\n\n// section\n\nconst B: u8 = 0;\n/// Doc\nconst A: u8 = 0;\n",
	"mod m {\n\n    fn b() {}\n\n\n    fn a() {}\n\n}\n",
	"impl X {\n    // header\n\n    fn b(&self) {}\n    fn new() -> Self { X }\n    const A: u8 = 0;\n    type T = u8;\n}\n",
];

#[test]
fn sorting_after_rustfmt_changes_nothing() {
	let Some(rustfmt) = Rustfmt::find() else {
		eprintln!("skipped: rustfmt is not installed");
		return;
	};

	for source in LAYOUTS {
		check_fixpoint(&rustfmt, source).unwrap_or_else(|message| panic!("{message}\n--- source:\n{source}"));
	}
}

/// The sources of this workspace, and the files below `RSCODE_SORT_FIXPOINT_DIR`.
#[test]
fn real_files_settle() {
	let Some(rustfmt) = Rustfmt::find() else {
		eprintln!("skipped: rustfmt is not installed");
		return;
	};
	let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
	let mut files = Vec::new();

	collect_rust_files(&workspace.join("crates"), &mut files);

	if let Ok(directory) = std::env::var("RSCODE_SORT_FIXPOINT_DIR") {
		collect_rust_files(Path::new(&directory), &mut files);
	}

	files.sort();

	let mut checked = 0;
	let mut failures = Vec::new();

	for path in &files {
		let Ok(source) = std::fs::read_to_string(path) else {
			continue;
		};

		if syn::parse_file(&source).is_err() {
			continue;
		}

		match check_fixpoint(&rustfmt, &source) {
			Ok(true) => checked += 1,
			Ok(false) => {}
			Err(message) => failures.push(format!("{}: {message}", path.display())),
		}
	}

	eprintln!("checked {checked} of {} files", files.len());
	assert!(failures.is_empty(), "{} files do not settle:\n{}", failures.len(), failures.join("\n\n"));
	assert!(checked > 0);
}

/// Checks that sorting the output of `rustfmt(sort(source))` changes nothing. Returns `Ok(false)` when rustfmt fails.
fn check_fixpoint(rustfmt: &Rustfmt, source: &str) -> Result<bool, String> {
	let sorted = rscode_sort::sort_str(source).map_err(|error| error.to_string())?;
	let Ok(formatted) = rustfmt.format(&sorted) else {
		return Ok(false);
	};
	let again = rscode_sort::sort_str(&formatted).map_err(|error| error.to_string())?;

	if again != formatted {
		return Err(format!("{}\n--- sorted, then formatted:\n{formatted}", first_difference(&formatted, &again)));
	}

	Ok(true)
}

fn first_difference(expected: &str, actual: &str) -> String {
	let line = expected.lines().zip(actual.lines()).take_while(|(a, b)| a == b).count();
	let context = |text: &str| text.lines().skip(line.saturating_sub(3)).take(8).collect::<Vec<_>>().join("\n");

	format!(
		"sorting again changes line {}:\n--- formatted:\n{}\n--- sorted again:\n{}",
		line + 1,
		context(expected),
		context(actual)
	)
}

fn collect_rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
	let Ok(entries) = std::fs::read_dir(directory) else {
		return;
	};

	for entry in entries.flatten() {
		let path = entry.path();

		if path.is_dir() {
			if path.file_name().is_some_and(|name| name != "target") {
				collect_rust_files(&path, files);
			}
		} else if path.extension().is_some_and(|extension| extension == "rs") {
			files.push(path);
		}
	}
}
