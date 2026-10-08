//! Applying [`EditSet`]s to files on disk.

use rscode::EditSet;
use rscode::Error;
use rscode::source::SourceFile;
use rscode::source::TextRange;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

/// A temporary directory, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
	fn new(name: &str) -> Self {
		static COUNTER: AtomicU32 = AtomicU32::new(0);

		let count = COUNTER.fetch_add(1, Ordering::Relaxed);
		let path = std::env::temp_dir().join(format!("rscode-edit-set-{}-{name}-{count}", std::process::id()));

		let _ = fs::remove_dir_all(&path);
		fs::create_dir_all(&path).unwrap();

		Self(path)
	}

	fn path(&self, relative: &str) -> PathBuf {
		self.0.join(relative)
	}

	/// Writes a file and returns it as loaded.
	fn file(&self, relative: &str, text: &str) -> SourceFile {
		let path = self.path(relative);

		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(&path, text).unwrap();

		SourceFile::new(path, text)
	}

	fn read(&self, relative: &str) -> String {
		fs::read_to_string(self.path(relative)).unwrap()
	}

	/// Every file below the directory, relative to it and sorted.
	fn listing(&self) -> Vec<String> {
		fn walk(directory: &Path, root: &Path, files: &mut Vec<String>) {
			for entry in fs::read_dir(directory).unwrap() {
				let path = entry.unwrap().path();

				if path.is_dir() {
					walk(&path, root, files);
				} else {
					files.push(path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
				}
			}
		}

		let mut files = Vec::new();

		walk(&self.0, &self.0, &mut files);
		files.sort();
		files
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.0);
	}
}

fn range(start: usize, end: usize) -> TextRange {
	TextRange::new(start, end)
}

#[test]
fn writes_changed_files() {
	let dir = TempDir::new("writes");
	let lib = dir.file("src/lib.rs", "mod a;\nfn f() {}\n");
	let a = dir.file("src/a.rs", "struct A;\n");
	let unchanged = dir.file("src/b.rs", "struct B;\n");
	let mut edits = EditSet::new();

	edits.replace(&lib, range(10, 11), "g");
	edits.replace(&a, range(10, 10), "\nstruct A2;\n");
	edits.replace(&unchanged, range(7, 8), "B");

	let applied = edits.apply().unwrap();

	assert_eq!(dir.read("src/lib.rs"), "mod a;\nfn g() {}\n");
	assert_eq!(dir.read("src/a.rs"), "struct A;\n\nstruct A2;\n");
	assert_eq!(dir.read("src/b.rs"), "struct B;\n");
	assert_eq!(applied.written, [dir.path("src/a.rs"), dir.path("src/lib.rs")]);
	assert!(applied.moved.is_empty());
	assert!(applied.deleted.is_empty());

	// no temporary files are left behind
	assert_eq!(dir.listing(), ["src/a.rs", "src/b.rs", "src/lib.rs"]);
}

#[test]
fn writes_nothing_when_an_edit_breaks_syntax() {
	let dir = TempDir::new("syntax");
	let good = dir.file("src/good.rs", "fn good() {}\n");
	let bad = dir.file("src/bad.rs", "fn bad() {}\n");
	let mut edits = EditSet::new();

	edits.replace(&good, range(3, 7), "better");
	edits.replace(&bad, range(10, 11), "");
	edits.delete_path(dir.path("src/good.rs"));

	match edits.apply() {
		Err(Error::EditBreaksSyntax { path, location, .. }) => {
			assert_eq!(path, dir.path("src/bad.rs"));
			assert_eq!(location.line, 1);
		}
		other => panic!("expected a syntax error, got {other:?}"),
	}

	assert_eq!(dir.read("src/good.rs"), "fn good() {}\n");
	assert_eq!(dir.read("src/bad.rs"), "fn bad() {}\n");
	assert_eq!(dir.listing(), ["src/bad.rs", "src/good.rs"]);
}

#[test]
fn writes_nothing_when_edits_overlap() {
	let dir = TempDir::new("overlap");
	let file = dir.file("src/lib.rs", "fn f() {}\n");
	let mut edits = EditSet::new();

	edits.replace(&file, range(3, 4), "g");
	edits.replace(&file, range(3, 4), "h");

	assert!(matches!(edits.apply(), Err(Error::OverlappingEdits { .. })));
	assert_eq!(dir.read("src/lib.rs"), "fn f() {}\n");
}

