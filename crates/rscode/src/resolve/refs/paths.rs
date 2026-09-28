//! Resolving the paths of walked code, and reporting the segments that name targets.
//!
//! Paths start in the local scopes (see [`super::scope`]), at `Self`, or in the module (resolved by the
//! [`Resolver`](crate::resolve::Resolver), cached per module and path). Paths continuing from a local import or from
//! `Self` are resolved one member at a time: bindings of modules, variants of enums, and associated items of types and
//! traits. Members of types follow the rules of `Type::name` for paths the resolver resolves too: variants shadow
//! associated items, items of inherent `impl`s shadow items of trait `impl`s, and without either, the provided items of
//! the implemented traits are found.

use super::ReferenceKind;
use super::scope::LocalBinding;
use super::walker::FileWalker;
use crate::model::DataShape;
use crate::model::ItemData;
use crate::model::ItemDetail;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::PathRef;
use crate::model::PathSegmentRef;
use crate::resolve::Namespace;
use crate::resolve::PathKind;
use crate::resolve::Res;
use proc_macro2::Ident;
use smol_str::SmolStr;
use syn::GenericArgument;
use syn::PathArguments;
use syn::QSelf;
use syn::Type;
use syn::ext::IdentExt;
use syn::visit::Visit;

/// Kinds of items `Self` and the self type of an `impl` can name.
const TYPE_KINDS: &[ItemKind] = &[
	ItemKind::Struct,
	ItemKind::Enum,
	ItemKind::Union,
	ItemKind::TypeAlias,
	ItemKind::ForeignType,
	ItemKind::Trait,
];

/// Kinds of items that can have `impl` blocks of their own (not traits).
const DATA_TYPE_KINDS: &[ItemKind] = &[ItemKind::Struct, ItemKind::Enum, ItemKind::Union, ItemKind::TypeAlias, ItemKind::ForeignType];

/// How much of the local scopes a path sees.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum Locals {
	/// Local variables, generic parameters, and the items of blocks (paths in code).
	All,

	/// The items of blocks only (identifier patterns, which bind a new variable unless they name an item).
	Items,

	/// Nothing (doc links).
	None,
}

/// What the segments of a path resolve to.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) enum PathRes {
	/// Element `i`: what the path up to segment `i` resolves to.
	Segments(Vec<Vec<Res>>),

	/// The path starts with a generic parameter (`T::Assoc`), so the rest depends on the parameter's bounds.
	Generic,

	/// The path starts with a local variable or with an item defined in a block.
	Local,
}

impl PathRes {
	/// What the whole path resolves to.
	pub(super) fn last(&self) -> &[Res] {
		match self {
			Self::Segments(segments) => segments.last().map_or(&[], Vec::as_slice),
			Self::Generic | Self::Local => &[],
		}
	}
}

/// The key of a cached path resolution: the module, the namespace of the last segment, the kind of path, and the path.
pub(super) type PathKey = (ItemId, Option<Namespace>, PathKind, String);

impl FileWalker<'_, '_> {
	/// A path segment for an identifier: its name without `r#`, and its range with it.
	pub(super) fn segment(&self, ident: &Ident) -> PathSegmentRef {
		PathSegmentRef {
			name: ident_name(ident),
			range: self.parsed.range(ident.span()),
			has_arguments: false,
		}
	}

	pub(super) fn path_ref(&self, path: &syn::Path) -> PathRef {
		PathRef {
			leading_colon: path.leading_colon.is_some(),
			segments: (path.segments.iter())
				.map(|segment| PathSegmentRef {
					has_arguments: !segment.arguments.is_none(),
					..self.segment(&segment.ident)
				})
				.collect(),
		}
	}

	/// Whether a path could contain a reference: a segment or an associated item binding is named like a target.
	pub(super) fn mentions_target(&self, path: &syn::Path) -> bool {
		(path.segments.iter()).any(|segment| self.targets.named(&segment.ident).is_some() || self.bindings_mention_target(&segment.arguments))
	}

	fn bindings_mention_target(&self, arguments: &PathArguments) -> bool {
		let PathArguments::AngleBracketed(arguments) = arguments else {
			return false;
		};

		(arguments.args.iter()).any(|argument| binding_ident(argument).is_some_and(|ident| self.targets.named(ident).is_some()))
	}

