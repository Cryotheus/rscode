//! Choosing the targets (crates) of the selected packages, like cargo's target selection flags.

use super::TargetSelection;
use super::context::LocalPackages;
use super::context::Selection;
use crate::Error;
use crate::model::TargetKind;
use cargo::core::Edition as CargoEdition;
use cargo::core::Package;
use cargo::core::Target;
use cargo::core::TargetKind as CargoTargetKind;
use cargo::ops::Packages;
use cargo::util::closest_msg;
use cargo::util::restricted_names::is_glob_pattern;
use rscode_fmt::Edition;
use std::collections::BTreeMap;
use std::path::Path;

/// A target of a package that can be loaded as a crate.
#[derive(Debug, Clone, Copy)]
pub(super) struct Candidate<'a> {
	pub package: &'a Package,
	pub target: &'a Target,
	pub kind: TargetKind,

	/// The crate root file.
	pub root: &'a Path,
}

impl<'a> Candidate<'a> {
	/// A loadable target: anything but build scripts (and targets without a source file).
	fn new(package: &'a Package, target: &'a Target) -> Option<Self> {
		Some(Self {
			package,
			target,
			kind: target_kind(target)?,
			root: target.src_path().path()?,
		})
	}

	/// The loadable targets of a package, in the order crates are planned in.
	pub fn of(package: &'a Package) -> Vec<Self> {
		let mut candidates: Vec<Self> = package.targets().iter().filter_map(|target| Self::new(package, target)).collect();

		candidates.sort_by(|a, b| (a.kind, a.target.name()).cmp(&(b.kind, b.target.name())));
		candidates
	}

	pub fn edition(&self) -> Edition {
		edition(self.target.edition())
	}

	/// Whether both are the same target of the same package.
	pub fn same(&self, other: &Candidate<'_>) -> bool {
		self.package.package_id() == other.package.package_id() && self.target == other.target
	}
}

/// Finds the targets of the selected packages.
struct Chooser<'s, 'a> {
	selection: &'s Selection<'a>,
	local: &'s LocalPackages<'a>,
}

