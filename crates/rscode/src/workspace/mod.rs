//! Loading crates from a cargo workspace (feature `cargo`).
//!
//! [`plan_workspace`] uses cargo as a library to work out what to load without parsing any Rust source: the selected
//! packages and targets (mirroring cargo's package and target selection flags), the enabled features, the `cfg`s of
//! the platform crates are compiled for, and the extern prelude of every crate. [`load_workspace`] (or
//! [`WorkspacePlan::load`]) then loads the planned crates into a [`Workspace`].
//!
//! Planning writes nothing to stdout, never writes `Cargo.lock`, and never accesses the network: cargo's status and
//! warning messages go to stderr (or nowhere, see [`LoadOptions::silent`]), and cargo always runs offline. With the
//! default feature resolution, it modifies no files at all; the exact one uses cargo's files in `$CARGO_HOME` like
//! `cargo metadata` does (see [`LoadOptions::exact_features`]).

mod context;
mod deps;
mod exact;
mod features;
mod plan;
mod platform;
mod required;
mod targets;

use crate::Error;
use crate::model::CrateSpec;
use crate::model::Package;
use crate::model::UnloadedMember;
use crate::model::Workspace;
use serde::Deserialize;
use serde::Serialize;
use std::fmt::Display;
use std::io::Write;
use std::path::PathBuf;

/// Which targets (crates) of the selected packages to load, mirroring cargo's target selection flags.
///
/// When nothing is selected, the library and all binaries are loaded (like `cargo build`). Targets selected in bulk
/// (by default, or with `--bins`, `--examples`, `--tests`, `--benches`, `--all-targets`) are skipped when their
/// `required-features` are not enabled, like cargo does; targets selected by name are always loaded. Build scripts are
/// never loaded.
#[derive(Debug, Default, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct TargetSelection {
	/// `--lib`: the library (or proc-macro) target. It is an error if no selected package has one.
	pub lib: bool,

	/// `--bin <NAME>...`: binaries by name. Glob patterns (`app-*`) are allowed; a name that matches nothing is an
	/// error.
	pub bins: Vec<String>,

	/// `--bins`: all binaries.
	pub all_bins: bool,

	/// `--example <NAME>...`: examples by name (or glob pattern).
	pub examples: Vec<String>,

	/// `--examples`: all examples.
	pub all_examples: bool,

	/// `--test <NAME>...`: integration tests by name (or glob pattern).
	pub tests: Vec<String>,

	/// `--tests`: all integration tests (`tests/*.rs` and `[[test]]` targets).
	///
	/// Unlike cargo, this does not add the unit tests of libraries and binaries: those are part of the library and
	/// binary crates, which are loaded with `cfg(test)` disabled.
	pub all_tests: bool,

	/// `--bench <NAME>...`: benchmarks by name (or glob pattern).
	pub benches: Vec<String>,

	/// `--benches`: all benchmarks (`benches/*.rs` and `[[bench]]` targets).
	pub all_benches: bool,

	/// `--all-targets`: the library, binaries, examples, tests, and benchmarks.
	pub all_targets: bool,
}

impl TargetSelection {
	/// Whether no target was explicitly selected.
	pub fn is_default(&self) -> bool {
		*self == Self::default()
	}

	/// Whether examples, tests, or benchmarks are requested, which puts dev-dependencies in use (like `cargo test`).
	pub fn uses_dev_dependencies(&self) -> bool {
		self.all_targets
			|| self.all_examples
			|| self.all_tests
			|| self.all_benches
			|| !self.examples.is_empty()
			|| !self.tests.is_empty()
			|| !self.benches.is_empty()
	}
}

