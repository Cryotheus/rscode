//! Selecting more workspace members, to search them for what a path names when it names nothing in the selected
//! crates.

use super::LoadOptions;
use crate::model::Workspace;
use crate::path::ItemPath;
use smol_str::SmolStr;

/// Load options that select more workspace members than others did (see [`LoadOptions::widened`]).
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Widening {
	/// The options, with more members selected.
	pub options: LoadOptions,

	/// The members (package names) that the options select and the narrower ones did not: those that were loaded
	/// (only to find references in them), then the others, each in member order.
	pub members: Vec<SmolStr>,

	/// The packages (names) that the narrower options selected, in member order: when a path names items of several
	/// crates once more members are selected, those of these packages are what it named before.
	pub selected: Vec<SmolStr>,
}

impl Widening {
	/// The members of [`Widening::members`] that `workspace`, loaded with [`Widening::options`], has selected crates
	/// of: the target options (`lib`, `bins`, ...) may leave out the crates of the others.
	pub fn searched(&self, workspace: &Workspace) -> Vec<SmolStr> {
		let selected = selected_packages(workspace);

		self.members.iter().filter(|member| selected.contains(member)).cloned().collect()
	}
}

impl LoadOptions {
	/// Options that select more workspace members, to search them when a path names nothing in the crates of
	/// `workspace`, which was loaded with these options. When `path` starts with the name of a crate of a member that
	/// is not loaded (`::member::Item`, `member::Item`; see [`Workspace::unloaded_member_of`]), that member is selected
	/// besides the packages the options selected; otherwise every member is (`workspace`). `None` when every member
	/// is selected already.
	///
	/// Selecting more members can make paths name more items: `crate::Item` names the items of every selected crate
	/// (see [`LoadOptions::with_members`]).
	pub fn widened(&self, workspace: &Workspace, path: Option<&ItemPath>) -> Option<Widening> {
		if self.workspace {
			return None;
		}

		if let Some(member) = path.and_then(|path| workspace.unloaded_member_of(path)) {
			let members = vec![member.name.clone()];

			return Some(Widening {
				options: self.with_members(workspace, &members),
				members,
				selected: selected_packages(workspace),
			});
		}

		let selected = selected_packages(workspace);

		// members that are loaded only to find references in them, and those that are not loaded at all
		let loaded = (workspace.packages().iter())
			.filter(|package| package.is_member && !selected.contains(&package.name))
			.map(|package| package.name.clone());
		let unloaded = workspace.unloaded_members().iter().map(|member| member.name.clone());
		let mut members: Vec<SmolStr> = loaded.chain(unloaded).collect();

		members.dedup();

		if members.is_empty() {
			return None;
		}

		let mut options = self.clone();

		options.workspace = true;
		options.packages.clear();
		options.exclude.clear();
		Some(Widening {
			options,
			members,
			selected,
		})
	}

	/// Options that select the workspace members `members` (package names) besides the packages that these options
	/// selected in `workspace`, which was loaded with them: to run again with only the members that the options of
	/// [`LoadOptions::widened`] found something in, so that paths such as `crate::Item` name no more items than needed.
	pub fn with_members(&self, workspace: &Workspace, members: &[impl AsRef<str>]) -> LoadOptions {
		let mut packages = selected_packages(workspace);
		let mut options = self.clone();

		for member in members.iter().map(AsRef::as_ref) {
			if !packages.iter().any(|package| package == member) {
				packages.push(member.into());
			}
		}

		options.workspace = false;
		options.packages = packages.iter().map(SmolStr::to_string).collect();
		options.exclude.clear();
		options
	}
}

/// The packages (names) of the selected crates of `workspace`, in member order.
fn selected_packages(workspace: &Workspace) -> Vec<SmolStr> {
	let mut selected: Vec<SmolStr> = (workspace.selected_crates())
		.filter_map(|krate| krate.package())
		.map(|package| workspace.package(package).name.clone())
		.collect();

	selected.dedup();
	selected
}
