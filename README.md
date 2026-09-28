# .rs Code

_Rust source file summaries and refactoring_

rscode views, searches, and edits Rust source files by **item path** (`crate::shapes::Circle::area`) instead of by
line number. It parses a cargo workspace with `syn` (nothing is compiled or macro-expanded), follows `mod`
declarations exactly like rustc, and resolves imports, re-exports, `impl` targets, and visibility. Every edit is text
based, so comments and formatting outside of the edited items are preserved, and every edit is checked to still parse
before anything is written.

It comes as:

| crate | what |
|---|---|
| [`cargo-rscode`](crates/cargo-rscode) | the `cargo rscode` command, which also serves the Model Context Protocol (`cargo rscode mcp`) for AI agents |
| [`rscode`](crates/rscode) | the library: loading, name resolution, find, view, rename, remove, replace, insert, format |
| [`rscode_fmt`](crates/rscode_fmt) | formatting of whole files or single items with rustfmt or prettyplease |
| [`rscode_sort`](crates/rscode_sort) | deterministic sorting of items (the "Cryotheum" ordering), usable on its own, e.g. for generated `bindgen` output |

## Installing

Prebuilt binaries are attached to the `cargo-rscode-v*` GitHub releases, for
[`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall):

```sh
cargo binstall --git https://github.com/Cryotheus/rscode cargo-rscode
```

Building from source requires a nightly toolchain (the workspace is developed on `1.101.0-nightly`):

```sh
cargo +nightly install --path crates/cargo-rscode
```

Building the `cargo` library takes a few minutes and needs a C compiler, `pkg-config`, and OpenSSL headers.

## Usage

```sh
cargo rscode find '*Error'                    # items whose name ends with `Error`, anywhere
cargo rscode find 'crate::shapes::*' -k fn    # functions directly in `crate::shapes`
cargo rscode find Circle --show all --from ::  # with cfg, visibility, and the path other crates can use
cargo rscode view crate::shapes               # an outline of a module (bodies elided)
cargo rscode view crate::shapes::Circle --impls -n
cargo rscode rename crate::shapes::Circle Disk --dry-run
cargo rscode remove crate::util::old --prune-imports
cargo rscode replace crate::util::area new_area.rs
cargo rscode insert '<crate::shapes::Circle>' method.rs --position after --anchor crate::shapes::Circle::new
cargo rscode fmt crate::ffi --check           # sort and rustfmt one module, show the diff
cargo rscode sort                             # only sort; formatting is left as it is
```

Every subcommand accepts cargo's selection flags (`-p`, `--workspace`, `--exclude`, `--features`,
`--all-features`, `--no-default-features`, `--manifest-path`, `--lib`, `--bin`, `--tests`, ..., `--target`) plus
`--cfg` and `--message-format human|json`. `cargo rscode <subcommand> --help` documents everything.

### Item paths

| path | meaning |
|---|---|
| `crate::m::Item` | an item of the selected crates |
| `::krate::Item` | an item of the crate `krate` |
| `m::Item` | presumed absolute: `crate::m::Item`, or `::m::Item` |
| `Type::name` | an associated item, trait item, or enum variant |
| `<Type as Trait>::name` | an item of a trait `impl` (`<Type>::name`: of an inherent one) |
| `impl Trait for Type` | an `impl` block (also `<Type as Trait>`, `<Type>`) |

Paths go through re-exports. Items behind `cfg`s are loaded regardless of the enabled features and target; one path
names every `cfg` variant (renaming renames all of them), and disabled variants are marked inactive. Generic arguments
of the type and trait pick `impl` blocks by their headers as written (`impl From<u8> for Wrapper`,
`<Wrapper<u16>>::get`): removing or replacing the items of several `impl` blocks whose headers differ fails as
ambiguous, even with `--all-variants`.

### Patterns (`find`, `fmt`, `sort`)

No regex. `*` matches within an identifier, `**` matches whole path segments:

| pattern | matches |
|---|---|
| `foo` / `foo*` / `*foo` / `*foo*` | exactly / starts with / ends with / contains `foo` |
| `foo*bar`, `*foo*bar*` | several parts, in order |
| `foo::*` | items directly in `foo` |
| `foo::**` | everything below `foo` |
| `**::Blam`, `Blam` | `Blam` anywhere (patterns without `crate::`/`::` are unanchored) |

`-i` ignores case; `--contains`, `--starts-with`, and `--ends-with` are spelled-out alternatives.

### Sorting

`fmt` and `sort` order items with the Cryotheum schema: module declarations, imports, re-exports, type aliases,
constants, statics, data types each followed by their `impl` blocks (inherent first, then trait impls), loose `impl`
blocks, `extern` blocks (merged when their ABI and attributes match), functions, inline modules; `impl` and `trait`
items are ordered `type`, `const`, `new`, other functions, then methods. Imports are ordered like rustfmt orders them
in the style edition it uses for the crate (its edition, unless `rustfmt.toml` sets `style_edition`), so `sort`,
`fmt`, and `cargo fmt` agree. `macro_rules!` definitions and item-position
macro invocations are never moved, and nothing is moved across them, because macros are scoped textually. Function
bodies, expressions, fields, and variants are never reordered. See the [`rscode_sort`](crates/rscode_sort) docs.

## MCP server (for AI agents)

`cargo rscode mcp` serves the same operations as tools over stdio: `workspace_info`, `find_items`, `view_items`,
`rename_item`, `remove_items`, `replace_item`, `insert_items`, and `format_items` (`--read-only` offers only the first
three). The workspace is reloaded for every call, so edits made by other tools are always seen, and modifying tools
support `dry_run` to return a diff instead of writing.

Claude Code:

```sh
claude mcp add rscode -- cargo rscode mcp --manifest-path /abs/path/to/Cargo.toml
```

Clients configured with JSON (`.mcp.json`, Claude Desktop, Cursor):

```json
{ "mcpServers": { "rscode": { "command": "cargo", "args": ["rscode", "mcp", "--manifest-path", "/abs/path/to/Cargo.toml"] } } }
```

VS Code uses `.vscode/mcp.json` with a top-level `"servers"` key instead of `"mcpServers"`.

## Shell completion

Completion is dynamic (item paths, packages, kinds, options):

```sh
source <(COMPLETE=bash cargo-rscode)     # ~/.bashrc
source <(COMPLETE=zsh cargo-rscode)      # ~/.zshrc, after compinit
COMPLETE=fish cargo-rscode | source      # ~/.config/fish/conf.d/cargo-rscode.fish
```

## Limitations

- Items generated by macros (including `macro_rules!` expansions and proc macros) are invisible.
- Method calls (`x.name()`) cannot be resolved without type inference; `rename` reports them as uncertain and only
  changes them with `--method-calls`, like `T::name` paths through generic parameters that the parameter's bounds do
  not resolve (such as through a supertrait). The same goes for identifiers inside macro invocations that don't parse as
  expressions and inside `macro_rules!` transcribers (`--macro-tokens`), except for paths there (`module::name`,
  `name!`, `$crate::name`), which are resolved where the macro is.
- Features are resolved over the workspace's own packages by default; `--exact-features` runs cargo's resolver.

# License

This project is licensed under either of

* Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or
  https://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in `rscode` by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
