//! The Cryotheum ordering schema: which group every item belongs to, and how items are ordered within their group.
//!
//! This is the single source of truth for both engines. The `plan_*` functions take the items of one container
//! (with their tie-breaking text) and return the new order, grouped, along with which `extern` blocks merge.
//!
//! Expressions, function bodies, and macro bodies are never sorted. Macro definitions, item-position macro
//! invocations, and `#[macro_use]` items are barriers: they never move, nothing moves across them, and the items
//! between two barriers are sorted on their own.

use crate::StyleEdition;
use crate::imports::UseKey;
use crate::version_cmp;
use proc_macro2::TokenTree;
use quote::ToTokens;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::HashMap;
use syn::ext::IdentExt;

/// rustfmt's order of `extern crate` items: by the crate's name (in bytes), then without a rename first, then by the
/// rename.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct ExternCrateKey {
	name: String,
	rename: Option<String>,
}

impl ExternCrateKey {
	fn new(item: &syn::ItemExternCrate) -> Self {
		Self {
			name: item.ident.unraw().to_string(),
			rename: item.rename.as_ref().map(|(_, rename)| rename.unraw().to_string()),
		}
	}
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ExternKey {
	/// The ABI string: `"C"` in `extern "C" {}`, and in `extern {}` (which rustfmt rewrites to `extern "C" {}`).
	abi: String,

	/// The normalized token text of the outer attributes.
	attrs: String,
}

impl ExternKey {
	fn new(block: &syn::ItemForeignMod) -> Self {
		let attrs = block
			.attrs
			.iter()
			.filter(|attr| matches!(attr.style, syn::AttrStyle::Outer))
			.map(|attr| attr.to_token_stream().to_string())
			.collect::<Vec<_>>()
			.join(" ");

		Self {
			abi: block.abi.name.as_ref().map_or_else(|| "C".to_owned(), syn::LitStr::value),
			attrs,
		}
	}
}

impl Ord for ExternKey {
	fn cmp(&self, other: &Self) -> Ordering {
		version_cmp(&self.abi, &other.abi).then_with(|| version_cmp(&self.attrs, &other.attrs))
	}
}

impl PartialOrd for ExternKey {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

#[derive(Debug, Clone)]
struct ImplKey {
	self_ty: String,

	/// The last segment of the trait's path, and the whole path.
	trait_path: Option<(String, String)>,
}

impl ImplKey {
	fn new(block: &syn::ItemImpl) -> Self {
		let trait_path = block.trait_.as_ref().map(|(path, _)| {
			let last = path.segments.last().map(|segment| segment.ident.unraw().to_string()).unwrap_or_default();

			(last, path.to_token_stream().to_string())
		});

		Self {
			self_ty: block.self_ty.to_token_stream().to_string(),
			trait_path,
		}
	}

	/// Inherent impls by self type, then trait impls by trait name, trait path, and self type.
	fn attached_cmp(&self, other: &Self) -> Ordering {
		match (&self.trait_path, &other.trait_path) {
			(None, None) => version_cmp(&self.self_ty, &other.self_ty),
			(None, Some(_)) => Ordering::Less,
			(Some(_), None) => Ordering::Greater,
			(Some((a_last, a_path)), Some((b_last, b_path))) => version_cmp(a_last, b_last)
				.then_with(|| version_cmp(a_path, b_path))
				.then_with(|| version_cmp(&self.self_ty, &other.self_ty)),
		}
	}

	/// By self type, then inherent first, then by trait path.
	fn loose_cmp(&self, other: &Self) -> Ordering {
		version_cmp(&self.self_ty, &other.self_ty).then_with(|| match (&self.trait_path, &other.trait_path) {
			(None, None) => Ordering::Equal,
			(None, Some(_)) => Ordering::Less,
			(Some(_), None) => Ordering::Greater,
			(Some((_, a)), Some((_, b))) => version_cmp(a, b),
		})
	}
}

/// How items compare within their group.
#[derive(Debug, Clone)]
enum Key {
	/// The original order is kept.
	Stable,

	Name(NameKey),

	/// Byte order, like rustfmt orders `mod foo;` declarations.
	Bytes(String),

	ExternCrate(ExternCrateKey),

	Use(UseKey),

	/// An `impl` block following its data type: inherent impls first.
	AttachedImpl(ImplKey),

	/// An `impl` block of a type defined elsewhere.
	LooseImpl(ImplKey),

	ExternBlock(ExternKey),
}

impl Key {
	fn name(ident: &syn::Ident) -> Self {
		Self::Name(NameKey::new(ident))
	}

	fn compare(&self, other: &Self) -> Ordering {
		match (self, other) {
			(Self::Name(a), Self::Name(b)) => a.cmp(b),
			(Self::Bytes(a), Self::Bytes(b)) => a.cmp(b),
			(Self::ExternCrate(a), Self::ExternCrate(b)) => a.cmp(b),
			(Self::Use(a), Self::Use(b)) => a.cmp(b),
			(Self::AttachedImpl(a), Self::AttachedImpl(b)) => a.attached_cmp(b),
			(Self::LooseImpl(a), Self::LooseImpl(b)) => a.loose_cmp(b),
			(Self::ExternBlock(a), Self::ExternBlock(b)) => a.cmp(b),
			// different kinds of keys never share a group
			_ => Ordering::Equal,
		}
	}
}

/// A name, compared without its leading underscores first, so `_mike` directly follows `mike`.
#[derive(Debug, Clone, Eq, PartialEq)]
struct NameKey {
	trimmed: String,
	underscores: usize,
}

impl NameKey {
	fn new(ident: &syn::Ident) -> Self {
		Self::from_name(&ident.unraw().to_string())
	}

