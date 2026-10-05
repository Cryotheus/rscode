//! Resolving dependencies and features with cargo's own resolvers ([`LoadOptions::exact_features`]).
//!
//! [`LoadOptions::exact_features`]: super::LoadOptions::exact_features

use super::features::Activation;
use super::features::Build;
use crate::Error;
use cargo::compiler::CompileKind;
use cargo::compiler::RustcTargetData;
use cargo::ops::WorkspaceResolve;
use cargo::resolver::CliFeatures;
use cargo::resolver::ForceAllTargets;
use cargo::resolver::HasDevUnits;
use cargo::resolver::features::ResolvedFeatures;
use cargo::util::interning::InternedString;
use cargo::workspace::Dependency;
use cargo::workspace::Package;
use cargo::workspace::PackageId;
use cargo::workspace::PackageIdSpec;
use cargo::workspace::Summary;
use cargo::workspace::Target;
use cargo::workspace::Workspace;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Display;

/// The packages cargo resolved the dependencies of the workspace members to.
#[derive(Debug, Default)]
pub(super) struct ResolvedDependencies {
	/// By member, and the dependency's name in `Cargo.toml` and package name.
	packages: HashMap<(PackageId, InternedString, InternedString), PackageId>,

	/// The summaries (features and dependencies) of the packages cargo loaded.
	summaries: HashMap<PackageId, Summary>,

	/// The library crate names of the packages cargo loaded (`None` for packages without a library that can be linked).
	libraries: HashMap<PackageId, Option<String>>,

	/// The packages cargo loaded that have a proc-macro target.
	proc_macros: HashSet<PackageId>,
}

impl ResolvedDependencies {
	fn new(ws: &Workspace<'_>, resolve: &WorkspaceResolve<'_>) -> Self {
		let mut resolved = Self::default();

		for package in resolve.pkg_set.packages() {
			let id = package.package_id();
			let library = package.library().filter(|library| library.is_linkable()).map(Target::crate_name);

			resolved.summaries.insert(id, package.summary().clone());
			resolved.libraries.insert(id, library);

			if package.proc_macro() {
				resolved.proc_macros.insert(id);
			}
		}

		// the targeted resolution last, as the resolved features refer to its packages
		for graph in resolve.workspace_resolve.iter().chain([&resolve.targeted_resolve]) {
			for member in ws.members().map(Package::package_id) {
				for (to, dependencies) in graph.deps(member) {
					for dependency in dependencies {
						resolved
							.packages
							.insert((member, dependency.name_in_toml(), dependency.package_name()), to);
					}
				}
			}
		}

		resolved
	}

	/// Whether a resolved package that cargo loaded has a proc-macro target.
	pub fn is_proc_macro(&self, package: PackageId) -> bool {
		self.proc_macros.contains(&package)
	}

	/// The library crate name of a member's dependency (`Some(None)` when it has none), if cargo loaded its package.
	pub fn library(&self, member: PackageId, dependency: &Dependency) -> Option<Option<&str>> {
		let package = self.package(member, dependency)?;

		self.libraries.get(&package).map(Option::as_deref)
	}

	/// The package a member's dependency was resolved to.
	pub fn package(&self, member: PackageId, dependency: &Dependency) -> Option<PackageId> {
		self.packages
			.get(&(member, dependency.name_in_toml(), dependency.package_name()))
			.copied()
	}

	/// The summary of a resolved package, if cargo loaded it.
	pub fn summary(&self, package: PackageId) -> Option<&Summary> {
		self.summaries.get(&package)
	}
}

/// The features of every resolved package, and the optional dependencies the members use.
fn activation(ws: &Workspace<'_>, resolve: &WorkspaceResolve<'_>) -> Activation {
	// one resolution per spec with `feature-unification = "package"`
	let resolutions: Vec<&ResolvedFeatures> = resolve.specs_and_features.iter().map(|resolved| &resolved.resolved_features).collect();
	let mut activation = Activation::default();

	for package in resolve.targeted_resolve.iter() {
		for build in [Build::Target, Build::Host] {
			for resolved in &resolutions {
				if let Some(features) = resolved.activated_features_unverified(package, build.features_for()) {
					activation.add_features(package, build, features);
				}
			}
		}
	}

	for member in ws.members() {
		let id = member.package_id();

		for build in [Build::Target, Build::Host] {
			if activation.features(id, build).is_none() {
				continue;
			}

			let optional = member.dependencies().iter().filter(|dependency| dependency.is_optional());
			let activated = optional.map(Dependency::name_in_toml).filter(|&name| {
				resolutions
					.iter()
					.any(|resolved| resolved.is_dep_activated(id, build.features_for(), name))
			});

			activation.add_dependencies(id, build, activated.collect::<Vec<InternedString>>());
		}
	}

	activation
}

/// A resolution error, with a hint when it may be caused by being offline.
fn offline_error(error: impl Display) -> Error {
	let message = format!("{error:#}");
	let hint = "note: rscode resolves dependencies offline; download the registry index and the dependencies with `cargo fetch`, \
		or resolve features without `exact_features`";

	// how cargo's errors mention being offline (`--frozen` implies `--offline`)
	let phrases = ["offline mode", "--offline was specified", "--frozen was specified"];

	if phrases.iter().any(|phrase| message.contains(phrase)) {
		Error::Cargo(format!("{message}\n{hint}"))
	} else {
		Error::Cargo(message)
	}
}

/// Resolves dependencies and features with cargo's resolvers, offline and without writing `Cargo.lock`.
///
/// With `--locked` (or `--frozen`), it fails like cargo when `Cargo.lock` is missing or does not match the resolution.
pub(super) fn resolve_exact<'gctx>(
	ws: &Workspace<'gctx>,
	target_data: &mut RustcTargetData<'gctx>,
	kind: CompileKind,
	specs: &[PackageIdSpec],
	cli_features: &CliFeatures,
	has_dev_units: bool,
) -> Result<(Activation, ResolvedDependencies), Error> {
	let has_dev_units = if has_dev_units { HasDevUnits::Yes } else { HasDevUnits::No };
	let requested = [kind];

	// cargo writes `Cargo.lock` only when it changes, which `--locked` forbids: then cargo fails instead, which is the
	// only place it checks `--locked`, so only a dry run (which skips all of this) could write the lockfile
	let dry_run = ws.gctx().locked_flag().is_none();

	let resolve = cargo::ops::resolve_ws_with_opts(
		ws,
		target_data,
		&requested,
		cli_features,
		specs,
		has_dev_units,
		ForceAllTargets::No,
		dry_run,
	)
	.map_err(offline_error)?;

	Ok((activation(ws, &resolve), ResolvedDependencies::new(ws, &resolve)))
}
