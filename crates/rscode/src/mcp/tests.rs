//! Tests of the server as a whole: the tools it offers, and sessions with a client over an in-process pipe.

use super::*;
use rmcp::ServerHandler;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::DuplexStream;
use tokio::io::Lines;
use tokio::io::ReadHalf;
use tokio::io::WriteHalf;
use tokio::task::JoinHandle;

const EDIT_TOOLS: [&str; 5] = ["format_items", "insert_items", "remove_items", "rename_item", "replace_item"];
const QUERY_TOOLS: [&str; 3] = ["find_items", "view_items", "workspace_info"];

/// Parameters every tool accepts.
const SELECTION: [&str; 7] = ["packages", "workspace", "features", "all_features", "all_targets", "lib", "bin"];

const SOURCE_TOOLS: [&str; 3] = ["attach_source", "detach_source", "list_sources"];

/// A client talking to a server over an in-process pipe, with newline-delimited JSON-RPC like stdio.
struct Client {
	lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
	writer: WriteHalf<DuplexStream>,
	server: JoinHandle<Result<(), Error>>,
	next_id: u64,
}

impl Client {
	/// Starts a server and initializes a session with it.
	async fn connect(options: ServerOptions) -> Self {
		Self::connect_to(Server::new(options)).await
	}

	async fn connect_to(server: Server) -> Self {
		let (client, server_end) = tokio::io::duplex(1 << 20);
		let server = tokio::spawn(run(server, server_end));
		let (reader, writer) = tokio::io::split(client);
		let mut client = Self {
			lines: BufReader::new(reader).lines(),
			writer,
			server,
			next_id: 0,
		};
		let initialize = json!({
			"protocolVersion": "2025-06-18",
			"capabilities": {},
			"clientInfo": { "name": "rscode-tests", "version": "0.0.0" },
		});
		let response = client.request("initialize", initialize).await;

		assert_eq!(response["result"]["serverInfo"]["name"], "rscode", "{response}");
		client.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
		client
	}

	/// Calls a tool: whether it failed, and its text.
	async fn call(&mut self, tool: &str, arguments: Value) -> (bool, String) {
		let response = self.request("tools/call", json!({ "name": tool, "arguments": arguments })).await;
		let result = &response["result"];
		let content = result["content"].as_array().unwrap_or_else(|| panic!("not a tool result: {response}"));
		let text: Vec<&str> = content.iter().filter_map(|content| content["text"].as_str()).collect();

		(result["isError"] == true, text.join("\n"))
	}

	/// Hangs up, and returns how the server ended.
	async fn close(mut self) -> Result<(), Error> {
		self.writer.shutdown().await.unwrap();
		drop(self.lines);
		self.server.await.unwrap()
	}

	/// Sends a request and waits for its response (skipping notifications).
	async fn request(&mut self, method: &str, params: Value) -> Value {
		self.next_id += 1;

		let id = self.next_id;

		self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await;

		loop {
			let line = tokio::time::timeout(Duration::from_secs(60), self.lines.next_line())
				.await
				.expect("no response within a minute")
				.unwrap()
				.expect("the server closed the connection");
			let message: Value = serde_json::from_str(&line).unwrap();

			if message["id"] == id {
				return message;
			}
		}
	}

	async fn send(&mut self, message: Value) {
		let line = format!("{message}\n");

		self.writer.write_all(line.as_bytes()).await.unwrap();
	}

	async fn tool_names(&mut self) -> Vec<String> {
		let response = self.request("tools/list", json!({})).await;
		let tools = response["result"]["tools"].as_array().unwrap();

		sorted(tools.iter().map(|tool| tool["name"].as_str().unwrap()))
	}
}

#[tokio::test]
async fn a_session_without_a_handshake_is_an_error() {
	let (mut client, server) = tokio::io::duplex(1 << 16);

	client
		.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n")
		.await
		.unwrap();

	let error = serve(ServerOptions::default(), server).await.unwrap_err();

	assert!(error.to_string().starts_with("MCP connection: failed to start the session"), "{error}");
}

fn all_tools() -> Vec<String> {
	sorted(QUERY_TOOLS.iter().chain(&EDIT_TOOLS))
}

#[test]
fn annotations() {
	let server = Server::new(ServerOptions::default());

	for tool in server.tools() {
		let annotations = tool.annotations.as_ref().unwrap_or_else(|| panic!("{} has no annotations", tool.name));
		let read_only = QUERY_TOOLS.contains(&tool.name.as_ref());

		assert!(annotations.title.is_some(), "{}", tool.name);
		assert_eq!(annotations.read_only_hint, Some(read_only), "{}", tool.name);
		assert_eq!(annotations.open_world_hint, Some(false), "{}", tool.name);

		match tool.name.as_ref() {
			"remove_items" | "replace_item" => assert_eq!(annotations.destructive_hint, Some(true)),
			"rename_item" | "insert_items" | "format_items" => assert_eq!(annotations.destructive_hint, Some(false)),
			_ => assert_eq!(annotations.idempotent_hint, Some(true), "{}", tool.name),
		}
	}

	assert_eq!(server.get_tool("format_items").unwrap().annotations.unwrap().idempotent_hint, Some(true));
}

#[test]
fn enumerations_and_defaults_are_in_the_schemas() {
	let server = Server::new(ServerOptions::default());
	let property = |tool: &str, name: &str| {
		let schema = Value::Object((*server.get_tool(tool).unwrap().input_schema).clone());

		schema["properties"][name].clone()
	};

	assert_eq!(property("view_items", "mode")["enum"], json!(["auto", "full", "outline"]));
	assert_eq!(property("view_items", "mode")["default"], "auto");
	assert_eq!(property("view_items", "line_numbers")["default"], true);
	assert_eq!(property("insert_items", "position")["enum"], json!(["end", "start", "before", "after"]));
	assert_eq!(property("format_items", "formatter")["enum"], json!(["rustfmt", "prettyplease", "none"]));
	assert_eq!(property("format_items", "targets")["default"], json!(["crate"]));
	assert_eq!(property("format_items", "sort")["default"], true);
	assert_eq!(property("find_items", "limit")["default"], 100);
}

/// Options exposing a directory for reading.
fn exposing() -> ServerOptions {
	ServerOptions {
		exposed: vec!["read=/nonexistent/refs/*".parse().unwrap()],
		..ServerOptions::default()
	}
}

