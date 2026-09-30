//! The walk of one file as one module of one crate.
//!
//! Items at module level are matched to the model's items by position (to know what `Self` and an inline module
//! are); items inside of bodies are not loaded, and only bind names locally.

use super::Capture;
use super::Reference;
use super::ReferenceKind;
use super::ReferenceOptions;
use super::TargetName;
use super::Targets;
use super::docs::DocStyle;
use super::is_identifier;
use super::paths::Locals;
use super::paths::PathKey;
use super::paths::PathRes;
use super::paths::ident_name;
use super::paths::is_pattern_item;
use super::paths::res_in_namespace;
use super::scope::LocalBinding;
use super::scope::LocalGlob;
use super::scope::Scope;
use super::scope::Scopes;
use super::scope::trait_bounds;
use crate::load::thread_local;
use crate::model::CrateId;
use crate::model::ItemId;
use crate::model::ItemKind;
use crate::model::PathRef;
use crate::model::Workspace;
use crate::resolve::Namespace;
use crate::resolve::PathKind;
use crate::resolve::Res;
use crate::resolve::Resolver;
use crate::resolve::fxhash::FxHashMap;
use crate::source::FileId;
use crate::source::ParsedFile;
use crate::source::SourceFile;
use crate::source::TextRange;
use proc_macro2::Ident;
use proc_macro2::TokenStream;
use smol_str::SmolStr;
use std::borrow::Cow;
use syn::Arm;
use syn::Attribute;
use syn::Block;
use syn::ExprClosure;
use syn::ExprForLoop;
use syn::ExprIf;
use syn::ExprLet;
use syn::ExprMethodCall;
use syn::ExprPath;
use syn::ExprStruct;
use syn::ExprWhile;
use syn::Field;
use syn::Fields;
use syn::FnArg;
use syn::ForeignItem;
use syn::ForeignItemFn;
use syn::ForeignItemStatic;
use syn::ForeignItemType;
use syn::GenericArgument;
use syn::Generics;
use syn::ImplItem;
use syn::ImplItemConst;
use syn::ImplItemFn;
use syn::ImplItemType;
use syn::Item;
use syn::ItemConst;
use syn::ItemEnum;
use syn::ItemFn;
use syn::ItemImpl;
use syn::ItemMacro;
use syn::ItemMod;
use syn::ItemStatic;
use syn::ItemStruct;
use syn::ItemTrait;
use syn::ItemTraitAlias;
use syn::ItemType;
use syn::ItemUnion;
use syn::ItemUse;
use syn::Local;
use syn::Macro;
use syn::Pat;
use syn::PatIdent;
use syn::Signature;
use syn::Stmt;
use syn::TraitBound;
use syn::TraitItem;
use syn::TraitItemConst;
use syn::TraitItemFn;
use syn::TraitItemType;
use syn::Type;
use syn::TypePath;
use syn::UseTree;
use syn::Variant;
use syn::VisRestricted;
use syn::visit;
use syn::visit::Visit;

/// Finds the references to targets in one file, walked as one module of one crate.
pub(super) struct FileWalker<'a, 'ws> {
	pub(super) resolver: &'a Resolver<'ws>,
	pub(super) ws: &'ws Workspace,
	pub(super) targets: &'a Targets,
	pub(super) options: &'a ReferenceOptions,
	pub(super) parsed: &'a ParsedFile<'ws>,
	pub(super) source: &'ws SourceFile,
	pub(super) krate: CrateId,
	pub(super) file: FileId,

	/// The (loaded) module the walked code is in.
	pub(super) module: ItemId,

	/// How deep the walk is in code whose items are not loaded: bodies (of functions, constants, ...), macro bodies,
	/// and modules inside of those.
	pub(super) body_depth: usize,

	/// How many enclosing modules are not loaded (modules inside of bodies): `self` and `super` paths are not resolved
	/// inside of them.
	pub(super) unloaded_modules: usize,

	pub(super) scopes: Scopes,

	/// What `Self` refers to, for the enclosing `impl` blocks, traits, and type definitions (innermost last).
	pub(super) self_types: Vec<SelfTypes>,

	/// Resolutions of paths without local bindings.
	pub(super) cache: FxHashMap<PathKey, Vec<Vec<Res>>>,

	/// The references found.
	pub(super) out: Vec<Reference>,

	/// References that local bindings named like [`Targets::shadow`] would capture.
	pub(super) captures: Vec<Capture>,
}

impl<'a, 'ws> FileWalker<'a, 'ws> {
	pub(super) fn new(
		resolver: &'a Resolver<'ws>,
		targets: &'a Targets,
		options: &'a ReferenceOptions,
		parsed: &'a ParsedFile<'ws>,
		krate: CrateId,
		file: FileId,
		module: ItemId,
	) -> Self {
		let ws = resolver.workspace();

		Self {
			resolver,
			ws,
			targets,
			options,
			parsed,
			source: ws.krate(krate).file(file),
			krate,
			file,
			module,
			body_depth: 0,
			unloaded_modules: 0,
			scopes: Scopes::default(),
			self_types: Vec::new(),
			cache: FxHashMap::default(),
			out: Vec::new(),
			captures: Vec::new(),
		}
	}

	/// Binds a local variable (if it could shadow a target).
	fn bind(&mut self, ident: &Ident) {
		if self.targets.tracks(ident) {
			self.scopes.bind_variable(ident_name(ident));
		}
	}

