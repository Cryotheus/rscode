//! The example of the crate documentation: sorting a scrambled version of it restores it.

mod common;

use common::assert_stable;

const SORTED: &str = "\
// macros are barriers: they stay in place, and the items after them are sorted on their own
macro_rules! papa {
    () => {};
}

mod alfa;

use bravo::Charlie;
use bravo::delta::{Echo, Foxtrot};

pub use Golf;
pub use hotel::India;

type Juliet = Kilo;

// enum, union, and struct are all sorted together in the same group
struct Lima;

// impl items follow immediately after the type they target (if it is defined in the same file)
impl Lima {
    // new goes at the top
    fn new() -> Option<Self> {
        /* ... code ... */
    }

    // other non-method functions follow
    unsafe fn new_unchecked() -> Option<Self> {
        /* ... code ... */
    }

    // typical methods follow
    fn mike(&self) {
        /* ... code ... */
    }

    // underscore-prefixed function names (implementation helpers) have the same sorting as their non-underscore-prefixed names
    // but they are always after the non-underscore-prefixed name
    fn _mike(&self) {
        /* ... code ... */
    }
}

impl November {
    // the definition doesn't exist here
    //so it goes after all data types (enum, union, struct)
}

unsafe extern \"C\" {
    //extern blocks have their items sorted and combined too
    //always make sure to check the attributes before merging!
}

mod oscar {
    //inline modules are sorted the same as a file is
}

#[cfg(test)]
mod tests {
    //modules with the `#[cfg(test)]` attribute are sorted in a group after normal modules
}
";

const SCRAMBLED: &str = "\
// macros are barriers: they stay in place, and the items after them are sorted on their own
macro_rules! papa {
    () => {};
}
#[cfg(test)]
mod tests {
    //modules with the `#[cfg(test)]` attribute are sorted in a group after normal modules
}
impl November {
    // the definition doesn't exist here
    //so it goes after all data types (enum, union, struct)
}
pub use hotel::India;
mod oscar {
    //inline modules are sorted the same as a file is
}
// impl items follow immediately after the type they target (if it is defined in the same file)
impl Lima {
    // typical methods follow
    fn mike(&self) {
        /* ... code ... */
    }
    // underscore-prefixed function names (implementation helpers) have the same sorting as their non-underscore-prefixed names
    // but they are always after the non-underscore-prefixed name
    fn _mike(&self) {
        /* ... code ... */
    }
    // other non-method functions follow
    unsafe fn new_unchecked() -> Option<Self> {
        /* ... code ... */
    }


    // new goes at the top
    fn new() -> Option<Self> {
        /* ... code ... */
    }
}
use bravo::delta::{Echo, Foxtrot};
unsafe extern \"C\" {
    //extern blocks have their items sorted and combined too
    //always make sure to check the attributes before merging!
}
type Juliet = Kilo;
pub use Golf;
// enum, union, and struct are all sorted together in the same group
struct Lima;
use bravo::Charlie;
mod alfa;
";

#[test]
fn scrambled_example_sorts_to_the_documented_layout() {
	assert_eq!(rscode_sort::sort_str(SCRAMBLED).unwrap(), SORTED);
	assert_stable(SORTED);
}

#[test]
fn documented_example_matches() {
	let docs = include_str!("../src/lib.rs");
	let example: String = docs
		.split("```text\n")
		.nth(1)
		.and_then(|rest| rest.split("//! ```").next())
		.unwrap()
		.lines()
		.map(|line| format!("{}\n", line.strip_prefix("//! ").or_else(|| line.strip_prefix("//!")).unwrap()))
		.collect();

	assert_eq!(example, SORTED);
}
