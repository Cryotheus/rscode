//! A small `syn`-based model builder for resolver tests, independent of [`crate::load`].
//!
//! It fills [`ItemData`] the way the loader does (visibility defaults, import leaves, `impl` details, module files)
//! for the constructs name resolution cares about, from in-memory sources or files on disk.

use crate::CfgContext;
use crate::CfgExpr;
use crate::model::Crate;
use crate::model::CrateId;
use crate::model::CrateSpec;
use crate::model::DataShape;
use crate::model::Dependency;
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
use crate::model::TargetKind;
use crate::model::TypeRef;
use crate::model::Visibility;
use crate::model::Workspace;
use crate::source::FileId;
use crate::source::ParsedFile;
use crate::source::SourceFile;
use crate::source::TextRange;
use proc_macro2::TokenTree;
use rscode_fmt::Edition;
use smol_str::SmolStr;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use syn::ext::IdentExt;

/// A crate to build: a root source plus other module files, in memory or on disk.
#[derive(Debug, Clone)]
pub(crate) struct TestCrate {
	name: SmolStr,
	edition: Edition,
	kind: TargetKind,
	selected: bool,
	dependencies: Vec<Dependency>,
	root: PathBuf,
	files: BTreeMap<PathBuf, String>,
	disk: bool,
}

impl TestCrate {
	/// A library crate whose root file (`/test/<name>/src/lib.rs`) contains `source`.
	pub(crate) fn new(name: &str, source: &str) -> Self {
		let root = PathBuf::from(format!("/test/{name}/src/lib.rs"));

		Self {
			name: name.into(),
			edition: Edition::E2021,
			kind: TargetKind::Lib,
			selected: true,
			dependencies: Vec::new(),
			files: BTreeMap::from([(root.clone(), source.to_owned())]),
			root,
			disk: false,
		}
	}

	/// A crate loaded from files on disk.
	pub(crate) fn on_disk(name: &str, root: impl Into<PathBuf>) -> Self {
		Self {
			root: root.into(),
			files: BTreeMap::new(),
			disk: true,
			..Self::new(name, "")
		}
	}

	pub(crate) fn edition(mut self, edition: Edition) -> Self {
		self.edition = edition;
		self
	}

	pub(crate) fn kind(mut self, kind: TargetKind) -> Self {
		self.kind = kind;
		self
	}

	pub(crate) fn unselected(mut self) -> Self {
		self.selected = false;
		self
	}

	/// A dependency known as `name` in the extern prelude, on the crate named `crate_name`.
	pub(crate) fn dep(mut self, name: &str, crate_name: &str) -> Self {
		self.dependencies.push(Dependency {
			name: name.into(),
			crate_name: crate_name.into(),
			package: None,
			krate: None,
		});

		self
	}

	/// Another source file, at a path relative to the directory of the root file.
	pub(crate) fn file(mut self, path: &str, source: &str) -> Self {
		let path = self.root.parent().unwrap_or(Path::new("/")).join(path);

		self.files.insert(path, source.to_owned());
		self
	}

	fn read(&self, path: &Path) -> Option<String> {
		match self.files.get(path) {
			Some(text) => Some(text.clone()),
			None if self.disk => std::fs::read_to_string(path).ok(),
			None => None,
		}
	}
}

/// Builds a linked workspace of the crates.
pub(crate) fn workspace(crates: impl IntoIterator<Item = TestCrate>) -> Workspace {
	let mut ws = Workspace::new("/test");

	for test in crates {
		let id = CrateId(ws.crates.len() as u32);

		ws.crates.push(Builder::load(id, &test));
	}

	ws.link();
	ws
}

/// Where `mod name;` declarations of the items being walked find their files.
#[derive(Debug, Clone)]
struct Dirs {
	file: FileId,

	/// The directory of the current file.
	file_dir: PathBuf,

	/// The directory child module files are looked up in.
	base: PathBuf,

	/// Whether the items are inside of an inline module.
	inline: bool,
}

struct Builder<'a> {
	test: &'a TestCrate,
	files: Vec<SourceFile>,
	items: Vec<ItemData>,
}

