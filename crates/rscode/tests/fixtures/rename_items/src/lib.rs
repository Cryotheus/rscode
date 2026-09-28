//! A crate for rename tests: items used across modules and by a binary.

pub mod docs;
pub mod generics;
pub mod keywords;
pub mod macros;
pub mod patterns;
pub mod shapes;
pub mod traits;
pub mod util;

pub use shapes::Circle;
pub use shapes::Circle as Round;
pub use util::*;

/// The unit circle, a [`Circle`].
pub fn unit_circle() -> Circle {
	Circle::new(1.0)
}

#[cfg(unix)]
pub fn platform() -> &'static str {
	"unix"
}

#[cfg(not(unix))]
pub fn platform() -> &'static str {
	"other"
}
