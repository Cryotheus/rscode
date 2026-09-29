//! cargo's global context and workspace, and the selection of packages.

use super::LoadOptions;
use super::cargo_error;
use crate::Error;
use cargo::GlobalContext;
use cargo::core::Dependency;
use cargo::core::Package;
use cargo::core::PackageId;
use cargo::core::PackageIdSpec;
use cargo::core::PackageIdSpecQuery;
use cargo::core::Workspace;
use cargo::ops::Packages;
use cargo::util::command_prelude::root_manifest;
use cargo::util::interning::InternedString;
use cargo_util_terminal::Shell;
use std::collections::HashMap;
use std::collections::HashSet;
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

	// rscode never accesses the network, so cargo always runs offline
	gctx.configure(0, options.silent, None, options.frozen, options.locked, true, &None, &[], &options.config)
		.map_err(cargo_error)?;

	Ok(gctx)
}

/// Selects packages like cargo's `-p`, `--workspace`, and `--exclude` flags do.
pub(super) fn select_packages<'ws>(ws: &'ws Workspace<'_>, options: &LoadOptions) -> Result<Selection<'ws>, Error> {
	let packages = Packages::from_flags(options.workspace, options.exclude.clone(), options.packages.clone()).map_err(cargo_error)?;

	// cargo's own validation: errors for empty selections and unmatched patterns, warnings for unknown `--exclude`s
	let specs = packages.to_package_id_specs(ws).map_err(cargo_error)?;

	let members: Vec<&Package> = match &packages {
		// `get_packages` would fail for unknown `--exclude`s, which cargo only warns about
		Packages::OptOut(_) => ws
			.members()
			.filter(|member| specs.iter().any(|spec| spec.matches(member.package_id())))
			.collect(),
		_ => packages.get_packages(ws).map_err(cargo_error)?,
	};

	// cargo errors out above already; features cannot be resolved for nothing
	if members.is_empty() {
		return Err(Error::Cargo(format!("no packages selected in workspace `{}`", ws.root().display())));
	}

	let specs = members.iter().map(|member| member.package_id().to_spec()).collect();

	Ok(Selection { packages, members, specs })
}

/// Loads the workspace of the manifest at `manifest_path`, or of the nearest `Cargo.toml` from the current directory
/// upwards, like cargo's `--manifest-path`.
pub(super) fn workspace<'gctx>(gctx: &'gctx GlobalContext, manifest_path: Option<&Path>) -> Result<Workspace<'gctx>, Error> {
	// an absolute path without `..` (`Workspace::new` compares paths textually), resolved like cargo does
	let manifest = root_manifest(manifest_path, gctx).map_err(cargo_error)?;

	Workspace::new(&manifest, gctx).map_err(cargo_error)
}
