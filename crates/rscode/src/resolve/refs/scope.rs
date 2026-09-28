//! Bindings local to the code being walked: local variables, generic parameters, and the items and imports of blocks.
//!
//! Only names that can matter are recorded: local variables only when they are named like a target (they can only
//! shadow single-segment paths), while generic parameters and block items are always recorded (they can start a
//! longer path whose later segments name a target).

use crate::model::PathRef;
use crate::resolve::Namespace;
use crate::resolve::Res;
use smol_str::SmolStr;
use syn::GenericParam;
use syn::Generics;
use syn::Type;
use syn::TypeParamBound;
use syn::WherePredicate;
use syn::ext::IdentExt;

/// A binding of a name in a local scope.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) enum LocalBinding {
	/// A local variable.
	Variable,

	/// A generic parameter.
	Generic,

	/// An item defined in a block, or an import of one (not loaded, so never a target).
	Opaque,

	/// An import in a block, of these (in the namespace looked up).
	Imported(Vec<Res>),
}

/// A glob import in a block.
#[derive(Debug, Clone)]
pub(super) enum LocalGlob {
	/// Of these modules and enums.
	Sources(Vec<Res>),

	/// Of an enum or module defined in a block, which has these members.
	Names(Vec<(SmolStr, Namespace)>),
}

/// One scope: a block, an item (its generic parameters), a closure, a `match` arm, or the condition and body of an
/// `if`/`while`.
#[derive(Debug, Default)]
pub(super) struct Scope {
	/// Whether code inside cannot see the local variables and generic parameters of the enclosing scopes (an item
	/// nested in a body).
	pub(super) barrier: bool,

	pub(super) variables: Vec<SmolStr>,

	/// Type and const parameters (const parameters in both namespaces: `N` in `Foo<N>` parses as a type).
	pub(super) generics: Vec<(SmolStr, Namespace)>,

	/// Trait bounds of type parameters, by parameter name: those of the item's own parameters, and those its `where`
	/// clause puts on the parameters of enclosing items.
	pub(super) bounds: Vec<(SmolStr, PathRef)>,

	/// Items and imports of a block.
	pub(super) items: Vec<(SmolStr, Namespace, LocalBinding)>,

	/// Glob imports of a block.
	pub(super) globs: Vec<LocalGlob>,

	/// Members of the enums and modules defined in a block, for glob imports of them.
	pub(super) members: Vec<(SmolStr, Vec<(SmolStr, Namespace)>)>,
}

impl Scope {
	/// The scope of an item with generic parameters. `nested`: whether the item is inside of a body.
	pub(super) fn item(nested: bool, generics: &Generics) -> Self {
		let mut scope = Self {
			barrier: nested,
			..Self::default()
		};

		for param in &generics.params {
			match param {
				GenericParam::Type(param) => scope.generics.push((name(&param.ident), Namespace::Type)),

				GenericParam::Const(param) => {
					scope.generics.push((name(&param.ident), Namespace::Type));
					scope.generics.push((name(&param.ident), Namespace::Value));
				}

				GenericParam::Lifetime(_) => {}
			}
		}

		scope
	}

	/// Adds an item or import binding.
	pub(super) fn declare(&mut self, name: SmolStr, namespace: Namespace, binding: LocalBinding) {
		self.items.push((name, namespace, binding));
	}

	fn item_binding(&self, name: &str, namespace: Namespace) -> Option<&LocalBinding> {
		// later declarations win (they cannot legally clash, but imports are declared after items)
		self.items.iter().rev().find(|(item, item_namespace, _)| item == name && *item_namespace == namespace).map(|(_, _, binding)| binding)
	}

	/// The members of an enum or module defined in the block.
	pub(super) fn members_of(&self, name: &str) -> Option<&[(SmolStr, Namespace)]> {
		self.members.iter().find(|(item, _)| item == name).map(|(_, members)| members.as_slice())
	}
}

fn name(ident: &proc_macro2::Ident) -> SmolStr {
	SmolStr::new(ident.unraw().to_string())
}