impl<'a> Chooser<'_, 'a> {
	/// The targets of the kinds, chosen in bulk.
	fn all(&self, kinds: impl Fn(TargetKind) -> bool) -> Vec<Proposal<'a>> {
		self.candidates()
			.filter(|candidate| kinds(candidate.kind))
			.map(|candidate| Proposal { candidate, named: false })
			.collect()
	}

	/// The targets of every selected package.
	fn candidates(&self) -> impl Iterator<Item = Candidate<'a>> + '_ {
		self.selection.members.iter().copied().flat_map(Candidate::of)
	}

	/// `--lib`: the libraries of the selected packages, of which there must be at least one.
	fn libraries(&self) -> Result<Vec<Proposal<'a>>, Error> {
		let libraries = self.all(TargetKind::is_lib);

		if !libraries.is_empty() {
			return Ok(libraries);
		}

		let names: Vec<&str> = self.selection.members.iter().map(|package| package.name().as_str()).collect();

		Err(match names.as_slice() {
			[name] => Error::Cargo(format!("no library targets found in package `{name}`")),
			_ => Error::Cargo(format!("no library targets found in packages: {}", names.join(", "))),
		})
	}

	/// The targets of the kind named `name` (or matching the glob pattern), of which there must be at least one.
	fn named(&self, name: &str, kind: TargetKind) -> Result<Vec<Proposal<'a>>, Error> {
		let pattern = is_glob_pattern(name)
			.then(|| glob::Pattern::new(name).map_err(|error| Error::Cargo(format!("cannot build glob pattern from `{name}`: {error}"))))
			.transpose()?;

		let matches = |candidate: &Candidate<'_>| {
			candidate.kind == kind
				&& match &pattern {
					Some(pattern) => pattern.matches(candidate.target.name()),
					None => candidate.target.name() == name,
				}
		};

		let proposals: Vec<Proposal<'a>> = self
			.candidates()
			.filter(|candidate| matches(candidate))
			.map(|candidate| Proposal { candidate, named: true })
			.collect();

		if proposals.is_empty() {
			let everywhere = self.local.members().flat_map(Candidate::of);
			let elsewhere: Vec<Candidate<'a>> = everywhere.filter(|candidate| matches(candidate)).collect();

			return Err(self.no_target_error(name, pattern.is_some(), kind, &elsewhere));
		}

		Ok(proposals)
	}

	/// cargo's error for a target name that matches nothing in the selected packages.
	fn no_target_error(&self, name: &str, glob: bool, kind: TargetKind, elsewhere: &[Candidate<'_>]) -> Error {
		// target names of the kind, with the packages that have them
		let mut available: BTreeMap<&str, Vec<&str>> = BTreeMap::new();

		for candidate in self.candidates().filter(|candidate| candidate.kind == kind) {
			available
				.entry(candidate.target.name())
				.or_default()
				.push(candidate.package.name().as_str());
		}

		let in_packages = match &self.selection.packages {
			Packages::Default | Packages::OptOut(_) | Packages::All(_) => " in default-run packages".to_owned(),

			Packages::Packages(specs) => match specs.as_slice() {
				[] => String::new(),
				[spec] => format!(" in `{spec}` package"),
				[spec, ..] => format!(" in `{spec}`, ... packages"),
			},
		};

		let named = if glob { "matches pattern" } else { "named" };
		let description = kind_description(kind);
		let suggestion = closest_msg(name, available.keys(), |name| **name, "target");
		let mut message = format!("no {description} target {named} `{name}`{in_packages}{suggestion}");

		if !elsewhere.is_empty() {
			let mut by_package: BTreeMap<&str, Vec<&str>> = BTreeMap::new();

			for candidate in elsewhere {
				by_package
					.entry(candidate.package.name().as_str())
					.or_default()
					.push(candidate.target.name());
			}

			for (package, mut names) in by_package {
				names.sort_unstable();
				message.push_str(&format!("\nhelp: available {description} in `{package}` package:"));

				for name in names {
					message.push_str(&format!("\n    {name}"));
				}
			}
		} else if suggestion.is_empty() && !available.is_empty() {
			message.push_str(&format!("\nhelp: available {description} targets:"));

			for (name, packages) in available {
				match packages.as_slice() {
					[_] => message.push_str(&format!("\n    {name}")),

					_ => packages
						.iter()
						.for_each(|package| message.push_str(&format!("\n    {name} in package {package}"))),
				}
			}
		}

		Error::Cargo(message)
	}
}

/// A target chosen by the target selection flags.
#[derive(Debug, Clone, Copy)]
pub(super) struct Proposal<'a> {
	pub candidate: Candidate<'a>,

	/// Chosen by name (`--bin <NAME>`) rather than in bulk (by default, `--bins`, `--all-targets`, ...). Like cargo,
	/// bulk-chosen targets are skipped when their `required-features` are not enabled.
	pub named: bool,
}

fn edition(edition: CargoEdition) -> Edition {
	match edition {
		CargoEdition::Edition2015 => Edition::E2015,
		CargoEdition::Edition2018 => Edition::E2018,
		CargoEdition::Edition2021 => Edition::E2021,

		// the permanently unstable future edition is closest to the latest one
		CargoEdition::Edition2024 | CargoEdition::EditionFuture => Edition::E2024,
	}
}

/// How cargo's messages call targets of a kind.
fn kind_description(kind: TargetKind) -> &'static str {
	match kind {
		TargetKind::Lib | TargetKind::ProcMacro => "lib",
		TargetKind::Bin => "bin",
		TargetKind::Example => "example",
		TargetKind::Test => "test",
		TargetKind::Bench => "bench",
		TargetKind::BuildScript => "build script",
	}
}