#[test]
fn refuses_to_overwrite_files_changed_since_loading() {
	let dir = TempDir::new("stale");
	let file = dir.file("src/lib.rs", "fn f() {}\n");
	let other = dir.file("src/other.rs", "fn o() {}\n");
	let mut edits = EditSet::new();

	edits.replace(&other, range(3, 4), "p");
	edits.replace(&file, range(3, 4), "g");
	fs::write(dir.path("src/lib.rs"), "fn f() {}\nfn added_by_someone_else() {}\n").unwrap();

	match edits.apply() {
		Err(Error::Io { path, source }) => {
			assert_eq!(path, dir.path("src/lib.rs"));
			assert!(source.to_string().contains("changed since it was loaded"), "{source}");
		}
		other => panic!("expected an I/O error, got {other:?}"),
	}

	assert_eq!(dir.read("src/lib.rs"), "fn f() {}\nfn added_by_someone_else() {}\n");
	assert_eq!(dir.read("src/other.rs"), "fn o() {}\n");
}

#[test]
fn moves_after_writing() {
	let dir = TempDir::new("moves");
	let lib = dir.file("src/lib.rs", "mod old;\n");
	let old = dir.file("src/old.rs", "mod child;\nstruct Old;\n");

	dir.file("src/old/child.rs", "struct Child;\n");

	let mut edits = EditSet::new();

	edits.replace(&lib, range(4, 7), "new");
	edits.replace(&old, range(18, 21), "New");
	edits.move_path(dir.path("src/old.rs"), dir.path("src/new.rs"));
	edits.move_path(dir.path("src/old"), dir.path("src/new"));
	edits.move_path(dir.path("src/new/child.rs"), dir.path("src/deeper/nested/child.rs"));

	let applied = edits.apply().unwrap();

	assert_eq!(dir.listing(), ["src/deeper/nested/child.rs", "src/lib.rs", "src/new.rs"]);
	assert_eq!(dir.read("src/lib.rs"), "mod new;\n");
	assert_eq!(dir.read("src/new.rs"), "mod child;\nstruct New;\n");
	assert_eq!(dir.read("src/deeper/nested/child.rs"), "struct Child;\n");
	assert_eq!(applied.moved.len(), 3);
	assert_eq!(applied.moved[0], (dir.path("src/old.rs"), dir.path("src/new.rs")));
}

#[test]
fn moves_in_a_cycle_through_a_temporary_name() {
	let dir = TempDir::new("swap");

	dir.file("a.rs", "struct A;\n");
	dir.file("b.rs", "struct B;\n");

	let mut edits = EditSet::new();

	edits.move_path(dir.path("a.rs"), dir.path("tmp.rs"));
	edits.move_path(dir.path("b.rs"), dir.path("a.rs"));
	edits.move_path(dir.path("tmp.rs"), dir.path("b.rs"));
	edits.apply().unwrap();

	assert_eq!(dir.read("a.rs"), "struct B;\n");
	assert_eq!(dir.read("b.rs"), "struct A;\n");
	assert_eq!(dir.listing(), ["a.rs", "b.rs"]);
}

#[test]
fn moves_to_names_in_another_case() {
	let dir = TempDir::new("case");
	let lib = dir.file("src/lib.rs", "mod Shapes;\n");

	dir.file("src/Shapes.rs", "mod Round;\n");
	dir.file("src/Shapes/Round.rs", "struct Circle;\n");

	let mut edits = EditSet::new();

	edits.replace(&lib, range(4, 10), "shapes");
	edits.move_path(dir.path("src/Shapes.rs"), dir.path("src/shapes.rs"));
	edits.move_path(dir.path("src/Shapes"), dir.path("src/shapes"));

	// (on file systems that ignore case, as on Windows and macOS, the new paths name the moved ones)
	let applied = edits.apply().unwrap();

	assert_eq!(dir.listing(), ["src/lib.rs", "src/shapes.rs", "src/shapes/Round.rs"]);
	assert_eq!(dir.read("src/lib.rs"), "mod shapes;\n");
	assert_eq!(applied.moved.len(), 2);
}

