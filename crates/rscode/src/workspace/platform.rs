//! The platforms crates are compiled for: target triples and their `cfg`s, as rustc reports them through cargo.

use super::cargo_error;
use super::rustc_error;
use crate::CfgContext;
use crate::Error;
use crate::model::TargetKind;
use cargo::GlobalContext;
use cargo::core::Dependency;
use cargo::core::compiler::CompileKind;
use cargo::core::compiler::RustcTargetData;
use cargo::core::compiler::TargetInfo;
use cargo::util::Rustc;
use cargo_platform::Cfg;

/// A platform crates are compiled for.
#[derive(Debug, Clone)]
pub(super) struct Platform {
	/// The target triple (e.g. `x86_64-unknown-linux-gnu`).
	pub triple: String,

	/// The `cfg`s rustc reports for the platform, honoring `RUSTFLAGS` and cargo's configuration (`build.rustflags`,
	/// `--config`, ...).
	pub cfgs: Vec<Cfg>,

	/// [`Platform::cfgs`] as a context for evaluating `cfg` predicates.
	context: CfgContext,
}

impl Platform {
	pub fn new(triple: impl Into<String>, cfgs: Vec<Cfg>) -> Self {
		let context = cfg_context(&cfgs);

		Self {
			triple: triple.into(),
			cfgs,
			context,
		}
	}

	/// Asks rustc about a platform, like cargo does before building.
	fn query(gctx: &GlobalContext, rustc: &Rustc, requested: &[CompileKind], kind: CompileKind) -> Result<Self, Error> {
		let info = TargetInfo::new(gctx, requested, rustc, kind).map_err(rustc_error)?;
		let triple = match &kind {
			CompileKind::Host => rustc.host.as_str(),
			CompileKind::Target(target) => target.short_name(),
		};

		Ok(Self::new(triple, info.cfg().to_vec()))
	}

	/// Whether a dependency applies to this platform (always, unless it is declared in a
	/// `[target.'cfg(..)'.dependencies]` or `[target.<triple>.dependencies]` table that does not match).
	pub fn activates(&self, dependency: &Dependency) -> bool {
		dependency.platform().is_none_or(|platform| platform.matches(&self.triple, &self.cfgs))
	}

	/// The platform's `cfg`s as a context for evaluating `cfg` predicates, before features and crate-specific names
	/// (`test`, `proc_macro`) are set.
	pub fn cfg_context(&self) -> &CfgContext {
		&self.context
	}
}

/// The platform crates are compiled for, and the host platform (for proc-macro crates).
#[derive(Debug, Clone)]
pub(super) struct Platforms {
	target: Platform,

	/// The host platform when it differs from the target: when compiling for a `--target` (even the host's own triple,
	/// as cargo then applies `RUSTFLAGS` to the target only).
	host: Option<Platform>,
}

impl Platforms {
	/// The platforms of target data cargo queried already.
	pub fn from_target_data(target_data: &RustcTargetData<'_>, kind: CompileKind) -> Self {
		let platform = |kind: CompileKind| Platform::new(target_data.short_name(&kind), target_data.cfg(kind).to_vec());
		let host = match kind {
			CompileKind::Host => None,
			CompileKind::Target(_) => Some(platform(CompileKind::Host)),
		};

		Self {
			target: platform(kind),
			host,
		}
	}

	/// Asks rustc (through cargo, so `RUSTC`, `RUSTFLAGS`, and cargo's configuration are honored) about the platform of
	/// `kind` and, when compiling for a target, the host.
	///
	/// rustc's information is not cached in the target directory, so nothing is written.
	pub fn query(gctx: &GlobalContext, kind: CompileKind) -> Result<Self, Error> {
		let rustc = gctx.load_global_rustc(None).map_err(rustc_error)?;
		let requested = [kind];
		let target = Platform::query(gctx, &rustc, &requested, kind)?;
		let host = match kind {
			CompileKind::Host => None,
			CompileKind::Target(_) => Some(Platform::query(gctx, &rustc, &requested, CompileKind::Host)?),
		};

		Ok(Self { target, host })
	}

	/// The platform build scripts and proc-macros are compiled for.
	pub fn host(&self) -> &Platform {
		self.host.as_ref().unwrap_or(&self.target)
	}

	/// The platform a crate of the kind is compiled for.
	pub fn of(&self, kind: TargetKind) -> &Platform {
		match kind {
			TargetKind::ProcMacro | TargetKind::BuildScript => self.host(),
			_ => self.target(),
		}
	}

	/// The platform crates are compiled for.
	pub fn target(&self) -> &Platform {
		&self.target
	}
}

/// Converts rustc's `cfg`s into a context in which every well-known name is definite.
fn cfg_context(cfgs: &[Cfg]) -> CfgContext {
	let lines: Vec<String> = cfgs.iter().map(cfg_line).collect();
	let mut context = CfgContext::from_rustc_print_cfg(&lines.join("\n"));
	let has_name = |name: &str| cfgs.iter().any(|cfg| matches!(cfg, Cfg::Name(ident) if ident.as_str() == name));

	// rustc enables overflow checks along with debug assertions, but only a nightly rustc prints this unstable cfg
	if has_name("debug_assertions") && !has_name("overflow_checks") {
		context.set_name("overflow_checks", true);
	}

	context
}

