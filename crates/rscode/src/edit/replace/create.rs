//! Creating a module: its file, and its `mod` declaration in its parent, in one edit.

use super::Target;
use super::article;
use super::collisions;
use super::order;
use super::order::Siblings;
use super::parse::Container;
use super::parse::parse_source;
use super::target;
use crate::Error;
use crate::edit::EditSet;
use crate::edit::rename::NewName;
use crate::edit::rename::child_dir;
use crate::edit::trivia;
use crate::edit::trivia::Placement;
use crate::model::ItemKind;
use crate::model::Workspace;
use crate::path::ItemPath;
use crate::resolve::Resolver;
use crate::source::LineCol;
use serde::Deserialize;
use serde::Serialize;
use std::io;
use std::path::Path;
use std::path::PathBuf;

/// Options for [`create_module`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct CreateModuleOptions {
	/// The visibility of the module, as written (`pub`, `pub(crate)`, `pub(super)`, `pub(in path)`); empty for a
	/// private module.
	pub vis: String,
}

/// The planned module.
#[derive(Debug, Clone, Serialize)]
pub struct ModuleCreation {
	/// The edits, to preview or apply.
	#[serde(skip)]
	pub edits: EditSet,

	/// The file of the new module, which the edits create.
	pub file: PathBuf,

	/// The declaration of the module in its parent (`mod name;`, with its visibility).
	pub declaration: String,

	/// The file the declaration goes into.
	pub declared_in: PathBuf,

	/// The line of the declaration in that file once the edits are applied (1-based).
	pub line: usize,

	/// Things to know about the new module.
	pub warnings: Vec<String>,
}

/// Plans creating the module `name` in the module named by `parent` (`crate` for the crate root): a file for it with
/// `source` (inner attributes and `//!` docs allowed; it may be empty), and the declaration `mod name;` (with the
/// visibility of [`CreateModuleOptions::vis`]) in the parent, in one [`EditSet`].
///
/// The file goes where rustc looks for it: `name.rs` in the directory of the parent's child modules (as for an inline
/// parent, or one loaded with `#[path]`), or `name/mod.rs` when the parent's other modules are in `mod.rs` files.
/// The source is laid out with the indentation style and line breaks of the parent's file, and ends with a line break.
/// The declaration goes where `cargo rscode sort` would put it: among the parent's `mod` declarations, in order (on
/// consecutive lines, like theirs), or else where `mod` declarations go (after `extern crate` items, before `use`
/// items).
///
/// Fails with [`Error::InvalidIdent`] for a name that is not an identifier, [`Error::Unsupported`] for a name that is
/// not ASCII, [`Error::Collision`] when the parent binds the name already, [`Error::Io`] when a file of the module
/// exists already (`name.rs` or `name/mod.rs`), and [`Error::InvalidSource`] when the source does not parse as a file
/// or `vis` is not a visibility.
pub fn create_module(
	resolver: &Resolver<'_>,
	parent: &ItemPath,
	name: &str,
	source: &str,
	options: &CreateModuleOptions,
) -> Result<ModuleCreation, Error> {
	let ws = resolver.workspace();
	let name = NewName::parse(name.trim())?;

	// (rustc looks for the files of modules with other names only by `#[path]`)
	if !name.bare.is_ascii() {
		return Err(Error::Unsupported(format!(
			"`{}` is not an ASCII name, and rustc loads the file of a module only by an ASCII name: choose one",
			name.written
		)));
	}

	let declaration = declaration(&name, options.vis.trim())?;
	let target = target(resolver, parent, None)?;

	if target.container != Container::Module {
		let kind = ws.item(target.item).kind;

		return Err(Error::Unsupported(format!(
			"cannot create a module in `{parent}`: it is {}, and modules go into modules",
			article(kind)
		)));
	}

	let parsed = parse_source(&declaration, Container::Module)?;
	let collisions = collisions(resolver, &target, &parsed.items);

	if !collisions.is_empty() {
		return Err(Error::Collision {
			name: name.written,
			collisions: collisions.iter().map(|(name, existing)| format!("`{name}`: {existing}")).collect(),
		});
	}

	let file = module_file(ws, &target, parent, &name.bare)?;
	let text = target.file.text();
	let contents = file_contents(source, text)?;

	// where sorting puts it, laid out like sorting lays it out
	let style_edition = order::style_edition(ws, &target);
	let siblings = Siblings::of(ws, &target, style_edition);
	let new = order::new_orders(&declaration, Container::Module, style_edition);
	let placement = (new.as_deref())
		.and_then(|new| new.first())
		.and_then(|new| siblings.sorted_placement(new))
		.unwrap_or(Placement::End(target.body));
	let spacing = siblings.spacing(placement, new.as_deref());
	let indent = trivia::body_indent(text, target.body);
	let edit = trivia::insertion(text, placement, &declaration, &indent, spacing);
	let line = line_of(target.file.line_col(edit.range.start), &edit.replacement, &declaration);
	let mut edits = EditSet::new();

	edits.replace(target.file, edit.range, edit.replacement);
	edits.create_file(&file, contents);

	Ok(ModuleCreation {
		edits,
		file,
		declaration,
		declared_in: target.file.path().to_path_buf(),
		line,
		warnings: Vec::new(),
	})
}