/// Chooses the targets of the selected packages, in workspace member order and then [`Candidate::of`] order.
///
/// Fails like cargo when a named target does not exist, or `--lib` finds no library.
pub(super) fn propose<'a>(selection: &Selection<'a>, local: &LocalPackages<'a>, targets: &TargetSelection) -> Result<Vec<Proposal<'a>>, Error> {
	let chooser = Chooser { selection, local };
	let mut proposals = Vec::new();

	if targets.all_targets {
		proposals.extend(chooser.all(|kind| kind != TargetKind::BuildScript));
	} else if targets.is_default() {
		proposals.extend(chooser.all(|kind| kind.is_lib() || kind == TargetKind::Bin));
	} else {
		if targets.lib {
			proposals.extend(chooser.libraries()?);
		}

		let rules = [
			(targets.all_bins, &targets.bins, TargetKind::Bin),
			(targets.all_examples, &targets.examples, TargetKind::Example),
			(targets.all_tests, &targets.tests, TargetKind::Test),
			(targets.all_benches, &targets.benches, TargetKind::Bench),
		];

		for (all, names, kind) in rules {
			if all {
				proposals.extend(chooser.all(|other| other == kind));
			} else {
				for name in names {
					proposals.extend(chooser.named(name, kind)?);
				}
			}
		}
	}

	let order = |proposal: &Proposal<'_>| {
		let candidate = &proposal.candidate;

		(
			local.position(candidate.package.package_id()),
			candidate.kind,
			candidate.target.name().to_owned(),
		)
	};

	proposals.sort_by_cached_key(order);
	proposals.dedup_by(|later, kept| {
		let same = later.candidate.same(&kept.candidate);

		if same {
			kept.named |= later.named;
		}

		same
	});

	Ok(proposals)
}

/// The kind of a cargo target, or `None` for build scripts.
fn target_kind(target: &Target) -> Option<TargetKind> {
	Some(match target.kind() {
		CargoTargetKind::Lib(_) if target.proc_macro() => TargetKind::ProcMacro,
		CargoTargetKind::Lib(_) => TargetKind::Lib,
		CargoTargetKind::Bin => TargetKind::Bin,
		CargoTargetKind::ExampleBin | CargoTargetKind::ExampleLib(_) => TargetKind::Example,
		CargoTargetKind::Test => TargetKind::Test,
		CargoTargetKind::Bench => TargetKind::Bench,
		CargoTargetKind::CustomBuild => return None,
	})
}

/// The warning cargo gives when bulk filters (`--bins`, `--examples`, ...) chose no target.
pub(super) fn unmatched_filters_warning(targets: &TargetSelection) -> Option<String> {
	let filters: Vec<&str> = if targets.all_targets {
		vec!["`all-targets`"]
	} else {
		[
			(targets.all_bins, "`bins`"),
			(targets.all_tests, "`tests`"),
			(targets.all_examples, "`examples`"),
			(targets.all_benches, "`benches`"),
		]
		.into_iter()
		.filter_map(|(all, name)| all.then_some(name))
		.collect()
	};

	let plural = if filters.len() > 1 { "filters" } else { "filter" };

	(!filters.is_empty()).then(|| {
		format!(
			"target {plural} {} specified, but no targets matched; this is a no-op",
			filters.join(", ")
		)
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn maps_editions() {
		assert_eq!(edition(CargoEdition::Edition2015), Edition::E2015);
		assert_eq!(edition(CargoEdition::Edition2018), Edition::E2018);
		assert_eq!(edition(CargoEdition::Edition2021), Edition::E2021);
		assert_eq!(edition(CargoEdition::Edition2024), Edition::E2024);
		assert_eq!(edition(CargoEdition::EditionFuture), Edition::E2024);
	}

	#[test]
	fn warns_about_unmatched_bulk_filters() {
		let targets = |f: fn(&mut TargetSelection)| {
			let mut targets = TargetSelection::default();

			f(&mut targets);
			unmatched_filters_warning(&targets)
		};

		assert_eq!(targets(|_| {}), None);
		assert_eq!(targets(|targets| targets.bins = vec!["a".to_owned()]), None);
		assert_eq!(
			targets(|targets| targets.all_examples = true).as_deref(),
			Some("target filter `examples` specified, but no targets matched; this is a no-op")
		);
		assert_eq!(
			targets(|targets| {
				targets.all_bins = true;
				targets.all_benches = true;
			})
			.as_deref(),
			Some("target filters `bins`, `benches` specified, but no targets matched; this is a no-op")
		);
		assert_eq!(
			targets(|targets| targets.all_targets = true).as_deref(),
			Some("target filter `all-targets` specified, but no targets matched; this is a no-op")
		);
	}
}
