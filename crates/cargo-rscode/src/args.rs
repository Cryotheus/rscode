//! Converting parsed arguments into rscode's options.
//!
//! Paths and patterns stay strings here; commands parse them (their errors are reported like other failures).

use crate::cli::parse_key_value;
use cargo::GlobalContext;
use cargo::context::TOP_LEVEL_CONFIG_KEYS;
use cargo::util::command_prelude::ArgMatchesExt as _;
use cargo::util::print_available_packages;
use cargo::workspace::Target;
use clap::ArgMatches;
use rscode::Edition;
use rscode::ItemKind;
use rscode::LoadOptions;
use rscode::edit::FmtOptions;
use rscode::edit::InsertOptions;
use rscode::edit::InsertPosition;
use rscode::edit::RemoveOptions;
use rscode::edit::RenameOptions;
use rscode::edit::ReplaceOptions;
use rscode::query::ViewMode;
use rscode::query::ViewOptions;
use rscode::resolve::ReferenceOptions;
use rscode::rscode_fmt::FormatOptions;
use rscode::rscode_fmt::RsFormatter;
use rscode::rscode_fmt::RustFmtOptions;
use rscode::rscode_sort::OrderingSchema;
use rscode::rscode_sort::SortOptions;
use rscode::workspace::TargetSelection;
use std::path::Path;
use std::path::PathBuf;

const TARGET_OPTIONS: [TargetOption; 4] = [
	TargetOption {
		id: "bin",
		plural: "binaries",
		is_kind: Target::is_bin,
	},
	TargetOption {
		id: "example",
		plural: "examples",
		is_kind: Target::is_example,
	},
	TargetOption {
		id: "test",
		plural: "test targets",
		is_kind: Target::is_test,
	},
	TargetOption {
		id: "bench",
		plural: "bench targets",
		is_kind: Target::is_bench,
	},
];

/// `fmt --config` values, split between cargo and rustfmt.
#[derive(Debug, Default)]
struct ConfigValues {
	/// Values for cargo, as given.
	cargo: Vec<String>,

	/// rustfmt's `KEY=VALUE` settings.
	rustfmt: Vec<(String, String)>,
}

impl ConfigValues {
	/// Splits `--config` values (see [`is_rustfmt_config`]); rustfmt's are comma-separated lists.
	fn split(values: Vec<String>) -> anyhow::Result<Self> {
		let mut split = Self::default();

		for value in values {
			if !is_rustfmt_config(&value) {
				split.cargo.push(value);
				continue;
			}

			for setting in value.split(',').filter(|setting| !setting.trim().is_empty()) {
				match parse_key_value(setting) {
					Ok(pair) => split.rustfmt.push(pair),
					Err(error) => anyhow::bail!("invalid rustfmt setting `{setting}` in `--config {value}`: {error}"),
				}
			}
		}

		Ok(split)
	}
}

/// What `fmt`/`sort` do with the results (`--emit`).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum Emit {
	/// Write changed files.
	Files,

	/// Print the formatted contents.
	Stdout,

	Diff,

	/// rustfmt's `--emit json`.
	Json,

	/// rustfmt's `--emit checkstyle`.
	Checkstyle,
}

impl Emit {
	/// The `--emit` value.
	pub(crate) fn name(self) -> &'static str {
		match self {
			Self::Files => "files",
			Self::Stdout => "stdout",
			Self::Diff => "diff",
			Self::Json => "json",
			Self::Checkstyle => "checkstyle",
		}
	}
}

/// `find`
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub(crate) struct FindArgs {
	pub(crate) patterns: Vec<String>,
	pub(crate) contains: Vec<String>,
	pub(crate) starts_with: Vec<String>,
	pub(crate) ends_with: Vec<String>,
	pub(crate) kinds: Vec<ItemKind>,
	pub(crate) ignore_case: bool,
	pub(crate) active_only: bool,
	pub(crate) imports: bool,
	pub(crate) from: Option<FromArg>,
	pub(crate) show: Vec<ShowField>,
	pub(crate) limit: Option<usize>,
}

impl FindArgs {
	pub(crate) fn from_matches(matches: &ArgMatches) -> Self {
		let mut from = matches._value_of("from").map(FromArg::parse);
		let mut show: Vec<ShowField> = Vec::new();
		let names = matches._values_of("show");
		let names = if names.is_empty() {
			vec!["kind".to_owned(), "location".to_owned()]
		} else {
			names
		};

		for field in names.iter().filter_map(|name| ShowField::parse(name)).flatten() {
			if !show.contains(field) {
				show.push(*field);
			}
		}

		// usable paths need a viewpoint, and asking for a viewpoint means wanting the paths
		if from.is_some() && !show.contains(&ShowField::Usable) {
			show.push(ShowField::Usable);
		} else if from.is_none() && show.contains(&ShowField::Usable) {
			from = Some(FromArg::CrateRoot);
		}

		let kinds: Vec<ItemKind> = matches
			.get_many::<ItemKind>("kind")
			.map(|kinds| kinds.copied().collect())
			.unwrap_or_default();

		Self {
			patterns: matches._values_of("patterns"),
			contains: matches._values_of("contains"),
			starts_with: matches._values_of("starts-with"),
			ends_with: matches._values_of("ends-with"),
			ignore_case: matches.flag("ignore-case"),
			active_only: matches.flag("active-only"),
			// asking for imports means wanting them
			imports: matches.flag("imports") || kinds.contains(&ItemKind::Import),
			kinds,
			from,
			show,
			limit: matches.get_one::<usize>("limit").copied(),
		}
	}
}

