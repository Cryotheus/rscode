//! A small crate for `cargo-rscode`'s end-to-end tests.

pub mod shapes;
mod util;

pub use shapes::Circle;

/// Adds two numbers.
pub fn add(left: i32, right: i32) -> i32 {
	left + right
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

/// Doubles a number.
pub fn twice(value: i32) -> i32 {
	util::double(value)
}