	fn from_name(name: &str) -> Self {
		let trimmed = name.trim_start_matches('_');

		Self {
			trimmed: trimmed.to_owned(),
			underscores: name.len() - trimmed.len(),
		}
	}
}

impl Ord for NameKey {
	fn cmp(&self, other: &Self) -> Ordering {
		version_cmp(&self.trimmed, &other.trimmed).then(self.underscores.cmp(&other.underscores))
	}
}

impl PartialOrd for NameKey {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

/// Items inside an `impl` block or `trait` definition
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum OrderingSchemaAssociated {
	/// `type Foo = Bar;`
	Type,

	/// `const FOO: u8 = 0;`
	Const,

	/// `fn new() -> T`
	New,

	/// `fn _new() -> U`
	NewInternal,

	/// `fn foo(bar: Biz) -> V`
	// Visibility, `const`, `unsafe` tokens don't impact ordering
	Fn,

	/// `fn foo(self, bar: Biz) -> W`
	/// `fn foo(&self, bar: Biz) -> W`
	/// `fn foo(&mut self, bar: Biz) -> W`
	/// `fn foo(self: Box<Self>, bar: Biz) -> W`
	/// `fn foo(self: Arc<Self>, bar: Biz) -> W`
	/// `fn foo(self: Pin<&mut Self>, bar: Biz) -> W`
	// and whatever else
	Method,

	/// `foo!();`
	///
	/// A barrier.
	MacroInvocation,

	/// Syntax `syn` does not model.
	Other,
}

impl OrderingSchemaAssociated {
	fn spacing(self) -> Spacing {
		match self {
			Self::Type | Self::Const => Spacing::Compact,
			Self::MacroInvocation => Spacing::Barrier,
			_ => Spacing::Loose,
		}
	}
}

/// Items inside a `extern "C"` block
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum OrderingSchemaForeign {
	/// `type Foo;`
	Type,
	// Visibility tokens don't impact ordering
	Static,
	// Visibility, `const`, `unsafe` tokens don't impact ordering
	Fn,
	/// A barrier.
	MacroInvocation,
	/// Syntax `syn` does not model.
	Other,
}

impl OrderingSchemaForeign {
	fn spacing(self) -> Spacing {
		match self {
			Self::MacroInvocation => Spacing::Barrier,
			_ => Spacing::Compact,
		}
	}
}

/// Items inside a module.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum OrderingSchemaItem {
	/// `extern crate foo;`
	/// `extern crate foo as bar;`
	ExternCrate,

	/// `macro_rules! foo { ... }`
	/// `#[macro_use] mod foo;`
	/// `#[macro_use] extern crate foo;`
	///
	/// `macro_rules!` macros are textually scoped: they must precede every use, including uses in child modules, and a
	/// redefinition shadows the earlier definition from there on. A barrier.
	MacroScope,

	/// `mod foo;`
	Module(OrderingSchemaModule),

	/// `use foo::Bar;`
	Use,

	/// `pub use foo;`
	/// `pub(crate) use foo;`
	ReExport,

	/// `type Foo = Bar;`
	TypeAlias,

	/// `const FOO: () = ();`
	Const,

	/// `static FOO: () = ();`
	Static,

	/// `static mut FOO: () = ();`
	StaticMut,

	/// Can have some amount of `impl Foo {}` and `impl Trait for Foo {}` blocks following
	/// trait impls are after normal impls
	///
	/// also counts `trait` items too
	/// maybe `DataType` isn't a fitting name
	/// but `impl Trait for Type` goes with the `Type` definition if in the same module
	/// otherwise, it falls back to going below the trait defitinion
	///
	/// `enum Foo { ... }`
	/// `struct Foo;`
	/// `struct Foo(...);`
	/// `struct Foo { ... }`
	/// `union Foo { ... }`
	/// `enum Foo { ... } impl Foo {}`
	/// `struct Foo; impl Foo {}`
	/// `struct Foo(...); impl Foo {}`
	/// `struct Foo { ... } impl Foo {}`
	/// `union Foo { ... } impl Foo {}`
	///
	/// `trait Foo {}`
	DataType,

	/// `impl Foo {}`
	///
	/// When `Foo` is not a type created in the same module item list
	LooseImpl,

	/// `unsafe extern "C" {}`
	ExternBlock,

	/// Loose functions
	/// Visibility, `const`, `unsafe` tokens don't impact ordering
	Fn,

	/// `mod foo {}`
	ModuleInlined(OrderingSchemaModule),

	/// `foo!();`
	/// `foo! { ... }`
	/// `include!("...");`
	///
	/// Invocations may define macros (`cfg_if! { macro_rules! .. }`) or expand to anything. A barrier.
	MacroInvocation,

	/// Anything else, such as syntax `syn` does not model (`fn foo();`, `macro foo() {}`).
	///
	/// These keep their original relative order.
	Other,
}

impl OrderingSchemaItem {
	fn spacing(self) -> Spacing {
		match self {
			Self::MacroScope | Self::MacroInvocation => Spacing::Barrier,
			Self::ExternCrate | Self::Module(_) | Self::Use | Self::ReExport => Spacing::Compact,
			Self::TypeAlias | Self::Const | Self::Static | Self::StaticMut => Spacing::Compact,
			_ => Spacing::Loose,
		}
	}
}

/// `mod` items inside a file or inline-module.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum OrderingSchemaModule {
	/// `mod foo;`
	/// `pub mod foo;`
	Typical,

	/// `#[cfg(target_os = "linux")] mod foo;`
	/// `#[cfg(target_os = "linux")] pub mod foo;`
	Cfg,

	/// `#[cfg(test)] mod tests;`
	Test,
}

/// The new order of the items of a container.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub(crate) struct Plan {
	pub(crate) groups: Vec<PlanGroup>,
}

impl Plan {
	/// The entries of every group, in order.
	pub(crate) fn entries(&self) -> impl Iterator<Item = &PlanEntry> {
		self.groups.iter().flat_map(|group| &group.entries)
	}
}

/// An item in its new position.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct PlanEntry {
	/// The index of the item within its container.
	pub(crate) index: usize,

	/// The indices of `extern` blocks whose items merge into this `extern` block, in order.
	pub(crate) merged: Vec<usize>,
}

impl PlanEntry {
	pub(crate) fn new(index: usize) -> Self {
		Self { index, merged: Vec::new() }
	}
}

/// Consecutive items of one group (or sub-group).
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct PlanGroup {
	pub(crate) spacing: Spacing,

	pub(crate) entries: Vec<PlanEntry>,
}

