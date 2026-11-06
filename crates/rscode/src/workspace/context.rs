//! cargo's global context and workspace, and the selection of packages.

use super::LoadOptions;
use super::cargo_error;
use crate::Error;
use cargo::GlobalContext;
use cargo::ops::Packages;
use cargo::util::closest;
use cargo::util::command_prelude::root_manifest;
use cargo::util::interning::InternedString;
use cargo::workspace::Dependency;
use cargo::workspace::Package;
use cargo::workspace::PackageId;
use cargo::workspace::PackageIdSpec;
use cargo::workspace::PackageIdSpecQuery;
use cargo::workspace::Workspace;
use cargo_util_terminal::Shell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Display;
use std::io::Write;
use std::path::Path;

/// The workspace members, and the packages they depend on by path (transitively), which cargo loads without resolving
/// any dependency.
pub(super) struct LocalPackages<'ws> {
	/// In workspace member order.
	members: Vec<&'ws Package>,

	/// Every local package, by name.
	by_name: HashMap<InternedString, Vec<Package>>,
}

impl<'ws> LocalPackages<'ws> {
	pub fn new(ws: &'ws Workspace<'_>) -> Self {
		let members: Vec<&Package> = ws.members().collect();
		let mut by_name: HashMap<InternedString, Vec<Package>> = HashMap::new();
		let mut seen = HashSet::new();
		let mut pending: Vec<Package> = members.iter().map(|&member| member.clone()).collect();

		while let Some(package) = pending.pop() {
			if !seen.insert(package.package_id()) {
				continue;
			}

			for dependency in package.dependencies() {
				// cargo reports broken path dependencies when building; they just cannot be found here
				if let Some(directory) = dependency.source_id().local_path()
					&& let Ok(local) = ws.load(&directory.join("Cargo.toml"))
					&& dependency.matches_id(local.package_id())
				{
					pending.push(local);
				}
			}

			by_name.entry(package.name()).or_default().push(package);
		}

		Self { members, by_name }
	}

	/// The local package a dependency refers to: a member, or another package depended upon by path.
	pub fn find(&self, dependency: &Dependency) -> Option<&Package> {
		self.by_name
			.get(&dependency.package_name())?
			.iter()
			.find(|package| dependency.matches_id(package.package_id()))
	}

	/// The workspace members, in workspace member order.
	pub fn members(&self) -> impl Iterator<Item = &'ws Package> + '_ {
		self.members.iter().copied()
	}

	/// A member's position in the workspace member order (other packages sort last).
	pub fn position(&self, id: PackageId) -> usize {
		self.members.iter().position(|member| member.package_id() == id).unwrap_or(usize::MAX)
	}
}

/// The selected packages, from cargo's package selection flags.
pub(super) struct Selection<'ws> {
	/// The flags, as cargo interprets them.
	pub packages: Packages,

	/// The selected workspace members (never empty), in workspace member order.
	pub members: Vec<&'ws Package>,

	/// [`Selection::members`] as package id specs.
	pub specs: Vec<PackageIdSpec>,
}

impl Selection<'_> {
	pub fn contains(&self, id: PackageId) -> bool {
		self.members.iter().any(|member| member.package_id() == id)
	}
}

/// Creates cargo's global context for the current directory, with its messages written to `output`.
pub(super) fn global_context(options: &LoadOptions, output: Box<dyn Write + Send + Sync>) -> Result<GlobalContext, Error> {
	let cwd = std::env::current_dir().map_err(|error| Error::Cargo(format!("couldn't get the current directory of the process: {error}")))?;
	let home = cargo::util::homedir(&cwd)
		.ok_or_else(|| Error::Cargo("couldn't find your home directory. This probably means that $HOME was not set.".to_owned()))?;

	// both of the shell's streams (status and warnings on stderr, and stdout) go to `output`
	let mut gctx = GlobalContext::new(Shell::from_write(output), cwd, home);

	if let Some(flags) = &options.rustflags {
		let separator = '\x1f';
		if let Some(flag) = flags.iter().find(|flag| flag.contains(separator)) {
			return Err(Error::Cargo(format!(
				"rustflag `{flag}` holds U+001F, which separates CARGO_ENCODED_RUSTFLAGS"
			)));
		}

		let mut env: cargo::util::data_structures::HashMap<String, String> = (std::env::vars())
			.filter(|(name, _)| !name.eq_ignore_ascii_case("RUSTFLAGS") && !name.eq_ignore_ascii_case("CARGO_ENCODED_RUSTFLAGS"))
			.collect();
		if !flags.is_empty() {
			env.insert("CARGO_ENCODED_RUSTFLAGS".to_owned(), flags.join("\x1f"));
		}
		gctx.set_env(env);
	}

	// rscode never accesses the network, so cargo always runs offline
	gctx.configure(0, options.silent, None, options.frozen, options.locked, true, &None, &[], &options.config)
		.map_err(cargo_error)?;

	Ok(gctx)
}

