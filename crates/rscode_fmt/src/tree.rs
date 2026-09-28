//! A uniform view of the items of a file at every nesting level: module items (including those of inline modules),
//! and the items of `impl` blocks, traits, and `extern` blocks.
//!
//! Items inside function bodies and other expressions are not part of the tree.

use crate::source::Parsed;
use proc_macro2::Span;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::ops::Range;
use syn::spanned::Spanned;

/// An item at any nesting level.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Node<'a> {
	Item(&'a syn::Item),
	ImplItem(&'a syn::ImplItem),
	TraitItem(&'a syn::TraitItem),
	ForeignItem(&'a syn::ForeignItem),
}

impl<'a> Node<'a> {
	/// The items at the root of a file.
	pub(crate) fn roots(file: &'a syn::File) -> Vec<Self> {
		file.items.iter().map(Node::Item).collect()
	}

	/// The node at a structural index path: `[4, 2]` is the 3rd item inside the 5th top-level item.
	pub(crate) fn at(file: &'a syn::File, path: &[usize]) -> Option<Self> {
		let (&first, rest) = path.split_first()?;
		let mut node = Node::Item(file.items.get(first)?);

		for &index in rest {
			node = node.child(index)?;
		}

		Some(node)
	}

	/// The span of the whole item, from its first outer attribute (or doc comment) to its last token.
	pub(crate) fn span(self) -> Span {
		match self {
			Self::Item(item) => item.span(),
			Self::ImplItem(item) => item.span(),
			Self::TraitItem(item) => item.span(),
			Self::ForeignItem(item) => item.span(),
		}
	}

	/// The items directly inside this item.
	pub(crate) fn children(self) -> Vec<Self> {
		match self {
			Self::Item(syn::Item::Mod(module)) => match &module.content {
				Some((_, items)) => items.iter().map(Node::Item).collect(),
				None => Vec::new(),
			},
			Self::Item(syn::Item::Impl(block)) => block.items.iter().map(Node::ImplItem).collect(),
			Self::Item(syn::Item::Trait(definition)) => definition.items.iter().map(Node::TraitItem).collect(),
			Self::Item(syn::Item::ForeignMod(block)) => block.items.iter().map(Node::ForeignItem).collect(),
			_ => Vec::new(),
		}
	}

	/// The item at `index` directly inside this item.
	pub(crate) fn child(self, index: usize) -> Option<Self> {
		match self {
			Self::Item(syn::Item::Mod(module)) => module.content.as_ref()?.1.get(index).map(Node::Item),
			Self::Item(syn::Item::Impl(block)) => block.items.get(index).map(Node::ImplItem),
			Self::Item(syn::Item::Trait(definition)) => definition.items.get(index).map(Node::TraitItem),
			Self::Item(syn::Item::ForeignMod(block)) => block.items.get(index).map(Node::ForeignItem),
			_ => None,
		}
	}

	/// Whether this item contains items that can be sorted: an inline module, an `impl` block, a trait, or an
	/// `extern` block.
	pub(crate) fn is_container(self) -> bool {
		match self {
			Self::Item(syn::Item::Mod(module)) => module.content.is_some(),
			Self::Item(syn::Item::Impl(_) | syn::Item::Trait(_) | syn::Item::ForeignMod(_)) => true,
			_ => false,
		}
	}

	/// A short description of the kind of item, for comparisons and messages.
	pub(crate) fn kind(self) -> &'static str {
		match self {
			Self::Item(item) => item_kind(item),
			Self::ImplItem(item) => match item {
				syn::ImplItem::Const(_) => "associated const",
				syn::ImplItem::Fn(_) => "associated fn",
				syn::ImplItem::Type(_) => "associated type",
				syn::ImplItem::Macro(_) => "macro invocation",
				syn::ImplItem::Verbatim(_) => "unmodeled item",
				_ => "unknown item",
			},
			Self::TraitItem(item) => match item {
				syn::TraitItem::Const(_) => "associated const",
				syn::TraitItem::Fn(_) => "associated fn",
				syn::TraitItem::Type(_) => "associated type",
				syn::TraitItem::Macro(_) => "macro invocation",
				syn::TraitItem::Verbatim(_) => "unmodeled item",
				_ => "unknown item",
			},
			Self::ForeignItem(item) => match item {
				syn::ForeignItem::Fn(_) => "foreign fn",
				syn::ForeignItem::Static(_) => "foreign static",
				syn::ForeignItem::Type(_) => "foreign type",
				syn::ForeignItem::Macro(_) => "macro invocation",
				syn::ForeignItem::Verbatim(_) => "unmodeled item",
				_ => "unknown item",
			},
		}
	}

	/// The name of the item, if it has one. `impl` blocks are named after their trait and self type
	/// (`Display for Foo`), and macro invocations after their macro.
	pub(crate) fn name(self) -> Option<String> {
		match self {
			Self::Item(item) => item_name(item),
			Self::ImplItem(item) => match item {
				syn::ImplItem::Const(item) => Some(item.ident.to_string()),
				syn::ImplItem::Fn(item) => Some(item.sig.ident.to_string()),
				syn::ImplItem::Type(item) => Some(item.ident.to_string()),
				syn::ImplItem::Macro(item) => macro_name(&item.mac),
				_ => None,
			},
			Self::TraitItem(item) => match item {
				syn::TraitItem::Const(item) => Some(item.ident.to_string()),
				syn::TraitItem::Fn(item) => Some(item.sig.ident.to_string()),
				syn::TraitItem::Type(item) => Some(item.ident.to_string()),
				syn::TraitItem::Macro(item) => macro_name(&item.mac),
				_ => None,
			},
			Self::ForeignItem(item) => match item {
				syn::ForeignItem::Fn(item) => Some(item.sig.ident.to_string()),
				syn::ForeignItem::Static(item) => Some(item.ident.to_string()),
				syn::ForeignItem::Type(item) => Some(item.ident.to_string()),
				syn::ForeignItem::Macro(item) => macro_name(&item.mac),
				_ => None,
			},
		}
	}

	/// `kind` and `name` for messages, such as ``fn `main` ``.
	pub(crate) fn describe(self) -> String {
		match self.name() {
			Some(name) => format!("{} `{name}`", self.kind()),
			None => self.kind().to_owned(),
		}
	}

	/// Whether rustfmt may reorder this item among its neighbors of the same kind: `use` and `extern crate` items
	/// (with its `reorder_imports` option, on by default).
	pub(crate) fn is_reorderable(self) -> bool {
		matches!(self, Self::Item(syn::Item::Use(_) | syn::Item::ExternCrate(_)))
	}

	/// Whether this is a `use` item that imports nothing, such as `use a::{};`, which rustfmt may remove.
	pub(crate) fn imports_nothing(self) -> bool {
		matches!(self, Self::Item(syn::Item::Use(item)) if imports_nothing(&item.tree))
	}

	/// What identifies a reorderable item (see [`Node::is_reorderable`]) wherever rustfmt moves it: its attributes,
	/// visibility, and what it imports. rustfmt's normalizations of import lists (sorting, removing redundant braces
	/// and renames, removing duplicates) and of visibilities (`pub(in crate)` to `pub(crate)`) do not change it.
	pub(crate) fn identity(self) -> Option<String> {
		match self {
			Self::Item(syn::Item::Use(item)) => {
				let mut leaves = Vec::new();
				let prefix = if item.leading_colon.is_some() { "::" } else { "" };

				flatten_use_tree(prefix.to_owned(), &item.tree, &mut leaves);
				leaves.sort_unstable();
				leaves.dedup();

				Some(format!("{} {} use {}", attributes(&item.attrs), visibility(&item.vis), leaves.join(", ")))
			}
			Self::Item(syn::Item::ExternCrate(item)) => {
				let rename = match &item.rename {
					Some((_, rename)) if *rename != item.ident => format!(" as {rename}"),
					_ => String::new(),
				};

				Some(format!("{} {} extern crate {}{rename}", attributes(&item.attrs), visibility(&item.vis), item.ident))
			}
			_ => None,
		}
	}
}

/// Why two lists of sibling items do not correspond (see [`align`]).
#[derive(Debug)]
pub(crate) enum Misalignment<'a> {
	/// The items are not the same items in the same order.
	Changed,

	/// A reorderable item has no counterpart.
	Missing(Node<'a>),
}

/// Maps each item of a list of sibling items to the corresponding item of another list: `alignment[i]` is the index
/// in `b` of the item corresponding to `a[i]`.
///
/// The lists must have the same items in the same order, except that reorderable items (see
/// [`Node::is_reorderable`]) may be permuted among each other, and that `use` items importing nothing (which rustfmt
/// may remove) may be missing from `b`: those map to `None`. Reorderable items are matched by identity, the n-th
/// item of an identity in `a` with the n-th in `b`.
pub(crate) fn align<'a>(a: &[Node<'a>], b: &[Node<'_>]) -> Result<Vec<Option<usize>>, Misalignment<'a>> {
	let kept = |nodes: &[Node<'_>]| -> Vec<usize> {
		nodes.iter().enumerate().filter(|(_, node)| !node.imports_nothing()).map(|(index, _)| index).collect()
	};
	let kept_a = kept(a);
	let kept_b = kept(b);
	let same_shape = kept_a.len() == kept_b.len()
		&& kept_a.iter().zip(&kept_b).all(|(&index_a, &index_b)| {
			let (node_a, node_b) = (a[index_a], b[index_b]);

			node_a.kind() == node_b.kind() && (node_a.is_reorderable() || node_a.name() == node_b.name())
		});

	if !same_shape {
		return Err(Misalignment::Changed);
	}

	let mut alignment = vec![None; a.len()];

	for (&index_a, &index_b) in kept_a.iter().zip(&kept_b) {
		if !a[index_a].is_reorderable() {
			alignment[index_a] = Some(index_b);
		}
	}

	// identities are computed once per item: they flatten import trees and print attributes
	let mut by_identity: HashMap<String, VecDeque<usize>> = HashMap::new();

	for (index, node) in b.iter().enumerate() {
		if let Some(identity) = node.identity() {
			by_identity.entry(identity).or_default().push_back(index);
		}
	}

	for (index, &node) in a.iter().enumerate() {
		if let Some(identity) = node.identity() {
			alignment[index] = by_identity.get_mut(&identity).and_then(VecDeque::pop_front);

			if alignment[index].is_none() && !node.imports_nothing() {
				return Err(Misalignment::Missing(node));
			}
		}
	}

	Ok(alignment)
}

/// Whether the items inside two items correspond (see [`align`]) at every level.
pub(crate) fn same_subtree(a: Node<'_>, b: Node<'_>) -> bool {
	let a = a.children();
	let b = b.children();

	match align(&a, &b) {
		// reorderable items have no children
		Ok(alignment) => alignment.iter().zip(&a).all(|(found, &node)| match found {
			Some(index) => node.is_reorderable() || same_subtree(node, b[*index]),
			None => true,
		}),
		Err(_) => false,
	}
}

/// Writes `pub(in crate)`, `pub(in self)`, and `pub(in super)` as `pub(crate)`, `pub(self)`, and `pub(super)`, as
/// rustfmt does.
pub(crate) fn drop_redundant_in(restricted: &mut syn::VisRestricted) {
	let keyword = restricted.path.get_ident().is_some_and(|ident| ident == "crate" || ident == "self" || ident == "super");

	if keyword {
		restricted.in_token = None;
	}
}

/// The tokens of a visibility as text, normalized like rustfmt does (see [`drop_redundant_in`]).
fn visibility(visibility: &syn::Visibility) -> String {
	match visibility {
		syn::Visibility::Restricted(restricted) if restricted.in_token.is_some() => {
			let mut restricted = restricted.clone();

			drop_redundant_in(&mut restricted);
			tokens(&syn::Visibility::Restricted(restricted))
		}
		_ => tokens(visibility),
	}
}

/// Whether a use tree imports nothing, such as `a::{}` or `a::{b::{}}`.
fn imports_nothing(tree: &syn::UseTree) -> bool {
	match tree {
		syn::UseTree::Path(path) => imports_nothing(&path.tree),
		syn::UseTree::Name(_) | syn::UseTree::Rename(_) | syn::UseTree::Glob(_) => false,
		syn::UseTree::Group(group) => group.items.iter().all(imports_nothing),
	}
}

/// Collects the imported paths of a use tree, such as `a::b`, `a::c as d`, and `a::*`.
fn flatten_use_tree(prefix: String, tree: &syn::UseTree, leaves: &mut Vec<String>) {
	match tree {
		syn::UseTree::Path(path) => flatten_use_tree(format!("{prefix}{}::", path.ident), &path.tree, leaves),
		syn::UseTree::Name(name) => leaves.push(format!("{prefix}{}", name.ident)),
		// rustfmt removes renames to the same name
		syn::UseTree::Rename(rename) if rename.rename == rename.ident => leaves.push(format!("{prefix}{}", rename.ident)),
		syn::UseTree::Rename(rename) => leaves.push(format!("{prefix}{} as {}", rename.ident, rename.rename)),
		syn::UseTree::Glob(_) => leaves.push(format!("{prefix}*")),
		syn::UseTree::Group(group) => {
			for tree in &group.items {
				flatten_use_tree(prefix.clone(), tree, leaves);
			}
		}
	}
}

/// The tokens of syntax as text, ignoring whitespace and comments.
fn tokens(node: &impl quote::ToTokens) -> String {
	node.to_token_stream().to_string()
}

fn attributes(attributes: &[syn::Attribute]) -> String {
	attributes.iter().map(tokens).collect::<Vec<_>>().join(" ")
}

/// An item of a parsed file, with its position.
#[derive(Debug, Clone)]
pub(crate) struct Indexed<'a> {
	/// The structural index path.
	pub(crate) path: Vec<usize>,

	/// The byte range in the parsed text.
	pub(crate) range: Range<usize>,

	pub(crate) node: Node<'a>,
}

/// Every item of a parsed file at every nesting level, in document order.
///
/// Spans are computed once per item: computing a span converts the whole item to tokens.
pub(crate) fn index<'a>(parsed: &'a Parsed) -> Vec<Indexed<'a>> {
	let mut items = Vec::new();
	let mut stack: Vec<(Vec<usize>, Node<'a>)> =
		Node::roots(&parsed.file).into_iter().enumerate().rev().map(|(index, node)| (vec![index], node)).collect();

	while let Some((path, node)) = stack.pop() {
		for (index, child) in node.children().into_iter().enumerate().rev() {
			let mut child_path = path.clone();

			child_path.push(index);
			stack.push((child_path, child));
		}

		items.push(Indexed {
			range: parsed.range(node.span()),
			path,
			node,
		});
	}

	items
}

