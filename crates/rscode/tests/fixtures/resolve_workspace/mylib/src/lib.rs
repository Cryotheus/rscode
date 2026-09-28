pub mod api {
	pub struct Client;

	impl Client {
		pub fn connect() -> Self {
			Client
		}
	}
}

pub use api::Client;

mod hidden {
	pub struct Secret;
}
