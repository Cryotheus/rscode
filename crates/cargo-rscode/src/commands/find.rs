//! `find`: items by name or path pattern.
//!
//! Every `cfg` variant of an item is found (items under disabled `cfg`s are marked inactive), since items with the
//! same path under different `cfg`s are usually one item in intent. `--show` selects the location fields; the path
//! shown is always the canonical (definition) path, visible from where one stands or not, and `--from` adds the
//! paths usable from a module (`crate`: the item's crate root; `::`: another crate).

use super::retry;
use crate::args;
use crate::args::FindArgs;
use crate::args::FromArg;
use crate::args::MessageFormat;
use crate::args::OutputArgs;
use crate::render;
use crate::render::MatchRow;
use crate::render::PathDisplay;
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
use rscode::Workspace;
use rscode::pattern::IdentPattern;
use rscode::query::FindMatch;
use std::process::ExitCode;

/// The matches of the search in `resolver`'s workspace (up to the `--limit`, which the second value tells when there
/// were more), with the usable paths that `--from` asks for.
fn matches_of(resolver: &Resolver<'_>, find: &Find, args: &FindArgs) -> anyhow::Result<(Vec<FindMatch>, Option<usize>)> {
	let mut find = find.clone();
	let from = args.from.as_ref().map(|from| viewpoint(resolver, from)).transpose()?;

	if let Some(Some(viewpoint)) = from {
		find = find.from(viewpoint);
	}

	// one more than the limit tells whether there were more
	if let Some(limit) = args.limit {
		find = find.limit(limit.saturating_add(1));
	}

	let mut found = find.run_with(resolver)?;
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

	Ok((found, truncated))
}

/// The crate a pattern starts from (`::name::…`, or `name::…`).
fn pattern_crate(pattern: &str) -> Option<&str> {
	let pattern = pattern.trim();
	let pattern = pattern
		.strip_prefix("use")
		.filter(|rest| rest.starts_with(char::is_whitespace))
		.unwrap_or(pattern)
		.trim();

	pattern.strip_prefix("::").unwrap_or(pattern).split("::").next()
}

/// Prints the matches, and notes: that nothing was found, or that there were more than the `--limit`, and the
/// workspace members that were not searched (unless the command line named the packages).
fn print(
	ui: &Ui,
	args: &FindArgs,
	output: &OutputArgs,
	workspace: &Workspace,
	paths: &PathDisplay,
	found: (Vec<FindMatch>, Option<usize>),
	named_packages: bool,
) -> anyhow::Result<ExitCode> {
	let (found, truncated) = found;
	let text = match output.format {
		MessageFormat::Human => {
			let rows: Vec<MatchRow> = found.iter().map(|found| MatchRow::new(found, paths)).collect();

			render::find_human(&rows, &args.show)
		}

		MessageFormat::Json => render::find_json(&found, paths)?,

		MessageFormat::FileLines => {
			let rows: Vec<MatchRow> = found.iter().map(|found| MatchRow::new(found, paths)).collect();

			render::file_lines(&rows)?
		}
	};

	ui::print(&text)?;

	if found.is_empty() {
		ui.note("no items found");
	} else if let Some(limit) = truncated {
		ui.note(format!("showing the first {limit} items (--limit)"));
	}

	let member = args.patterns.iter().find_map(|pattern| workspace.unloaded_member_with_crate(pattern_crate(pattern)?));

	// (with matches, only when the command line left the selection to cargo)
	if (found.is_empty() || !named_packages)
		&& let Some(hint) = super::unloaded_hint(workspace, member)
	{
		ui.note(hint);
	}

	Ok(ExitCode::SUCCESS)
}

pub(super) fn run(matches: &ArgMatches, ui: &Ui) -> anyhow::Result<ExitCode> {
	let args = FindArgs::from_matches(matches);
	let output = OutputArgs::from_matches(matches);
	let options = args::load_options(matches)?;
	let find = search(&args)?;
	let (workspace, paths) = super::load(ui, &options, output.absolute_paths)?;
	let resolver = Resolver::new(&workspace);
	let found = matches_of(&resolver, &find, &args)?;

	// nothing found: search the workspace members that the command line did not select
	let named_packages = options.workspace || !options.packages.is_empty();
	let member = (args.patterns.iter())
		.find_map(|pattern| pattern_crate(pattern))
		.and_then(|krate| ItemPath::parse(krate).ok());

	if found.0.is_empty()
		&& !named_packages
		&& let Some(widening) = options.widened(&workspace, member.as_ref())
	{
		let (wider, paths) = super::load(ui, &widening.options, output.absolute_paths)?;
		let resolver = Resolver::new(&wider);
		let found = matches_of(&resolver, &find, &args)?;
		let members = retry::members_of(&wider, found.0.iter().map(|found| found.item), &widening.members);

		match members.is_empty() {
			true => ui.note(retry::note(&widening.members, false)),
			false => ui.note(retry::note(&members, true)),
		}

		return print(ui, &args, &output, &wider, &paths, found, named_packages);
	}

	print(ui, &args, &output, &workspace, &paths, found, named_packages)
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