fn item_kind(item: &syn::Item) -> &'static str {
	match item {
		syn::Item::Const(_) => "const",
		syn::Item::Enum(_) => "enum",
		syn::Item::ExternCrate(_) => "extern crate",
		syn::Item::Fn(_) => "fn",
		syn::Item::ForeignMod(_) => "extern block",
		syn::Item::Impl(_) => "impl",
		syn::Item::Macro(item) if item.ident.is_some() => "macro_rules",
		syn::Item::Macro(_) => "macro invocation",
		syn::Item::Mod(_) => "mod",
		syn::Item::Static(_) => "static",
		syn::Item::Struct(_) => "struct",
		syn::Item::Trait(_) => "trait",
		syn::Item::TraitAlias(_) => "trait alias",
		syn::Item::Type(_) => "type alias",
		syn::Item::Union(_) => "union",
		syn::Item::Use(_) => "use",
		syn::Item::Verbatim(_) => "unmodeled item",
		_ => "unknown item",
	}
}

fn item_name(item: &syn::Item) -> Option<String> {
	match item {
		syn::Item::Const(item) => Some(item.ident.to_string()),
		syn::Item::Enum(item) => Some(item.ident.to_string()),
		syn::Item::ExternCrate(item) => Some(item.ident.to_string()),
		syn::Item::Fn(item) => Some(item.sig.ident.to_string()),
		syn::Item::Impl(item) => Some(impl_name(item)),
		syn::Item::Macro(item) => match &item.ident {
			Some(ident) => Some(ident.to_string()),
			None => macro_name(&item.mac),
		},
		syn::Item::Mod(item) => Some(item.ident.to_string()),
		syn::Item::Static(item) => Some(item.ident.to_string()),
		syn::Item::Struct(item) => Some(item.ident.to_string()),
		syn::Item::Trait(item) => Some(item.ident.to_string()),
		syn::Item::TraitAlias(item) => Some(item.ident.to_string()),
		syn::Item::Type(item) => Some(item.ident.to_string()),
		syn::Item::Union(item) => Some(item.ident.to_string()),
		_ => None,
	}
}