#[test]
fn exposing_directories_offers_sources() {
	let server = Server::new(exposing());
	let tools = server.tools();

	assert_eq!(
		sorted(tools.iter().map(|tool| &tool.name)),
		sorted(all_tools().iter().chain(&sorted(SOURCE_TOOLS)))
	);

	for tool in &tools {
		let schema = Value::Object((*tool.input_schema).clone());
		let attached = schema["properties"].get("attached").is_some();

		assert_eq!(attached, !SOURCE_TOOLS.contains(&tool.name.as_ref()), "{}: {schema}", tool.name);
	}

	// without exposed directories, neither the tools nor the parameter are offered
	for tool in Server::new(ServerOptions::default()).tools() {
		assert!(
			Value::Object((*tool.input_schema).clone())["properties"].get("attached").is_none(),
			"{}",
			tool.name
		);
	}

	// the parameters of every tool are complete and their own: the item code of `replace_item` and `insert_items`
	// (`source`) is not taken over by another parameter
	for server in [Server::new(ServerOptions::default()), Server::new(exposing())] {
		for tool in server.tools() {
			let schema = Value::Object((*tool.input_schema).clone());

			for name in schema["required"].as_array().into_iter().flatten() {
				let property = &schema["properties"][name.as_str().unwrap()];

				assert!(property.is_object(), "{}: `{name}` is required, but not in {schema}", tool.name);
				assert!(property["description"].as_str().unwrap_or_default().len() > 10, "{}.{name}", tool.name);
			}
		}

		for tool in ["replace_item", "insert_items"] {
			let schema = Value::Object((*server.get_tool(tool).unwrap().input_schema).clone());

			assert_eq!(schema["properties"]["source"]["type"], "string", "{tool}: {schema}");
		}
	}

	let read_only = Server::new(ServerOptions {
		read_only: true,
		..exposing()
	});

	assert_eq!(
		sorted(read_only.tools().iter().map(|tool| &tool.name)),
		sorted(QUERY_TOOLS.iter().chain(&SOURCE_TOOLS))
	);
}

#[tokio::test]
async fn failures_are_tool_errors() {
	let mut client = Client::connect(ServerOptions::default()).await;

	// checked before anything is loaded
	let (failed, text) = client.call("find_items", json!({ "pattern": "x", "kinds": "fn, nope" })).await;

	assert!(failed);
	assert!(text.starts_with("unknown item kind `nope`; expected one of: mod, struct"), "{text}");

	let (failed, text) = client
		.call("insert_items", json!({ "parent": "crate", "source": "fn f() {}", "position": "before" }))
		.await;

	assert!(failed);
	assert!(text.contains("`anchor`"), "{text}");

	let (failed, text) = client.call("format_items", json!({ "formatter": "none", "sort": false })).await;

	assert!(failed);
	assert!(text.starts_with("nothing to do"), "{text}");

	// arguments that do not match the schema
	let (failed, text) = client.call("view_items", json!({})).await;

	assert!(failed);
	assert!(text.contains("missing field `paths`"), "{text}");

	let (failed, text) = client.call("view_items", json!({ "paths": ["crate"], "mode": "everything" })).await;

	assert!(failed);
	assert!(text.contains("unknown variant `everything`"), "{text}");

	// an unknown tool is a protocol error
	let response = client.request("tools/call", json!({ "name": "nope", "arguments": {} })).await;

	assert_eq!(response["error"]["code"], -32602, "{response}");
	client.close().await.unwrap();
}

#[test]
fn handshake_failures() {
	let kind = |result: Result<(), Error>| match result {
		Err(Error::Io { path, source }) => {
			assert_eq!(path, Path::new("MCP connection"));
			assert!(source.to_string().starts_with("failed to start the session: "), "{source}");
			source.kind()
		}

		other => panic!("not a connection error: {other:?}"),
	};
	let transport = |kind: ErrorKind| ServerInitializeError::TransportError {
		error: rmcp::transport::DynamicTransportError::from_parts("test", std::any::TypeId::of::<()>(), Box::new(std::io::Error::from(kind))),
		context: "sending the initialize result".into(),
	};

	// the client left early
	assert!(handshake_failure(ServerInitializeError::ConnectionClosed("initialize request".to_owned())).is_ok());
	assert!(handshake_failure(transport(ErrorKind::BrokenPipe)).is_ok());

	// the kind of an I/O error is kept, so that callers can tell failures apart
	assert_eq!(
		kind(handshake_failure(transport(ErrorKind::PermissionDenied))),
		ErrorKind::PermissionDenied
	);
	assert_eq!(
		kind(handshake_failure(ServerInitializeError::ExpectedInitializeRequest(None))),
		ErrorKind::InvalidData
	);
	assert_eq!(kind(handshake_failure(ServerInitializeError::Cancelled)), ErrorKind::Other);
}

#[tokio::test]
async fn hanging_up_before_initializing_is_not_an_error() {
	let (client, server) = tokio::io::duplex(1024);

	drop(client);
	serve(ServerOptions::default(), server).await.unwrap();
}

#[test]
fn input_schemas_are_objects_with_described_properties() {
	for tool in Server::new(ServerOptions::default()).tools() {
		let schema = Value::Object((*tool.input_schema).clone());
		let properties = schema["properties"].as_object().unwrap_or_else(|| panic!("{}: {schema}", tool.name));

		assert_eq!(schema["type"], "object", "{}", tool.name);

		for name in SELECTION {
			assert!(properties.contains_key(name), "{} lacks `{name}`", tool.name);
		}

		for (name, property) in properties {
			let description = property["description"].as_str().unwrap_or_default();

			assert!(description.len() > 10, "{}.{name} is not described: {property}", tool.name);
		}

		// clients that don't resolve references still see every parameter's type
		assert!(!schema.to_string().contains("$ref"), "{}: {schema}", tool.name);

		let description = tool.description.as_deref().unwrap_or_default();

		assert!(description.len() > 100, "{} is not described", tool.name);
	}
}

#[test]
fn io_error_kinds_are_found_in_causes() {
	#[derive(Debug)]
	struct Wrapper(std::io::Error);

	impl std::fmt::Display for Wrapper {
		fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
			f.write_str("wrapped")
		}
	}

	impl std::error::Error for Wrapper {
		fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
			Some(&self.0)
		}
	}

	assert_eq!(io_error_kind(&Wrapper(ErrorKind::TimedOut.into())), ErrorKind::TimedOut);
	assert_eq!(io_error_kind(&std::io::Error::from(ErrorKind::BrokenPipe)), ErrorKind::BrokenPipe);
	assert_eq!(io_error_kind(&std::fmt::Error), ErrorKind::Other);
}

