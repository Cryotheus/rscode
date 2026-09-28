//! Variants through a glob import of their enum; an empty link [ ] and [`Shape::Dot`].

use Shape::*;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
	Dot,
	Line(u8),
	Box { side: u8 },
}

impl Shape {
	pub fn all() -> [Shape; 3] {
		[Self::Dot, Shape::Line(1), Box { side: 2 }]
	}

	pub fn size(&self) -> u8 {
		match *self {
			Dot => 0,
			Self::Line(length) | Shape::Box { side: length } => length,
		}
	}
}

pub fn lines() -> Vec<Shape> {
	[1, 2].into_iter().map(Line).collect()
}

pub fn is_dot(shape: Shape) -> bool {
	matches!(shape, Dot | Line(0))
}
