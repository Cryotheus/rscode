use dep::Widget;

fn main() {
	let widget = ::dep::Widget::new();
	let other: dep::Widget = Widget::default();

	println!("{} {} {}", widget.label(), other.label(), dep::inner::helper());
	println!("{}", dep::shout!("hi"));
}