	/// Records a reference by a path whose first segment names a target, if a local binding named like
	/// [`Targets::shadow`] would capture it (the last segment of the path being in `namespace`).
	pub(super) fn check_capture(&mut self, path: &PathRef, res: &PathRes, namespace: Namespace) {
		let (Some(shadow), PathRes::Segments(segments)) = (self.targets.shadow.as_ref().map(|shadow| shadow.name.as_str()), res) else {
			return;
		};

		let Some(first) = path.segments.first().filter(|first| !path.leading_colon && !is_path_keyword(&first.name)) else {
			return;
		};

		let Some(target) = (self.targets.named_str(&first.name)).and_then(|target| target.find(segments.first()?)) else {
			return;
		};

		let namespace = if path.segments.len() == 1 { namespace } else { Namespace::Type };

		let binding = match self.lookup_local(shadow, namespace, Locals::All) {
			Some(LocalBinding::Variable) => "local variable",
			Some(LocalBinding::Generic) => "generic parameter",
			Some(LocalBinding::Opaque | LocalBinding::Imported(_)) => "local item or import",
			None => return,
		};

		if let Some(reference) = self.reference(target, ReferenceKind::Path, first.range, true) {
			self.captures.push(Capture {
				reference,
				module: self.module,
				binding,
			});
		}
	}

	/// The loaded child of `parent` whose range contains `offset`.
	fn child_at(&self, parent: ItemId, offset: usize) -> Option<ItemId> {
		let children = &self.ws.item(parent).children;
		let krate = parent.krate();
		let count = children.partition_point(|&child| self.ws.item(ItemId::new(krate, child)).range.start <= offset);
		let child = ItemId::new(krate, *children.get(count.checked_sub(1)?)?);

		self.ws.item(child).range.contains(offset).then_some(child)
	}

	/// A function: generic parameters, parameters (whose patterns bind variables in the body), and body.
	fn function(&mut self, signature: &Signature, body: Option<&Block>) {
		self.item_scope(Some(&signature.generics), |this| {
			this.visit_generics(&signature.generics);

			for input in &signature.inputs {
				match input {
					FnArg::Receiver(receiver) => visit::visit_receiver(this, receiver),

					FnArg::Typed(typed) => {
						this.visit_type(&typed.ty);
						this.pattern(&typed.pat);
					}
				}
			}

			if let Some((pattern, _)) = signature.variadic.as_ref().and_then(|variadic| variadic.pat.as_ref()) {
				this.pattern(pattern);
			}

			this.visit_return_type(&signature.output);

			if let Some(body) = body {
				this.visit_block(body);
			}
		});
	}

	/// An identifier pattern: a path pattern if it names a constant or unit struct or variant, else a binding.
	fn ident_pattern(&mut self, pattern: &PatIdent) {
		let targets = self.targets;

		if let Some(target) = targets.named(&pattern.ident) {
			let simple = pattern.by_ref.is_none() && pattern.mutability.is_none() && pattern.subpat.is_none();

			if !(simple && self.path_pattern(&pattern.ident, target)) {
				self.scopes.bind_variable(ident_name(&pattern.ident));
			}
		} else if targets.tracks(&pattern.ident) {
			self.scopes.bind_variable(ident_name(&pattern.ident));
		}

		if let Some((_, subpattern)) = &pattern.subpat {
			self.pattern(subpattern);
		}
	}

	/// What `Self` refers to in an `impl` block (whose generic parameters are in scope), and whether it is loaded.
	fn impl_self_types(&mut self, item: &ItemImpl) -> (SelfTypes, bool) {
		let loaded = (self.body_depth == 0)
			.then(|| self.child_at(self.module, self.parsed.range(item.impl_token.span).start))
			.flatten()
			.filter(|&child| self.ws.item(child).kind == ItemKind::Impl);

		if let Some(impl_block) = loaded {
			let self_types = SelfTypes {
				types: self.resolver.impl_self_types(impl_block),
				traits: self.resolver.impl_traits(impl_block),
			};

			return (self_types, true);
		}

		let traits = match &item.trait_ {
			Some((path, _)) => {
				let path = self.path_ref(path);
				let res = self.resolve_path(&path, Namespace::Type, Locals::All);

				self.loaded(res.last(), &[ItemKind::Trait])
			}

			None => Vec::new(),
		};

		let self_types = SelfTypes {
			types: self.type_items(&item.self_ty),
			traits,
		};

		(self_types, false)
	}

	/// In an `impl` block that is not loaded (inside of a body), the items implementing target items of its traits.
	fn implementing_items(&mut self, item: &ItemImpl, traits: &[ItemId]) {
		if traits.is_empty() {
			return;
		}

		let targets = self.targets;

		for member in &item.items {
			let (ident, kind) = match member {
				ImplItem::Fn(member) => (&member.sig.ident, ItemKind::AssocFn),
				ImplItem::Const(member) => (&member.ident, ItemKind::AssocConst),
				ImplItem::Type(member) => (&member.ident, ItemKind::AssocType),
				_ => continue,
			};

			let Some(target) = targets.named(ident) else {
				continue;
			};

			let implemented = (target.items.iter().copied())
				.find(|&candidate| self.ws.item(candidate).kind == kind && self.ws.parent(candidate).is_some_and(|parent| traits.contains(&parent)));

			if let Some(implemented) = implemented {
				self.report(implemented, ReferenceKind::Definition, self.parsed.range(ident.span()), true);
			}
		}
	}

