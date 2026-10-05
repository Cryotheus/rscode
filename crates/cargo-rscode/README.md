# cargo-rscode

`cargo rscode`: view, search, format, and edit Rust source by item path, from the command line or through the Model
Context Protocol for AI agents. It is built on the [`rscode`](../rscode) library.

## Installing

Prebuilt binaries for Linux (x86_64, aarch64), macOS (x86_64, aarch64), and Windows (x86_64) are attached to the
`cargo-rscode-v*` GitHub releases, and [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall) knows how to
find them:

```sh
cargo binstall cargo-rscode                                          # the latest release
cargo binstall --git https://github.com/Cryotheus/rscode cargo-rscode  # from the repository
```

Or build from source with Rust 1.97 or newer:

```sh
cargo install cargo-rscode
```

Building the `cargo` library it depends on takes a few minutes, and needs a C compiler, `pkg-config`, and OpenSSL
headers (or the `vendored-openssl` feature, which builds OpenSSL from source and links it statically).

## Releasing

Push a tag `cargo-rscode-v<version>`, where `<version>` is the workspace `version` (`[workspace.package]` in the
repository's root `Cargo.toml`, which cargo-rscode inherits). The `release` workflow (`.github/workflows/release.yml`)
creates the GitHub release and attaches the archives `cargo binstall` expects, as described by
`[package.metadata.binstall]` in this crate's `Cargo.toml`. `cargo binstall` reads that metadata from the version
published on crates.io, so publish the crates for the same version.

## Commands

| command | does |
|---|---|
| `find <PATTERN>...` | find items by name or path pattern (`*Error`, `crate::m::*`, `**::Circle`) |
| `view <PATH>...` | print the source of items, or an outline of modules |
| `fmt [TARGET]...` | sort (Cryotheum ordering) and format items, modules, or whole crates |
| `sort [TARGET]...` | sort without formatting |
| `rename <PATH> <NEW_NAME>` | rename an item and update its references across the workspace |
| `remove <PATH>...` | remove items, the files of out-of-line modules, and (optionally) their imports |
| `replace <PATH> [SOURCE]` | replace the source of an item |
| `insert <PARENT> [SOURCE]` | insert items into a module, `impl` block, or trait |
| `mcp` | serve all of the above over the Model Context Protocol (stdio) |

```sh
cargo rscode find '*Error' -k enum
cargo rscode view crate::shapes::Circle --impls -n
cargo rscode rename crate::shapes::Circle Disk --dry-run
cargo rscode fmt crate::ffi --check
```

Every command takes cargo's selection flags (`-p`, `--workspace`, `--exclude`, `--features`, `--all-features`,
`--no-default-features`, `--manifest-path`, `--lib`, `--bin`, `--tests`, ..., `--target`), plus `--cfg` and
`--message-format human|json`. `cargo rscode <command> --help` documents everything. Edits are checked to still parse
before anything is written, and `--dry-run`/`--check` print a diff instead of writing.

## MCP server

`cargo rscode mcp` offers the tools `workspace_info`, `find_items`, `view_items`, `rename_item`, `remove_items`,
`replace_item`, `insert_items`, and `format_items`. With `--read-only`, the last five are not offered. The workspace
is reloaded for every call, so changes made by other tools are always seen.

```sh
claude mcp add rscode -- cargo rscode mcp --manifest-path /abs/path/to/Cargo.toml
```

```json
{ "mcpServers": { "rscode": { "command": "cargo", "args": ["rscode", "mcp", "--manifest-path", "/abs/path/to/Cargo.toml"] } } }
```

The `mcp` feature (on by default) builds the server.

### Several workspaces from one server

With `--expose ACCESS=DIRS`, clients can attach more cargo workspaces and packages while connected, by the path of
their `Cargo.toml` (or its directory) and a name of their choice, and then pass that name as `attached` to any tool:

```json
{
  "mcpServers": {
    "rscode": {
      "command": "cargo",
      "args": [
        "rscode", "mcp",
        "--expose", "write=/abs/path/engine",
        "--expose", "read=/abs/path/references/*",
        "--expose", "read=/abs/path/references/misc/**"
      ]
    }
  }
}
```

- A `Cargo.toml` can be attached when its directory matches a pattern. `*` matches within one path component and
  `**` any number of them: `references/*` matches `references/log` but not `references/misc/log`, and
  `references/misc/**` matches `references/misc` and everything below it. Wildcards skip hidden directories, and
  symbolic links are resolved before matching.
- Clients ask for write access when attaching. Only directories matching a `write` pattern grant it, and `write`
  patterns take precedence over `read` ones. Edits of attached sources are only ever written below directories
  matching a `write` pattern, even when they reach elsewhere (through `#[path]` attributes or other workspace
  members). Sources attached read-only can still be previewed with `dry_run`. Reading is not confined the same way:
  a source is loaded like cargo loads it, including its other workspace members and `#[path]` files.
- Names are scoped to the client that attached them. Over stdio each client has its own server process, so clients
  never see or break each other's names. Attaching checks that cargo can plan loading the source (so it fails for
  manifests cargo rejects), and then only records the name: every tool call loads its source from disk anyway.
  A name keeps its source until `detach_source` forgets it: attaching under a taken name changes nothing and answers
  with what the name has (a read-only request never replaces a writable attachment, and says that a writable one is
  active), except that attaching the same `Cargo.toml` for writing makes a read-only attachment writable.
  `list_sources` shows the attached sources and the exposed directories.
- The server's own workspace (`--manifest-path`, or the one containing the working directory) remains the default
  when a tool call names no `attached` source, and `--expose` does not restrict its edits, like on the command line.

## Shell completion

Completion of commands, options, item paths, packages, and item kinds is dynamic:

```sh
source <(COMPLETE=bash cargo-rscode)     # ~/.bashrc
source <(COMPLETE=zsh cargo-rscode)      # ~/.zshrc, after compinit
COMPLETE=fish cargo-rscode | source      # ~/.config/fish/conf.d/cargo-rscode.fish
```

## License

MIT or Apache-2.0, at your option.
