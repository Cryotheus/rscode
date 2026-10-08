//! `cargo rscode`: view, search, format, and edit Rust source by item path.
//!
//! The crates of a cargo workspace are loaded (selected like `cargo check` does: `-p`, `--workspace`, `--lib`,
//! `--bins`, ...; `--features` and `--target` decide which `cfg`s are active) and parsed, without compiling them or
//! expanding macros. Then:
//!
//! - `cargo rscode find <PATTERN>...` finds items by glob-like path patterns (`Foo`, `*Error`, `crate::ast::**`,
//!   `<Foo as Display>::fmt`) and prints their paths and locations (`--show kind,span,cfg,...`), optionally with
//!   the paths usable from a module (`--from crate|::|MODULE`). `--message-format json` is for programs, and
//!   `--message-format file-lines` prints rustfmt's `--file-lines` JSON.
//! - `cargo rscode view <PATH>...` prints the source of items; modules are shown as outlines.
//! - `cargo rscode refs <PATH>...` prints the references to items by file, with the item and line of code of each.
//! - `cargo rscode fmt [TARGET]...` sorts items (with the Cryotheum ordering schema) and formats them (with rustfmt
//!   or prettyplease), with rustfmt's `--check`, `--emit`, `--skip-children`, and `--config max_width=80,...`
//!   (dotted keys and files are cargo's); `cargo rscode sort` only sorts.
//! - `cargo rscode rename <PATH> <NEW_NAME>` renames an item and updates its references across the workspace.
//! - `cargo rscode remove <PATH>...` removes items, and the files of out-of-line modules.
//! - `cargo rscode replace <PATH> [SOURCE]` replaces the source of an item, and
//!   `cargo rscode insert <PARENT> [SOURCE]` adds items to a module, `impl` block, or trait.
//! - `cargo rscode create-module <PARENT> <NAME> [SOURCE]` creates a module's file and declares it in its parent,
//!   and `cargo rscode import <MODULE> <PATH>...` adds imports to a module, where sorting puts them.
//! - `cargo rscode mcp` serves all of this to AI assistants over the Model Context Protocol.
//!
//! Edits are validated to still parse before anything is written (all or nothing), and `--dry-run` prints them as
//! a diff instead.
//!
//! # Item paths
//!
//! Item paths are written like Rust paths: `crate::module::Item` (in the selected crates), `::crate_name::Item`,
//! `module::Item` (presumed absolute), `Type::method`, `<Type as Trait>::method`, `impl Trait for Type` for `impl`
//! blocks, and `'use module::Item'` for imports (other paths go through them). Every `cfg` variant of an item is
//! addressed by its path. File paths are printed relative to the
//! current directory when below it (`--absolute-paths` prints them absolute).
//!
//! # Invocation
//!
//! `cargo rscode <ARGS>` runs `cargo-rscode rscode <ARGS>`; running `cargo-rscode <ARGS>` directly works as well.
//! Cargo does not forward its own options given before `rscode` (`cargo -v rscode ...`), so every subcommand takes
//! `-v`, `-q`, `--color`, `--offline`, `--locked`, `--frozen`, and `--config` itself. Errors exit with 1, usage
//! errors with 2, and `fmt --check` exits with 1 when files would change (also when its output is cut short, as by
//! `| head`).
//!
//! # Shell completion
//!
//! Completion is dynamic: the shell asks `cargo-rscode` for candidates on every TAB, so item paths, package names,
//! and item kinds are completed too. Register it with
//!
//! - bash: `source <(COMPLETE=bash cargo-rscode)` in `~/.bashrc` (or, to have bash-completion load it lazily, that
//!   line in `~/.local/share/bash-completion/completions/cargo-rscode`);
//! - zsh: `source <(COMPLETE=zsh cargo-rscode)` in `~/.zshrc`, after `compinit`;
//! - fish: `COMPLETE=fish cargo-rscode | source` in `~/.config/fish/conf.d/cargo-rscode.fish`;
//! - elvish: `eval (E:COMPLETE=elvish cargo-rscode | slurp)`;
//! - PowerShell: `$env:COMPLETE = "powershell"; cargo-rscode | Out-String | Invoke-Expression; Remove-Item Env:\COMPLETE`.
//!
//! Both `cargo-rscode <TAB>` and `cargo rscode <TAB>` are completed (in bash, the latter needs rustup's cargo
//! completion and bash-completion 2.12 or newer). The scripts call back into the binary with an unstable protocol,
//! so generate them on shell startup rather than saving them.
//!
//! # MCP clients
//!
//! `cargo rscode mcp` speaks MCP on stdin and stdout. For Claude Code:
//! `claude mcp add rscode -- cargo rscode mcp --manifest-path /abs/path/to/Cargo.toml`; for `.mcp.json`, Claude
//! Desktop, or Cursor:
//!
//! ```json
//! { "mcpServers": { "rscode": { "command": "cargo", "args": ["rscode", "mcp", "--manifest-path", "/abs/path/to/Cargo.toml"] } } }
//! ```
//!
//! See `cargo rscode mcp --help`.

mod args;
mod cli;
mod commands;
mod complete;
mod render;
mod report;
mod shells;
mod ui;

use clap_complete::CompleteEnv;
use std::process::ExitCode;
use ui::Ui;

fn main() -> ExitCode {
    if complete::is_requested() {
        // completers run on every TAB and stdout is their reply: a panic message would garble the prompt
        std::panic::set_hook(Box::new(|_| {}));
    }

    // first: it edits the environment, which is only sound while the process has a single thread
    CompleteEnv::with_factory(cli::cli)
        .var(complete::COMPLETE_VAR)
        .shells(shells::SHELLS)
        .complete();

    let (args, via_cargo) = cli::normalize_args(std::env::args_os().collect());

    // only the usage, help, and error texts change (clap would take the name the binary was started with, like
    // `cargo-rscode.exe`)
    let bin_name = match via_cargo {
        true => format!("cargo {}", cli::CARGO_SUBCOMMAND),
        false => cli::BIN_NAME.to_owned(),
    };
    let command = cli::cli()
        .color(cli::color_choice(&args))
        .bin_name(bin_name);
    let matches = command
        .try_get_matches_from(args)
        .unwrap_or_else(|error| error.exit());

    // `subcommand_required` has clap refuse a missing subcommand
    let Some((name, matches)) = matches.subcommand() else {
        return ExitCode::from(2);
    };

    let ui = Ui::new(matches);

    // parsing and formatting recurse as deep as the code is nested, which can exhaust the main thread's stack
    let result = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("rscode".to_owned())
            .stack_size(rscode::rscode_fmt::RECOMMENDED_STACK_SIZE)
            .spawn_scoped(scope, || commands::run(name, matches, &ui))
            .map(|thread| thread.join())
    });

    // a closed stdout is no error (see `ui::print`), so the exit code is always the command's own
    match result {
        Ok(Ok(Ok(code))) => code,

        Ok(Ok(Err(error))) => {
            ui.error(format_args!("{error:#}"));
            ExitCode::FAILURE
        }

        // the panic message was already printed by the panic hook
        Ok(Err(_)) => ExitCode::FAILURE,

        Err(error) => {
            ui.error(format_args!("failed to start a thread: {error}"));
            ExitCode::FAILURE
        }
    }
}
