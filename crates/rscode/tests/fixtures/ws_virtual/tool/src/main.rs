fn main() {
	app::Engine.start();

	let _shiny = oddly_named::Shiny;

	#[cfg(windows)]
	let _basic = extra::Basic;
}
