//! A library used by `app`.

/// A widget; see [`Widget::new`].
pub struct Widget;

impl Widget {
	pub fn new() -> Self {
		Widget
	}

	pub fn label(&self) -> String {
		String::from("widget")
	}
}

impl Default for Widget {
	fn default() -> Self {
		Self::new()
	}
}

pub mod inner {
	pub fn helper() -> u8 {
		1
	}
}

#[macro_export]
macro_rules! shout {
	($text:expr) => {
		format!("{}!", $text)
	};
}