#[test]
fn undoes_everything_when_a_step_fails() {
	let dir = TempDir::new("undo");
	let lib = dir.file("src/lib.rs", "mod a;\nmod c;\nmod d;\n");
	let a = dir.file("src/a.rs", "struct A;\n");

	dir.file("src/c.rs", "struct C;\n");
	dir.file("src/d.rs", "struct D;\n");
	dir.file("blocker", "a file where a directory is needed\n");

	let mut edits = EditSet::new();

	edits.replace(&lib, range(4, 5), "b");
	edits.replace(&a, range(7, 8), "B");
	edits.move_path(dir.path("src/a.rs"), dir.path("src/b.rs"));
	edits.move_path(dir.path("src/d.rs"), dir.path("src/new/d.rs"));
	edits.move_path(dir.path("src/c.rs"), dir.path("blocker/c.rs"));

	// files are written, `src/a.rs` and `src/d.rs` moved (into the new directory `src/new`), and then moving
	// `src/c.rs` fails: all of it is undone
	let error = edits.apply().unwrap_err();

	assert!(
		matches!(&error, Error::Apply { path, kept, .. } if *path == dir.path("src/c.rs") && kept.is_empty()),
		"{error:?}"
	);
	assert!(error.to_string().ends_with("; nothing was changed"), "{error}");
	assert_eq!(dir.listing(), ["blocker", "src/a.rs", "src/c.rs", "src/d.rs", "src/lib.rs"]);
	assert_eq!(dir.read("src/lib.rs"), "mod a;\nmod c;\nmod d;\n");
	assert_eq!(dir.read("src/a.rs"), "struct A;\n");
	assert!(!dir.path("src/new").exists());
}

/// On Windows, a file that another process has open (without sharing its deletion) cannot be moved or deleted.
#[cfg(windows)]
#[test]
fn undoes_everything_when_a_file_is_in_use() {
	use std::os::windows::fs::OpenOptionsExt;

	let dir = TempDir::new("in-use");
	let lib = dir.file("src/lib.rs", "mod a;\nmod gone;\n");

	dir.file("src/a.rs", "struct A;\n");
	dir.file("src/gone.rs", "struct Gone;\n");

	let mut edits = EditSet::new();

	edits.replace(&lib, range(0, 17), "mod b;\n");
	edits.move_path(dir.path("src/a.rs"), dir.path("src/b.rs"));
	edits.delete_path(dir.path("src/gone.rs"));

	// opened without sharing anything
	let open = fs::OpenOptions::new().read(true).share_mode(0).open(dir.path("src/gone.rs")).unwrap();
	let error = edits.apply().unwrap_err();

	drop(open);

	assert!(
		matches!(&error, Error::Apply { path, kept, .. } if *path == dir.path("src/gone.rs") && kept.is_empty()),
		"{error:?}"
	);
	assert_eq!(dir.listing(), ["src/a.rs", "src/gone.rs", "src/lib.rs"]);
	assert_eq!(dir.read("src/lib.rs"), "mod a;\nmod gone;\n");

	// once it is closed
	edits.apply().unwrap();

	assert_eq!(dir.listing(), ["src/b.rs", "src/lib.rs"]);
}

#[test]
fn refuses_to_overwrite_with_a_move() {
	let dir = TempDir::new("overwrite");
	let lib = dir.file("src/lib.rs", "mod a;\n");

	dir.file("src/a.rs", "struct A;\n");
	dir.file("src/b.rs", "struct B;\n");

	let mut edits = EditSet::new();

	edits.replace(&lib, range(4, 5), "b");
	edits.move_path(dir.path("src/a.rs"), dir.path("src/b.rs"));

	match edits.apply() {
		Err(Error::Io { path, source }) => {
			assert_eq!(path, dir.path("src/b.rs"));
			assert_eq!(source.kind(), std::io::ErrorKind::AlreadyExists);
		}
		other => panic!("expected an I/O error, got {other:?}"),
	}

	// nothing was written
	assert_eq!(dir.read("src/lib.rs"), "mod a;\n");
	assert_eq!(dir.read("src/a.rs"), "struct A;\n");
	assert_eq!(dir.read("src/b.rs"), "struct B;\n");
}

