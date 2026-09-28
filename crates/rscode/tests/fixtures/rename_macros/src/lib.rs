//! Macro bodies, whose tokens are references by their role.

pub mod hints {
	pub fn add(a: usize, b: usize) -> usize {
		a + b
	}
}

macro_rules! capture {
	() => {
		Some(std::backtrace::Backtrace::capture())
	};
}

macro_rules! capture_if {
	($condition:expr) => {
		if $condition { capture!() } else { None }
	};
}

pub struct Counter(pub usize);

impl Counter {
	const ONE: usize = 1;

	pub fn size(&self) -> usize {
		self.0
	}
}

macro_rules! impl_hints {
	($name:ident) => {
		impl $name {
			pub fn hints(&self) -> usize {
				hints::add(self.size(), 1)
			}

			pub fn twice(&self) -> usize {
				self.hints() + crate::hints::add(0, $name::ONE)
			}
		}
	};
}

impl_hints!(Counter);

pub fn backtrace(condition: bool) -> Option<std::backtrace::Backtrace> {
	capture_if!(condition)
}