/// Selects packages like cargo's `-p`, `--workspace`, and `--exclude` flags do.
pub(super) fn select_packages<'ws>(ws: &'ws Workspace<'_>, options: &LoadOptions) -> Result<Selection<'ws>, Error> {
	let exclude = spelled_like_members(ws, &options.exclude);
	let packages = spelled_like_members(ws, &options.packages);
	let packages = Packages::from_flags(options.workspace, exclude, packages).map_err(cargo_error)?;

	// cargo's own validation: errors for empty selections, unmatched patterns, and unknown packages, warnings for
	// unknown `--exclude`s
	let specs = packages
		.to_package_id_specs(ws)
		.map_err(|error| unknown_packages_error(ws, &options.packages, error))?;

	let members: Vec<&Package> = match &packages {
		// `get_packages` would fail for unknown `--exclude`s, which cargo only warns about
		Packages::OptOut(_) => ws
			.members()
			.filter(|member| specs.iter().any(|spec| spec.matches(member.package_id())))
			.collect(),

		_ => packages
			.get_packages(ws)
			.map_err(|error| unknown_packages_error(ws, &options.packages, error))?,
	};

	// cargo errors out above already; features cannot be resolved for nothing
	if members.is_empty() {
		return Err(Error::Cargo(format!("no packages selected in workspace `{}`", ws.root().display())));
	}

	let specs = members.iter().map(|member| member.package_id().to_spec()).collect();

	Ok(Selection { packages, members, specs })
}

/// The specs, with package names spelled like the workspace members they name.
///
/// cargo matches package names exactly, while rustc turns the `-`s of a package name into `_`s in the names of its
/// crates, so `-p senpo-core` for the package `senpo_core` (and the other way around) is an easy mistake that says
/// nothing about what was meant.
fn spelled_like_members(ws: &Workspace<'_>, specs: &[String]) -> Vec<String> {
	// a name, or a name with a version (`name@1.0`): other specs (URLs and glob patterns) are left alone
	let spelling = |spec: &str| -> Option<String> {
		let (name, version) = match spec.split_once('@') {
			Some((name, version)) => (name, Some(version)),
			None => (spec, None),
		};

		if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
			return None;
		}

		let folded = name.replace('_', "-");
		let mut members = ws.members().filter(|member| member.name().as_str().replace('_', "-") == folded);
		let member = members.next()?;

		// both spellings name a member, so the spec has to say which
		if members.next().is_some() || member.name().as_str() == name {
			return None;
		}

		Some(match version {
			Some(version) => format!("{}@{version}", member.name()),
			None => member.name().to_string(),
		})
	};

	specs.iter().map(|spec| spelling(spec).unwrap_or_else(|| spec.clone())).collect()
}

/// cargo's error for package specs that name no workspace member, with the members there are to choose from.
fn unknown_packages_error(ws: &Workspace<'_>, specs: &[String], error: impl Display) -> Error {
	let message = format!("{error:#}");

	if !message.contains("not found in workspace") {
		return Error::Cargo(message);
	}

	let members: Vec<&str> = ws.members().map(|member| member.name().as_str()).collect();
	let mut message = format!("{message}\nnote: the workspace members are {}", members.join(", "));

	for spec in specs {
		let Ok(spec) = PackageIdSpec::parse(spec) else { continue };

		// cargo complained about specs that name no member; a member with the spec's name does have another problem
		if ws.members().any(|member| member.name().as_str() == spec.name()) {
			continue;
		}

		if let Some(name) = closest(spec.name(), members.iter().copied(), |name| *name) {
			message.push_str(&format!("\nhelp: did you mean `{name}`?"));
		}
	}

	Error::Cargo(message)
}

/// Loads the workspace of the manifest at `manifest_path`, or of the nearest `Cargo.toml` from the current directory
/// upwards, like cargo's `--manifest-path`.
pub(super) fn workspace<'gctx>(gctx: &'gctx GlobalContext, manifest_path: Option<&Path>) -> Result<Workspace<'gctx>, Error> {
	// an absolute path without `..` (`Workspace::new` compares paths textually), resolved like cargo does
	let manifest = root_manifest(manifest_path, gctx).map_err(cargo_error)?;

	Workspace::new(&manifest, gctx).map_err(cargo_error)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn context(rustflags: Option<Vec<String>>) -> GlobalContext {
		global_context(
			&LoadOptions {
				rustflags,
				..LoadOptions::default()
			},
			Box::new(Vec::new()),
		)
		.unwrap()
	}

	#[test]
	fn empty_rustflags_clear_process_flags_without_overriding_cargo_configuration() {
		let flags = context(Some(Vec::new()));

		assert!(flags.get_env("CARGO_ENCODED_RUSTFLAGS").is_err());
		assert!(flags.get_env("RUSTFLAGS").is_err());
	}

	#[test]
	fn rustflags_override_the_cargo_context_environment() {
		let flags = context(Some(vec!["--cfg".to_owned(), "rscode_probe".to_owned()]));

		assert_eq!(flags.get_env("CARGO_ENCODED_RUSTFLAGS").unwrap(), "--cfg\x1frscode_probe");
		assert!(flags.get_env("RUSTFLAGS").is_err());
	}

	#[test]
	fn rustflags_reject_the_encoded_separator() {
		let error = global_context(
			&LoadOptions {
				rustflags: Some(vec!["bad\x1fflag".to_owned()]),
				..LoadOptions::default()
			},
			Box::new(Vec::new()),
		)
		.unwrap_err();

		assert!(error.to_string().contains("U+001F"), "{error}");
	}
}
