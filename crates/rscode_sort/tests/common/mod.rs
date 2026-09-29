//! Helpers shared by the integration tests.

#![allow(dead_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

/// A small deterministic pseudo-random number generator (xorshift64*).
pub struct Rng(u64);

impl Rng {
	pub fn new(seed: u64) -> Self {
		Self(seed.max(1))
	}

	pub fn next(&mut self) -> u64 {
		self.0 ^= self.0 >> 12;
		self.0 ^= self.0 << 25;
		self.0 ^= self.0 >> 27;
		self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
	}

	/// A number in `0..bound`.
	pub fn below(&mut self, bound: usize) -> usize {
		(self.next() % bound as u64) as usize
	}

	/// `true` with a probability of `1 / n`.
	pub fn one_in(&mut self, n: usize) -> bool {
		self.below(n) == 0
	}

	pub fn shuffle<T>(&mut self, items: &mut [T]) {
		for index in (1..items.len()).rev() {
			items.swap(index, self.below(index + 1));
		}
	}
}

/// Asserts that sorting `output` again changes nothing and that it parses.
#[track_caller]
pub fn assert_stable(output: &str) {
	if let Err(error) = syn::parse_file(output) {
		panic!("sorted output does not parse: {error}\n{output}");
	}

	let again = rscode_sort::sort_str(output).unwrap();

	assert_eq!(again, output, "sorting is not idempotent\n--- first:\n{output}\n--- second:\n{again}");
}

/// The token text of a file as `syn` prints it.
pub fn file_tokens(source: &str) -> String {
	let file: syn::File = syn::parse_file(source).unwrap();

	quote::ToTokens::into_token_stream(file).to_string()
}

/// rustfmt with an empty configuration.
pub struct Rustfmt {
	toolchain: Option<&'static str>,
	config: PathBuf,
}

impl Rustfmt {
	/// rustfmt from the nightly toolchain if rustup provides it, otherwise the default one.
	pub fn find() -> Option<Self> {
		// an empty config, so no `rustfmt.toml` of the environment applies
		let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("rscode_sort_rustfmt");

		std::fs::create_dir_all(&directory).ok()?;

		let config = directory.join("rustfmt.toml");

		std::fs::write(&config, "").ok()?;

		[Some("+nightly"), None].into_iter().find_map(|toolchain| {
			let rustfmt = Self { toolchain, config: config.clone() };

			// by formatting: `--version` succeeds even when no rustup proxy takes the `+toolchain` argument
			rustfmt.format("").is_ok().then_some(rustfmt)
		})
	}

	/// Formats `source` for the 2024 edition, or returns rustfmt's error message.
	pub fn format(&self, source: &str) -> Result<String, String> {
		self.format_for(source, "2024")
	}

	/// Formats `source` for an edition (which is also rustfmt's style edition), or returns rustfmt's error message.
	pub fn format_for(&self, source: &str, edition: &str) -> Result<String, String> {
		let mut child = Command::new("rustfmt")
			.args(self.toolchain)
			.args(["--edition", edition, "--emit", "stdout", "--config-path"])
			.arg(&self.config)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.map_err(|error| error.to_string())?;
		let mut stdin = child.stdin.take().ok_or("no stdin")?;
		let input = source.to_owned();
		let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
		let output = child.wait_with_output().map_err(|error| error.to_string())?;

		writer.join().map_err(|_| "writer panicked")?.map_err(|error| error.to_string())?;

		if !output.status.success() {
			return Err(String::from_utf8_lossy(&output.stderr).into_owned());
		}

		String::from_utf8(output.stdout).map_err(|error| error.to_string())
	}
}
