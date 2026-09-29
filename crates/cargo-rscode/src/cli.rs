//! The clap command tree.
//!
//! One tree serves both invocation forms: cargo runs `cargo-rscode rscode <args>` for `cargo rscode <args>`, and
//! [`normalize_args`] drops that `rscode` word, so the subcommands sit at the top level either way.
//! [`cli`] must not set a `bin_name`: it is also the [`clap_complete::CompleteEnv`] factory, and a bin name of
//! `cargo rscode` would register the completion for `cargo` itself.

use crate::complete;
use cargo::util::command_prelude::CommandExt as _;
use cargo::util::command_prelude::flag;
use cargo::util::command_prelude::heading;
use cargo::util::command_prelude::multi_opt;
use cargo::util::command_prelude::opt;
use clap::Arg;
use clap::ArgAction;
use clap::ArgGroup;
use clap::ColorChoice;
use clap::Command;
use clap::ValueHint;
use clap::builder::EnumValueParser;
use clap::builder::PossibleValue;
use clap::builder::PossibleValuesParser;
use clap::builder::RangedU64ValueParser;
use clap::builder::TypedValueParser;
use clap::builder::styling::Styles;
use clap::error::ContextKind;
use clap::error::ContextValue;
use clap_complete::ArgValueCandidates;
use clap_complete::ArgValueCompleter;
use rscode::CfgExpr;
use rscode::Edition;
use rscode::ItemKind;
use rscode::rscode_fmt::RsFormatter;
use rscode::rscode_sort::OrderingSchema;
use std::ffi::OsStr;
use std::ffi::OsString;

/// The executable's name, which is also the command the shell completion is registered for.
pub(crate) const BIN_NAME: &str = "cargo-rscode";

/// The word cargo inserts as `argv[1]` when invoked as `cargo rscode ...`.
pub(crate) const CARGO_SUBCOMMAND: &str = "rscode";

/// `--message-format` values of `find`.
pub(crate) const FIND_FORMATS: &[&str] = &["human", "json", "file-lines"];

/// `--message-format` values of every other subcommand.
pub(crate) const FORMATS: &[&str] = &["human", "json"];

/// `find --show` fields.
pub(crate) const SHOW_FIELDS: &[&str] = &["kind", "location", "span", "cfg", "vis", "crate", "usable", "all"];

/// `fmt --emit` modes (like rustfmt's).
pub(crate) const EMIT_MODES: &[&str] = &["files", "stdout", "diff", "json", "checkstyle"];

/// `insert --position` values.
pub(crate) const POSITIONS: &[&str] = &["start", "end", "before", "after"];

/// Same help colors as cargo itself.
const STYLES: Styles = {
	use cargo::util::style;

	Styles::styled()
		.header(style::HEADER)
		.usage(style::USAGE)
		.literal(style::LITERAL)
		.placeholder(style::PLACEHOLDER)
		.error(style::ERROR)
		.valid(style::VALID)
		.invalid(style::INVALID)
};

const ABOUT: &str = "View, search, format, and edit Rust source by item path";

const LONG_ABOUT: &str = "\
View, search, format, and edit Rust source by item path.

Crates are loaded from the cargo workspace (like `cargo check`, selected with -p/--workspace and the target flags) \
and parsed without being compiled or macro-expanded. Items behind `cfg`s that are disabled for the selected features \
and target are still found; they are marked inactive.

Item paths are written like Rust paths:
  crate::m::Item         an item of the selected crates
  ::krate::Item          an item of the crate `krate`
  m::Item                presumed absolute: crate::m::Item, or ::m::Item
  Type::name             an associated item, a trait item, or a variant
  <Type as Trait>::name  an item of a trait impl (<Type>::name: inherent)
  impl Trait for Type    an impl block (also <Type as Trait>, <Type>)
  'use m::Name'          the imports binding Name in m (use m::*: globs, use m::_: `as _` ones)
Generic arguments of the type and trait pick impl blocks: impl From<u8> for W, <W<u16>>::get.
Every `cfg` variant of an item is addressed by its path. Paths go through imports to what they import, except `use` \
paths (quoted as one argument), which `find --imports` prints; `remove` and `replace` refuse a path whose last segment \
is bound only by a private import, which could mean either.";

const COMPLETION_HELP: &str = "\
Run `cargo rscode --help` for how to set up shell completion.";

const COMPLETION_LONG_HELP: &str = "\
Shell completion completes subcommands, options, item paths, packages, and item kinds dynamically:
  bash  add to ~/.bashrc:
          source <(COMPLETE=bash cargo-rscode)
        or, to load it lazily with bash-completion, put that line into
          ~/.local/share/bash-completion/completions/cargo-rscode
  zsh   add to ~/.zshrc, after `compinit`:
          source <(COMPLETE=zsh cargo-rscode)
  fish  put into ~/.config/fish/conf.d/cargo-rscode.fish:
          COMPLETE=fish cargo-rscode | source
Both `cargo-rscode <TAB>` and `cargo rscode <TAB>` are completed; in bash the latter needs rustup's cargo \
completion and bash-completion 2.12 or newer. The script calls back into cargo-rscode on every TAB, so regenerate \
it on shell startup (as above) rather than saving its output.";

const FIND_LONG_ABOUT: &str = "\
Find items by name or path pattern, and print their paths and locations.

