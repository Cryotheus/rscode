//! Tool parameters: what clients send, their JSON schemas, and their conversion into library options.
//!
//! Every field's doc comment becomes its description in the tool's input schema, which is what a model reads to
//! call the tool. Parameters are lenient: unknown fields are ignored, and a single string is accepted where a list
//! of strings is expected.

use crate::ItemKind;
use crate::edit::FmtOptions;
use crate::edit::InsertOptions;
use crate::edit::InsertPosition;
use crate::edit::RemoveOptions;
use crate::edit::RenameOptions;
use crate::edit::ReplaceOptions;
use crate::query::ViewMode;
use crate::query::ViewOptions;
use crate::resolve::ReferenceOptions;
use crate::rscode_fmt::FormatOptions;
use crate::rscode_fmt::RsFormatter;
use crate::rscode_sort::SortOptions;
use crate::workspace::LoadOptions;
use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::SeqAccess;
use serde::de::Visitor;

/// Default of `find_items`' `limit`.
pub(crate) const DEFAULT_FIND_LIMIT: usize = 100;

/// Parameters of `attach_source`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub(crate) struct AttachParams {
	/// Path of the `Cargo.toml` of the workspace or package (or of the directory it is in). Its directory must be
	/// exposed by the server (see `list_sources`).
	#[serde(alias = "manifest", alias = "path")]
	pub(crate) manifest_path: String,

	/// The name to refer to the source by: pass it as `attached` to the other tools. ASCII letters, digits, `_`, `-`,
	/// and `.`. A name that is attached already keeps its source: the response says what it is, and `detach_source`
	/// frees the name.
	pub(crate) name: String,

	/// Attach for writing, so that the editing tools can change its files. Refused unless its directory is exposed
	/// for writing. Without it, the source is read-only (the editing tools can still preview changes with `dry_run`),
	/// but a writable source attached under the name stays writable.
	#[serde(default)]
	pub(crate) write: bool,
}

/// Parameters of `detach_source`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub(crate) struct DetachParams {
	/// The name the source was attached as.
	pub(crate) name: String,
}

/// Parameters of `find_items`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub(crate) struct FindParams {
	/// Glob pattern for item paths. `*` matches within one path segment, `**` any number of segments:
	/// `parse_*`, `*Error`, `Config::*` (associated items), `crate::config::**`, `**::tests::*`,
	/// `<Config as Default>::default`. Patterns not starting with `crate::` or `::` match anywhere:
	/// `Config::load` finds `my_crate::config::Config::load`. `use` patterns find imports: `use crate::a::*` (every
	/// import in `a`), `use Config`.
	pub(crate) pattern: String,

	/// Only items of these kinds: `mod`, `struct`, `enum`, `union`, `trait`, `trait-alias`, `type`, `fn`,
	/// `const`, `static`, `macro-rules`, `extern-crate`, `import`, `impl` (with qualified patterns such as
	/// `<Config as *>`), `assoc-fn`, `assoc-const`, `assoc-type`, `variant`, `foreign-fn`, `foreign-static`,
	/// `foreign-type`. Aliases such as `function`, `method`, and `module` work too.
	#[serde(default, deserialize_with = "split_list")]
	pub(crate) kinds: Vec<String>,

	/// Match names case-insensitively.
	#[serde(default)]
	pub(crate) ignore_case: bool,

	/// Skip items whose cfg is definitely disabled with the current features and target.
	#[serde(default)]
	pub(crate) active_only: bool,

	/// Also list the paths through which each item can be used from a viewpoint: `crate` (the item's own crate
	/// root), `::` (another crate: only public paths), or a module path such as `crate::a::b`.
	#[serde(default)]
	pub(crate) from: Option<String>,

	/// Also find `use` imports, as `use module::Name` paths that name them (with the paths of what they import).
	#[serde(default)]
	pub(crate) include_imports: bool,

	/// Maximum number of matches to list (0 only counts them).
	#[serde(default = "default_find_limit")]
	pub(crate) limit: usize,

	/// Number of matches to skip, to page through many matches.
	#[serde(default)]
	pub(crate) offset: usize,

	#[serde(flatten)]
	pub(crate) selection: Selection,
}

impl FindParams {
	/// Whether imports are searched: requested explicitly, or by asking for the `import` kind.
	pub(crate) fn imports(&self, kinds: &[ItemKind]) -> bool {
		self.include_imports || kinds.contains(&ItemKind::Import)
	}