	/// Resolves a path written at the current point, whose last segment is in `namespace`.
	pub(super) fn resolve_path(&mut self, path: &PathRef, namespace: Namespace, locals: Locals) -> PathRes {
		let Some(first) = path.segments.first() else {
			return PathRes::Segments(Vec::new());
		};

		if !path.leading_colon {
			match first.name.as_str() {
				"Self" => return PathRes::Segments(self.self_path(path, namespace)),

				// relative to a module that is not loaded
				"self" | "super" if self.unloaded_modules > 0 => return PathRes::Local,

				"crate" | "self" | "super" | "$crate" => {}

				name => {
					let first_namespace = if path.segments.len() == 1 { namespace } else { Namespace::Type };

					match self.lookup_local(name, first_namespace, locals) {
						Some(LocalBinding::Variable | LocalBinding::Opaque) => return PathRes::Local,

						Some(LocalBinding::Generic) => {
							return match self.generic_path(name, path, namespace) {
								Some(segments) => PathRes::Segments(segments),
								None => PathRes::Generic,
							};
						}

						Some(LocalBinding::Imported(first)) => return PathRes::Segments(self.continue_path(first, path, namespace)),
						None => {}
					}
				}
			}
		}

		PathRes::Segments(self.module_path(self.module, path, Some(namespace), PathKind::Code))
	}

	/// The resolutions of the segments of a path through the generic parameter `name` (`T::item`), when the traits
	/// the parameter is bounded by have the item: it can only be theirs, as an item that several bounds have would be
	/// ambiguous. `None` when they do not (it may be an item of a supertrait, or of a trait that is not loaded).
	fn generic_path(&mut self, name: &str, path: &PathRef, namespace: Namespace) -> Option<Vec<Vec<Res>>> {
		let member = path.segments.get(1)?;
		let bounds: Vec<PathRef> = self.scopes.bounds_of(name).into_iter().cloned().collect();
		let mut traits: Vec<Res> = Vec::new();

		for bound in &bounds {
			// not through generic parameters (a bound like `T: T::X` would never end)
			let res = self.resolve_path(bound, Namespace::Type, Locals::Items);

			traits.extend(res.last().iter().filter(|res| matches!(res, Res::Item(item) if self.ws.item(*item).kind == ItemKind::Trait)).cloned());
		}

		let wanted = if path.segments.len() == 2 { namespace } else { Namespace::Type };
		let mut found: Vec<Res> = traits.iter().flat_map(|bound| self.members(bound, &member.name, wanted)).collect();

		found.sort();
		found.dedup();

		if found.is_empty() {
			return None;
		}

		// the parameter itself is never a target
		let rest = PathRef { leading_colon: false, segments: path.segments[1..].to_vec() };
		let mut segments = vec![Vec::new()];

		segments.extend(self.continue_path(found, &rest, namespace));
		Some(segments)
	}

	/// The local binding of a name, if any.
	pub(super) fn lookup_local(&self, name: &str, namespace: Namespace, locals: Locals) -> Option<LocalBinding> {
		if locals == Locals::None {
			return None;
		}

		self.scopes.lookup(name, namespace, locals == Locals::Items, |source, name, namespace| self.members(source, name, namespace))
	}

	/// Resolves a path written in `module` (not considering local scopes), with a cache.
	pub(super) fn module_path(&mut self, module: ItemId, path: &PathRef, namespace: Option<Namespace>, kind: PathKind) -> Vec<Vec<Res>> {
		let key = (module, namespace, kind, path.to_string());

		if let Some(found) = self.cache.get(&key) {
			return found.clone();
		}

		let mut found = self.resolver.resolve_prefixes(module, path, namespace, kind);

		if kind == PathKind::Code {
			for (index, segment) in path.segments.iter().enumerate().take(found.len()).skip(1) {
				let wanted = if index + 1 == found.len() { namespace } else { Some(Namespace::Type) };
				let (containers, members) = found.split_at_mut(index);

				self.refine_members(&containers[index - 1], &mut members[0], &segment.name, wanted);
			}
		}

		self.cache.insert(key, found.clone());
		found
	}

