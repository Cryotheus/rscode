use std::fmt;

pub trait Shape {
	fn area(&self) -> f64;

	fn name(&self) -> &'static str {
		"shape"
	}
}

pub struct Circle {
	pub radius: f64,
}

pub struct Square(pub f64);

impl Circle {
	pub fn new(radius: f64) -> Self {
		Self { radius }
	}
}

impl Shape for Circle {
	fn area(&self) -> f64 {
		3.0 * self.radius * self.radius
	}
}

impl Shape for Square {
	fn area(&self) -> f64 {
		self.0 * self.0
	}

	fn name(&self) -> &'static str {
		"square"
	}
}

impl fmt::Display for Circle {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "circle")
	}
}

impl<T> Shape for Vec<T> {
	fn area(&self) -> f64 {
		0.0
	}
}
