//! Local bindings named like new names.

pub struct Unit;

pub fn step(value: u32) -> u32 {
	value + 1
}

pub fn walk<Item: Copy>(start: u32, item: Item) -> (u32, Item) {
	let next = step(start);

	(step(next), item)
}

pub fn make<Item>() -> Unit {
	Unit
}
