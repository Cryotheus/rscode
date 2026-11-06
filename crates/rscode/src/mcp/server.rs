//! The server: tool definitions (their descriptions are what a model reads) and the protocol handler.

use super::ServerOptions;
use super::params::AddImportParams;
use super::params::AttachParams;
use super::params::CreateModuleParams;
use super::params::DetachParams;
use super::params::EditItemParams;
use super::params::FindParams;
use super::params::FormatParams;
use super::params::InsertParams;
use super::params::ItemsAtLineParams;
use super::params::MoveParams;
use super::params::ReferencesParams;
use super::params::RemoveParams;
use super::params::RenameParams;
use super::params::ReplaceParams;
use super::params::SearchTextParams;
use super::params::Selection;
use super::params::UseParams;
use super::params::ViewParams;
use super::render;
use super::sources;
use super::sources::Access;
use super::sources::Exposure;
use super::sources::Session;
use super::sources::Source;
use super::sources::Sources;
use super::sources::Verdict;
use super::sources::WriteScope;
use super::tools;
use super::tools::Output;
use super::tools::Permit;
use super::worker;
use crate::workspace::LoadOptions;
use rmcp::ErrorData;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResponse;
use rmcp::model::CallToolResult;
use rmcp::model::ContentBlock;
use rmcp::model::Implementation;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerConfig;
use rmcp::model::Tool;
use rmcp::service::RequestContext;
use rmcp::tool;
use rmcp::tool_handler;
use rmcp::tool_router;
use serde_json::Value;
use std::collections::BTreeSet;
use std::convert::identity;
use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tokio::sync::Mutex;

/// Guidance for the client (and its model), sent when connecting.
const INSTRUCTIONS: &str = "\
rscode reads, searches, and edits the Rust crates of a cargo workspace by item path, without compiling anything. \
Items that macros generate are invisible, except the statics of `thread_local!` and the `static NAME = value;` \
entries of item-position macro invocations (statics of their module; uses of entries are not tracked).

Paths are written like Rust paths: `crate::m::Item` (from the root of every selected crate), `::crate_name::Item`, \
or `m::Item` (tried from every crate root). Also `Type::method`, `<Type as Trait>::method`, `impl Trait for Type`, \
`impl Type`, `Enum::Variant`, fields (`Type.field`, `Tuple.0`, `Enum::Variant.field`), macro invocations \
(`m::name!`, `name![2]` for the second), and imports (`use m::Name`: a leaf of a `use` item; `use m::Name[2]` for \
the second matching import; `use m::*` for globs). \
Generic arguments pick impl blocks (`impl From<u8> for W`), and a selector picks one of several blocks with the same \
header: `impl Tools[add_bots]` (with that item), `impl Tools[#tool_router]` (with that attribute), `<Tools>[2]::new` \
(the second); printed paths carry one when needed. A path names every cfg variant of an item (disabled ones are \
marked inactive) and goes through imports and re-exports. A path that names nothing in the selected packages is \
looked up in the other workspace members.

