//! `cfg` and `cfg_attr` on items and modules.
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(unix)]
pub fn unix_only() {}

#[cfg(unix)]
#[cfg(feature = "std")]
pub fn two_cfgs() {}

#[cfg_attr(feature = "serde", cfg(test))]
pub fn cfg_via_cfg_attr() {}

#[cfg_attr(unix, cfg_attr(feature = "std", cfg(debug_assertions)))]
pub fn nested_cfg_attr() {}

#[cfg_attr(windows, cfg(unix))]
pub fn false_cfg_attr() {}

#[cfg(this is not a predicate)]
pub fn invalid_cfg() {}

#[cfg_attr(not(feature = "std"), doc(hidden))]
pub fn maybe_hidden() {}

#[doc(hidden, alias = "x")]
pub fn hidden() {}

#[cfg(test)]
mod tests {
	#[test]
	fn a_test() {}
}

#[cfg(feature = "std")]
pub mod std_only {
	pub fn inner() {}
}

pub mod file_cfg;

#[cfg(windows)]
mod broken_windows;
