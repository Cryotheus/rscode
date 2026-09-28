//! Fixture for finding and viewing items.

pub mod shapes;
mod impls;
pub mod nested;

pub use nested::*;
pub use shapes::Circle;
pub use shapes::Shape as ShapeTrait;

/// Adds two numbers.
///
/// With a second paragraph.
pub fn add(left: i32, right: i32) -> i32 {
	left + right
}

/// On unix.
#[cfg(unix)]
pub fn platform() -> &'static str {
	"unix"
}

/// Elsewhere.
#[cfg(not(unix))]
pub fn platform() -> &'static str {
	"other"
}

#[cfg(feature = "extra")]
pub mod extra {
	pub fn bonus() {}
}