#[test]
fn refuses_to_move_or_delete_missing_paths() {
	let dir = TempDir::new("missing");
	let lib = dir.file("src/lib.rs", "mod a;\n");
	let mut edits = EditSet::new();

	edits.replace(&lib, range(4, 5), "b");
	edits.move_path(dir.path("src/a.rs"), dir.path("src/b.rs"));

	match edits.apply() {
		Err(Error::Io { path, source }) => {
			assert_eq!(path, dir.path("src/a.rs"));
			assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
		}
		other => panic!("expected an I/O error, got {other:?}"),
	}

	let mut edits = EditSet::new();

	edits.replace(&lib, range(4, 5), "b");
	edits.delete_path(dir.path("src/a.rs"));
	assert!(matches!(edits.apply(), Err(Error::Io { .. })));

	// a path moved away cannot be deleted afterwards
	dir.file("src/c.rs", "struct C;\n");

	let mut edits = EditSet::new();

	edits.move_path(dir.path("src/c.rs"), dir.path("src/d.rs"));
	edits.delete_path(dir.path("src/c.rs"));
	assert!(matches!(edits.apply(), Err(Error::Io { .. })));

	assert_eq!(dir.read("src/lib.rs"), "mod a;\n");
	assert_eq!(dir.listing(), ["src/c.rs", "src/lib.rs"]);
}

#[test]
fn refuses_to_move_a_directory_into_itself() {
	let dir = TempDir::new("into-itself");

	dir.file("a/x.rs", "struct X;\n");

	let mut edits = EditSet::new();

	edits.move_path(dir.path("a"), dir.path("a/b"));

	match edits.apply() {
		Err(Error::Io { source, .. }) => assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput),
		other => panic!("expected an I/O error, got {other:?}"),
	}

	assert_eq!(dir.listing(), ["a/x.rs"]);
}

#[test]
fn deletes_files_and_directories() {
	let dir = TempDir::new("deletes");
	let lib = dir.file("src/lib.rs", "mod gone;\nmod kept;\n");

	dir.file("src/gone.rs", "mod inner;\n");
	dir.file("src/gone/inner.rs", "struct Inner;\n");
	dir.file("src/gone/inner/deep.rs", "struct Deep;\n");
	dir.file("src/kept.rs", "struct Kept;\n");

	let mut edits = EditSet::new();

	edits.replace(&lib, range(0, 10), "");
	edits.delete_path(dir.path("src/gone.rs"));
	edits.delete_path(dir.path("src/gone/inner.rs"));
	edits.delete_path(dir.path("src/gone"));

	let applied = edits.apply().unwrap();

	assert_eq!(dir.listing(), ["src/kept.rs", "src/lib.rs"]);
	assert_eq!(dir.read("src/lib.rs"), "mod kept;\n");
	assert_eq!(applied.deleted, [dir.path("src/gone.rs"), dir.path("src/gone")]);
}

#[test]
fn creates_files_and_their_directories() {
	let dir = TempDir::new("creates");
	let lib = dir.file("src/lib.rs", "mod a;\n");
	let mut edits = EditSet::new();

	edits.replace(&lib, range(6, 6), "\nmod b;\nmod c;");
	edits.create_file(dir.path("src/b/deep/mod.rs"), "struct B;\n");
	edits.create_file(dir.path("src/c.rs"), "");
	dir.file("src/a.rs", "struct A;\n");

	assert_eq!(edits.created().collect::<Vec<_>>(), [dir.path("src/b/deep/mod.rs"), dir.path("src/c.rs")]);
	assert!(edits.edited_files().any(|path| path == dir.path("src/c.rs")));

	let applied = edits.apply().unwrap();

	assert_eq!(dir.listing(), ["src/a.rs", "src/b/deep/mod.rs", "src/c.rs", "src/lib.rs"]);
	assert_eq!(dir.read("src/b/deep/mod.rs"), "struct B;\n");
	assert_eq!(dir.read("src/c.rs"), "");
	assert_eq!(dir.read("src/lib.rs"), "mod a;\nmod b;\nmod c;\n");
	assert_eq!(applied.written, [dir.path("src/lib.rs")]);
	assert_eq!(applied.created, [dir.path("src/b/deep/mod.rs"), dir.path("src/c.rs")]);

	// a file is created once, with one text
	let mut edits = EditSet::new();
	let mut again = EditSet::new();

	edits.create_file(dir.path("src/d.rs"), "struct D;\n");
	again.create_file(dir.path("src/d.rs"), "struct D;\n");
	edits.extend(again.clone());
	assert_eq!(edits.preview().unwrap().len(), 1);

	again.create_file(dir.path("src/d.rs"), "struct E;\n");
	edits.extend(again);
	assert!(matches!(edits.preview(), Err(Error::OverlappingEdits { .. })));
}