	/// The requested item kinds.
	pub(crate) fn kinds(&self) -> Result<Vec<ItemKind>, String> {
		self.kinds
			.iter()
			.map(|kind| kind.parse::<ItemKind>().map_err(|_| unknown_kind(kind)))
			.collect()
	}
}

/// Parameters of `format_items`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub(crate) struct FormatParams {
	/// Path patterns of what to format: `crate` (every loaded crate), modules (`crate::a`: their files, and child
	/// modules' files unless `skip_children`), or any other items (formatted in place, leaving the rest of the file
	/// untouched). Globs are allowed (`crate::a::*`); `use crate::a::*` matches every import of `a`.
	#[serde(default = "crate_root", deserialize_with = "string_list")]
	pub(crate) targets: Vec<String>,

	/// `rustfmt` (honors the project's rustfmt.toml), `prettyplease` (removes non-doc comments, so it refuses
	/// files with comments unless `allow_comment_loss`), or `none` (only sort).
	#[serde(default)]
	pub(crate) formatter: Formatter,

	/// Sort items first, with the Cryotheum ordering (groups items by kind, then orders them by name).
	#[serde(default = "yes")]
	pub(crate) sort: bool,

	/// Only process the targets themselves, not child modules.
	#[serde(default)]
	pub(crate) skip_children: bool,

	/// Write nothing: tell whether formatting would change anything, with a unified diff.
	#[serde(default, alias = "dry_run", alias = "dryRun", alias = "dry-run")]
	pub(crate) check: bool,

	/// Let prettyplease remove non-doc comments instead of refusing to format.
	#[serde(default)]
	pub(crate) allow_comment_loss: bool,

	#[serde(flatten)]
	pub(crate) selection: Selection,
}

impl FormatParams {
	pub(crate) fn options(&self) -> Result<FmtOptions, String> {
		if self.formatter == Formatter::None && !self.sort {
			return Err("nothing to do: `formatter` is `none` and `sort` is false".to_owned());
		}

		let sort = self.sort.then(|| SortOptions::new().recursive(!self.skip_children));

		Ok(FmtOptions {
			format: FormatOptions::new()
				.formatter(self.formatter.into())
				.sort(sort)
				.allow_comment_loss(self.allow_comment_loss),
			skip_children: self.skip_children,
			active_only: false,
		})
	}

	/// The target patterns; `crate` when none are given.
	pub(crate) fn targets(&self) -> Vec<String> {
		let targets: Vec<String> = self
			.targets
			.iter()
			.map(|target| target.trim())
			.filter(|target| !target.is_empty())
			.map(str::to_owned)
			.collect();

		match targets.is_empty() {
			true => crate_root(),
			false => targets,
		}
	}
}

/// The formatter `format_items` runs.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars", inline)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Formatter {
	#[default]
	Rustfmt,
	Prettyplease,
	None,
}

/// Parameters of `insert_items`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub(crate) struct InsertParams {
	/// The container: a module (`crate` for the crate root, `crate::a::b`), an impl block (`impl Trait for Type`,
	/// `impl Type`, `<Type as Trait>`), or a trait.
	pub(crate) parent: String,

	/// The items to insert (one or more, with their doc comments and attributes). Associated items for impl
	/// blocks and traits.
	pub(crate) source: String,

	/// `end` of the container, `start` (after inner attributes and `//!` docs), or `before`/`after` the sibling
	/// item named by `anchor`.
	#[serde(default)]
	pub(crate) position: Position,

	/// Path of the sibling item, for position `before` or `after` (e.g. `crate::a::b::helper`, or an import's
	/// `use crate::a::Name`, which stands for its `use` item).
	#[serde(default)]
	pub(crate) anchor: Option<String>,

	/// Insert even when a name is already taken in the container.
	#[serde(default)]
	pub(crate) force: bool,

	/// Write nothing: return the summary and a unified diff of the changes.
	#[serde(default, alias = "dryRun", alias = "dry-run", alias = "check")]
	pub(crate) dry_run: bool,

	/// Format the inserted items with rustfmt after writing them.
	#[serde(default)]
	pub(crate) format: bool,

	#[serde(flatten)]
	pub(crate) selection: Selection,
}

