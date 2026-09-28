//! The features and optional dependencies enabled per package, resolved over the local packages.
//!
//! By default, cargo's feature resolution algorithm (`cargo::core::resolver::features::FeatureResolver`) runs over the
//! local packages only (the workspace members and their path dependencies), which needs no dependency resolution at
//! all, after checking the features requested of local packages like cargo's dependency resolver does. With
//! [`LoadOptions::exact_features`], cargo's own resolvers run instead (see [`super::exact`]).
//!
//! [`LoadOptions::exact_features`]: super::LoadOptions::exact_features

use super::context::LocalPackages;
use super::platform::Platforms;
use crate::Error;
use cargo::core::Dependency;
use cargo::core::FeatureValue;
use cargo::core::Package;
use cargo::core::PackageId;
use cargo::core::Summary;
use cargo::core::Target;
use cargo::core::Workspace;
use cargo::core::dependency::DepKind;
use cargo::core::resolver::CliFeatures;
use cargo::core::resolver::ResolveBehavior;
use cargo::core::resolver::features::FeaturesFor;
use cargo::util::closest;
use cargo::util::closest_msg;
use cargo::util::interning::InternedString;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;

/// What a package is built for, which cargo resolves features separately for (`FeaturesFor`, without artifact
/// dependencies).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub(super) enum Build {
	/// The platform crates are compiled for (`FeaturesFor::NormalOrDev`).
	Target,

	/// The host, for build scripts and proc-macros and their dependencies (`FeaturesFor::HostDep`).
	Host,
}

impl Build {
	/// What a target is built for.
	pub fn of(target: &Target) -> Self {
		if target.proc_macro() { Self::Host } else { Self::Target }
	}

	/// What the package's features are reported for: its library's build (or the target's, without a library).
	pub fn of_package(package: &Package) -> Self {
		package.library().map_or(Self::Target, Self::of)
	}

	pub fn features_for(self) -> FeaturesFor {
		FeaturesFor::from_for_host(self == Self::Host)
	}

	fn other(self) -> Self {
		match self {
			Self::Target => Self::Host,
			Self::Host => Self::Target,
		}
	}
}

/// The features and optional dependencies activated per package and [`Build`].
///
/// A package is recorded for both builds when cargo does not resolve them separately (with resolver version 1).
#[derive(Debug, Default)]
pub(super) struct Activation {
	features: HashMap<(PackageId, Build), BTreeSet<InternedString>>,

	/// Activated optional dependencies, by their name in `Cargo.toml`.
	dependencies: HashMap<(PackageId, Build), BTreeSet<InternedString>>,
}

impl Activation {
	/// Whether the package was resolved (for any build).
	pub fn contains(&self, package: PackageId) -> bool {
		self.features.contains_key(&(package, Build::Target)) || self.features.contains_key(&(package, Build::Host))
	}

	/// The package's features for the build, if it was resolved for the build.
	pub fn features(&self, package: PackageId, build: Build) -> Option<&BTreeSet<InternedString>> {
		self.features.get(&(package, build))
	}

	/// The features a crate of the package is loaded with: those of the build, or of the other build if the package was
	/// only resolved for that (e.g. the tests of a proc-macro package that is only built as a dependency).
	pub fn crate_features(&self, package: PackageId, build: Build) -> Option<&BTreeSet<InternedString>> {
		self.features(package, build).or_else(|| self.features(package, build.other()))
	}

	/// Whether the package uses the dependency when built for `build` (or for the other build, if the package was only
	/// resolved for that): always for required dependencies, and for optional ones when a feature activates them.
	pub fn uses(&self, package: PackageId, build: Build, dependency: &Dependency) -> bool {
		if !dependency.is_optional() {
			return true;
		}

		let key = if self.features.contains_key(&(package, build)) { build } else { build.other() };

		self.dependencies.get(&(package, key)).is_some_and(|names| names.contains(&dependency.name_in_toml()))
	}

	/// Records features of a package's build.
	pub fn add_features(&mut self, package: PackageId, build: Build, features: impl IntoIterator<Item = InternedString>) {
		self.features.entry((package, build)).or_default().extend(features);
	}