#[test]
fn refuses_to_create_what_exists() {
	let dir = TempDir::new("create-existing");
	let lib = dir.file("src/lib.rs", "mod a;\n");

	dir.file("src/a.rs", "struct A;\n");
	dir.file("src/b/x.rs", "struct X;\n");

	for existing in ["src/a.rs", "src/b"] {
		let mut edits = EditSet::new();

		edits.replace(&lib, range(4, 5), "b");
		edits.create_file(dir.path(existing), "struct New;\n");

		match edits.apply() {
			Err(Error::Io { path, source }) => {
				assert_eq!(path, dir.path(existing));
				assert_eq!(source.kind(), std::io::ErrorKind::AlreadyExists);
			}
			other => panic!("expected an I/O error, got {other:?}"),
		}
	}

	// nor does a move go where a file is created
	let mut edits = EditSet::new();

	edits.create_file(dir.path("src/c.rs"), "struct C;\n");
	edits.move_path(dir.path("src/a.rs"), dir.path("src/c.rs"));

	match edits.apply() {
		Err(Error::Io { path, source }) => {
			assert_eq!(path, dir.path("src/c.rs"));
			assert_eq!(source.kind(), std::io::ErrorKind::AlreadyExists);
		}
		other => panic!("expected an I/O error, got {other:?}"),
	}

	// a created file can move, or be deleted, afterwards
	let mut edits = EditSet::new();

	edits.create_file(dir.path("src/d.rs"), "struct D;\n");
	edits.move_path(dir.path("src/d.rs"), dir.path("src/e.rs"));
	edits.create_file(dir.path("src/f.rs"), "struct F;\n");
	edits.delete_path(dir.path("src/f.rs"));
	edits.apply().unwrap();

	assert_eq!(dir.read("src/lib.rs"), "mod a;\n");
	assert_eq!(dir.listing(), ["src/a.rs", "src/b/x.rs", "src/e.rs", "src/lib.rs"]);
}

#[test]
fn creates_nothing_when_a_created_file_does_not_parse() {
	let dir = TempDir::new("create-syntax");
	let lib = dir.file("src/lib.rs", "mod a;\n");
	let mut edits = EditSet::new();

	edits.replace(&lib, range(4, 5), "b");
	edits.create_file(dir.path("src/b.rs"), "fn b( {}\n");

	match edits.apply() {
		Err(Error::EditBreaksSyntax { path, location, .. }) => {
			assert_eq!(path, dir.path("src/b.rs"));
			assert_eq!(location.line, 1);
		}
		other => panic!("expected a syntax error, got {other:?}"),
	}

	assert_eq!(dir.listing(), ["src/lib.rs"]);
	assert_eq!(dir.read("src/lib.rs"), "mod a;\n");
}

#[test]
fn undoes_created_files_when_a_step_fails() {
	let dir = TempDir::new("create-undo");
	let lib = dir.file("src/lib.rs", "mod a;\n");

	dir.file("src/a.rs", "struct A;\n");
	dir.file("blocker", "a file where a directory is needed\n");

	let mut edits = EditSet::new();

	edits.replace(&lib, range(6, 6), "\nmod b;");
	edits.create_file(dir.path("src/b/c/mod.rs"), "struct B;\n");
	edits.move_path(dir.path("src/a.rs"), dir.path("blocker/a.rs"));

	// the file and its directories are created, and then the move fails: all of it is undone
	let error = edits.apply().unwrap_err();

	assert!(
		matches!(&error, Error::Apply { path, kept, .. } if *path == dir.path("src/a.rs") && kept.is_empty()),
		"{error:?}"
	);
	assert_eq!(dir.listing(), ["blocker", "src/a.rs", "src/lib.rs"]);
	assert!(!dir.path("src/b").exists());
	assert_eq!(dir.read("src/lib.rs"), "mod a;\n");
}