Every tool takes `packages` (cargo package specs; by default the server's selection, see workspace_info). Also \
accepted, rarely needed: `workspace` (every member), `features`, `all_features`, `all_targets` (tests, examples, \
benches too), and `lib` (true) or `bin` (binary names) to pick the library or binaries of a package that has both, \
whose roots are both `crate`.

Edits are all-or-nothing: every changed file must still parse, or nothing is written. With `dry_run` they return \
their summary and a diff and write nothing; replace_item, edit_item, and insert_items take `format` to have rustfmt \
format what they wrote. Code outside of the edited items is kept as it is. For small changes use edit_item (exact \
`old`/`new` text, visibility, docs, attributes) rather than replace_item. JSON strings must escape hard tabs as `\t`.

Every call reads the files again, so other tools' changes are seen. Lines are 1-based. Typically: workspace_info, \
find_items, view_items, then edit. To find the items at or near a known source location, use items_at_line. To find \
text without knowing item paths (call sites, strings, a regex across crates), use search_text rather than a generic \
file search: it names the item of each match. For file:line citations, view_items with `line_numbers` numbers lines as \
the file does.";

/// Appended to [`INSTRUCTIONS`] for read-only servers.
const READ_ONLY_INSTRUCTIONS: &str = "\n\nThis server is read-only: the editing tools are disabled.";

/// The parameters that several tools share, which [`INSTRUCTIONS`] describes once rather than every tool schema.
const SHARED_PARAMETERS: [&str; 4] = ["attached", "dry_run", "format", "packages"];

/// The tools about sources, offered when the server exposes directories.
const SOURCE_TOOLS: [&str; 4] = ["attach_source", "detach_source", "list_sources", "use_source"];

/// The rscode MCP server.
pub(crate) struct Server {
	options: ServerOptions,

	/// The exposed directories ([`ServerOptions::exposed`]), shared with the editing calls that check where they write.
	exposed: Arc<[Exposure]>,

	/// The sources this session attached, and the one it uses by default. The lock is never held across an `await`.
	session: std::sync::Mutex<Session>,

	/// When this connection started (a server serves one): sources attached before then were attached on another one,
	/// and went with it.
	started: Instant,

	/// Whether editing calls are done noting that they work on the server's own workspace: only the first one does,
	/// and only while nothing has been attached on this connection (see [`Server::own_workspace_note`]).
	noted: AtomicBool,

	/// Every tool; the editing tools are disabled when the server is read-only, and the tools about sources when it
	/// exposes no directories.
	tool_router: ToolRouter<Self>,

	/// Held by an editing call for its whole run, on its worker thread: edits never interleave, and shutdown can wait
	/// for an edit that is still writing even when its request is gone.
	edits: Arc<Mutex<()>>,
}

#[tool_router(router = edit_tools)]
impl Server {
	/// Import into a module: each leaf of the `use` trees in `paths` becomes a `use` item where `cargo rscode sort`
	/// puts it. When the module already groups imports, it joins its grouped `use` items; otherwise it preserves the
	/// existing one-import-per-line style. What the module imports already is left alone.
	#[tool(annotations(
		title = "Add imports",
		read_only_hint = false,
		destructive_hint = false,
		idempotent_hint = true,
		open_world_hint = false
	))]
	async fn add_import(&self, Parameters(params): Parameters<AddImportParams>, context: RequestContext<RoleServer>) -> CallToolResult {
		let target = self.target(&params.selection, false);
		let dry_run = params.dry_run;

		self.edit("add_import", context, target, dry_run, move |load, permit| {
			tools::add_import(load, &params, permit)
		})
		.await
	}

	/// Create a module: write its file (`source`, may be empty) where rustc looks for it, and declare it in `parent`
	/// (`mod name;` with `vis`) in sorted position.
	#[tool(annotations(
		title = "Create a module",
		read_only_hint = false,
		destructive_hint = false,
		idempotent_hint = false,
		open_world_hint = false
	))]
	async fn create_module(&self, Parameters(params): Parameters<CreateModuleParams>, context: RequestContext<RoleServer>) -> CallToolResult {
		let target = self.target(&params.selection, false);
		let dry_run = params.dry_run;

		self.edit("create_module", context, target, dry_run, move |load, permit| {
			tools::create_module(load, &params, permit)
		})
		.await
	}

	/// Edit an item in place, sending only the change: replace exact text in it (`old` must occur once, or use
	/// `replace_all` for every occurrence within that item; copy it from view_items), and/or set its visibility,
	/// doc comment, or attributes. Use this rather than replace_item for small changes.
	#[tool(annotations(title = "Edit an item", read_only_hint = false, destructive_hint = true, open_world_hint = false))]
	async fn edit_item(&self, Parameters(params): Parameters<EditItemParams>, context: RequestContext<RoleServer>) -> CallToolResult {
		let target = self.target(&params.selection, false);
		let dry_run = params.dry_run;

		self.edit("edit_item", context, target, dry_run, move |load, permit| {
			tools::edit_item(load, &params, permit)
		})
		.await
	}

	/// Format items with rustfmt or prettyplease; `sort: true` first applies Cryotheum ordering. A module target also
	/// formats its out-of-line child modules' files unless `skip_children`; inline children in its file are always
	/// formatted. Other targets format only themselves. `check` shows the diff without writing.
	#[tool(annotations(
		title = "Format items",
		read_only_hint = false,
		destructive_hint = false,
		idempotent_hint = true,
		open_world_hint = false
	))]
	async fn format_items(&self, Parameters(params): Parameters<FormatParams>, context: RequestContext<RoleServer>) -> CallToolResult {
		let target = self.target(&params.selection, false);
		let dry_run = params.check;

		self.edit("format_items", context, target, dry_run, move |load, permit| {
			tools::format(load, &params, permit)
		})
		.await
	}

	/// Insert items into a module, impl block, or trait: at its `end` or `start`, or `before`/`after` the sibling
	/// `anchor` (then `parent` may be left out). Indented to fit and spaced like `cargo rscode sort` lays items out.
	/// Refuses names that are taken unless `force`.
	#[tool(annotations(title = "Insert items", read_only_hint = false, destructive_hint = false, open_world_hint = false))]
	async fn insert_items(&self, Parameters(params): Parameters<InsertParams>, context: RequestContext<RoleServer>) -> CallToolResult {
		let target = self.target(&params.selection, false);
		let dry_run = params.dry_run;

		self.edit("insert_items", context, target, dry_run, move |load, permit| {
			tools::insert(load, &params, permit)
		})
		.await
	}

	/// Reorder an item, with directly complementary `cfg` variants and attached comments, within its existing
	/// container. `start` and `end` need no `anchor`; `before` and `after` take a sibling there. Imports, fields, enum
	/// variants, macro definitions/invocations and their entries, `#[macro_use]` items, out-of-line module declarations,
	/// and cross-container moves are not supported. Ordinary items cannot cross a direct-sibling macro definition,
	/// item-position macro invocation, or `#[macro_use]` item.
	#[tool(annotations(
		title = "Move an item",
		read_only_hint = false,
		destructive_hint = false,
		idempotent_hint = true,
		open_world_hint = false
	))]
	async fn move_item(&self, Parameters(params): Parameters<MoveParams>, context: RequestContext<RoleServer>) -> CallToolResult {
		let target = self.target(&params.selection, false);
		let dry_run = params.dry_run;

		self.edit("move_item", context, target, dry_run, move |load, permit| {
			tools::move_item(load, &params, permit)
		})
		.await
	}

	/// Remove items (every cfg variant) with their docs, attributes, and attached comments, and the files of
	/// out-of-line modules unless `keep_files`. Reports references left dangling.
	#[tool(annotations(title = "Remove items", read_only_hint = false, destructive_hint = true, open_world_hint = false))]
	async fn remove_items(&self, Parameters(params): Parameters<RemoveParams>, context: RequestContext<RoleServer>) -> CallToolResult {
		let target = self.target(&params.selection, true);
		let dry_run = params.dry_run;

		self.edit("remove_items", context, target, dry_run, move |load, permit| {
			tools::remove(load, &params, permit)
		})
		.await
	}

	/// Rename an item (every cfg variant) and update its references in every crate: paths, imports, re-exports, and a
	/// module's file. Refuses collisions unless `force`. Method calls, macro contents, and doc links are uncertain:
	/// listed, and renamed only with `method_calls`, `macro_tokens`, `doc_links`.
	#[tool(annotations(title = "Rename an item", read_only_hint = false, destructive_hint = false, open_world_hint = false))]
	async fn rename_item(&self, Parameters(params): Parameters<RenameParams>, context: RequestContext<RoleServer>) -> CallToolResult {
		let target = self.target(&params.selection, true);
		let dry_run = params.dry_run;

		self.edit("rename_item", context, target, dry_run, move |load, permit| {
			tools::rename(load, &params, permit)
		})
		.await
	}

	/// Replace one item's whole source (docs and attributes included) with one item of the same kind, re-indented to
	/// fit. Add adjacent items with insert_items. `allow_kind_change` permits another kind or several items. For small
	/// changes, edit_item is cheaper. An import is replaced as its `use` item.
	#[tool(annotations(title = "Replace an item", read_only_hint = false, destructive_hint = true, open_world_hint = false))]
	async fn replace_item(&self, Parameters(params): Parameters<ReplaceParams>, context: RequestContext<RoleServer>) -> CallToolResult {
		let target = self.target(&params.selection, false);
		let dry_run = params.dry_run;

		self.edit("replace_item", context, target, dry_run, move |load, permit| {
			tools::replace(load, &params, permit)
		})
		.await
	}
}

