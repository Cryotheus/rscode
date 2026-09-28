mod shared;

use rename_more::chain::Tok;
use rename_more::Token;

fn main() {
	let _ = (Token, Tok, rename_more::chain::inner::token::Token::default());
	println!("{} {} {}", shared::common(), shared::twice(), rename_more::shared::twice());
	println!("{:?}", rename_more::counter::values(&rename_more::counter::Counter));
	println!("{} {:?}", rename_more::platform().name, rename_more::variants::Shape::all());
}
