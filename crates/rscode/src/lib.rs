// rough project outline
// 
//  providers builders and data types for working with Rust source files
//  needs to offer configurable methods of the following:
//      - resolve item paths and their file path mappings
//          - item paths
//              - absolute path, all the way down to `::` prefixed paths or `crate::` prefixed paths
//              - relative path resolution
//                  - 
//          - file paths
//              - line-column location for start and end
//      - finding item paths by a part of the path, or just identifiers
//          - find by ident:
//              - full exact ident of item, probably treat raw identifiers as equivalent to non-raw identifiers?
//              - no regex. I'm a hater.
//                  - maybe allow glob-pattern-like asterisk-syntax
//              - *containing* a sub-string
//                  - `contains foo`
//                  - asterisk equivalent: `*foo*`
//              - *starting* with a sub-string
//                  - `starts-with foo`
//                  - asterisk equivalent: `foo*`
//              - *ending* with a sub-string
//                  - `end-with foo`
//                  - asterisk equivalent: `*foo`
//              - combination asterisk-syntax?
//                  - `foo*bar` starts with `foo`, ends with `bar`
//                  - `foo*bar*` starts with `foo`, contains `bar`
//                  - `*foo*bar` contains `foo`, ends with bar `bar`
//                  - `*foo*bar*` contains `foo`, and contains `bar` after `foo`
//                  - patterns with more than two string segments
//              - case-sensitivity option
//          - absolute & relative item paths and file paths
//              - search by asterisk glob patterns
//                  - `foo::*` / `foo*` anything immediately in the `foo` path, such as `foo::Bar` but not `foo::biz::Baz`
//                  - `foo::**` / `foo**` anything starting with the `foo` path, such as `foo::Bar` and `foo::biz::Baz`
//                  - opposite direction `*::Blam` / `*Blam` and `**::Blam` / `**Blam` works the same
//          - support location info such as line-column start and end data
//
//  offer `Builder`/`Options` types for confuring and performing these operations
//
//  offer unified types for the following:
//      fully (or partially/lazily) resolved workspace tree (of packages?)
//      fully (or partially/lazily) resolved package module trees
//      fully (or partially/lazily) resolved module items (gloss over `impl Type {}`, `extern "C" {}`, `mod foo {}`
//
//  this *must* support input and output using `proc-macro2`
//  do not directly transfer `syn` types across crate boundaries, however
//  they should be wrapped in another type if not already accompanied by other data
//
//  convenience functions for using strings should be offered so users of the crate do not need to add `proc-macro2` if they are only working with strings



