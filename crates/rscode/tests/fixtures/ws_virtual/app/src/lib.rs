//! The application library.

pub use kore::Engine;

pub mod config {
	#[cfg(feature = "json")]
	pub fn from_json() {}
}

#[macros::describe]
pub struct Settings;
