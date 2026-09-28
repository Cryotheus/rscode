#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Marker;

pub const ZERO: i32 = 0;

pub fn classify(value: i32, marker: Marker) -> &'static str {
	let Marker = marker;

	match value {
		ZERO => "zero",
		other if other > ZERO => "positive",
		_ => "negative",
	}
}

pub fn bindings(pair: (i32, i32)) -> i32 {
	let (zero, one) = pair;

	match pair {
		(ZERO, x) => x + zero + one,
		(x, _) => x,
	}
}
