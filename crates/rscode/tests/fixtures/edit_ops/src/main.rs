mod util;

fn main() {
	let doubled = util::double(1);

	println!("{} {}", edit_ops::twice(doubled), edit_ops::add(1, 2));
}