impl InsertParams {
	pub(crate) fn options(&self) -> Result<InsertOptions, String> {
		let anchor = self.anchor.as_deref().map(str::trim).filter(|anchor| !anchor.is_empty());
		let position = match (self.position, anchor) {
			(Position::End, None) => InsertPosition::End,
			(Position::Start, None) => InsertPosition::Start,
			(Position::Before, Some(anchor)) => InsertPosition::Before(anchor.to_owned()),
			(Position::After, Some(anchor)) => InsertPosition::After(anchor.to_owned()),

			(Position::Before | Position::After, None) => {
				return Err("`anchor` (the path of a sibling item) is required with position `before` and `after`".to_owned());
			}

			(Position::End | Position::Start, Some(_)) => {
				return Err("`anchor` is only used with position `before` or `after`".to_owned());
			}
		};

		Ok(InsertOptions { position, force: self.force })
	}
}

/// How much of an item `view_items` shows.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars", inline)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
	#[default]
	Auto,
	Full,
	Outline,
}

/// Where `insert_items` inserts.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars", inline)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Position {
	#[default]
	End,
	Start,
	Before,
	After,
}

/// Parameters of `remove_items`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub(crate) struct RemoveParams {
	/// Paths of the items to remove; `use crate::a::Name` removes an import (not what it imports).
	#[serde(deserialize_with = "string_list")]
	pub(crate) paths: Vec<String>,

	/// Keep the files of removed out-of-line modules (only their `mod name;` declarations are removed).
	#[serde(default)]
	pub(crate) keep_files: bool,

	/// Also remove the `use` imports of the removed items.
	#[serde(default)]
	pub(crate) prune_imports: bool,

	/// Only remove the cfg variants that are not definitely disabled.
	#[serde(default)]
	pub(crate) active_only: bool,

	/// Write nothing: return the summary and a unified diff of the changes.
	#[serde(default, alias = "dryRun", alias = "dry-run", alias = "check")]
	pub(crate) dry_run: bool,

	#[serde(flatten)]
	pub(crate) selection: Selection,
}

impl RemoveParams {
	pub(crate) fn options(&self) -> RemoveOptions {
		RemoveOptions {
			keep_files: self.keep_files,
			prune_imports: self.prune_imports,
			active_only: self.active_only,
		}
	}
}

/// Parameters of `rename_item`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub(crate) struct RenameParams {
	/// Path of the item to rename: `crate::config::Config`, `Config::load`, `<Config as Default>::default`,
	/// `crate::config` (a module: its file is renamed too).
	pub(crate) path: String,

	/// The new name, an identifier (`r#` for keywords).
	pub(crate) new_name: String,

	/// Rename even when the new name collides with an existing name.
	#[serde(default)]
	pub(crate) force: bool,

	/// Write nothing: return the summary and a unified diff of the changes.
	#[serde(default, alias = "dryRun", alias = "dry-run", alias = "check")]
	pub(crate) dry_run: bool,

	/// Also rename method calls `x.old_name(..)`, which are not type-checked and may belong to other types, and
	/// `T::old_name` paths through generic parameters whose bounds do not tell (such as through supertraits).
	#[serde(default)]
	pub(crate) method_calls: bool,

	/// Also rename occurrences inside of macro invocations and `macro_rules!` transcribers that could not be analyzed
	/// (paths there, like `module::name` and `name!`, are resolved and renamed anyway).
	#[serde(default)]
	pub(crate) macro_tokens: bool,

	/// Also update intra-doc links (``[`OldName`]``) in doc comments.
	#[serde(default)]
	pub(crate) doc_links: bool,

	#[serde(flatten)]
	pub(crate) selection: Selection,
}

impl RenameParams {
	pub(crate) fn options(&self) -> RenameOptions {
		RenameOptions {
			force: self.force,
			references: ReferenceOptions {
				method_calls: self.method_calls,
				macro_tokens: self.macro_tokens,
				doc_links: self.doc_links,
			},
		}
	}
}