	/// Reports the segments of a leaf of a `use` tree that name targets.
	fn import(&mut self, leaf: &UseLeaf<'_>, leading_colon: bool) {
		let targets = self.targets;

		if !leaf.path.iter().any(|ident| targets.named(ident).is_some()) {
			return;
		}

		let path = PathRef {
			leading_colon,
			segments: leaf.path.iter().map(|ident| self.segment(ident)).collect(),
		};

		let wanted = matches!(leaf.kind, LeafKind::Glob | LeafKind::SelfImport(_)).then_some(Namespace::Type);
		let first = &path.segments[0].name;

		// in a block, a path may start at a local item or import
		let local = match leading_colon || is_path_keyword(first) {
			true => None,
			false => self.lookup_local(first, Namespace::Type, Locals::All),
		};

		let segments = match local {
			None => self.module_path(self.module, &path, wanted, PathKind::Use),

			Some(LocalBinding::Imported(first)) => {
				let namespaces = match &wanted {
					Some(namespace) => std::slice::from_ref(namespace),
					None => &Namespace::ALL[..],
				};
				let mut merged: Vec<Vec<Res>> = vec![Vec::new(); path.segments.len()];

				for &namespace in namespaces {
					for (all, found) in merged.iter_mut().zip(self.continue_path(first.clone(), &path, namespace)) {
						all.extend(found);
					}
				}

				merged
			}

			Some(_) => return,
		};

		let res = PathRes::Segments(segments);

		self.report_path(&path, &res, ReferenceKind::Import);

		// `use a::Name as Name;` binds the name it imports
		if let LeafKind::Rename(alias) = leaf.kind
			&& let Some(&last) = leaf.path.last()
			&& ident_name(alias) == ident_name(last)
			&& let Some(target) = targets.named(last).and_then(|target| target.find(res.last()))
		{
			self.report(target, ReferenceKind::Import, self.parsed.range(alias.span()), true);
		}
	}

	/// Runs `walk` in the scope of an item with generic parameters.
	fn item_scope(&mut self, generics: Option<&Generics>, walk: impl FnOnce(&mut Self)) {
		let nested = self.body_depth > 0;

		let scope = match generics {
			Some(generics) => {
				let mut scope = Scope::item(nested, generics);

				scope.bounds = (trait_bounds(generics).into_iter())
					.map(|(parameter, bound)| (parameter, self.path_ref(bound)))
					.collect();
				scope
			}

			None => Scope {
				barrier: nested,
				..Scope::default()
			},
		};

		self.scopes.push(scope);
		walk(self);
		self.scopes.pop();
	}

	/// The loaded item of kind `kind` named by the identifier, when walking the items of a loaded module.
	pub(super) fn loaded_item(&self, ident: &Ident, kind: ItemKind) -> Option<ItemId> {
		if self.body_depth > 0 {
			return None;
		}

		let range = self.parsed.range(ident.span());
		let child = self.child_at(self.module, range.start)?;
		let data = self.ws.item(child);

		(data.kind == kind && data.name_range == Some(range)).then_some(child)
	}