#[tokio::test]
async fn lists_tools() {
	let mut client = Client::connect(ServerOptions::default()).await;
	let response = client.request("tools/list", json!({})).await;

	for tool in response["result"]["tools"].as_array().unwrap() {
		assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
	}

	assert_eq!(client.tool_names().await, all_tools());
	client.close().await.unwrap();
}

#[test]
fn offers_every_tool() {
	let server = Server::new(ServerOptions::default());

	assert_eq!(sorted(server.tools().iter().map(|tool| &tool.name)), all_tools());
}

fn read_only() -> ServerOptions {
	ServerOptions {
		read_only: true,
		..ServerOptions::default()
	}
}

#[test]
fn read_only_servers_do_not_offer_editing_tools() {
	let server = Server::new(read_only());

	assert_eq!(sorted(server.tools().iter().map(|tool| &tool.name)), sorted(QUERY_TOOLS));

	for tool in EDIT_TOOLS {
		assert!(server.get_tool(tool).is_none(), "{tool}");
	}
}

#[tokio::test]
async fn read_only_servers_refuse_edits() {
	let mut client = Client::connect(read_only()).await;

	assert_eq!(client.tool_names().await, sorted(QUERY_TOOLS));

	for tool in EDIT_TOOLS {
		let (failed, text) = client.call(tool, json!({ "path": "crate::a", "new_name": "b", "dry_run": true })).await;

		assert!(failed, "{tool}");
		assert_eq!(
			text,
			format!("`{tool}` modifies files, but this rscode server is read-only (it was started with `--read-only`)")
		);
	}

	client.close().await.unwrap();
}

#[test]
fn required_parameters() {
	let expected = [
		("find_items", vec!["pattern"]),
		("format_items", vec![]),
		("insert_items", vec!["parent", "source"]),
		("remove_items", vec!["paths"]),
		("rename_item", vec!["new_name", "path"]),
		("replace_item", vec!["path", "source"]),
		("view_items", vec!["paths"]),
		("workspace_info", vec![]),
	];
	let server = Server::new(ServerOptions::default());

	for (name, required) in expected {
		let tool = server.get_tool(name).unwrap();
		let schema = Value::Object((*tool.input_schema).clone());
		let actual = schema["required"]
			.as_array()
			.map(|required| sorted(required.iter().map(|name| name.as_str().unwrap())));

		assert_eq!(actual.unwrap_or_default(), sorted(required), "{name}");
	}
}

#[test]
fn server_info() {
	let info = Server::new(ServerOptions::default()).get_info();

	assert_eq!(info.server_info.name, "rscode");
	assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
	assert!(info.capabilities.tools.is_some());

	let instructions = info.instructions.unwrap();

	for needle in [
		"crate::module::Item",
		"::crate_name::Item",
		"`Type::method`",
		"<Type as Trait>::method",
		"impl Trait for Type",
		"`**`",
		"matches anywhere",
		"all-or-nothing",
		"must still parse",
		"dry_run",
		"loaded from disk again for every call",
		"`use module::Name`",
	] {
		assert!(instructions.contains(needle), "the instructions do not mention {needle}");
	}

	assert!(!instructions.contains("read-only"));
	assert!(
		Server::new(read_only())
			.get_info()
			.instructions
			.unwrap()
			.ends_with("the editing tools are disabled.")
	);
}

#[tokio::test]
async fn servers_without_exposed_directories_explain_the_source_tools() {
	let mut client = Client::connect(ServerOptions::default()).await;

	for tool in SOURCE_TOOLS {
		let (failed, text) = client.call(tool, json!({ "name": "a", "manifest_path": "/a/Cargo.toml" })).await;

		assert!(failed, "{tool}");
		assert!(text.contains("exposes no directories to attach sources from"), "{tool}: {text}");
	}

	// a source is never ignored, even where the parameter is not offered
	let (failed, text) = client.call("workspace_info", json!({ "attached": "a" })).await;

	assert!(failed);
	assert!(
		text.starts_with("no source is attached as `a`: this rscode server exposes no directories"),
		"{text}"
	);
	assert!(!text.contains("attach_source"), "{text}");
	client.close().await.unwrap();
}

#[tokio::test]
async fn shutting_down_waits_for_running_edits() {
	let server = Server::new(ServerOptions::default());
	let edit = server.edit_lock().lock_owned().await;
	let client = Client::connect_to(server).await;
	let closing = tokio::spawn(client.close());

	tokio::time::sleep(Duration::from_millis(200)).await;
	assert!(!closing.is_finished(), "the server stopped while an edit was running");

	drop(edit);
	tokio::time::timeout(Duration::from_secs(10), closing).await.unwrap().unwrap().unwrap();
}

fn sorted(names: impl IntoIterator<Item = impl ToString>) -> Vec<String> {
	let mut names: Vec<String> = names.into_iter().map(|name| name.to_string()).collect();

	names.sort();
	names
}

#[test]
fn source_tools() {
	let server = Server::new(exposing());
	let schema = |name: &str| Value::Object((*server.get_tool(name).unwrap().input_schema).clone());
	let required = |name: &str| schema(name)["required"].as_array().cloned().unwrap_or_default();

	assert_eq!(
		sorted(required("attach_source").iter().map(|name| name.as_str().unwrap())),
		["manifest_path", "name"]
	);
	assert_eq!(required("detach_source"), [json!("name")]);
	assert_eq!(required("list_sources"), Vec::<Value>::new());
	assert_eq!(schema("attach_source")["properties"]["write"]["default"], false);

	for name in SOURCE_TOOLS {
		let tool = server.get_tool(name).unwrap();
		let annotations = tool.annotations.unwrap();

		assert!(tool.description.unwrap().len() > 100, "{name} is not described");
		assert!(annotations.title.is_some(), "{name}");
		assert_eq!(annotations.read_only_hint, Some(name == "list_sources"), "{name}");
		assert!(!annotations.destructive_hint.unwrap_or_default(), "{name}");
		assert_eq!(annotations.idempotent_hint, Some(true), "{name}");
		assert_eq!(annotations.open_world_hint, Some(false), "{name}");
	}

	let instructions = server.get_info().instructions.unwrap();
	// patterns are listed resolved: on Windows, with a drive and `\`
	let resolved = |exposure: &str| exposure.parse::<Exposure>().unwrap().resolved_pattern().to_owned();

	assert!(instructions.contains("attach_source"), "{instructions}");
	assert!(instructions.contains("pass that name as `attached`"), "{instructions}");
	assert!(
		instructions.ends_with(&format!("\n- read: {}", resolved("read=/nonexistent/refs/*"))),
		"{instructions}"
	);
	assert!(
		!Server::new(ServerOptions::default())
			.get_info()
			.instructions
			.unwrap()
			.contains("attach_source")
	);

	// a read-only server does not offer writing
	let exposed = vec!["write=/nonexistent/engine".parse().unwrap()];
	let writable = Server::new(ServerOptions {
		exposed: exposed.clone(),
		..ServerOptions::default()
	});
	let read_only = Server::new(ServerOptions {
		exposed,
		read_only: true,
		..ServerOptions::default()
	});
	let writable = writable.get_info().instructions.unwrap();
	let read_only = read_only.get_info().instructions.unwrap();
	let engine = resolved("write=/nonexistent/engine");

	assert!(writable.contains("`write` if you need to edit them") && writable.ends_with(&format!("- write: {engine}")));
	assert!(
		!read_only.contains("`write`") && read_only.ends_with(&format!("- read: {engine}")),
		"{read_only}"
	);
}

