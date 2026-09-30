//! `--fmt` of `replace` and `insert`: formatting the new items with rustfmt once they are written.
//!
//! The items are addressed by their canonical paths, anchored at their crates (`::demo::shapes::Circle`,
//! `<::demo::shapes::Circle>::new`, `<::demo::shapes::Circle as Shape>::area`, with the generic arguments of the
//! `impl` blocks' types and traits), which are taken from the items the edit resolved, before it is applied. The path
//! as typed would not do: formatting matches qualified paths against canonical paths, so `<crate::Circle>::new`
//! (`Circle` being a re-export) would match nothing, and `<Circle>::new` the `new` of every `Circle`.

use crate::render::PathDisplay;
use rscode::CanonicalPath;
use rscode::Find;
use rscode::ItemId;
use rscode::ItemKind;
use rscode::ItemPath;
use rscode::LoadOptions;
use rscode::PathPattern;
use rscode::Resolver;
use rscode::Workspace;
use rscode::edit::FmtOptions;
use rscode::path::Anchor;
use rscode::path::Qualifier;
use rscode::path::last_segment_arguments;
use rscode::path::normalize_arguments;
use rscode::rscode_fmt::FormatOptions;
use rscode::rscode_fmt::RsFormatter;
use std::path::Path;
use std::path::PathBuf;

/// What formatting after an edit did.
#[derive(Debug, Default)]
struct Formatted {
	/// Written files.
	files: Vec<PathBuf>,

	/// Why items were not formatted, and the formatter's warnings.
	warnings: Vec<String>,
}

/// The items to format after an edit.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub(super) struct Targets {
	/// Paths naming exactly the items, with their canonical paths.
	paths: Vec<(ItemPath, CanonicalPath)>,

	/// Items without a path (of `impl`s whose type is not a loaded item).
	unnamable: Vec<CanonicalPath>,

	warnings: Vec<String>,
}

impl Targets {
	/// Adds the item with this canonical path. `in_impl`: the item is an associated item of an `impl` block.
	fn add(&mut self, path: CanonicalPath, in_impl: bool) {
		match item_path(&path, in_impl) {
			Some(item_path) => {
				if !self.paths.iter().any(|(known, _)| *known == item_path) {
					self.paths.push((item_path, path));
				}
			}

			None => {
				if !self.unnamable.contains(&path) {
					self.unnamable.push(path);
				}
			}
		}
	}
}

/// The canonical path of the item `name` in a container (a module, trait, or `impl` block), and whether it is an
/// associated item of an `impl` block.
fn child_path(container: &CanonicalPath, name: &str) -> (CanonicalPath, bool) {
	if container.is_impl {
		let child = CanonicalPath {
			is_impl: false,
			is_import: false,
			name: Some(name.into()),
			..container.clone()
		};

		return (child, true);
	}

	let child = CanonicalPath {
		segments: container.segments.iter().chain(&container.name).cloned().collect(),
		impl_trait: None,
		self_ty_arguments: None,
		unresolved_self_ty: None,
		is_impl: false,
		is_import: false,
		name: Some(name.into()),
	};

	(child, false)
}

/// The file that holds a container's items: an out-of-line module's own file, else the container's file.
fn children_file(workspace: &Workspace, container: ItemId) -> &Path {
	match workspace.item(container).module_info().and_then(|info| info.file) {
		Some(file) => workspace.krate(container.krate()).file(file).path(),
		None => workspace.file_of(container).path(),
	}
}

/// Formats the targets once the edit is written (see [`try_format`]). Returns the formatted files (for display) and
/// the warnings, among them a failure to format: the edit is done, and failing to format it is no failure to edit.
pub(super) fn format(options: &LoadOptions, targets: Targets, paths: &PathDisplay) -> (Vec<String>, Vec<String>) {
	match try_format(options, targets) {
		Ok(formatted) => (formatted.files.iter().map(|file| paths.display(file)).collect(), formatted.warnings),
		Err(error) => (Vec::new(), vec![format!("the edit is written, but formatting failed: {error:#}")]),
	}
}

/// The items among `items` whose file (per `file_of`) is one of `files`, or all of them when none is (the edit may
/// name its files differently).
fn in_files<'ws>(workspace: &'ws Workspace, items: Vec<ItemId>, files: &[PathBuf], file_of: impl Fn(ItemId) -> &'ws Path) -> Vec<ItemId> {
	let root = workspace.root();
	let files: Vec<PathBuf> = files.iter().map(|file| root.join(file)).collect();
	let chosen: Vec<ItemId> = items.iter().copied().filter(|&item| files.contains(&root.join(file_of(item)))).collect();

	if chosen.is_empty() { items } else { chosen }
}

