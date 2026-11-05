//! Searching the text of the loaded crates' files for a regular expression: each matching line, with the item it is in.

use super::find::Find;
use super::references::ModuleFiles;
use super::references::Place;
use super::references::line_of;
use crate::Error;
use crate::model::CrateId;
use crate::model::ItemId;
use crate::model::Workspace;
use crate::pattern::PathPattern;
use crate::resolve::Resolver;
use crate::source::FileId;
use crate::source::LineCol;
use crate::source::TextRange;
use regex::Regex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

/// Options for [`search_text`].
#[derive(Debug, Default, Clone)]
pub struct SearchTextOptions {
	/// Keep only the matches inside of an item that this pattern matches (as [`Find`] matches items), or inside of its
	/// descendants (the items of a module, of its file too, and of an `impl` block, the code of a function).
	pub within: Option<PathPattern>,
}

/// A line found by [`search_text`], with where it is.
#[derive(Debug, Clone, Serialize)]
pub struct TextMatch {
	/// The crate whose module the file is attributed to (the first selected crate that loads the file, or with
	/// [`SearchTextOptions::within`], the first in which the match is within a matching item).
	#[serde(skip)]
	pub krate: CrateId,

	/// The file's path.
	pub path: PathBuf,

	/// The byte range of the first match on the line.
	pub range: TextRange,

	/// Where the first match on the line starts.
	pub start: LineCol,

	/// The canonical path of the module whose file the line is in (the file's own module, not an inline module inside
	/// of it).
	pub module: String,

	/// The canonical path of the innermost item the match is in, below [`TextMatch::module`] (see
	/// [`FoundReference::item`](super::FoundReference::item)). `None` at the top level of the module, and in files that
	/// do not parse.
	pub item: Option<String>,

	/// [`TextMatch::item`] relative to [`TextMatch::module`] when it is below it (`Type::method`, `tests::case`), or
	/// else its canonical path.
	pub local_item: Option<String>,

	/// The line, without leading and trailing whitespace. Lines longer than 100 characters are cut around the match,
	/// with `...` where they are cut.
	pub line: String,
}

/// The lines found by [`search_text`].
#[derive(Debug, Default, Clone, Serialize)]
pub struct TextReport {
	/// Sorted by file path and position, one per matching line.
	pub matches: Vec<TextMatch>,

	/// The number of files searched.
	pub searched: usize,

	/// Human-readable notes about files whose matches could not be placed in items (files that do not parse).
	pub notes: Vec<String>,
}

impl TextReport {
	/// The number of files with matches.
	pub fn files(&self) -> usize {
		let mut files: Vec<&PathBuf> = self.matches.iter().map(|found| &found.path).collect();

		files.dedup();
		files.len()
	}
}

/// Whether `item`, or one of its ancestors, is one of `within`.
fn is_within(ws: &Workspace, item: ItemId, within: &HashSet<ItemId>) -> bool {
	std::iter::once(item).chain(ws.ancestors(item)).any(|item| within.contains(&item))
}