/// `fmt` and `sort`
#[derive(Debug, Clone)]
pub(crate) struct FmtArgs {
	pub(crate) targets: Vec<String>,
	pub(crate) options: FmtOptions,
	pub(crate) check: bool,
	pub(crate) emit: Emit,

	/// The `--config` values that are cargo's (for `fmt`, the others are rustfmt's).
	pub(crate) cargo_config: Vec<String>,
}

impl FmtArgs {
	/// `sort_only`: the arguments of `sort`, which only sorts.
	pub(crate) fn from_matches(matches: &ArgMatches, sort_only: bool) -> anyhow::Result<Self> {
		let check = matches.flag("check");
		let emit = match matches._value_of("emit") {
			None if check => Emit::Diff,
			None | Some("files") => Emit::Files,
			Some("stdout") => Emit::Stdout,
			Some("diff") => Emit::Diff,
			Some("json") => Emit::Json,
			Some("checkstyle") => Emit::Checkstyle,
			Some(other) => anyhow::bail!("unknown `--emit` mode `{other}`"),
		};

		anyhow::ensure!(
			!(check && emit == Emit::Files),
			"`--check` never writes files; it cannot be used with `--emit files`"
		);

		// `--config` is cargo's on every subcommand, and on `fmt` also rustfmt's (`--rustfmt-config` is only rustfmt's)
		let mut config = match sort_only {
			true => ConfigValues {
				cargo: matches._values_of("config"),
				rustfmt: Vec::new(),
			},

			false => ConfigValues::split(matches._values_of("config"))?,
		};

		config.rustfmt.extend(
			matches
				.try_get_many::<(String, String)>("rustfmt-config")
				.ok()
				.flatten()
				.into_iter()
				.flatten()
				.cloned(),
		);

		let skip_children = matches.flag("skip-children");
		let sort = (sort_only || !matches.flag("no-sort")).then(|| {
			SortOptions::new()
				.schema(matches.get_one::<OrderingSchema>("schema").copied().unwrap_or_default())
				.merge_extern_blocks(!matches.flag("no-merge-extern-blocks"))
				.recursive(!skip_children)
		});
		let formatter = match sort_only {
			true => RsFormatter::None,
			false => matches.get_one::<RsFormatter>("formatter").copied().unwrap_or_default(),
		};
		let rustfmt = RustFmtOptions {
			program: None,
			edition: matches.try_get_one::<Edition>("edition").ok().flatten().copied(),
			style_edition: matches.try_get_one::<Edition>("style-edition").ok().flatten().copied(),
			config_path: matches._value_of("config-path").map(absolute_path),
			config: config.rustfmt,
		};

		Ok(Self {
			targets: matches._values_of("targets"),
			options: FmtOptions {
				format: FormatOptions {
					formatter,
					rustfmt,
					sort,
					allow_comment_loss: matches.flag("allow-comment-loss"),
				},
				skip_children,
				active_only: matches.flag("active-only"),
			},
			check,
			emit,
			cargo_config: config.cargo,
		})
	}

	/// The workspace options, with cargo's share of the `--config` values.
	pub(crate) fn load_options(&self, matches: &ArgMatches) -> anyhow::Result<LoadOptions> {
		load_options_with_config(matches, self.cargo_config.clone())
	}
}

/// The viewpoint of `find --from`.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum FromArg {
	/// `crate`: the crate root of each found item's crate.
	CrateRoot,

	/// `::`: another crate.
	Foreign,

	/// A module path.
	Module(String),
}

impl FromArg {
	fn parse(text: &str) -> Self {
		match text.trim() {
			"crate" => Self::CrateRoot,
			"::" => Self::Foreign,
			path => Self::Module(path.to_owned()),
		}
	}
}

/// `insert`
#[derive(Debug, Clone)]
pub(crate) struct InsertArgs {
	pub(crate) parent: String,
	pub(crate) source: SourceArg,
	pub(crate) options: InsertOptions,
	pub(crate) dry_run: bool,

	/// `--fmt`
	pub(crate) format: bool,
}

impl InsertArgs {
	pub(crate) fn from_matches(matches: &ArgMatches) -> anyhow::Result<Self> {
		let anchor = matches._value_of("anchor").map(str::to_owned);
		let position = match (matches._value_of("position").unwrap_or("end"), anchor) {
			("before", Some(anchor)) => InsertPosition::Before(anchor),
			("after", Some(anchor)) => InsertPosition::After(anchor),

			("before" | "after", None) => {
				anyhow::bail!("`--position before` and `--position after` need an `--anchor`")
			}

			(_, Some(_)) => anyhow::bail!("`--anchor` needs `--position before` or `--position after`"),
			("start", None) => InsertPosition::Start,
			(_, None) => InsertPosition::End,
		};

		Ok(Self {
			parent: matches._value_of("parent").unwrap_or_default().to_owned(),
			source: SourceArg::from_matches(matches),
			options: InsertOptions {
				position,
				force: matches.flag("force"),
			},
			dry_run: matches.flag("dry-run"),
			format: matches.flag("fmt"),
		})
	}
}

/// How results are printed (`--message-format`).
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub(crate) enum MessageFormat {
	#[default]
	Human,
	Json,

	/// rustfmt's `--file-lines` JSON (`find` only).
	FileLines,
}

/// The output options of every subcommand but `mcp`.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub(crate) struct OutputArgs {
	pub(crate) format: MessageFormat,

	/// `--absolute-paths`
	pub(crate) absolute_paths: bool,
}