/// The named items among `inserted`, in the container `parent` resolves to (the one in `file`).
pub(super) fn inserted(
	resolver: &Resolver<'_>,
	parent: &ItemPath,
	file: &Path,
	inserted: &[(ItemKind, Option<String>)],
	imports: &[(usize, String)],
) -> Targets {
	let workspace = resolver.workspace();
	let containers = in_files(workspace, resolver.resolve_item_path(parent), &[file.to_path_buf()], |container| {
		children_file(workspace, container)
	});
	let names: Vec<&str> = inserted.iter().filter_map(|(_, name)| name.as_deref()).collect();
	let mut targets = Targets::default();

	for container in containers {
		let container = resolver.canonical_path(container);

		for name in &names {
			let (path, in_impl) = child_path(&container, name);

			targets.add(path, in_impl);
		}

		// `use` items, by their imports
		for (_, name) in imports.iter().filter(|_| !container.is_impl) {
			let (path, _) = child_path(&container, name);

			targets.add(CanonicalPath { is_import: true, ..path }, false);
		}
	}

	let unnamed = (inserted.iter().enumerate())
		.filter(|(index, (_, name))| name.is_none() && !imports.iter().any(|(import_of, _)| import_of == index))
		.count();

	if unnamed > 0 {
		targets
			.warnings
			.push("unnamed items (like `impl` blocks) are not formatted by `--fmt`".to_owned());
	}

	targets
}

/// A path naming exactly the item with this canonical path, anchored at its crate: `::krate::m::Item`,
/// `<::krate::m::Type>::item` or `<::krate::m::Type as Trait>::item` for associated items of `impl`s (`in_impl`, as
/// their canonical paths look like those of trait items), and `<::krate::m::Type as Trait>` for `impl` blocks, with
/// the generic arguments of the type and trait, and `use ::krate::m::Name` for imports (a plain path would name what
/// they import).
/// `None` for `impl`s whose type is not a loaded item, and traits whose name cannot be told.
fn item_path(path: &CanonicalPath, in_impl: bool) -> Option<ItemPath> {
	if path.unresolved_self_ty.is_some() {
		return None;
	}

	if path.is_import {
		return Some(ItemPath {
			anchor: Anchor::Global,
			segments: path.segments.iter().chain(&path.name).cloned().collect(),
			import: true,
			..ItemPath::default()
		});
	}

	if !(path.is_impl || in_impl) {
		return Some(ItemPath {
			anchor: Anchor::Global,
			segments: path.segments.iter().chain(&path.name).cloned().collect(),
			..ItemPath::default()
		});
	}

	// the trait's name and arguments: they are matched as written in the `impl`, which may be any path to it
	let trait_path = match &path.impl_trait {
		Some(written) => Some(Box::new(ItemPath {
			arguments: last_segment_arguments(written),
			..ItemPath::from_segments([trait_name(written)?])
		})),

		None => None,
	};
	let self_ty = ItemPath {
		anchor: Anchor::Global,
		segments: path.segments.clone(),
		arguments: path.self_ty_arguments.as_deref().map(normalize_arguments),
		..ItemPath::default()
	};

	Some(ItemPath {
		qualifier: Some(Qualifier {
			self_ty: Box::new(self_ty),
			trait_path,
		}),
		segments: path.name.iter().cloned().collect(),
		..ItemPath::default()
	})
}

/// The items a replacement replaces: what `path` resolves to (in the replaced `files`).
pub(super) fn replaced(resolver: &Resolver<'_>, path: &ItemPath, files: &[PathBuf]) -> Targets {
	let workspace = resolver.workspace();
	let items = in_files(workspace, resolver.resolve_item_path(path), files, |item| workspace.file_of(item).path());
	let mut targets = Targets::default();

	for item in items {
		let in_impl = workspace.parent(item).is_some_and(|parent| workspace.item(parent).kind == ItemKind::Impl);

		targets.add(resolver.canonical_path(item), in_impl);
	}

	targets
}

