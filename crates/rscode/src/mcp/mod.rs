//! A Model Context Protocol server exposing rscode's operations as tools (feature `mcp`).
//!
//! Run it over stdio with [`serve_stdio`]; `cargo rscode mcp` does exactly that. For example, to register it with
//! Claude Code: `claude mcp add rscode -- cargo rscode mcp --manifest-path /abs/path/to/Cargo.toml`.
//!
//! | tool | does |
//! |---|---|
//! | `workspace_info` | packages (with features), crates, and load problems |
//! | `find_items` | items whose paths match a glob pattern (`*`, `**`), one line each, with locations and `cfg`s |
//! | `view_items` | the source (or an outline) of items by path, with line numbers |
//! | `rename_item` | renames an item and updates the references to it across the workspace |
//! | `remove_items` | removes items with their attached comments (and out-of-line module files) |
//! | `replace_item` | replaces the source of an item |
//! | `insert_items` | inserts items into a module, `impl` block, or trait |
//! | `format_items` | sorts and formats items with rustfmt or prettyplease |
//! | `attach_source` | attaches another workspace or package under a name (with [`ServerOptions::exposed`]) |
//! | `detach_source` | forgets an attached source |
//! | `list_sources` | the attached sources, the server's own workspace, and the exposed directories |
//!
//! Every tool but the ones about sources also accepts `packages`, `workspace`, `features`, `all_features`,
//! `all_targets`, `lib`, and `bin`, which adjust the server's [`LoadOptions`] for that call (and `attached`, see
//! [Sources](#sources)). Editing tools accept `dry_run` (`check` for `format_items`), which
//! returns a unified diff instead of writing. With [`ServerOptions::read_only`], the editing tools are not offered.
//!
//! # Sources
//!
//! A server works on its own workspace ([`ServerOptions::load`]), and when it exposes directories
//! ([`ServerOptions::exposed`]), on the cargo workspaces and packages in them that a client attaches: `attach_source`
//! takes the path of a `Cargo.toml` whose directory an [`Exposure`] matches, a name, and whether the client needs to
//! write. The tools then take that name as `attached`. A source can only be attached for writing where it is exposed
//! for writing, and edits of attached sources are only written below directories exposed for writing, however the
//! edit reaches them (for example through `#[path]` attributes or other workspace members); edits of the server's own
//! workspace are not restricted. Sources attached read-only can still be previewed with `dry_run`. Without exposed
//! directories, the tools about sources and the `attached` parameter are not offered.
//!
//! Reading is not confined like writing: an attached source is loaded like cargo loads it, so its other workspace
//! members and the files its `#[path]` attributes name can be read too. With [`LoadOptions::exact_features`], cargo
//! may update its cache of rustc's output in a source's target directory, like `cargo metadata` does.
//!
//! Names belong to a session (a connection): over stdio, a server has exactly one client, and clients do not see
//! (or break) each other's names. Attaching is cheap, since it only records the name for the canonical path of the
//! `Cargo.toml` (after checking that cargo can plan loading it); every call loads its source anyway.
//!
//! A name keeps its source until `detach_source` forgets it, since clients work on a source by its name. Attaching
//! under a taken name changes nothing, and the response says what the name has: an ordinary result when it is the same
//! `Cargo.toml` with the access asked for or more (a read-only request meets a writable attachment: the response says
//! that a writable source is active), a tool error when it is another `Cargo.toml`, with a hint to detach the name
//! first. The one change is attaching the same `Cargo.toml` for writing, which makes a read-only attachment writable.
//!
//! Every call loads the workspace from disk again, so the server never works with stale source, and runs on a
//! fresh thread: `proc_macro2` keeps the locations of everything parsed on a thread in a thread-local map that only
//! ever grows, until the thread exits. Editing calls run one at a time. Failures are reported to the client as tool
//! errors, which the model sees, rather than as protocol errors.
//!
//! Nothing but the protocol is written to stdout.

mod input;
mod params;
mod render;
mod server;
mod sources;
mod tools;
mod worker;

#[cfg(test)]
mod tests;

use crate::Error;
use crate::workspace::LoadOptions;
use futures::channel::oneshot;
use futures::future;
use futures::future::Either;
use input::CheckedInput;
use rmcp::RoleServer;
use rmcp::ServiceExt;
use rmcp::service::ServerInitializeError;
use rmcp::transport::IntoTransport;
use server::Server;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::Mutex;

