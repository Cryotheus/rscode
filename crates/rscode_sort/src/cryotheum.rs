// contents of expressions, function bodies, contents of macro_rules, and what not obviously should not be sorted
// this may need to extend to the ordering of declarative macro definition and invocation, but I won't think about it right now

/// Items inside a module.
#[derive(Debug)]
enum OrderingSchema {
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

	MacroInvocation,
}

/// Items inside an `impl` block or `trait` definition
#[derive(Debug)]
enum OrderingSchemaAssociated {
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

	MacroInvocation,
}

/// Items inside a `extern "C"` block
#[derive(Debug)]
enum OrderingSchemaForeign {
	// Visibility tokens don't impact ordering
	Static,
	// Visibility, `const`, `unsafe` tokens don't impact ordering
	Fn,
	MacroInvocation,
}

/// `mod` items inside a file or inline-module.
#[derive(Debug)]
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
