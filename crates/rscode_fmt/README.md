# .rs Code: Fmt

Formatting of Rust source files, or of only some items within them, with rustfmt or prettyplease. Items can be sorted
with [`rscode_sort`](../rscode_sort) first.

- `Formatter::format_str` formats a whole file.
- `Formatter::format_items` formats only the selected items of a file. Every byte outside of them is left untouched.
- `Formatter::format_tokens` formats a `proc_macro2::TokenStream`, such as generated `bindgen` output.
- `emit` renders the difference between the original and formatted source as a unified diff, or as JSON or
  checkstyle XML compatible with `rustfmt --emit`.

```rust
use rscode_fmt::FormatOptions;
use rscode_fmt::FormatTarget;
use rscode_fmt::Formatter;
use rscode_fmt::RsFormatter;

let formatter = Formatter::new(FormatOptions::new().formatter(RsFormatter::PrettyPlease));
let source = "fn  a( ) {}\nfn  b( ) {}\n";

// only `b` is formatted
let b = source.find("fn  b").unwrap();

assert_eq!(formatter.format_items(source, &[FormatTarget::Item(b)])?, "fn  a( ) {}\nfn b() {}\n");
```

## Formatters

- **rustfmt** (the default) runs as a subprocess, with the source passed over stdin. It keeps comments and follows
  `rustfmt.toml`: set `RustFmtOptions::config_path` to the file's directory, because rustfmt cannot find the
  configuration on its own for stdin input.
- **prettyplease** needs no external tool, but it discards comments, so it refuses sources with comments unless
  `allow_comment_loss` is set. Its output is checked to have the same tokens as the input, so a prettyplease bug
  cannot silently change the meaning of code.

## Threads

Parsing recurses once per level of nesting, and running out of stack aborts the process. Format on a thread with a
stack of `RECOMMENDED_STACK_SIZE`, and in long-running processes on short-lived threads, because `proc_macro2` keeps
a thread-local copy of every parsed text.

The `clap` feature implements `clap::ValueEnum` for the option enums.

## License

MIT or Apache-2.0, at your option.