pub use sources::Access;
pub use sources::Exposure;
pub use sources::ExposureError;

/// How long a server whose client is gone waits for a running edit to finish.
const EDIT_GRACE_PERIOD: Duration = Duration::from_secs(30);

/// Configuration of the server.
#[derive(Debug, Default, Clone)]
pub struct ServerOptions {
	/// How the workspace is loaded for every tool call (tool arguments can narrow the package selection).
	pub load: LoadOptions,

	/// Refuse tools that modify files.
	pub read_only: bool,

	/// Directories whose cargo workspaces and packages clients may attach as sources (see [Sources](self#sources)).
	pub exposed: Vec<Exposure>,
}

/// A failure of the MCP connection itself (not of a tool call).
fn connection_error(kind: ErrorKind, message: String) -> Error {
	Error::Io {
		path: PathBuf::from("MCP connection"),
		source: std::io::Error::new(kind, message),
	}
}

/// Waits until no edit is running, but no longer than `grace`: an edit that hangs (e.g. on a file lock of cargo's)
/// must not keep the process alive forever. Returns whether the edits finished.
///
/// The deadline is a thread rather than a `tokio` timer, which would need a runtime with the time driver enabled.
async fn edits_finished(edits: &Mutex<()>, grace: Duration) -> bool {
	let (expire, expired) = oneshot::channel::<()>();

	std::thread::spawn(move || {
		std::thread::sleep(grace);

		let _ = expire.send(());
	});

	let idle = std::pin::pin!(edits.lock());

	matches!(future::select(idle, expired).await, Either::Left(_))
}

/// The outcome of a session that failed to start: a client that left before the session started is not an error.
fn handshake_failure(error: ServerInitializeError) -> Result<(), Error> {
	let kind = match &error {
		ServerInitializeError::ConnectionClosed(_) => return Ok(()),
		ServerInitializeError::TransportError { error, .. } => io_error_kind(error.error.as_ref()),
		ServerInitializeError::ExpectedInitializeRequest(_) | ServerInitializeError::UnexpectedInitializeResponse(_) => ErrorKind::InvalidData,
		_ => ErrorKind::Other,
	};

	match kind {
		ErrorKind::BrokenPipe => Ok(()),
		kind => Err(connection_error(kind, format!("failed to start the session: {error}"))),
	}
}

/// The kind of the I/O error that caused an error, if any.
fn io_error_kind(error: &(dyn std::error::Error + 'static)) -> ErrorKind {
	std::iter::successors(Some(error), |error| error.source())
		.find_map(|cause| cause.downcast_ref::<std::io::Error>())
		.map_or(ErrorKind::Other, std::io::Error::kind)
}

async fn run<T, E, A>(server: Server, transport: T) -> Result<(), Error>
where
	T: IntoTransport<RoleServer, E, A>,
	E: std::error::Error + Send + Sync + 'static,
{
	let edits = server.edit_lock();

	let running = match server.serve(transport).await {
		Ok(running) => running,
		Err(error) => return handshake_failure(error),
	};

	let stopped = running.waiting().await;

	// an edit may still be writing on its worker thread: let it finish before the process may exit
	edits_finished(&edits, EDIT_GRACE_PERIOD).await;

	match stopped {
		Ok(_) => Ok(()),
		Err(error) => Err(connection_error(ErrorKind::Other, format!("the server stopped unexpectedly: {error}"))),
	}
}

/// Serves MCP over any transport of `rmcp` (for example a `(reader, writer)` pair of `tokio` I/O objects) until the
/// client disconnects.
///
/// A client that disconnects before initializing the session is not an error. When the client is gone, an edit
/// that is still running gets up to half a minute to finish writing before this returns.
pub async fn serve<T, E, A>(options: ServerOptions, transport: T) -> Result<(), Error>
where
	T: IntoTransport<RoleServer, E, A>,
	E: std::error::Error + Send + Sync + 'static,
{
	run(Server::new(options), transport).await
}

/// Serves MCP over stdin/stdout until the client disconnects.
///
/// Lines of input that are not JSON get no answer (there is no request id to answer), and are reported on stderr.
pub async fn serve_stdio(options: ServerOptions) -> Result<(), Error> {
	let input = CheckedInput::new(tokio::io::stdin(), |message| eprintln!("{message}"));

	serve(options, (input, tokio::io::stdout())).await
}
