//! The core library, used by `app` as `kore`.

pub struct Engine;

#[cfg(feature = "std")]
impl Engine {
	pub fn start(&self) {}
}

#[cfg(feature = "serde")]
pub use extra::Basic;
