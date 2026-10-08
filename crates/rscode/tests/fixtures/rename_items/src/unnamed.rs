//! Unnamed items, and items made by a macro.

pub struct Marker;

const _: () = {
	let _ = Marker;
};

macro_rules! marked {
	() => {
		pub fn marked() -> Marker {
			Marker
		}
	};
}

marked!();