Patterns are item paths with wildcards (no regex):
  Foo, Foo*, *Foo, *Foo*, F*o  names: exact, prefix, suffix, infix, ...
  a::*                         the items directly in `a`
  a::**                        every item below `a` (also `a**`)
  **::b, a::**::b              `b` at any depth (below `a`)
  crate::a::b, ::krate::a      anchored at the selected crates, or `krate`
  <Type as Trait>::name        items of matching trait impls
  'use a::*', 'use Foo'        imports: every import in `a`, the imports binding `Foo`
Patterns without an anchor match anywhere: `Foo` finds every item named `Foo`, and `m::*Error` every `...Error` \
item directly in a module `m`. Imports are found by `use` patterns, `--imports`, or `-k import`, and printed as the \
`use` paths that name them (`use my_crate::a::Foo`).";

const FIND_AFTER_HELP: &str = "\
`--message-format file-lines` prints rustfmt's `--file-lines` JSON, to format only the found items:
  lines=$(cargo rscode find 'crate::ast::**' --message-format file-lines)
  rustfmt +nightly --unstable-features --file-lines \"$lines\" src/lib.rs";

#[cfg(feature = "mcp")]
const MCP_LONG_ABOUT: &str = "\
Serve rscode's operations (workspace info, find, view, rename, remove, replace, insert, format) as tools over the \
Model Context Protocol, on stdin and stdout.

The workspace options given here are the server's defaults (tools can narrow the package selection per call). The \
workspace is loaded anew for every tool call, so changes made by other tools are always seen, and every edit is \
validated to still parse before anything is written.

With --expose, clients can also attach other workspaces and packages at runtime, by the path of their Cargo.toml and \
a name of their choice, and then work on them by that name. Names are only known to the client that attached them.";

#[cfg(feature = "mcp")]
const MCP_AFTER_HELP: &str = "\
Registering the server with MCP clients, which start it themselves (prefer absolute paths: GUI apps often lack cargo \
in their PATH):

  Claude Code:
    claude mcp add rscode -- cargo rscode mcp --manifest-path /abs/path/to/Cargo.toml
    claude mcp add -s project rscode -- cargo-rscode mcp    (shared through ./.mcp.json)

  .mcp.json, Claude Desktop's claude_desktop_config.json, or Cursor's .cursor/mcp.json:
    {
      \"mcpServers\": {
        \"rscode\": {
          \"command\": \"cargo\",
          \"args\": [\"rscode\", \"mcp\", \"--manifest-path\", \"/abs/path/to/Cargo.toml\"]
        }
      }
    }