	/// Applies the rules of `Type::name` to what the resolver found as members of types: items of inherent `impl`s
	/// shadow the items of trait `impl`s, and without either, the provided items of the traits the types implement are
	/// found.
	fn refine_members(&self, containers: &[Res], found: &mut Vec<Res>, name: &str, wanted: Option<Namespace>) {
		let types = self.loaded(containers, DATA_TYPE_KINDS);

		if types.is_empty() {
			return;
		}

		let is_item = |res: &Res, test: &dyn Fn(ItemId) -> bool| matches!(*res, Res::Item(item) if test(item));

		if found.iter().any(|res| is_item(res, &|item| is_inherent_item(self.ws, item))) {
			found.retain(|res| !is_item(res, &|item| is_trait_impl_item(self.ws, item)));
		}

		let is_member = |item: ItemId| self.ws.item(item).kind.is_associated() || self.ws.item(item).kind == ItemKind::Variant;

		if found.iter().any(|res| is_item(res, &is_member)) {
			return;
		}

		let namespaces = match &wanted {
			Some(namespace) => std::slice::from_ref(namespace),
			None => &Namespace::ALL[..],
		};

		for ty in types {
			for &namespace in namespaces {
				found.extend(self.provided(ty, name, namespace).into_iter().map(Res::Item));
			}
		}

		found.sort();
		found.dedup();
	}

	/// A path starting with `Self`.
	fn self_path(&self, path: &PathRef, namespace: Namespace) -> Vec<Vec<Res>> {
		let first: Vec<Res> = match self.self_types.last() {
			// `Self` alone names types; `Self::item` may also name an item of the implemented (or defined) trait
			Some(self_types) if path.segments.len() == 1 => self_types.types.iter().map(|&item| Res::Item(item)).collect(),
			Some(self_types) => self_types.types.iter().chain(&self_types.traits).map(|&item| Res::Item(item)).collect(),
			None => Vec::new(),
		};

		self.continue_path(first, path, namespace)
	}

	/// The resolutions of the segments of a path whose first segment resolves to `first`.
	pub(super) fn continue_path(&self, first: Vec<Res>, path: &PathRef, namespace: Namespace) -> Vec<Vec<Res>> {
		let count = path.segments.len();
		let mut segments = Vec::with_capacity(count);

		segments.push(first);

		for index in 1..count {
			let wanted = if index + 1 == count { namespace } else { Namespace::Type };
			let name = &path.segments[index].name;
			let mut next: Vec<Res> = segments[index - 1].iter().flat_map(|container| self.members(container, name, wanted)).collect();

			next.sort();
			next.dedup();
			segments.push(next);
		}

		segments
	}

	/// What `container::name` names in `namespace`: a module's binding, a variant of an enum, or an associated item of a
	/// type or trait.
	pub(super) fn members(&self, container: &Res, name: &str, namespace: Namespace) -> Vec<Res> {
		let Res::Item(item) = *container else {
			return Vec::new();
		};

		let members = match self.ws.item(item).kind {
			ItemKind::Module => return self.resolver.bindings(item, name, namespace).into_iter().map(|binding| binding.res).collect(),

			// variants shadow associated items
			ItemKind::Enum => {
				let variants: Vec<ItemId> = (self.ws.children(item))
					.filter(|&variant| {
						let data = self.ws.item(variant);

						data.kind == ItemKind::Variant && data.name.as_deref() == Some(name) && item_namespaces(data).contains(&namespace)
					})
					.collect();

				match variants.is_empty() {
					true => self.associated(item, name, namespace),
					false => variants,
				}
			}

			ItemKind::Struct | ItemKind::Union | ItemKind::TypeAlias | ItemKind::ForeignType | ItemKind::Trait => {
				self.associated(item, name, namespace)
			}

			_ => Vec::new(),
		};

		members.into_iter().map(Res::Item).collect()
	}

