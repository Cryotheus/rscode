use crate::shapes::Circle;

pub fn shadowed<Circle: Clone>(value: Circle) -> Circle {
	value.clone()
}

pub fn not_shadowed(value: Circle) -> Circle {
	value
}

pub struct Wrapper<T>(pub T);

impl<Circle> Wrapper<Circle> {
	pub fn inner(self) -> Circle {
		self.0
	}
}
