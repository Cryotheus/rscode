//! Sorts real crates in place, to check afterwards that they still compile.
//!
//! ```text
//! RSCODE_SORT_DIR=/path/to/copied/crates cargo +nightly test -p rscode_sort --test real_world -- --ignored
//! ```

mod common;

use std::path::Path;
use std::path::PathBuf;

#[test]
#[ignore = "rewrites the `.rs` files below `RSCODE_SORT_DIR` in place"]
fn sort_directory_in_place() {
	let Ok(directory) = std::env::var("RSCODE_SORT_DIR") else {
		panic!("set RSCODE_SORT_DIR to a directory of copied crates");
	};
	let mut files = Vec::new();

	collect_rust_files(Path::new(&directory), &mut files);
	files.sort();

	let mut changed = 0;

	for path in &files {
		let source = std::fs::read_to_string(path).unwrap();
		let output = match rscode_sort::sort_str(&source) {
			Ok(output) => output,
			Err(rscode_sort::SortError::Parse { .. }) if syn::parse_file(&source).is_err() => {
				eprintln!("skipped (does not parse): {}", path.display());
				continue;
			}
			Err(error) => panic!("{}: {error}", path.display()),
		};

		common::assert_stable(&output);

		if output != source {
			std::fs::write(path, &output).unwrap();
			changed += 1;
		}
	}

	eprintln!("sorted {changed} of {} files", files.len());
}

fn collect_rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
	for entry in std::fs::read_dir(directory).unwrap() {
		let path = entry.unwrap().path();

		if path.is_dir() {
			if path.file_name().is_some_and(|name| name != "target") {
				collect_rust_files(&path, files);
			}
		} else if path.extension().is_some_and(|extension| extension == "rs") {
			files.push(path);
		}
	}
}