Running cargo-rscode itself (\"command\": \"/home/me/.cargo/bin/cargo-rscode\", \"args\": [\"mcp\"]) skips cargo \
and rustup. Without --manifest-path, the workspace is found from the server's working directory (for Claude Code, \
the project directory).

One server for several workspaces: the project's own, one more to edit, and references to read:
    \"args\": [\"rscode\", \"mcp\",
      \"--expose\", \"write=/abs/path/engine\",
      \"--expose\", \"read=/abs/path/references/*\",
      \"--expose\", \"read=/abs/path/references/misc/**\"]";

#[cfg(feature = "mcp")]
const EXPOSE_LONG_HELP: &str = "\
Let clients attach the cargo workspaces and packages whose Cargo.toml is in a directory matching DIRS, a glob pattern \
of directories, with ACCESS `read` or `write`. `*` matches within one path component and `**` any number of them: \
`/refs/*` matches /refs/log but not /refs/misc/log, and `/refs/misc/**` matches /refs/misc and every directory below \
it. Wildcards do not match hidden directories, and symbolic links are resolved.

Sources attached for writing can be edited, but their edits are only ever written below directories matched by \
`write` patterns (which take precedence over `read` patterns). The server's own workspace is not restricted. Can be \
given several times.";

/// The command tree (without a `bin_name`; see the module docs).
pub(crate) fn cli() -> Command {
	let command = Command::new(BIN_NAME)
		.version(env!("CARGO_PKG_VERSION"))
		.about(ABOUT)
		.long_about(LONG_ABOUT)
		.after_help(COMPLETION_HELP)
		.after_long_help(COMPLETION_LONG_HELP)
		.styles(STYLES)
		.subcommand_required(true)
		.arg_required_else_help(true)
		.subcommands([find(), view(), fmt(), sort(), rename(), remove(), replace(), insert()]);

	#[cfg(feature = "mcp")]
	let command = command.subcommand(mcp());

	command
}

/// Drops the `rscode` word cargo puts at `argv[1]` for `cargo rscode <args>`, returning whether it was there.
///
/// This is unambiguous because the top-level command has no positional arguments and no `rscode` subcommand.
pub(crate) fn normalize_args(mut args: Vec<OsString>) -> (Vec<OsString>, bool) {
	let via_cargo = args.get(1).is_some_and(|arg| arg == CARGO_SUBCOMMAND);

	if via_cargo {
		args.remove(1);
	}

	(args, via_cargo)
}

/// The `--color` choice among the arguments, so clap's own help and errors honor it too.
pub(crate) fn color_choice(args: &[OsString]) -> ColorChoice {
	let mut value = None;
	let mut args = args.iter().filter_map(|arg| arg.to_str());

	while let Some(arg) = args.next() {
		match arg {
			"--" => break,
			"--color" => value = args.next(),
			_ => {
				if let Some(inline) = arg.strip_prefix("--color=") {
					value = Some(inline);
				}
			}
		}
	}

	match value.map(str::to_ascii_lowercase).as_deref() {
		Some("always") => ColorChoice::Always,
		Some("never") => ColorChoice::Never,
		_ => ColorChoice::Auto,
	}
}

fn find() -> Command {
	let command = Command::new("find")
		.about("Find items by name or path pattern")
		.long_about(FIND_LONG_ABOUT)
		.after_help(FIND_AFTER_HELP)
		.arg(
			Arg::new("patterns")
				.value_name("PATTERN")
				.help("Item path patterns (an item matching any of them is printed)")
				.num_args(1..)
				.action(ArgAction::Append)
				.add(ArgValueCompleter::new(complete::item_paths)),
		)
		.arg(
			opt("kind", "Only items of these kinds (repeatable, comma-separated; `import` implies --imports)")
				.short('k')
				.value_name("KIND")
				.action(ArgAction::Append)
				.value_delimiter(',')
				.value_parser(ItemKindParser)
				.add(ArgValueCandidates::new(complete::kind_candidates)),
		)
		.arg(flag("ignore-case", "Match identifiers case-insensitively").short('i'))
		.arg(multi_opt("contains", "TEXT", "Also find items whose name contains TEXT"))
		.arg(multi_opt("starts-with", "TEXT", "Also find items whose name starts with TEXT"))
		.arg(multi_opt("ends-with", "TEXT", "Also find items whose name ends with TEXT"))
		.group(
			ArgGroup::new("pattern-sources")
				.args(["patterns", "contains", "starts-with", "ends-with"])
				.multiple(true)
				.required(true),
		)
		.arg(flag("active-only", "Skip items whose `cfg` is disabled"))
		.arg(flag("imports", "Also find `use` imports (as `use` paths, with what they import)"))
		.arg(
			opt(
				"from",
				"Show the paths through which items can be named from `crate` (each item's crate root), `::` \
				 (another crate), or a module",
			)
			.value_name("crate|::|MODULE")
			.add(ArgValueCompleter::new(complete::from_paths)),
		)
		.arg(
			opt("show", "Fields to show after each path [default: kind,location]")
				.value_name("FIELDS")
				.action(ArgAction::Append)
				.value_delimiter(',')
				.value_parser(PossibleValuesParser::new(SHOW_FIELDS.iter().copied())),
		)
		.arg(
			opt("limit", "Show at most N items")
				.value_name("N")
				.value_parser(RangedU64ValueParser::<usize>::new().range(1..)),
		);

	output_args(load_args(command), FIND_FORMATS)
}

fn view() -> Command {
	let command = Command::new("view")
		.about("Print the source (or an outline) of items")
		.long_about(
			"Print the source of items. Modules (and the crate root, `crate`) are shown as outlines: their items with \
			 function and macro bodies elided (but for the statics of `thread_local!`, which are items). Imports \
			 (`'use crate::a::Name'`) are shown as their `use` items. Every `cfg` variant of a path is shown.",
		)
		.arg(item_paths("paths", "PATH", "Item paths to view"))
		.arg(flag("outline", "Show outlines (bodies elided) of every item").conflicts_with("full"))
		.arg(flag("full", "Show the full source of every item (for out-of-line modules, their files)"))
		.arg(flag("no-docs", "Leave out doc comments"))
		.arg(flag("line-numbers", "Prefix lines with their line numbers").short('n'))
		.arg(flag("impls", "Also show the `impl` blocks of types and traits"))
		.arg(flag("active-only", "Skip `cfg` variants that are disabled"));

	output_args(load_args(command), FORMATS)
}

fn fmt() -> Command {
	let command = Command::new("fmt")
		.about("Sort and format items")
		.long_about(
			"Sort (with the Cryotheum ordering schema) and format items. A module target formats its file(s), including \
			 child modules unless --skip-children; other items are formatted in place, leaving the rest of their file \
			 untouched.",
		)
		.arg(targets())
		.arg(
			opt("formatter", "The formatter to run")
				.value_name("FORMATTER")
				.value_parser(EnumValueParser::<RsFormatter>::new())
				.default_value("rustfmt"),
		)
		.arg(
			flag("no-sort", "Only format, do not sort items").conflicts_with_all(["schema", "no-merge-extern-blocks"]),
		);

	let command = check_args(sort_args(command))
		.arg(
			opt("edition", "Rust edition for rustfmt [default: each crate's edition]")
				.value_name("EDITION")
				.value_parser(EnumValueParser::<Edition>::new()),
		)
		.arg(
			opt("style-edition", "rustfmt's style edition")
				.value_name("EDITION")
				.value_parser(EnumValueParser::<Edition>::new()),
		)
		.arg(
			opt("config-path", "rustfmt config file, or a directory to search from [default: each file's directory]")
				.value_name("PATH")
				.value_hint(ValueHint::AnyPath),
		)
		.arg(
			multi_opt("rustfmt-config", "KEY=VALUE", "Override rustfmt configuration values (comma-separated)")
				.value_delimiter(',')
				.value_parser(parse_key_value),
		)
		.arg(flag("allow-comment-loss", "Let prettyplease drop comments of formatted items"))
		.arg(flag("active-only", "Skip `cfg` variants that are disabled"));

	output_args(load_args(command), FORMATS).mut_arg("config", |config| {
		config.value_name("KEY=VALUE|PATH").help(
			"Override a cargo configuration value (`net.offline=true`, or a file), or rustfmt's \
			 (`max_width=80,hard_tabs=true`, like --rustfmt-config)",
		)
	})
}

fn sort() -> Command {
	let command = Command::new("sort")
		.about("Sort items (like `fmt --formatter none`)")
		.long_about(
			"Sort items with the Cryotheum ordering schema without formatting them: the items of module targets, and \
			 of the `impl` blocks, traits, and `extern` blocks among them (and in child modules, unless \
			 --skip-children).",
		)
		.arg(targets());

	let command = check_args(sort_args(command)).arg(flag("active-only", "Skip `cfg` variants that are disabled"));

	output_args(load_args(command), FORMATS)
}

fn rename() -> Command {
	let command = Command::new("rename")
		.about("Rename an item and update its references across the workspace")
		.long_about(
			"Rename an item (every `cfg` variant, and for trait items the items of every impl) and update the \
			 references to it in every crate of the workspace. Out-of-line modules have their files moved. Renaming is \
			 refused when the new name collides with an existing one, unless --force.",
		)
		.arg(item_path("path", "PATH", "The item to rename"))
		.arg(Arg::new("new-name").value_name("NEW_NAME").help("The new identifier").required(true))
		.arg(flag("force", "Rename even when the new name collides with existing names"))
		.arg(dry_run())
		.arg(flag(
			"method-calls",
			"Also rename method calls (`x.name()`) of renamed methods, which cannot be resolved without types, and \
			 `T::name` paths through generic parameters whose bounds do not tell (such as through supertraits)",
		))
		.arg(flag(
			"macro-tokens",
			"Also rename matching identifiers in macro bodies that are not expressions (and in `macro_rules!` \
			 transcribers) outside of the paths resolved there",
		))
		.arg(flag("doc-links", "Also update intra-doc links (``[`Name`]``)"));

	output_args(load_args(command), FORMATS)
}

fn remove() -> Command {
	let command = Command::new("remove")
		.about("Remove items")
		.long_about(
			"Remove items (every `cfg` variant) with their attributes, doc comments, and attached comments. Removing an \
			 out-of-line module also deletes its files. Imports are removed by their `use` paths \
			 (`'use crate::a::Name'`), leaving the rest of their `use` items; a plain path whose last segment is bound \
			 only by a private import is refused, as it could name the import or what it imports.",
		)
		.arg(item_paths("paths", "PATH", "Items to remove"))
		.arg(flag("keep-files", "Keep the files of removed out-of-line modules"))
		.arg(flag("prune-imports", "Also remove `use` imports of the removed items"))
		.arg(flag("active-only", "Only remove `cfg` variants that are not disabled"))
		.arg(dry_run());

	output_args(load_args(command), FORMATS)
}

fn replace() -> Command {
	let command = Command::new("replace")
		.about("Replace the source of an item")
		.long_about(
			"Replace the source of an item (including its attributes and doc comments) with new source, which must \
			 parse as an item of the same kind. The new source is re-indented to the item's indentation. An import \
			 (`'use crate::a::Name'`) is replaced as its `use` item, which must import nothing else; a plain path whose \
			 last segment is bound only by a private import is refused, as it could name the import or what it imports.",
		)
		.arg(item_path("path", "PATH", "The item to replace"))
		.arg(source_arg("The new source: a file, or `-` for stdin"))
		.arg(flag("allow-kind-change", "Allow a different kind of item, or several items"))
		.arg(flag("all-variants", "Replace every `cfg` variant instead of failing when there are several"))
		.arg(dry_run())
		.arg(flag("fmt", "Format the replaced item with rustfmt afterwards"));

	output_args(load_args(command), FORMATS)
}

fn insert() -> Command {
	let command = Command::new("insert")
		.about("Insert items into a module, impl block, or trait")
		.long_about(
			"Insert items into a module (`crate` for the crate root), an impl block (`<Type as Trait>`, `<Type>`), or a \
			 trait. The items must be valid there, their names must not be taken (unless --force), and they are \
			 indented like the container's items.",
		)
		.arg(item_path(
			"parent",
			"PARENT",
			"The container: a module (`crate` for the crate root), an impl block (`<Type as Trait>`), or a trait",
		))
		.arg(source_arg("The items to insert: a file, or `-` for stdin"))
		.arg(
			opt("position", "Where to insert the items")
				.value_name("POSITION")
				.value_parser(PossibleValuesParser::new(POSITIONS.iter().copied()))
				.default_value("end"),
		)
		.arg(
			opt("anchor", "The sibling item to insert before or after (an import stands for its `use` item)")
				.value_name("PATH")
				.required_if_eq_any([("position", "before"), ("position", "after")])
				.add(ArgValueCompleter::new(complete::item_paths)),
		)
		.arg(flag("force", "Insert even when a name is already taken in the container"))
		.arg(dry_run())
		.arg(flag("fmt", "Format the inserted items with rustfmt afterwards"));

	output_args(load_args(command), FORMATS)
}

#[cfg(feature = "mcp")]
fn mcp() -> Command {
	let command = Command::new("mcp")
		.about("Serve rscode over the Model Context Protocol (stdio)")
		.long_about(MCP_LONG_ABOUT)
		.after_help(MCP_AFTER_HELP)
		.arg(flag("read-only", "Do not offer tools that modify files"))
		.arg(
			Arg::new("expose")
				.long("expose")
				.value_name("ACCESS=DIRS")
				.action(ArgAction::Append)
				.value_parser(|value: &str| value.parse::<rscode::mcp::Exposure>())
				.value_hint(ValueHint::Other)
				.help("Let clients attach the workspaces and packages in directories matching a glob (ACCESS: read or write)")
				.long_help(EXPOSE_LONG_HELP),
		);

	load_args(command)
}

/// Options selecting and loading the workspace (cargo's flags, using its own helpers and argument ids), and cargo's
/// global flags, which cargo does not forward when they precede `rscode`.
fn load_args(command: Command) -> Command {
	command
		.arg_package_spec_no_all(
			"Package(s) to operate on (see `cargo help pkgid`)",
			"Operate on all packages in the workspace",
			"Exclude packages (with --workspace)",
			ArgValueCandidates::new(complete::package_candidates),
		)
		.arg_targets_all(
			"Load the package's library",
			"Load the specified binary",
			"Load all binaries",
			"Load the specified example",
			"Load all examples",
			"Load the specified test target",
			"Load all test targets",
			"Load the specified bench target",
			"Load all bench targets",
			"Load all targets",
		)
		.arg_features()
		.arg(
			flag("exact-features", "Resolve features with cargo's resolver (slower, may need the registry index)")
				.help_heading(heading::FEATURE_SELECTION),
		)
		.arg_target_triple("Evaluate `cfg`s for the target triple [default: the host]")
		.arg(
			multi_opt("cfg", "SPEC", "Enable a `cfg` (`name` or `key=\"value\"`) when evaluating `cfg` attributes")
				.value_parser(parse_cfg)
				.help_heading(heading::COMPILATION_OPTIONS),
		)
		.arg_manifest_path()
		.arg(flag("locked", "Assert that `Cargo.lock` will remain unchanged").help_heading(heading::MANIFEST_OPTIONS))
		.arg(flag("offline", "Run without accessing the network").help_heading(heading::MANIFEST_OPTIONS))
		.arg(
			flag("frozen", "Equivalent to specifying both --locked and --offline")
				.help_heading(heading::MANIFEST_OPTIONS),
		)
		// Exactly like cargo's: `ArgMatchesExt::verbose` reads a `u8` count and panics otherwise.
		.arg(
			opt("verbose", "Use verbose output (also print load warnings)")
				.short('v')
				.action(ArgAction::Count)
				.conflicts_with("quiet"),
		)
		.arg(flag("quiet", "Do not print warnings, notes, or status messages").short('q'))
		.arg(opt("color", "Coloring").value_name("WHEN").value_parser(["auto", "always", "never"]).ignore_case(true))
		.arg(multi_opt("config", "KEY=VALUE|PATH", "Override a cargo configuration value"))
		.arg_silent_suggestion()
}

/// How results are printed.
fn output_args(command: Command, formats: &'static [&'static str]) -> Command {
	command
		.arg(
			opt("message-format", "The output format")
				.value_name("FMT")
				.value_parser(PossibleValuesParser::new(formats.iter().copied()))
				.ignore_case(true)
				.default_value("human"),
		)
		.arg(flag(
			"absolute-paths",
			"Print absolute file paths [default: relative to the current directory when below it]",
		))
}

