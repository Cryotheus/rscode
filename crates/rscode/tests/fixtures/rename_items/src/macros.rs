macro_rules! double {
	($value:expr) => {
		$value * 2
	};
}

#[macro_export]
macro_rules! triple {
	($value:expr) => {
		$crate::macros::times(3, $value)
	};
	(twice $value:expr) => {
		triple!(triple!($value))
	};
}

pub fn times(factor: i32, value: i32) -> i32 {
	factor * value
}

pub fn use_macros() -> i32 {
	double!(1) + triple!(2) + crate::triple!(3) + triple!(twice 4)
}
