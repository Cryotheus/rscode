mod b;
#[path = "c.rs"]
mod c;
mod inl {
	mod d;
	#[path = "e.rs"]
	mod e;
}
#[path = "x"]
mod inl_path {
	mod d;
}