/// `--schema`, `--no-merge-extern-blocks`, and `--skip-children`.
fn sort_args(command: Command) -> Command {
	command
		.arg(
			opt("schema", "The ordering schema")
				.value_name("SCHEMA")
				.value_parser(EnumValueParser::<OrderingSchema>::new())
				.default_value("cryotheum"),
		)
		.arg(flag("no-merge-extern-blocks", "Do not merge sibling `extern` blocks with the same ABI and attributes"))
		.arg(flag("skip-children", "Do not process child modules of module targets"))
}

/// `--check` and `--emit`.
fn check_args(command: Command) -> Command {
	command
		.arg(flag(
			"check",
			"Do not write; exit with 1 when anything would change (shows a diff unless --emit is given)",
		))
		.arg(
			opt("emit", "What to do with the results [default: files, or diff with --check]")
				.value_name("MODE")
				.value_parser(PossibleValuesParser::new(EMIT_MODES.iter().copied())),
		)
}

fn targets() -> Arg {
	Arg::new("targets")
		.value_name("TARGET")
		.help("Path patterns of the items or modules to process (globs allowed: 'use a::*' is every import in `a`)")
		.num_args(1..)
		.action(ArgAction::Append)
		.default_value("crate")
		.add(ArgValueCompleter::new(complete::item_paths))
}