#[tool_router(router = query_tools)]
impl Server {
	/// Find items whose paths match a glob `pattern`. One line per item: `path  kind  file:line-endline`, then `cfg: …`
	/// (`inactive` when disabled), `-> target` for imports, and usable paths with `from`.
	#[tool(annotations(title = "Find items", read_only_hint = true, idempotent_hint = true, open_world_hint = false))]
	async fn find_items(&self, Parameters(params): Parameters<FindParams>) -> CallToolResult {
		let target = self.target(&params.selection, false);

		self.query("find_items", target, move |target| tools::find(&target.load, &params)).await
	}

	/// Find the references to an item in every crate of the workspace (for trait items, also of their impls), by file:
	/// `line:col in item: code`. Method calls, macro contents, and doc links only on request.
	#[tool(annotations(title = "Find references", read_only_hint = true, idempotent_hint = true, open_world_hint = false))]
	async fn find_references(&self, Parameters(params): Parameters<ReferencesParams>) -> CallToolResult {
		let target = self.target(&params.selection, true);

		self.query("find_references", target, move |target| tools::references(&target.load, &params))
			.await
	}

	/// Find the items containing a 1-based source `line`, and the nearest items before and after it. Every item includes
	/// its canonical path, kind, and actual `file:start-end` line range.
	#[tool(annotations(
		title = "Find items at a source line",
		read_only_hint = true,
		idempotent_hint = true,
		open_world_hint = false
	))]
	async fn items_at_line(&self, Parameters(params): Parameters<ItemsAtLineParams>) -> CallToolResult {
		let target = self.target(&params.selection, false);

		self.query("items_at_line", target, move |target| tools::items_at_line(&target.load, &params))
			.await
	}

	/// Search the loaded crates' source for a regex, line by line, when you do not know item paths: each matching line
	/// as `line:col in item: code`, grouped by file. `within` limits it to items whose paths match a glob.
	#[tool(annotations(title = "Search text", read_only_hint = true, idempotent_hint = true, open_world_hint = false))]
	async fn search_text(&self, Parameters(params): Parameters<SearchTextParams>) -> CallToolResult {
		let target = self.target(&params.selection, false);

		self.query("search_text", target, move |target| tools::search_text(&target.load, &params))
			.await
	}

	/// Show items by path, in full or outlined (the default for modules: bodies elided, and runs of private `use`
	/// items shown as `use ...;` unless `imports`). Each starts with a header `// path (kind) file:line-endline`; lines are
	/// numbered with the file's line numbers (for citing as file:line) when they are not consecutive, or with
	/// `line_numbers`.
	#[tool(annotations(title = "View items", read_only_hint = true, idempotent_hint = true, open_world_hint = false))]
	async fn view_items(&self, Parameters(params): Parameters<ViewParams>) -> CallToolResult {
		let target = self.target(&params.selection, false);

		self.query("view_items", target, move |target| tools::view(&target.load, &params)).await
	}

	/// Describe the workspace: packages (version, manifest, features, enabled ones marked `*`), crates (kind, root
	/// file, edition, selected or not), and load problems. Call it first to learn the crate names.
	#[tool(annotations(title = "Workspace info", read_only_hint = true, idempotent_hint = true, open_world_hint = false))]
	async fn workspace_info(&self, Parameters(selection): Parameters<Selection>) -> CallToolResult {
		let target = self.target(&selection, false);

		self.query("workspace_info", target, |target| {
			Ok(target.header() + &tools::workspace_info(&target.load)?)
		})
		.await
	}
}