impl Builder<'_> {
	fn load(id: CrateId, test: &TestCrate) -> Crate {
		let mut builder = Builder {
			test,
			files: Vec::new(),
			items: Vec::new(),
		};

		let text = test.read(&test.root).unwrap_or_default();
		let file = builder.add_file(test.root.clone(), text);
		let len = builder.files[0].text().len();

		builder.items.push(ItemData {
			kind: ItemKind::Module,
			name: Some(test.name.clone()),
			parent: None,
			children: Vec::new(),
			file,
			range: TextRange::new(0, len),
			name_range: None,
			vis: Visibility::Public,
			cfg: None,
			attrs: ItemAttrs::default(),
			detail: ItemDetail::Module(ModuleInfo {
				inline: false,
				file: Some(file),
				dir_owner: true,
				..ModuleInfo::default()
			}),
		});

		builder.load_file(0, file, true);

		let mut spec = CrateSpec::new(test.name.clone(), test.root.clone());

		spec.kind = test.kind;
		spec.edition = test.edition;
		spec.cfg = CfgContext::new();
		spec.dependencies = test.dependencies.clone();
		spec.selected = test.selected;

		Crate {
			id,
			spec,
			files: builder.files,
			items: builder.items,
			diagnostics: Vec::new(),
		}
	}

	fn add_file(&mut self, path: PathBuf, text: String) -> FileId {
		self.files.push(SourceFile::new(path, text));

		FileId(self.files.len() as u32 - 1)
	}

	fn push(&mut self, parent: u32, mut data: ItemData) -> u32 {
		let index = self.items.len() as u32;

		data.parent = Some(parent);
		self.items.push(data);
		self.items[parent as usize].children.push(index);
		index
	}

	fn load_file(&mut self, module: u32, file: FileId, dir_owner: bool) {
		let source = self.files[file.index()].clone();

		let Ok(parsed) = ParsedFile::parse(source.text()) else {
			return;
		};

		let file_dir = source.path().parent().map(Path::to_path_buf).unwrap_or_default();
		let stem = source.path().file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();

		let dirs = Dirs {
			file,
			base: if dir_owner { file_dir.clone() } else { file_dir.join(stem) },
			file_dir,
			inline: false,
		};

		for item in &parsed.file.items {
			self.item(module, &parsed, item, &dirs);
		}
	}

	fn item(&mut self, parent: u32, parsed: &ParsedFile<'_>, item: &syn::Item, dirs: &Dirs) {
		let range = parsed.range_of(item);
		let private = |item_vis: &syn::Visibility| vis(parsed, item_vis, Visibility::Private);
		let braced = |brace: &syn::token::Brace| Some(inner(parsed.range(brace.span.join())));
		let no_attrs = Vec::new();

		let (kind, ident, attrs, item_vis, detail) = match item {
			syn::Item::Mod(module) => return self.module(parent, parsed, module, dirs),
			syn::Item::Use(item) => return self.use_item(parent, parsed, item, dirs),
			syn::Item::Impl(item) => return self.impl_item(parent, parsed, item, dirs),
			syn::Item::Verbatim(tokens) => return self.verbatim(parent, parsed, tokens, range, dirs),
			syn::Item::Struct(item) => (ItemKind::Struct, Some(&item.ident), &item.attrs, private(&item.vis), data_detail(parsed, &item.fields)),

			syn::Item::Union(item) => {
				let detail = ItemDetail::Data {
					shape: DataShape::Named,
					body: braced(&item.fields.brace_token),
				};

				(ItemKind::Union, Some(&item.ident), &item.attrs, private(&item.vis), detail)
			}

			syn::Item::Enum(item) => {
				let detail = ItemDetail::Data {
					shape: DataShape::Named,
					body: braced(&item.brace_token),
				};

				(ItemKind::Enum, Some(&item.ident), &item.attrs, private(&item.vis), detail)
			}

			syn::Item::Trait(item) => {
				let detail = ItemDetail::Trait {
					is_unsafe: item.unsafety.is_some(),
					is_auto: item.modifiers.auto_token.is_some(),
					body: braced(&item.brace_token),
				};

				(ItemKind::Trait, Some(&item.ident), &item.attrs, private(&item.vis), detail)
			}

			syn::Item::TraitAlias(item) => {
				let detail = ItemDetail::Trait {
					is_unsafe: false,
					is_auto: false,
					body: None,
				};

				(ItemKind::TraitAlias, Some(&item.ident), &item.attrs, private(&item.vis), detail)
			}

			syn::Item::Type(item) => (ItemKind::TypeAlias, Some(&item.ident), &item.attrs, private(&item.vis), ItemDetail::None),
			syn::Item::Fn(item) => (ItemKind::Fn, Some(&item.sig.ident), &item.attrs, private(&item.vis), ItemDetail::Fn(FnInfo::default())),

			syn::Item::Const(item) => {
				let ident = Some(&item.ident).filter(|ident| *ident != "_");

				(ItemKind::Const, ident, &item.attrs, private(&item.vis), ItemDetail::None)
			}

			syn::Item::Static(item) => {
				let detail = ItemDetail::Static {
					thread_local: false,
					mutable: matches!(item.mutability, syn::StaticMutability::Mut(_)),
				};

				(ItemKind::Static, Some(&item.ident), &item.attrs, private(&item.vis), detail)
			}

			syn::Item::Macro(item) if item.ident.is_some() && item.mac.path.is_ident("macro_rules") => {
				(ItemKind::MacroRules, item.ident.as_ref(), &item.attrs, Visibility::Private, macro_detail("macro_rules"))
			}

			syn::Item::Macro(item) => {
				let path = collapse(parsed.slice(parsed.range_of(&item.mac.path)));

				(ItemKind::MacroCall, None, &item.attrs, Visibility::Private, macro_detail(&path))
			}

			syn::Item::ExternCrate(item) => {
				let detail = ItemDetail::ExternCrate {
					crate_name: SmolStr::new(item.ident.unraw().to_string()),
					alias: item.rename.as_ref().map(|(_, rename)| SmolStr::new(rename.unraw().to_string())),
				};

				let ident = item.rename.as_ref().map_or(&item.ident, |(_, rename)| rename);

				(ItemKind::ExternCrate, Some(ident), &item.attrs, private(&item.vis), detail)
			}

			syn::Item::ForeignMod(item) => {
				let detail = ItemDetail::ExternBlock {
					abi: item.abi.name.as_ref().map(syn::LitStr::value),
					is_unsafe: item.unsafety.is_some(),
					body: inner(parsed.range(item.brace_token.span.join())),
				};

				(ItemKind::ExternBlock, None, &item.attrs, Visibility::Private, detail)
			}

			_ => (ItemKind::MacroCall, None, &no_attrs, Visibility::Private, macro_detail("")),
		};

		let index = self.push(parent, data(parsed, dirs, kind, ident, attrs, item_vis, range, detail));

		match item {
			syn::Item::Enum(item) => {
				for variant in &item.variants {
					let range = parsed.range_of(variant);
					let detail = data_detail(parsed, &variant.fields);
					let data = data(parsed, dirs, ItemKind::Variant, Some(&variant.ident), &variant.attrs, Visibility::Inherited, range, detail);

					self.push(index, data);
				}
			}

			syn::Item::Trait(item) => {
				for trait_item in &item.items {
					self.trait_item(index, parsed, trait_item, dirs);
				}
			}

			syn::Item::ForeignMod(item) => {
				for foreign in &item.items {
					self.foreign_item(index, parsed, foreign, dirs);
				}
			}

			_ => {}
		}
	}

	fn module(&mut self, parent: u32, parsed: &ParsedFile<'_>, module: &syn::ItemMod, dirs: &Dirs) {
		let range = parsed.range_of(module);
		let name = module.ident.unraw().to_string();
		let module_vis = vis(parsed, &module.vis, Visibility::Private);
		let item = data(parsed, dirs, ItemKind::Module, Some(&module.ident), &module.attrs, module_vis, range, ItemDetail::None);
		let path_attr = item.attrs.path.clone();
		let index = self.push(parent, item);

		let relative = |path: &str| {
			if dirs.inline {
				dirs.base.join(path)
			} else {
				dirs.file_dir.join(path)
			}
		};

		let mut info = ModuleInfo {
			inline: module.content.is_some(),
			..ModuleInfo::default()
		};

		if let Some((brace, items)) = &module.content {
			info.body = Some(inner(parsed.range(brace.span.join())));
			self.items[index as usize].detail = ItemDetail::Module(info);

			let inner_dirs = Dirs {
				base: path_attr.as_deref().map_or_else(|| dirs.base.join(&name), relative),
				inline: true,
				..dirs.clone()
			};

			for item in items {
				self.item(index, parsed, item, &inner_dirs);
			}

			return;
		}

		let (path, dir_owner) = match &path_attr {
			Some(path) => (relative(path), true),

			None => {
				let flat = dirs.base.join(format!("{name}.rs"));

				match self.test.read(&flat) {
					Some(_) => (flat, false),
					None => (dirs.base.join(&name).join("mod.rs"), true),
				}
			}
		};

		info.file_path = Some(path.clone());
		info.dir_owner = dir_owner;

		let already_loaded = self.files.iter().any(|file| file.path() == path);

		match self.test.read(&path).filter(|_| !already_loaded) {
			Some(text) => {
				let file = self.add_file(path, text);

				info.file = Some(file);
				self.items[index as usize].detail = ItemDetail::Module(info);
				self.load_file(index, file, dir_owner);
			}

			None => {
				info.load_error = Some("file not found".to_owned());
				self.items[index as usize].detail = ItemDetail::Module(info);
			}
		}
	}

	fn use_item(&mut self, parent: u32, parsed: &ParsedFile<'_>, item: &syn::ItemUse, dirs: &Dirs) {
		let range = parsed.range_of(item);
		let use_vis = vis(parsed, &item.vis, Visibility::Private);
		let index = self.push(parent, data(parsed, dirs, ItemKind::Use, None, &item.attrs, use_vis.clone(), range, ItemDetail::None));
		let mut leaves = Vec::new();

		flatten_use(parsed, &item.tree, &mut Vec::new(), &mut leaves);

		for (mut info, range, name_range) in leaves {
			info.path.leading_colon = item.leading_colon.is_some();

			self.push(index, ItemData {
				kind: ItemKind::Import,
				name: info.binding_name().cloned(),
				parent: None,
				children: Vec::new(),
				file: dirs.file,
				range,
				name_range: Some(name_range),
				vis: use_vis.clone(),
				cfg: None,
				attrs: ItemAttrs::default(),
				detail: ItemDetail::Import(info),
			});
		}
	}

	fn impl_item(&mut self, parent: u32, parsed: &ParsedFile<'_>, item: &syn::ItemImpl, dirs: &Dirs) {
		let range = parsed.range_of(item);
		let trait_path = item.trait_.as_ref().map(|(path, _)| path);

		let info = ImplInfo {
			self_ty: type_ref(parsed, &item.self_ty),
			self_ty_text: collapse(parsed.slice(parsed.range_of(&*item.self_ty))),
			trait_path: trait_path.map(|path| path_ref(parsed, path)),
			trait_text: trait_path.map(|path| collapse(parsed.slice(parsed.range_of(path)))),
			negative: item.modifiers.polarity.is_some(),
			is_unsafe: item.unsafety.is_some(),
			body: inner(parsed.range(item.brace_token.span.join())),
		};

		let mut impl_data = data(parsed, dirs, ItemKind::Impl, None, &item.attrs, Visibility::Private, range, ItemDetail::Impl(info));

		impl_data.attrs.after_attrs = [item.modifiers.defaultness.map(|token| token.span), item.unsafety.map(|token| token.span)]
			.into_iter()
			.flatten()
			.chain([item.impl_token.span])
			.map(|span| parsed.range(span).start)
			.min()
			.unwrap_or(range.start);

		let index = self.push(parent, impl_data);
		let default_vis = if trait_path.is_some() { Visibility::Inherited } else { Visibility::Private };

		for impl_item in &item.items {
			let range = parsed.range_of(impl_item);

			let (kind, ident, attrs, item_vis) = match impl_item {
				syn::ImplItem::Fn(item) => (ItemKind::AssocFn, Some(&item.sig.ident), &item.attrs, Some(&item.vis)),
				syn::ImplItem::Const(item) => (ItemKind::AssocConst, Some(&item.ident), &item.attrs, Some(&item.vis)),
				syn::ImplItem::Type(item) => (ItemKind::AssocType, Some(&item.ident), &item.attrs, Some(&item.vis)),
				syn::ImplItem::Macro(item) => (ItemKind::AssocMacro, None, &item.attrs, None),
				_ => continue,
			};

			let item_vis = item_vis.map_or(default_vis.clone(), |item_vis| vis(parsed, item_vis, default_vis.clone()));
			let detail = if kind == ItemKind::AssocFn { ItemDetail::Fn(FnInfo::default()) } else { ItemDetail::None };

			self.push(index, data(parsed, dirs, kind, ident, attrs, item_vis, range, detail));
		}
	}

	fn trait_item(&mut self, parent: u32, parsed: &ParsedFile<'_>, item: &syn::TraitItem, dirs: &Dirs) {
		let range = parsed.range_of(item);

		let (kind, ident, attrs) = match item {
			syn::TraitItem::Fn(item) => (ItemKind::AssocFn, Some(&item.sig.ident), &item.attrs),
			syn::TraitItem::Const(item) => (ItemKind::AssocConst, Some(&item.ident), &item.attrs),
			syn::TraitItem::Type(item) => (ItemKind::AssocType, Some(&item.ident), &item.attrs),
			syn::TraitItem::Macro(item) => (ItemKind::AssocMacro, None, &item.attrs),
			_ => return,
		};

		let detail = if kind == ItemKind::AssocFn { ItemDetail::Fn(FnInfo::default()) } else { ItemDetail::None };

		self.push(parent, data(parsed, dirs, kind, ident, attrs, Visibility::Inherited, range, detail));
	}

	fn foreign_item(&mut self, parent: u32, parsed: &ParsedFile<'_>, item: &syn::ForeignItem, dirs: &Dirs) {
		let range = parsed.range_of(item);

		let (kind, ident, attrs, item_vis) = match item {
			syn::ForeignItem::Fn(item) => (ItemKind::ForeignFn, Some(&item.sig.ident), &item.attrs, Some(&item.vis)),
			syn::ForeignItem::Static(item) => (ItemKind::ForeignStatic, Some(&item.ident), &item.attrs, Some(&item.vis)),
			syn::ForeignItem::Type(item) => (ItemKind::ForeignType, Some(&item.ident), &item.attrs, Some(&item.vis)),
			syn::ForeignItem::Macro(item) => (ItemKind::ForeignMacro, None, &item.attrs, None),
			_ => return,
		};

		let item_vis = item_vis.map_or(Visibility::Private, |item_vis| vis(parsed, item_vis, Visibility::Private));

		self.push(parent, data(parsed, dirs, kind, ident, attrs, item_vis, range, ItemDetail::None));
	}

	/// Items syn does not model: `macro name(..) {..}` (decl macros 2.0) and `fn f();` are classified; anything else
	/// becomes a macro call.
	fn verbatim(&mut self, parent: u32, parsed: &ParsedFile<'_>, tokens: &proc_macro2::TokenStream, range: TextRange, dirs: &Dirs) {
		let tokens: Vec<TokenTree> = tokens.clone().into_iter().collect();
		let mut index = 0;
		let mut item_vis = Visibility::Private;

		// skip outer attributes
		while matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '#') {
			index += 2;
		}

		if matches!(tokens.get(index), Some(TokenTree::Ident(ident)) if ident == "pub") {
			item_vis = Visibility::Public;
			index += 1;

			if matches!(tokens.get(index), Some(TokenTree::Group(_))) {
				item_vis = Visibility::Crate;
				index += 1;
			}
		}

		let keyword = match tokens.get(index) {
			Some(TokenTree::Ident(ident)) => ident.to_string(),
			_ => String::new(),
		};

		let (kind, detail) = match keyword.as_str() {
			"macro" => (ItemKind::MacroRules, macro_detail("macro")),
			"fn" => (ItemKind::Fn, ItemDetail::Fn(FnInfo::default())),
			_ => (ItemKind::MacroCall, macro_detail("")),
		};

		let ident = match (kind, tokens.get(index + 1)) {
			(ItemKind::MacroCall, _) => None,
			(_, Some(TokenTree::Ident(ident))) => Some(ident.clone()),
			_ => None,
		};

		self.push(parent, data(parsed, dirs, kind, ident.as_ref(), &[], item_vis, range, detail));
	}
}