	/// Records activated optional dependencies (by their name in `Cargo.toml`) of a package's build.
	pub fn add_dependencies(&mut self, package: PackageId, build: Build, names: impl IntoIterator<Item = InternedString>) {
		self.dependencies.entry((package, build)).or_default().extend(names);
	}

	/// Adds a package's resolution from another activation, unless the package is resolved already.
	pub fn adopt(&mut self, package: PackageId, mut other: Activation) {
		if self.contains(package) {
			return;
		}

		for build in [Build::Target, Build::Host] {
			if let Some(features) = other.features.remove(&(package, build)) {
				self.features.insert((package, build), features);
			}

			if let Some(dependencies) = other.dependencies.remove(&(package, build)) {
				self.dependencies.insert((package, build), dependencies);
			}
		}
	}
}

/// Resolves the features of the local packages like cargo's feature resolver does for building `seeds`: the workspace
/// members being built, with the features requested for them (see `Workspace::members_with_features`).
///
/// This is `FeatureResolver`'s algorithm with the dependency graph restricted to dependencies on local packages. Like
/// cargo, it fails when a feature is requested of a local package that does not have it.
pub(super) fn resolve_local(
	ws: &Workspace<'_>,
	local: &LocalPackages<'_>,
	seeds: &[(&Package, CliFeatures)],
	platforms: &Platforms,
	has_dev_units: bool,
) -> Result<Activation, Error> {
	let unification = Unification::new(ws.resolve_behavior(), has_dev_units);
	let roots = seeds.iter().map(|(member, _)| member.package_id()).collect();
	let mut resolver = LocalResolver::new(local, Emulation::Features, unification, Some(platforms), roots);

	for (member, cli_features) in seeds {
		resolver.activate_member(member, cli_features)?;
	}

	Ok(resolver.into_activation())
}

/// Checks the features requested of the local packages like cargo's dependency resolver does before features are
/// resolved: it resolves every workspace member with all features (for `Cargo.lock`) and then `seeds`, and fails when
/// a feature is requested of a package that does not have it, or a feature enables itself.
pub(super) fn check_local(local: &LocalPackages<'_>, seeds: &[(&Package, CliFeatures)]) -> Result<(), Error> {
	let all_features = CliFeatures::new_all(true);
	let workspace: Vec<(&Package, CliFeatures)> = local.members().map(|member| (member, all_features.clone())).collect();
	let roots = workspace.iter().map(|(member, _)| member.package_id()).collect();
	let mut resolver = LocalResolver::new(local, Emulation::Dependencies, Unification::NONE, None, roots);

	for (member, cli_features) in workspace.iter().chain(seeds) {
		resolver.activate_member(member, cli_features)?;
	}

	Ok(())
}

/// Which of cargo's resolvers a [`LocalResolver`] emulates.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Emulation {
	/// The feature resolver, which decides the features packages are built with.
	Features,

	/// The dependency resolver, which activates everything the feature resolver might: it also activates the optional
	/// dependencies that features only refer to weakly (`dep_name?/feature`).
	Dependencies,
}

/// Options changing how features are unified (`FeatureOpts`), from the workspace's resolver version and whether
/// dev-dependencies are in use.
#[derive(Debug, Clone, Copy)]
struct Unification {
	/// Resolve features for [`Build::Host`] separately.
	decouple_host_deps: bool,

	/// Ignore dev-dependencies (when no example, test, or benchmark is built).
	decouple_dev_deps: bool,

	/// Ignore target-specific dependencies of other platforms.
	ignore_inactive_targets: bool,
}

impl Unification {
	/// Everything unified, like cargo's dependency resolver (and its feature resolver with resolver version 1).
	const NONE: Self = Self {
		decouple_host_deps: false,
		decouple_dev_deps: false,
		ignore_inactive_targets: false,
	};

	fn new(behavior: ResolveBehavior, has_dev_units: bool) -> Self {
		let v2 = !matches!(behavior, ResolveBehavior::V1);

		Self {
			decouple_host_deps: v2,
			decouple_dev_deps: v2 && !has_dev_units,
			ignore_inactive_targets: v2,
		}
	}