fn impl_name(item: &syn::ItemImpl) -> String {
	let self_name = type_name(&item.self_ty).unwrap_or_else(|| "_".to_owned());

	match item.trait_.as_ref().and_then(|(path, _)| path.segments.last()) {
		Some(segment) => format!("{} for {self_name}", segment.ident),
		None => self_name,
	}
}

/// The last identifier of a path type, looking through references, pointers, parentheses, and groups.
fn type_name(ty: &syn::Type) -> Option<String> {
	match ty {
		syn::Type::Path(path) => path.path.segments.last().map(|segment| segment.ident.to_string()),
		syn::Type::Reference(reference) => type_name(&reference.elem),
		syn::Type::Ptr(pointer) => type_name(&pointer.elem),
		syn::Type::Paren(paren) => type_name(&paren.elem),
		syn::Type::Group(group) => type_name(&group.elem),
		_ => None,
	}
}

fn macro_name(mac: &syn::Macro) -> Option<String> {
	mac.path.segments.last().map(|segment| format!("{}!", segment.ident))
}

#[cfg(test)]
mod tests {
	use super::*;

	const SOURCE: &str = "\
use std::fmt;

/// Docs.
#[derive(Debug)]
struct Foo;

impl fmt::Display for &Foo {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		Ok(())
	}
}

