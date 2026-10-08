//! Canonical (definition) paths of loaded items.

use super::Resolver;
use super::names::is_macro_rules;
use super::vis::home_module;
use super::vis::parent_module;
use crate::model::ImportInfo;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::path::CanonicalPath;
use crate::path::written_arguments;
use smol_str::SmolStr;

impl Resolver<'_> {
	pub(super) fn compute_canonical_path(&self, item: ItemId) -> CanonicalPath {
		let data = self.ws.item(item);

		let mut path = CanonicalPath {
			segments: Vec::new(),
			impl_trait: None,
			self_ty_arguments: None,
			unresolved_self_ty: None,
			is_impl: false,
			is_import: false,
			name: data.name.clone(),
			selector: None,
			is_field: false,
			is_macro_call: false,
		};

		let Some(parent) = self.ws.parent(item) else {
			path.name = Some(self.ws.krate(item.krate()).name().clone());
			return path;
		};

		match (data.kind, self.ws.item(parent).kind) {
			(ItemKind::Field, _) => {
				path.segments = self.flat_path(parent);
				path.is_field = true;
			}

			(ItemKind::Impl, _) => {
				path = self.impl_base(item);
				path.selector = self.impl_selector(item, &path);
			}

			// (items of inherent `impl`s are named by plain paths, which selectors do not apply to)
			(_, ItemKind::Impl) => {
				self.set_impl_owner(parent, &mut path);

				if path.impl_trait.is_some() {
					path.selector = self.compute_canonical_path(parent).selector;
				}
			}
			(_, ItemKind::Trait | ItemKind::Enum) => path.segments = self.flat_path(parent),

			(ItemKind::Import, _) => {
				path.segments = self.module_segments(home_module(self.ws, item));
				path.name = data.import_info().map(ImportInfo::path_name);
				path.is_import = true;
			}

			// invocations of a macro are named by its name and a `!` (and an index, when the path names others too)
			(ItemKind::MacroCall, ItemKind::Module) if data.macro_name().is_some() => {
				path.segments = self.module_segments(home_module(self.ws, item));
				path.name = data.macro_name().map(SmolStr::from);
				path.is_macro_call = true;
				path.selector = self.macro_selector(item, &path);
			}

			// `#[macro_export]` macros live at the crate root
			(ItemKind::MacroRules, _) if is_macro_rules(data) && data.attrs.macro_export => {
				path.segments = vec![self.ws.krate(item.krate()).name().clone()];
			}

			_ => path.segments = self.module_segments(home_module(self.ws, item)),
		}

		path
	}

	/// The canonical path of a type or trait as plain segments (including its own name).
	pub(super) fn flat_path(&self, item: ItemId) -> Vec<SmolStr> {
		let path = self.compute_canonical_path(item);
		let mut segments = path.segments;

		segments.extend(path.name);
		segments
	}

	/// The canonical path of an `impl` block without its selector.
	pub(super) fn impl_base(&self, impl_block: ItemId) -> CanonicalPath {
		let mut path = CanonicalPath {
			segments: Vec::new(),
			impl_trait: None,
			self_ty_arguments: None,
			unresolved_self_ty: None,
			is_impl: true,
			is_import: false,
			name: None,
			selector: None,
			is_field: false,
			is_macro_call: false,
		};

		self.set_impl_owner(impl_block, &mut path);
		path
	}

	/// The crate name followed by the names of the modules down to `module`.
	pub(super) fn module_segments(&self, module: ItemId) -> Vec<SmolStr> {
		let mut segments = Vec::new();
		let mut current = Some(module);

		while let Some(module) = current {
			let name = if module.is_crate_root() {
				self.ws.krate(module.krate()).name().clone()
			} else {
				self.ws.item(module).name.clone().unwrap_or_default()
			};

			segments.push(name);
			current = parent_module(self.ws, module);
		}

		segments.reverse();
		segments
	}

	/// Sets the owner of an `impl` block's path: its first resolved self type, or its module and the self type text.
	///
	/// A trait is not the owner of an `impl` for its trait objects (`impl Trait {}` in editions 2015 and 2018), whose
	/// items would otherwise look like the trait's own.
	fn set_impl_owner(&self, impl_block: ItemId, path: &mut CanonicalPath) {
		let info = self.ws.item(impl_block).impl_info();
		let owner = self
			.impls
			.self_types(impl_block)
			.iter()
			.copied()
			.find(|&owner| self.ws.item(owner).kind != ItemKind::Trait);

		path.impl_trait = info.and_then(|info| info.trait_text.clone());

		match owner {
			Some(owner) => {
				path.segments = self.flat_path(owner);
				path.self_ty_arguments = info.and_then(|info| written_arguments(&info.self_ty_text)).map(str::to_owned);
			}

			None => {
				path.segments = self.module_segments(home_module(self.ws, impl_block));
				path.unresolved_self_ty = Some(info.map(|info| info.self_ty_text.clone()).unwrap_or_default());
			}
		}
	}
}