	/// Whether [`Build::Host`] is tracked while traversing the dependency graph.
	fn track_for_host(self) -> bool {
		self.decouple_host_deps || self.ignore_inactive_targets
	}

	/// The key features are recorded under.
	fn key(self, build: Build) -> Build {
		if self.decouple_host_deps { build } else { Build::Target }
	}
}

/// A dependency of a package.
struct Edge<'a> {
	dependency: &'a Dependency,

	/// The local package the dependency refers to. Other packages are not resolved.
	package: Option<&'a Package>,

	/// What the dependency is built for.
	build: Build,
}

/// A port of cargo's `FeatureResolver` that only resolves the features of local packages, and that checks the
/// features requested of them like cargo's dependency resolver does.
struct LocalResolver<'a> {
	local: &'a LocalPackages<'a>,
	emulation: Emulation,
	unification: Unification,

	/// The platforms whose target-specific dependencies are followed (with [`Unification::ignore_inactive_targets`]).
	platforms: Option<&'a Platforms>,

	/// The packages being built, which alone have their dev-dependencies resolved (like `ResolveOpts::dev_deps`).
	roots: HashSet<PackageId>,

	activated_features: HashMap<(PackageId, Build), BTreeSet<InternedString>>,
	activated_dependencies: HashMap<(PackageId, Build), BTreeSet<InternedString>>,

	/// Packages whose dependencies were activated, to avoid cycles.
	processed_deps: HashSet<(PackageId, Build)>,

	/// `dep_name?/feature` values waiting for the optional dependency `dep_name` to be activated.
	deferred_weak_dependencies: HashMap<(PackageId, Build, InternedString), BTreeSet<InternedString>>,

	/// The package and dependency each package (other than the roots) was first reached through, for error messages.
	parents: HashMap<PackageId, (PackageId, &'a Dependency)>,
}

impl<'a> LocalResolver<'a> {
	fn new(
		local: &'a LocalPackages<'a>,
		emulation: Emulation,
		unification: Unification,
		platforms: Option<&'a Platforms>,
		roots: HashSet<PackageId>,
	) -> Self {
		Self {
			local,
			emulation,
			unification,
			platforms,
			roots,
			activated_features: HashMap::new(),
			activated_dependencies: HashMap::new(),
			processed_deps: HashSet::new(),
			deferred_weak_dependencies: HashMap::new(),
			parents: HashMap::new(),
		}
	}

	/// Activates a member being built with the features requested on the command line (`do_resolve`).
	fn activate_member(&mut self, member: &'a Package, cli_features: &CliFeatures) -> Result<(), Error> {
		check_requested(member, cli_features)?;

		let values = requested_values(member, cli_features);

		// a selected proc-macro package is also built for the target (its tests and binaries are)
		let build = if self.unification.track_for_host() && member.proc_macro() {
			self.activate_package(member, Build::Target, &values)?;
			Build::Host
		} else {
			Build::Target
		};

		self.activate_package(member, build, &values)
	}

	fn activate_package(&mut self, package: &'a Package, build: Build, values: &[FeatureValue]) -> Result<(), Error> {
		self.activated_features.entry((package.package_id(), self.unification.key(build))).or_default();

		for value in values {
			self.activate_value(package, build, value)?;
		}

		if !self.processed_deps.insert((package.package_id(), build)) {
			// features activated later are propagated as they are activated
			return Ok(());
		}

		for edge in self.dependencies(package, build) {
			// optional dependencies are activated by features
			if let Some(dependency_package) = edge.package
				&& !edge.dependency.is_optional()
			{
				self.activate_edge(package, &edge, dependency_package)?;
			}
		}

		Ok(())
	}

	/// Activates the package a dependency refers to, with the features the dependency declares (`fvs_from_dependency`).
	fn activate_edge(&mut self, parent: &'a Package, edge: &Edge<'a>, package: &'a Package) -> Result<(), Error> {
		if !self.roots.contains(&package.package_id()) {
			self.parents.entry(package.package_id()).or_insert((parent.package_id(), edge.dependency));
		}

		let mut values = Vec::new();

		for &feature in edge.dependency.features() {
			self.check_feature(parent, edge.dependency, package, feature)?;
			values.push(FeatureValue::new(feature));
		}

		if edge.dependency.uses_default_features() && package.summary().features().contains_key("default") {
			values.push(FeatureValue::Feature("default".into()));
		}

		self.activate_package(package, edge.build, &values)
	}