/// Parameters of `replace_item`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub(crate) struct ReplaceParams {
	/// Path of the item to replace; `use crate::a::Name` replaces an import's `use` item.
	pub(crate) path: String,

	/// The complete new source of the item, including its doc comments and attributes.
	pub(crate) source: String,

	/// Allow replacing the item with a different kind of item, or with several items.
	#[serde(default)]
	pub(crate) allow_kind_change: bool,

	/// When the path names several cfg variants of the item, replace every one of them (otherwise that is an
	/// error listing the variants).
	#[serde(default)]
	pub(crate) all_variants: bool,

	/// Write nothing: return the summary and a unified diff of the changes.
	#[serde(default, alias = "dryRun", alias = "dry-run", alias = "check")]
	pub(crate) dry_run: bool,

	/// Format the new item with rustfmt after writing it.
	#[serde(default)]
	pub(crate) format: bool,

	#[serde(flatten)]
	pub(crate) selection: Selection,
}

impl ReplaceParams {
	pub(crate) fn options(&self) -> ReplaceOptions {
		ReplaceOptions {
			allow_kind_change: self.allow_kind_change,
			all_variants: self.all_variants,
		}
	}
}

/// What to load: an attached source instead of the server's own workspace, which of its packages, and with which
/// features. Accepted by every tool but the tools about sources.
#[derive(Debug, Default, Clone, Eq, PartialEq, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(default)]
pub(crate) struct Selection {
	/// Work on this attached source (the name given to `attach_source`) instead of the server's own workspace.
	pub(crate) attached: Option<String>,

	/// Packages to load (cargo package specs). Default: the server's selection, see `workspace_info`.
	#[serde(deserialize_with = "string_list")]
	pub(crate) packages: Vec<String>,

	/// Load every workspace member.
	pub(crate) workspace: bool,

	/// Features to enable, in addition to the server's (`feature` or `package/feature`).
	#[serde(deserialize_with = "split_list")]
	pub(crate) features: Vec<String>,

	/// Enable all features.
	pub(crate) all_features: bool,

	/// Also load examples, tests, and benches (not only libraries and binaries).
	pub(crate) all_targets: bool,

	/// Load the library of each selected package, and no binaries unless named in `bin`. Selects the library when
	/// a path (like `crate`) names items of both a library and a binary.
	pub(crate) lib: bool,

	/// Load these binaries (by name), and no other binaries (nor the library unless `lib`).
	#[serde(deserialize_with = "string_list")]
	pub(crate) bin: Vec<String>,
}

impl Selection {
	/// The server's load options with this selection applied: `workspace` replaces the package selection,
	/// `packages` narrow it, and features are added.
	pub(crate) fn apply(&self, defaults: &LoadOptions) -> LoadOptions {
		let mut options = defaults.clone();

		if self.workspace {
			options.workspace = true;
			options.packages.clear();
		} else if !self.packages.is_empty() {
			options.workspace = false;
			options.exclude.clear();
			options.packages.clone_from(&self.packages);
		}

		for feature in &self.features {
			if !options.features.contains(feature) {
				options.features.push(feature.clone());
			}
		}

		options.all_features |= self.all_features;
		options.targets.all_targets |= self.all_targets;
		options.targets.lib |= self.lib;

		for bin in &self.bin {
			if !options.targets.bins.contains(bin) {
				options.targets.bins.push(bin.clone());
			}
		}

		options
	}
}

struct StringList;

impl<'de> Visitor<'de> for StringList {
	type Value = Vec<String>;

	fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("a list of strings")
	}

	fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
		Ok(Vec::new())
	}

	fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
		let mut items = Vec::new();

		while let Some(item) = seq.next_element::<String>()? {
			items.push(item);
		}

		Ok(items)
	}

	fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
		Ok(vec![value.to_owned()])
	}

	fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
		Ok(Vec::new())
	}
}

/// Parameters of `view_items`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub(crate) struct ViewParams {
	/// Paths of the items to show: `crate::a::Item`, `::crate_name::Item`, `a::Item` (from each crate's root),
	/// `Type::method`, `Trait::method`, `<Type as Trait>::method`, `impl Trait for Type`, `Enum::Variant`,
	/// `crate` (the crate root module), or `use crate::a::Item` (an import, shown as its `use` item).
	#[serde(deserialize_with = "string_list")]
	pub(crate) paths: Vec<String>,

	/// `auto`: an outline for modules and the full source of anything else; `full`: the exact source (for
	/// out-of-line modules: the module's file); `outline`: function and macro bodies (other than of `thread_local!`,
	/// whose statics are items) elided, modules as lists of their items.
	#[serde(default)]
	pub(crate) mode: Mode,

	/// Include doc comments.
	#[serde(default = "yes")]
	pub(crate) docs: bool,

	/// Prefix every line with its line number in the file.
	#[serde(default = "yes")]
	pub(crate) line_numbers: bool,

	/// Also show the `impl` blocks of types and traits (outlined unless `mode` is `full`).
	#[serde(default)]
	pub(crate) impls: bool,

	/// Skip cfg variants that are definitely disabled.
	#[serde(default)]
	pub(crate) active_only: bool,

	#[serde(flatten)]
	pub(crate) selection: Selection,
}