mod inner {
	trait Tr {
		const C: u8;
		type T;
		fn f();
		m!();
	}

	unsafe extern \"C\" {
		fn ext();
		static S: u8;
		type Opaque;
	}

	mod out_of_line;
}

macro_rules! mac {
	() => {};
}

mac!();
";

	fn parse() -> Parsed {
		Parsed::parse(SOURCE).unwrap()
	}

	fn start_of(needle: &str) -> usize {
		SOURCE.find(needle).unwrap()
	}

	fn find(parsed: &Parsed, start: usize) -> Option<Indexed<'_>> {
		index(parsed).into_iter().find(|item| item.range.start == start)
	}

	#[test]
	fn finds_items_at_every_level() {
		let parsed = parse();
		let cases = [
			("use std", vec![0], "use", None),
			("/// Docs.", vec![1], "struct", Some("Foo")),
			("impl fmt", vec![2], "impl", Some("Display for Foo")),
			("fn fmt", vec![2, 0], "associated fn", Some("fmt")),
			("mod inner", vec![3], "mod", Some("inner")),
			("trait Tr", vec![3, 0], "trait", Some("Tr")),
			("const C", vec![3, 0, 0], "associated const", Some("C")),
			("type T", vec![3, 0, 1], "associated type", Some("T")),
			("fn f()", vec![3, 0, 2], "associated fn", Some("f")),
			("m!();", vec![3, 0, 3], "macro invocation", Some("m!")),
			("unsafe extern", vec![3, 1], "extern block", None),
			("fn ext", vec![3, 1, 0], "foreign fn", Some("ext")),
			("static S", vec![3, 1, 1], "foreign static", Some("S")),
			("type Opaque", vec![3, 1, 2], "foreign type", Some("Opaque")),
			("mod out_of_line", vec![3, 2], "mod", Some("out_of_line")),
			("macro_rules", vec![4], "macro_rules", Some("mac")),
			("mac!();", vec![5], "macro invocation", Some("mac!")),
		];

		for (needle, path, kind, name) in &cases {
			let item = find(&parsed, start_of(needle)).unwrap_or_else(|| panic!("{needle}"));

			assert_eq!(&item.path, path, "{needle}");
			assert_eq!(item.node.kind(), *kind, "{needle}");
			assert_eq!(item.node.name().as_deref(), *name, "{needle}");
			assert_eq!(parsed.range(Node::at(&parsed.file, path).unwrap().span()), item.range);
		}

		// every item is indexed, in document order
		let items = index(&parsed);

		assert_eq!(items.len(), cases.len());
		assert!(items.windows(2).all(|pair| pair[0].range.start < pair[1].range.start));
	}

	#[test]
	fn rejects_offsets_that_are_not_item_starts() {
		let parsed = parse();

		for offset in [start_of("#[derive"), start_of("struct Foo"), start_of("Ok(())"), start_of("C\""), SOURCE.len(), 10_000] {
			assert!(find(&parsed, offset).is_none(), "{offset}");
		}
	}

	#[test]
	fn item_ranges_include_attributes_and_docs() {
		let parsed = parse();
		let item = find(&parsed, start_of("/// Docs.")).unwrap();

		assert_eq!(&SOURCE[item.range], "/// Docs.\n#[derive(Debug)]\nstruct Foo;");
	}

	#[test]
	fn containers() {
		let parsed = parse();
		let container = |path: &[usize]| Node::at(&parsed.file, path).unwrap().is_container();

		assert!(!container(&[0]));
		assert!(!container(&[1]));
		assert!(container(&[2]));
		assert!(container(&[3]));
		assert!(container(&[3, 0]));
		assert!(container(&[3, 1]));
		assert!(!container(&[3, 2]));
		assert!(!container(&[4]));
		assert!(Node::at(&parsed.file, &[3, 2, 0]).is_none());
		assert!(Node::at(&parsed.file, &[]).is_none());
		assert!(Node::at(&parsed.file, &[99]).is_none());
	}

	#[test]
	fn identities_normalize_visibilities() {
		let parsed = Parsed::parse("pub(in crate) use a;\npub(crate) use a;\npub(in crate::m) use a;\npub(in self) extern crate b;\npub(self) extern crate b;\n").unwrap();
		let identity = |index: usize| Node::at(&parsed.file, &[index]).unwrap().identity().unwrap();

		assert_eq!(identity(0), identity(1));
		assert_ne!(identity(1), identity(2));
		assert_eq!(identity(3), identity(4));
	}

	#[test]
	fn imports_of_nothing() {
		let parsed = Parsed::parse("use a::{};\nuse {};\nuse a::{b::{}, c::{}};\nuse a::{b, c::{}};\nuse a::*;\nfn f() {}\n").unwrap();
		let empty: Vec<bool> = Node::roots(&parsed.file).into_iter().map(Node::imports_nothing).collect();

		assert_eq!(empty, [true, true, true, false, false, false]);
	}

	#[test]
	fn aligns_siblings() {
		let a = Parsed::parse("use b;\nuse a::{};\nuse a;\nuse b;\nfn f() {}\n").unwrap();
		let b = Parsed::parse("use a;\nuse b;\nuse b;\nfn f() {}\n").unwrap();
		let alignment = align(&Node::roots(&a.file), &Node::roots(&b.file)).unwrap();

		assert_eq!(alignment, [Some(1), None, Some(0), Some(2), Some(3)]);

		let renamed = Parsed::parse("use a;\nuse b;\nuse b;\nfn g() {}\n").unwrap();
		let changed_import = Parsed::parse("use a;\nuse b;\nuse c;\nfn f() {}\n").unwrap();

		assert!(matches!(align(&Node::roots(&a.file), &Node::roots(&renamed.file)), Err(Misalignment::Changed)));
		assert!(matches!(align(&Node::roots(&a.file), &Node::roots(&changed_import.file)), Err(Misalignment::Missing(_))));
	}

	#[test]
	fn describe() {
		let parsed = parse();

		assert_eq!(Node::at(&parsed.file, &[1]).unwrap().describe(), "struct `Foo`");
		assert_eq!(Node::at(&parsed.file, &[0]).unwrap().describe(), "use");
	}
}