/// The stack of local scopes, innermost last.
#[derive(Debug, Default)]
pub(super) struct Scopes {
	stack: Vec<Scope>,
}

impl Scopes {
	pub(super) fn push(&mut self, scope: Scope) {
		self.stack.push(scope);
	}

	pub(super) fn pop(&mut self) {
		self.stack.pop();
	}

	/// Binds a local variable in the innermost scope.
	pub(super) fn bind_variable(&mut self, name: SmolStr) {
		if let Some(scope) = self.stack.last_mut() {
			scope.variables.push(name);
		}
	}

	/// The innermost local binding of `name` in `namespace`, if any.
	///
	/// `items_only`: skip local variables and generic parameters (for identifier patterns). `members` looks up a name
	/// in the source of a glob import.
	pub(super) fn lookup(
		&self,
		name: &str,
		namespace: Namespace,
		items_only: bool,
		members: impl Fn(&Res, &str, Namespace) -> Vec<Res>,
	) -> Option<LocalBinding> {
		let mut outside_item = false;

		for scope in self.stack.iter().rev() {
			if !(outside_item || items_only) {
				if namespace == Namespace::Value && scope.variables.iter().any(|variable| variable == name) {
					return Some(LocalBinding::Variable);
				}

				if scope.generics.iter().any(|(generic, generic_namespace)| generic == name && *generic_namespace == namespace) {
					return Some(LocalBinding::Generic);
				}
			}

			if let Some(binding) = scope.item_binding(name, namespace) {
				return Some(binding.clone());
			}

			for glob in &scope.globs {
				match glob {
					LocalGlob::Sources(sources) => {
						let mut found: Vec<Res> = sources.iter().flat_map(|source| members(source, name, namespace)).collect();

						if !found.is_empty() {
							found.sort();
							found.dedup();
							return Some(LocalBinding::Imported(found));
						}
					}

					LocalGlob::Names(names) => {
						if names.iter().any(|(member, member_namespace)| member == name && *member_namespace == namespace) {
							return Some(LocalBinding::Opaque);
						}
					}
				}
			}

			outside_item |= scope.barrier;
		}

		None
	}

	/// The members of an enum or module defined in an enclosing block.
	pub(super) fn members_of(&self, name: &str) -> Option<&[(SmolStr, Namespace)]> {
		self.stack.iter().rev().find_map(|scope| scope.members_of(name))
	}

	/// The trait bounds of the type parameter `name` (the innermost one of that name): those of its declaration, and
	/// those of the `where` clauses of the items inside of the one declaring it.
	pub(super) fn bounds_of(&self, name: &str) -> Vec<&PathRef> {
		let mut bounds = Vec::new();

		for scope in self.stack.iter().rev() {
			bounds.extend(scope.bounds.iter().filter(|(parameter, _)| parameter == name).map(|(_, bound)| bound));

			if scope.generics.iter().any(|(generic, namespace)| generic == name && *namespace == Namespace::Type) {
				break;
			}
		}

		bounds
	}
}

/// The trait bounds that generics put on type parameters, as `(parameter, trait path)`: in the parameters'
/// declarations and in the `where` clause (on any single-identifier type, which may be a parameter of an enclosing
/// item).
pub(super) fn trait_bounds(generics: &Generics) -> Vec<(SmolStr, &syn::Path)> {
	let mut bounds = Vec::new();

	for param in &generics.params {
		if let GenericParam::Type(param) = param {
			bounds.extend(trait_paths(&param.bounds).map(|path| (name(&param.ident), path)));
		}
	}

	let predicates = generics.where_clause.iter().flat_map(|clause| &clause.predicates);

	for predicate in predicates {
		let WherePredicate::Type(predicate) = predicate else {
			continue;
		};

		let Type::Path(ty) = &predicate.bounded_ty else {
			continue;
		};

		if ty.qself.is_none()
			&& let Some(parameter) = ty.path.get_ident()
		{
			bounds.extend(trait_paths(&predicate.bounds).map(|path| (name(parameter), path)));
		}
	}

	bounds
}