#[tool_router(router = source_tools)]
impl Server {
	/// Attach a cargo workspace or package (its Cargo.toml must be in a directory the server exposes, see list_sources)
	/// under a name, to pass as `attached` to the other tools. A name keeps the source it has until detach_source frees
	/// it or this MCP connection ends; reconnecting starts a new attachment session. Attaching is cheap: every call
	/// loads from disk.
	#[tool(annotations(
		title = "Attach a source",
		read_only_hint = false,
		destructive_hint = false,
		idempotent_hint = true,
		open_world_hint = false
	))]
	async fn attach_source(&self, Parameters(params): Parameters<AttachParams>) -> CallToolResult {
		const TOOL: &str = "attach_source";

		let name = params.name.trim().to_owned();

		if let Err(message) = sources::check_name(&name) {
			return respond(TOOL, Err(message));
		}

		if params.write && self.options.read_only {
			return respond(
				TOOL,
				Err(
					"this rscode server is read-only (it was started with `--read-only`): sources can only be attached \
				     without `write`"
						.to_owned(),
				),
			);
		}

		let access = if params.write { Access::Write } else { Access::Read };
		let source = match sources::manifest(&params.manifest_path) {
			Ok(manifest) => Source { manifest, access },
			Err(message) => return respond(TOOL, Err(message)),
		};

		// a taken name keeps its source: say what it has, before cargo plans anything
		let verdict = sources::verdict(&name, self.session().sources.get(&name), &source);

		match verdict {
			Verdict::Keep(text) => return respond(TOOL, Ok(self.keep(&name, text, params.use_it))),
			Verdict::Refuse(message) => return respond(TOOL, Err(message)),
			Verdict::Attach | Verdict::Upgrade => {}
		}

		let exposed = self.exposed.clone();
		let own = self.options.load.clone();
		let cap = self.access_cap();
		let job = {
			let source = source.clone();

			move || sources::attach(&source, &exposed, cap, &own)
		};
		let summary = match worker::run(TOOL, job).await.and_then(identity) {
			Ok(summary) => summary,
			Err(message) => return respond(TOOL, Err(message)),
		};

		// the name may have been attached meanwhile (calls run concurrently): decide again, as the name is attached
		let verdict = {
			let mut session = self.session();
			let verdict = sources::register(&mut session.sources, &name, &source);
			let attached = matches!(verdict, Verdict::Attach | Verdict::Upgrade);

			// a caller that attached something knows what it attached: no note about the server's own workspace
			if attached {
				self.noted.store(true, Ordering::Relaxed);
			}

			if params.use_it && attached {
				session.default = Some(name.clone());
			}

			verdict
		};
		let mut text = format!("attached `{name}` ({}): {}\n", source.access.describe(), source.manifest.display());

		match verdict {
			Verdict::Keep(kept) => return respond(TOOL, Ok(self.keep(&name, kept, params.use_it))),
			Verdict::Refuse(message) => return respond(TOOL, Err(message)),
			Verdict::Attach => {}
			Verdict::Upgrade => text.push_str("it was attached read-only before: the editing tools can write to it now\n"),
		}

		match params.use_it {
			true => writeln!(text, "{summary}calls without `attached` work on it now").unwrap(),
			false => writeln!(text, "{summary}pass `\"attached\": \"{name}\"` to the other tools to work on it").unwrap(),
		}

		respond(TOOL, Ok(text))
	}

	/// Forget an attached source's name (calls that used it by default work on the server's own workspace again).
	/// Nothing on disk is touched.
	#[tool(annotations(
		title = "Detach a source",
		read_only_hint = false,
		destructive_hint = false,
		idempotent_hint = true,
		open_world_hint = false
	))]
	async fn detach_source(&self, Parameters(params): Parameters<DetachParams>) -> CallToolResult {
		let name = params.name.trim();
		let result = {
			let mut session = self.session();

			match session.sources.remove(name) {
				None => Err(self.unknown(name, &session.sources)),

				Some(source) => {
					let mut text = format!("detached `{name}` ({})\n", source.manifest.display());

					if session.default.as_deref() == Some(name) {
						session.default = None;
						text.push_str("calls without `attached` work on the server's own workspace again\n");
					}

					Ok(text)
				}
			}
		};

		respond("detach_source", result)
	}

	/// List the attached sources (access and Cargo.toml), the server's own workspace, and the directories that sources
	/// can be attached from.
	#[tool(annotations(title = "List sources", read_only_hint = true, idempotent_hint = true, open_world_hint = false))]
	async fn list_sources(&self) -> CallToolResult {
		respond("list_sources", Ok(self.describe_sources()))
	}

	/// Make an attached source the one that calls without `attached` work on, for the rest of this MCP connection (""
	/// for the server's own workspace). The default is shared by every caller on the connection; concurrent workflows
	/// should pass `attached` on each call instead.
	#[tool(annotations(
		title = "Use a source",
		read_only_hint = false,
		destructive_hint = false,
		idempotent_hint = true,
		open_world_hint = false
	))]
	async fn use_source(&self, Parameters(params): Parameters<UseParams>) -> CallToolResult {
		let name = params.name.trim();
		let mut session = self.session();
		let result = match session.sources.get(name) {
			_ if name.is_empty() => {
				session.default = None;
				Ok("calls without `attached` work on the server's own workspace\n".to_owned())
			}

			Some(source) => {
				let text = format!(
					"calls without `attached` work on `{name}` ({}): {}\n",
					source.access.describe(),
					source.manifest.display()
				);

				session.default = Some(name.to_owned());
				Ok(text)
			}

			None => Err(self.unknown(name, &session.sources)),
		};

		drop(session);
		respond("use_source", result)
	}
}

impl Server {
	pub(crate) fn new(options: ServerOptions) -> Self {
		let mut tool_router = Self::query_tools() + Self::edit_tools() + Self::source_tools();

		if options.read_only {
			for name in Self::edit_tools().map.into_keys() {
				tool_router.disable_route(name);
			}
		}

		// nothing can be attached: hide the tools and the parameter about sources
		if options.exposed.is_empty() {
			for name in SOURCE_TOOLS {
				tool_router.disable_route(name);
			}

			for route in tool_router.map.values_mut() {
				without_attached(&mut route.attr);
			}
		}

		for route in tool_router.map.values_mut() {
			slim(&mut route.attr);
		}

		Self {
			exposed: options.exposed.clone().into(),
			options,
			session: std::sync::Mutex::default(),
			started: Instant::now(),
			noted: AtomicBool::new(false),
			tool_router,
			edits: Arc::default(),
		}
	}

