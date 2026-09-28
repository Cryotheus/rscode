//! Implementations, away from their types.

use crate::shapes::Circle;
use crate::shapes::Shape;
use crate::shapes::Square;

impl Circle {
	/// Creates a circle.
	pub fn new(radius: f64) -> Self {
		Self { radius }
	}
}

impl Shape for Circle {
	fn area(&self) -> f64 {
		std::f64::consts::PI * self.radius * self.radius
	}
}

// squares are simple
impl Shape for Square {
	fn area(&self) -> f64 {
		self.0 * self.0
	}



	fn name(&self) -> &'static str {
		"square"
	}
}

impl std::fmt::Display for Circle {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "circle({})", self.radius)
	}
}
