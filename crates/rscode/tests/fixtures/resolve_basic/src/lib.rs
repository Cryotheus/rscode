//! Name resolution fixture: modules, re-exports, globs, enums, cfg variants, macros, and visibility.

pub mod shapes;
mod util;

pub use shapes::Circle;
pub use shapes::Shape as ShapeTrait;
pub use util::helpers::*;

pub mod prelude {
	pub use crate::Color::*;
	pub use crate::shapes::{self, Circle, Square as Box2};
}

pub enum Color {
	Red,
	Green(u8),
	Blue { level: u8 },
}

#[cfg(unix)]
pub struct Platform;

#[cfg(windows)]
pub struct Platform(u32);

#[macro_export]
macro_rules! exported {
	() => {};
}

mod macros {
	macro_rules! local_macro {
		() => {};
	}

	pub(crate) use local_macro;
}

pub mod vis {
	pub(crate) fn crate_visible() {}

	pub(super) fn super_visible() {}

	pub(in crate::vis) fn in_vis() {}

	fn private() {}

	pub mod inner {
		pub(in crate::vis) fn in_vis_inner() {}
	}
}
