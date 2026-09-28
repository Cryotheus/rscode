pub mod inner {
	pub mod token {
		#[derive(Debug, Default)]
		pub struct Token;
	}

	pub use self::token::Token;
}

pub use inner::Token;
pub use self::inner::token::Token as Tok;