	/// The associated items named `name` of a type (from its `impl`s) or trait, in `namespace`. Items of inherent `impl`s
	/// shadow the items of trait `impl`s (like `Type::name` does).
	pub(super) fn associated(&self, owner: ItemId, name: &str, namespace: Namespace) -> Vec<ItemId> {
		let mut items: Vec<ItemId> = (self.resolver.associated_items(owner).into_iter())
			.filter(|&item| {
				let data = self.ws.item(item);

				data.name.as_deref() == Some(name) && item_namespaces(data).contains(&namespace)
			})
			.collect();

		if items.iter().any(|&item| is_inherent_item(self.ws, item)) {
			items.retain(|&item| is_inherent_item(self.ws, item));
		}

		if items.is_empty() && DATA_TYPE_KINDS.contains(&self.ws.item(owner).kind) {
			items = self.provided(owner, name, namespace);
		}

		items
	}

	/// The items named `name` of the traits a type implements (in `namespace`), which `Type::name` names when the
	/// `impl` does not define them (provided methods, constants, and types).
	fn provided(&self, ty: ItemId, name: &str, namespace: Namespace) -> Vec<ItemId> {
		let mut items: Vec<ItemId> = (self.resolver.impls_of(ty).into_iter())
			.filter(|&impl_block| self.named_children(impl_block, name, namespace).next().is_none())
			.flat_map(|impl_block| self.resolver.impl_traits(impl_block))
			.flat_map(|trait_item| self.named_children(trait_item, name, namespace).collect::<Vec<_>>())
			.collect();

		items.sort();
		items.dedup();
		items
	}

	/// Reports the segments of a resolved path that name targets (by their own name, so not through aliases).
	pub(super) fn report_path(&mut self, path: &PathRef, res: &PathRes, kind: ReferenceKind) {
		let targets = self.targets;

		match res {
			PathRes::Segments(segments) => {
				for (segment, resolutions) in path.segments.iter().zip(segments) {
					if let Some(target) = targets.named_str(&segment.name).and_then(|target| target.find(resolutions)) {
						self.report(target, kind, segment.range, true);
					}
				}
			}

			// `T::item`: an item of a trait `T` might be bounded by
			PathRes::Generic if kind == ReferenceKind::Path && self.options.method_calls => {
				let target = path.segments.get(1).and_then(|segment| Some((segment, targets.named_str(&segment.name)?.trait_items.first()?)));

				if let Some((segment, &target)) = target {
					self.report(target, kind, segment.range, false);
				}
			}

			PathRes::Generic | PathRes::Local => {}
		}
	}

	/// Resolves and reports a path in code whose last segment is in `namespace`, and walks its generic arguments.
	pub(super) fn code_path(&mut self, qself: Option<&QSelf>, path: &syn::Path, namespace: Namespace) {
		if let Some(qself) = qself {
			self.visit_type(&qself.ty);
		}

		if self.mentions_target(path) {
			match qself {
				Some(qself) => self.qualified_path(qself, path, namespace),

				None => {
					let path_ref = self.path_ref(path);
					let res = self.resolve_path(&path_ref, namespace, Locals::All);

					self.report_path(&path_ref, &res, ReferenceKind::Path);
					self.check_capture(&path_ref, &res, namespace);
					self.assoc_bindings(path, &res);
				}
			}
		}

		for segment in &path.segments {
			self.visit_path_arguments(&segment.arguments);
		}
	}

	/// A single identifier in a generic argument (`Foo<N>`), which is a type unless it only resolves to a constant.
	pub(super) fn type_or_const_argument(&mut self, ident: &Ident) {
		let path = PathRef {
			leading_colon: false,
			segments: vec![self.segment(ident)],
		};

		let mut res = self.resolve_path(&path, Namespace::Type, Locals::All);

		if res.last().is_empty() && matches!(res, PathRes::Segments(_)) {
			res = self.resolve_path(&path, Namespace::Value, Locals::All);
		}

		self.report_path(&path, &res, ReferenceKind::Path);
		self.check_capture(&path, &res, Namespace::Type);
	}