impl PlanGroup {
	fn barrier(index: usize) -> Self {
		Self {
			spacing: Spacing::Barrier,
			entries: vec![PlanEntry::new(index)],
		}
	}
}

/// An item waiting to be sorted within its group.
#[derive(Debug)]
struct Sortable<'a> {
	index: usize,
	key: Key,
	tie: &'a str,
}

impl<'a> Sortable<'a> {
	fn new(index: usize, key: Key, ties: &'a [String]) -> Self {
		Self {
			index,
			key,
			tie: ties.get(index).map_or("", String::as_str),
		}
	}
}

/// How the items of a group are separated from each other in the text engine. Items of different groups are always
/// separated by a blank line.
///
/// None of these depend on how many lines an item spans, which rustfmt may change: sorting after rustfmt lays out
/// the items the same way.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum Spacing {
	/// A line break, or a blank line next to an item with attributes, doc comments, or comments above it.
	Compact,

	/// A blank line.
	Loose,

	/// A single item that never moves. Consecutive barriers keep whether a blank line separated them.
	Barrier,
}

/// The data type (or trait) an `impl` block attaches to.
fn attachment(block: &syn::ItemImpl, types: &HashMap<String, usize>, traits: &HashMap<String, usize>) -> Option<usize> {
	if let Some(ident) = self_type_ident(&block.self_ty) {
		let name = ident.unraw().to_string();
		let is_generic = block.generics.type_params().any(|param| param.ident.unraw() == name);

		if !is_generic && let Some(&owner) = types.get(&name) {
			return Some(owner);
		}
	}

	let (path, _) = block.trait_.as_ref()?;

	traits.get(&local_ident(path)?.unraw().to_string()).copied()
}

/// The group and sort key of a module item, or `None` for `impl` blocks, which need their container's data types.
fn classify_item(item: &syn::Item, style_edition: StyleEdition) -> Option<(OrderingSchemaItem, Key)> {
	use OrderingSchemaItem as Group;

	let classified = match item {
		syn::Item::ExternCrate(item) if has_macro_use(&item.attrs) => (Group::MacroScope, Key::Stable),
		syn::Item::ExternCrate(item) => (Group::ExternCrate, Key::ExternCrate(ExternCrateKey::new(item))),
		syn::Item::Macro(item) if item.ident.is_some() && item.mac.path.is_ident("macro_rules") => (Group::MacroScope, Key::Stable),
		syn::Item::Macro(_) => (Group::MacroInvocation, Key::Stable),
		syn::Item::Mod(item) if has_macro_use(&item.attrs) => (Group::MacroScope, Key::Stable),
		syn::Item::Mod(item) if item.content.is_none() => {
			// ordered like rustfmt orders them (by bytes), so rustfmt leaves the order alone
			(Group::Module(module_cfg(&item.attrs)), Key::Bytes(item.ident.unraw().to_string()))
		}
		syn::Item::Mod(item) => (Group::ModuleInlined(module_cfg(&item.attrs)), Key::name(&item.ident)),
		syn::Item::Use(item) if matches!(item.vis, syn::Visibility::Inherited) => (Group::Use, Key::Use(UseKey::new(item, style_edition))),
		syn::Item::Use(item) => (Group::ReExport, Key::Use(UseKey::new(item, style_edition))),
		syn::Item::Type(item) => (Group::TypeAlias, Key::name(&item.ident)),
		syn::Item::Const(item) => (Group::Const, Key::name(&item.ident)),
		syn::Item::Static(item) => match item.mutability {
			syn::StaticMutability::Mut(_) => (Group::StaticMut, Key::name(&item.ident)),
			_ => (Group::Static, Key::name(&item.ident)),
		},
		syn::Item::Struct(item) => (Group::DataType, Key::name(&item.ident)),
		syn::Item::Enum(item) => (Group::DataType, Key::name(&item.ident)),
		syn::Item::Union(item) => (Group::DataType, Key::name(&item.ident)),
		syn::Item::Trait(item) => (Group::DataType, Key::name(&item.ident)),
		syn::Item::TraitAlias(item) => (Group::DataType, Key::name(&item.ident)),
		syn::Item::Impl(_) => return None,
		syn::Item::ForeignMod(block) => (Group::ExternBlock, Key::ExternBlock(ExternKey::new(block))),
		syn::Item::Fn(item) => (Group::Fn, Key::name(&item.sig.ident)),
		_ => (Group::Other, Key::Stable),
	};

	Some(classified)
}

/// Whether sibling `extern` blocks may merge into one: `None` for blocks that must never merge.
fn extern_merge_key(block: &syn::ItemForeignMod) -> Option<(bool, String, String)> {
	if block.attrs.iter().any(|attr| matches!(attr.style, syn::AttrStyle::Inner(_))) {
		return None;
	}

	let key = ExternKey::new(block);

	Some((block.unsafety.is_some(), key.abi, key.attrs))
}

/// Sorts the items of each group and appends the groups in order.
fn flush_buckets<G: Copy + Ord>(buckets: &mut BTreeMap<G, Vec<Sortable<'_>>>, groups: &mut Vec<PlanGroup>, spacing: &impl Fn(G) -> Spacing) {
	for (group, mut entries) in std::mem::take(buckets) {
		sort_entries(&mut entries);
		groups.push(PlanGroup {
			spacing: spacing(group),
			entries: entries.iter().map(|entry| PlanEntry::new(entry.index)).collect(),
		});
	}
}

fn function_group(signature: &syn::Signature) -> OrderingSchemaAssociated {
	if signature.receiver().is_some() {
		return OrderingSchemaAssociated::Method;
	}

	match signature.ident.unraw().to_string().as_str() {
		"new" => OrderingSchemaAssociated::New,
		"_new" => OrderingSchemaAssociated::NewInternal,
		_ => OrderingSchemaAssociated::Fn,
	}
}