	fn activate_value(&mut self, package: &'a Package, build: Build, value: &FeatureValue) -> Result<(), Error> {
		match value {
			FeatureValue::Feature(feature) => self.activate_feature(package, build, *feature),
			FeatureValue::Dep { dep_name } => self.activate_dependency(package, build, *dep_name),
			FeatureValue::DepFeature {
				dep_name,
				dep_feature,
				weak,
			} => self.activate_dependency_feature(package, build, *dep_name, *dep_feature, *weak),
		}
	}

	/// Activates a feature and the features and dependencies it enables.
	fn activate_feature(&mut self, package: &'a Package, build: Build, feature: InternedString) -> Result<(), Error> {
		let key = (package.package_id(), self.unification.key(build));

		if !self.activated_features.entry(key).or_default().insert(feature) {
			return Ok(());
		}

		// every feature activated is in the map (see `check_requested` and `check_feature`)
		let Some(values) = package.summary().features().get(&feature) else {
			return Ok(());
		};

		if values.contains(&FeatureValue::Feature(feature)) {
			return Err(Error::Cargo(format!("cyclic feature dependency: feature `{feature}` depends on itself")));
		}

		for value in values {
			self.activate_value(package, build, value)?;
		}

		Ok(())
	}

	/// Activates an optional dependency (`dep:name`).
	fn activate_dependency(&mut self, package: &'a Package, build: Build, dep_name: InternedString) -> Result<(), Error> {
		let key = (package.package_id(), self.unification.key(build));

		self.activated_dependencies.entry(key).or_default().insert(dep_name);

		let deferred = self.deferred_weak_dependencies.remove(&(package.package_id(), build, dep_name));

		for edge in self.dependencies(package, build) {
			let Some(dependency_package) = edge.package.filter(|_| edge.dependency.name_in_toml() == dep_name) else {
				continue;
			};

			for &feature in deferred.iter().flatten() {
				self.check_feature(package, edge.dependency, dependency_package, feature)?;
				self.activate_value(dependency_package, edge.build, &FeatureValue::new(feature))?;
			}

			self.activate_edge(package, &edge, dependency_package)?;
		}

		Ok(())
	}

	/// Activates a feature of a dependency (`dep_name/feature` or, if the dependency is activated anyway,
	/// `dep_name?/feature`).
	fn activate_dependency_feature(
		&mut self,
		package: &'a Package,
		build: Build,
		dep_name: InternedString,
		dep_feature: InternedString,
		weak: bool,
	) -> Result<(), Error> {
		let key = (package.package_id(), self.unification.key(build));

		for edge in self.dependencies(package, build) {
			if edge.dependency.name_in_toml() != dep_name {
				continue;
			}

			if edge.dependency.is_optional() {
				let activated = self.activated_dependencies.get(&key).is_some_and(|names| names.contains(&dep_name));

				// the dependency resolver activates weakly referenced dependencies, the feature resolver waits for them
				if weak && !activated && self.emulation == Emulation::Features {
					self.deferred_weak_dependencies.entry((package.package_id(), build, dep_name)).or_default().insert(dep_feature);

					continue;
				}

				self.activate_dependency(package, build, dep_name)?;

				// `dep_name/feature` also enables the implicit feature of the optional dependency, if there is one
				if !weak && package.summary().features().contains_key(&dep_name) {
					self.activate_feature(package, build, dep_name)?;
				}
			}

			if let Some(dependency_package) = edge.package {
				self.check_feature(package, edge.dependency, dependency_package, dep_feature)?;
				self.activate_value(dependency_package, edge.build, &FeatureValue::new(dep_feature))?;
			}
		}

		Ok(())
	}