/// The paths of the trait bounds among bounds.
fn trait_paths<'a>(bounds: impl IntoIterator<Item = &'a TypeParamBound>) -> impl Iterator<Item = &'a syn::Path> {
	bounds.into_iter().filter_map(|bound| match bound {
		TypeParamBound::Trait(bound) => Some(&bound.path),
		_ => None,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::model::CrateId;
	use crate::model::ItemId;

	fn no_members(_: &Res, _: &str, _: Namespace) -> Vec<Res> {
		Vec::new()
	}

	fn lookup(scopes: &Scopes, name: &str, namespace: Namespace) -> Option<LocalBinding> {
		scopes.lookup(name, namespace, false, no_members)
	}

	#[test]
	fn inner_scopes_shadow_outer_ones() {
		let mut scopes = Scopes::default();
		let generics: Generics = syn::parse_quote!(<T, const N: usize, 'a>);

		scopes.push(Scope::item(false, &generics));
		scopes.bind_variable("x".into());
		scopes.push(Scope::default());

		assert_eq!(lookup(&scopes, "T", Namespace::Type), Some(LocalBinding::Generic));
		assert_eq!(lookup(&scopes, "T", Namespace::Value), None);
		assert_eq!(lookup(&scopes, "N", Namespace::Value), Some(LocalBinding::Generic));
		assert_eq!(lookup(&scopes, "N", Namespace::Type), Some(LocalBinding::Generic));
		assert_eq!(lookup(&scopes, "a", Namespace::Type), None);
		assert_eq!(lookup(&scopes, "x", Namespace::Value), Some(LocalBinding::Variable));
		assert_eq!(lookup(&scopes, "x", Namespace::Type), None);

		let mut block = Scope::default();

		block.declare("x".into(), Namespace::Value, LocalBinding::Opaque);
		scopes.push(block);

		// a variable of an inner scope shadows an item of an outer one, and vice versa
		assert_eq!(lookup(&scopes, "x", Namespace::Value), Some(LocalBinding::Opaque));
		scopes.bind_variable("x".into());
		assert_eq!(lookup(&scopes, "x", Namespace::Value), Some(LocalBinding::Variable));
		assert_eq!(scopes.lookup("x", Namespace::Value, true, no_members), Some(LocalBinding::Opaque));
	}

	#[test]
	fn items_nested_in_bodies_only_see_outer_items() {
		let mut scopes = Scopes::default();
		let generics: Generics = syn::parse_quote!(<T>);
		let mut block = Scope::default();

		block.declare("Local".into(), Namespace::Type, LocalBinding::Opaque);
		scopes.push(Scope::item(false, &generics));
		scopes.push(block);
		scopes.bind_variable("x".into());
		scopes.push(Scope::item(true, &Generics::default()));

		assert_eq!(lookup(&scopes, "x", Namespace::Value), None);
		assert_eq!(lookup(&scopes, "T", Namespace::Type), None);
		assert_eq!(lookup(&scopes, "Local", Namespace::Type), Some(LocalBinding::Opaque));

		scopes.pop();
		assert_eq!(lookup(&scopes, "T", Namespace::Type), Some(LocalBinding::Generic));
	}

	#[test]
	fn glob_imports_of_blocks() {
		let module = Res::Item(ItemId::new(CrateId(0), 7));
		let mut block = Scope::default();

		block.globs.push(LocalGlob::Sources(vec![module.clone()]));
		block.globs.push(LocalGlob::Names(vec![("A".into(), Namespace::Value)]));

		let mut scopes = Scopes::default();

		scopes.push(block);

		let members = |source: &Res, name: &str, _: Namespace| if name == "B" { vec![source.clone()] } else { Vec::new() };

		assert_eq!(scopes.lookup("B", Namespace::Type, false, members), Some(LocalBinding::Imported(vec![module])));
		assert_eq!(scopes.lookup("A", Namespace::Value, false, members), Some(LocalBinding::Opaque));
		assert_eq!(scopes.lookup("A", Namespace::Type, false, members), None);
	}
}
