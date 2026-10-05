//! Assembling a [`WorkspacePlan`] from cargo's view of the workspace.

use super::LoadOptions;
use super::WorkspacePlan;
use super::cargo_error;
use super::context;
use super::context::LocalPackages;
use super::deps::Prelude;
use super::exact;
use super::exact::ResolvedDependencies;
use super::features;
use super::features::Activation;
use super::features::Build;
use super::platform;
use super::platform::Platforms;
use super::required::RequiredFeatures;
use super::rustc_error;
use super::targets;
use super::targets::Candidate;
use crate::CfgContext;
use crate::Error;
use crate::model;
use crate::model::CrateSpec;
use crate::model::TargetKind;
use cargo::compiler::RustcTargetData;
use cargo::resolver::CliFeatures;
use cargo::util::interning::InternedString;
use cargo::workspace::Package;
use cargo::workspace::Workspace;
use smol_str::SmolStr;
use std::io::Write;

/// A crate to load.
struct Planned<'a> {
	candidate: Candidate<'a>,
	selected: bool,
}

/// The spec of a planned crate.
fn crate_spec(
	planned: &Planned<'_>,
	package: Option<model::PackageId>,
	platforms: &Platforms,
	activation: &Activation,
	prelude: &Prelude<'_>,
	cfgs: &[String],
) -> Result<CrateSpec, Error> {
	let candidate = &planned.candidate;
	let platform = platforms.of(candidate.kind);
	let features = activation.crate_features(candidate.package.package_id(), Build::of(candidate.target));
	let mut cfg = platform.cfg_context().clone();

	cfg.set_features(features.into_iter().flatten().map(InternedString::as_str));
	cfg.set_name("test", matches!(candidate.kind, TargetKind::Test | TargetKind::Bench));
	cfg.set_name("proc_macro", candidate.kind == TargetKind::ProcMacro);

	for spec in cfgs {
		cfg.enable(spec)?;
	}

	Ok(CrateSpec {
		name: candidate.target.crate_name().into(),
		root: candidate.root.to_path_buf(),
		kind: candidate.kind,
		edition: candidate.edition(),
		package,
		cfg,
		dependencies: prelude.of(candidate, platform),
		selected: planned.selected,
	})
}

/// A package as rscode's model describes it.
fn package_model(ws: &Workspace<'_>, package: &Package, activation: &Activation) -> model::Package {
	let features = package
		.summary()
		.features()
		.iter()
		.map(|(name, values)| {
			(
				SmolStr::new(name.as_str()),
				values.iter().map(|value| SmolStr::new(value.to_string())).collect(),
			)
		})
		.collect();

	let enabled_features = activation
		.crate_features(package.package_id(), Build::of_package(package))
		.into_iter()
		.flatten()
		.map(|feature| SmolStr::new(feature.as_str()))
		.collect();

	model::Package {
		name: package.name().as_str().into(),
		version: package.version().to_string(),
		manifest_path: package.manifest_path().to_path_buf(),
		features,
		enabled_features,
		is_member: ws.is_member(package),
	}
}

