//! A module file restricting itself with an inner `cfg`.
#![cfg(windows)]
#![doc(hidden)]

pub fn windows_only() {}
