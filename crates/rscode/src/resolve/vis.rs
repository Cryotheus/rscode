//! Visibility scopes and the module tree.

use super::names::is_macro_rules;
use crate::model::Crate;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::PathRef;
use crate::model::Visibility;
use crate::model::Workspace;

/// Answers whether a module is inside of another one in constant time.
#[derive(Debug, Default)]
pub(super) struct ModuleTree {
	/// By crate and item index: the pre-order entry and exit numbers of the item's module (of itself, for modules).
	spans: Vec<Vec<(u32, u32)>>,
}

impl ModuleTree {
	pub(super) fn new(ws: &Workspace) -> Self {
		Self {
			spans: ws.crates().iter().map(|krate| module_spans(ws, krate)).collect(),
		}
	}

	/// Whether `module` is `ancestor` or one of its descendants (for other items: whether their module is).
	pub(super) fn is_within(&self, module: ItemId, ancestor: ItemId) -> bool {
		if module.krate() != ancestor.krate() {
			return false;
		}

		let Some(spans) = self.spans.get(module.krate().index()) else {
			return false;
		};

		match (spans.get(module.index()), spans.get(ancestor.index())) {
			(Some(&(start, end)), Some(&(ancestor_start, ancestor_end))) => ancestor_start <= start && end <= ancestor_end,
			_ => false,
		}
	}
}

/// Where an item or a binding can be seen from.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub(super) enum Vis {
	/// Everywhere, including other crates.
	Public,

	/// Inside of the module and its descendants.
	Module(ItemId),
}

impl Vis {
	pub(super) fn is_visible_from(self, tree: &ModuleTree, module: ItemId) -> bool {
		match self {
			Self::Public => true,
			Self::Module(scope) => tree.is_within(module, scope),
		}
	}

	/// Whether `self` admits strictly more modules than `other`.
	pub(super) fn is_wider_than(self, tree: &ModuleTree, other: Self) -> bool {
		match (self, other) {
			(Self::Public, Self::Module(_)) => true,
			(Self::Module(this), Self::Module(other)) => this != other && tree.is_within(other, this),
			_ => false,
		}
	}

	/// The more restrictive of two visibilities: an import never makes what it imports more visible.
	pub(super) fn narrow(self, tree: &ModuleTree, other: Self) -> Self {
		match (self, other) {
			(Self::Public, other) | (other, Self::Public) => other,
			(Self::Module(this), Self::Module(other)) if tree.is_within(other, this) => Self::Module(other),
			(this, _) => this,
		}
	}

	/// The module whose subtree may see the item, or `None` when it is public.
	pub(super) fn scope(self) -> Option<ItemId> {
		match self {
			Self::Public => None,
			Self::Module(module) => Some(module),
		}
	}
}

/// Whether `module` is `ancestor` or one of its descendants.
fn contains(ws: &Workspace, ancestor: ItemId, module: ItemId) -> bool {
	let mut current = Some(module);

	while let Some(module) = current {
		if module == ancestor {
			return true;
		}

		current = parent_module(ws, module);
	}

	false
}

/// The visibility an item is declared with.
///
/// `macro_rules!` macros are visible in the whole crate (`pub(crate)`, as far as re-exports are concerned), or
/// everywhere with `#[macro_export]`. Items of trait `impl`s are public; variants and trait items have the visibility
/// of their enum or trait.
pub(super) fn declared_vis(ws: &Workspace, item: ItemId) -> Vis {
	if item.is_crate_root() {
		return Vis::Public;
	}

	let data = ws.item(item);
	let home = home_module(ws, item);
	let root = ItemId::crate_root(item.krate());

	if is_macro_rules(data) {
		return if data.attrs.macro_export { Vis::Public } else { Vis::Module(root) };
	}

	match &data.vis {
		Visibility::Public => Vis::Public,
		Visibility::Crate => Vis::Module(root),
		Visibility::Super => Vis::Module(parent_module(ws, home).unwrap_or(root)),
		Visibility::SelfModule | Visibility::Private => Vis::Module(home),

		// an unresolvable `pub(in path)` is an error; be lenient
		Visibility::InPath(path) => Vis::Module(vis_path_module(ws, home, path).unwrap_or(root)),

		Visibility::Inherited => inherited_vis(ws, item, home),
	}
}