	/// `<Type as Trait>::item` (the `Trait` segments are the first `qself.position` segments of `path`) or `<Type>::item`.
	/// Segments after the first member depend on types, so they are not resolved.
	fn qualified_path(&mut self, qself: &QSelf, path: &syn::Path, namespace: Namespace) {
		let position = qself.position.min(path.segments.len());
		let mut traits = Vec::new();

		if position > 0 {
			let mut trait_path = self.path_ref(path);

			trait_path.segments.truncate(position);

			let res = self.resolve_path(&trait_path, Namespace::Type, Locals::All);

			self.report_path(&trait_path, &res, ReferenceKind::Path);
			self.check_capture(&trait_path, &res, Namespace::Type);
			self.assoc_bindings(path, &res);
			traits = self.loaded(res.last(), &[ItemKind::Trait]);
		}

		let Some(member) = path.segments.get(position) else {
			return;
		};

		let targets = self.targets;

		let Some(target) = targets.named(&member.ident) else {
			return;
		};

		let wanted = if position + 1 == path.segments.len() { namespace } else { Namespace::Type };
		let name = ident_name(&member.ident);
		let types = self.type_items(&qself.ty);
		let mut candidates: Vec<Res> = Vec::new();

		if position == 0 {
			for &ty in &types {
				candidates.extend(self.members(&Res::Item(ty), &name, wanted));
			}
		} else {
			for &trait_item in &traits {
				candidates.extend(self.associated(trait_item, &name, wanted).into_iter().map(Res::Item));

				// the item of the type's `impl` of the trait
				for &ty in &types {
					for impl_block in self.resolver.impls_of(ty) {
						if self.resolver.impl_traits(impl_block).contains(&trait_item) {
							candidates.extend(self.named_children(impl_block, &name, wanted).map(Res::Item));
						}
					}
				}
			}
		}

		if let Some(found) = target.find(&candidates) {
			self.report(found, ReferenceKind::Path, self.parsed.range(member.ident.span()), true);
		}
	}

	/// The children of an `impl` block or trait named `name`, in `namespace`.
	pub(super) fn named_children(&self, container: ItemId, name: &str, namespace: Namespace) -> impl Iterator<Item = ItemId> {
		self.ws.children(container).filter(move |&child| {
			let data = self.ws.item(child);

			data.name.as_deref() == Some(name) && item_namespaces(data).contains(&namespace)
		})
	}

	/// The loaded items among resolutions, of the given kinds.
	pub(super) fn loaded(&self, resolutions: &[Res], kinds: &[ItemKind]) -> Vec<ItemId> {
		(resolutions.iter())
			.filter_map(|res| match res {
				Res::Item(item) if kinds.contains(&self.ws.item(*item).kind) => Some(*item),
				_ => None,
			})
			.collect()
	}

	/// The loaded types (or traits) a type names, after stripping references, pointers, parentheses, slices, and arrays.
	pub(super) fn type_items(&mut self, ty: &Type) -> Vec<ItemId> {
		let mut ty = ty;

		loop {
			ty = match ty {
				Type::Reference(reference) => &reference.elem,
				Type::Ptr(pointer) => &pointer.elem,
				Type::Paren(paren) => &paren.elem,
				Type::Group(group) => &group.elem,
				Type::Slice(slice) => &slice.elem,
				Type::Array(array) => &array.elem,
				_ => break,
			};
		}

		let Type::Path(path) = ty else {
			return Vec::new();
		};

		if path.qself.is_some() {
			return Vec::new();
		}

		let path = self.path_ref(&path.path);
		let res = self.resolve_path(&path, Namespace::Type, Locals::All);

		self.loaded(res.last(), TYPE_KINDS)
	}

	/// Reports associated item bindings of traits (`Iterator<Item = T>`, `Trait<N = 1>`, `Trait<Assoc: Bound>`) that name
	/// target items.
	fn assoc_bindings(&mut self, path: &syn::Path, res: &PathRes) {
		let PathRes::Segments(segments) = res else {
			return;
		};

		let targets = self.targets;

		for (segment, resolutions) in path.segments.iter().zip(segments) {
			let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
				continue;
			};

			for argument in &arguments.args {
				let Some((ident, target)) = binding_ident(argument).and_then(|ident| Some((ident, targets.named(ident)?))) else {
					continue;
				};

				let kind = if matches!(argument, GenericArgument::AssocConst(_)) { ItemKind::AssocConst } else { ItemKind::AssocType };
				let name = ident_name(ident);

				let candidates: Vec<Res> = (self.loaded(resolutions, &[ItemKind::Trait]).into_iter())
					.flat_map(|trait_item| self.ws.children(trait_item))
					.filter(|&item| {
						let data = self.ws.item(item);

						data.kind == kind && data.name.as_deref() == Some(name.as_str())
					})
					.map(Res::Item)
					.collect();

				if let Some(found) = target.find(&candidates) {
					self.report(found, ReferenceKind::Path, self.parsed.range(ident.span()), true);
				}
			}
		}
	}
}