#[test]
fn applying_an_empty_edit_set_does_nothing() {
	let applied = EditSet::new().apply().unwrap();

	assert!(applied.written.is_empty() && applied.created.is_empty());
	assert!(applied.moved.is_empty() && applied.deleted.is_empty());
}

#[cfg(unix)]
#[test]
fn preserves_permissions() {
	use std::os::unix::fs::PermissionsExt;

	let dir = TempDir::new("permissions");
	let script = dir.file("build.rs", "fn main() {}\n");

	fs::set_permissions(dir.path("build.rs"), fs::Permissions::from_mode(0o751)).unwrap();

	let mut edits = EditSet::new();

	edits.replace(&script, range(12, 12), "\n// generated");
	edits.apply().unwrap();

	assert_eq!(dir.read("build.rs"), "fn main() {}\n// generated\n");
	assert_eq!(fs::metadata(dir.path("build.rs")).unwrap().permissions().mode() & 0o777, 0o751);
}

#[cfg(unix)]
#[test]
fn writes_through_symbolic_links() {
	let dir = TempDir::new("symlinks");

	dir.file("real/lib.rs", "fn f() {}\n");
	std::os::unix::fs::symlink(dir.path("real/lib.rs"), dir.path("link.rs")).unwrap();

	let linked = SourceFile::new(dir.path("link.rs"), "fn f() {}\n");
	let mut edits = EditSet::new();

	edits.replace(&linked, range(3, 4), "g");
	edits.apply().unwrap();

	assert!(fs::symlink_metadata(dir.path("link.rs")).unwrap().file_type().is_symlink());
	assert_eq!(dir.read("real/lib.rs"), "fn g() {}\n");
	assert_eq!(dir.listing(), ["link.rs", "real/lib.rs"]);
}

#[test]
fn diffs_edits_moves_and_deletions() {
	let dir = TempDir::new("diff");
	let lib = dir.file("src/lib.rs", "mod a;\n\nfn f() {}\n");
	let mut edits = EditSet::new();

	edits.replace(&lib, range(4, 5), "b");
	edits.move_path(dir.path("src/a.rs"), dir.path("src/b.rs"));
	edits.delete_path(dir.path("src/c.rs"));

	let diff = edits.diff_relative_to(&dir.0).unwrap();

	assert!(diff.starts_with("--- a/src/lib.rs\n+++ b/src/lib.rs\n"), "{diff}");
	assert!(diff.contains("-mod a;\n+mod b;\n"), "{diff}");
	assert!(diff.ends_with("rename src/a.rs -> src/b.rs\ndelete src/c.rs\n"), "{diff}");

	let absolute = edits.diff().unwrap();

	assert!(absolute.contains(&format!(
		"rename {} -> {}\n",
		dir.path("src/a.rs").display(),
		dir.path("src/b.rs").display()
	)));
}

#[test]
fn diffs_created_files_from_nothing() {
	let dir = TempDir::new("diff-created");
	let lib = dir.file("src/lib.rs", "mod a;\n");
	let mut edits = EditSet::new();

	edits.replace(&lib, range(6, 6), "mod b;\nmod c;\n");
	edits.create_file(dir.path("src/b.rs"), "struct B;\n\nstruct C;\n");
	edits.create_file(dir.path("src/c.rs"), "");

	let diff = edits.diff_relative_to(&dir.0).unwrap();

	assert!(diff.contains("--- a/src/lib.rs\n+++ b/src/lib.rs\n"), "{diff}");
	assert!(diff.contains("--- /dev/null\n+++ b/src/b.rs\n@@ -0,0 +1,3 @@\n+struct B;\n+\n+struct C;\n"), "{diff}");
	assert!(!diff.contains("c.rs\n@@"), "{diff}");
	assert!(diff.ends_with("create src/b.rs\ncreate src/c.rs\n"), "{diff}");
}

#[test]
fn diffs_only_moves() {
	let mut edits = EditSet::new();

	edits.move_path("/p/src/a.rs", "/p/src/b.rs");

	assert_eq!(edits.diff_relative_to(Path::new("/p")).unwrap(), "rename src/a.rs -> src/b.rs\n");
	assert_eq!(EditSet::new().diff().unwrap(), "");
}
