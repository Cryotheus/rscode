//! Nesting, macros, constants, and raw identifiers.

/// An outer module.
pub mod outer {
	/// An inner module.
	pub mod inner {
		/// Deeply nested.
		pub fn deep() -> u8 {
			1
		}
	}

	pub fn shallow() {}

	/// A table.
	pub const TABLE: [u8; 3] = [
		1,
		2,
		3,
	];

	pub const SHORT: u8 = 1;

	pub static GREETING: &str = "hello
  world";

	pub fn text() -> &'static str {
		let s = "first
second";

		s
	}

	pub trait Limits {
		const MAX: u8 = u8::MAX
			- 1;

		fn check() -> bool;
	}

	unsafe extern "C" {
		pub fn abs(value: i32) -> i32;
	}
}

macro_rules! square {
	($x:expr) => {
		$x * $x
	};
}

thread_local!(static COUNTER: u8 = 0);

pub fn r#match() -> u8 {
	square!(2)
}

pub mod r#type {
	/// A unit struct.
	pub struct Unit; // with a trailing comment

	pub type Alias = Unit;

	pub union Bits {
		pub int: u32,
		pub float: f32,
	}
}
