//! `find`: items by name or path pattern.
//!
//! Every `cfg` variant of an item is found (items under disabled `cfg`s are marked inactive), since items with the
//! same path under different `cfg`s are usually one item in intent. `--show` selects the location fields; the path
//! shown is always the canonical (definition) path, visible from where one stands or not, and `--from` adds the
//! paths usable from a module (`crate`: the item's crate root; `::`: another crate).

use crate::args;
use crate::args::FindArgs;
use crate::args::FromArg;
use crate::args::MessageFormat;
use crate::args::OutputArgs;
use crate::render;
use crate::render::MatchRow;
use crate::ui;
use crate::ui::Ui;
use clap::ArgMatches;
use rscode::Find;
use rscode::ItemId;
use rscode::ItemKind;
use rscode::ItemPath;
use rscode::MatchOptions;
use rscode::PathPattern;
use rscode::Resolver;
use rscode::Viewpoint;
use rscode::pattern::IdentPattern;
use std::process::ExitCode;

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	let args = FindArgs::from_matches(matches);
	let output = OutputArgs::from_matches(matches);
	let (workspace, paths) = super::load(ui, &args::load_options(matches)?, output.absolute_paths)?;
	let resolver = Resolver::new(&workspace);
	let mut find = search(&args)?;
	let from = args.from.as_ref().map(|from| viewpoint(&resolver, from)).transpose()?;

	if let Some(Some(viewpoint)) = from {
		find = find.from(viewpoint);
	}

	// one more than the limit tells whether there were more
	if let Some(limit) = args.limit {
		find = find.limit(limit.saturating_add(1));
	}

	let mut found = find.run_with(&resolver)?;
	let truncated = args.limit.filter(|&limit| found.len() > limit);

	if let Some(limit) = truncated {
		found.truncate(limit);
	}

	if let Some(None) = from {
		for found in &mut found {
			let root = ItemId::crate_root(found.item.krate());

			found.usable_paths = resolver.usable_paths(found.item, Viewpoint::Module(root));
		}
	}

	let text = match output.format {
		MessageFormat::Human => {
			let rows: Vec<MatchRow> = found.iter().map(|found| MatchRow::new(found, &paths)).collect();

			render::find_human(&rows, &args.show)
		}

		MessageFormat::Json => render::find_json(&found, &paths)?,

		MessageFormat::FileLines => {
			let rows: Vec<MatchRow> = found.iter().map(|found| MatchRow::new(found, &paths)).collect();

			render::file_lines(&rows)?
		}
	};

	ui::print(&text)?;

	if found.is_empty() {
		ui.note("no items found");

		let member = args.patterns.iter().find_map(|pattern| {
			let pattern = pattern.trim();
			let pattern = pattern
				.strip_prefix("use")
				.filter(|rest| rest.starts_with(char::is_whitespace))
				.unwrap_or(pattern)
				.trim();
			let first = pattern.strip_prefix("::").unwrap_or(pattern).split("::").next()?;

			workspace.unloaded_member_with_crate(first)
		});

		if let Some(hint) = super::unloaded_hint(&workspace, member) {
			ui.note(hint);
		}
	} else if let Some(limit) = truncated {
		ui.note(format!("showing the first {limit} items (--limit)"));
	}

	Ok(ExitCode::SUCCESS)
}

/// The search: patterns, identifier patterns, kinds, and flags.
fn search(args: &FindArgs) -> Result<Find, rscode::Error> {
	let options = MatchOptions {
		ignore_case: args.ignore_case,
	};
	let mut find = Find::new()
		.ignore_case(args.ignore_case)
		.active_only(args.active_only)
		.imports(args.imports);

	for pattern in &args.patterns {
		find = find.pattern(pattern)?;
	}

	let identifiers = args
		.contains
		.iter()
		.map(|text| IdentPattern::contains(text, options))
		.chain(args.starts_with.iter().map(|text| IdentPattern::starts_with(text, options)))
		.chain(args.ends_with.iter().map(|text| IdentPattern::ends_with(text, options)));

	for identifier in identifiers {
		find = find.path_pattern(PathPattern::from_ident(identifier));
	}

	for &kind in &args.kinds {
		find = find.kind(kind);
	}

	Ok(find)
}

/// The viewpoint of `--from`; `None` for `crate`, which is each found item's own crate root.
fn viewpoint(resolver: &Resolver<'_>, from: &FromArg) -> anyhow::Result<Option<Viewpoint>> {
	let text = match from {
		FromArg::CrateRoot => return Ok(None),
		FromArg::Foreign => return Ok(Some(Viewpoint::Foreign)),
		FromArg::Module(text) => text,
	};

	let workspace = resolver.workspace();
	let modules: Vec<ItemId> = resolver
		.resolve_item_path(&ItemPath::parse(text)?)
		.into_iter()
		.filter(|&item| workspace.item(item).kind == ItemKind::Module)
		.collect();

	match modules.as_slice() {
		[module] => Ok(Some(Viewpoint::Module(*module))),
		[] => anyhow::bail!("`--from {text}` does not name a module"),

		_ => {
			let candidates: Vec<String> = modules.iter().map(|&module| resolver.canonical_path(module).to_string()).collect();

			anyhow::bail!("`--from {text}` names several modules:\n{}", candidates.join("\n"))
		}
	}
}
