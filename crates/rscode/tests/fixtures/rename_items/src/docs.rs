//! Docs mentioning [`crate::shapes::Circle`].

use crate::shapes::Circle;

/// A disc, unlike a [`Circle`] (see [the constructor](Circle::new) and [Circle][]).
///
/// ```
/// // [Circle] in code is not a link
/// ```
pub struct Disc;

/// Makes a [`Circle`](struct@Circle).
pub fn make() -> Circle {
	Circle::new(2.0)
}