	/// The scope of the items of a block (or of a module inside of a body): the names they bind, and imports.
	pub(super) fn local_items<'i>(&mut self, items: impl Iterator<Item = &'i Item> + Clone) -> Scope {
		let mut scope = Scope::default();

		for item in items.clone() {
			for (name, namespace) in local_item_names(item) {
				scope.declare(name, namespace, LocalBinding::Opaque);
			}

			scope.members.extend(local_item_members(item));
		}

		for item in items {
			if let Item::Use(item) = item {
				self.local_use(&mut scope, item);
			}
		}

		scope
	}

	/// Declares the bindings of a `use` item of a block.
	fn local_use(&mut self, scope: &mut Scope, item: &ItemUse) {
		let leading_colon = item.leading_colon.is_some();

		for leaf in use_leaves(&item.tree) {
			let binding = leaf.binding();

			let Some(first) = leaf.path.first().map(|ident| ident_name(ident)) else {
				continue;
			};

			// a path starting at an item of a block, whose members are only known for enums and modules
			let local_start = !leading_colon
				&& !is_path_keyword(&first)
				&& (scope
					.items
					.iter()
					.any(|(name, namespace, _)| *name == first && *namespace == Namespace::Type)
					|| self
						.lookup_local(&first, Namespace::Type, Locals::All)
						.is_some_and(|local| !matches!(local, LocalBinding::Imported(_))));

			if local_start {
				match leaf.kind {
					LeafKind::Glob if leaf.path.len() == 1 => {
						let members = scope.members_of(&first).or_else(|| self.scopes.members_of(&first)).map(<[_]>::to_vec);

						scope.globs.extend(members.map(LocalGlob::Names));
					}

					LeafKind::Glob => {}

					_ => {
						if let Some(name) = binding {
							for namespace in Namespace::ALL {
								scope.declare(name.clone(), namespace, LocalBinding::Opaque);
							}
						}
					}
				}

				continue;
			}

			let path = PathRef {
				leading_colon,
				segments: leaf.path.iter().map(|ident| self.segment(ident)).collect(),
			};

			match leaf.kind {
				LeafKind::Glob => {
					let sources = self
						.module_path(self.module, &path, Some(Namespace::Type), PathKind::Use)
						.pop()
						.unwrap_or_default();

					scope.globs.push(LocalGlob::Sources(sources));
				}

				_ => {
					let Some(name) = binding else {
						continue;
					};

					let wanted = matches!(leaf.kind, LeafKind::SelfImport(_)).then_some(Namespace::Type);
					let imported = self.module_path(self.module, &path, wanted, PathKind::Use).pop().unwrap_or_default();
					let mut bound = false;

					for namespace in Namespace::ALL {
						let found: Vec<Res> = imported
							.iter()
							.filter(|&res| res_in_namespace(self.ws, res, namespace))
							.cloned()
							.collect();

						if !found.is_empty() {
							scope.declare(name.clone(), namespace, LocalBinding::Imported(found));
							bound = true;
						}
					}

					// an import of something unknown still shadows
					if !bound {
						for namespace in Namespace::ALL {
							scope.declare(name.clone(), namespace, LocalBinding::Opaque);
						}
					}
				}
			}
		}
	}

	/// A method call, which might call a target method.
	pub(super) fn method_call(&mut self, method: &Ident) {
		if !self.options.method_calls {
			return;
		}

		if let Some(&target) = self.targets.named(method).and_then(|target| target.methods.first()) {
			self.report(target, ReferenceKind::MethodCall, self.parsed.range(method.span()), false);
		}
	}

	/// Whether an identifier pattern names a constant or unit struct or variant (and reports it if it is a target).
	fn path_pattern(&mut self, ident: &Ident, target: &TargetName) -> bool {
		let path = PathRef {
			leading_colon: false,
			segments: vec![self.segment(ident)],
		};

		let res = self.resolve_path(&path, Namespace::Value, Locals::Items);

		let items: Vec<ItemId> = (res.last().iter())
			.filter_map(|res| match res {
				Res::Item(item) if is_pattern_item(self.ws.item(*item)) => Some(*item),
				_ => None,
			})
			.collect();

		if items.is_empty() {
			return false;
		}

		if let Some(&found) = items.iter().find(|&&item| target.contains(item)) {
			self.report(found, ReferenceKind::Path, path.segments[0].range, true);
		}

		true
	}

	/// Walks a pattern: reports the paths in it, and binds its variables in the innermost scope.
	pub(super) fn pattern(&mut self, pattern: &Pat) {
		match pattern {
			Pat::Ident(pattern) => self.ident_pattern(pattern),
			Pat::Path(path) => self.code_path(path.qself.as_ref(), &path.path, Namespace::Value),

			Pat::TupleStruct(pattern) => {
				self.code_path(pattern.qself.as_ref(), &pattern.path, Namespace::Value);

				for element in &pattern.elems {
					self.pattern(element);
				}
			}

			Pat::Struct(pattern) => {
				self.code_path(pattern.qself.as_ref(), &pattern.path, Namespace::Type);

				for field in &pattern.fields {
					match (&field.colon_token, &*field.pat) {
						// `Struct { name }` binds the field `name` (renaming a constant `name` would rename the field)
						(None, Pat::Ident(binding)) => self.bind(&binding.ident),

						_ => self.pattern(&field.pat),
					}
				}
			}

			Pat::Or(pattern) => {
				for case in &pattern.cases {
					self.pattern(case);
				}
			}

			Pat::Paren(pattern) => self.pattern(&pattern.pat),
			Pat::Reference(pattern) => self.pattern(&pattern.pat),

			Pat::Slice(pattern) => {
				for element in &pattern.elems {
					self.pattern(element);
				}
			}

			Pat::Tuple(pattern) => {
				for element in &pattern.elems {
					self.pattern(element);
				}
			}

			Pat::Type(pattern) => {
				self.pattern(&pattern.pat);
				self.visit_type(&pattern.ty);
			}

			Pat::Guard(pattern) => {
				self.pattern(&pattern.pat);
				self.visit_expr(&pattern.guard);
			}

			Pat::Lit(literal) => self.visit_expr_lit(literal),
			Pat::Range(range) => self.visit_expr_range(range),
			Pat::Const(block) => self.visit_expr_const(block),
			Pat::Macro(mac) => self.visit_macro(&mac.mac),
			Pat::Verbatim(tokens) => self.visit_token_stream(tokens),

			// `..`, `_`, and syntax added to syn later
			_ => {}
		}
	}

	fn reference(&self, target: ItemId, kind: ReferenceKind, range: TextRange, certain: bool) -> Option<Reference> {
		let name = self.ws.item(target).name.as_deref().unwrap_or_default();

		is_identifier(self.parsed.text.get(range.as_range()), name).then(|| Reference {
			target,
			kind,
			krate: self.krate,
			file: self.file,
			path: self.source.path().to_path_buf(),
			range,
			start: self.source.line_col(range.start),
			certain,
		})
	}

	/// Records a reference, unless the text at `range` is not the target's name (which only a bug could cause).
	pub(super) fn report(&mut self, target: ItemId, kind: ReferenceKind, range: TextRange, certain: bool) {
		if let Some(reference) = self.reference(target, kind, range, certain) {
			self.out.push(reference);
		}
	}

	/// Runs `walk` with `Self` referring to `self_types`.
	fn with_self(&mut self, self_types: SelfTypes, walk: impl FnOnce(&mut Self)) {
		self.self_types.push(self_types);
		walk(self);
		self.self_types.pop();
	}
}

impl<'ast> Visit<'ast> for FileWalker<'_, '_> {
	fn visit_arm(&mut self, arm: &'ast Arm) {
		self.scopes.push(Scope::default());
		self.pattern(&arm.pat);
		self.visit_expr(&arm.body);
		self.scopes.pop();
	}

	// attributes are not searched, only doc comments (see `doc_comments`)
	fn visit_attribute(&mut self, _: &'ast Attribute) {}

	fn visit_block(&mut self, block: &'ast Block) {
		self.body_depth += 1;

		let items = statement_items(&block.stmts);
		let scope = self.local_items(items.iter().map(AsRef::as_ref));

		self.scopes.push(scope);

		for stmt in &block.stmts {
			self.visit_stmt(stmt);
		}

		self.scopes.pop();
		self.body_depth -= 1;
	}

	fn visit_expr_closure(&mut self, closure: &'ast ExprClosure) {
		self.scopes.push(Scope::default());

		for input in &closure.inputs {
			self.pattern(input);
		}

		self.visit_return_type(&closure.output);
		self.visit_expr(&closure.body);
		self.scopes.pop();
	}

	fn visit_expr_for_loop(&mut self, expr: &'ast ExprForLoop) {
		self.visit_expr(&expr.expr);
		self.scopes.push(Scope::default());
		self.pattern(&expr.pat);
		self.visit_block(&expr.body);
		self.scopes.pop();
	}

	fn visit_expr_if(&mut self, expr: &'ast ExprIf) {
		// variables of `if let` (chains) are visible in the rest of the condition and the `then` block
		self.scopes.push(Scope::default());
		self.visit_expr(&expr.cond);
		self.visit_block(&expr.then_branch);
		self.scopes.pop();

		if let Some((_, otherwise)) = &expr.else_branch {
			self.visit_expr(otherwise);
		}
	}

	fn visit_expr_let(&mut self, expr: &'ast ExprLet) {
		self.visit_expr(&expr.expr);
		self.pattern(&expr.pat);
	}

	fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
		self.visit_expr(&call.receiver);
		self.method_call(&call.method);

		if let Some(turbofish) = &call.turbofish {
			self.visit_angle_bracketed_generic_arguments(turbofish);
		}

		for argument in &call.args {
			self.visit_expr(argument);
		}
	}

