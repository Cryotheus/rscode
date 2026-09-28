// rough outline of the design for this project
//
// it's a command-line tool for viewing, editing, searching, and formatting rust source files
//
// subcommands:
//  find
//      "searches for file path and syntactic path of a named item"
//      relative paths
//
//      the exact file path relative to cwd (confiurable, can be absolute)
//      (optional) shows what cfg feature is required for the item to be compiled
//          sometimes, an item may exist with the same publicly-exported path but in different source locations based on `cfg` attributes
//          will need to be clever in that case: maybe follow some kind of `--features` argument, and acknowledge the existence of the excluded item too?
//          ...
//          it would be nice if multiple items which are re-exported publicly through the same path but only one at a time are defined due to `cfg` attributes
//          would be targettable by other functions like `rename` in such a manner that all the different feature gated types are renamed too
//          because cases like that, the overlap is (probably) intentional
//          but, I am not sure about how `view` and `remove` should behaves
//      (optional) shows location info (many of which can be selected: line start, line end, column start, column end)
//          rustfmt uses `--file-lines JSON`
//      (optional) shows the item's true path to definition even if it's not visible through the path
//      (optional) shows the item's usable absolute path from the specified path e.g.
//          `--from crate` shows what path items inside the crate's root would need to use to refer to the item found
//          `--from ::` shows what path items inside a foreign crate's root would need to take to refer to the item found
//
//  fmt
//      "formats the contents of the specified items"
//      args are a list of item paths
//      items can be modules, even `::` or `crate`
//      items can have their contents sorted, if applciable
//          such as:
//              impl items in `impl` blocks
//              foreign items in `extern "C"`
//              items in inline-modules
//      supports glob patterns
//      also sorts items via the "Cryotheum" ordering scheme
//      rustfmt has `--skip-children`, so we should probably have something like that as well
//      rustfmt has `--emit [files|stdout|coverage|checkstyle|json]`
//          `coverage-json` might be nice
//
//  rename
//      rename an item to a new identifier
//      refuses to rename by default if it detects a collision
//      can be forced to do so anyways
//
//  remove
//      args are a list of item paths which are presumed absolute
//      removes
//
//  view
//      args are a list of item paths which are presumed absolute
//
//
//
// global options:
//  --package / -p
//      package selection for filtering subcommand operations
//
//  --workspace
//      operations process all packages in the workspace
//

use cargo::util::command_prelude::CommandExt;
use clap::Command;

fn main() -> anyhow::Result<()> {
	let command = Command::new("rscode")
		.version(env!("CARGO_PKG_VERSION"))
		.about("Summarized Rust source file viewing and editing.")
		.arg_package("package");

	let args = command.get_matches();

	Ok(())
}
