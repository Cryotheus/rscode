//  this *must* support input and output using `proc-macro2`
//  do not directly transfer `syn` types across crate boundaries, however
//  they should be wrapped in another type if not already accompanied by other data
//
//  convenience functions for using strings should be offered so users of the crate do not need to add `proc-macro2` if they are only working with strings

//! Sort contens of Rust source files.

mod cryotheum;

// Maybe have other ordering schemas in the future.
// For now, I'll just focus on mine.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum OrderingSchema {
	Cryotheum,
}

// TODO: Cryotheum-style deterministic sorting
//
// mod alfa;
//
// use bravo::Charlie;
// use bravo::delta::{Echo, Foxtrot};
//
// pub use Golf;
// pub use hotel::India;
//
// type Juliet = Kilo;
//
// //enum, union, and struct are all sorted together in the same group
// struct Lima;
//
// // impl items follow immediately after the type they target (if it is defined in the same file)
// impl Lima {
// 	// new goes at the top
// 	fn new() -> Option<Self> {
// 		/* ... code ... */
// 	}
//
//
// 	// other non-method functions follow
// 	unsafe fn new_unchecked() -> Option<Self> {
// 		/* ... code ... */
// 	}
//
// 	// typical methods follow
// 	fn mike(&self) {
// 		/* ... code ... */
// 	}
//
// 	// underscore-prefixed function names (implementation helpers) have the same sorting as their non-underscore-prefixed names
// 	// but they are always after the non-underscore-prefixed name
// 	fn _mike(&self) {
// 		/* ... code ... */
// 	}
// }
//
// impl November {
// 	// the definition doesn't exist here
// 	//so it goes after all data types (enum, union, struct)
// }
//
// unsafe extern "C" {
// 	//extern blocks have their items sorted and combined too
// 	//always make sure to check the attributes before merging!
// }
//
// mod oscar {
// 	//inline modules are sorted the same as a file is
// }
//
// #[cfg(test)]
// mod tests {
// 	//modules with the `#[cfg(test)]` attribute are sorted in a group after normal modules
// }
