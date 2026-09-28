use crate::shapes::Circle;

pub const LIMIT: u32 = 10;

pub static COUNTER: u32 = 0;

pub fn helper(value: f64) -> f64 {
	value * 2.0
}

pub fn shadowing() -> f64 {
	let helper = 3.0;

	helper + crate::util::helper(1.0)
}

pub fn closure() -> f64 {
	let apply = |helper: f64| helper + 1.0;

	apply(helper(2.0))
}

pub fn make() -> Circle {
	Circle { radius: helper(1.0) }
}

pub fn local_items() -> f64 {
	fn helper(value: f64) -> f64 {
		value
	}

	helper(1.0)
}

pub fn local_import() -> f64 {
	use crate::shapes::Circle as C;

	C::new(1.0).radius
}

pub fn outer() -> f64 {
	fn inner() -> f64 {
		helper(2.0)
	}

	let helper = 1.0;

	helper + inner()
}

pub fn check(value: u32) -> bool {
	match value {
		LIMIT => true,
		limit => limit > LIMIT + COUNTER,
	}
}

pub fn show() -> String {
	format!("{LIMIT} {} {}", helper(1.0), COUNTER)
}

pub fn repeated() -> Vec<f64> {
	vec![helper(1.0); 3]
}

pub fn guarded(value: Option<u32>) -> bool {
	matches!(value, Some(found) if found == LIMIT)
}