impl OutputArgs {
	pub(crate) fn from_matches(matches: &ArgMatches) -> Self {
		let format = match matches._value_of("message-format").map(str::to_ascii_lowercase).as_deref() {
			Some("json") => MessageFormat::Json,
			Some("file-lines") => MessageFormat::FileLines,
			_ => MessageFormat::Human,
		};

		Self {
			format,
			absolute_paths: matches.flag("absolute-paths"),
		}
	}
}

/// `remove`
#[derive(Debug, Clone)]
pub(crate) struct RemoveArgs {
	pub(crate) paths: Vec<String>,
	pub(crate) options: RemoveOptions,
	pub(crate) dry_run: bool,
}

impl RemoveArgs {
	pub(crate) fn from_matches(matches: &ArgMatches) -> Self {
		Self {
			paths: matches._values_of("paths"),
			options: RemoveOptions {
				keep_files: matches.flag("keep-files"),
				prune_imports: matches.flag("prune-imports"),
				active_only: matches.flag("active-only"),
			},
			dry_run: matches.flag("dry-run"),
		}
	}
}

/// `rename`
#[derive(Debug, Clone)]
pub(crate) struct RenameArgs {
	pub(crate) path: String,
	pub(crate) new_name: String,
	pub(crate) options: RenameOptions,
	pub(crate) dry_run: bool,
}

impl RenameArgs {
	pub(crate) fn from_matches(matches: &ArgMatches) -> Self {
		Self {
			path: matches._value_of("path").unwrap_or_default().to_owned(),
			new_name: matches._value_of("new-name").unwrap_or_default().to_owned(),
			options: RenameOptions {
				force: matches.flag("force"),
				references: ReferenceOptions {
					method_calls: matches.flag("method-calls"),
					macro_tokens: matches.flag("macro-tokens"),
					doc_links: matches.flag("doc-links"),
				},
			},
			dry_run: matches.flag("dry-run"),
		}
	}
}

/// `replace`
#[derive(Debug, Clone)]
pub(crate) struct ReplaceArgs {
	pub(crate) path: String,
	pub(crate) source: SourceArg,
	pub(crate) options: ReplaceOptions,
	pub(crate) dry_run: bool,

	/// `--fmt`
	pub(crate) format: bool,
}

impl ReplaceArgs {
	pub(crate) fn from_matches(matches: &ArgMatches) -> Self {
		Self {
			path: matches._value_of("path").unwrap_or_default().to_owned(),
			source: SourceArg::from_matches(matches),
			options: ReplaceOptions {
				allow_kind_change: matches.flag("allow-kind-change"),
				all_variants: matches.flag("all-variants"),
			},
			dry_run: matches.flag("dry-run"),
			format: matches.flag("fmt"),
		}
	}
}

/// `find --show` fields.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ShowField {
	Kind,

	/// `file:line:column` of the start.
	Location,

	/// `file:line:column-line:column` (the end is exclusive).
	Span,

	Cfg,
	Vis,
	Crate,

	/// Paths usable from `--from`.
	Usable,
}

impl ShowField {
	/// What `all` stands for.
	const ALL: [Self; 6] = [Self::Kind, Self::Span, Self::Vis, Self::Crate, Self::Cfg, Self::Usable];

	fn parse(name: &str) -> Option<&'static [Self]> {
		let fields: &'static [Self] = match name {
			"kind" => &[Self::Kind],
			"location" => &[Self::Location],
			"span" => &[Self::Span],
			"cfg" => &[Self::Cfg],
			"vis" => &[Self::Vis],
			"crate" => &[Self::Crate],
			"usable" => &[Self::Usable],
			"all" => &Self::ALL,
			_ => return None,
		};

		Some(fields)
	}
}

/// Where new source comes from.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum SourceArg {
	Stdin,
	File(PathBuf),
}

impl SourceArg {
	fn from_matches(matches: &ArgMatches) -> Self {
		match matches._value_of("source") {
			None | Some("-") => Self::Stdin,
			Some(path) => Self::File(PathBuf::from(path)),
		}
	}
}

/// A target option (`--bin NAME`, ...) whose bare form lists the available targets.
struct TargetOption {
	id: &'static str,
	plural: &'static str,
	is_kind: fn(&Target) -> bool,
}

/// `view`
#[derive(Debug, Clone)]
pub(crate) struct ViewArgs {
	pub(crate) paths: Vec<String>,
	pub(crate) options: ViewOptions,
}

impl ViewArgs {
	pub(crate) fn from_matches(matches: &ArgMatches) -> Self {
		let mode = if matches.flag("outline") {
			ViewMode::Outline
		} else if matches.flag("full") {
			ViewMode::Full
		} else {
			ViewMode::Auto
		};

		Self {
			paths: matches._values_of("paths"),
			options: ViewOptions {
				mode,
				docs: !matches.flag("no-docs"),
				line_numbers: matches.flag("line-numbers"),
				impls: matches.flag("impls"),
				active_only: matches.flag("active-only"),
			},
		}
	}
}