/// The declaration of the module: `mod name;` with the visibility `vis` (empty for private).
fn declaration(name: &NewName, vis: &str) -> Result<String, Error> {
	let declaration = match vis {
		"" => format!("mod {};", name.written),
		vis => format!("{vis} mod {};", name.written),
	};

	let is_visibility = crate::source::isolated(|| {
		let parsed = syn::parse_str::<syn::ItemMod>(&declaration);

		parsed.is_ok_and(|item| item.attrs.is_empty() && item.unsafety.is_none() && item.content.is_none())
	});

	match is_visibility {
		true => Ok(declaration),
		false => Err(Error::InvalidSource(format!(
			"`{vis}` is not a visibility (such as `pub`, `pub(crate)`, or `pub(super)`)"
		))),
	}
}

/// The text of the new file: `source` laid out with the indentation style and line breaks of `parent_text` (the
/// parent's file), ending with a line break (empty for empty source). Fails when it does not parse as a file.
fn file_contents(source: &str, parent_text: &str) -> Result<String, Error> {
	let mut text = trivia::reindent(source, "", parent_text);

	if !text.is_empty() {
		text.push_str(trivia::line_ending(parent_text));
	}

	let parsed = crate::source::isolated(|| match syn::parse_file(&text) {
		Ok(_) => Ok(()),

		Err(error) => {
			let start = error.span().start();

			Err(format!("{}:{}: {error}", start.line, start.column + 1))
		}
	});

	match parsed {
		Ok(()) => Ok(text),
		Err(error) => Err(Error::InvalidSource(format!("the source does not parse as the file of a module at {error}"))),
	}
}

/// The 1-based line where `declaration` is in the text after an edit that starts at `start` and inserts `replacement`.
fn line_of(start: LineCol, replacement: &str, declaration: &str) -> usize {
	let before = replacement.find(declaration).map_or("", |index| &replacement[..index]);

	start.line + before.matches('\n').count()
}

/// The file of the new module `name` of `target`: `name.rs` in the directory of its child modules, or `name/mod.rs`
/// when its other out-of-line modules (declared without `#[path]`) are all in `mod.rs` files. Fails when either file
/// exists.
fn module_file(ws: &Workspace, target: &Target<'_>, parent: &ItemPath, name: &str) -> Result<PathBuf, Error> {
	let directory = child_dir(ws, target.item)
		.ok_or_else(|| Error::Unsupported(format!("cannot tell where the files of the modules of `{parent}` go")))?;

	let modules: Vec<&Path> = ws
		.children(target.item)
		.map(|child| ws.item(child))
		.filter(|data| data.kind == ItemKind::Module && data.attrs.path.is_none())
		.filter_map(|data| data.module_info().filter(|info| !info.inline)?.file_path.as_deref())
		.collect();
	let mod_rs = !modules.is_empty() && modules.iter().all(|file| file.file_name().is_some_and(|name| name == "mod.rs"));
	let candidates = [directory.join(format!("{name}.rs")), directory.join(name).join("mod.rs")];

	if let Some(existing) = candidates.iter().find(|file| std::fs::symlink_metadata(file).is_ok()) {
		let message = format!("the file of a module `{name}` exists already (it is not declared in `{parent}`)");

		return Err(Error::io(existing, io::Error::new(io::ErrorKind::AlreadyExists, message)));
	}

	let [file, mod_rs_file] = candidates;

	Ok(if mod_rs { mod_rs_file } else { file })
}