	/// The most access a source can get: none can be written on a read-only server.
	fn access_cap(&self) -> Access {
		match self.options.read_only {
			true => Access::Read,
			false => Access::Write,
		}
	}

	/// Fails when a call uses the server's own workspace, but it has none while sources could be attached. (Without
	/// exposed directories, loading fails with a hint for the user instead.) With nothing attached, the hint says when
	/// the connection started, as a server restart loses the sources (and the default) a caller attached before.
	fn check_own_workspace(&self) -> Result<(), String> {
		if self.exposed.is_empty() || self.options.load.manifest_path.is_some() {
			return Ok(());
		}

		let why = match nearest_manifest() {
			Some((_, Some(_))) => return Ok(()),
			Some((directory, None)) => format!("its working directory ({}) is not in a cargo workspace", directory.display()),
			None => "its working directory is unknown".to_owned(),
		};
		let advice = "attach one with `attach_source`, and pass its name as `attached`";
		let hint = match self.session().sources.is_empty() {
			true => format!("{}: {advice}", sources::nothing_attached(self.started.elapsed())),
			false => advice.to_owned(),
		};

		Err(format!("the server has no workspace of its own: {why}\nhint: {hint}"))
	}

	/// What `list_sources` shows.
	fn describe_sources(&self) -> String {
		let access = if self.options.read_only { Access::Read } else { Access::Write };
		let session = self.session();
		let mut text = match &session.default {
			None => "the server's own workspace (used without `attached`): ".to_owned(),
			Some(_) => "the server's own workspace: ".to_owned(),
		};

		match (&self.options.load.manifest_path, nearest_manifest()) {
			(Some(manifest), _) => writeln!(text, "{} ({})", manifest.display(), access.describe()),

			(None, Some((_, Some(manifest)))) => {
				writeln!(text, "{} ({}; found from the working directory)", manifest.display(), access.describe())
			}

			(None, Some((directory, None))) => {
				writeln!(text, "none (the working directory, {}, is not in a cargo workspace)", directory.display())
			}

			(None, None) => writeln!(text, "none (the working directory is unknown)"),
		}
		.unwrap();

		if let Some(name) = &session.default {
			writeln!(text, "calls without `attached` work on `{name}` (see `use_source`)").unwrap();
		}

		let width = session.sources.keys().map(String::len).max().unwrap_or(0);
		let age = sources::age(self.started.elapsed());

		match session.sources.is_empty() {
			true => writeln!(text, "\nno sources are attached (connection started {age} ago; see `attach_source`)"),
			false => writeln!(text, "\nattached sources (connection started {age} ago; pass the name as `attached`):"),
		}
		.unwrap();

		for (name, source) in session.sources.iter() {
			writeln!(text, "  {name:width$}  {:14}  {}", source.access.describe(), source.manifest.display()).unwrap();
		}

		drop(session);
		text.push('\n');
		text.push_str(&sources::exposed_list(&self.exposed, self.access_cap()));
		text
	}