#[expect(clippy::too_many_arguments)]
fn data(
	parsed: &ParsedFile<'_>,
	dirs: &Dirs,
	kind: ItemKind,
	ident: Option<&syn::Ident>,
	attrs: &[syn::Attribute],
	vis: Visibility,
	range: TextRange,
	detail: ItemDetail,
) -> ItemData {
	let outer: Vec<&syn::Attribute> = attrs.iter().filter(|attr| matches!(attr.style, syn::AttrStyle::Outer)).collect();

	ItemData {
		kind,
		name: ident.map(|ident| SmolStr::new(ident.unraw().to_string())),
		parent: None,
		children: Vec::new(),
		file: dirs.file,
		range,
		name_range: ident.map(|ident| parsed.range(ident.span())),
		vis,
		cfg: CfgExpr::all(outer.iter().filter_map(|attr| cfg_of(attr))),
		attrs: ItemAttrs {
			macro_export: outer.iter().any(|attr| attr.path().is_ident("macro_export")),
			macro_use: outer.iter().any(|attr| attr.path().is_ident("macro_use")),
			path: outer.iter().find_map(|attr| path_attr(attr)),
			after_attrs: outer.iter().map(|attr| parsed.range_of(*attr).end).max().unwrap_or(range.start),
			..ItemAttrs::default()
		},
		detail,
	}
}

