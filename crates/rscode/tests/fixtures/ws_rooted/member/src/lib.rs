pub fn greet() -> &'static str {
	if cfg!(feature = "loud") { "HELLO" } else { "hello" }
}
