//! Read-only operations: finding and viewing items, finding their references, and suggesting what paths that name
//! nothing may have meant.

mod find;
mod outline;
mod references;
mod snippet;
mod suggest;
mod view;

pub use find::Find;
pub use find::FindMatch;
pub use find::FindOptions;
pub use outline::outline_text;
pub use references::FindReferencesOptions;
pub use references::FoundReference;
pub use references::ReferenceReport;
pub use references::find_references;
pub(crate) use snippet::PrintedText;
pub(crate) use snippet::multiline_strings;
pub use suggest::Suggestions;
pub use suggest::suggest;
pub use view::ItemView;
pub use view::LineNumbers;
pub use view::View;
pub use view::ViewMode;
pub use view::ViewOptions;

#[cfg(feature = "mcp")]
pub(crate) use view::shown_item;