	fn visit_expr_path(&mut self, expr: &'ast ExprPath) {
		self.code_path(expr.qself.as_ref(), &expr.path, Namespace::Value);
	}

	fn visit_expr_struct(&mut self, expr: &'ast ExprStruct) {
		self.code_path(expr.qself.as_ref(), &expr.path, Namespace::Type);

		for field in &expr.fields {
			// `Struct { name }` is `Struct { name: name }`: renaming `name` would rename the field too
			if field.colon_token.is_some() {
				self.visit_expr(&field.expr);
			}
		}

		if let Some(rest) = &expr.rest {
			self.visit_expr(rest);
		}
	}

	fn visit_expr_while(&mut self, expr: &'ast ExprWhile) {
		self.scopes.push(Scope::default());
		self.visit_expr(&expr.cond);
		self.visit_block(&expr.body);
		self.scopes.pop();
	}

	fn visit_field(&mut self, field: &'ast Field) {
		self.doc_comments(&field.attrs, DocStyle::Outer);
		visit::visit_field(self, field);
	}

	fn visit_file(&mut self, file: &'ast syn::File) {
		self.doc_comments(&file.attrs, DocStyle::Inner);

		for item in &file.items {
			self.visit_item(item);
		}
	}

	fn visit_foreign_item(&mut self, item: &'ast ForeignItem) {
		if let Some(attrs) = foreign_item_attrs(item) {
			self.doc_comments(attrs, DocStyle::Outer);
		}

		visit::visit_foreign_item(self, item);
	}

	fn visit_foreign_item_fn(&mut self, item: &'ast ForeignItemFn) {
		self.visit_visibility(&item.vis);
		self.function(&item.sig, None);
	}

	fn visit_foreign_item_static(&mut self, item: &'ast ForeignItemStatic) {
		self.item_scope(None, |this| visit::visit_foreign_item_static(this, item));
	}

	fn visit_foreign_item_type(&mut self, item: &'ast ForeignItemType) {
		self.item_scope(Some(&item.generics), |this| visit::visit_foreign_item_type(this, item));
	}

	fn visit_generic_argument(&mut self, argument: &'ast GenericArgument) {
		if let GenericArgument::Type(Type::Path(ty)) = argument
			&& ty.qself.is_none()
			&& let Some(ident) = ty.path.get_ident()
			&& self.targets.named(ident).is_some()
		{
			self.type_or_const_argument(ident);
			return;
		}

		visit::visit_generic_argument(self, argument);
	}

	fn visit_impl_item(&mut self, item: &'ast ImplItem) {
		if let Some(attrs) = impl_item_attrs(item) {
			self.doc_comments(attrs, DocStyle::Outer);
		}

		visit::visit_impl_item(self, item);
	}

	fn visit_impl_item_const(&mut self, item: &'ast ImplItemConst) {
		self.item_scope(Some(&item.generics), |this| visit::visit_impl_item_const(this, item));
	}

	fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
		self.visit_visibility(&item.vis);
		self.function(&item.sig, Some(&item.block));
	}

	fn visit_impl_item_type(&mut self, item: &'ast ImplItemType) {
		self.item_scope(Some(&item.generics), |this| visit::visit_impl_item_type(this, item));
	}

	fn visit_item(&mut self, item: &'ast Item) {
		// items that define what `Self` (or the module) is handle their doc comments themselves
		let defines_scope = matches!(
			item,
			Item::Struct(_) | Item::Enum(_) | Item::Union(_) | Item::Trait(_) | Item::Impl(_) | Item::Mod(_)
		);

		if !defines_scope && let Some(attrs) = item_attrs(item) {
			self.doc_comments(attrs, DocStyle::Outer);
		}

		visit::visit_item(self, item);
	}

	fn visit_item_const(&mut self, item: &'ast ItemConst) {
		self.item_scope(Some(&item.generics), |this| visit::visit_item_const(this, item));
	}

	fn visit_item_enum(&mut self, item: &'ast ItemEnum) {
		let self_types = SelfTypes {
			types: self.loaded_item(&item.ident, ItemKind::Enum).into_iter().collect(),
			traits: Vec::new(),
		};

		self.with_self(self_types, |this| {
			this.doc_comments(&item.attrs, DocStyle::Outer);
			this.item_scope(Some(&item.generics), |this| visit::visit_item_enum(this, item));
		});
	}

	fn visit_item_fn(&mut self, item: &'ast ItemFn) {
		self.visit_visibility(&item.vis);
		self.function(&item.sig, Some(&item.block));
	}

	fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
		self.item_scope(Some(&item.generics), |this| {
			let (self_types, loaded) = this.impl_self_types(item);
			let traits = self_types.traits.clone();

			this.with_self(self_types, |this| {
				this.doc_comments(&item.attrs, DocStyle::Outer);
				this.visit_generics(&item.generics);

				if let Some((path, _)) = &item.trait_ {
					this.code_path(None, path, Namespace::Type);
				}

				this.visit_type(&item.self_ty);

				if !loaded {
					this.implementing_items(item, &traits);
				}

				for member in &item.items {
					this.visit_impl_item(member);
				}
			});
		});
	}

	fn visit_item_macro(&mut self, item: &'ast ItemMacro) {
		match &item.ident {
			Some(name) if item.mac.path.is_ident("macro_rules") => self.macro_rules(name, &item.mac),
			_ => self.visit_macro(&item.mac),
		}
	}

	fn visit_item_mod(&mut self, item: &'ast ItemMod) {
		self.doc_comments(&item.attrs, DocStyle::Outer);
		self.visit_visibility(&item.vis);

		let Some((_, items)) = &item.content else {
			return;
		};

		let loaded =
			(self.loaded_item(&item.ident, ItemKind::Module)).filter(|&module| self.ws.item(module).module_info().is_some_and(|info| info.inline));

		match loaded {
			Some(module) => {
				let outer = std::mem::replace(&mut self.module, module);

				self.doc_comments(&item.attrs, DocStyle::Inner);

				for item in items {
					self.visit_item(item);
				}

				self.module = outer;
			}

			// a module inside of a body, whose items are only known here
			None => {
				self.body_depth += 1;
				self.unloaded_modules += 1;

				let mut scope = self.local_items(items.iter());

				scope.barrier = true;
				self.scopes.push(scope);

				self.with_self(SelfTypes::default(), |this| {
					for item in items {
						this.visit_item(item);
					}
				});

				self.scopes.pop();
				self.unloaded_modules -= 1;
				self.body_depth -= 1;
			}
		}
	}

	fn visit_item_static(&mut self, item: &'ast ItemStatic) {
		self.item_scope(None, |this| visit::visit_item_static(this, item));
	}

	fn visit_item_struct(&mut self, item: &'ast ItemStruct) {
		let self_types = SelfTypes {
			types: self.loaded_item(&item.ident, ItemKind::Struct).into_iter().collect(),
			traits: Vec::new(),
		};

		self.with_self(self_types, |this| {
			this.doc_comments(&item.attrs, DocStyle::Outer);
			this.item_scope(Some(&item.generics), |this| visit::visit_item_struct(this, item));
		});
	}

	fn visit_item_trait(&mut self, item: &'ast ItemTrait) {
		let self_types = SelfTypes {
			types: Vec::new(),
			traits: self.loaded_item(&item.ident, ItemKind::Trait).into_iter().collect(),
		};

		self.with_self(self_types, |this| {
			this.doc_comments(&item.attrs, DocStyle::Outer);
			this.item_scope(Some(&item.generics), |this| visit::visit_item_trait(this, item));
		});
	}

	fn visit_item_trait_alias(&mut self, item: &'ast ItemTraitAlias) {
		self.item_scope(Some(&item.generics), |this| visit::visit_item_trait_alias(this, item));
	}

	fn visit_item_type(&mut self, item: &'ast ItemType) {
		self.item_scope(Some(&item.generics), |this| visit::visit_item_type(this, item));
	}

	fn visit_item_union(&mut self, item: &'ast ItemUnion) {
		let self_types = SelfTypes {
			types: self.loaded_item(&item.ident, ItemKind::Union).into_iter().collect(),
			traits: Vec::new(),
		};

		self.with_self(self_types, |this| {
			this.doc_comments(&item.attrs, DocStyle::Outer);
			this.item_scope(Some(&item.generics), |this| visit::visit_item_union(this, item));
		});
	}

	fn visit_item_use(&mut self, item: &'ast ItemUse) {
		self.visit_visibility(&item.vis);

		for leaf in use_leaves(&item.tree) {
			self.import(&leaf, item.leading_colon.is_some());
		}
	}

	fn visit_local(&mut self, local: &'ast Local) {
		if let Some(init) = &local.init {
			self.visit_expr(&init.expr);

			if let Some((_, diverge)) = &init.diverge {
				self.visit_expr(diverge);
			}
		}

		// the variables are visible after the statement
		self.pattern(&local.pat);
	}

	fn visit_macro(&mut self, mac: &'ast Macro) {
		self.macro_call(mac);
	}

	fn visit_pat(&mut self, pattern: &'ast Pat) {
		self.pattern(pattern);
	}

	// paths are resolved by the nodes containing them, which know their namespace
	fn visit_path(&mut self, path: &'ast syn::Path) {
		for segment in &path.segments {
			self.visit_path_arguments(&segment.arguments);
		}
	}

	// syntax that syn does not model
	fn visit_token_stream(&mut self, tokens: &'ast TokenStream) {
		self.verbatim(tokens);
	}

	fn visit_trait_bound(&mut self, bound: &'ast TraitBound) {
		self.code_path(None, &bound.path, Namespace::Type);
	}

	fn visit_trait_item(&mut self, item: &'ast TraitItem) {
		if let Some(attrs) = trait_item_attrs(item) {
			self.doc_comments(attrs, DocStyle::Outer);
		}

		visit::visit_trait_item(self, item);
	}

	fn visit_trait_item_const(&mut self, item: &'ast TraitItemConst) {
		self.item_scope(Some(&item.generics), |this| visit::visit_trait_item_const(this, item));
	}

	fn visit_trait_item_fn(&mut self, item: &'ast TraitItemFn) {
		self.function(&item.sig, item.default.as_ref());
	}

	fn visit_trait_item_type(&mut self, item: &'ast TraitItemType) {
		self.item_scope(Some(&item.generics), |this| visit::visit_trait_item_type(this, item));
	}

	fn visit_type_path(&mut self, ty: &'ast TypePath) {
		self.code_path(ty.qself.as_ref(), &ty.path, Namespace::Type);
	}

	fn visit_variant(&mut self, variant: &'ast Variant) {
		self.doc_comments(&variant.attrs, DocStyle::Outer);
		visit::visit_variant(self, variant);
	}

	fn visit_vis_restricted(&mut self, vis: &'ast VisRestricted) {
		self.code_path(None, &vis.path, Namespace::Type);
	}
}

