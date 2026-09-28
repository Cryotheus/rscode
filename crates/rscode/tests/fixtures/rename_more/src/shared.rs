pub fn common() -> u8 {
	1
}

pub fn twice() -> u8 {
	common() + self::common()
}