impl ViewParams {
	pub(crate) fn options(&self) -> ViewOptions {
		ViewOptions {
			mode: self.mode.into(),
			docs: self.docs,
			line_numbers: self.line_numbers,
			impls: self.impls,
			active_only: self.active_only,
		}
	}
}

impl From<Formatter> for RsFormatter {
	fn from(formatter: Formatter) -> Self {
		match formatter {
			Formatter::Rustfmt => Self::RustFmt,
			Formatter::Prettyplease => Self::PrettyPlease,
			Formatter::None => Self::None,
		}
	}
}

impl From<Mode> for ViewMode {
	fn from(mode: Mode) -> Self {
		match mode {
			Mode::Auto => Self::Auto,
			Mode::Full => Self::Full,
			Mode::Outline => Self::Outline,
		}
	}
}

fn crate_root() -> Vec<String> {
	vec!["crate".to_owned()]
}

fn default_find_limit() -> usize {
	DEFAULT_FIND_LIMIT
}

/// Like [`string_list`], and splits every string at commas and whitespace (`"fn, struct"`), like cargo's
/// `--features`.
fn split_list<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
	let items = string_list(deserializer)?;

	Ok(items
		.iter()
		.flat_map(|item| item.split(|c: char| c == ',' || c.is_whitespace()))
		.filter(|item| !item.is_empty())
		.map(str::to_owned)
		.collect())
}

/// Deserializes a list of strings, also accepting a single string (or `null` for none).
fn string_list<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
	deserializer.deserialize_any(StringList)
}

/// Lists the kinds that can be found: named items, and `impl` blocks (with qualified patterns).
fn unknown_kind(kind: &str) -> String {
	let names: Vec<&str> = ItemKind::ALL
		.iter()
		.filter(|kind| kind.is_nameable() || **kind == ItemKind::Impl)
		.map(|kind| kind.name())
		.collect();

	format!("unknown item kind `{kind}`; expected one of: {}", names.join(", "))
}

