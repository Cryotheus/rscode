//! [`CompleteEnv`](clap_complete::CompleteEnv) shell adapters for a cargo subcommand whose values are Rust paths.
//!
//! - [`RustPathBash`]: the stock bash script passes `COMP_WORDS` on, but bash splits words at `COMP_WORDBREAKS`
//!   (which contains `:`, `=`, and `@`), so `crate::a::B` would reach clap as `crate` `::` `a` `::` `B`. This script
//!   joins the pieces again and trims the candidates to the part bash actually replaces.
//! - [`CargoZsh`] and [`CargoFish`]: the stock scripts plus a hook so that `cargo rscode <TAB>` works through the
//!   shell's own `cargo` completion (rustup's `_cargo` calls `_cargo-<cmd>`; fish's `cargo.fish` never delegates).
//!
//! bash needs no hook for `cargo rscode <TAB>`: rustup's cargo completion forwards to the `cargo-rscode` completion
//! when bash-completion 2.12 or newer is installed.

use crate::cli::CARGO_SUBCOMMAND;
use clap_complete::env::Elvish;
use clap_complete::env::EnvCompleter;
use clap_complete::env::Fish;
use clap_complete::env::Powershell;
use clap_complete::env::Shells;
use clap_complete::env::Zsh;
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;

/// The supported shells.
pub(crate) const SHELLS: Shells<'static> =
    Shells(&[&RustPathBash, &CargoZsh, &CargoFish, &Elvish, &Powershell]);

/// fish, also completing `cargo rscode`.
pub(crate) struct CargoFish;

impl EnvCompleter for CargoFish {
    fn is(&self, name: &str) -> bool {
        Fish.is(name)
    }

    fn name(&self) -> &'static str {
        Fish.name()
    }

    fn write_complete(
        &self,
        command: &mut clap::Command,
        args: Vec<OsString>,
        current_dir: Option<&Path>,
        buf: &mut dyn Write,
    ) -> Result<(), std::io::Error> {
        Fish.write_complete(command, args, current_dir, buf)
    }

    fn write_registration(
        &self,
        var: &str,
        name: &str,
        bin: &str,
        completer: &str,
        buf: &mut dyn Write,
    ) -> Result<(), std::io::Error> {
        let mut stock = Vec::new();

        Fish.write_registration(var, name, bin, completer, &mut stock)?;
        buf.write_all(&stock)?;

        // The same line again (reusing clap's quoting of `completer`), bound to `cargo` when its subcommand is
        // `rscode`. The words then arrive as `cargo rscode <args>`: the engine skips `cargo` as the binary name and
        // `rscode` as a stray positional (the top-level command has no positionals).
        // `__fish_seen_subcommand_from rscode` would also match `cargo test -p rscode`, hence the anchored regex.
        let condition = format!("__cargo_{}_args", CARGO_SUBCOMMAND.replace('-', "_"));
        let stock = String::from_utf8_lossy(&stock);
        let for_cargo = stock.replacen(
            &format!("--command {bin} "),
            &format!("--command cargo --condition {condition} "),
            1,
        );

        if for_cargo != stock {
            writeln!(
                buf,
                r"function {condition}
    string match -qr -- '^\s*\S*cargo(\s+\+\S+)?\s+{CARGO_SUBCOMMAND}\s' (commandline --current-process --cut-at-cursor)
end"
            )?;
            buf.write_all(for_cargo.as_bytes())?;
        }

        Ok(())
    }
}

/// zsh, also completing `cargo rscode` through rustup's `_cargo`.
pub(crate) struct CargoZsh;

impl EnvCompleter for CargoZsh {
    fn is(&self, name: &str) -> bool {
        Zsh.is(name)
    }