	/// Runs an editing tool's job, one at a time. The job gets a [`Permit`] telling whether the request was cancelled
	/// and where it may write, which it checks right before writing. A source attached read-only is only previewed
	/// (`dry_run`). The result may end with a note about the server's own workspace (see [`Self::own_workspace_note`]).
	async fn edit(
		&self,
		name: &'static str,
		context: RequestContext<RoleServer>,
		target: Result<Target, String>,
		dry_run: bool,
		job: impl FnOnce(&LoadOptions, &Permit<'_>) -> Output + Send + 'static,
	) -> CallToolResult {
		if self.options.read_only {
			return read_only(name);
		}

		let target = match target {
			Ok(target) => target,
			Err(message) => return respond(name, Err(message)),
		};

		if !dry_run
			&& let Some((source, attached)) = &target.source
			&& attached.access == Access::Read
		{
			let mut message = self.read_only_source(name, source, attached);

			if target.by_default {
				message.push_str("\nhint: the connection uses it by default (see `use_source`): pass `attached` to work on another one");
			}

			return respond(name, Err(message));
		}

		let note = self.own_workspace_note(&target);
		let guard = self.edits.clone().lock_owned().await;
		let token = context.ct;
		let result = worker::run(name, move || {
			let _guard = guard;
			let cancelled = || token.is_cancelled();

			job(
				&target.load,
				&Permit {
					cancelled: &cancelled,
					scope: &target.scope,
				},
			)
		})
		.await;
		let mut response = respond(name, result.and_then(identity));

		// after truncating, which would cut it off
		if let Some(note) = note
			&& let Some(ContentBlock::Text(content)) = response.content.first_mut()
		{
			if !content.text.ends_with('\n') {
				content.text.push('\n');
			}

			writeln!(content.text, "{note}").unwrap();
		}

		response
	}

	/// The lock editing calls hold while they run.
	pub(crate) fn edit_lock(&self) -> Arc<Mutex<()>> {
		self.edits.clone()
	}

	/// The response of `attach_source` for a name that keeps its source (see [`Verdict::Keep`]): with `use_it`, the
	/// connection uses it by default from now on.
	fn keep(&self, name: &str, text: String, use_it: bool) -> String {
		if !use_it {
			return text;
		}

		self.session().default = Some(name.to_owned());
		text + "calls without `attached` work on it now\n"
	}

	/// The note that an editing call's result ends with when the call works on the server's own workspace for want of
	/// `attached` (and of a default) on a server that exposes directories. Only the first such call of a connection
	/// gets it, and only while nothing has been attached on the connection: it shows a caller whose server restarted,
	/// losing the source it attached (and made the default), that the call went to the server's own workspace.
	fn own_workspace_note(&self, target: &Target) -> Option<String> {
		if target.source.is_some() || !target.by_default || self.exposed.is_empty() {
			return None;
		}

		let manifest = self.options.load.manifest_path.clone().or_else(|| nearest_manifest()?.1)?;

		if self.noted.swap(true, Ordering::Relaxed) {
			return None;
		}

		Some(format!(
			"note: this call ran on the server's own workspace ({}); {}",
			manifest.display(),
			sources::nothing_attached(self.started.elapsed())
		))
	}

	/// Runs a read-only tool's job.
	async fn query(
		&self,
		name: &'static str,
		target: Result<Target, String>,
		job: impl FnOnce(&Target) -> Output + Send + 'static,
	) -> CallToolResult {
		let target = match target {
			Ok(target) => target,
			Err(message) => return respond(name, Err(message)),
		};

		respond(name, worker::run(name, move || job(&target)).await.and_then(identity))
	}

	/// Why an editing tool does not write to a source attached read-only, and what to do.
	fn read_only_source(&self, tool: &str, name: &str, source: &Source) -> String {
		let preview = if tool == "format_items" { "check" } else { "dry_run" };
		let directory = source.manifest.parent().unwrap_or(Path::new(""));
		let hint = match sources::access(&self.exposed, directory) {
			Some(Access::Write) => {
				format!("set `{preview}` to preview the change, or attach the source again with `write` set to true")
			}

			_ => format!("set `{preview}` to preview the change (its directory is only exposed for reading)"),
		};

		format!("source `{name}` is attached read-only, so `{tool}` cannot write to it; nothing was written\nhint: {hint}")
	}

	/// What this session attached, and the source it uses by default.
	fn session(&self) -> MutexGuard<'_, Session> {
		self.session.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// What a call with `selection` works on: the attached source it names, or else the connection's default source (see
	/// `use_source`), or the server's own workspace (also for an empty `attached`). With `everything`, every other
	/// target of every workspace member is loaded too, so that references everywhere are found.
	fn target(&self, selection: &Selection, everything: bool) -> Result<Target, String> {
		let (name, by_default) = match selection.attached.as_deref().map(str::trim) {
			Some("") => (None, false),
			Some(name) => (Some(name.to_owned()), false),
			None => (self.session().default.clone(), true),
		};
		let (defaults, scope, source) = match name.as_deref() {
			None => {
				self.check_own_workspace()?;
				(self.options.load.clone(), WriteScope::Anywhere, None)
			}

			Some(name) => {
				let source = {
					let session = self.session();

					session.sources.get(name).cloned().ok_or_else(|| self.unknown(name, &session.sources))?
				};

				source.check(name)?;

				let scope = match source.access {
					Access::Write => WriteScope::Exposed {
						source: name.to_owned(),
						exposed: self.exposed.clone(),
					},

					Access::Read => WriteScope::Nowhere { source: name.to_owned() },
				};

				(
					sources::load_options(&self.options.load, &source.manifest),
					scope,
					Some((name.to_owned(), source)),
				)
			}
		};
		let mut load = selection.apply(&defaults);

		load.load_all_members |= everything;

		Ok(Target {
			load,
			scope,
			by_default,
			source,
		})
	}

	/// The tools the server offers.
	#[cfg(test)]
	pub(crate) fn tools(&self) -> Vec<rmcp::model::Tool> {
		self.tool_router.list_all()
	}

	/// The error for a name that no source has.
	fn unknown(&self, name: &str, attached: &Sources) -> String {
		match self.exposed.is_empty() {
			true => format!(
				"no source is attached as `{name}`: this rscode server exposes no directories to attach sources from \
				 (it was started without `--expose`)\nhint: leave out `attached` to work on the server's own workspace"
			),

			false => sources::unknown(name, attached, self.started.elapsed()),
		}
	}
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
	async fn call_tool(&self, request: CallToolRequestParams, context: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
		// the editing tools of a read-only server are hidden, like the tools about sources when nothing can be
		// attached: tell why rather than "tool not found"
		if self.tool_router.is_disabled(&request.name) {
			let result = match SOURCE_TOOLS.contains(&request.name.as_ref()) {
				true => nothing_exposed(&request.name),
				false => read_only(&request.name),
			};

			return Ok(result.into());
		}

		if let Some(tool) = self.tool_router.get(&request.name)
			&& let Err(message) = validate_arguments(tool, request.arguments.as_ref())
		{
			return Ok(respond(&request.name, Err(message)).into());
		}

		self.tool_router.call(ToolCallContext::new(self, request, context)).await
	}

	fn get_info(&self) -> ServerConfig {
		let mut instructions = INSTRUCTIONS.to_owned();

		if self.options.read_only {
			instructions.push_str(READ_ONLY_INSTRUCTIONS);
		}

		if !self.exposed.is_empty() {
			instructions.push_str(&sources_instructions(self.options.read_only));

			for exposure in self.exposed.iter() {
				let access = exposure.access().min(self.access_cap());

				write!(instructions, "\n- {access}: {}", exposure.resolved_pattern()).unwrap();
			}
		}

		let implementation = Implementation::new("rscode", env!("CARGO_PKG_VERSION"))
			.with_title("rscode")
			.with_description("View, search, and edit Rust source files by item path");

		ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
			.with_server_info(implementation)
			.with_instructions(instructions)
	}
}

/// What a tool call works on.
struct Target {
	/// Its source's load options, with the call's selection.
	load: LoadOptions,

	/// Where its edits may be written.
	scope: WriteScope,