/// Details of a macro (`macro_rules!`, a decl macro 2.0 for `macro`, or an invocation).
fn macro_detail(path: &str) -> ItemDetail {
	ItemDetail::Macro {
		path: path.to_owned(),
		body: TextRange::default(),
	}
}

fn cfg_of(attr: &syn::Attribute) -> Option<CfgExpr> {
	match &attr.meta {
		syn::Meta::List(list) if list.path.is_ident("cfg") => {
			Some(CfgExpr::from_tokens(list.tokens.clone()).unwrap_or_else(|_| CfgExpr::Other(list.tokens.to_string())))
		}

		_ => None,
	}
}

fn path_attr(attr: &syn::Attribute) -> Option<String> {
	match &attr.meta {
		syn::Meta::NameValue(name_value) if name_value.path.is_ident("path") => match &name_value.value {
			syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(value), .. }) => Some(value.value()),
			_ => None,
		},

		_ => None,
	}
}

fn vis(parsed: &ParsedFile<'_>, vis: &syn::Visibility, default: Visibility) -> Visibility {
	match vis {
		syn::Visibility::Public(_) => Visibility::Public,
		syn::Visibility::Inherited => default,
		syn::Visibility::Restricted(restricted) if restricted.in_token.is_some() => Visibility::InPath(path_ref(parsed, &restricted.path)),
		syn::Visibility::Restricted(restricted) if restricted.path.is_ident("crate") => Visibility::Crate,
		syn::Visibility::Restricted(restricted) if restricted.path.is_ident("super") => Visibility::Super,
		syn::Visibility::Restricted(restricted) if restricted.path.is_ident("self") => Visibility::SelfModule,
		syn::Visibility::Restricted(restricted) => Visibility::InPath(path_ref(parsed, &restricted.path)),
	}
}

