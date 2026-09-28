//! Shapes.

use std::fmt;

/// A circle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Circle {
	pub radius: f64,
}

impl Circle {
	/// Creates a circle.
	pub fn new(radius: f64) -> Self {
		Self { radius }
	}

	/// The diameter.
	pub fn diameter(&self) -> f64 {
		2.0 * self.radius
	}

	pub fn doubled(&self) -> Circle {
		Circle::new(self.diameter())
	}
}

impl fmt::Display for Circle {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "circle({})", self.radius)
	}
}

pub enum Kind {
	Round,
	Square(u8),
	Other { name: &'static str },
}

impl Kind {
	pub fn is_round(&self) -> bool {
		matches!(self, Self::Round)
	}

	pub fn describe(&self) -> &'static str {
		use Kind::*;

		match self {
			Round => "round",
			Square(_) => "square",
			Other { name } => name,
		}
	}
}

impl Default for Kind {
	fn default() -> Self {
		Kind::Round
	}
}
