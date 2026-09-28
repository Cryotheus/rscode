//! Shapes.

/// A circle.
#[derive(Debug, Clone, Copy)]
pub struct Circle {
	pub radius: f64,
}

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

/// Kinds of shapes.
pub enum Kind {
	Round,
	Square,
}

/// Something with an area.
pub trait Shape {
	fn area(&self) -> f64;
}
