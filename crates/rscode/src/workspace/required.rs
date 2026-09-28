//! The `required-features` of targets, which decide whether cargo builds targets chosen in bulk.

use super::context::LocalPackages;
use super::exact::ResolvedDependencies;
use super::features::Activation;
use super::features::Build;
use super::platform::Platforms;
use super::targets::Candidate;
use crate::Error;
use cargo::core::Dependency;
use cargo::core::FeatureValue;
use cargo::core::Package;
use cargo::core::PackageId;
use cargo::core::Summary;
use cargo::core::dependency::DepKind;
use cargo::util::interning::InternedString;

/// Evaluates the `required-features` of the selected packages' targets like cargo does.
pub(super) struct RequiredFeatures<'a> {
	pub local: &'a LocalPackages<'a>,
	pub activation: &'a Activation,
	pub resolved: &'a ResolvedDependencies,
	pub platforms: &'a Platforms,

	/// Whether examples, tests, or benchmarks are built, which puts dev-dependencies in use.
	pub has_dev_units: bool,
}

/// The package a dependency refers to.
struct Known<'a> {
	id: PackageId,

	/// Unknown for packages that cargo resolved, but did not load.
	summary: Option<&'a Summary>,

	proc_macro: bool,
}

impl RequiredFeatures<'_> {
	/// cargo's checks of a target's `required-features` (`validate_required_features`): values that are not allowed
	/// are errors, and the warnings about features that do not exist are returned.
	pub fn check(&self, candidate: &Candidate<'_>) -> Result<Vec<String>, Error> {
		let Some(required) = candidate.target.required_features() else {
			return Ok(Vec::new());
		};

		let package = candidate.package;
		let target = candidate.target.name();
		let mut warnings = Vec::new();

		for feature in required {
			let value = FeatureValue::new(feature.as_str().into());
			let invalid = format!("invalid feature `{value}` in required-features of target `{target}`");

			match &value {
				FeatureValue::Feature(feature) => {
					if !package.summary().features().contains_key(feature) {
						warnings.push(format!("{invalid}: `{value}` is not present in [features] section"));
					}
				}
				FeatureValue::Dep { .. } => {
					return Err(Error::Cargo(format!("{invalid}: `dep:` prefixed feature values are not allowed in required-features")));
				}
				FeatureValue::DepFeature { weak: true, .. } => {
					return Err(Error::Cargo(format!("{invalid}: optional dependency with `?` is not allowed in required-features")));
				}
				FeatureValue::DepFeature {
					dep_name,
					dep_feature,
					weak: false,
				} => {
					let Some(dependency) = package.dependencies().iter().find(|dependency| dependency.name_in_toml() == *dep_name) else {
						warnings.push(format!("{invalid}: dependency `{dep_name}` does not exist"));
						continue;
					};

					// the features of registry and git dependencies are unknown without `exact_features`
					if let Some(known) = self.known(package, dependency)
						&& let Some(summary) = known.summary
						&& !has_feature(summary, *dep_feature)
					{
						warnings.push(format!("{invalid}: feature `{dep_feature}` does not exist in package `{}`", known.id));
					}
				}
			}
		}

		Ok(warnings)
	}

	/// Whether all of a target's `required-features` are enabled (`resolve_all_features`): features of its package, and
	/// `dependency/feature` for features of its dependencies.
	pub fn enabled(&self, candidate: &Candidate<'_>) -> bool {
		let Some(required) = candidate.target.required_features() else {
			return true;
		};

		let package = candidate.package;

		// cargo checks the features of the package's build for the target platform, even for proc-macro packages
		let features = self.activation.features(package.package_id(), Build::Target);

		required.iter().all(|feature| match FeatureValue::new(feature.as_str().into()) {
			FeatureValue::Feature(feature) => features.is_some_and(|features| features.contains(&feature)),
			FeatureValue::DepFeature {
				dep_name,
				dep_feature,
				weak: false,
			} => self.dependency_feature_enabled(package, dep_name, dep_feature),

			// not allowed (see `check`)
			FeatureValue::Dep { .. } | FeatureValue::DepFeature { weak: true, .. } => false,
		})
	}

	/// Whether a feature of a dependency is enabled, for a dependency that the package uses.
	fn dependency_feature_enabled(&self, package: &Package, dep_name: InternedString, dep_feature: InternedString) -> bool {
		let mut dependencies = package
			.dependencies()
			.iter()
			.filter(|dependency| dependency.name_in_toml() == dep_name && self.in_use(package.package_id(), dependency));

		dependencies.any(|dependency| {
			let Some(known) = self.known(package, dependency) else {
				// the features of registry and git dependencies are unknown without `exact_features`
				return true;
			};

			let build = if known.proc_macro || dependency.is_build() { Build::Host } else { Build::Target };

			self.activation.features(known.id, build).is_some_and(|features| features.contains(&dep_feature))
		})
	}

	/// Whether cargo counts the features of a dependency (`PackageSet::filter_deps`): a dev-dependency only when
	/// examples, tests, or benchmarks are built, only for the target or the host platform, and an optional dependency
	/// only when a feature activates it.
	fn in_use(&self, package: PackageId, dependency: &Dependency) -> bool {
		let kind = dependency.kind() != DepKind::Development || self.has_dev_units;
		let platform = self.platforms.target().activates(dependency) || self.platforms.host().activates(dependency);

		kind && platform && self.activation.uses(package, Build::Target, dependency)
	}

	/// The package a dependency of the package refers to, if it is known: a local package, or (with `exact_features`)
	/// the package cargo resolved it to.
	fn known(&self, package: &Package, dependency: &Dependency) -> Option<Known<'_>> {
		if let Some(local) = self.local.find(dependency) {
			return Some(Known {
				id: local.package_id(),
				summary: Some(local.summary()),
				proc_macro: local.proc_macro(),
			});
		}

		let id = self.resolved.package(package.package_id(), dependency)?;

		Some(Known {
			id,
			summary: self.resolved.summary(id),
			proc_macro: self.resolved.is_proc_macro(id),
		})
	}
}

/// Whether a package has a feature, or an optional dependency that can be named like one.
fn has_feature(summary: &Summary, feature: InternedString) -> bool {
	summary.features().contains_key(&feature)
		|| summary.dependencies().iter().any(|dependency| dependency.name_in_toml() == feature && dependency.is_optional())
}
