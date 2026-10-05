//! The sources of this workspace's dependencies in cargo's registry, for tests on large real crates.

use std::path::Path;
use std::path::PathBuf;

/// The version of a dependency that this workspace's `Cargo.lock` locks it to (the highest, when it locks several).
pub(crate) fn locked_version(name: &str) -> String {
	let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
	let lock = std::fs::read_to_string(&lock).unwrap_or_else(|error| panic!("cannot read {}: {error}", lock.display()));
	let name_line = format!("name = \"{name}\"");

	lock.split("[[package]]")
		.filter(|package| package.lines().any(|line| line.trim() == name_line))
		.filter_map(|package| package.lines().find_map(|line| line.trim().strip_prefix("version = \"")?.strip_suffix('"')))
		.max_by_key(|version| version.split('.').map(|part| part.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>())
		.unwrap_or_else(|| panic!("{name} is not in Cargo.lock"))
		.to_owned()
}

/// The source of a dependency in cargo's registry, at its [locked version](locked_version), if it is there.
pub(crate) fn registry_crate(name: &str) -> Option<PathBuf> {
	let cargo_home = std::env::var_os("CARGO_HOME")
		.map(PathBuf::from)
		.or_else(|| std::env::home_dir().map(|home| home.join(".cargo")))?;
	let directory = format!("{name}-{}", locked_version(name));

	(std::fs::read_dir(cargo_home.join("registry/src")).ok()?.filter_map(Result::ok))
		.map(|index| index.path().join(&directory))
		.find(|path| path.is_dir())
}