	/// The package's dependencies that are in use for the build.
	fn dependencies(&self, package: &'a Package, build: Build) -> Vec<Edge<'a>> {
		package
			.dependencies()
			.iter()
			.filter(|dependency| self.follows(package, dependency, build))
			.map(|dependency| {
				let dependency_package = self.local.find(dependency);
				let for_host = dependency.is_build() || dependency_package.and_then(Package::library).is_some_and(Target::proc_macro);
				let build = if build == Build::Target && self.unification.track_for_host() && for_host {
					Build::Host
				} else {
					build
				};

				Edge {
					dependency,
					package: dependency_package,
					build,
				}
			})
			.collect()
	}

	/// Whether cargo's resolvers follow a dependency of the package.
	fn follows(&self, package: &Package, dependency: &Dependency, build: Build) -> bool {
		if self.unification.ignore_inactive_targets
			&& dependency.platform().is_some()
			&& let Some(platforms) = self.platforms
		{
			let platform = if dependency.is_build() || build == Build::Host {
				platforms.host()
			} else {
				platforms.target()
			};

			if !platform.activates(dependency) {
				return false;
			}
		}

		// cargo only resolves the dev-dependencies of the packages being built (and the feature resolver ignores them
		// unless examples, tests, or benchmarks are built, with resolver version 2)
		dependency.kind() != DepKind::Development || (self.roots.contains(&package.package_id()) && !self.unification.decouple_dev_deps)
	}

	/// Checks that a package has a feature a dependency on it requests.
	fn check_feature(&self, parent: &Package, dependency: &Dependency, package: &Package, feature: InternedString) -> Result<(), Error> {
		if package.summary().features().contains_key(&feature) {
			Ok(())
		} else {
			Err(self.missing_feature(parent, dependency, package, feature))
		}
	}

	/// cargo's error for a feature that a dependency requests of a package that does not have it.
	fn missing_feature(&self, parent: &Package, dependency: &Dependency, package: &Package, feature: InternedString) -> Error {
		let name = dependency.package_name();
		let summary = package.summary();
		let same_name: Vec<&Dependency> = summary.dependencies().iter().filter(|other| other.name_in_toml() == feature).collect();

		let explanation = if same_name.is_empty() {
			match closest(&feature, summary.features().keys(), |feature| feature.as_str()) {
				Some(similar) => format!("help: there is a feature `{similar}` with a similar name\n"),
				None if summary.features().is_empty() => String::new(),
				None => {
					let mut features: Vec<&str> = summary.features().keys().map(|feature| feature.as_str()).collect();

					features.sort_unstable();
					format!("help: available features: {}\n", features.join(", "))
				}
			}
		} else if same_name.iter().any(|other| other.is_optional()) {
			"note: an optional dependency with that name exists, but that dependency uses the \"dep:\" syntax in the \
			features table, so it does not have an implicit feature with that name.\n"
				.to_owned()
		} else {
			"note: a required dependency with that name exists, but only optional dependencies can be used as features.\n".to_owned()
		};

		Error::Cargo(format!(
			"failed to select a version for `{name}`.\n    ... required by {path}\nversions that meet the requirements \
			`{requirement}` are: {version}\n\npackage `{parent}` depends on `{name}` with feature `{feature}` but `{name}` \
			does not have that feature.\n{explanation}\n\nfailed to select a version for `{name}` which could resolve this conflict",
			path = self.path(parent.package_id()),
			requirement = dependency.version_req(),
			version = package.version(),
			parent = parent.name(),
		))
	}

	/// How cargo describes the way a package was reached from a package being built (`describe_path`).
	fn path(&self, package: PackageId) -> String {
		let mut description = format!("package `{package}`");
		let mut current = package;

		// every package was first reached from one reached before it, so this ends at a package being built
		for _ in 0..self.parents.len() {
			let Some(&(parent, dependency)) = self.parents.get(&current) else {
				break;
			};

			description.push_str(&format!(
				"\n    ... which satisfies path dependency `{}` of package `{parent}`",
				dependency.name_in_toml()
			));

			current = parent;
		}

		description
	}

	/// The results for every package (recorded under both builds when they are not resolved separately).
	fn into_activation(self) -> Activation {
		let mut activation = Activation::default();
		let packages: HashSet<PackageId> = self.activated_features.keys().map(|&(package, _)| package).collect();

		for package in packages {
			for build in [Build::Target, Build::Host] {
				let key = (package, self.unification.key(build));

				if let Some(features) = self.activated_features.get(&key) {
					let dependencies = self.activated_dependencies.get(&key).into_iter().flatten().copied();

					activation.add_features(package, build, features.iter().copied());
					activation.add_dependencies(package, build, dependencies);
				}
			}
		}

		activation
	}
}

/// Checks the features requested on the command line for a member being built, like cargo's dependency resolver
/// (`build_requirements` and `resolve_features`): `Workspace::members_with_features` lets some through, and does not
/// check them at all for the package in the current directory with resolver version 1.
fn check_requested(member: &Package, cli_features: &CliFeatures) -> Result<(), Error> {
	let summary = member.summary();

	for value in cli_features.features.iter() {
		if let FeatureValue::Feature(feature) = value
			&& !summary.features().contains_key(feature)
		{
			return Err(missing_requested_feature(summary, *feature));
		}
	}

	for value in cli_features.features.iter() {
		if let FeatureValue::DepFeature { dep_name, .. } = value
			&& !summary.dependencies().iter().any(|dependency| dependency.name_in_toml() == *dep_name)
		{
			return Err(Error::Cargo(format!(
				"package `{}` does not have a dependency named `{dep_name}`",
				summary.package_id()
			)));
		}
	}

	Ok(())
}

/// cargo's error for a feature requested on the command line that a package being built does not have.
fn missing_requested_feature(summary: &Summary, feature: InternedString) -> Error {
	let id = summary.package_id();
	let mut same_name = summary.dependencies().iter().filter(|dependency| dependency.name_in_toml() == feature).peekable();

	let message = if same_name.peek().is_none() {
		let suggestion = closest_msg(&feature, summary.features().keys(), |feature| feature.as_str(), "feature");

		format!("package `{id}` does not have the feature `{feature}`{suggestion}")
	} else if same_name.any(Dependency::is_optional) {
		let enabling: Vec<InternedString> = features_enabling(summary, feature).collect();
		let mut suggestion = String::new();

		if !enabling.is_empty() {
			suggestion = format!("\nDependency `{feature}` would be enabled by these features:");

			for name in enabling.iter().take(3) {
				suggestion.push_str(&format!("\n\t- `{name}`"));
			}

			if enabling.len() > 3 {
				suggestion.push_str("\n\t  ...");
			}
		}

		format!(
			"package `{id}` does not have feature `{feature}`\n\nhelp: an optional dependency with that name exists, but \
			the `features` table includes it with the \"dep:\" syntax so it does not have an implicit feature with that \
			name{suggestion}"
		)
	} else {
		format!(
			"package `{id}` does not have feature `{feature}`\n\nhelp: a dependency with that name exists but it is \
			required dependency and only optional dependencies can be used as features."
		)
	};

	Error::Cargo(message)
}

/// The features that activate an optional dependency, sorted.
fn features_enabling(summary: &Summary, dependency: InternedString) -> impl Iterator<Item = InternedString> + '_ {
	let enables = move |value: &FeatureValue| match value {
		FeatureValue::Dep { dep_name } => *dep_name == dependency,
		FeatureValue::DepFeature { dep_name, weak, .. } => !weak && *dep_name == dependency,
		FeatureValue::Feature(_) => false,
	};

	summary.features().iter().filter(move |(_, values)| values.iter().any(enables)).map(|(&name, _)| name)
}

/// The feature values requested for a member on the command line (`fvs_from_requested`).
fn requested_values(package: &Package, cli_features: &CliFeatures) -> Vec<FeatureValue> {
	let features = package.summary().features();
	let mut values: Vec<FeatureValue> = cli_features.features.iter().cloned().collect();

	if cli_features.uses_default_features && features.contains_key("default") {
		values.push(FeatureValue::Feature("default".into()));
	}

	if cli_features.all_features {
		values.extend(features.keys().map(|&feature| FeatureValue::Feature(feature)));
	}

	values
}
