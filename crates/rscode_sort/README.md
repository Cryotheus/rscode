# rscode_sort

Deterministic sorting of the items in Rust source files.

Sorting the same items gives the same output whatever order they started in. Two runs of a code generator such as
`bindgen` that emit the same items in a different order therefore produce byte-identical files, and nothing churns in
version control. Handwritten code gets a consistent layout.

Two engines share one ordering:

- **Text** (`Sorter::sort_str`, `Sorter::sort_str_within`): whole items move as chunks of the original text, together
  with the comments attached to them, so comments and formatting are preserved.
- **Tokens** (`Sorter::sort_tokens`): a `proc_macro2::TokenStream` is sorted structurally, for generated code that has
  no comments to keep.

```rust
let sorted = rscode_sort::sort_str("fn b() {}\nfn a() {}\n")?;

assert_eq!(sorted, "fn a() {}\n\nfn b() {}\n");
```

Only some containers can be sorted, and only on request, with `SortOptions`:

```rust
use rscode_sort::SortOptions;
use rscode_sort::SortTarget;
use rscode_sort::Sorter;

// only the file's own items; inline modules, `impl` blocks, and the like keep their order
let sorter = Sorter::new(SortOptions::new().recursive(false));
let sorted = sorter.sort_str_within(source, &[SortTarget::File])?;
```

## The Cryotheum ordering

The items of a module are grouped in this order:

1. `extern crate` items
2. `mod foo;` declarations (without `cfg`, with a `cfg`, then `#[cfg(test)]`)
3. `use` items, ordered exactly as rustfmt orders them
4. re-exports (`pub use`, `pub(crate) use`, ...)
5. type aliases, then constants, statics, and mutable statics
6. data types (structs, enums, unions, traits), each followed by its `impl` blocks, inherent ones first
7. `impl` blocks of types defined elsewhere
8. `extern` blocks, merged when their safety, ABI, and attributes match
9. functions
10. inline modules

The items of `impl` blocks and traits are ordered as associated types, constants, `fn new`, other functions without
a receiver, then methods. Within a group, items are ordered by name using the Rust style guide's version sorting, with
`_name` directly after `name`.

Expressions, function bodies, fields, and enum variants are never reordered. `macro_rules!` definitions, item-position
macro invocations, and `#[macro_use]` items are barriers: they never move, and nothing moves across them, because
macros are scoped textually.

The `clap` feature implements `clap::ValueEnum` for the option enums.

## License

MIT or Apache-2.0, at your option.