/// A `cfg` in the syntax of `rustc --print cfg` (and of `#[cfg]`), with the value escaped.
fn cfg_line(cfg: &Cfg) -> String {
	match cfg {
		Cfg::Name(name) => name.to_string(),
		Cfg::KeyPair(key, value) => format!("{key} = {value:?}"),
	}
}

/// The kind of compilation for `--target`, or else cargo's `build.target` configuration (whose first target is used
/// when it lists several).
pub(super) fn compile_kind(gctx: &GlobalContext, target: Option<&str>) -> Result<CompileKind, Error> {
	let requested: Vec<String> = target.map(str::to_owned).into_iter().collect();
	let kinds = CompileKind::from_requested_targets(gctx, &requested).map_err(cargo_error)?;

	Ok(kinds.first().copied().unwrap_or(CompileKind::Host))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::CfgExpr;
	use crate::Tristate;
	use std::str::FromStr;

	fn cfgs(lines: &[&str]) -> Vec<Cfg> {
		lines.iter().map(|line| Cfg::from_str(line).unwrap()).collect()
	}

	#[test]
	fn converts_rustc_cfgs() {
		let platform = Platform::new(
			"x86_64-unknown-linux-gnu",
			cfgs(&[
				"debug_assertions",
				"panic=\"unwind\"",
				"target_abi=\"\"",
				"target_feature=\"sse2\"",
				"target_feature=\"fxsr\"",
				"target_os=\"linux\"",
				"unix",
				"r#from_rustflags",
			]),
		);

		assert_eq!(eval(&platform, "unix"), Tristate::True);
		assert_eq!(eval(&platform, "windows"), Tristate::False);
		assert_eq!(eval(&platform, r#"target_os = "linux""#), Tristate::True);
		assert_eq!(eval(&platform, r#"target_os = "windows""#), Tristate::False);
		assert_eq!(
			eval(&platform, r#"all(target_feature = "sse2", target_feature = "fxsr")"#),
			Tristate::True
		);
		assert_eq!(eval(&platform, r#"target_feature = "avx2""#), Tristate::False);
		assert_eq!(eval(&platform, r#"target_abi = """#), Tristate::True);
		assert_eq!(eval(&platform, r#"panic = "abort""#), Tristate::False);
		assert_eq!(eval(&platform, "from_rustflags"), Tristate::True);
		assert_eq!(eval(&platform, "debug_assertions"), Tristate::True);
		assert_eq!(eval(&platform, "overflow_checks"), Tristate::True);

		// well-known names rustc never prints are definite, others are unknown
		assert_eq!(eval(&platform, "test"), Tristate::False);
		assert_eq!(eval(&platform, "proc_macro"), Tristate::False);
		assert_eq!(eval(&platform, "miri"), Tristate::False);
		assert_eq!(eval(&platform, "from_build_script"), Tristate::Unknown);

		// features are set per crate
		assert_eq!(eval(&platform, r#"feature = "std""#), Tristate::Unknown);
	}

	#[test]
	fn escapes_values() {
		let key = cargo_platform::Ident {
			name: "key".into(),
			raw: false,
		};

		let platform = Platform::new("custom", vec![Cfg::KeyPair(key, "a\"b\\c".into())]);

		assert_eq!(cfg_line(&platform.cfgs[0]), r#"key = "a\"b\\c""#);
		assert_eq!(eval(&platform, r#"key = "a\"b\\c""#), Tristate::True);
	}

	fn eval(platform: &Platform, predicate: &str) -> Tristate {
		platform.cfg_context().eval(&CfgExpr::parse(predicate).unwrap())
	}

	#[test]
	fn host_defaults_to_target() {
		let linux = Platform::new("x86_64-unknown-linux-gnu", cfgs(&["unix"]));
		let windows = Platform::new("x86_64-pc-windows-msvc", cfgs(&["windows"]));
		let native = Platforms {
			target: linux.clone(),
			host: None,
		};

		let cross = Platforms {
			target: windows,
			host: Some(linux),
		};

		assert_eq!(native.host().triple, "x86_64-unknown-linux-gnu");
		assert_eq!(native.of(TargetKind::ProcMacro).triple, "x86_64-unknown-linux-gnu");
		assert_eq!(cross.of(TargetKind::Lib).triple, "x86_64-pc-windows-msvc");
		assert_eq!(cross.of(TargetKind::Test).triple, "x86_64-pc-windows-msvc");
		assert_eq!(cross.of(TargetKind::ProcMacro).triple, "x86_64-unknown-linux-gnu");
	}

	#[test]
	fn overflow_checks_follow_debug_assertions() {
		let release = Platform::new("x86_64-unknown-linux-gnu", cfgs(&["unix"]));

		assert_eq!(eval(&release, "debug_assertions"), Tristate::False);
		assert_eq!(eval(&release, "overflow_checks"), Tristate::False);
	}
}