/// Options for [`plan_workspace`] and [`load_workspace`], mirroring cargo's command-line flags.
#[derive(Debug, Default, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct LoadOptions {
	/// `--manifest-path`. Defaults to the manifest found from the current directory upwards.
	///
	/// Like cargo, a relative path is relative to the current directory, and the path is normalized (without `.` and
	/// `..`), but symbolic links are not resolved.
	pub manifest_path: Option<PathBuf>,

	/// `-p/--package` specs: `name`, `name@version`, or glob patterns (`app-*`).
	///
	/// Without `-p` and `--workspace`, cargo's default applies: the package of the manifest (for `--manifest-path` or
	/// the current directory), or the workspace's `default-members`, or all members of a virtual workspace.
	pub packages: Vec<String>,

	/// `--workspace`
	pub workspace: bool,

	/// `--exclude` specs (with `--workspace`).
	pub exclude: Vec<String>,

	/// The targets of the selected packages to load.
	pub targets: TargetSelection,

	/// `--features`: features to enable (comma or space separated; `package/feature` for a specific member).
	pub features: Vec<String>,

	/// `--all-features`
	pub all_features: bool,

	/// `--no-default-features`
	pub no_default_features: bool,

	/// `--target <triple>`: evaluate `cfg`s (and target-specific dependencies) for this target instead of the host.
	/// Proc-macro crates are still evaluated for the host, which they are compiled for.
	pub target: Option<String>,

	/// `--cfg <spec>`: additional enabled `cfg`s (`test`, `feature="x"`, `my_cfg`), for every crate.
	pub cfgs: Vec<String>,

	/// Also load every other target of every workspace member (as unselected crates), so references in them
	/// can be found (used by renames and removals).
	///
	/// Packages that are only loaded this way get the features the selected packages enable on them, or else the
	/// features they are built with on their own (as with `-p <package>`): their default features.
	pub load_all_members: bool,

	/// Resolve features with cargo's resolvers (exact, but slower, and it needs the registry index) instead of the
	/// fast default.
	///
	/// The default runs cargo's feature resolution algorithm over the local packages only (the workspace members and
	/// the packages they depend on by path), without resolving any dependency versions: it takes a few milliseconds
	/// (besides asking rustc about the target once), needs no registry index, lockfile, or downloaded sources, and
	/// agrees with cargo, including its errors for features that local packages do not have, unless:
	/// - features of a local package are enabled through a registry or git dependency (e.g. a `[patch]`ed one);
	/// - features are requested of registry or git dependencies that they do not have (cargo fails);
	/// - the `required-features` of a target name features of a registry or git dependency (`serde/derive`): they are
	///   assumed to be enabled;
	/// - the library of a registry or git dependency is not named after its package (e.g. the `rust-crypto` package's
	///   library is `crypto`): only the manifests of local packages are read, so other dependencies are assumed to be
	///   named after their package (and to have a library);
	/// - `-Z features`, `resolver.feature-unification`, or artifact dependencies change how cargo unifies features.
	///
	/// Its errors are cargo's, except that they do not mention the versions that dependencies are locked to.
	///
	/// The exact resolution reads the registry index and the dependencies' sources offline, so it fails (with cargo's
	/// error) until they are downloaded, e.g. by `cargo fetch` or a build. It takes about a second. It never writes
	/// `Cargo.lock` (see [`LoadOptions::locked`]), but it uses cargo's files in `$CARGO_HOME` like `cargo metadata`
	/// does: it takes cargo's package cache lock (waiting while another cargo process holds it), records the use of
	/// the packages in cargo's global cache tracker (`$CARGO_HOME/.global-cache`), and unpacks the downloaded sources of
	/// dependencies that are not unpacked yet. cargo may also update its cache of rustc's output in the target
	/// directory.
	pub exact_features: bool,

	/// `--offline`. rscode never accesses the network, so cargo always runs as if this was set.
	pub offline: bool,

	/// `--locked`: with [`LoadOptions::exact_features`], fail (with cargo's error) if `Cargo.lock` is missing or does
	/// not match the resolution, instead of resolving as if it was updated. The default feature resolution resolves no
	/// dependency versions, so this does not affect it.
	pub locked: bool,

	/// `--frozen`: `--locked` and `--offline`.
	pub frozen: bool,

	/// `--config <KEY=VALUE|PATH>` overrides for cargo's configuration.
	pub config: Vec<String>,

	/// Discard cargo's status and warning output instead of writing it to stderr.
	pub silent: bool,
}

/// Everything needed to load a cargo workspace, computed from cargo's metadata without parsing any Rust source.
#[derive(Debug, Clone)]
pub struct WorkspacePlan {
	/// The cargo workspace root directory.
	pub root: PathBuf,

	/// The selected packages and the packages of all planned crates, in workspace member order.
	///
	/// [`CrateSpec::package`] refers to them by index, as [`Workspace::add_package`] numbers packages added in this
	/// order.
	pub packages: Vec<Package>,

	/// The crates to load: the selected ones first, then (with [`LoadOptions::load_all_members`]) every other target
	/// of every workspace member. Each group is in workspace member order, and then library, binaries, examples,
	/// tests, and benchmarks by name.
	pub crates: Vec<CrateSpec>,

	/// The workspace members none of whose crates are planned, in member order.
	pub unloaded_members: Vec<UnloadedMember>,
}

impl WorkspacePlan {
	/// Loads every planned crate into a new [`Workspace`] and links their dependencies.
	pub fn load(self) -> Workspace {
		let mut workspace = Workspace::new(self.root);

		for package in self.packages {
			workspace.add_package(package);
		}

		for spec in self.crates {
			workspace.load_crate(spec);
		}

		for member in self.unloaded_members {
			workspace.add_unloaded_member(member);
		}

		workspace.link();
		workspace
	}

	/// The planned crates that are part of the selection.
	pub fn selected_crates(&self) -> impl Iterator<Item = &CrateSpec> {
		self.crates.iter().filter(|spec| spec.selected)
	}
}

/// Works out which crates of a cargo workspace to load and how (see [`WorkspacePlan`]), without parsing any Rust
/// source.
///
/// Fails with cargo's messages in [`Error::Cargo`] (e.g. for invalid manifests or configuration, and unknown
/// packages, features, or targets), with [`Error::Rustc`] when rustc cannot tell about the target, and with
/// [`Error::CfgParse`] for invalid [`LoadOptions::cfgs`].
pub fn plan_workspace(options: &LoadOptions) -> Result<WorkspacePlan, Error> {
	let output: Box<dyn Write + Send + Sync> = if options.silent {
		Box::new(std::io::sink())
	} else {
		Box::new(std::io::stderr())
	};

	plan::plan(options, output)
}

/// Loads the selected packages' targets of a cargo workspace: [`plan_workspace`], then [`WorkspacePlan::load`].
pub fn load_workspace(options: &LoadOptions) -> Result<Workspace, Error> {
	Ok(plan_workspace(options)?.load())
}

/// An error reported by cargo, with its causes.
fn cargo_error(error: impl Display) -> Error {
	Error::Cargo(format!("{error:#}"))
}

/// An error from running rustc (through cargo), with its causes.
fn rustc_error(error: impl Display) -> Error {
	Error::Rustc(format!("{error:#}"))
}