	/// The attached source, if the call works on one (rather than on the server's own workspace).
	source: Option<(String, Source)>,

	/// Whether the call names no `attached`, so that it works on the connection's default: the source that `use_source`
	/// chose, or else the server's own workspace.
	by_default: bool,
}

impl Target {
	/// A line naming the attached source, if any.
	fn header(&self) -> String {
		let Some((name, source)) = &self.source else {
			return String::new();
		};

		let default = if self.by_default { ", used by default" } else { "" };

		format!("source `{name}` ({}{default}): {}\n", source.access.describe(), source.manifest.display())
	}
}

/// A compact, valid-looking value for a required parameter in an error's example call.
fn example_value(name: &str, schema: &Value) -> Value {
	if let Some(value) = schema.get("default") {
		return value.clone();
	}

	if let Some(value) = schema.get("enum").and_then(Value::as_array).and_then(|values| values.first()) {
		return value.clone();
	}

	match schema.get("type").and_then(Value::as_str) {
		Some("array") => Value::Array(vec![Value::String(
			match name {
				"paths" | "targets" => "crate::item",
				_ => "value",
			}
			.to_owned(),
		)]),

		Some("boolean") => Value::Bool(false),
		Some("integer" | "number") => Value::from(0),

		_ => Value::String(
			match name {
				"manifest_path" => "/path/to/Cargo.toml",
				"module" | "parent" => "crate",
				"path" => "crate::item",
				"pattern" => "*",
				"source" => "fn example() {}",
				"new_name" => "new_name",
				_ => "value",
			}
			.to_owned(),
		),
	}
}

/// The working directory, and the nearest `Cargo.toml` in it or above it (from which cargo finds the workspace).
fn nearest_manifest() -> Option<(PathBuf, Option<PathBuf>)> {
	let directory = std::env::current_dir().ok()?;
	let manifest = directory
		.ancestors()
		.map(|directory| directory.join("Cargo.toml"))
		.find(|path| path.is_file());

	Some((directory, manifest))
}

fn nothing_exposed(tool: &str) -> CallToolResult {
	CallToolResult::error(vec![ContentBlock::text(format!(
		"`{tool}` is not available: this rscode server exposes no directories to attach sources from (it was started \
		 without `--expose`)"
	))])
}

/// Alternate spellings accepted by serde for a tool parameter.
fn parameter_aliases(tool: &str) -> &'static [(&'static str, &'static str)] {
	const ADD_IMPORT: &[(&str, &str)] = &[
		("path", "paths"),
		("imports", "paths"),
		("dryRun", "dry_run"),
		("dry-run", "dry_run"),
		("check", "dry_run"),
	];
	const ATTACH: &[(&str, &str)] = &[("manifest", "manifest_path"), ("path", "manifest_path")];
	const CREATE: &[(&str, &str)] = &[("content", "source"), ("dryRun", "dry_run"), ("dry-run", "dry_run"), ("check", "dry_run")];
	const EDIT: &[(&str, &str)] = &[("visibility", "vis"), ("dryRun", "dry_run"), ("dry-run", "dry_run"), ("check", "dry_run")];
	const FORMAT: &[(&str, &str)] = &[("paths", "targets"), ("dry_run", "check"), ("dryRun", "check"), ("dry-run", "check")];
	const INSERT: &[(&str, &str)] = &[("content", "source"), ("dryRun", "dry_run"), ("dry-run", "dry_run"), ("check", "dry_run")];
	const DRY_RUN: &[(&str, &str)] = &[("dryRun", "dry_run"), ("dry-run", "dry_run"), ("check", "dry_run")];

	match tool {
		"add_import" => ADD_IMPORT,
		"attach_source" => ATTACH,
		"create_module" => CREATE,
		"edit_item" => EDIT,
		"format_items" => FORMAT,
		"insert_items" => INSERT,
		"move_item" | "remove_items" | "rename_item" | "replace_item" => DRY_RUN,
		_ => &[],
	}
}

fn read_only(tool: &str) -> CallToolResult {
	CallToolResult::error(vec![ContentBlock::text(format!(
		"`{tool}` modifies files, but this rscode server is read-only (it was started with `--read-only`)"
	))])
}

/// A tool's result for the client: text, or an error the model can react to. Overlong text is truncated.
fn respond(tool: &str, result: Result<String, String>) -> CallToolResult {
	let truncate = |text| render::truncate(text, render::MAX_OUTPUT_CHARS, render::truncation_hint(tool));

	match result {
		Ok(text) => CallToolResult::success(vec![ContentBlock::text(truncate(text))]),
		Err(message) => CallToolResult::error(vec![ContentBlock::text(truncate(message))]),
	}
}

/// Slims a tool's input schema to what a model needs to call the tool (the schemas of every tool are sent with every
/// request): no `$schema`, no defaults that say nothing (`false`, `null`, empty), no `null` types of optional
/// parameters (leaving a parameter out is the same), and no descriptions of [`SHARED_PARAMETERS`].
fn slim(tool: &mut Tool) {
	let mut schema = (*tool.input_schema).clone();

	schema.remove("$schema");

	if let Some(Value::Object(properties)) = schema.get_mut("properties") {
		for (name, property) in properties.iter_mut() {
			let Value::Object(property) = property else {
				continue;
			};

			let says_nothing = |default: &Value| match default {
				Value::Null | Value::Bool(false) => true,
				Value::String(text) => text.is_empty(),
				Value::Array(values) => values.is_empty(),
				_ => false,
			};

			if property.get("default").is_some_and(says_nothing) {
				property.remove("default");
			}

			if let Some(Value::Array(types)) = property.get("type")
				&& let [ty] = types.iter().filter(|ty| *ty != "null").collect::<Vec<_>>().as_slice()
			{
				let ty = (*ty).clone();

				property.insert("type".to_owned(), ty);
			}

			// (`uint` and its minimum say what `integer` and the description say)
			if property.get("format").is_some_and(|format| format == "uint") {
				property.remove("format");
				property.remove("minimum");
			}

			if SHARED_PARAMETERS.contains(&name.as_str()) {
				property.remove("description");
			}

			// (what the items of lists are, and their fields, the list's description says)
			if let Some(Value::Object(items)) = property.get_mut("items") {
				items.remove("description");

				if let Some(Value::Object(fields)) = items.get_mut("properties") {
					for field in fields.values_mut().filter_map(Value::as_object_mut) {
						field.remove("description");
					}
				}
			}
		}
	}

	tool.input_schema = Arc::new(schema);
}