fn item_path(id: &'static str, value_name: &'static str, help: &'static str) -> Arg {
	Arg::new(id).value_name(value_name).help(help).required(true).add(ArgValueCompleter::new(complete::item_paths))
}

fn item_paths(id: &'static str, value_name: &'static str, help: &'static str) -> Arg {
	item_path(id, value_name, help).num_args(1..).action(ArgAction::Append)
}

fn source_arg(help: &'static str) -> Arg {
	Arg::new("source").value_name("SOURCE").help(help).default_value("-").value_hint(ValueHint::FilePath)
}

fn dry_run() -> Arg {
	flag("dry-run", "Print the changes as a diff instead of writing them").short('n')
}

/// Accepts `name` and `key="value"` (and, for convenience, `key=value`), as `--cfg` of rustc does.
fn parse_cfg(spec: &str) -> Result<String, String> {
	// rustc refuses `_` as a name
	let is_single = |spec: &str| match CfgExpr::parse(spec) {
		Ok(CfgExpr::Name(name) | CfgExpr::KeyValue(name, _)) => name != "_",
		_ => false,
	};

	if is_single(spec) {
		return Ok(spec.to_owned());
	}

	if let Some((key, value)) = spec.split_once('=')
		&& !value.contains('"')
	{
		let quoted = format!("{}=\"{}\"", key.trim(), value.trim().replace('\\', "\\\\"));

		if is_single(&quoted) {
			return Ok(quoted);
		}
	}

	Err("expected a `cfg` name or `key=\"value\"`".to_owned())
}

