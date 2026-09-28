//! Helpers shared by the integration tests.

#![allow(dead_code)]

use rscode_fmt::Edition;
use rscode_fmt::FormatOptions;
use rscode_fmt::FormatTarget;
use rscode_fmt::Formatter;
use rscode_fmt::RsFormatter;
use rscode_fmt::RustFmtOptions;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// A fixture directory.
pub fn fixture(path: &str) -> PathBuf {
	PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(path)
}

/// The rustfmt program rscode_fmt runs by default: `$RUSTFMT`, else `rustfmt`.
pub fn rustfmt_program() -> OsString {
	std::env::var_os("RUSTFMT").filter(|program| !program.is_empty()).unwrap_or_else(|| "rustfmt".into())
}

/// Whether `rustfmt` can be run. Tests that need it return early (with a message) when it cannot.
pub fn rustfmt_available() -> bool {
	static AVAILABLE: OnceLock<bool> = OnceLock::new();

	let available = *AVAILABLE.get_or_init(|| {
		Command::new(rustfmt_program()).arg("--version").output().is_ok_and(|output| output.status.success())
	});

	if !available {
		eprintln!("rustfmt is not available: skipping");
	}

	available
}

/// rustfmt options with rustfmt's default configuration (so that no configuration file around the repository is
/// picked up) and the 2024 edition.
pub fn rustfmt_options() -> RustFmtOptions {
	RustFmtOptions {
		edition: Some(Edition::E2024),
		config_path: Some(fixture("rustfmt/default/rustfmt.toml")),
		..RustFmtOptions::default()
	}
}

/// Options for rustfmt with [`rustfmt_options`], without sorting.
pub fn rustfmt() -> FormatOptions {
	FormatOptions::new().rustfmt(rustfmt_options())
}

/// Options for prettyplease, without sorting.
pub fn prettyplease() -> FormatOptions {
	FormatOptions::new().formatter(RsFormatter::PrettyPlease)
}

/// The byte offset of the first occurrence of `needle`.
pub fn offset(source: &str, needle: &str) -> usize {
	source.find(needle).unwrap_or_else(|| panic!("`{needle}` not found"))
}

/// Item targets at the first occurrence of each needle.
pub fn targets(source: &str, needles: &[&str]) -> Vec<FormatTarget> {
	needles.iter().map(|needle| FormatTarget::Item(offset(source, needle))).collect()
}

/// Formats the items starting at the first occurrence of each needle.
pub fn format_items(options: FormatOptions, source: &str, needles: &[&str]) -> String {
	Formatter::new(options).format_items(source, &targets(source, needles)).unwrap()
}