    fn name(&self) -> &'static str {
        Zsh.name()
    }

    fn write_complete(
        &self,
        command: &mut clap::Command,
        args: Vec<OsString>,
        current_dir: Option<&Path>,
        buf: &mut dyn Write,
    ) -> Result<(), std::io::Error> {
        Zsh.write_complete(command, args, current_dir, buf)
    }

    fn write_registration(
        &self,
        var: &str,
        name: &str,
        bin: &str,
        completer: &str,
        buf: &mut dyn Write,
    ) -> Result<(), std::io::Error> {
        Zsh.write_registration(var, name, bin, completer, buf)?;

        // rustup's `_cargo` runs `_cargo-<cmd>` for unknown subcommands with `words=(rscode <args>...)`, and the
        // engine skips `words[1]` like a binary name. `_clap_dynamic_completer_<name>` is the stock script's function.
        let function = format!("_clap_dynamic_completer_{}", name.replace('-', "_"));

        writeln!(buf, "\n_cargo-{CARGO_SUBCOMMAND}() {{ {function} \"$@\" }}")
    }
}

/// bash, completing Rust paths as whole words.
pub(crate) struct RustPathBash;

impl EnvCompleter for RustPathBash {
    fn is(&self, name: &str) -> bool {
        name == "bash"
    }

