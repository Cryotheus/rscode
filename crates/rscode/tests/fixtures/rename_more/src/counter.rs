pub trait Limit {
	const MAX: u32;

	fn limit(&self) -> u32 {
		Self::MAX
	}

	fn describe(&self) -> String {
		format!("limit {}", self.limit())
	}
}

pub struct Counter;

impl Counter {
	pub const MAX: u32 = 10;

	pub fn limit(&self) -> u32 {
		Self::MAX
	}
}

impl Limit for Counter {
	const MAX: u32 = 20;

	fn limit(&self) -> u32 {
		<Self as Limit>::MAX
	}
}

pub fn values(counter: &Counter) -> (u32, u32, u32, u32, String) {
	(Counter::MAX, <Counter as Limit>::MAX, Counter::limit(counter), Limit::limit(counter), Counter::describe(counter))
}
