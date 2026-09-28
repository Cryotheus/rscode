//! Extracting items from syntax trees.

use super::Container;
use super::ModDir;
use super::Walker;
use super::syntax::ident_name;
use super::thread_local;
use super::verbatim;
use super::verbatim::Shape;
use crate::CfgExpr;
use crate::Tristate;
use crate::model::DataShape;
use crate::model::FnInfo;
use crate::model::ImplInfo;
use crate::model::ImportInfo;
use crate::model::ItemAttrs;
use crate::model::ItemData;
use crate::model::ItemDetail;
use crate::model::ItemKind;
use crate::model::ModuleInfo;
use crate::model::PathRef;
use crate::model::PathSegmentRef;
use crate::model::Visibility;
use crate::source::TextRange;
use proc_macro2::Ident;
use proc_macro2::TokenStream;
use proc_macro2::extra::DelimSpan;
use quote::ToTokens;
use syn::Attribute;
use syn::ForeignItem;
use syn::ImplItem;
use syn::Item;
use syn::ItemForeignMod;
use syn::ItemImpl;
use syn::ItemMod;
use syn::ItemTrait;
use syn::ItemTraitAlias;
use syn::Macro;
use syn::StaticMutability;
use syn::Token;
use syn::TraitItem;
use syn::UseTree;
use syn::Variant;

/// A tree of a `use` item that may start with `::`: the whole tree of the item, or an element of a group that syn
/// does not model (see [`verbatim::UseElement`]).
pub(super) struct UseRoot<'a> {
	/// Whether the tree starts with `::`.
	pub(super) leading_colon: bool,

	pub(super) tree: &'a UseTree,

	/// The range of the tree, including its `::`.
	pub(super) range: TextRange,
}

