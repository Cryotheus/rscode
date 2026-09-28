use demo::shapes::Shape;

fn main() {
	let circle = demo::Circle::new(f64::from(demo::add(1, 2)));
	let area = circle.area();

	println!("{circle:?} {area} {} {}", demo::twice(2), demo::extra());
}