#[test]
fn truncation_hints_name_the_tools_own_parameters() {
	for tool in Server::new(ServerOptions::default()).tools() {
		let schema = Value::Object((*tool.input_schema).clone()).to_string();
		let hint = render::truncation_hint(&tool.name);

		// every other piece of the hint is quoted: parameters, and values of enumerations
		for word in hint.split('`').skip(1).step_by(2) {
			assert!(schema.contains(&format!("\"{word}\"")), "{}: `{word}` is not in {schema}", tool.name);
		}
	}
}

#[tokio::test]
async fn waiting_for_edits_is_bounded() {
	let edits = Mutex::new(());

	assert!(edits_finished(&edits, Duration::from_secs(60)).await);

	let _running = edits.lock().await;

	assert!(!edits_finished(&edits, Duration::from_millis(50)).await);
}

/// Sessions with fixture crates on disk, through every layer of rscode.
mod end_to_end {
	use super::*;

	/// Two `impl` blocks of `W`, with a `get` each.
	const IMPLS: &str = "\nimpl W<u8> {\n\tfn get() {}\n}\n\nimpl W<u16> {\n\tfn get() {}\n}\n";

	/// A cargo package in a temporary directory, deleted on drop.
	struct Fixture {
		root: PathBuf,
	}

	impl Fixture {
		fn new(name: &str) -> Self {
			let files = [
				(
					"Cargo.toml",
					"[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nextra = []\n\n[workspace]\n",
				),
				(
					"src/lib.rs",
					"//! A demo.\n\npub mod shapes;\n\npub use shapes::Circle;\n\n/// Adds two numbers.\npub fn add(a: i32, b: i32) -> \
					 i32 {\n\ta + b\n}\n\n#[cfg(feature = \"extra\")]\npub fn extra() {}\n\npub fn three() -> i32 {\n\tadd(1, 2)\n}\n",
				),
				(
					"src/shapes.rs",
					"/// A circle.\npub struct Circle {\n\tpub radius: f64,\n}\n\nimpl Circle {\n\tpub fn new(radius: f64) -> Self {\n\t\tSelf \
					 { radius }\n\t}\n\n\tpub fn area(&self) -> f64 {\n\t\t3.0 * self.radius * self.radius\n\t}\n}\n",
				),
			];

			Self::with_files(name, &files)
		}

		fn with_files(name: &str, files: &[(&str, &str)]) -> Self {
			let root = std::env::temp_dir().join(format!("rscode-mcp-{name}-{}", std::process::id()));
			let _ = std::fs::remove_dir_all(&root);

			for (path, text) in files {
				let path = root.join(path);

				std::fs::create_dir_all(path.parent().unwrap()).unwrap();
				std::fs::write(path, text).unwrap();
			}

			Self { root }
		}

		fn options(&self) -> ServerOptions {
			ServerOptions {
				load: LoadOptions {
					manifest_path: Some(self.root.join("Cargo.toml")),
					silent: true,
					..LoadOptions::default()
				},
				..ServerOptions::default()
			}
		}

		fn read(&self, path: &str) -> String {
			std::fs::read_to_string(self.root.join(path)).unwrap()
		}
	}