    fn name(&self) -> &'static str {
        "bash"
    }

    fn write_complete(
        &self,
        command: &mut clap::Command,
        args: Vec<OsString>,
        current_dir: Option<&Path>,
        buf: &mut dyn Write,
    ) -> Result<(), std::io::Error> {
        // the same protocol (`_CLAP_COMPLETE_INDEX`, `_CLAP_IFS`) as the stock adapter
        clap_complete::env::Bash.write_complete(command, args, current_dir, buf)
    }

    fn write_registration(
        &self,
        var: &str,
        name: &str,
        bin: &str,
        completer: &str,
        buf: &mut dyn Write,
    ) -> Result<(), std::io::Error> {
        let script = r##"
_clap_complete_@NAME@() {
    local IFS=$'\013'
    local line=${COMP_LINE:0:COMP_POINT} wordbreaks=${COMP_WORDBREAKS-}$' \t\n'
    local rest=$line gap word i last
    local -a words=("${COMP_WORDS[0]}")
    COMPREPLY=()

    # Rebuild the shell words before the cursor: bash splits COMP_WORDS at COMP_WORDBREAKS
    # (`:`, `=`, `@`, ...), so glue back pieces not separated by whitespace in COMP_LINE
    # (`crate` `::` `a` -> `crate::a`). The command word is skipped by text: when forwarded
    # from cargo's completion, COMP_WORDS[0] is `cargo-rscode` but COMP_LINE says `rscode`.
    rest=${rest#"${rest%%[![:space:]]*}"}
    rest=${rest#"${rest%%[[:space:]]*}"}
    for (( i = 1; i <= COMP_CWORD; i++ )); do
        gap=${rest%%[![:space:]]*}
        rest=${rest#"$gap"}
        if (( i < COMP_CWORD )); then
            word=${COMP_WORDS[i]}
        else
            word=$rest
        fi
        last=$(( ${#words[@]} - 1 ))
        if (( last > 0 )) && [[ -z $gap ]]; then
            words[last]+=$word
        else
            words+=("$word")
        fi
        rest=${rest#"$word"}
    done
    last=$(( ${#words[@]} - 1 ))

    # readline replaces only the text after the last word break character, so candidates
    # must be trimmed to that (`crate::cli` -> `cli` when completing `crate::c`).
    local cur=${line##*["$wordbreaks"]} prefix=
    if [[ ${words[last]} == *"$cur" ]]; then
        prefix=${words[last]%"$cur"}
    fi

    local -a candidates
    candidates=( $( \
        _CLAP_IFS="$IFS" \
        _CLAP_COMPLETE_INDEX="$last" \
        @VAR@="bash" \
        @COMPLETER@ -- "${words[@]}" 2> /dev/null \
    ) ) || return 0

    local candidate
    for candidate in "${candidates[@]}"; do
        [[ $candidate == "$prefix"* ]] && COMPREPLY+=("${candidate#"$prefix"}")
    done
    if (( ${#COMPREPLY[@]} == 1 )) && [[ ${COMPREPLY[0]} == *[:/=] ]]; then
        compopt -o nospace 2> /dev/null
    fi
    return 0
}
complete -o bashdefault -o nosort -F _clap_complete_@NAME@ @BIN@ 2> /dev/null ||
    complete -o bashdefault -F _clap_complete_@NAME@ @BIN@
"##
        .replace("@NAME@", &name.replace('-', "_"))
        .replace("@BIN@", &sh_quote(bin))
        .replace("@COMPLETER@", &sh_quote(completer))
        .replace("@VAR@", var);

        writeln!(buf, "{script}")
    }
}

/// POSIX-shell single-quotes `text` unless it only has safe characters.
fn sh_quote(text: &str) -> String {
    let safe = |byte: u8| byte.is_ascii_alphanumeric() || b"/_-.+,:@%=".contains(&byte);

    if !text.is_empty() && text.bytes().all(safe) {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_shells_by_name() {
        let names: Vec<&str> = SHELLS.names().collect();

        assert_eq!(names, ["bash", "zsh", "fish", "elvish", "powershell"]);
        assert!(SHELLS.completer("bash").is_some());
        assert!(SHELLS.completer("tcsh").is_none());
    }

    #[test]
    fn quotes_for_sh() {
        assert_eq!(sh_quote("cargo-rscode"), "cargo-rscode");
        assert_eq!(sh_quote("/a/b_c.d+e,f:g@h%i=j"), "/a/b_c.d+e,f:g@h%i=j");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn registers_for_bash() {
        let script = registration(&RustPathBash, "/home/me/.cargo/bin/cargo-rscode");

        assert!(
            script.contains("_clap_complete_cargo_rscode() {"),
            "{script}"
        );
        assert!(
            script.contains(
                "complete -o bashdefault -o nosort -F _clap_complete_cargo_rscode cargo-rscode"
            ),
            "{script}"
        );
        assert!(script.contains("COMPLETE=\"bash\""), "{script}");
        assert!(
            script.contains("/home/me/.cargo/bin/cargo-rscode -- \"${words[@]}\""),
            "{script}"
        );

        for placeholder in ["@NAME@", "@BIN@", "@COMPLETER@", "@VAR@"] {
            assert!(!script.contains(placeholder), "{script}");
        }

        let quoted = registration(&RustPathBash, "/opt/my tools/cargo-rscode");

        assert!(
            quoted.contains("'/opt/my tools/cargo-rscode' --"),
            "{quoted}"
        );
    }

    #[test]
    fn registers_for_fish_and_cargo() {
        let script = registration(&CargoFish, "cargo-rscode");

        assert!(
            script.contains("complete --keep-order --exclusive --command cargo-rscode "),
            "{script}"
        );
        assert!(script.contains("function __cargo_rscode_args"), "{script}");
        assert!(
            script.contains("--command cargo --condition __cargo_rscode_args "),
            "{script}"
        );
        assert!(
            script.contains(r"'^\s*\S*cargo(\s+\+\S+)?\s+rscode\s'"),
            "{script}"
        );
    }

    #[test]
    fn registers_for_zsh_and_cargo() {
        let script = registration(&CargoZsh, "cargo-rscode");

        assert!(
            script.contains("compdef _clap_dynamic_completer_cargo_rscode cargo-rscode"),
            "{script}"
        );
        assert!(
            script.contains("_cargo-rscode() { _clap_dynamic_completer_cargo_rscode \"$@\" }"),
            "{script}"
        );
    }

    fn registration(shell: &dyn EnvCompleter, completer: &str) -> String {
        let mut buf = Vec::new();

        shell
            .write_registration(
                "COMPLETE",
                "cargo-rscode",
                "cargo-rscode",
                completer,
                &mut buf,
            )
            .unwrap();
        String::from_utf8(buf).unwrap()
    }
}
