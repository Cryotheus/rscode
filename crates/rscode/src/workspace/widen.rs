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
}

impl LoadOptions {
	/// Options that select more workspace members, to search them when a path names nothing in the crates of
	/// `workspace`, which was loaded with these options. When `path` starts with the name of a crate of a member that
	/// is not loaded (`::member::Item`, `member::Item`; see [`Workspace::unloaded_member_of`]), that member is selected
	/// besides the packages the options selected; otherwise every member is (`workspace`). `None` when every member
	/// is selected already.
	///
	/// Selecting more members can make paths name more items: `crate::Item` names the items of every selected crate.
	pub fn widened(&self, workspace: &Workspace, path: Option<&ItemPath>) -> Option<Widening> {
		if self.workspace {
			return None;
		}

		let mut selected: Vec<SmolStr> = (workspace.selected_crates())
			.filter_map(|krate| krate.package())
			.map(|package| workspace.package(package).name.clone())
			.collect();

		selected.dedup();

		if let Some(member) = path.and_then(|path| workspace.unloaded_member_of(path)) {
			let mut options = self.clone();

			options.packages = selected.iter().chain([&member.name]).map(SmolStr::to_string).collect();
			options.exclude.clear();

			return Some(Widening {
				options,
				members: vec![member.name.clone()],
			});
		}

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
		Some(Widening { options, members })
	}
}