/// The module an item is declared in: the nearest module strictly above it (the crate root for itself).
///
/// Extern blocks are transparent, and items of `impl` blocks, traits, and enums (and fields) belong to the enclosing
/// module.
pub(super) fn home_module(ws: &Workspace, item: ItemId) -> ItemId {
	match ws.parent(item) {
		Some(parent) => ws.module_of(parent),
		None => item,
	}
}

fn inherited_vis(ws: &Workspace, item: ItemId, home: ItemId) -> Vis {
	let Some(parent) = ws.parent(item) else {
		return Vis::Public;
	};

	let parent_data = ws.item(parent);

	match parent_data.kind {
		// (the fields of variants are as visible as their enum)
		ItemKind::Enum | ItemKind::Trait | ItemKind::Variant => declared_vis(ws, parent),
		ItemKind::Impl if parent_data.impl_info().is_some_and(|info| info.trait_path.is_some()) => Vis::Public,
		_ => Vis::Module(home),
	}
}

/// Numbers every module of a crate in pre-order (entry and exit), and gives other items the numbers of their module.
fn module_spans(ws: &Workspace, krate: &Crate) -> Vec<(u32, u32)> {
	let items = &krate.items;
	let mut spans = vec![(0, 0); items.len()];

	if items.is_empty() {
		return spans;
	}

	let mut counter = 1;

	// modules being visited, with the position of their next child
	let mut stack = vec![(0, 0)];

	while let Some(&(module, next)) = stack.last() {
		match items[module].children.get(next) {
			Some(&child) => {
				if let Some(top) = stack.last_mut() {
					top.1 += 1;
				}

				let child = child as usize;

				if items[child].kind == ItemKind::Module {
					spans[child].0 = counter;
					counter += 1;
					stack.push((child, 0));
				}
			}

			None => {
				spans[module].1 = counter;
				counter += 1;
				stack.pop();
			}
		}
	}

	for (id, data) in krate.items() {
		if data.kind != ItemKind::Module {
			spans[id.index()] = spans[ws.module_of(id).index()];
		}
	}

	spans
}

/// The module containing a module (`None` for crate roots).
pub(super) fn parent_module(ws: &Workspace, module: ItemId) -> Option<ItemId> {
	ws.parent(module).map(|parent| ws.module_of(parent))
}

/// The module named by the path of `pub(in path)` (`crate::a`, `super`, `self::b`; crate-relative in 2015).
fn vis_path_module(ws: &Workspace, home: ItemId, path: &PathRef) -> Option<ItemId> {
	let root = ItemId::crate_root(home.krate());
	let mut segments = path.segments.iter().peekable();

	let mut current = match segments.peek().map(|segment| segment.name.as_str()) {
		Some("crate") => {
			segments.next();
			root
		}

		Some("self") => {
			segments.next();
			home
		}

		Some("super") => home,
		_ => root,
	};

	for segment in segments {
		current = match segment.name.as_str() {
			"super" => parent_module(ws, current)?,
			"self" => current,

			// the path names an ancestor of the item: of `cfg` variants of a module, the one around the item
			name => {
				let mut modules = ws.children(current).filter(|&child| {
					let data = ws.item(child);

					data.kind == ItemKind::Module && data.name.as_deref() == Some(name)
				});

				let first = modules.next()?;

				if contains(ws, first, home) {
					first
				} else {
					modules.find(|&module| contains(ws, module, home)).unwrap_or(first)
				}
			}
		};
	}

	Some(current)
}
