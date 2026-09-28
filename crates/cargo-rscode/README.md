# cargo-rscode

`cargo rscode`: view, search, format, and edit Rust source by item path, from the command line or through the Model
Context Protocol for AI agents. It is built on the [`rscode`](../rscode) library.

```sh
cargo +nightly install --path crates/cargo-rscode
```

Building the `cargo` library it depends on takes a few minutes, and needs a C compiler, `pkg-config`, and OpenSSL
headers.

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
`replace_item`, `insert_items`, and `format_items`. With `--read-only`, only the first three are offered. The workspace
is reloaded for every call, so changes made by other tools are always seen.

```sh
claude mcp add rscode -- cargo rscode mcp --manifest-path /abs/path/to/Cargo.toml
```

```json
{ "mcpServers": { "rscode": { "command": "cargo", "args": ["rscode", "mcp", "--manifest-path", "/abs/path/to/Cargo.toml"] } } }
```

The `mcp` feature (on by default) builds the server.

## Shell completion

Completion of commands, options, item paths, packages, and item kinds is dynamic:

```sh
source <(COMPLETE=bash cargo-rscode)     # ~/.bashrc
source <(COMPLETE=zsh cargo-rscode)      # ~/.zshrc, after compinit
COMPLETE=fish cargo-rscode | source      # ~/.config/fish/conf.d/cargo-rscode.fish
```

## License

MIT or Apache-2.0, at your option.
