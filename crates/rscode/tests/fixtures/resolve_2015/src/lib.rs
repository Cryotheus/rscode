//! Edition 2015 fixture: `use` paths are relative to the crate root.

mod a {
	pub struct S;
}

mod b {
	use a::S;

	pub fn make() -> S {
		S
	}
}

mod c {
	use ::a::S as T;
	use std::fmt;

	pub fn make() -> T {
		::a::S
	}
}
