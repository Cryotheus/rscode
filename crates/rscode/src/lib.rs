//! Viewing, searching, and editing Rust source files by item path.
//!
//! rscode loads crates into a [`Workspace`]: a tree of modules and items per crate, with source locations, `cfg`
//! predicates, and visibility. Nothing is compiled or expanded; source files are parsed with `syn` and module
//! files are found by following `mod` declarations the same way rustc does. Items produced by macros are
//! therefore invisible, except for the statics declared by `thread_local!`, which are loaded as `static` items of the
//! module the invocation is in. Item-position macro invocations are items themselves, named `module::name!`, and so
//! are the `static NAME = value;` entries of an invocation whose body is only such entries (`module::NAME`).
//!
//! - Load: [`load_workspace`] (a cargo workspace, feature `cargo`), or [`Workspace::load_crate`] with a
//!   [`CrateSpec`] for a standalone crate root.
//! - Resolve: [`Resolver`] resolves `use` imports, paths, `impl` targets, visibility, and usable paths.
//! - Search: [`Find`] with glob-like [`pattern`]s (no regex).
//! - View: [`View`] shows full source or outlines (bodies elided).
//! - Edit: [`edit::remove`], [`edit::rename`], [`edit::replace`], [`edit::insert`], and [`edit::format`] plan
//!   changes as an [`EditSet`], which is previewed or applied atomically. Comments and formatting outside of the
//!   edited ranges are always preserved.
//! - Serve: the `mcp` feature exposes all of this as a Model Context Protocol server ([`mcp`]).
//!
//! Item paths are written like Rust paths: `crate::module::Item`, `::other_crate::Item`, `Type::method`,
//! `<Type as Trait>::method`, `impl Type[method]` for one of several `impl` blocks with the same header, `Type.field`
//! for fields, and `use module::Item` for the imports themselves (other paths go through imports). See [`ItemPath`]
//! and [`pattern`] for the exact syntax.
//!
//! Formatting and sorting are provided by the [`rscode_fmt`] and [`rscode_sort`] crates, re-exported here.
//!
//! # Threads
//!
//! `proc_macro2`, which `syn` parses with, keeps the text of everything parsed on a thread in a thread-local source
//! map that only grows until the thread exits, and parsing recurses as deeply as the code is nested. So rscode parses
//! on short-lived threads of its own, with large stacks: loading ([`Workspace::load_crate`], [`load_workspace`]),
//! viewing ([`View`]), finding references (renames and removals), checking edits ([`EditSet::preview`] and
//! [`EditSet::apply`]), parsing new source ([`edit::replace`], [`edit::insert`]), and formatting ([`edit::format`])
//! neither grow the calling thread's source map nor need a large stack on it. Only parsing a [`CfgExpr`] from text
//! (as [`CfgContext::enable`] does) happens on the calling thread, which keeps that (short) text.

#![warn(missing_docs)]

pub mod cfg;
pub mod edit;
mod error;
mod load;
pub mod model;
pub mod path;
pub mod pattern;
pub mod query;
pub mod resolve;
pub mod source;

#[cfg(feature = "mcp")]
pub mod mcp;

#[cfg(feature = "cargo")]
pub mod workspace;

#[cfg(test)]
mod test_registry;

pub use cfg::CfgContext;
pub use cfg::CfgExpr;
pub use cfg::Tristate;
pub use edit::EditSet;
pub use error::Error;
pub use model::Crate;
pub use model::CrateId;
pub use model::CrateSpec;
pub use model::ItemData;
pub use model::ItemId;
pub use model::ItemKind;
pub use model::Workspace;
pub use path::CanonicalPath;
pub use path::ItemPath;
pub use pattern::MatchOptions;
pub use pattern::PathPattern;
pub use query::Find;
pub use query::FindMatch;
pub use query::View;
pub use query::ViewMode;
pub use resolve::Resolver;
pub use resolve::Viewpoint;
pub use rscode_fmt;
pub use rscode_fmt::Edition;
pub use rscode_sort;

#[cfg(feature = "cargo")]
pub use workspace::LoadOptions;

#[cfg(feature = "cargo")]
pub use workspace::load_workspace;
