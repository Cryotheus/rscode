pub mod child;

pub fn restricted_user() -> u8 {
	child::restricted()
}
