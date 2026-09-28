//! Modules in every layout: `plain.rs`, `nested.rs` with `nested/`, `dir/mod.rs`, an inline module with a child
//! file in `inline/`, and a module loaded with `#[path]`.

pub mod dir;
pub mod nested;
pub mod plain;

pub mod inline {
	pub mod deep;

	pub fn shallow() -> u8 {
		deep::value()
	}
}

#[path = "custom_file.rs"]
pub mod custom;

pub use plain::value as plain_value;

pub fn all() -> u8 {
	plain::value() + nested::child::value() + dir::leaf::value() + inline::deep::value() + inline::shallow() + custom::value() + plain_value()
}
