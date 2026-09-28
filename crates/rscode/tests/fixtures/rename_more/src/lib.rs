//! Associated items shadowing each other, provided trait items, re-export chains, `cfg` variants, and a module file
//! shared with the binary.

pub mod captures;
pub mod chain;
pub mod counter;
pub mod shared;
pub mod variants;

pub use chain::Token;

#[cfg(unix)]
pub struct Platform {
	pub name: &'static str,
}

#[cfg(not(unix))]
pub struct Platform {
	pub name: &'static str,
}

pub fn platform() -> Platform {
	Platform { name: "here" }
}
