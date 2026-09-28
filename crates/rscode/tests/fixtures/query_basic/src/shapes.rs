//! Shapes.

/// A circle.
#[derive(Debug, Clone, Copy)]
pub struct Circle {
	/// The radius.
	pub radius: f64,
}

/// A square.
pub struct Square(pub f64);

/// Kinds of shapes.
pub enum Kind {
	/// Round.
	Round,
	Square,
}

/// Something with an area.
pub trait Shape {
	/// The area.
	fn area(&self) -> f64;

	/// The name, by default "shape".
	fn name(&self) -> &'static str {
		"shape"
	}
}
