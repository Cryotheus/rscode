pub fn kind() -> u8 {
	1
}

pub fn use_kind() -> u8 {
	kind() + self::kind()
}
