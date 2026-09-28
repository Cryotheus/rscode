pub trait Area {
	fn area(&self) -> f64;

	fn describe(&self) -> String {
		format!("area {}", self.area())
	}
}

pub struct Square(pub f64);

impl Area for Square {
	fn area(&self) -> f64 {
		self.0 * self.0
	}
}

impl Area for crate::shapes::Circle {
	fn area(&self) -> f64 {
		3.0 * self.radius * self.radius
	}
}

pub fn total(shapes: &[&dyn Area]) -> f64 {
	shapes.iter().map(|shape| shape.area()).sum()
}

pub fn explicit(square: &Square) -> f64 {
	Area::area(square) + <Square as Area>::area(square) + Square::area(square)
}

pub fn generic<T: Area>(shape: &T) -> f64 {
	T::area(shape)
}

pub fn local_impl() -> f64 {
	struct Local;

	impl Area for Local {
		fn area(&self) -> f64 {
			1.0
		}
	}

	Local.area()
}

pub fn bounded_by_where<T>(shape: &T) -> f64
where
	T: Area,
{
	T::area(shape)
}

pub struct Wrapper<T>(pub T);

impl<T: Clone + Area> Wrapper<T> {
	pub fn inner(&self) -> f64 {
		T::area(&self.0)
	}
}

impl<T> Wrapper<T> {
	pub fn bounded_later(&self) -> f64
	where
		T: Area,
	{
		T::area(&self.0)
	}
}

pub trait Solid: Area {}

pub fn through_supertrait<S: Solid>(shape: &S) -> f64 {
	S::area(shape)
}