fn yes() -> bool {
	true
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::rscode_sort::OrderingSchema;
	use serde::de::DeserializeOwned;
	use serde_json::json;

	#[test]
	fn defaults() {
		let find: FindParams = parse(json!({ "pattern": "*Error" }));

		assert_eq!(find.pattern, "*Error");
		assert_eq!((find.limit, find.offset), (DEFAULT_FIND_LIMIT, 0));
		assert!(find.kinds.is_empty() && find.from.is_none());
		assert!(!find.ignore_case && !find.active_only && !find.include_imports);
		assert_eq!(find.selection, Selection::default());

		let view: ViewParams = parse(json!({ "paths": ["crate"] }));

		assert_eq!(view.mode, Mode::Auto);
		assert!(view.docs && view.line_numbers && !view.impls && !view.active_only);

		let format: FormatParams = parse(json!({}));

		assert_eq!(format.targets(), ["crate"]);
		assert_eq!(format.formatter, Formatter::Rustfmt);
		assert!(format.sort && !format.skip_children && !format.check && !format.allow_comment_loss);

		let insert: InsertParams = parse(json!({ "parent": "crate", "source": "fn f() {}" }));

		assert_eq!(insert.position, Position::End);
		assert!(insert.anchor.is_none() && !insert.force && !insert.dry_run && !insert.format);
	}

	#[test]
	fn dry_runs_are_understood_in_every_spelling() {
		let with = |mut arguments: serde_json::Value, flag: &str| {
			arguments[flag] = json!(true);
			arguments
		};

		for flag in ["dry_run", "dryRun", "dry-run", "check"] {
			let rename: RenameParams = parse(with(json!({ "path": "a", "new_name": "b" }), flag));
			let remove: RemoveParams = parse(with(json!({ "paths": ["a"] }), flag));
			let replace: ReplaceParams = parse(with(json!({ "path": "a", "source": "" }), flag));
			let insert: InsertParams = parse(with(json!({ "parent": "crate", "source": "" }), flag));
			let format: FormatParams = parse(with(json!({}), flag));

			assert!(
				rename.dry_run && remove.dry_run && replace.dry_run && insert.dry_run && format.check,
				"{flag}"
			);
		}
	}

	#[test]
	fn edit_options() {
		let rename: RenameParams = parse(json!({ "path": "a", "new_name": "b", "force": true, "doc_links": true }));
		let options = rename.options();

		assert!(options.force);
		assert_eq!(
			options.references,
			ReferenceOptions {
				method_calls: false,
				macro_tokens: false,
				doc_links: true,
			}
		);

		let remove: RemoveParams = parse(json!({ "paths": ["a"], "keep_files": true, "active_only": true }));
		let options = remove.options();

		assert!(options.keep_files && !options.prune_imports && options.active_only);

		let replace: ReplaceParams = parse(json!({ "path": "a", "source": "fn a() {}", "all_variants": true }));
		let options = replace.options();

		assert!(!options.allow_kind_change && options.all_variants);
	}

	fn error<T: DeserializeOwned + std::fmt::Debug>(value: serde_json::Value) -> String {
		serde_json::from_value::<T>(value).unwrap_err().to_string()
	}

	#[test]
	fn format_options() {
		let format: FormatParams = parse(json!({ "targets": ["crate::a", " ", "crate::b::*"], "skip_children": true }));
		let options = format.options().unwrap();

		assert_eq!(format.targets(), ["crate::a", "crate::b::*"]);
		assert_eq!(options.format.formatter, RsFormatter::RustFmt);
		assert_eq!(options.format.sort, Some(SortOptions::new().recursive(false)));
		assert_eq!(options.format.sort.as_ref().unwrap().schema, OrderingSchema::Cryotheum);
		assert!(options.skip_children && !options.active_only);

		let format: FormatParams = parse(json!({ "targets": [], "formatter": "prettyplease", "sort": false, "allow_comment_loss": true }));
		let options = format.options().unwrap();

		assert_eq!(format.targets(), ["crate"]);
		assert_eq!(options.format.formatter, RsFormatter::PrettyPlease);
		assert_eq!(options.format.sort, None);
		assert!(options.format.allow_comment_loss);

		let sort_only: FormatParams = parse(json!({ "formatter": "none" }));

		assert_eq!(sort_only.options().unwrap().format.formatter, RsFormatter::None);
		assert_eq!(sort_only.options().unwrap().format.sort, Some(SortOptions::new()));

		let nothing: FormatParams = parse(json!({ "formatter": "none", "sort": false }));

		assert!(nothing.options().unwrap_err().starts_with("nothing to do"));
	}

	#[test]
	fn insert_positions() {
		let position = |position: serde_json::Value, anchor: serde_json::Value| {
			parse::<InsertParams>(json!({ "parent": "crate", "source": "", "position": position, "anchor": anchor }))
				.options()
				.map(|options| options.position)
		};

		assert_eq!(position(json!("end"), json!(null)), Ok(InsertPosition::End));
		assert_eq!(position(json!("start"), json!("")), Ok(InsertPosition::Start));
		assert_eq!(
			position(json!("before"), json!(" crate::f ")),
			Ok(InsertPosition::Before("crate::f".to_owned()))
		);
		assert_eq!(
			position(json!("after"), json!("crate::f")),
			Ok(InsertPosition::After("crate::f".to_owned()))
		);
		assert!(
			position(json!("after"), json!(null))
				.unwrap_err()
				.contains("`anchor` (the path of a sibling item) is required")
		);
		assert!(
			position(json!("end"), json!("crate::f"))
				.unwrap_err()
				.contains("only used with position `before` or `after`")
		);

		let insert: InsertParams = parse(json!({ "parent": "crate", "source": "", "force": true }));

		assert!(insert.options().unwrap().force);
	}

	#[test]
	fn kinds() {
		let find: FindParams = parse(json!({ "pattern": "x", "kinds": ["fn", "Function", "method", "assoc_fn", "mod", "variant"] }));

		assert_eq!(
			find.kinds().unwrap(),
			[
				ItemKind::Fn,
				ItemKind::Fn,
				ItemKind::AssocFn,
				ItemKind::AssocFn,
				ItemKind::Module,
				ItemKind::Variant
			]
		);
		assert!(!find.imports(&find.kinds().unwrap()));
		assert!(find.imports(&[ItemKind::Import]));

		let find: FindParams = parse(json!({ "pattern": "x", "kinds": ["fn", "func"] }));
		let message = find.kinds().unwrap_err();

		assert_eq!(
			message,
			"unknown item kind `func`; expected one of: mod, struct, enum, union, trait, trait-alias, type, fn, const, static, \
			 macro-rules, import, extern-crate, foreign-fn, foreign-static, foreign-type, impl, assoc-fn, assoc-const, \
			 assoc-type, variant"
		);
	}

	#[test]
	fn lists_are_lenient() {
		let view: ViewParams = parse(json!({ "paths": "crate::a" }));

		assert_eq!(view.paths, ["crate::a"]);

		let find: FindParams = parse(json!({ "pattern": "x", "kinds": "fn, struct\tenum", "features": ["a,b", "c d"] }));

		assert_eq!(find.kinds, ["fn", "struct", "enum"]);
		assert_eq!(find.selection.features, ["a", "b", "c", "d"]);

		let find: FindParams = parse(json!({ "pattern": "x", "kinds": null, "packages": "demo" }));

		assert!(find.kinds.is_empty());
		assert_eq!(find.selection.packages, ["demo"]);

		// items are not split where commas cannot separate items
		let remove: RemoveParams = parse(json!({ "paths": ["a, b"] }));

		assert_eq!(remove.paths, ["a, b"]);
	}

	#[test]
	fn malformed_values_are_refused() {
		assert!(error::<FindParams>(json!({})).contains("missing field `pattern`"));
		assert!(error::<FindParams>(json!({ "pattern": 3 })).contains("invalid type"));
		assert!(error::<ViewParams>(json!({ "paths": [1] })).contains("invalid type"));
		assert!(error::<ViewParams>(json!({ "paths": {} })).contains("expected a list of strings"));
		assert!(error::<ViewParams>(json!({ "paths": [], "mode": "Full" })).contains("unknown variant `Full`"));
		assert!(error::<FindParams>(json!({ "pattern": "x", "limit": -1 })).contains("invalid value"));
		assert!(error::<RenameParams>(json!({ "path": "a" })).contains("missing field `new_name`"));
	}

	fn parse<T: DeserializeOwned>(value: serde_json::Value) -> T {
		serde_json::from_value(value).unwrap()
	}

	#[test]
	fn selections_adjust_the_server_defaults() {
		let defaults = LoadOptions {
			packages: vec!["server-pick".to_owned()],
			exclude: vec!["excluded".to_owned()],
			features: vec!["base".to_owned()],
			offline: true,
			..LoadOptions::default()
		};

		// nothing given: the server's options
		assert_eq!(Selection::default().apply(&defaults), defaults);

		let narrowed = parse::<Selection>(json!({ "packages": ["a", "b@1.0.0"], "features": ["base", "extra"] })).apply(&defaults);

		assert_eq!(narrowed.packages, ["a", "b@1.0.0"]);
		assert!(!narrowed.workspace);
		assert!(narrowed.exclude.is_empty());
		assert_eq!(narrowed.features, ["base", "extra"]);
		assert!(narrowed.offline);

		let everything =
			parse::<Selection>(json!({ "workspace": true, "packages": ["ignored"], "all_features": true, "all_targets": true })).apply(&defaults);

		assert!(everything.workspace);
		assert!(everything.packages.is_empty());
		assert_eq!(everything.exclude, ["excluded"]);
		assert!(everything.all_features);
		assert!(everything.targets.all_targets);
	}

	#[test]
	fn unknown_fields_are_ignored() {
		let find: FindParams = parse(json!({ "pattern": "x", "regex": true, "explain": "please" }));

		assert_eq!(find.pattern, "x");
	}

	#[test]
	fn view_options() {
		let view: ViewParams = parse(json!({
			"paths": ["a"],
			"mode": "outline",
			"docs": false,
			"line_numbers": false,
			"impls": true,
			"active_only": true,
		}));
		let options = view.options();

		assert_eq!(options.mode, ViewMode::Outline);
		assert!(!options.docs && !options.line_numbers && options.impls && options.active_only);
		assert_eq!(ViewMode::from(Mode::Full), ViewMode::Full);
		assert_eq!(ViewMode::from(Mode::Auto), ViewMode::Auto);
	}
}