/// Searches the text of every file of the selected crates (each file once, however many crates load it) for
/// `regex`, line by line (`^` and `$` match at the line's start and end, and a match never spans lines), and reports
/// each matching line at its first match, with the module whose file it is, the innermost item the match is in,
/// and the line.
///
/// Files that do not parse are searched too: their matches have no item, and a note says so (with
/// [`SearchTextOptions::within`], they are left out, since they are in no item).
///
/// Fails when finding the items of [`SearchTextOptions::within`] fails.
pub fn search_text(resolver: &Resolver<'_>, regex: &Regex, options: &SearchTextOptions) -> Result<TextReport, Error> {
	let ws = resolver.workspace();
	let within: Option<HashSet<ItemId>> = match &options.within {
		None => None,

		Some(pattern) => {
			let found = Find::new().path_pattern(pattern.clone()).run_with(resolver)?;

			Some(found.into_iter().map(|found| found.item).collect())
		}
	};

	// each file once, with every selected crate that loads it
	let mut files: BTreeMap<&Path, Vec<(CrateId, FileId)>> = BTreeMap::new();

	for krate in ws.selected_crates() {
		for (index, file) in krate.files().iter().enumerate() {
			files.entry(file.path()).or_default().push((krate.id(), FileId(index as u32)));
		}
	}

	let mut modules = ModuleFiles::new(ws);
	let mut report = TextReport {
		searched: files.len(),
		..TextReport::default()
	};

	for (path, loaders) in &files {
		let (first_crate, first_file) = loaders[0];
		let source = ws.krate(first_crate).file(first_file);
		let text = source.text();
		let index = source.line_index();
		let unparsed =
			(modules.of(first_crate, first_file).iter()).any(|&module| ws.item(module).module_info().is_some_and(|info| info.load_error.is_some()));
		let mut unplaced = 0;

		for line in 1..=index.line_count() {
			let start = index.line_start(line);

			// (no line follows a final line break)
			if start == text.len() && line > 1 {
				break;
			}

			let Some(found) = regex.find(&text[start..index.line_end(text, line)]) else {
				continue;
			};

			let range = TextRange::new(start + found.start(), start + found.end());
			if unparsed {
				unplaced += 1;
			}

			let located = (loaders.iter())
				.map(|&(krate, file)| (krate, modules.containing(krate, file, range.start)))
				.find(|&(_, (_, item))| within.as_ref().is_none_or(|within| is_within(ws, item, within)));
			let Some((krate, (module, item))) = located else {
				continue;
			};
			let place = Place::new(resolver, module, item);

			report.matches.push(TextMatch {
				krate,
				path: path.to_path_buf(),
				range,
				start: source.line_col(range.start),
				module: place.module,
				item: place.item,
				local_item: place.local_item,
				line: line_of(source, range),
			});
		}

		if unplaced > 0 {
			let what = match within {
				Some(_) => "were left out, since they are in no item",
				None => "are not placed in items",
			};

			report.notes.push(format!(
				"{}: this file does not parse: its matching lines ({unplaced}) {what}",
				ws.display_path(path).display()
			));
		}
	}

	Ok(report)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::CrateSpec;
	use crate::query::references::MAX_LINE_CHARS;

	/// A crate of one file, `lib.rs` with `text`, written to a directory of its own.
	fn workspace(name: &str, text: &str) -> Workspace {
		let dir = std::env::temp_dir().join(format!("rscode-search-text-{name}-{}", std::process::id()));

		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(dir.join("lib.rs"), text).unwrap();

		let mut ws = Workspace::new(&dir);

		ws.load_crate(CrateSpec::new("demo", dir.join("lib.rs")));
		ws.link();
		std::fs::remove_dir_all(&dir).unwrap();
		ws
	}

	#[test]
	fn reports_the_first_match_of_each_line_in_chars() {
		let ws = workspace("columns", "fn é() { needle(); needle(); }\n\nconst X: u8 = 1;\n");
		let resolver = Resolver::new(&ws);
		let report = search_text(&resolver, &Regex::new("needle").unwrap(), &SearchTextOptions::default()).unwrap();

		assert_eq!(report.searched, 1);
		assert_eq!(report.matches.len(), 1);

		let found = &report.matches[0];

		assert_eq!(found.start, LineCol { line: 1, column: 10 });
		assert_eq!(found.local_item.as_deref(), Some("é"));
		assert_eq!(found.module, "demo");
		assert_eq!(found.line, "fn é() { needle(); needle(); }");
	}

	#[test]
	fn anchors_match_at_lines_and_no_line_follows_the_last_break() {
		let ws = workspace("anchors", "a\r\nb\r\n");
		let resolver = Resolver::new(&ws);
		let report = search_text(&resolver, &Regex::new("^.?$").unwrap(), &SearchTextOptions::default()).unwrap();
		let lines: Vec<usize> = report.matches.iter().map(|found| found.start.line).collect();

		assert_eq!(lines, [1, 2]);
	}

	#[test]
	fn notes_files_that_do_not_parse() {
		let ws = workspace("unparsed", "fn a() {\n\tneedle\n");
		let resolver = Resolver::new(&ws);
		let report = search_text(&resolver, &Regex::new("needle").unwrap(), &SearchTextOptions::default()).unwrap();

		assert_eq!(report.matches.len(), 1);
		assert_eq!(report.matches[0].item, None);
		assert_eq!(report.notes.len(), 1);
		assert!(report.notes[0].contains("does not parse"), "{:?}", report.notes);

		let within = SearchTextOptions {
			within: Some(PathPattern::parse("a", Default::default()).unwrap()),
		};
		let report = search_text(&resolver, &Regex::new("needle").unwrap(), &within).unwrap();

		assert!(report.matches.is_empty());
		assert!(report.notes[0].contains("left out"), "{:?}", report.notes);
	}

	/// `lib.rs` of [`attributes_matches_to_the_innermost_items`] and [`keeps_the_matches_within_items`]: `needle` in a
	/// method, a function, and a test.
	const ITEMS: &str = "pub struct S;\n\nimpl S {\n\tpub fn method(&self) {\n\t\tneedle();\n\t}\n}\n\nfn needle() {}\n\n#[cfg(test)]\nmod tests {\n\t#[test]\n\tfn case() {\n\t\tsuper::needle();\n\t}\n}\n";

	/// Where each match is, and the item it is in.
	fn places(report: &TextReport) -> Vec<(usize, usize, Option<&str>)> {
		(report.matches.iter())
			.map(|found| (found.start.line, found.start.column, found.local_item.as_deref()))
			.collect()
	}

	#[test]
	fn attributes_matches_to_the_innermost_items() {
		let ws = workspace("items", ITEMS);
		let resolver = Resolver::new(&ws);
		let report = search_text(&resolver, &Regex::new("needle").unwrap(), &SearchTextOptions::default()).unwrap();

		assert_eq!(
			places(&report),
			[(5, 3, Some("S::method")), (9, 4, Some("needle")), (15, 10, Some("tests::case"))]
		);
		assert!(report.matches.iter().all(|found| found.module == "demo"));
		assert_eq!(report.files(), 1);
		assert!(report.notes.is_empty(), "{:?}", report.notes);
	}

	#[test]
	fn keeps_the_matches_within_items() {
		let ws = workspace("within", ITEMS);
		let resolver = Resolver::new(&ws);
		let needle = Regex::new("needle").unwrap();
		let within = |pattern: &str| SearchTextOptions {
			within: Some(PathPattern::parse(pattern, Default::default()).unwrap()),
		};

		let report = search_text(&resolver, &needle, &within("S::*")).unwrap();

		assert_eq!(places(&report), [(5, 3, Some("S::method"))]);

		// descendants of a matching item are within it
		let report = search_text(&resolver, &needle, &within("tests")).unwrap();

		assert_eq!(places(&report), [(15, 10, Some("tests::case"))]);

		let report = search_text(&resolver, &needle, &within("crate::needle")).unwrap();

		assert_eq!(places(&report), [(9, 4, Some("needle"))]);

		let report = search_text(&resolver, &needle, &within("nothing")).unwrap();

		assert!(report.matches.is_empty());
	}

	#[test]
	fn cuts_long_lines_around_the_match() {
		let text = format!("const LONG: &str = \"{}needle{}\";\n", "x".repeat(200), "y".repeat(200));
		let ws = workspace("long", &text);
		let resolver = Resolver::new(&ws);
		let report = search_text(&resolver, &Regex::new("needle").unwrap(), &SearchTextOptions::default()).unwrap();
		let line = &report.matches[0].line;

		assert_eq!(report.matches[0].start, LineCol { line: 1, column: 221 });
		assert!(line.starts_with("...x") && line.ends_with("y..."), "{line}");
		assert!(line.contains("needle"), "{line}");
		assert_eq!(line.chars().count(), MAX_LINE_CHARS + 6);
	}
}