fn segment(parsed: &ParsedFile<'_>, ident: &syn::Ident, has_arguments: bool) -> PathSegmentRef {
	PathSegmentRef {
		name: ident.unraw().to_string().into(),
		range: parsed.range(ident.span()),
		has_arguments,
	}
}

fn path_ref(parsed: &ParsedFile<'_>, path: &syn::Path) -> PathRef {
	PathRef {
		leading_colon: path.leading_colon.is_some(),
		segments: path.segments.iter().map(|segment_| segment(parsed, &segment_.ident, !segment_.arguments.is_none())).collect(),
	}
}

fn type_ref(parsed: &ParsedFile<'_>, ty: &syn::Type) -> TypeRef {
	let indirect = |inner: &syn::Type| TypeRef::Indirect {
		inner: Box::new(type_ref(parsed, inner)),
	};

	match ty {
		syn::Type::Path(path) if path.qself.is_none() => TypeRef::Path {
			path: path_ref(parsed, &path.path),
		},

		syn::Type::Reference(reference) => indirect(&reference.elem),
		syn::Type::Ptr(pointer) => indirect(&pointer.elem),
		syn::Type::Paren(paren) => indirect(&paren.elem),
		syn::Type::Group(group) => indirect(&group.elem),
		syn::Type::Slice(slice) => indirect(&slice.elem),
		syn::Type::Array(array) => indirect(&array.elem),
		_ => TypeRef::Other,
	}
}