/// Whether `#[macro_use]` (possibly through `#[cfg_attr(.., macro_use)]`) is present.
fn has_macro_use(attrs: &[syn::Attribute]) -> bool {
	attrs.iter().any(|attr| {
		let path = attr.path();

		path.is_ident("macro_use")
			|| (path.is_ident("cfg_attr")
				&& matches!(&attr.meta, syn::Meta::List(list) if list.tokens.clone().into_iter().any(
					|token| matches!(token, TokenTree::Ident(ident) if ident == "macro_use")
				)))
	})
}

/// The identifier of a path that names an item of the current container: `Foo`, `Foo<T>`, or `self::Foo`.
fn local_ident(path: &syn::Path) -> Option<&syn::Ident> {
	if path.leading_colon.is_some() {
		return None;
	}

	let mut segments = path.segments.iter();
	let first = segments.next()?;

	match (segments.next(), segments.next()) {
		(None, _) => Some(&first.ident),
		(Some(second), None) if first.ident == "self" && first.arguments.is_none() => Some(&second.ident),
		_ => None,
	}
}

/// The names of the data types and traits of a container, mapped to the index of the first of each name in sorted
/// order (the one `impl` blocks attach to when several `cfg` variants exist).
fn local_type_names(items: &[&syn::Item], data_types: &[Sortable<'_>]) -> (HashMap<String, usize>, HashMap<String, usize>) {
	let mut types = HashMap::new();
	let mut traits = HashMap::new();

	for entry in data_types {
		let (ident, is_trait) = match items[entry.index] {
			syn::Item::Struct(item) => (&item.ident, false),
			syn::Item::Enum(item) => (&item.ident, false),
			syn::Item::Union(item) => (&item.ident, false),
			syn::Item::Trait(item) => (&item.ident, true),
			syn::Item::TraitAlias(item) => (&item.ident, false),
			_ => continue,
		};
		let name = ident.unraw().to_string();

		if is_trait {
			traits.entry(name.clone()).or_insert(entry.index);
		}

		types.entry(name).or_insert(entry.index);
	}

	(types, traits)
}

/// Sorted `extern` blocks, where every mergeable block with the same unsafety, ABI, and attributes as an earlier
/// mergeable block merges into that earlier block.
fn merge_extern_blocks_of(items: &[&syn::Item], blocks: &[Sortable<'_>], mergeable: &dyn Fn(usize) -> bool) -> Vec<PlanEntry> {
	let mut entries: Vec<PlanEntry> = Vec::new();
	let mut survivors = HashMap::new();

	for block in blocks {
		let key = match items[block.index] {
			syn::Item::ForeignMod(item) if mergeable(block.index) => extern_merge_key(item),
			_ => None,
		};

		if let Some(key) = key {
			if let Some(&position) = survivors.get(&key) {
				let survivor: &mut PlanEntry = &mut entries[position];

				survivor.merged.push(block.index);
				continue;
			}

			survivors.insert(key, entries.len());
		}

		entries.push(PlanEntry::new(block.index));
	}

	entries
}

/// The `mod` sub-group: no `cfg`, some `cfg`, or exactly `#[cfg(test)]`.
fn module_cfg(attrs: &[syn::Attribute]) -> OrderingSchemaModule {
	let mut cfgs = attrs.iter().filter(|attr| attr.path().is_ident("cfg"));

	match (cfgs.next(), cfgs.next()) {
		(None, _) => OrderingSchemaModule::Typical,
		(Some(attr), None) if matches!(&attr.meta, syn::Meta::List(list) if list.tokens.to_string() == "test") => OrderingSchemaModule::Test,
		_ => OrderingSchemaModule::Cfg,
	}
}

/// Orders the items of an `extern` block.
pub(crate) fn plan_foreign_items(items: &[&syn::ForeignItem], ties: &[String]) -> Plan {
	let classified = items.iter().map(|item| match item {
		syn::ForeignItem::Type(item) => (OrderingSchemaForeign::Type, Key::name(&item.ident)),
		syn::ForeignItem::Static(item) => (OrderingSchemaForeign::Static, Key::name(&item.ident)),
		syn::ForeignItem::Fn(item) => (OrderingSchemaForeign::Fn, Key::name(&item.sig.ident)),
		syn::ForeignItem::Macro(_) => (OrderingSchemaForeign::MacroInvocation, Key::Stable),
		_ => (OrderingSchemaForeign::Other, Key::Stable),
	});

	plan_groups(classified, ties, OrderingSchemaForeign::spacing)
}

/// Sorts the items between barriers by group, then within each group.
fn plan_groups<G: Copy + Ord>(classified: impl Iterator<Item = (G, Key)>, ties: &[String], spacing: impl Fn(G) -> Spacing) -> Plan {
	let mut groups = Vec::new();
	let mut buckets: BTreeMap<G, Vec<Sortable<'_>>> = BTreeMap::new();

	for (index, (group, key)) in classified.enumerate() {
		if spacing(group) == Spacing::Barrier {
			flush_buckets(&mut buckets, &mut groups, &spacing);
			groups.push(PlanGroup::barrier(index));
		} else {
			buckets.entry(group).or_default().push(Sortable::new(index, key, ties));
		}
	}

	flush_buckets(&mut buckets, &mut groups, &spacing);

	Plan { groups }
}

/// Orders the items of an `impl` block.
pub(crate) fn plan_impl_items(items: &[&syn::ImplItem], ties: &[String]) -> Plan {
	let classified = items.iter().map(|item| match item {
		syn::ImplItem::Type(item) => (OrderingSchemaAssociated::Type, Key::name(&item.ident)),
		syn::ImplItem::Const(item) => (OrderingSchemaAssociated::Const, Key::name(&item.ident)),
		syn::ImplItem::Fn(item) => (function_group(&item.sig), Key::name(&item.sig.ident)),
		syn::ImplItem::Macro(_) => (OrderingSchemaAssociated::MacroInvocation, Key::Stable),
		_ => (OrderingSchemaAssociated::Other, Key::Stable),
	});

	plan_groups(classified, ties, OrderingSchemaAssociated::spacing)
}

/// Orders the items of a module (a file root or an inline module), with `use` items ordered for `style_edition`.
///
/// `ties` holds the tie-breaking text of each item (see [`crate::tokens::tie_text`]), and `mergeable` tells which
/// `extern` blocks may merge with their siblings.
pub(crate) fn plan_items(items: &[&syn::Item], ties: &[String], mergeable: &dyn Fn(usize) -> bool, style_edition: StyleEdition) -> Plan {
	let mut groups = Vec::new();
	let mut segment = Vec::new();

	for (index, item) in items.iter().enumerate() {
		match classify_item(item, style_edition) {
			Some((group, _)) if group.spacing() == Spacing::Barrier => {
				groups.extend(plan_module_segment(items, std::mem::take(&mut segment), ties, mergeable));
				groups.push(PlanGroup::barrier(index));
			}
			class => segment.push((index, class)),
		}
	}

	groups.extend(plan_module_segment(items, segment, ties, mergeable));

	Plan { groups }
}

/// Orders the items between two barriers of a module: `segment` holds their indices and classifications (`None` for
/// `impl` blocks).
fn plan_module_segment(
	items: &[&syn::Item],
	segment: Vec<(usize, Option<(OrderingSchemaItem, Key)>)>,
	ties: &[String],
	mergeable: &dyn Fn(usize) -> bool,
) -> Vec<PlanGroup> {
	let mut buckets: BTreeMap<OrderingSchemaItem, Vec<Sortable<'_>>> = BTreeMap::new();
	let mut impls = Vec::new();

	for (index, class) in segment {
		match class {
			Some((group, key)) => buckets.entry(group).or_default().push(Sortable::new(index, key, ties)),
			None => impls.push(index),
		}
	}

	for entries in buckets.values_mut() {
		sort_entries(entries);
	}

	let data_types = buckets.get(&OrderingSchemaItem::DataType).map_or(&[][..], Vec::as_slice);
	let (types, traits) = local_type_names(items, data_types);
	let mut attached: HashMap<usize, Vec<Sortable<'_>>> = HashMap::new();
	let mut loose = Vec::new();

	for index in impls {
		let syn::Item::Impl(block) = items[index] else {
			continue;
		};
		let key = ImplKey::new(block);

		match attachment(block, &types, &traits) {
			Some(owner) => attached
				.entry(owner)
				.or_default()
				.push(Sortable::new(index, Key::AttachedImpl(key), ties)),
			None => loose.push(Sortable::new(index, Key::LooseImpl(key), ties)),
		}
	}

	if !loose.is_empty() {
		sort_entries(&mut loose);
		buckets.insert(OrderingSchemaItem::LooseImpl, loose);
	}

	buckets
		.into_iter()
		.map(|(group, entries)| {
			let entries = match group {
				OrderingSchemaItem::DataType => with_attached_impls(&entries, &mut attached),
				OrderingSchemaItem::ExternBlock => merge_extern_blocks_of(items, &entries, mergeable),
				_ => entries.iter().map(|entry| PlanEntry::new(entry.index)).collect(),
			};

			PlanGroup {
				spacing: group.spacing(),
				entries,
			}
		})
		.collect()
}

/// Orders the items of a `trait` definition.
pub(crate) fn plan_trait_items(items: &[&syn::TraitItem], ties: &[String]) -> Plan {
	let classified = items.iter().map(|item| match item {
		syn::TraitItem::Type(item) => (OrderingSchemaAssociated::Type, Key::name(&item.ident)),
		syn::TraitItem::Const(item) => (OrderingSchemaAssociated::Const, Key::name(&item.ident)),
		syn::TraitItem::Fn(item) => (function_group(&item.sig), Key::name(&item.sig.ident)),
		syn::TraitItem::Macro(_) => (OrderingSchemaAssociated::MacroInvocation, Key::Stable),
		_ => (OrderingSchemaAssociated::Other, Key::Stable),
	});

	plan_groups(classified, ties, OrderingSchemaAssociated::spacing)
}

/// The name of an `impl` block's self type if it may be a data type of the same container:
/// `Foo`, `Foo<T>`, `self::Foo`, `&Foo`, `&mut (Foo)`, and so on.
fn self_type_ident(ty: &syn::Type) -> Option<&syn::Ident> {
	match ty {
		syn::Type::Reference(reference) => self_type_ident(&reference.elem),
		syn::Type::Paren(paren) => self_type_ident(&paren.elem),
		syn::Type::Group(group) => self_type_ident(&group.elem),
		syn::Type::Path(path) if path.qself.is_none() => local_ident(&path.path),
		_ => None,
	}
}

/// Sorts by key, then by tie-breaking text, then by original position.
fn sort_entries(entries: &mut [Sortable<'_>]) {
	entries.sort_by(|a, b| {
		a.key
			.compare(&b.key)
			.then_with(|| match a.key {
				Key::Stable => Ordering::Equal,
				_ => version_cmp(a.tie, b.tie),
			})
			.then(a.index.cmp(&b.index))
	});
}

/// Data types in order, each followed by its attached `impl` blocks.
fn with_attached_impls(data_types: &[Sortable<'_>], attached: &mut HashMap<usize, Vec<Sortable<'_>>>) -> Vec<PlanEntry> {
	let mut entries = Vec::new();

	for data_type in data_types {
		entries.push(PlanEntry::new(data_type.index));

		if let Some(mut impls) = attached.remove(&data_type.index) {
			sort_entries(&mut impls);
			entries.extend(impls.iter().map(|entry| PlanEntry::new(entry.index)));
		}
	}

	entries
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn all_data_type_kinds_share_a_group() {
		assert_eq!(
			planned("impl Delta {} trait Echo = Clone; union Delta { a: u8 } trait Charlie {} enum Bravo {} struct Alpha;"),
			"struct Alpha, enum Bravo, trait Charlie, union Delta, impl Delta, trait alias Echo"
		);
	}

	#[test]
	fn associated_items() {
		let source = "impl Lima {
			fn _mike(&self) {}
			fn mike(&self) {}
			unsafe fn new_unchecked() -> Option<Self> {}
			fn boxed(self: Box<Self>) {}
			const B: u8 = 0;
			fn _new() {}
			pub const fn new() -> Option<Self> {}
			type Output = ();
			fn consume(mut self) {}
			const A: u8 = 0;
			fn by_ref<'a>(&'a mut self) {}
			fn _helper() {}
			fn new(self) {}
			m!();
			n!{}
		}";

		assert_eq!(
			planned_associated(source),
			"type Output | const A, const B | fn new | fn _new | fn _helper, fn new_unchecked \
			 | fn boxed, fn by_ref, fn consume, fn mike, fn _mike, fn new | m ! () ; | n ! { }"
		);

		// macro invocations are barriers
		assert_eq!(
			planned_associated("impl X { fn b() {} m!(); fn d() {} fn c() {} const A: u8 = 0; }"),
			"fn b | m ! () ; | const A | fn c, fn d"
		);
	}

	/// Token text without the spaces that are not needed between identifiers: `< T as X > :: Y` → `<T as X>::Y`.
	fn compact_tokens(node: &impl ToTokens) -> String {
		let text = node.to_token_stream().to_string();
		let chars: Vec<char> = text.chars().collect();
		let is_word = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric() || *c == '_');

		chars
			.iter()
			.enumerate()
			.filter(|&(index, c)| *c != ' ' || is_word(chars.get(index.wrapping_sub(1))) && is_word(chars.get(index + 1)))
			.map(|(_, c)| c)
			.collect()
	}

	/// A short description of an item: its kind keyword and name.
	fn describe(item: &syn::Item) -> String {
		match item {
			syn::Item::ExternCrate(item) => match &item.rename {
				Some((_, rename)) => format!("extern crate {} as {rename}", item.ident),
				None => format!("extern crate {}", item.ident),
			},
			syn::Item::Macro(item) => match &item.ident {
				Some(ident) => format!("macro_rules {ident}"),
				None => format!("{}!", item.mac.path.to_token_stream()),
			},
			syn::Item::Mod(item) => format!("mod {}{}", item.ident, if item.content.is_some() { " {}" } else { ";" }),
			syn::Item::Use(item) => item.to_token_stream().to_string(),
			syn::Item::Type(item) => format!("type {}", item.ident),
			syn::Item::Const(item) => format!("const {}", item.ident),
			syn::Item::Static(item) => format!("static {}", item.ident),
			syn::Item::Struct(item) => format!("struct {}", item.ident),
			syn::Item::Enum(item) => format!("enum {}", item.ident),
			syn::Item::Union(item) => format!("union {}", item.ident),
			syn::Item::Trait(item) => format!("trait {}", item.ident),
			syn::Item::TraitAlias(item) => format!("trait alias {}", item.ident),
			syn::Item::Impl(item) => {
				let self_ty = compact_tokens(&item.self_ty);

				match &item.trait_ {
					Some((path, _)) => format!("impl {} for {self_ty}", compact_tokens(path)),
					None => format!("impl {self_ty}"),
				}
			}
			syn::Item::ForeignMod(item) => {
				let names: Vec<String> = item
					.items
					.iter()
					.map(|item| match item {
						syn::ForeignItem::Fn(item) => item.sig.ident.to_string(),
						syn::ForeignItem::Static(item) => item.ident.to_string(),
						syn::ForeignItem::Type(item) => item.ident.to_string(),
						_ => "?".to_owned(),
					})
					.collect();

				format!("extern [{}]", names.join(" "))
			}
			syn::Item::Fn(item) => format!("fn {}", item.sig.ident),
			other => other.to_token_stream().to_string(),
		}
	}

	#[test]
	fn extern_blocks_merge() {
		let source = "
			unsafe extern \"C\" { pub fn c(); }
			unsafe extern \"C\" { pub fn a(); }
			extern \"C\" { pub fn safe(); }
			#[link(name = \"x\")] unsafe extern \"C\" { pub fn linked(); }
			unsafe extern \"system\" { pub fn sys(); }
			unsafe extern \"C\" { pub fn b(); }
			extern { fn no_abi(); }
			unsafe extern \"C\" { #![allow(x)] pub fn inner(); }
			#[link(name = \"x\")] unsafe extern \"C\" { pub fn linked_too(); }
		";

		// by ABI, then attributes, then token text (`extern` before `unsafe extern`)
		// `extern {}` is `extern "C" {}`
		assert_eq!(
			planned(source),
			"extern [no_abi] +extern [safe], extern [inner], extern [a] +extern [b] +extern [c], \
			 extern [linked] +extern [linked_too], extern [sys]"
		);
		assert_eq!(
			planned_with(source, false),
			"extern [no_abi], extern [safe], extern [inner], extern [a], extern [b], extern [c], extern [linked], \
			 extern [linked_too], extern [sys]"
		);
	}

	#[test]
	fn extern_crates_sort_like_rustfmt() {
		// by bytes of the crate name (without `r#`), then without a rename first, then by the rename
		assert_eq!(
			planned(
				"extern crate zeta as alpha; extern crate beta; extern crate alloc as _; extern crate beta as abc; \
				 extern crate beta as _; extern crate Zed; extern crate r#async; extern crate asynd;"
			),
			"extern crate Zed, extern crate alloc as _, extern crate r#async, extern crate asynd, extern crate beta, \
			 extern crate beta as _, extern crate beta as abc, extern crate zeta as alpha"
		);
	}

	#[test]
	fn foreign_items() {
		let file = syn::parse_file("extern \"C\" { fn b(); m!(); static S: u8; fn a(); type T; static mut R: u8; }").unwrap();
		let syn::Item::ForeignMod(block) = &file.items[0] else { panic!() };
		let ties = ties_of(&block.items);
		let plan = plan_foreign_items(&block.items.iter().collect::<Vec<_>>(), &ties);
		let order: Vec<usize> = plan.entries().map(|entry| entry.index).collect();

		// the macro is a barrier
		assert_eq!(order, [0, 1, 4, 5, 2, 3]);
		assert_eq!(
			plan.groups.iter().map(|group| group.spacing).collect::<Vec<_>>(),
			[Spacing::Compact, Spacing::Barrier, Spacing::Compact, Spacing::Compact, Spacing::Compact]
		);
	}

	#[test]
	fn group_spacing() {
		let items = parse_items(
			"extern crate a; mod b; use c; pub use d; type E = (); const F: () = (); static G: () = (); static mut H: () = (); struct I; fn j() {} mod k {}",
		);
		let plan = plan_of(&items, true);
		let (compact, loose) = (Spacing::Compact, Spacing::Loose);

		assert_eq!(
			plan.groups.iter().map(|group| group.spacing).collect::<Vec<_>>(),
			[
				compact, compact, compact, compact, compact, compact, compact, compact, loose, loose, loose
			]
		);
	}

	#[test]
	fn impl_attachment_edge_cases() {
		// a generic parameter shadowing a local type does not attach, nor do paths with more segments
		assert_eq!(
			planned("struct T; impl<T> Tr for T {} impl a::T {} impl ::T {} impl <T as X>::Y {} impl T {}"),
			"struct T, impl T | impl ::T, impl <T as X>::Y, impl Tr for T, impl a::T"
		);

		// trait impls of foreign types attach to local traits
		assert_eq!(
			planned("impl Local for u8 {} trait Local {} impl Foreign for u8 {}"),
			"trait Local, impl Local for u8 | impl Foreign for u8"
		);

		// type aliases are not data types
		assert_eq!(planned("impl Alias {} type Alias = u8;"), "type Alias | impl Alias");
	}

	#[test]
	fn impls_attach_to_first_cfg_variant() {
		let source = "#[cfg(windows)] struct Imp; impl Imp {} #[cfg(unix)] struct Imp;";

		assert_eq!(planned(source), "struct Imp, impl Imp, struct Imp");

		let items = parse_items(source);
		let plan = plan_of(&items, true);

		// `#[cfg(unix)]` sorts first by token text
		assert_eq!(plan.entries().map(|entry| entry.index).collect::<Vec<_>>(), [2, 1, 0]);
	}

	#[test]
	fn impls_follow_their_data_types() {
		let source = "
			impl Display for Beta {}
			impl Beta {}
			impl<T> From<T> for Beta {}
			impl Debug for Beta {}
			impl Alpha {}
			struct Beta;
			enum Alpha {}
			impl fmt::Debug for Beta {}
			impl Beta<u8> {}
			impl self::Alpha {}
			impl &Alpha {}
			impl Tr for u8 {}
			trait Tr {}
			impl Tr for Alpha {}
			impl Other {}
			impl Tr for &mut (Beta) {}
		";

		assert_eq!(
			planned(source),
			"enum Alpha, impl &Alpha, impl Alpha, impl self::Alpha, impl Tr for Alpha, \
			 struct Beta, impl Beta, impl Beta<u8>, impl Debug for Beta, impl fmt::Debug for Beta, impl Display for Beta, \
			 impl From<T> for Beta, impl Tr for &mut(Beta), \
			 trait Tr, impl Tr for u8 \
			 | impl Other"
		);
	}

	#[test]
	fn loose_impls() {
		assert_eq!(
			planned("impl Tr for B {} impl B {} impl A {} impl Ar for A {} impl Tr for A {}"),
			"impl A, impl Ar for A, impl Tr for A, impl B, impl Tr for B"
		);
	}

	#[test]
	fn macros_are_barriers() {
		// items are sorted between barriers, which never move
		assert_eq!(
			planned(
				"fn b() {} macro_rules! m { () => {} } fn z() {} fn a() {} m!(); struct S; fn c() {} \
				 #[macro_use] mod macros; use x; #[macro_use] extern crate log; extern crate alloc; include!(\"x.rs\");"
			),
			"fn b | macro_rules m | fn a, fn z | m! | struct S | fn c | mod macros; | use x ; | extern crate log \
			 | extern crate alloc | include!"
		);
		assert_eq!(
			planned("z!(); macro_rules! b { () => {} } a!(); #[macro_use] mod z_macros; macro_rules! a { () => {} }"),
			"z! | macro_rules b | a! | mod z_macros; | macro_rules a"
		);
		assert_eq!(planned("#[cfg_attr(feature = \"x\", macro_use)] mod b; mod a;"), "mod b; | mod a;");

		// a redefined macro keeps its uses in place
		assert_eq!(
			planned(
				"macro_rules! v { () => { 1 } } const B: i32 = v!(); const A: i32 = 0; \
				 macro_rules! v { () => { 2 } } const C: i32 = v!();"
			),
			"macro_rules v | const A, const B | macro_rules v | const C"
		);

		// invocations that define macros stay above their uses
		assert_eq!(
			planned("cfg_x! { macro_rules! m { () => {} } } fn b() { m!() } fn a() {}"),
			"cfg_x! | fn a, fn b"
		);

		// `impl` blocks do not attach across barriers, and `extern` blocks do not merge across them
		assert_eq!(
			planned("struct A; m!(); impl A {} extern \"C\" { fn b(); } n!(); extern \"C\" { fn a(); }"),
			"struct A | m! | impl A | extern [b] | n! | extern [a]"
		);
	}

	#[test]
	fn module_declarations_sort_like_rustfmt() {
		// by bytes, without `r#`
		assert_eq!(
			planned("mod v10; mod v2; mod alpha; mod __private; mod Beta; mod r#zz; mod s; mod é; mod z;"),
			"mod Beta;, mod __private;, mod alpha;, mod s;, mod v10;, mod v2;, mod z;, mod r#zz;, mod é;"
		);
	}

	#[test]
	fn module_groups_in_order() {
		let source = "
			#[cfg(test)] mod tests {}
			fn function() {}
			unsafe extern \"C\" { fn ext(); }
			impl Loose {}
			struct Data;
			static mut STATIC_MUT: u8 = 0;
			static STATIC: u8 = 0;
			const CONST: u8 = 0;
			type Alias = u8;
			pub use reexport::Item;
			use private::Item;
			#[cfg(test)] mod test_decl;
			#[cfg(unix)] mod cfg_decl;
			mod decl;
			extern crate krate;
			mod inline {}
			#[cfg(unix)] mod cfg_inline {}
			fn no_body();
		";

		assert_eq!(
			planned(source),
			"extern crate krate | mod decl; | mod cfg_decl; | mod test_decl; | use private :: Item ; \
			 | pub use reexport :: Item ; | type Alias | const CONST | static STATIC | static STATIC_MUT | struct Data \
			 | impl Loose | extern [ext] | fn function | mod inline {} | mod cfg_inline {} | mod tests {} \
			 | fn no_body () ;"
		);
	}

	#[test]
	fn module_sub_groups() {
		assert_eq!(
			planned("#[cfg(test)] mod a; #[cfg(all(test, unix))] mod b; #[cfg(test)] #[cfg(unix)] mod c; mod d; #[cfg_attr(x, y)] mod e;"),
			"mod d;, mod e; | mod b;, mod c; | mod a;"
		);
		assert_eq!(
			planned("#[cfg(test)] mod tests {} mod b {} #[cfg(unix)] mod a {} mod c {}"),
			"mod b {}, mod c {} | mod a {} | mod tests {}"
		);
	}

	#[test]
	fn names_sort_with_underscores_following() {
		assert_eq!(
			planned("fn _mike() {} fn mikey() {} fn mike() {} fn __mike() {} fn Mike() {} fn _a() {} fn a() {}"),
			"fn Mike, fn a, fn _a, fn mike, fn _mike, fn __mike, fn mikey"
		);
		assert_eq!(
			planned("const _: () = (); const A: u8 = 0; const __: () = ();"),
			"const _, const __, const A"
		);
		assert_eq!(planned("fn r#type() {} fn typ() {} fn r#try() {}"), "fn r#try, fn typ, fn r#type");
		assert_eq!(planned("fn x16() {} fn x8() {} fn x_1() {}"), "fn x_1, fn x8, fn x16");
	}

	fn parse_items(source: &str) -> Vec<syn::Item> {
		syn::parse_file(source).unwrap().items
	}

	fn plan_of(items: &[syn::Item], merge: bool) -> Plan {
		let ties: Vec<String> = items.iter().map(crate::tokens::tie_text).collect();

		plan_items(&items.iter().collect::<Vec<_>>(), &ties, &|_| merge, StyleEdition::E2024)
	}

	/// The items of a module in planned order, as their token text, one group per line (groups separated by `|`).
	fn planned(source: &str) -> String {
		planned_with(source, true)
	}

	fn planned_associated(source: &str) -> String {
		let file = syn::parse_file(source).unwrap();
		let syn::Item::Impl(block) = &file.items[0] else { panic!() };
		let ties = ties_of(&block.items);
		let plan = plan_impl_items(&block.items.iter().collect::<Vec<_>>(), &ties);

		render_plan(&plan, |index| match &block.items[index] {
			syn::ImplItem::Type(item) => format!("type {}", item.ident),
			syn::ImplItem::Const(item) => format!("const {}", item.ident),
			syn::ImplItem::Fn(item) => format!("fn {}", item.sig.ident),
			other => other.to_token_stream().to_string(),
		})
	}

	fn planned_with(source: &str, merge: bool) -> String {
		let items = parse_items(source);

		render_plan(&plan_of(&items, merge), |index| describe(&items[index]))
	}

	#[test]
	fn plans_are_permutations() {
		let source = "
			impl A {} struct A; extern \"C\" { fn x(); } extern \"C\" { fn y(); } use a; pub use b; mod c; mod d {}
			macro_rules! e { () => {} } e!(); fn f() {} const G: u8 = 0; impl Tr for A {} trait Tr {} impl B {}
		";
		let items = parse_items(source);
		let plan = plan_of(&items, true);
		let mut seen: Vec<usize> = plan
			.entries()
			.flat_map(|entry| std::iter::once(entry.index).chain(entry.merged.iter().copied()))
			.collect();

		seen.sort_unstable();
		assert_eq!(seen, (0..items.len()).collect::<Vec<_>>());
	}

	fn render_plan(plan: &Plan, describe: impl Fn(usize) -> String) -> String {
		plan.groups
			.iter()
			.map(|group| {
				group
					.entries
					.iter()
					.map(|entry| {
						let mut text = describe(entry.index);

						for merged in &entry.merged {
							text.push_str(&format!(" +{}", describe(*merged)));
						}

						text
					})
					.collect::<Vec<_>>()
					.join(", ")
			})
			.collect::<Vec<_>>()
			.join(" | ")
	}

	#[test]
	fn ties_break_by_token_text() {
		// cfg variants of the same name
		assert_eq!(
			planned("#[cfg(windows)] fn imp() {} #[cfg(unix)] fn imp() {}"),
			planned("#[cfg(unix)] fn imp() {} #[cfg(windows)] fn imp() {}")
		);

		let items = parse_items("#[cfg(windows)] fn imp() {} #[cfg(unix)] fn imp() {}");
		let plan = plan_of(&items, true);

		assert_eq!(plan.entries().map(|entry| entry.index).collect::<Vec<_>>(), [1, 0]);
	}

	fn ties_of<T: ToTokens>(items: &[T]) -> Vec<String> {
		items.iter().map(|item| item.to_token_stream().to_string()).collect()
	}

	#[test]
	fn trait_items() {
		let file = syn::parse_file("trait T { fn b(&self); fn new() -> Self; type X; const C: u8; fn a(); }").unwrap();
		let syn::Item::Trait(block) = &file.items[0] else { panic!() };
		let ties = ties_of(&block.items);
		let plan = plan_trait_items(&block.items.iter().collect::<Vec<_>>(), &ties);
		let order: Vec<usize> = plan.entries().map(|entry| entry.index).collect();

		assert_eq!(order, [2, 3, 1, 4, 0]);
		assert_eq!(
			plan.groups.iter().map(|group| group.spacing).collect::<Vec<_>>(),
			[Spacing::Compact, Spacing::Compact, Spacing::Loose, Spacing::Loose, Spacing::Loose]
		);
	}

	#[test]
	fn uses_and_reexports() {
		assert_eq!(
			planned("pub(crate) use c::C; use b::B; pub use a::A; use self::x; use crate::y; use super::z; use ::abs;"),
			"use self :: x ;, use super :: z ;, use crate :: y ;, use :: abs ;, use b :: B ; \
			 | pub use a :: A ;, pub (crate) use c :: C ;"
		);
	}
}