pub(super) enum LeafKind<'t> {
	/// `a::Name`
	Name,

	/// `a::Name as Alias`
	Rename(&'t Ident),

	/// `a::{self}`, `a::{self as Alias}`
	SelfImport(Option<&'t Ident>),

	/// `a::*`
	Glob,
}

/// What `Self` refers to.
#[derive(Debug, Default, Clone)]
pub(super) struct SelfTypes {
	/// The loaded types `Self` names: the self types of an `impl` block, or the type being defined.
	pub(super) types: Vec<ItemId>,

	/// Traits whose items `Self::item` may name: the trait being defined, or the traits an `impl` block implements.
	pub(super) traits: Vec<ItemId>,
}

/// A leaf of a `use` tree.
pub(super) struct UseLeaf<'t> {
	/// The imported path: of the imported item, of the module of a `self` import, or of what a glob imports from.
	pub(super) path: Vec<&'t Ident>,

	pub(super) kind: LeafKind<'t>,
}

impl UseLeaf<'_> {
	/// The name the leaf binds (none for globs and `_` imports).
	fn binding(&self) -> Option<SmolStr> {
		let ident = match self.kind {
			LeafKind::Name | LeafKind::SelfImport(None) => self.path.last()?,
			LeafKind::Rename(alias) | LeafKind::SelfImport(Some(alias)) => alias,
			LeafKind::Glob => return None,
		};

		Some(ident_name(ident)).filter(|name| name != "_")
	}
}

fn foreign_item_attrs(item: &ForeignItem) -> Option<&[Attribute]> {
	let attrs = match item {
		ForeignItem::Fn(item) => &item.attrs,
		ForeignItem::Static(item) => &item.attrs,
		ForeignItem::Type(item) => &item.attrs,
		ForeignItem::Macro(item) => &item.attrs,
		_ => return None,
	};

	Some(attrs)
}

fn impl_item_attrs(item: &ImplItem) -> Option<&[Attribute]> {
	let attrs = match item {
		ImplItem::Const(item) => &item.attrs,
		ImplItem::Fn(item) => &item.attrs,
		ImplItem::Type(item) => &item.attrs,
		ImplItem::Macro(item) => &item.attrs,
		_ => return None,
	};

	Some(attrs)
}

/// Whether a path segment is a keyword with a special meaning, which never names an item.
pub(super) fn is_path_keyword(name: &str) -> bool {
	matches!(name, "crate" | "self" | "super" | "Self" | "$crate")
}

fn item_attrs(item: &Item) -> Option<&[Attribute]> {
	let attrs = match item {
		Item::Const(item) => &item.attrs,
		Item::Enum(item) => &item.attrs,
		Item::ExternCrate(item) => &item.attrs,
		Item::Fn(item) => &item.attrs,
		Item::ForeignMod(item) => &item.attrs,
		Item::Impl(item) => &item.attrs,
		Item::Macro(item) => &item.attrs,
		Item::Mod(item) => &item.attrs,
		Item::Static(item) => &item.attrs,
		Item::Struct(item) => &item.attrs,
		Item::Trait(item) => &item.attrs,
		Item::TraitAlias(item) => &item.attrs,
		Item::Type(item) => &item.attrs,
		Item::Union(item) => &item.attrs,
		Item::Use(item) => &item.attrs,
		_ => return None,
	};

	Some(attrs)
}

/// The members of an enum or module defined in a block (for glob imports of it).
fn local_item_members(item: &Item) -> Option<(SmolStr, Vec<(SmolStr, Namespace)>)> {
	match item {
		Item::Enum(item) => {
			let variants = item.variants.iter().flat_map(|variant| {
				let namespaces: &[Namespace] = match variant.fields {
					Fields::Named(_) => &[Namespace::Type],
					_ => &[Namespace::Type, Namespace::Value],
				};

				namespaces.iter().map(|&namespace| (ident_name(&variant.ident), namespace))
			});

			Some((ident_name(&item.ident), variants.collect()))
		}

		Item::Mod(item) => item
			.content
			.as_ref()
			.map(|(_, items)| (ident_name(&item.ident), items.iter().flat_map(local_item_names).collect())),

		_ => None,
	}
}

/// The names an item of a block binds, with their namespaces.
fn local_item_names(item: &Item) -> Vec<(SmolStr, Namespace)> {
	use Namespace::Macro;
	use Namespace::Type;
	use Namespace::Value;

	let named = |ident: &Ident, namespaces: &[Namespace]| namespaces.iter().map(|&namespace| (ident_name(ident), namespace)).collect::<Vec<_>>();

	match item {
		Item::Const(item) => named(&item.ident, &[Value]),
		Item::Static(item) => named(&item.ident, &[Value]),
		Item::Fn(item) => named(&item.sig.ident, &[Value]),
		Item::Struct(item) if matches!(item.fields, Fields::Named(_)) => named(&item.ident, &[Type]),
		Item::Struct(item) => named(&item.ident, &[Type, Value]),
		Item::Enum(item) => named(&item.ident, &[Type]),
		Item::Union(item) => named(&item.ident, &[Type]),
		Item::Trait(item) => named(&item.ident, &[Type]),
		Item::TraitAlias(item) => named(&item.ident, &[Type]),
		Item::Type(item) => named(&item.ident, &[Type]),
		Item::Mod(item) => named(&item.ident, &[Type]),
		Item::ExternCrate(item) => named(item.rename.as_ref().map_or(&item.ident, |(_, alias)| alias), &[Type]),

		Item::Macro(item) => match &item.ident {
			Some(ident) => named(ident, &[Macro]),

			// the statics a `thread_local!` declares
			None => (thread_local::declarations(&item.mac).into_iter().flatten())
				.map(|declaration| (ident_name(&declaration.ident), Value))
				.collect(),
		},

		Item::ForeignMod(block) => (block.items.iter())
			.flat_map(|item| match item {
				ForeignItem::Fn(item) => named(&item.sig.ident, &[Value]),
				ForeignItem::Static(item) => named(&item.ident, &[Value]),
				ForeignItem::Type(item) => named(&item.ident, &[Type]),
				_ => Vec::new(),
			})
			.collect(),

		_ => Vec::new(),
	}
}

