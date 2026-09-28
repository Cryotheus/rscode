//! Shapes.

pub mod round;

use round::Radius;

/// A circle.
#[derive(Debug, Clone, Copy)]
pub struct Circle {
	radius: Radius,
}

impl Circle {
	/// Creates a circle.
	pub fn new(radius: f64) -> Self {
		let radius = Radius(radius);

		Self { radius }
	}

	/// The radius.
	pub fn radius(&self) -> Radius {
		self.radius
	}
}

/// Something with an area.
pub trait Shape {
	/// The area.
	fn area(&self) -> f64;
}

impl Shape for Circle {
	fn area(&self) -> f64 {
		std::f64::consts::PI * self.radius.0 * self.radius.0
	}
}

/// Kinds of shapes.
pub enum Kind {
	Round,
	Square,
	Triangle,
}