	impl Drop for Fixture {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.root);
		}
	}

	fn assert_contains(text: &str, needles: &[&str]) {
		for needle in needles {
			assert!(text.contains(needle), "{needle:?} is not in:\n{text}");
		}
	}

	#[tokio::test]
	async fn edits() {
		let fixture = Fixture::new("edits");
		let mut client = Client::connect(fixture.options()).await;
		let original = fixture.read("src/lib.rs");

		// a dry run writes nothing
		let (failed, text) = client
			.call("rename_item", json!({ "path": "crate::add", "new_name": "sum", "dry_run": true }))
			.await;

		assert!(!failed, "{text}");
		assert_contains(
			&text,
			&[
				"would rename 1 item to `sum`",
				"demo::add",
				"nothing was written",
				"-pub fn add",
				"+pub fn sum",
			],
		);
		assert_eq!(fixture.read("src/lib.rs"), original);

		let (failed, text) = client.call("rename_item", json!({ "path": "crate::add", "new_name": "sum" })).await;

		assert!(!failed, "{text}");
		assert_contains(&text, &["renamed 1 item to `sum`"]);
		assert_contains(&fixture.read("src/lib.rs"), &["pub fn sum(a: i32, b: i32)", "\tsum(1, 2)"]);

		let (failed, text) = client.call("remove_items", json!({ "paths": ["crate::extra"] })).await;

		assert!(!failed, "{text}");
		assert!(!fixture.read("src/lib.rs").contains("extra"));

		let source = "pub fn area(&self) -> f64 {\n\tstd::f64::consts::PI * self.radius * self.radius\n}";
		let (failed, text) = client
			.call("replace_item", json!({ "path": "crate::shapes::Circle::area", "source": source }))
			.await;

		assert!(!failed, "{text}");
		assert_contains(
			&fixture.read("src/shapes.rs"),
			&["\tpub fn area(&self) -> f64 {\n\t\tstd::f64::consts::PI"],
		);

		let source = "pub fn diameter(&self) -> f64 {\n\t2.0 * self.radius\n}";
		let (failed, text) = client
			.call(
				"insert_items",
				json!({ "parent": "impl crate::shapes::Circle", "source": source, "format": true }),
			)
			.await;

		assert!(!failed, "{text}");
		assert_contains(&fixture.read("src/shapes.rs"), &["pub fn diameter(&self) -> f64"]);

		// an edit that would break the syntax writes nothing
		let before = fixture.read("src/shapes.rs");
		let (failed, _) = client
			.call("replace_item", json!({ "path": "crate::shapes::Circle::new", "source": "pub fn new(" }))
			.await;

		assert!(failed);
		assert_eq!(fixture.read("src/shapes.rs"), before);

		let (failed, text) = client.call("format_items", json!({ "check": true })).await;

		assert!(!failed, "{text}");
		client.close().await.unwrap();
	}

	#[test]
	fn fixtures_are_cleaned_up() {
		let root = {
			let fixture = Fixture::new("cleanup");

			assert!(fixture.read("src/lib.rs").contains("pub fn add"));
			fixture.root.clone()
		};

		assert!(!Path::new(&root).exists());
	}

	/// Formatting after an edit formats what the edit touched: a replaced glob import, and an inserted `use` item.
	#[tokio::test]
	async fn formats_edited_imports() {
		let fixture = Fixture::with_files(
			"format-imports",
			&[
				(
					"Cargo.toml",
					"[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
				),
				("rustfmt.toml", "hard_tabs = true\n"),
				(
					"src/lib.rs",
					"pub mod shapes {\n\tpub struct Circle;\n}\n\nuse   shapes::{Circle};\nuse shapes::*;\n\npub fn make() -> Circle {\n\tCircle\n}\n",
				),
			],
		);
		let mut client = Client::connect(fixture.options()).await;
		let replaced = json!({ "path": "use crate::*", "source": "use   crate::shapes::*;", "format": true });
		let (failed, text) = client.call("replace_item", replaced).await;

		assert!(!failed, "{text}");
		assert!(
			fixture.read("src/lib.rs").contains("use   shapes::{Circle};\nuse crate::shapes::*;\n"),
			"{}",
			fixture.read("src/lib.rs")
		);

		let inserted = json!({ "parent": "crate", "source": "use   std::fmt::Debug;", "position": "start", "format": true });
		let (failed, text) = client.call("insert_items", inserted).await;

		assert!(!failed, "{text}");
		assert!(
			fixture.read("src/lib.rs").starts_with("use std::fmt::Debug;\n"),
			"{}",
			fixture.read("src/lib.rs")
		);
		client.close().await.unwrap();
	}

	/// Imports are named by `use` paths; a plain path through a private import is ambiguous for edits.
	#[tokio::test]
	async fn imports() {
		let fixture = Fixture::with_files(
			"imports",
			&[
				(
					"Cargo.toml",
					"[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
				),
				(
					"src/lib.rs",
					"pub mod shapes {\n\tpub struct Circle;\n}\n\nuse shapes::Circle;\n\npub fn make() -> Circle {\n\tCircle\n}\n",
				),
			],
		);
		let mut client = Client::connect(fixture.options()).await;

		let (failed, text) = client.call("find_items", json!({ "pattern": "use *" })).await;

		assert!(!failed, "{text}");
		assert_contains(
			&text,
			&[&native("use demo::Circle  import  src/lib.rs:5:5-5:19  -> demo::shapes::Circle")],
		);

		let (failed, text) = client.call("view_items", json!({ "paths": "use crate::Circle" })).await;

		assert!(!failed, "{text}");
		assert_contains(
			&text,
			&[&format!("// {}", native("use demo::Circle (import) src/lib.rs:5")), "use shapes::Circle;"],
		);

		let (failed, text) = client.call("remove_items", json!({ "paths": "crate::Circle", "dry_run": true })).await;

		assert!(failed);
		assert_contains(
			&text,
			&[
				"`crate::Circle` is ambiguous",
				&native("`use demo::Circle` (import) at src/lib.rs:5:5"),
				&native("`demo::shapes::Circle` (struct) at src/lib.rs:2:2"),
				"hint: the path names an item through a private import",
			],
		);

		let (failed, text) = client
			.call("remove_items", json!({ "paths": "use crate::Circle", "dry_run": true }))
			.await;

		assert!(!failed, "{text}");
		assert_contains(&text, &["-use shapes::Circle;", "nothing was written"]);

		// a `use` path of an import's own text names nothing, with a hint
		let (failed, text) = client
			.call("remove_items", json!({ "paths": "use crate::shapes::Circle", "dry_run": true }))
			.await;

		assert!(failed);
		assert_contains(
			&text,
			&["no item found for `use crate::shapes::Circle`", "hint: a `use` path names the imports of"],
		);

		let (_, text) = client.call("find_items", json!({ "pattern": "use Circl" })).await;

		assert_contains(&text, &["try `use *Circl*`"]);
		client.close().await.unwrap();
	}

	/// `text` with `/` replaced by the platform's path separator, which the tools write paths with (except in the
	/// headers of diffs).
	/// A name keeps its source: attaching under it again says what it has and changes nothing (except that attaching
	/// the same `Cargo.toml` for writing makes a read-only attachment writable), and a read-only attachment never
	/// replaces a writable one.
	#[tokio::test]
	async fn names_keep_their_sources() {
		let package = |name: &str| format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n");
		let fixture = Fixture::with_files(
			"names",
			&[
				("own/Cargo.toml", &package("own")),
				("own/src/lib.rs", "pub fn mine() {}\n"),
				("project/Cargo.toml", &package("engine")),
				("project/src/lib.rs", "pub fn run() {}\n"),
				("refs/log/Cargo.toml", &package("log")),
				("refs/log/src/lib.rs", "pub fn info() {}\n"),
			],
		);
		let root = sources::resolve(&fixture.root);
		let pattern = |pattern: &str| root.join(pattern).to_str().unwrap().to_owned();
		let options = ServerOptions {
			load: LoadOptions {
				manifest_path: Some(root.join("own/Cargo.toml")),
				silent: true,
				..LoadOptions::default()
			},
			exposed: vec![
				Exposure::new(Access::Write, &pattern("project")).unwrap(),
				Exposure::new(Access::Read, &pattern("refs/*")).unwrap(),
			],
			..ServerOptions::default()
		};
		let mut client = Client::connect(options).await;
		let attach =
			|path: &str, name: &str, write: bool| json!({ "manifest_path": root.join(path).to_str().unwrap(), "name": name, "write": write });
		let insert = |function: &str, name: &str| json!({ "parent": "crate", "source": format!("pub fn {function}() {{}}"), "attached": name });
		let project = root.join(native("project/Cargo.toml")).display().to_string();
		let log = root.join(native("refs/log/Cargo.toml")).display().to_string();

		let (failed, text) = client.call("attach_source", attach("project", "engine", true)).await;

		assert!(!failed, "{text}");
		assert_contains(
			&text,
			&[
				&format!("attached `engine` (read and write): {project}"),
				"packages loaded by default: engine 0.1.0",
			],
		);

		// asking for what the name has says so, and plans nothing
		let (failed, text) = client.call("attach_source", attach("project", "engine", true)).await;

		assert!(!failed, "{text}");
		assert_contains(
			&text,
			&[
				&format!("source `engine` is already attached (read and write): {project}"),
				"nothing changed",
				"pass `\"attached\": \"engine\"`",
			],
		);
		assert!(!text.contains("packages loaded by default"), "{text}");

		// a writable source that is active is not made read-only: the same Cargo.toml is answered with it
		let (failed, text) = client.call("attach_source", attach("project", "engine", false)).await;

		assert!(!failed, "{text}");
		assert_contains(
			&text,
			&[
				&format!("there is already a writable source attached as `engine` (read and write): {project}"),
				"nothing changed",
				"the editing tools can write to it",
				"attach the same Cargo.toml under another name",
			],
		);

		// and another Cargo.toml is refused, read-only or not, whatever the directory is exposed for
		for write in [false, true] {
			let (failed, text) = client.call("attach_source", attach("refs/log", "engine", write)).await;
			let already = match write {
				false => "there is already a writable source attached as `engine`",
				true => "source `engine` is already attached",
			};

			assert!(failed, "{text}");
			assert_contains(
				&text,
				&[
					&format!("{already} (read and write): {project}"),
					"nothing was attached",
					&format!("attach {log} under another name, or detach `engine` first with `detach_source`"),
				],
			);
		}

		// the writable source is still there, and still writable
		let (_, text) = client.call("list_sources", json!({})).await;

		assert_contains(&text, &[&format!("engine  read and write  {project}")]);
		assert!(!text.contains(&log), "{text}");

		let (failed, text) = client.call("insert_items", insert("halt", "engine")).await;

		assert!(!failed, "{text}");
		assert_contains(&fixture.read("project/src/lib.rs"), &["pub fn halt() {}"]);

		// a name of a read-only source keeps it, too
		let (failed, text) = client.call("attach_source", attach("refs/log", "log", false)).await;

		assert!(!failed, "{text}");
		assert_contains(&text, &[&format!("attached `log` (read-only): {log}")]);

		let (failed, text) = client.call("attach_source", attach("refs/log", "log", false)).await;

		assert!(!failed, "{text}");
		assert_contains(
			&text,
			&[&format!("source `log` is already attached (read-only): {log}"), "nothing changed"],
		);

		let (failed, text) = client.call("attach_source", attach("project", "log", false)).await;

		assert!(failed, "{text}");
		assert_contains(
			&text,
			&[
				&format!("source `log` is already attached (read-only): {log}"),
				"nothing was attached",
				&format!("attach {project} under another name, or detach `log` first with `detach_source`"),
			],
		);

		// writing is checked like for any attachment, and the read-only source stays as it is
		let (failed, text) = client.call("attach_source", attach("refs/log", "log", true)).await;

		assert!(failed, "{text}");
		assert_contains(&text, &["is only exposed for reading"]);

		let (failed, text) = client.call("insert_items", insert("warn", "log")).await;

		assert!(failed, "{text}");
		assert_contains(&text, &["source `log` is attached read-only"]);

		// attaching the same Cargo.toml for writing makes a read-only source writable
		let (failed, text) = client.call("attach_source", attach("project", "p", false)).await;

		assert!(!failed, "{text}");

		let (failed, text) = client.call("insert_items", insert("freeze", "p")).await;

		assert!(failed, "{text}");
		assert_contains(&text, &["source `p` is attached read-only"]);

		let (failed, text) = client.call("attach_source", attach("project", "p", true)).await;

		assert!(!failed, "{text}");
		assert_contains(
			&text,
			&[
				&format!("attached `p` (read and write): {project}"),
				"it was attached read-only before",
				"packages loaded by default: engine 0.1.0",
			],
		);

		let (failed, text) = client.call("insert_items", insert("freeze", "p")).await;

		assert!(!failed, "{text}");
		assert_contains(&fixture.read("project/src/lib.rs"), &["pub fn halt() {}", "pub fn freeze() {}"]);

		// but never back
		let (failed, text) = client.call("attach_source", attach("project", "p", false)).await;

		assert!(!failed, "{text}");
		assert_contains(&text, &["there is already a writable source attached as `p` (read and write)"]);

		let (failed, text) = client.call("insert_items", insert("thaw", "p")).await;

		assert!(!failed, "{text}");

		// detaching frees the name
		let (failed, text) = client.call("detach_source", json!({ "name": "engine" })).await;

		assert!(!failed, "{text}");

		let (failed, text) = client.call("attach_source", attach("refs/log", "engine", false)).await;

		assert!(!failed, "{text}");
		assert_contains(&text, &[&format!("attached `engine` (read-only): {log}")]);
		assert!(!text.contains("already"), "{text}");
		client.close().await.unwrap();
	}

	fn native(text: &str) -> String {
		text.replace('/', std::path::MAIN_SEPARATOR_STR)
	}

	#[tokio::test]
	async fn points_to_what_is_not_loaded() {
		let package = |name: &str, workspace: &str| format!("[package]\nname = \"{name}\"\nversion = \"0.2.0\"\nedition = \"2024\"\n{workspace}");
		let fixture = Fixture::with_files(
			"members",
			&[
				("Cargo.toml", &package("demo", "\n[workspace]\nmembers = [\"helper\"]\n")),
				("src/lib.rs", &format!("pub fn add() {{}}\n\npub struct W<T>(T);\n{IMPLS}")),
				("src/main.rs", "fn main() {}\n"),
				("helper/Cargo.toml", &package("helper", "")),
				("helper/src/lib.rs", "pub fn assist() {}\n"),
			],
		);
		let mut client = Client::connect(fixture.options()).await;

		// the member that is not selected is listed
		let (failed, text) = client.call("workspace_info", json!({})).await;

		assert!(!failed, "{text}");
		assert_contains(
			&text,
			&[
				&native("helper 0.2.0  helper/Cargo.toml  (not selected, so not loaded; crates: helper)"),
				"set `workspace` to true",
			],
		);

		// and named when paths or patterns match nothing
		let (failed, text) = client.call("view_items", json!({ "paths": ["::helper::assist"] })).await;

		assert!(failed);
		assert_contains(
			&text,
			&[
				"no item found for `::helper::assist`",
				"`helper` is a workspace member that is not selected",
			],
		);
		assert!(!text.contains("find_items"), "{text}");

		let (_, text) = client.call("find_items", json!({ "pattern": "assist" })).await;

		assert_contains(&text, &["no items match", "the workspace member `helper` is not selected"]);

		let (_, text) = client.call("find_items", json!({ "pattern": "assist", "packages": "helper" })).await;

		assert_contains(&text, &["helper::assist  fn"]);

		// `crate` is the root of the library and of the binary: `lib` or `bin` picks one
		let (failed, text) = client
			.call("insert_items", json!({ "parent": "crate", "source": "pub fn q() {}", "dry_run": true }))
			.await;

		assert!(failed);
		assert_contains(
			&text,
			&[
				&native("(lib crate root) at src/lib.rs:1:1"),
				&native("(bin crate root) at src/main.rs:1:1"),
				"select one with `lib`",
			],
		);

		let (failed, text) = client
			.call(
				"insert_items",
				json!({ "parent": "crate", "source": "pub fn q() {}", "dry_run": true, "lib": true }),
			)
			.await;

		assert!(!failed, "{text}");
		assert_contains(&text, &["+++ b/src/lib.rs", "+pub fn q() {}"]);

		let (failed, text) = client
			.call(
				"insert_items",
				json!({ "parent": "crate", "source": "fn q() {}", "dry_run": true, "bin": "demo" }),
			)
			.await;

		assert!(!failed, "{text}");
		assert_contains(&text, &["+++ b/src/main.rs", "+fn q() {}"]);

		// a collision names the parameter that overrides it
		let (failed, text) = client
			.call("insert_items", json!({ "parent": "crate", "source": "pub fn add() {}", "lib": true }))
			.await;

		assert!(failed);
		assert_contains(&text, &["collides with existing names", "hint: set `force` to proceed anyway"]);

		// `all_variants` is for `cfg` variants, not for `impl` blocks whose headers differ
		let replace = json!({ "path": "crate::W::get", "source": "fn get() {}", "all_variants": true, "lib": true });
		let (failed, text) = client.call("replace_item", replace).await;

		assert!(failed);
		assert_contains(
			&text,
			&[
				&native("`<demo::W<u16>>::get` (assoc-fn) at src/lib.rs:10:2"),
				"hint: use one of the candidates'",
			],
		);
		assert!(!text.contains("all_variants"), "{text}");
		client.close().await.unwrap();
	}

	#[tokio::test]
	async fn queries() {
		let fixture = Fixture::new("queries");
		let mut client = Client::connect(fixture.options()).await;

		let (failed, text) = client.call("workspace_info", json!({})).await;

		assert!(!failed, "{text}");
		assert_contains(
			&text,
			&[
				"demo 0.1.0  Cargo.toml",
				"extra",
				&native("demo  lib  src/lib.rs  edition 2024"),
				"load problems: none",
			],
		);

		let (failed, text) = client.call("find_items", json!({ "pattern": "*", "kinds": ["fn", "assoc-fn"] })).await;

		assert!(!failed, "{text}");
		assert_contains(
			&text,
			&[
				&native("demo::add  fn  src/lib.rs:7:1-10:2"),
				&native("demo::extra  fn  src/lib.rs:12:1-13:18  cfg: feature = \"extra\"  inactive"),
				&native("demo::shapes::Circle::new  assoc-fn  src/shapes.rs:"),
				"5 matches",
			],
		);

		let (_, text) = client
			.call("find_items", json!({ "pattern": "*", "kinds": "fn", "limit": 1, "offset": 1 }))
			.await;

		assert_contains(&text, &["3 matches; showing 2-2; for more, call again with `offset` 2"]);

		let (_, text) = client
			.call("find_items", json!({ "pattern": "Circle", "kinds": "struct", "from": "::" }))
			.await;

		assert_contains(&text, &["demo::shapes::Circle  struct", "usable: ::demo::Circle"]);

		let (_, text) = client.call("find_items", json!({ "pattern": "extra", "features": "extra" })).await;

		assert!(!text.contains("inactive"), "{text}");

		let (failed, text) = client.call("view_items", json!({ "paths": ["crate::add", "crate::nope"] })).await;

		assert!(!failed, "{text}");
		assert_contains(
			&text,
			&[
				"// error: no item found for `crate::nope`",
				&format!("// {}", native("demo::add (fn) src/lib.rs:7-10")),
				"a + b",
				"│",
			],
		);

		let (failed, text) = client.call("view_items", json!({ "paths": "crate::nope" })).await;

		assert!(failed);
		assert_contains(&text, &["no item found for `crate::nope`", "hint: search with `find_items`"]);
		client.close().await.unwrap();
	}

	#[tokio::test]
	async fn sources() {
		let package = |name: &str| format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n");
		let fixture = Fixture::with_files(
			"sources",
			&[
				("own/Cargo.toml", &package("own")),
				("own/src/lib.rs", "pub fn mine() {}\n"),
				("project/Cargo.toml", &package("engine")),
				(
					"project/src/lib.rs",
					"#[path = \"../../outside/escape.rs\"]\npub mod escape;\n\npub fn run() {}\n\npub fn go() {\n\trun();\n}\n",
				),
				("outside/escape.rs", "pub fn escaped() {}\n"),
				("refs/log/Cargo.toml", &package("log")),
				("refs/log/src/lib.rs", "pub fn info() {}\n"),
				("secret/Cargo.toml", &package("secret")),
				("secret/src/lib.rs", "pub fn hidden() {}\n"),
			],
		);
		// resolved like the manifests of attached sources: on Windows, `C:\...` rather than `\\?\C:\...`
		let root = sources::resolve(&fixture.root);
		let pattern = |pattern: &str| root.join(pattern).to_str().unwrap().to_owned();
		let project = Exposure::new(Access::Write, &pattern("project")).unwrap();
		let refs = Exposure::new(Access::Read, &pattern("refs/*")).unwrap();
		let options = ServerOptions {
			load: LoadOptions {
				manifest_path: Some(root.join("own/Cargo.toml")),
				silent: true,
				..LoadOptions::default()
			},
			exposed: vec![project.clone(), refs.clone()],
			..ServerOptions::default()
		};
		let mut client = Client::connect(options).await;

		let (failed, text) = client.call("list_sources", json!({})).await;

		assert!(!failed, "{text}");
		assert_contains(
			&text,
			&[
				&format!(
					"the server's own workspace (used without `attached`): {}",
					root.join("own/Cargo.toml").display()
				),
				"no sources are attached",
				&format!("write  {}", project.resolved_pattern()),
				&format!("read   {}", refs.resolved_pattern()),
			],
		);

		// reading
		let log = root.join(native("refs/log/Cargo.toml"));
		let (failed, text) = client
			.call("attach_source", json!({ "manifest_path": log.to_str().unwrap(), "name": "log" }))
			.await;

		assert!(!failed, "{text}");
		assert_contains(
			&text,
			&[
				&format!("attached `log` (read-only): {}", log.display()),
				"packages loaded by default: log 0.1.0",
				"pass `\"attached\": \"log\"`",
			],
		);

		let (failed, text) = client.call("find_items", json!({ "pattern": "info", "attached": "log" })).await;

		assert!(!failed, "{text}");
		assert_contains(&text, &[&native("log::info  fn  src/lib.rs:1:1")]);

		let (_, text) = client.call("workspace_info", json!({ "attached": "log" })).await;

		assert!(text.starts_with(&format!("source `log` (read-only): {}\n", log.display())), "{text}");

		// the server's own workspace is still the default
		let (_, text) = client.call("find_items", json!({ "pattern": "*" })).await;

		assert_contains(&text, &["own::mine  fn"]);
		assert!(!text.contains("info"), "{text}");

		// read-only sources are only previewed
		let rename = json!({ "path": "crate::info", "new_name": "notice", "attached": "log" });
		let (failed, text) = client.call("rename_item", rename.clone()).await;

		assert!(failed);
		assert_contains(
			&text,
			&["source `log` is attached read-only", "nothing was written", "only exposed for reading"],
		);

		let mut preview = rename;

		preview["dry_run"] = json!(true);

		let (failed, text) = client.call("rename_item", preview).await;

		assert!(!failed, "{text}");
		assert_contains(&text, &["-pub fn info() {}", "+pub fn notice() {}"]);
		assert_eq!(fixture.read("refs/log/src/lib.rs"), "pub fn info() {}\n");

		// the tools taking item code (`source`) name attached sources like every other tool
		let own = fixture.read("own/src/lib.rs");
		let replace = json!({ "path": "crate::info", "source": "pub fn info() { todo!() }", "attached": "log" });
		let insert = json!({ "parent": "crate", "source": "pub fn warn() {}", "attached": "log" });
		let format = json!({ "attached": "log" });

		for (tool, arguments) in [("replace_item", &replace), ("insert_items", &insert), ("format_items", &format)] {
			let (failed, text) = client.call(tool, arguments.clone()).await;

			assert!(failed, "{tool}: {text}");
			assert_contains(&text, &["source `log` is attached read-only"]);
		}

		let (failed, text) = client.call("format_items", json!({ "attached": "log", "check": true })).await;

		assert!(!failed, "{text}");

		let mut preview = insert.clone();

		preview["dry_run"] = json!(true);

		let (failed, text) = client.call("insert_items", preview).await;

		assert!(!failed, "{text}");
		assert_contains(&text, &["+++ b/src/lib.rs", "+pub fn warn() {}"]);
		assert_eq!(fixture.read("refs/log/src/lib.rs"), "pub fn info() {}\n");
		assert_eq!(fixture.read("own/src/lib.rs"), own);

		// what may be attached, and how
		let attach =
			|path: &str, name: &str, write: bool| json!({ "manifest_path": root.join(path).to_str().unwrap(), "name": name, "write": write });
		let (failed, text) = client.call("attach_source", attach("refs/log", "log", true)).await;

		assert!(failed);
		assert_contains(&text, &["is only exposed for reading", &refs.to_string()]);

		let (failed, text) = client.call("attach_source", attach("secret", "secret", false)).await;

		assert!(failed);
		assert_contains(&text, &["is not in a directory exposed by this server", refs.resolved_pattern()]);

		let (failed, text) = client.call("attach_source", attach("project", "no good", true)).await;

		assert!(failed);
		assert_contains(&text, &["invalid name `no good`"]);

		// writing, by the directory of the manifest
		let (failed, text) = client.call("attach_source", attach("project", "engine", true)).await;

		assert!(!failed, "{text}");
		assert_contains(&text, &["attached `engine` (read and write)", "packages loaded by default: engine 0.1.0"]);

		let (failed, text) = client
			.call("rename_item", json!({ "path": "crate::run", "new_name": "start", "attached": "engine" }))
			.await;

		assert!(!failed, "{text}");
		assert_contains(&fixture.read("project/src/lib.rs"), &["pub fn start() {}", "\tstart();"]);

		let replace = json!({ "path": "crate::start", "source": "pub fn start() {\n\tgo();\n}", "attached": "engine" });
		let (failed, text) = client.call("replace_item", replace).await;

		assert!(!failed, "{text}");

		let insert = json!({ "parent": "crate", "source": "pub fn stop() {}", "attached": "engine" });
		let (failed, text) = client.call("insert_items", insert).await;

		assert!(!failed, "{text}");
		assert_contains(&fixture.read("project/src/lib.rs"), &["pub fn start() {\n\tgo();\n}", "pub fn stop() {}"]);
		assert_eq!(fixture.read("own/src/lib.rs"), own);

		// but only below the directories exposed for writing
		let before = fixture.read("project/src/lib.rs");
		let (failed, text) = client
			.call(
				"rename_item",
				json!({ "path": "crate::escape::escaped", "new_name": "caught", "attached": "engine" }),
			)
			.await;

		assert!(failed);
		assert_contains(&text, &["the edit of source `engine` would change", "escape.rs", "nothing was written"]);
		assert_eq!(fixture.read("outside/escape.rs"), "pub fn escaped() {}\n");
		assert_eq!(fixture.read("project/src/lib.rs"), before);

		// names
		let (_, text) = client.call("list_sources", json!({})).await;

		assert_contains(&text, &["engine  read and write", "log     read-only"]);

		let (failed, text) = client.call("find_items", json!({ "pattern": "*", "attached": "nope" })).await;

		assert!(failed);
		assert_contains(&text, &["no source is attached as `nope`", "the attached sources are `engine`, `log`"]);

		let (failed, text) = client.call("detach_source", json!({ "name": "log" })).await;

		assert!(!failed, "{text}");
		assert_contains(&text, &["detached `log`"]);

		let (failed, text) = client.call("find_items", json!({ "pattern": "*", "attached": "log" })).await;

		assert!(failed);
		assert_contains(&text, &["no source is attached as `log`"]);

		let (failed, _) = client.call("detach_source", json!({ "name": "log" })).await;

		assert!(failed);

		// a read-only attachment does not replace a writable one
		let (failed, text) = client.call("attach_source", attach("refs/log", "engine", false)).await;

		assert!(failed, "{text}");
		assert_contains(
			&text,
			&[
				"there is already a writable source attached as `engine` (read and write)",
				"nothing was attached",
			],
		);
		client.close().await.unwrap();
	}
}