/// A `KEY=VALUE` pair.
pub(crate) fn parse_key_value(pair: &str) -> Result<(String, String), String> {
	match pair.split_once('=') {
		Some((key, value)) if !key.trim().is_empty() => Ok((key.trim().to_owned(), value.trim().to_owned())),
		_ => Err("expected KEY=VALUE".to_owned()),
	}
}

/// The kinds of items that `find` can find: those that patterns match, which excludes the unnamed items but `impl`
/// blocks (`<Type as Trait>`).
pub(crate) fn find_kinds() -> impl Iterator<Item = ItemKind> {
	ItemKind::ALL.iter().copied().filter(|&kind| kind.is_nameable() || kind == ItemKind::Impl)
}

/// Why `find` never finds items of a kind.
fn not_found_because(kind: ItemKind) -> &'static str {
	match kind {
		ItemKind::Use => "`use` declarations are not found as a whole; `--kind import` finds their imports",
		ItemKind::ExternBlock => "`extern` blocks have no names; their items are found in the enclosing module",
		_ => "macro invocations have no names, so no pattern matches them",
	}
}

/// Parses the item kinds of `find --kind` by their names ([`ItemKind::name`]) and aliases (`module`, `function`,
/// `method`, ...). Kinds that `find` never finds are refused with the reason (see [`find_kinds`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ItemKindParser;

impl TypedValueParser for ItemKindParser {
	type Value = ItemKind;

	fn parse_ref(&self, command: &Command, arg: Option<&Arg>, value: &OsStr) -> Result<ItemKind, clap::Error> {
		let kind = value.to_str().and_then(|text| text.parse::<ItemKind>().ok());

		if let Some(kind) = kind.filter(|kind| find_kinds().any(|findable| findable == *kind)) {
			return Ok(kind);
		}

		// clap's possible-value parser produces its usual error (with suggestions)
		let names = PossibleValuesParser::new(find_kinds().map(ItemKind::name));
		let mut error = match names.parse_ref(command, arg, value) {
			Err(error) => error,
			Ok(_) => clap::Error::new(clap::error::ErrorKind::InvalidValue).with_cmd(command),
		};

		if let Some(kind) = kind {
			error.insert(ContextKind::Suggested, ContextValue::StyledStrs(vec![not_found_because(kind).into()]));
		}

		Err(error)
	}

	fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
		Some(Box::new(find_kinds().map(|kind| PossibleValue::new(kind.name()))))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn args(words: &[&str]) -> Vec<OsString> {
		words.iter().map(OsString::from).collect()
	}

	#[test]
	fn command_tree_is_valid() {
		cli().debug_assert();
	}

	#[test]
	fn completion_registers_for_the_binary() {
		// a bin name of `cargo rscode` would register the completion for `cargo` itself
		assert_eq!(cli().get_bin_name(), None);
		assert_eq!(cli().get_name(), BIN_NAME);
	}

	#[test]
	fn normalizes_both_invocation_forms() {
		let (direct, via_cargo) = normalize_args(args(&["cargo-rscode", "find", "x"]));

		assert!(!via_cargo);
		assert_eq!(direct, args(&["cargo-rscode", "find", "x"]));

		let (forwarded, via_cargo) = normalize_args(args(&["/bin/cargo-rscode", "rscode", "find", "x"]));

		assert!(via_cargo);
		assert_eq!(forwarded, args(&["/bin/cargo-rscode", "find", "x"]));

		// only the word right after the binary is cargo's
		let (later, via_cargo) = normalize_args(args(&["cargo-rscode", "find", "rscode"]));

		assert!(!via_cargo);
		assert_eq!(later, args(&["cargo-rscode", "find", "rscode"]));
		assert_eq!(normalize_args(args(&["cargo-rscode"])), (args(&["cargo-rscode"]), false));
	}

	#[test]
	fn both_forms_parse_the_same() {
		for words in
			[&["cargo-rscode", "find", "Foo", "-p", "x"][..], &["cargo-rscode", "rscode", "find", "Foo", "-p", "x"]]
		{
			let (words, _) = normalize_args(args(words));
			let matches = cli().try_get_matches_from(words).unwrap();
			let (name, sub) = matches.subcommand().unwrap();

			assert_eq!(name, "find");
			assert_eq!(sub.get_many::<String>("patterns").unwrap().collect::<Vec<_>>(), ["Foo"]);
			assert_eq!(sub.get_many::<String>("package").unwrap().collect::<Vec<_>>(), ["x"]);
		}
	}