fn absolute_path(path: &str) -> PathBuf {
	std::path::absolute(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// Whether a `fmt --config` value is rustfmt's (`max_width=80,hard_tabs=true`, like `rustfmt --config`) rather than
/// cargo's (every subcommand takes cargo's `--config`).
///
/// Cargo's values are files, or TOML settings of its configuration: dotted keys (`net.offline=true`) or its few
/// top-level keys (`paths`, `include`). rustfmt's keys are plain identifiers, none of which is a key of cargo's.
fn is_rustfmt_config(value: &str) -> bool {
	let Some((key, _)) = value.split_once('=') else {
		return false;
	};

	let key = key.trim();
	let is_cargo_key = key == "include" || TOP_LEVEL_CONFIG_KEYS.contains(&key);

	!key.is_empty()
		&& key.chars().all(|char| char.is_ascii_alphanumeric() || char == '_')
		&& !is_cargo_key
		&& !value.ends_with(".toml")
		&& !Path::new(value).exists()
}

fn list_available_on_bare_values(matches: &ArgMatches, config: &[String]) -> anyhow::Result<()> {
	let bare_package = matches.is_present_with_zero_values("package");
	let bare_target = TARGET_OPTIONS.iter().find(|option| matches.is_present_with_zero_values(option.id));

	if !bare_package && bare_target.is_none() {
		return Ok(());
	}

	let mut gctx = GlobalContext::default()?;

	gctx.configure(
		0,
		true,
		None,
		matches.flag("frozen"),
		matches.flag("locked"),
		matches.flag("offline"),
		&None,
		&[],
		config,
	)?;

	let workspace = matches.workspace(&gctx)?;

	if bare_package {
		print_available_packages(&workspace)?;
	}

	if let Some(option) = bare_target {
		let packages = matches.packages_from_flags()?.get_packages(&workspace)?;
		let mut names: Vec<&str> = packages
			.iter()
			.flat_map(|package| package.targets())
			.filter(|target| (option.is_kind)(target))
			.map(Target::name)
			.collect();

		names.sort_unstable();
		names.dedup();

		let list = if names.is_empty() {
			format!("No {} available.", option.plural)
		} else {
			format!("Available {}:\n    {}", option.plural, names.join("\n    "))
		};

		anyhow::bail!("\"--{}\" takes one argument.\n{list}", option.id);
	}

	Ok(())
}

/// The workspace options shared by every subcommand.
///
/// Validates like cargo does (`--exclude` needs `--workspace`, `--target` needs a value); a bare `-p` or `--bin`
/// (or `--example`, `--test`, `--bench`) fails with the list of what could be given, also like cargo.
pub(crate) fn load_options(matches: &ArgMatches) -> anyhow::Result<LoadOptions> {
	load_options_with_config(matches, matches._values_of("config"))
}

/// [`load_options`] with cargo's `--config` values (of `fmt`, some are rustfmt's).
fn load_options_with_config(matches: &ArgMatches, config: Vec<String>) -> anyhow::Result<LoadOptions> {
	matches.packages_from_flags()?;
	list_available_on_bare_values(matches, &config)?;

	let mut targets = matches.targets()?;

	anyhow::ensure!(targets.len() <= 1, "only one `--target` can be given");

	Ok(LoadOptions {
		manifest_path: matches._value_of("manifest-path").map(absolute_path),
		packages: matches._values_of("package"),
		workspace: matches.flag("workspace"),
		exclude: matches._values_of("exclude"),
		targets: target_selection(matches),
		features: split_features(&matches._values_of("features")),
		all_features: matches.flag("all-features"),
		no_default_features: matches.flag("no-default-features"),
		target: targets.pop(),
		cfgs: matches._values_of("cfg"),
		load_all_members: false,
		exact_features: matches.flag("exact-features"),
		offline: matches.flag("offline"),
		locked: matches.flag("locked"),
		frozen: matches.flag("frozen"),
		config,
		silent: matches.flag("quiet"),
	})
}

/// `mcp`
#[cfg(feature = "mcp")]
pub(crate) fn server_options(matches: &ArgMatches) -> anyhow::Result<rscode::mcp::ServerOptions> {
	Ok(rscode::mcp::ServerOptions {
		load: load_options(matches)?,
		read_only: matches.flag("read-only"),
		exposed: matches
			.get_many::<rscode::mcp::Exposure>("expose")
			.into_iter()
			.flatten()
			.cloned()
			.collect(),
	})
}

/// Splits `--features` values at whitespace and commas like cargo does, without duplicates.
pub(crate) fn split_features(values: &[String]) -> Vec<String> {
	let mut features: Vec<String> = Vec::new();

	for feature in values
		.iter()
		.flat_map(|value| value.split_whitespace())
		.flat_map(|value| value.split(','))
	{
		if !feature.is_empty() && !features.iter().any(|known| known == feature) {
			features.push(feature.to_owned());
		}
	}

	features
}

/// The target selection flags (`--lib`, `--bin NAME`, `--bins`, ..., `--all-targets`).
pub(crate) fn target_selection(matches: &ArgMatches) -> TargetSelection {
	TargetSelection {
		lib: matches.flag("lib"),
		bins: matches._values_of("bin"),
		all_bins: matches.flag("bins"),
		examples: matches._values_of("example"),
		all_examples: matches.flag("examples"),
		tests: matches._values_of("test"),
		all_tests: matches.flag("tests"),
		benches: matches._values_of("bench"),
		all_benches: matches.flag("benches"),
		all_targets: matches.flag("all-targets"),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::cli::cli;

	#[test]
	fn a_bare_bin_lists_the_binaries() {
		let error = load_options(&parse(&["cargo-rscode", "view", "x", "-p", "cargo-rscode", "--bin"]))
			.unwrap_err()
			.to_string();

		assert_eq!(error, "\"--bin\" takes one argument.\nAvailable binaries:\n    cargo-rscode");

		let error = load_options(&parse(&["cargo-rscode", "view", "x", "-p", "cargo-rscode", "--example"]))
			.unwrap_err()
			.to_string();

		assert_eq!(error, "\"--example\" takes one argument.\nNo examples available.");
	}

	#[test]
	fn a_bare_package_lists_the_members() {
		// the tests run inside of this workspace
		let error = load_options(&parse(&["cargo-rscode", "view", "x", "-p"])).unwrap_err().to_string();

		assert!(error.contains("\"--package <SPEC>\" requires a SPEC format value"), "{error}");
		assert!(error.contains("cargo-rscode"), "{error}");
	}

	#[test]
	fn default_load_options() {
		let options = load_options(&parse(&["cargo-rscode", "view", "x"])).unwrap();

		assert_eq!(options, LoadOptions::default());
		assert!(options.targets.is_default());
	}

	#[test]
	fn find_arguments() {
		let args = FindArgs::from_matches(&parse(&[
			"cargo-rscode",
			"find",
			"crate::a::*",
			"**::Foo",
			"-k",
			"fn,struct",
			"-i",
			"--contains",
			"oo",
			"--starts-with",
			"F",
			"--ends-with",
			"o",
			"--active-only",
			"--imports",
			"--limit",
			"5",
		]));

		assert_eq!(
			args,
			FindArgs {
				patterns: vec!["crate::a::*".to_owned(), "**::Foo".to_owned()],
				contains: vec!["oo".to_owned()],
				starts_with: vec!["F".to_owned()],
				ends_with: vec!["o".to_owned()],
				kinds: vec![ItemKind::Fn, ItemKind::Struct],
				ignore_case: true,
				active_only: true,
				imports: true,
				from: None,
				show: vec![ShowField::Kind, ShowField::Location],
				limit: Some(5),
			}
		);
	}

	#[test]
	fn finding_imports_by_kind_includes_imports() {
		let find = |extra: &[&str]| {
			let mut words = vec!["cargo-rscode", "find", "*"];

			words.extend(extra);
			FindArgs::from_matches(&parse(&words))
		};

		// Find only reports imports with `imports` set, so `-k import` alone would never find anything
		let args = find(&["-k", "import"]);

		assert_eq!(args.kinds, [ItemKind::Import]);
		assert!(args.imports);
		assert!(find(&["-k", "fn,import"]).imports);
		assert!(find(&["--imports"]).imports);
		assert!(!find(&["-k", "fn"]).imports);
		assert!(!find(&[]).imports);
	}

	#[test]
	fn fmt_arguments() {
		let args = FmtArgs::from_matches(
			&parse(&[
				"cargo-rscode",
				"fmt",
				"crate::a",
				"crate::b::**",
				"--formatter",
				"prettyplease",
				"--no-merge-extern-blocks",
				"--skip-children",
				"--check",
				"--edition",
				"2021",
				"--style-edition",
				"2024",
				"--config-path",
				"/cfg/rustfmt.toml",
				"--rustfmt-config",
				"max_width=80,hard_tabs=true",
				"--rustfmt-config",
				"edition=2018",
				"--allow-comment-loss",
				"--active-only",
			]),
			false,
		)
		.unwrap();

		assert_eq!(args.targets, ["crate::a", "crate::b::**"]);
		assert!(args.check);
		assert_eq!(args.emit, Emit::Diff);
		assert!(args.options.skip_children && args.options.active_only);
		assert_eq!(
			args.options.format,
			FormatOptions {
				formatter: RsFormatter::PrettyPlease,
				rustfmt: RustFmtOptions {
					program: None,
					edition: Some(Edition::E2021),
					style_edition: Some(Edition::E2024),
					config_path: Some(std::path::absolute("/cfg/rustfmt.toml").unwrap()),
					config: vec![
						("max_width".to_owned(), "80".to_owned()),
						("hard_tabs".to_owned(), "true".to_owned()),
						("edition".to_owned(), "2018".to_owned()),
					],
				},
				sort: Some(SortOptions::new().merge_extern_blocks(false).recursive(false)),
				allow_comment_loss: true,
			}
		);

		let args = FmtArgs::from_matches(&parse(&["cargo-rscode", "fmt", "--no-sort", "--formatter", "none"]), false).unwrap();

		assert_eq!(args.options.format.sort, None);
		assert_eq!(args.options.format.formatter, RsFormatter::None);
	}

	#[test]
	fn fmt_defaults() {
		let args = FmtArgs::from_matches(&parse(&["cargo-rscode", "fmt"]), false).unwrap();

		assert_eq!(args.targets, ["crate"]);
		assert_eq!(args.emit, Emit::Files);
		assert!(!args.check && !args.options.skip_children && !args.options.active_only);
		assert_eq!(args.options.format, FormatOptions::new().sort(Some(SortOptions::new())));
	}

	#[test]
	fn fmt_emit_modes() {
		let emit = |extra: &[&str]| {
			let mut words = vec!["cargo-rscode", "fmt"];

			words.extend(extra);
			FmtArgs::from_matches(&parse(&words), false).map(|args| args.emit)
		};

		assert_eq!(emit(&[]).unwrap(), Emit::Files);
		assert_eq!(emit(&["--check"]).unwrap(), Emit::Diff);
		assert_eq!(emit(&["--emit", "stdout"]).unwrap(), Emit::Stdout);
		assert_eq!(emit(&["--check", "--emit", "json"]).unwrap(), Emit::Json);
		assert_eq!(emit(&["--emit", "checkstyle"]).unwrap(), Emit::Checkstyle);
		assert_eq!(emit(&["--emit", "diff"]).unwrap(), Emit::Diff);
		assert!(emit(&["--check", "--emit", "files"]).is_err());
	}

	#[test]
	fn fmt_splits_config_between_cargo_and_rustfmt() {
		let fmt = |extra: &[&str]| {
			let mut words = vec!["cargo-rscode", "fmt"];

			words.extend(extra);
			FmtArgs::from_matches(&parse(&words), false)
		};
		let pair = |key: &str, value: &str| (key.to_owned(), value.to_owned());

		let args = fmt(&[
			"--config",
			"max_width=80,hard_tabs=true",
			"--config",
			"net.offline=true",
			"--rustfmt-config",
			"edition=2018",
			"--config",
			"/home/me/cargo.toml",
			"--config",
			"paths=[\"../dep\"]",
			"--config",
			"include=\"more.toml\"",
			"--config",
			" newline_style = Unix ",
		])
		.unwrap();

		assert_eq!(
			args.options.format.rustfmt.config,
			[
				pair("max_width", "80"),
				pair("hard_tabs", "true"),
				pair("newline_style", "Unix"),
				pair("edition", "2018")
			]
		);
		assert_eq!(
			args.cargo_config,
			["net.offline=true", "/home/me/cargo.toml", "paths=[\"../dep\"]", "include=\"more.toml\""]
		);

		let load = args.load_options(&parse(&["cargo-rscode", "fmt", "--config", "max_width=80"])).unwrap();

		assert_eq!(load.config, args.cargo_config);

		let error = fmt(&["--config", "max_width=80,oops"]).unwrap_err().to_string();

		assert_eq!(
			error,
			"invalid rustfmt setting `oops` in `--config max_width=80,oops`: expected KEY=VALUE"
		);

		// `sort` has no formatter: every value is cargo's
		let sort = FmtArgs::from_matches(&parse(&["cargo-rscode", "sort", "--config", "a=b"]), true).unwrap();

		assert_eq!(sort.cargo_config, ["a=b"]);
		assert!(sort.options.format.rustfmt.config.is_empty());
	}

	#[test]
	fn insert_arguments() {
		let args = InsertArgs::from_matches(&parse(&["cargo-rscode", "insert", "crate::m"])).unwrap();

		assert_eq!(args.parent, "crate::m");
		assert_eq!(args.source, SourceArg::Stdin);
		assert_eq!(args.options.position, InsertPosition::End);
		assert!(!args.options.force && !args.dry_run && !args.format);

		let position = |extra: &[&str]| {
			let mut words = vec!["cargo-rscode", "insert", "crate", "items.rs"];

			words.extend(extra);
			InsertArgs::from_matches(&parse(&words)).map(|args| args.options.position)
		};

		assert_eq!(position(&["--position", "start"]).unwrap(), InsertPosition::Start);
		assert_eq!(position(&["--position", "end"]).unwrap(), InsertPosition::End);
		assert_eq!(
			position(&["--position", "before", "--anchor", "crate::f"]).unwrap(),
			InsertPosition::Before("crate::f".to_owned())
		);
		assert_eq!(
			position(&["--position=after", "--anchor=crate::g"]).unwrap(),
			InsertPosition::After("crate::g".to_owned())
		);
		assert!(position(&["--anchor", "crate::f"]).is_err());
		assert!(position(&["--position", "start", "--anchor", "crate::f"]).is_err());

		let args = InsertArgs::from_matches(&parse(&["cargo-rscode", "insert", "<Foo as Bar>", "-", "--force", "-n", "--fmt"])).unwrap();

		assert_eq!(args.parent, "<Foo as Bar>");
		assert!(args.options.force && args.dry_run && args.format);
	}

	#[test]
	fn maps_every_load_option() {
		let matches = parse(&[
			"cargo-rscode",
			"find",
			"x",
			"--workspace",
			"--exclude",
			"a",
			"--exclude=b",
			"--features",
			"f1 f2,f3",
			"-F",
			"f1",
			"--all-features",
			"--no-default-features",
			"--exact-features",
			"--manifest-path",
			"/abs/Cargo.toml",
			"--target",
			"x86_64-pc-windows-msvc",
			"--cfg",
			"test",
			"--cfg",
			"feature=x",
			"--offline",
			"--locked",
			"--frozen",
			"--config",
			"net.retry=2",
			"-q",
		]);
		let options = load_options(&matches).unwrap();

		assert_eq!(
			options,
			LoadOptions {
				// made absolute (on Windows, `/abs` has no drive: it is `C:\abs` on drive `C:`)
				manifest_path: Some(std::path::absolute("/abs/Cargo.toml").unwrap()),
				packages: vec![],
				workspace: true,
				exclude: vec!["a".to_owned(), "b".to_owned()],
				targets: TargetSelection::default(),
				features: vec!["f1".to_owned(), "f2".to_owned(), "f3".to_owned()],
				all_features: true,
				no_default_features: true,
				target: Some("x86_64-pc-windows-msvc".to_owned()),
				cfgs: vec!["test".to_owned(), "feature=\"x\"".to_owned()],
				load_all_members: false,
				exact_features: true,
				offline: true,
				locked: true,
				frozen: true,
				config: vec!["net.retry=2".to_owned()],
				silent: true,
			}
		);
	}

	#[cfg(feature = "mcp")]
	#[test]
	fn maps_exposed_directories() {
		let options = server_options(&parse(&["cargo-rscode", "mcp", "--expose", "write=/abs/a", "--expose=read=/abs/refs/*"])).unwrap();
		let exposed: Vec<String> = options.exposed.iter().map(ToString::to_string).collect();

		assert_eq!(exposed, ["write=/abs/a", "read=/abs/refs/*"]);
		assert!(server_options(&parse(&["cargo-rscode", "mcp"])).unwrap().exposed.is_empty());

		for invalid in ["/abs/a", "execute=/abs/a", "read=/abs/a**"] {
			let error = crate::cli::cli()
				.try_get_matches_from(["cargo-rscode", "mcp", "--expose", invalid])
				.unwrap_err();

			assert!(error.to_string().contains("--expose <ACCESS=DIRS>"), "{invalid}: {error}");
		}
	}

	#[test]
	fn maps_target_selection() {
		let selection = target_selection(&parse(&[
			"cargo-rscode",
			"find",
			"x",
			"--lib",
			"--bin",
			"a",
			"--bin=b",
			"--bins",
			"--example",
			"e",
			"--examples",
			"--test",
			"t",
			"--tests",
			"--bench",
			"b",
			"--benches",
			"--all-targets",
		]));

		assert_eq!(
			selection,
			TargetSelection {
				lib: true,
				bins: vec!["a".to_owned(), "b".to_owned()],
				all_bins: true,
				examples: vec!["e".to_owned()],
				all_examples: true,
				tests: vec!["t".to_owned()],
				all_tests: true,
				benches: vec!["b".to_owned()],
				all_benches: true,
				all_targets: true,
			}
		);

		let selection = target_selection(&parse(&["cargo-rscode", "find", "x", "--tests"]));

		assert_eq!(
			selection,
			TargetSelection {
				all_tests: true,
				..TargetSelection::default()
			}
		);
	}

	#[cfg(feature = "mcp")]
	#[test]
	fn mcp_arguments() {
		let options = server_options(&parse(&["cargo-rscode", "mcp", "--read-only", "--workspace", "-F", "a,b"])).unwrap();

		assert!(options.read_only);
		assert!(options.load.workspace);
		assert_eq!(options.load.features, ["a", "b"]);
		assert!(!server_options(&parse(&["cargo-rscode", "mcp"])).unwrap().read_only);
	}

	#[test]
	fn output_options() {
		assert_eq!(OutputArgs::from_matches(&parse(&["cargo-rscode", "view", "x"])), OutputArgs::default());

		let output = OutputArgs::from_matches(&parse(&[
			"cargo-rscode",
			"find",
			"x",
			"--message-format",
			"File-Lines",
			"--absolute-paths",
		]));

		assert_eq!(output.format, MessageFormat::FileLines);
		assert!(output.absolute_paths);
		assert_eq!(
			OutputArgs::from_matches(&parse(&["cargo-rscode", "rename", "a", "b", "--message-format=json"])).format,
			MessageFormat::Json
		);
	}

	#[test]
	fn package_selection() {
		let options = load_options(&parse(&["cargo-rscode", "remove", "x", "-p", "a", "--package", "b@0.1.0"])).unwrap();

		assert_eq!(options.packages, ["a", "b@0.1.0"]);
		assert!(!options.workspace);

		let error = load_options(&parse(&["cargo-rscode", "remove", "x", "--exclude", "a"])).unwrap_err();

		assert!(
			error.to_string().contains("--exclude can only be used together with --workspace"),
			"{error}"
		);
	}

	/// The subcommand's matches of a command line.
	fn parse(words: &[&str]) -> ArgMatches {
		let matches = cli().try_get_matches_from(words).unwrap_or_else(|error| panic!("{words:?}: {error}"));

		matches.subcommand().unwrap().1.clone()
	}

	#[test]
	fn parses_show_fields() {
		let show = |extra: &[&str]| {
			let mut words = vec!["cargo-rscode", "find", "x"];

			words.extend(extra);
			FindArgs::from_matches(&parse(&words))
		};

		assert_eq!(show(&["--show", "span,cfg"]).show, [ShowField::Span, ShowField::Cfg]);
		assert_eq!(show(&["--show", "vis", "--show", "kind,vis"]).show, [ShowField::Vis, ShowField::Kind]);
		assert_eq!(
			show(&["--show", "location,all"]).show,
			[
				ShowField::Location,
				ShowField::Kind,
				ShowField::Span,
				ShowField::Vis,
				ShowField::Crate,
				ShowField::Cfg,
				ShowField::Usable,
			]
		);

		// `usable` implies `--from crate`, and `--from` implies `usable`
		let args = show(&["--show", "usable"]);

		assert_eq!(args.show, [ShowField::Usable]);
		assert_eq!(args.from, Some(FromArg::CrateRoot));

		let args = show(&["--from", "::"]);

		assert_eq!(args.show, [ShowField::Kind, ShowField::Location, ShowField::Usable]);
		assert_eq!(args.from, Some(FromArg::Foreign));
		assert_eq!(show(&["--from", "crate::a::b"]).from, Some(FromArg::Module("crate::a::b".to_owned())));
		assert_eq!(show(&["--from", "crate"]).from, Some(FromArg::CrateRoot));

		let error = cli()
			.try_get_matches_from(["cargo-rscode", "find", "x", "--show", "kind,colour"])
			.unwrap_err();

		assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
	}

	#[test]
	fn remove_arguments() {
		let args = RemoveArgs::from_matches(&parse(&["cargo-rscode", "remove", "a", "b"]));

		assert_eq!(args.paths, ["a", "b"]);
		assert!(!args.dry_run && !args.options.keep_files && !args.options.prune_imports && !args.options.active_only);

		let args = RemoveArgs::from_matches(&parse(&[
			"cargo-rscode",
			"remove",
			"a",
			"--keep-files",
			"--prune-imports",
			"--active-only",
			"--dry-run",
		]));

		assert!(args.dry_run && args.options.keep_files && args.options.prune_imports && args.options.active_only);
	}

	#[test]
	fn rename_arguments() {
		let args = RenameArgs::from_matches(&parse(&["cargo-rscode", "rename", "crate::Foo", "Bar"]));

		assert_eq!((args.path.as_str(), args.new_name.as_str()), ("crate::Foo", "Bar"));
		assert!(!args.dry_run && !args.options.force);
		assert_eq!(args.options.references, ReferenceOptions::default());

		let args = RenameArgs::from_matches(&parse(&[
			"cargo-rscode",
			"rename",
			"crate::Foo",
			"r#type",
			"--force",
			"-n",
			"--method-calls",
			"--macro-tokens",
			"--doc-links",
		]));

		assert_eq!(args.new_name, "r#type");
		assert!(args.dry_run && args.options.force);
		assert_eq!(
			args.options.references,
			ReferenceOptions {
				method_calls: true,
				macro_tokens: true,
				doc_links: true,
			}
		);
	}

	#[test]
	fn replace_arguments() {
		let args = ReplaceArgs::from_matches(&parse(&["cargo-rscode", "replace", "crate::f"]));

		assert_eq!(args.path, "crate::f");
		assert_eq!(args.source, SourceArg::Stdin);
		assert!(!args.dry_run && !args.format && !args.options.allow_kind_change && !args.options.all_variants);

		let args = ReplaceArgs::from_matches(&parse(&[
			"cargo-rscode",
			"replace",
			"crate::f",
			"new.rs",
			"--allow-kind-change",
			"--all-variants",
			"-n",
			"--fmt",
		]));

		assert_eq!(args.source, SourceArg::File(PathBuf::from("new.rs")));
		assert!(args.dry_run && args.format && args.options.allow_kind_change && args.options.all_variants);
		assert_eq!(
			ReplaceArgs::from_matches(&parse(&["cargo-rscode", "replace", "f", "-"])).source,
			SourceArg::Stdin
		);
	}

	#[test]
	fn sort_only_sorts() {
		let args = FmtArgs::from_matches(&parse(&["cargo-rscode", "sort", "crate::m", "--skip-children", "--emit", "stdout"]), true).unwrap();

		assert_eq!(args.targets, ["crate::m"]);
		assert_eq!(args.emit, Emit::Stdout);
		assert_eq!(
			args.options.format,
			FormatOptions::new()
				.formatter(RsFormatter::None)
				.sort(Some(SortOptions::new().schema(OrderingSchema::Cryotheum).recursive(false)))
		);
	}

	#[test]
	fn splits_features_like_cargo() {
		let split = |values: &[&str]| split_features(&values.iter().map(|value| value.to_string()).collect::<Vec<_>>());

		assert_eq!(split(&["a,b", "c d", " e ,, f ", "a"]), ["a", "b", "c", "d", "e", "f"]);
		assert_eq!(split(&["pkg/feat,other/x"]), ["pkg/feat", "other/x"]);
		assert!(split(&["", " , "]).is_empty());
	}

	#[test]
	fn target_triples() {
		let options = load_options(&parse(&["cargo-rscode", "view", "x", "--target", "wasm32-unknown-unknown"])).unwrap();

		assert_eq!(options.target.as_deref(), Some("wasm32-unknown-unknown"));

		let error = load_options(&parse(&["cargo-rscode", "view", "x", "--target"])).unwrap_err();

		assert!(error.to_string().contains("takes a target architecture"), "{error}");

		let error = load_options(&parse(&["cargo-rscode", "view", "x", "--target", "a", "--target", "b"])).unwrap_err();

		assert!(error.to_string().contains("only one `--target`"), "{error}");
	}

	#[test]
	fn tells_rustfmt_config_from_cargo_config() {
		assert!(is_rustfmt_config("max_width=80"));
		assert!(is_rustfmt_config("max_width = 80,hard_tabs=true"));
		assert!(!is_rustfmt_config("build.rustflags=[]"));
		assert!(!is_rustfmt_config("target.'cfg(unix)'.runner=\"x\""));
		assert!(!is_rustfmt_config("alias={}"));
		assert!(!is_rustfmt_config("=1"));
		assert!(!is_rustfmt_config("config.toml"));
		assert!(!is_rustfmt_config("weird=x.toml"));
		assert!(!is_rustfmt_config("/etc/cargo/config.toml"));
	}

	#[test]
	fn view_arguments() {
		let args = ViewArgs::from_matches(&parse(&["cargo-rscode", "view", "a", "b::c"]));

		assert_eq!(args.paths, ["a", "b::c"]);
		assert_eq!(args.options.mode, ViewMode::Auto);
		assert!(args.options.docs && !args.options.line_numbers && !args.options.impls && !args.options.active_only);

		let args = ViewArgs::from_matches(&parse(&[
			"cargo-rscode",
			"view",
			"a",
			"--outline",
			"--no-docs",
			"-n",
			"--impls",
			"--active-only",
		]));

		assert_eq!(args.options.mode, ViewMode::Outline);
		assert!(!args.options.docs && args.options.line_numbers && args.options.impls && args.options.active_only);
		assert_eq!(
			ViewArgs::from_matches(&parse(&["cargo-rscode", "view", "a", "--full"])).options.mode,
			ViewMode::Full
		);
	}
}