/// The unraw'd name of a trait as written in an `impl` (`fmt::Display`, `From<u8>`, `!Send`).
fn trait_name(written: &str) -> Option<&str> {
	let path = written.trim().trim_start_matches(['!', '?']);
	let path = path.split(['<', '(']).next().unwrap_or(path);
	let name = path.rsplit("::").next().unwrap_or(path).trim();
	let name = name.strip_prefix("r#").unwrap_or(name);
	let is_ident =
		name.starts_with(|char: char| char.is_alphabetic() || char == '_') && name.chars().all(|char| char.is_alphanumeric() || char == '_');

	is_ident.then_some(name)
}

/// Loads the workspace again (to see the edit) and formats the targets with rustfmt, without sorting. Items that are
/// gone (a replacement may rename its item) or cannot be named are left alone, with a warning.
fn try_format(options: &LoadOptions, targets: Targets) -> anyhow::Result<Formatted> {
	let mut formatted = Formatted {
		files: Vec::new(),
		warnings: targets.warnings,
	};

	for path in &targets.unnamable {
		formatted
			.warnings
			.push(format!("`{path}` has no path to be formatted by, so `--fmt` leaves it"));
	}

	if targets.paths.is_empty() {
		return Ok(formatted);
	}

	let workspace = rscode::load_workspace(options)?;
	let resolver = Resolver::new(&workspace);
	let mut patterns = Vec::new();

	for (path, canonical) in &targets.paths {
		let pattern = PathPattern::exact(path);

		if Find::new().path_pattern(pattern.clone()).run_with(&resolver)?.is_empty() {
			formatted
				.warnings
				.push(format!("`{canonical}` is gone after the edit (renamed?), so `--fmt` leaves it"));
		} else {
			patterns.push(pattern);
		}
	}

	if patterns.is_empty() {
		return Ok(formatted);
	}

	let options = FmtOptions {
		format: FormatOptions::new().formatter(RsFormatter::RustFmt).sort(None),
		skip_children: true,
		active_only: false,
	};
	let formatting = rscode::edit::format(&resolver, &patterns, &options)?;

	formatted.warnings.extend(formatting.warnings);
	formatted.files = formatting.edits.apply()?.written;
	Ok(formatted)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn cannot_name_items_of_impls_for_foreign_types() {
		let method = CanonicalPath {
			unresolved_self_ty: Some("Vec<u8>".to_owned()),
			..canonical(&["demo", "m"], Some("len"))
		};

		assert_eq!(item_path(&method, true), None);
		assert_eq!(
			item_path(&trait_impl("Tr<'a, T>(x)"), false),
			Some(qualified(&["demo", "shapes", "Circle"], Some("Tr"), &[]))
		);
		assert_eq!(item_path(&trait_impl("<>"), false), None);
	}

	fn canonical(segments: &[&str], name: Option<&str>) -> CanonicalPath {
		CanonicalPath {
			segments: segments.iter().map(|&segment| segment.into()).collect(),
			impl_trait: None,
			self_ty_arguments: None,
			unresolved_self_ty: None,
			is_impl: false,
			is_import: false,
			name: name.map(Into::into),
		}
	}

	/// `demo::shapes::Circle`, `impl demo::shapes::Circle`, and `impl Shape for demo::shapes::Circle`.
	fn circle() -> CanonicalPath {
		canonical(&["demo", "shapes"], Some("Circle"))
	}

	#[test]
	fn collects_each_target_once() {
		let mut targets = Targets::default();
		let foreign = CanonicalPath {
			unresolved_self_ty: Some("Vec<u8>".to_owned()),
			..canonical(&["demo"], Some("len"))
		};

		// `cfg` variants share their path
		targets.add(circle(), false);
		targets.add(circle(), false);
		targets.add(foreign.clone(), true);
		targets.add(foreign.clone(), true);

		assert_eq!(targets.paths, [(global(&["demo", "shapes", "Circle"]), circle())]);
		assert_eq!(targets.unnamable, [foreign]);
	}

	fn global(segments: &[&str]) -> ItemPath {
		ItemPath {
			anchor: Anchor::Global,
			..ItemPath::from_segments(segments.iter().copied())
		}
	}

	fn inherent_impl() -> CanonicalPath {
		CanonicalPath {
			is_impl: true,
			is_import: false,
			..canonical(&["demo", "shapes", "Circle"], None)
		}
	}

	#[test]
	fn names_inserted_items() {
		// into a module (or trait)
		let (path, in_impl) = child_path(&canonical(&["demo"], Some("util")), "last");

		assert!(!in_impl);
		assert_eq!(item_path(&path, in_impl), Some(global(&["demo", "util", "last"])));

		// into the crate root
		let (path, in_impl) = child_path(&canonical(&["demo"], None), "first");

		assert_eq!(item_path(&path, in_impl), Some(global(&["demo", "first"])));

		// into `impl` blocks
		let (path, in_impl) = child_path(&inherent_impl(), "unit");

		assert!(in_impl);
		assert_eq!(path, canonical(&["demo", "shapes", "Circle"], Some("unit")));
		assert_eq!(item_path(&path, in_impl), Some(qualified(&["demo", "shapes", "Circle"], None, &["unit"])));

		let (path, in_impl) = child_path(&trait_impl("Shape"), "perimeter");

		assert_eq!(
			item_path(&path, in_impl),
			Some(qualified(&["demo", "shapes", "Circle"], Some("Shape"), &["perimeter"]))
		);
	}

	#[test]
	fn names_items_by_their_crate() {
		assert_eq!(item_path(&circle(), false), Some(global(&["demo", "shapes", "Circle"])));
		assert_eq!(item_path(&canonical(&["demo"], None), false), Some(global(&["demo"])));

		// trait items and variants: `demo::shapes::Shape::area`
		assert_eq!(
			item_path(&canonical(&["demo", "shapes", "Shape"], Some("area")), false),
			Some(global(&["demo", "shapes", "Shape", "area"]))
		);
	}

	#[test]
	fn names_items_of_impls_by_their_types() {
		// `<crate::Circle>::new` is `demo::shapes::Circle::new` through the re-export `crate::Circle`
		let new = canonical(&["demo", "shapes", "Circle"], Some("new"));

		assert_eq!(item_path(&new, true), Some(qualified(&["demo", "shapes", "Circle"], None, &["new"])));

		let area = CanonicalPath {
			name: Some("area".into()),
			..trait_impl("Shape")
		};

		assert_eq!(
			item_path(&area, true),
			Some(qualified(&["demo", "shapes", "Circle"], Some("Shape"), &["area"]))
		);

		// generic arguments tell apart `impl` blocks of one type
		let get = CanonicalPath {
			self_ty_arguments: Some("<u8, T>".to_owned()),
			..canonical(&["demo", "Wrapper"], Some("get"))
		};

		assert_eq!(item_path(&get, true).unwrap().to_string(), "<::demo::Wrapper<u8,T>>::get");

		let from = CanonicalPath {
			impl_trait: Some("From< u8 >".to_owned()),
			..get
		};

		assert_eq!(item_path(&from, true).unwrap().to_string(), "<::demo::Wrapper<u8,T> as From<u8>>::get");
		assert_eq!(
			item_path(&inherent_impl(), false),
			Some(qualified(&["demo", "shapes", "Circle"], None, &[]))
		);
		assert_eq!(
			item_path(&trait_impl("crate::shapes::Shape"), false),
			Some(qualified(&["demo", "shapes", "Circle"], Some("Shape"), &[]))
		);
	}

	fn qualified(self_ty: &[&str], trait_name: Option<&str>, segments: &[&str]) -> ItemPath {
		ItemPath {
			qualifier: Some(Qualifier {
				self_ty: Box::new(global(self_ty)),
				trait_path: trait_name.map(|name| Box::new(ItemPath::from_segments([name]))),
			}),
			segments: segments.iter().map(|&segment| segment.into()).collect(),
			..ItemPath::default()
		}
	}

	#[test]
	fn tells_trait_names() {
		assert_eq!(trait_name("Display"), Some("Display"));
		assert_eq!(trait_name("fmt::Display"), Some("Display"));
		assert_eq!(trait_name("::core :: fmt :: Display"), Some("Display"));
		assert_eq!(trait_name("From<u8>"), Some("From"));
		assert_eq!(trait_name("From < Vec < u8 > >"), Some("From"));
		assert_eq!(trait_name("ops::Add<Output = Self>"), Some("Add"));
		assert_eq!(trait_name("Fn(u8) -> u8"), Some("Fn"));
		assert_eq!(trait_name("!Send"), Some("Send"));
		assert_eq!(trait_name("! Sync"), Some("Sync"));
		assert_eq!(trait_name("r#try::r#Try"), Some("Try"));
		assert_eq!(trait_name("Größe"), Some("Größe"));
		assert_eq!(trait_name(""), None);
		assert_eq!(trait_name("dyn Tr + Send"), None);
		assert_eq!(trait_name("1x"), None);
	}

	fn trait_impl(written: &str) -> CanonicalPath {
		CanonicalPath {
			impl_trait: Some(written.to_owned()),
			..inherent_impl()
		}
	}
}