	#[test]
	fn usage_names_the_invocation() {
		let mut direct = cli();

		direct.build();

		let help = direct.find_subcommand_mut("find").unwrap().render_usage().to_string();

		assert!(help.starts_with("Usage: cargo-rscode find"), "{help}");

		let mut via_cargo = cli().bin_name("cargo rscode");

		via_cargo.build();

		let help = via_cargo.find_subcommand_mut("find").unwrap().render_usage().to_string();

		assert!(help.starts_with("Usage: cargo rscode find"), "{help}");
	}

	#[test]
	fn requires_a_subcommand() {
		let error = cli().try_get_matches_from(["cargo-rscode"]).unwrap_err();

		assert_eq!(error.exit_code(), 2);

		let error = cli().try_get_matches_from(["cargo-rscode", "fnd"]).unwrap_err();

		assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
		assert_eq!(error.exit_code(), 2);
	}

	#[test]
	fn every_subcommand_has_the_load_options() {
		for subcommand in cli().get_subcommands() {
			for id in [
				"package",
				"workspace",
				"exclude",
				"features",
				"all-features",
				"no-default-features",
				"manifest-path",
				"lib",
				"bin",
				"bins",
				"example",
				"examples",
				"test",
				"tests",
				"bench",
				"benches",
				"all-targets",
				"target",
				"cfg",
				"verbose",
				"quiet",
				"color",
				"offline",
				"locked",
				"frozen",
				"config",
			] {
				assert!(
					subcommand.get_arguments().any(|arg| arg.get_id() == id),
					"`{}` lacks `{id}`",
					subcommand.get_name()
				);
			}

			let has_output = subcommand.get_arguments().any(|arg| arg.get_id() == "message-format");

			assert_eq!(has_output, subcommand.get_name() != "mcp", "{}", subcommand.get_name());
		}
	}

	#[test]
	fn verbose_counts_and_conflicts_with_quiet() {
		let matches = cli().try_get_matches_from(["cargo-rscode", "view", "x", "-vv"]).unwrap();
		let (_, sub) = matches.subcommand().unwrap();

		assert_eq!(sub.get_count("verbose"), 2);

		let error = cli().try_get_matches_from(["cargo-rscode", "view", "x", "-v", "-q"]).unwrap_err();

		assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
	}

	#[test]
	fn find_needs_a_pattern_of_some_sort() {
		let error = cli().try_get_matches_from(["cargo-rscode", "find"]).unwrap_err();

		assert_eq!(error.kind(), clap::error::ErrorKind::MissingRequiredArgument);

		for words in [
			&["cargo-rscode", "find", "Foo"][..],
			&["cargo-rscode", "find", "--contains", "oo"],
			&["cargo-rscode", "find", "--starts-with", "F"],
			&["cargo-rscode", "find", "--ends-with", "o", "--contains", "x"],
		] {
			assert!(cli().try_get_matches_from(words).is_ok(), "{words:?}");
		}
	}

