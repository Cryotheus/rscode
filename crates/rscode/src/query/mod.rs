//! Read-only operations: finding and viewing items.

mod find;
mod outline;
mod snippet;
mod view;

pub use find::Find;
pub use find::FindMatch;
pub use find::FindOptions;
pub use outline::outline_text;
pub use view::ItemView;
pub use view::View;
pub use view::ViewMode;
pub use view::ViewOptions;
