#[macros::describe]
struct Described;

#[test]
fn expands() {
	let _described = Described;
}