	#[test]
	fn conflicting_flags_are_refused() {
		for words in [
			&["cargo-rscode", "view", "x", "--outline", "--full"][..],
			&["cargo-rscode", "fmt", "--no-sort", "--schema", "cryotheum"],
			&["cargo-rscode", "fmt", "--no-sort", "--no-merge-extern-blocks"],
		] {
			let error = cli().try_get_matches_from(words).unwrap_err();

			assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict, "{words:?}");
		}
	}

	#[test]
	fn sort_has_no_formatter_options() {
		for option in
			["--formatter", "--no-sort", "--edition", "--config-path", "--rustfmt-config", "--allow-comment-loss"]
		{
			let error = cli().try_get_matches_from(["cargo-rscode", "sort", option, "x"]).unwrap_err();

			assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument, "{option}");
		}
	}

	#[test]
	fn message_formats_per_subcommand() {
		assert!(cli().try_get_matches_from(["cargo-rscode", "find", "x", "--message-format", "file-lines"]).is_ok());
		assert!(cli().try_get_matches_from(["cargo-rscode", "find", "x", "--message-format", "JSON"]).is_ok());

		let error =
			cli().try_get_matches_from(["cargo-rscode", "view", "x", "--message-format", "file-lines"]).unwrap_err();

		assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
	}

	#[test]
	fn limits_are_positive() {
		let matches = cli().try_get_matches_from(["cargo-rscode", "find", "x", "--limit", "3"]).unwrap();

		assert_eq!(matches.subcommand().unwrap().1.get_one::<usize>("limit"), Some(&3));

		for limit in ["0", "many"] {
			let error = cli().try_get_matches_from(["cargo-rscode", "find", "x", "--limit", limit]).unwrap_err();

			assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation, "{limit}");
		}
	}

	#[test]
	fn insert_before_and_after_need_an_anchor() {
		for position in ["before", "after"] {
			let error =
				cli().try_get_matches_from(["cargo-rscode", "insert", "crate", "--position", position]).unwrap_err();

			assert_eq!(error.kind(), clap::error::ErrorKind::MissingRequiredArgument, "{position}");
		}

		assert!(cli().try_get_matches_from(["cargo-rscode", "insert", "crate", "--position", "start"]).is_ok());
	}

	#[test]
	fn parses_item_kinds_and_aliases() {
		let matches = cli()
			.try_get_matches_from([
				"cargo-rscode",
				"find",
				"x",
				"-k",
				"struct,Function",
				"--kind",
				"assoc_fn",
				"-k",
				"module",
			])
			.unwrap();
		let (_, sub) = matches.subcommand().unwrap();
		let kinds: Vec<ItemKind> = sub.get_many::<ItemKind>("kind").unwrap().copied().collect();

		assert_eq!(kinds, [ItemKind::Struct, ItemKind::Fn, ItemKind::AssocFn, ItemKind::Module]);

		let error = cli().try_get_matches_from(["cargo-rscode", "find", "x", "-k", "strukt"]).unwrap_err();

		assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
		assert!(error.to_string().contains("struct"), "{error}");

		let matches = cli().try_get_matches_from(["cargo-rscode", "find", "x", "-k", "import,impl"]).unwrap();
		let (_, sub) = matches.subcommand().unwrap();
		let kinds: Vec<ItemKind> = sub.get_many::<ItemKind>("kind").unwrap().copied().collect();

		assert_eq!(kinds, [ItemKind::Import, ItemKind::Impl]);
	}

	#[test]
	fn refuses_kinds_that_are_never_found() {
		let refused = |kind: &str| {
			let error = cli().try_get_matches_from(["cargo-rscode", "find", "x", "-k", kind]).unwrap_err();

			assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue, "{kind}");
			error.to_string()
		};

		let error = refused("use");

		assert!(error.contains("invalid value 'use' for '--kind <KIND>'"), "{error}");
		assert!(
			error.contains("tip: `use` declarations are not found as a whole; `--kind import` finds their imports"),
			"{error}"
		);
		assert!(!error.contains("macro-call"), "the possible values are the ones found: {error}");
		assert!(refused("macro-call").contains("tip: macro invocations have no names"));
		assert!(refused("assoc_macro").contains("tip: macro invocations have no names"));
		assert!(refused("foreign-macro").contains("tip: macro invocations have no names"));

		// by name or alias
		assert!(refused("extern-block").contains("tip: `extern` blocks have no names"));
		assert!(refused("extern").contains("tip: `extern` blocks have no names"));
		assert!(!refused("strukt").contains("tip: `"));

		let help = cli().find_subcommand_mut("find").unwrap().render_long_help().to_string();

		assert!(help.contains("assoc-fn") && help.contains("import") && !help.contains("macro-call"), "{help}");
	}

	#[test]
	fn validates_cfg_specs() {
		assert_eq!(parse_cfg("test"), Ok("test".to_owned()));
		assert_eq!(parse_cfg("feature=\"x\""), Ok("feature=\"x\"".to_owned()));
		assert_eq!(parse_cfg("feature = \"x\""), Ok("feature = \"x\"".to_owned()));
		assert_eq!(parse_cfg("feature=x"), Ok("feature=\"x\"".to_owned()));
		assert_eq!(parse_cfg("my_cfg = some value"), Ok("my_cfg=\"some value\"".to_owned()));
		assert_eq!(parse_cfg(r"path=C:\x"), Ok(r#"path="C:\\x""#.to_owned()));
		assert!(parse_cfg("feature=\"x").is_err());
		assert!(parse_cfg("all(unix, test)").is_err());
		assert!(parse_cfg("a b").is_err());
		assert!(parse_cfg("").is_err());
		assert!(parse_cfg("=x").is_err());
		assert!(parse_cfg("_").is_err());
		assert!(parse_cfg("_=x").is_err());
		assert_eq!(parse_cfg("_x"), Ok("_x".to_owned()));

		let error = cli().try_get_matches_from(["cargo-rscode", "view", "x", "--cfg", "not(test)"]).unwrap_err();

		assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
	}

	#[test]
	fn parses_key_value_pairs() {
		assert_eq!(parse_key_value("max_width=80"), Ok(("max_width".to_owned(), "80".to_owned())));
		assert_eq!(parse_key_value(" a = b=c "), Ok(("a".to_owned(), "b=c".to_owned())));
		assert!(parse_key_value("novalue").is_err());
		assert!(parse_key_value("=1").is_err());
	}

	#[test]
	fn reads_the_color_choice() {
		assert_eq!(color_choice(&args(&["x", "view", "--color", "never"])), ColorChoice::Never);
		assert_eq!(color_choice(&args(&["x", "view", "--color=Always"])), ColorChoice::Always);
		assert_eq!(color_choice(&args(&["x", "view", "--color", "auto"])), ColorChoice::Auto);
		assert_eq!(color_choice(&args(&["x", "view", "--", "--color", "never"])), ColorChoice::Auto);
		assert_eq!(color_choice(&args(&["x", "view"])), ColorChoice::Auto);
	}

	#[test]
	fn help_documents_completion_and_mcp_registration() {
		let help = cli().render_long_help().to_string();

		assert!(help.contains("source <(COMPLETE=bash cargo-rscode)"), "{help}");
		assert!(help.contains("source <(COMPLETE=zsh cargo-rscode)"), "{help}");
		assert!(help.contains("COMPLETE=fish cargo-rscode | source"), "{help}");

		#[cfg(feature = "mcp")]
		{
			let help = cli().find_subcommand_mut("mcp").unwrap().render_long_help().to_string();

			assert!(help.contains("claude mcp add rscode -- cargo rscode mcp"), "{help}");
			assert!(help.contains("\"mcpServers\""), "{help}");
		}
	}
}