/// The items of statements, for the scope of their block: item statements, and `thread_local!` invocations in
/// statement position (which declare statics).
pub(super) fn statement_items(stmts: &[Stmt]) -> Vec<Cow<'_, Item>> {
	(stmts.iter())
		.filter_map(|stmt| match stmt {
			Stmt::Item(item) => Some(Cow::Borrowed(item)),

			Stmt::Macro(statement) if thread_local::is_thread_local(&statement.mac) => Some(Cow::Owned(Item::Macro(ItemMacro {
				attrs: statement.attrs.clone(),
				ident: None,
				mac: statement.mac.clone(),
				semi_token: statement.semi_token,
			}))),

			_ => None,
		})
		.collect()
}

fn trait_item_attrs(item: &TraitItem) -> Option<&[Attribute]> {
	let attrs = match item {
		TraitItem::Const(item) => &item.attrs,
		TraitItem::Fn(item) => &item.attrs,
		TraitItem::Type(item) => &item.attrs,
		TraitItem::Macro(item) => &item.attrs,
		_ => return None,
	};

	Some(attrs)
}

/// The leaves of a `use` tree.
pub(super) fn use_leaves(tree: &UseTree) -> Vec<UseLeaf<'_>> {
	fn collect<'t>(tree: &'t UseTree, prefix: &mut Vec<&'t Ident>, leaves: &mut Vec<UseLeaf<'t>>) {
		let leaf = |path: Vec<&'t Ident>, kind| UseLeaf { path, kind };

		match tree {
			UseTree::Path(path) => {
				prefix.push(&path.ident);
				collect(&path.tree, prefix, leaves);
				prefix.pop();
			}

			UseTree::Name(name) if name.ident == "self" => leaves.push(leaf(prefix.clone(), LeafKind::SelfImport(None))),
			UseTree::Name(name) => leaves.push(leaf([&prefix[..], &[&name.ident]].concat(), LeafKind::Name)),
			UseTree::Rename(rename) if rename.ident == "self" => leaves.push(leaf(prefix.clone(), LeafKind::SelfImport(Some(&rename.rename)))),
			UseTree::Rename(rename) => leaves.push(leaf([&prefix[..], &[&rename.ident]].concat(), LeafKind::Rename(&rename.rename))),
			UseTree::Glob(_) => leaves.push(leaf(prefix.clone(), LeafKind::Glob)),

			UseTree::Group(group) => {
				for tree in &group.items {
					collect(tree, prefix, leaves);
				}
			}
		}
	}

	let mut leaves = Vec::new();

	collect(tree, &mut Vec::new(), &mut leaves);
	leaves
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn flattens_use_trees() {
		let found = leaves("use a::{b::{self, C as D}, e::*, F, r#type as _};");
		let expected = [
			("a::b", "self", Some("b")),
			("a::b::C", "rename", Some("D")),
			("a::e", "glob", None),
			("a::F", "name", Some("F")),
			("a::r#type", "rename", None),
		];

		assert_eq!(found.len(), expected.len());

		for (found, expected) in found.iter().zip(expected) {
			assert_eq!((found.0.as_str(), found.1, found.2.as_deref()), expected);
		}

		assert_eq!(leaves("use x::{self as y};")[0].2.as_deref(), Some("y"));
	}

	fn leaves(source: &str) -> Vec<(String, &'static str, Option<String>)> {
		let item: ItemUse = syn::parse_str(source).unwrap();

		use_leaves(&item.tree)
			.into_iter()
			.map(|leaf| {
				let path = leaf.path.iter().map(ToString::to_string).collect::<Vec<_>>().join("::");

				let kind = match leaf.kind {
					LeafKind::Name => "name",
					LeafKind::Rename(_) => "rename",
					LeafKind::SelfImport(_) => "self",
					LeafKind::Glob => "glob",
				};

				(path, kind, leaf.binding().map(String::from))
			})
			.collect()
	}

	#[test]
	fn names_of_local_items() {
		let names = |source: &str| {
			let item: Item = syn::parse_str(source).unwrap();

			local_item_names(&item)
				.into_iter()
				.map(|(name, namespace)| format!("{name}:{namespace:?}"))
				.collect::<Vec<_>>()
		};

		assert_eq!(names("struct S { a: u8 }"), ["S:Type"]);
		assert_eq!(names("struct S(u8);"), ["S:Type", "S:Value"]);
		assert_eq!(names("fn r#type() {}"), ["type:Value"]);
		assert_eq!(names("macro_rules! m { () => {} }"), ["m:Macro"]);
		assert_eq!(names("extern crate a as b;"), ["b:Type"]);
		assert_eq!(names("extern \"C\" { fn f(); static S: u8; type T; }"), ["f:Value", "S:Value", "T:Type"]);
		assert!(names("impl S {}").is_empty());

		let item: Item = syn::parse_str("enum E { A, B { x: u8 }, C(u8) }").unwrap();
		let (name, members) = local_item_members(&item).unwrap();

		assert_eq!(name, "E");
		assert_eq!(members.len(), 5);
	}
}
