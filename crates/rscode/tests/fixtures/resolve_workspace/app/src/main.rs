use mylib::api;
use renamed::Client as C;

mod sub {
	pub fn run() {
		let _ = crate::C::connect();
		let _ = super::api::Client::connect();
	}
}

fn main() {
	sub::run();
}
