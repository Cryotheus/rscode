//! CRLF line breaks and a byte order mark.

pub mod inline {
	/// Docs.
	pub fn f() -> &'static str {
		"a
b"
	}
}
