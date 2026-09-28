//! The extern prelude of crates: the dependencies their paths can start with.

use super::context::LocalPackages;
use super::exact::ResolvedDependencies;
use super::features::Activation;
use super::features::Build;
use super::platform::Platform;
use super::targets::Candidate;
use crate::model::Dependency;
use crate::model::TargetKind;
use cargo::core::Dependency as CargoDependency;
use cargo::core::Package;
use cargo::core::Target;
use cargo::core::dependency::DepKind;
use smol_str::SmolStr;

/// What decides which dependencies crates can name.
pub(super) struct Prelude<'a> {
	pub local: &'a LocalPackages<'a>,
	pub activation: &'a Activation,
	pub resolved: &'a ResolvedDependencies,
}

impl Prelude<'_> {
	/// The crates a crate can name, as cargo passes them with `--extern`, sorted by name:
	/// - for binaries, examples, tests, and benchmarks, their package's library;
	/// - the package's normal dependencies (and for examples, tests, and benchmarks, its dev-dependencies) that apply to
	///   the platform, are used (optional ones only when a feature activates them), and have a library.
	pub fn of(&self, candidate: &Candidate<'_>, platform: &Platform) -> Vec<Dependency> {
		let package = candidate.package;
		let build = Build::of(candidate.target);
		let dev = matches!(candidate.kind, TargetKind::Example | TargetKind::Test | TargetKind::Bench);
		let mut prelude = Vec::new();

		if !candidate.kind.is_lib()
			&& let Some(library) = package.library().filter(|library| library.is_linkable())
		{
			let name = SmolStr::new(library.crate_name());

			prelude.push(Dependency {
				name: name.clone(),
				crate_name: name,
				package: Some(package.name().as_str().into()),
				krate: None,
			});
		}

		for dependency in package.dependencies() {
			let kind_applies = match dependency.kind() {
				DepKind::Normal => true,
				DepKind::Development => dev,
				DepKind::Build => false,
			};

			if !kind_applies || !platform.activates(dependency) || !self.activation.uses(package.package_id(), build, dependency) {
				continue;
			}

			let Some(library) = self.library_name(package, dependency) else {
				continue;
			};

			// renamed dependencies (`name = { package = "..." }`) are named after their key, others after their library
			let name = SmolStr::new(dependency.explicit_name_in_toml().map_or_else(|| library.clone(), |name| name.replace('-', "_")));

			// a dependency may be declared more than once (as a dev-dependency too, for other platforms, ...)
			if prelude.iter().any(|existing: &Dependency| existing.name == name) {
				continue;
			}

			prelude.push(Dependency {
				name,
				crate_name: library.into(),
				package: Some(dependency.package_name().as_str().into()),
				krate: None,
			});
		}

		prelude.sort_by(|a, b| a.name.cmp(&b.name));
		prelude
	}

	/// The crate name of a dependency's library, or `None` if it has none.
	fn library_name(&self, package: &Package, dependency: &CargoDependency) -> Option<String> {
		// artifact dependencies (`artifact = "bin"`) are only libraries with `lib = true`
		if dependency.artifact().is_some_and(|artifact| !artifact.is_lib()) {
			return None;
		}

		if let Some(local) = self.local.find(dependency) {
			return local.library().filter(|library| library.is_linkable()).map(Target::crate_name);
		}

		match self.resolved.library(package.package_id(), dependency) {
			Some(library) => library.map(str::to_owned),

			// most libraries are named after their package
			None => Some(dependency.package_name().replace('-', "_")),
		}
	}
}