/// Appended to [`INSTRUCTIONS`] for servers that expose directories, followed by a list of them.
fn sources_instructions(read_only: bool) -> String {
	let (write, writing) = match read_only {
		true => ("", ""),

		false => (
			", and `write` if you need to edit them",
			", for writing only where the pattern is marked write",
		),
	};

	format!(
		"\n\nOther workspaces and packages can be attached with attach_source (the path of their Cargo.toml, and a \
		 name{write}), then named as any tool's `attached`, or made the connection-wide default with use_source. Names \
		 last until detach_source or this MCP connection ends: list_sources lists them. Attachable are the directories that match \
		 (`*` within a path component, `**` across){writing}:"
	)
}

/// Validates names and required parameters together, before serde can ignore an unknown name or stop at the first
/// missing field. Aliases count as their canonical parameters.
fn validate_arguments(tool: &Tool, arguments: Option<&serde_json::Map<String, Value>>) -> Result<(), String> {
	let Some(Value::Object(properties)) = tool.input_schema.get("properties") else {
		return Ok(());
	};
	let aliases = parameter_aliases(&tool.name);
	let accepts_selection = !SOURCE_TOOLS.contains(&tool.name.as_ref());
	let contains = |name: &str| arguments.is_some_and(|arguments| arguments.contains_key(name));
	let provided = |name: &str| contains(name) || aliases.iter().any(|(alias, canonical)| *canonical == name && contains(alias));
	let mut unknown: Vec<&str> = arguments
		.into_iter()
		.flat_map(|arguments| arguments.keys())
		.filter(|name| {
			!properties.contains_key(*name)
				&& !aliases.iter().any(|(alias, _)| *alias == name.as_str())
				&& !(accepts_selection && name.as_str() == "attached")
		})
		.map(String::as_str)
		.collect();
	let required: Vec<&str> = tool
		.input_schema
		.get("required")
		.and_then(Value::as_array)
		.into_iter()
		.flatten()
		.filter_map(Value::as_str)
		.collect();
	let mut missing: Vec<&str> = required.iter().copied().filter(|name| !provided(name)).collect();
	let position = arguments.and_then(|arguments| arguments.get("position")).and_then(Value::as_str);
	let relative = matches!(position, Some("before" | "after"));
	let absolute = position.is_none() || matches!(position, Some("start" | "end"));

	match tool.name.as_ref() {
		"insert_items" if relative && !provided("anchor") => missing.push("anchor"),
		"insert_items" if absolute && !provided("parent") => missing.push("parent"),
		"move_item" if relative && !provided("anchor") => missing.push("anchor"),
		_ => {}
	}

	unknown.sort_unstable();
	missing.sort_unstable();

	if unknown.is_empty() && missing.is_empty() {
		return Ok(());
	}

	let mut message = format!("invalid parameters for `{}`:", tool.name);

	if !unknown.is_empty() {
		write!(message, "\n- unknown: {}", unknown.join(", ")).unwrap();
	}

	if !missing.is_empty() {
		write!(message, "\n- missing required: {}", missing.join(", ")).unwrap();
	}

	let mut accepted: BTreeSet<&str> = properties.keys().map(String::as_str).collect();

	if accepts_selection {
		accepted.insert("attached");
	}

	accepted.extend(aliases.iter().map(|(alias, _)| *alias));
	writeln!(message, "\naccepted parameters: {}", accepted.into_iter().collect::<Vec<_>>().join(", ")).unwrap();

	if !aliases.is_empty() {
		writeln!(
			message,
			"aliases: {}",
			aliases
				.iter()
				.map(|(alias, canonical)| format!("{alias} -> {canonical}"))
				.collect::<Vec<_>>()
				.join(", ")
		)
		.unwrap();
	}

	let mut example = serde_json::Map::new();

	for name in required {
		let value = properties
			.get(name)
			.map_or(Value::String("value".to_owned()), |schema| example_value(name, schema));

		example.insert(name.to_owned(), value);
	}

	// Some constraints span several optional schema fields, so a required-fields-only example would still fail.
	match tool.name.as_ref() {
		"edit_item" => {
			example.insert("old".to_owned(), Value::String("old text".to_owned()));
			example.insert("new".to_owned(), Value::String("new text".to_owned()));
		}

		"insert_items" => {
			example.insert("parent".to_owned(), Value::String("crate".to_owned()));
		}

		_ => {}
	}

	write!(message, "example arguments: {}", Value::Object(example)).unwrap();
	Err(message)
}

/// Removes the `attached` parameter from a tool's input schema.
fn without_attached(tool: &mut Tool) {
	let mut schema = (*tool.input_schema).clone();

	if let Some(Value::Object(properties)) = schema.get_mut("properties") {
		properties.remove("attached");
	}

	tool.input_schema = Arc::new(schema);
}