fn data_detail(parsed: &ParsedFile<'_>, fields: &syn::Fields) -> ItemDetail {
	let (shape, body) = match fields {
		syn::Fields::Named(named) => (DataShape::Named, Some(inner(parsed.range(named.brace_token.span.join())))),
		syn::Fields::Unnamed(unnamed) => (DataShape::Tuple, Some(inner(parsed.range(unnamed.paren_token.span.join())))),
		syn::Fields::Unit => (DataShape::Unit, None),
	};

	ItemDetail::Data { shape, body }
}

/// The range inside of a delimited group's range.
fn inner(range: TextRange) -> TextRange {
	TextRange::new((range.start + 1).min(range.end), range.end.saturating_sub(1).max(range.start))
}

/// Source text with whitespace runs collapsed, and no spaces around `<`, `>`, and `::`.
fn collapse(text: &str) -> String {
	let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");

	["<", ">", "::"].iter().fold(collapsed, |text, token| text.replace(&format!(" {token}"), token).replace(&format!("{token} "), token))
}

/// A leaf of a use tree: its details, its range, and the range of the ident introducing the binding.
type Leaf = (ImportInfo, TextRange, TextRange);

/// Flattens a use tree into its leaves.
fn flatten_use(parsed: &ParsedFile<'_>, tree: &syn::UseTree, prefix: &mut Vec<PathSegmentRef>, out: &mut Vec<Leaf>) {
	let ident_range = |ident: &syn::Ident| parsed.range(ident.span());

	let leaf = |segments: Vec<PathSegmentRef>, alias: Option<&syn::Ident>, glob: bool, is_self: bool| ImportInfo {
		path: PathRef {
			leading_colon: false,
			segments,
		},
		alias: alias.map(|alias| SmolStr::new(alias.unraw().to_string())),
		alias_range: alias.map(ident_range),
		glob,
		is_self,
	};

	let range = parsed.range_of(tree);

	match tree {
		syn::UseTree::Path(path) => {
			prefix.push(segment(parsed, &path.ident, false));
			flatten_use(parsed, &path.tree, prefix, out);
			prefix.pop();
		}

		syn::UseTree::Name(name) if name.ident == "self" => out.push((leaf(prefix.clone(), None, false, true), range, ident_range(&name.ident))),

		syn::UseTree::Name(name) => {
			let mut segments = prefix.clone();

			segments.push(segment(parsed, &name.ident, false));
			out.push((leaf(segments, None, false, false), range, ident_range(&name.ident)));
		}

		syn::UseTree::Rename(rename) => {
			let is_self = rename.ident == "self";
			let mut segments = prefix.clone();

			if !is_self {
				segments.push(segment(parsed, &rename.ident, false));
			}

			out.push((leaf(segments, Some(&rename.rename), false, is_self), range, ident_range(&rename.rename)));
		}

		syn::UseTree::Glob(glob) => out.push((leaf(prefix.clone(), None, true, false), range, parsed.range(glob.star_token.span))),

		syn::UseTree::Group(group) => {
			for tree in &group.items {
				flatten_use(parsed, tree, prefix, out);
			}
		}
	}
}
