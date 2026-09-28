//! A crate for the tests of `rscode`'s edit operations.

pub mod docs;
pub mod nested;
#[path = "custom/placed.rs"]
pub mod placed;
pub mod shapes;
mod util;

pub use shapes::Circle;
use shapes::round::Radius;

// Attached comment about `add`.
/// Adds two numbers.
#[inline]
pub fn add(left: i32, right: i32) -> i32 {
	left + right
} // trailing comment of add

/// Doubles a number.
pub fn twice(value: i32) -> i32 {
	util::double(value)
}

/// Only with the `extra` feature.
#[cfg(feature = "extra")]
pub fn extra() -> &'static str {
	"extra"
}

/// Without the `extra` feature.
#[cfg(not(feature = "extra"))]
pub fn extra() -> &'static str {
	"plain"
}

/// The radius of the unit circle.
pub fn unit() -> Radius {
	Circle::new(1.0).radius()
}

mod inline {
	/// A function of an inline module.
	pub fn inner() -> u8 {
		1
	}
}
