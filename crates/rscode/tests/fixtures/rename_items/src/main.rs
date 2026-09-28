use rename_items::shapes::Kind;
use rename_items::Circle;

fn main() {
	let circle = Circle::new(2.0);
	let kind = Kind::Round;

	println!("{circle} {} {}", rename_items::util::helper(1.0), rename_items::helper(2.0));
	println!("{} {}", Circle::new(3.0).diameter(), kind.describe());
	println!("{:?} {}", ::rename_items::unit_circle(), rename_items::platform());
	println!("{}", rename_items::Round::new(4.0).radius);
}
