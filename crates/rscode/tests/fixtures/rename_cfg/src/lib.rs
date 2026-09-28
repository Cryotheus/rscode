//! Names bound to different items under different `cfg`s.

#[cfg(feature = "a")]
pub struct Chain(pub u8);

mod chain {
	#[cfg(feature = "a")]
	pub(crate) use crate::Chain;

	#[cfg(not(feature = "a"))]
	pub(crate) struct Chain(pub u8);

	impl Chain {
		pub fn new() -> Self {
			Chain(0)
		}
	}
}

pub fn make() -> u8 {
	chain::Chain::new().0
}

pub mod unix_impl {
	pub struct Handle;
}

pub mod windows_impl {
	pub struct Handle;

	pub struct Socket;
}

pub mod platform {
	#[cfg(unix)]
	pub use crate::unix_impl::Handle;

	#[cfg(not(unix))]
	pub use crate::windows_impl::Handle;

	pub fn handle() -> Handle {
		Handle
	}
}

pub mod aliased {
	#[cfg(unix)]
	pub use crate::unix_impl::Handle;

	#[cfg(not(unix))]
	pub use crate::windows_impl::Socket as Handle;
}