impl Walker<'_, '_, '_> {
	/// Starts an item from what every item has: its tokens, attributes, and visibility.
	fn new_item(&mut self, kind: ItemKind, source: &dyn ToTokens, attrs: &[Attribute], vis: Visibility) -> ItemData {
		let outer = attrs.iter().filter(|attr| matches!(attr.style, syn::AttrStyle::Outer)).count();
		let extent = self.extent(source, outer);
		let summary = self.attributes(attrs, kind == ItemKind::Module);

		ItemData {
			kind,
			name: None,
			parent: None,
			children: Vec::new(),
			file: self.file,
			range: extent.range,
			name_range: None,
			vis,
			cfg: CfgExpr::all(summary.cfgs),
			attrs: ItemAttrs {
				after_attrs: extent.after_attrs,
				..summary.attrs
			},
			detail: ItemDetail::None,
		}
	}

	/// Adds an item named by `ident` (unless it is `_`), returning its index.
	fn push_named(&mut self, parent: u32, mut item: ItemData, ident: Option<&Ident>) -> u32 {
		if let Some(ident) = ident.filter(|ident| *ident != "_") {
			item.name = Some(ident_name(ident));
			item.name_range = Some(self.range(ident.span()));
		}

		self.loader.push(parent, item)
	}

	pub(super) fn module_items(&mut self, module: u32, items: &[Item], dir: &ModDir, active: Tristate) {
		for item in items {
			self.module_item(module, item, dir, active);
		}
	}

	/// Adds an item of a module. `active`: whether the module is active.
	fn module_item(&mut self, parent: u32, item: &Item, dir: &ModDir, active: Tristate) {
		let private = Visibility::Private;

		match item {
			Item::Const(item) => {
				let vis = self.visibility(&item.vis, private);
				let data = self.new_item(ItemKind::Const, item, &item.attrs, vis);

				self.push_named(parent, data, Some(&item.ident));
			}

			Item::Enum(item) => {
				let vis = self.visibility(&item.vis, private);
				let mut data = self.new_item(ItemKind::Enum, item, &item.attrs, vis);

				data.detail = ItemDetail::Data {
					shape: DataShape::Named,
					body: Some(self.inside(&item.brace_token.span)),
				};

				let index = self.push_named(parent, data, Some(&item.ident));

				for variant in &item.variants {
					self.variant(index, variant);
				}
			}

			Item::ExternCrate(item) => {
				let vis = self.visibility(&item.vis, private);
				let mut data = self.new_item(ItemKind::ExternCrate, item, &item.attrs, vis);
				let alias = item.rename.as_ref().map(|(_, alias)| alias);

				data.detail = ItemDetail::ExternCrate {
					crate_name: ident_name(&item.ident),
					alias: alias.map(ident_name),
				};

				self.push_named(parent, data, Some(alias.unwrap_or(&item.ident)));
			}

			Item::Fn(item) => {
				let vis = self.visibility(&item.vis, private);
				let mut data = self.new_item(ItemKind::Fn, item, &item.attrs, vis);

				data.detail = ItemDetail::Fn(self.fn_info(&item.sig, Some(&item.block)));
				self.push_named(parent, data, Some(&item.sig.ident));
			}

			Item::ForeignMod(item) => self.extern_block(parent, item),
			Item::Impl(item) => self.impl_block(parent, item, item),
			Item::Macro(item) => self.macro_item(parent, item, &item.attrs, item.ident.as_ref(), &item.mac, Container::Module),
			Item::Mod(item) => self.module(parent, item, dir, active),

			Item::Static(item) => {
				let vis = self.visibility(&item.vis, private);
				let mut data = self.new_item(ItemKind::Static, item, &item.attrs, vis);

				data.detail = ItemDetail::Static {
					mutable: matches!(item.mutability, StaticMutability::Mut(_)),
					thread_local: false,
				};

				self.push_named(parent, data, Some(&item.ident));
			}

			Item::Struct(item) => {
				let vis = self.visibility(&item.vis, private);
				let mut data = self.new_item(ItemKind::Struct, item, &item.attrs, vis);

				data.detail = self.fields_detail(&item.fields);
				self.push_named(parent, data, Some(&item.ident));
			}

			Item::Trait(item) => self.trait_def(parent, item, item),
			Item::TraitAlias(item) => self.trait_alias(parent, item, item),

			Item::Type(item) => {
				let vis = self.visibility(&item.vis, private);
				let data = self.new_item(ItemKind::TypeAlias, item, &item.attrs, vis);

				self.push_named(parent, data, Some(&item.ident));
			}

			Item::Union(item) => {
				let vis = self.visibility(&item.vis, private);
				let mut data = self.new_item(ItemKind::Union, item, &item.attrs, vis);

				data.detail = ItemDetail::Data {
					shape: DataShape::Named,
					body: Some(self.inside(&item.fields.brace_token.span)),
				};

				self.push_named(parent, data, Some(&item.ident));
			}

			Item::Use(item) => {
				let vis = self.visibility(&item.vis, private);

				let root = UseRoot {
					leading_colon: item.leading_colon.is_some(),
					tree: &item.tree,
					range: self.rooted_range(item.leading_colon.as_ref(), &item.tree),
				};

				self.use_item(parent, item, &item.attrs, vis, &[root]);
			}

			Item::Verbatim(tokens) => self.verbatim(parent, tokens, Container::Module),

			// syntax added to syn after this was written
			item => self.verbatim(parent, &item.to_token_stream(), Container::Module),
		}
	}

	fn variant(&mut self, parent: u32, variant: &Variant) {
		let mut data = self.new_item(ItemKind::Variant, variant, &variant.attrs, Visibility::Inherited);

		data.detail = self.fields_detail(&variant.fields);
		self.push_named(parent, data, Some(&variant.ident));
	}

	fn module(&mut self, parent: u32, item: &ItemMod, dir: &ModDir, active: Tristate) {
		let vis = self.visibility(&item.vis, Visibility::Private);
		let mut data = self.new_item(ItemKind::Module, item, &item.attrs, vis);
		let name = ident_name(&item.ident);
		let active = active.and(self.loader.eval(data.cfg.as_ref()));
		let path_attr = data.attrs.path.clone();

		match &item.content {
			Some((brace, items)) => {
				data.detail = ItemDetail::Module(ModuleInfo {
					inline: true,
					body: Some(self.inside(&brace.span)),
					..ModuleInfo::default()
				});

				let index = self.push_named(parent, data, Some(&item.ident));

				self.module_items(index, items, &dir.inline(&name, path_attr.as_deref()), active);
			}

			None => {
				let file = dir.resolve(&name, path_attr.as_deref());
				let declaration = self.range(item.ident.span()).start;

				data.detail = ItemDetail::Module(ModuleInfo {
					inline: false,
					body: None,
					file: None,
					file_path: Some(file.path.clone()),
					dir_owner: file.dir.relative.is_none(),
					load_error: None,
				});

				let index = self.push_named(parent, data, Some(&item.ident));

				self.load_out_of_line(index, &name, file, declaration, active);
			}
		}
	}

	/// An `impl` block; `source` spans its tokens (the item itself, or the `Verbatim` it was re-parsed from).
	fn impl_block(&mut self, parent: u32, source: &dyn ToTokens, item: &ItemImpl) {
		let mut data = self.new_item(ItemKind::Impl, source, &item.attrs, Visibility::Private);
		let trait_path = item.trait_.as_ref().map(|(path, _)| path);

		data.detail = ItemDetail::Impl(ImplInfo {
			self_ty: self.type_ref(&item.self_ty),
			self_ty_text: self.compact_text(item.self_ty.to_token_stream()),
			trait_path: trait_path.map(|path| self.path_ref(path)),
			trait_text: trait_path.map(|path| self.compact_text(path.to_token_stream())),
			negative: item.modifiers.polarity.is_some(),
			is_unsafe: item.unsafety.is_some(),
			body: self.inside(&item.brace_token.span),
		});

		let index = self.loader.push(parent, data);
		let container = Container::Impl {
			of_trait: trait_path.is_some(),
		};

		for member in &item.items {
			self.impl_member(index, member, container);
		}
	}

	fn impl_member(&mut self, parent: u32, item: &ImplItem, container: Container) {
		let default = container.default_vis();

		match item {
			ImplItem::Const(item) => {
				let vis = self.visibility(&item.vis, default);
				let data = self.new_item(ItemKind::AssocConst, item, &item.attrs, vis);

				self.push_named(parent, data, Some(&item.ident));
			}

			ImplItem::Fn(item) => {
				let vis = self.visibility(&item.vis, default);
				let mut data = self.new_item(ItemKind::AssocFn, item, &item.attrs, vis);

				data.detail = ItemDetail::Fn(self.fn_info(&item.sig, Some(&item.block)));
				self.push_named(parent, data, Some(&item.sig.ident));
			}

			ImplItem::Type(item) => {
				let vis = self.visibility(&item.vis, default);
				let data = self.new_item(ItemKind::AssocType, item, &item.attrs, vis);

				self.push_named(parent, data, Some(&item.ident));
			}

			ImplItem::Macro(item) => self.macro_item(parent, item, &item.attrs, None, &item.mac, container),
			ImplItem::Verbatim(tokens) => self.verbatim(parent, tokens, container),
			item => self.verbatim(parent, &item.to_token_stream(), container),
		}
	}

	/// A trait; `source` spans its tokens (the item itself, or the `Verbatim` it was re-parsed from).
	fn trait_def(&mut self, parent: u32, source: &dyn ToTokens, item: &ItemTrait) {
		let vis = self.visibility(&item.vis, Visibility::Private);
		let mut data = self.new_item(ItemKind::Trait, source, &item.attrs, vis);

		data.detail = ItemDetail::Trait {
			is_unsafe: item.unsafety.is_some(),
			is_auto: item.modifiers.auto_token.is_some(),
			body: Some(self.inside(&item.brace_token.span)),
		};

		let index = self.push_named(parent, data, Some(&item.ident));

		for member in &item.items {
			self.trait_member(index, member);
		}
	}

	fn trait_alias(&mut self, parent: u32, source: &dyn ToTokens, item: &ItemTraitAlias) {
		let vis = self.visibility(&item.vis, Visibility::Private);
		let mut data = self.new_item(ItemKind::TraitAlias, source, &item.attrs, vis);

		data.detail = ItemDetail::Trait {
			is_unsafe: false,
			is_auto: false,
			body: None,
		};

		self.push_named(parent, data, Some(&item.ident));
	}

	fn trait_member(&mut self, parent: u32, item: &TraitItem) {
		let vis = Visibility::Inherited;

		match item {
			TraitItem::Const(item) => {
				let data = self.new_item(ItemKind::AssocConst, item, &item.attrs, vis);

				self.push_named(parent, data, Some(&item.ident));
			}

			TraitItem::Fn(item) => {
				let mut data = self.new_item(ItemKind::AssocFn, item, &item.attrs, vis);

				data.detail = ItemDetail::Fn(self.fn_info(&item.sig, item.default.as_ref()));
				self.push_named(parent, data, Some(&item.sig.ident));
			}

			TraitItem::Type(item) => {
				let data = self.new_item(ItemKind::AssocType, item, &item.attrs, vis);

				self.push_named(parent, data, Some(&item.ident));
			}

			TraitItem::Macro(item) => self.macro_item(parent, item, &item.attrs, None, &item.mac, Container::Trait),
			TraitItem::Verbatim(tokens) => self.verbatim(parent, tokens, Container::Trait),
			item => self.verbatim(parent, &item.to_token_stream(), Container::Trait),
		}
	}

	fn extern_block(&mut self, parent: u32, item: &ItemForeignMod) {
		let mut data = self.new_item(ItemKind::ExternBlock, item, &item.attrs, Visibility::Private);

		data.detail = ItemDetail::ExternBlock {
			abi: item.abi.name.as_ref().map(syn::LitStr::value),
			is_unsafe: item.unsafety.is_some(),
			body: self.inside(&item.brace_token.span),
		};

		let index = self.loader.push(parent, data);

		for member in &item.items {
			self.foreign_item(index, member);
		}
	}

	fn foreign_item(&mut self, parent: u32, item: &ForeignItem) {
		let private = Visibility::Private;

		match item {
			ForeignItem::Fn(item) => {
				let vis = self.visibility(&item.vis, private);
				let mut data = self.new_item(ItemKind::ForeignFn, item, &item.attrs, vis);

				data.detail = ItemDetail::Fn(self.fn_info(&item.sig, None));
				self.push_named(parent, data, Some(&item.sig.ident));
			}

			ForeignItem::Static(item) => {
				let vis = self.visibility(&item.vis, private);
				let mut data = self.new_item(ItemKind::ForeignStatic, item, &item.attrs, vis);

				data.detail = ItemDetail::Static {
					mutable: matches!(item.mutability, StaticMutability::Mut(_)),
					thread_local: false,
				};

				self.push_named(parent, data, Some(&item.ident));
			}

			ForeignItem::Type(item) => {
				let vis = self.visibility(&item.vis, private);
				let data = self.new_item(ItemKind::ForeignType, item, &item.attrs, vis);

				self.push_named(parent, data, Some(&item.ident));
			}

			ForeignItem::Macro(item) => self.macro_item(parent, item, &item.attrs, None, &item.mac, Container::Extern),
			ForeignItem::Verbatim(tokens) => self.verbatim(parent, tokens, Container::Extern),
			item => self.verbatim(parent, &item.to_token_stream(), Container::Extern),
		}
	}

	/// A `macro_rules!` definition (named by `ident`) or another macro invocation.
	fn macro_item(
		&mut self,
		parent: u32,
		source: &dyn ToTokens,
		attrs: &[Attribute],
		ident: Option<&Ident>,
		mac: &Macro,
		container: Container,
	) {
		let rules = container == Container::Module && ident.is_some() && mac.path.is_ident("macro_rules");
		let kind = if rules { ItemKind::MacroRules } else { container.macro_kind() };
		let mut data = self.new_item(kind, source, attrs, container.default_vis());

		data.detail = ItemDetail::Macro {
			path: self.compact_text(mac.path.to_token_stream()),
			body: self.inside(mac.delimiter.span()),
		};

		let index = self.push_named(parent, data, ident.filter(|_| rules));

		// the statics a `thread_local!` declares live in the module, like the items of `extern` blocks
		if container == Container::Module
			&& !rules
			&& let Some(declarations) = thread_local::declarations(mac)
		{
			for declaration in &declarations {
				let vis = self.visibility(&declaration.vis, Visibility::Private);
				let mut data = self.new_item(ItemKind::Static, declaration, &declaration.attrs, vis);

				data.detail = ItemDetail::Static { mutable: false, thread_local: true };
				self.push_named(index, data, Some(&declaration.ident));
			}
		}
	}

	/// A `use` item and its leaves.
	fn use_item(&mut self, parent: u32, source: &dyn ToTokens, attrs: &[Attribute], vis: Visibility, roots: &[UseRoot]) {
		let data = self.new_item(ItemKind::Use, source, attrs, vis.clone());
		let index = self.loader.push(parent, data);
		let mut prefix = Vec::new();

		for root in roots {
			let leaf = Leaf {
				use_item: index,
				vis: &vis,
				leading_colon: root.leading_colon,
			};

			self.use_tree(&leaf, root.tree, root.range, &mut prefix);
		}
	}

	/// The range of a tree of a `use` item, including its leading `::`.
	fn rooted_range(&self, leading_colon: Option<&Token![::]>, tree: &UseTree) -> TextRange {
		let range = self.range_of(tree);

		leading_colon.map_or(range, |colon| self.range_of(colon).cover(range))
	}

	/// Adds an [`ItemKind::Import`] for every leaf of a `use` tree.
	///
	/// `element`: the range of the element of the innermost group that contains `tree` (or of the whole tree of the
	/// `use` item), which is the range of the tree's leaves outside nested groups (see [`ItemData::range`]).
	fn use_tree(&mut self, leaf: &Leaf, tree: &UseTree, element: TextRange, prefix: &mut Vec<PathSegmentRef>) {
		match tree {
			UseTree::Path(path) => {
				prefix.push(self.segment(&path.ident));
				self.use_tree(leaf, &path.tree, element, prefix);
				prefix.pop();
			}

			UseTree::Name(name) => self.import(leaf, element, prefix, Some(&name.ident), None),
			UseTree::Rename(rename) => self.import(leaf, element, prefix, Some(&rename.ident), Some(&rename.rename)),
			UseTree::Glob(_) => self.import(leaf, element, prefix, None, None),

			UseTree::Group(group) => {
				for tree in &group.items {
					self.use_tree(leaf, tree, self.range_of(tree), prefix);
				}
			}
		}
	}

	/// An import of `prefix::ident [as alias]`, or of `prefix::*` if `ident` is `None`.
	fn import(&mut self, leaf: &Leaf, range: TextRange, prefix: &[PathSegmentRef], ident: Option<&Ident>, alias: Option<&Ident>) {
		let is_self = ident.is_some_and(|ident| *ident == "self");
		let mut path = PathRef {
			leading_colon: leaf.leading_colon,
			segments: prefix.to_vec(),
		};

		if let Some(ident) = ident.filter(|_| !is_self) {
			path.segments.push(self.segment(ident));
		}

		let info = ImportInfo {
			path,
			alias: alias.map(ident_name),
			alias_range: alias.map(|alias| self.range(alias.span())),
			glob: ident.is_none(),
			is_self,
		};

		let name = info.binding_name().cloned();
		let name_range = name.as_ref().and(alias.or(ident)).map(|ident| self.range(ident.span()));

		let item = ItemData {
			kind: ItemKind::Import,
			name,
			parent: None,
			children: Vec::new(),
			file: self.file,
			range,
			name_range,
			vis: leaf.vis.clone(),
			cfg: None,
			attrs: ItemAttrs {
				after_attrs: range.start,
				..ItemAttrs::default()
			},
			detail: ItemDetail::Import(info),
		};

		self.loader.push(leaf.use_item, item);
	}

	/// Syntax syn does not model, classified by its tokens (see [`verbatim`]).
	fn verbatim(&mut self, parent: u32, tokens: &TokenStream, container: Container) {
		let parts = verbatim::Parts::split(tokens);
		let vis = self.visibility(&parts.vis, container.default_vis());
		let end = self.range_of(tokens).end;
		let unknown = |this: &Self, body| (container.macro_kind(), None, this.macro_detail("", body, end));

		let (kind, name, detail) = match verbatim::classify(&parts, container) {
			Shape::Trait(item) => return self.trait_def(parent, tokens, &item),
			Shape::TraitAlias(item) => return self.trait_alias(parent, tokens, &item),
			Shape::Impl(item) => return self.impl_block(parent, tokens, &item),
			Shape::Use(elements) => return self.verbatim_use(parent, tokens, &parts.attrs, vis, &elements),

			Shape::Fn {
				name,
				receiver,
				is_const,
				is_async,
				is_unsafe,
				body,
			} => {
				let info = FnInfo {
					receiver,
					is_const,
					is_async,
					is_unsafe,
					body: body.map(|span| self.range(span)),
				};

				(container.fn_kind(), Some(name), ItemDetail::Fn(info))
			}

			Shape::Const { name } => match container.const_kind() {
				Some(kind) => (kind, Some(name), ItemDetail::None),
				None => unknown(self, None),
			},

			Shape::Static { name, mutable } => match container.static_kind() {
				Some(kind) => (kind, Some(name), ItemDetail::Static { mutable, thread_local: false }),
				None => unknown(self, None),
			},

			Shape::Type { name } => (container.type_kind(), Some(name), ItemDetail::None),
			Shape::Macro { name, body } if container == Container::Module => {
				(ItemKind::MacroRules, Some(name), self.macro_detail("macro", body, end))
			}

			Shape::Macro { body, .. } | Shape::Unknown { body } => unknown(self, body),
		};

		let mut data = self.new_item(kind, tokens, &parts.attrs, vis);

		data.detail = detail;
		self.push_named(parent, data, name.as_ref());
	}

	/// Details of a macro-like item; without a delimited `body`, the body is empty and at the item's `end`.
	fn macro_detail(&self, path: &str, body: Option<DelimSpan>, end: usize) -> ItemDetail {
		ItemDetail::Macro {
			path: path.to_owned(),
			body: body.map_or(TextRange::new(end, end), |body| self.inside(&body)),
		}
	}

	/// A `use` item with `::` at the start of group elements (`use {::a, b};`, `use {a, {::b}};`).
	fn verbatim_use(&mut self, parent: u32, tokens: &TokenStream, attrs: &[Attribute], vis: Visibility, elements: &[verbatim::UseElement]) {
		let roots: Vec<UseRoot> = elements
			.iter()
			.map(|element| UseRoot {
				leading_colon: element.leading_colon.is_some(),
				tree: &element.tree,
				range: self.rooted_range(element.leading_colon.as_ref(), &element.tree),
			})
			.collect();

		self.use_item(parent, tokens, attrs, vis, &roots);
	}
}

/// What the leaves of a `use` item share.
struct Leaf<'a> {
	use_item: u32,
	vis: &'a Visibility,
	leading_colon: bool,
}