/// The identifier of an associated item binding in generic arguments.
fn binding_ident(argument: &GenericArgument) -> Option<&Ident> {
	match argument {
		GenericArgument::AssocType(binding) => Some(&binding.ident),
		GenericArgument::AssocConst(binding) => Some(&binding.ident),
		GenericArgument::Constraint(constraint) => Some(&constraint.ident),
		_ => None,
	}
}

/// The name of an identifier, without `r#`.
pub(super) fn ident_name(ident: &Ident) -> SmolStr {
	SmolStr::new(ident.unraw().to_string())
}

/// The namespaces an item is bound in as a member of a module (or of an enum, type, or trait).
pub(super) fn item_namespaces(data: &ItemData) -> &'static [Namespace] {
	const TYPE: &[Namespace] = &[Namespace::Type];
	const VALUE: &[Namespace] = &[Namespace::Value];
	const TYPE_AND_VALUE: &[Namespace] = &[Namespace::Type, Namespace::Value];
	const MACRO: &[Namespace] = &[Namespace::Macro];

	match data.kind {
		ItemKind::Module
		| ItemKind::Enum
		| ItemKind::Union
		| ItemKind::Trait
		| ItemKind::TraitAlias
		| ItemKind::TypeAlias
		| ItemKind::ExternCrate
		| ItemKind::ForeignType
		| ItemKind::AssocType => TYPE,

		// tuple and unit structs and variants are also constructors
		ItemKind::Struct | ItemKind::Variant => match data.detail {
			ItemDetail::Data {
				shape: DataShape::Tuple | DataShape::Unit,
				..
			} => TYPE_AND_VALUE,
			_ => TYPE,
		},

		ItemKind::Fn
		| ItemKind::Const
		| ItemKind::Static
		| ItemKind::ForeignFn
		| ItemKind::ForeignStatic
		| ItemKind::AssocFn
		| ItemKind::AssocConst => VALUE,

		ItemKind::MacroRules => MACRO,

		ItemKind::MacroCall
		| ItemKind::Use
		| ItemKind::Import
		| ItemKind::ExternBlock
		| ItemKind::ForeignMacro
		| ItemKind::Impl
		| ItemKind::AssocMacro => &[],
	}
}

/// Whether an item is an item of an inherent `impl` block.
fn is_inherent_item(ws: &crate::model::Workspace, item: ItemId) -> bool {
	(ws.parent(item).map(|parent| ws.item(parent)))
		.and_then(ItemData::impl_info)
		.is_some_and(|info| info.trait_path.is_none())
}

/// Whether an item is an item of a trait `impl` block.
fn is_trait_impl_item(ws: &crate::model::Workspace, item: ItemId) -> bool {
	(ws.parent(item).map(|parent| ws.item(parent)))
		.and_then(ItemData::impl_info)
		.is_some_and(|info| info.trait_path.is_some())
}

/// Whether a resolution can be bound in a namespace (what things outside of the loaded crates are is unknown).
pub(super) fn res_in_namespace(ws: &crate::model::Workspace, res: &Res, namespace: Namespace) -> bool {
	match res {
		Res::Item(item) => item_namespaces(ws.item(*item)).contains(&namespace),
		Res::External(_) | Res::Builtin(_) => true,
	}
}

/// Whether an identifier pattern naming this item is a path pattern rather than a new binding: constants, statics, and
/// unit structs and variants.
pub(super) fn is_pattern_item(data: &ItemData) -> bool {
	match data.kind {
		ItemKind::Const | ItemKind::Static | ItemKind::ForeignStatic | ItemKind::AssocConst => true,
		ItemKind::Struct | ItemKind::Variant => matches!(
			data.detail,
			ItemDetail::Data {
				shape: DataShape::Unit,
				..
			}
		),
		_ => false,
	}
}
