//! Glob imports that import each other.

mod a {
	pub use super::b::*;

	pub struct A;
}

mod b {
	pub use super::a::*;

	pub struct B;
}

pub use a::*;