/// Plans the workspace, with cargo's messages written to `output`.
pub(super) fn plan(options: &LoadOptions, output: Box<dyn Write + Send + Sync>) -> Result<WorkspacePlan, Error> {
	validate_cfgs(&options.cfgs)?;

	let gctx = context::global_context(options, output)?;
	let ws = context::workspace(&gctx, options.manifest_path.as_deref())?;
	let local = LocalPackages::new(&ws);
	let selection = context::select_packages(&ws, options)?;
	let cli_features = CliFeatures::from_command_line(&options.features, options.all_features, !options.no_default_features).map_err(cargo_error)?;

	// validates the requested features, and distributes `member/feature`s, before anything expensive
	let seeds = ws.members_with_features(&selection.specs, &cli_features).map_err(cargo_error)?;

	// fails for features that packages do not have like cargo's dependency resolver, before choosing targets like cargo
	// (the exact resolution runs cargo's resolvers instead)
	if !options.exact_features {
		features::check_local(&local, &seeds)?;
	}

	let proposals = targets::propose(&selection, &local, &options.targets)?;
	let kind = platform::compile_kind(&gctx, options.target.as_deref())?;
	let has_dev_units = options.targets.uses_dev_dependencies();

	let (platforms, mut activation, resolved) = if options.exact_features {
		let mut target_data = RustcTargetData::new(&ws, &[kind]).map_err(rustc_error)?;
		let platforms = Platforms::from_target_data(&target_data, kind);
		let (activation, resolved) = exact::resolve_exact(&ws, &mut target_data, kind, &selection.specs, &cli_features, has_dev_units)?;

		(platforms, activation, resolved)
	} else {
		let platforms = Platforms::query(&gctx, kind)?;
		let activation = features::resolve_local(&ws, &local, &seeds, &platforms, has_dev_units)?;

		(platforms, activation, ResolvedDependencies::default())
	};

	let required = RequiredFeatures {
		local: &local,
		activation: &activation,
		resolved: &resolved,
		platforms: &platforms,
		has_dev_units,
	};

	let mut crates = Vec::new();

	for proposal in proposals {
		for warning in required.check(&proposal.candidate)? {
			gctx.shell().warn(warning).map_err(cargo_error)?;
		}

		// like cargo, targets chosen in bulk are skipped without their required features
		if proposal.named || required.enabled(&proposal.candidate) {
			crates.push(Planned {
				candidate: proposal.candidate,
				selected: true,
			});
		}
	}

	if crates.is_empty()
		&& let Some(warning) = targets::unmatched_filters_warning(&options.targets)
	{
		gctx.shell().warn(warning).map_err(cargo_error)?;
	}

	if options.load_all_members {
		let others: Vec<Candidate<'_>> = local
			.members()
			.flat_map(Candidate::of)
			.filter(|candidate| !crates.iter().any(|planned| planned.candidate.same(candidate)))
			.collect();

		crates.extend(others.into_iter().map(|candidate| Planned { candidate, selected: false }));
	}

	// the selected packages and the packages of the planned crates, in member order
	let packages: Vec<&Package> = local
		.members()
		.filter(|member| {
			let id = member.package_id();

			selection.contains(id) || crates.iter().any(|planned| planned.candidate.package.package_id() == id)
		})
		.collect();

	// packages that are only loaded to find references in them get the features they are built with on their own
	for &package in &packages {
		if !activation.contains(package.package_id()) {
			let alone = features::resolve_local(&ws, &local, &[(package, CliFeatures::new_all(false))], &platforms, has_dev_units)?;

			activation.adopt(package.package_id(), alone);
		}
	}

	let prelude = Prelude {
		local: &local,
		activation: &activation,
		resolved: &resolved,
	};

	let specs = crates
		.iter()
		.map(|planned| {
			let index = packages
				.iter()
				.position(|package| package.package_id() == planned.candidate.package.package_id());
			let package = index.map(|index| model::PackageId(index as u32));

			crate_spec(planned, package, &platforms, &activation, &prelude, &options.cfgs)
		})
		.collect::<Result<Vec<CrateSpec>, Error>>()?;

	let unloaded_members = local
		.members()
		.filter(|member| !crates.iter().any(|planned| planned.candidate.package.package_id() == member.package_id()))
		.map(unloaded_member)
		.collect();

	Ok(WorkspacePlan {
		root: ws.root().to_path_buf(),
		packages: packages.iter().map(|package| package_model(&ws, package, &activation)).collect(),
		crates: specs,
		unloaded_members,
	})
}

/// A workspace member none of whose crates are loaded.
fn unloaded_member(package: &Package) -> model::UnloadedMember {
	let mut crate_names: Vec<SmolStr> = Vec::new();

	// libraries come first
	for candidate in Candidate::of(package) {
		let name = SmolStr::new(candidate.target.crate_name());

		if !crate_names.contains(&name) {
			crate_names.push(name);
		}
	}

	model::UnloadedMember {
		name: package.name().as_str().into(),
		version: package.version().to_string(),
		manifest_path: package.manifest_path().to_path_buf(),
		crate_names,
	}
}

/// Checks the `--cfg` specs before doing anything expensive.
fn validate_cfgs(cfgs: &[String]) -> Result<(), Error> {
	let mut context = CfgContext::new();

	for spec in cfgs {
		context.enable(spec)?;
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::workspace::TargetSelection;
	use std::path::Path;
	use std::sync::Arc;
	use std::sync::Mutex;

	/// A writer whose output the test can read.
	#[derive(Clone, Default)]
	struct Captured(Arc<Mutex<Vec<u8>>>);

	impl Captured {
		fn text(&self) -> String {
			String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
		}
	}

	impl Write for Captured {
		fn flush(&mut self) -> std::io::Result<()> {
			Ok(())
		}

		fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
			self.0.lock().unwrap().extend_from_slice(bytes);

			Ok(bytes.len())
		}
	}

	/// Plans with cargo's messages captured.
	fn captured(options: &LoadOptions) -> String {
		let output = Captured::default();

		plan(options, Box::new(output.clone())).unwrap();
		output.text()
	}

	fn fixture() -> LoadOptions {
		LoadOptions {
			manifest_path: Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ws_virtual/Cargo.toml")),
			..LoadOptions::default()
		}
	}

	#[test]
	fn silences_cargo() {
		let options = LoadOptions {
			workspace: true,
			exclude: vec!["nope".to_owned()],
			targets: TargetSelection {
				all_examples: true,
				..TargetSelection::default()
			},
			silent: true,
			..fixture()
		};

		assert_eq!(captured(&options), "");
	}

	#[test]
	fn writes_cargo_messages_to_the_output() {
		let unknown_exclude = LoadOptions {
			workspace: true,
			exclude: vec!["nope".to_owned()],
			..fixture()
		};

		let unmatched_filter = LoadOptions {
			packages: vec!["tool".to_owned()],
			targets: TargetSelection {
				all_examples: true,
				..TargetSelection::default()
			},
			..fixture()
		};

		let warning = captured(&unknown_exclude);

		assert!(
			warning.starts_with("warning: excluded package(s) `nope` not found in workspace `"),
			"{warning}"
		);
		assert_eq!(
			captured(&unmatched_filter),
			"warning: target filter `examples` specified, but no targets matched; this is a no-op\n"
		);
		assert_eq!(captured(&fixture()), "");
	}
}
