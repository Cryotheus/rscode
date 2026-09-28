//! Sorting is independent of the original order of items, like the orders in which `bindgen` may emit them.

mod common;

use common::Rng;
use common::assert_stable;
use common::file_tokens;
use proc_macro2::TokenStream;

/// The items of a `bindgen`-like file, each as a chunk of text.
fn bindgen_items() -> Vec<String> {
	let mut items = Vec::new();

	for index in [0, 1, 2, 3, 8, 10, 16, 21] {
		items.push(format!("pub const LIB_CONSTANT_{index}: u32 = {index};"));
		items.push(format!("pub type lib_handle_{index} = *mut ::std::os::raw::c_void;"));
		items.push(format!(
			"#[doc = \" A struct numbered {index}.\"]\n#[repr(C)]\n#[derive(Debug, Copy, Clone)]\npub struct lib_struct_{index} {{\n    pub \
			 value: u32,\n    pub next: *mut lib_struct_{index},\n}}"
		));
		items.push(format!(
			"impl Default for lib_struct_{index} {{\n    fn default() -> Self {{\n        let mut s = \
			 ::std::mem::MaybeUninit::<Self>::uninit();\n        unsafe {{\n            ::std::ptr::write_bytes(s.as_mut_ptr(), 0, \
			 1);\n            s.assume_init()\n        }}\n    }}\n}}"
		));
		items.push(format!(
			"unsafe extern \"C\" {{\n    #[doc = \" Creates object {index}.\"]\n    pub fn lib_create_{index}(value: u32) -> \
			 *mut lib_struct_{index};\n}}"
		));
		items.push(format!("unsafe extern \"C\" {{\n    pub static mut lib_global_{index}: u32;\n}}"));
		items.push(format!(
			"#[test]\nfn bindgen_test_layout_lib_struct_{index}() {{\n    assert_eq!(::std::mem::size_of::<lib_struct_{index}>(), \
			 16usize);\n}}"
		));
	}

	items.push("#[repr(u32)]\n#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]\npub enum lib_kind {\n    B = 1,\n    A = 0,\n}".to_owned());
	items.push("#[link(name = \"lib\")]\nunsafe extern \"C\" {\n    pub fn lib_linked();\n}".to_owned());
	items.push("unsafe extern \"system\" {\n    pub fn lib_system();\n}".to_owned());
	items
}

/// Deterministic permutations of the items, joined as `bindgen` would (one item per line group).
fn permutations(items: &[String], count: usize) -> Vec<String> {
	let mut rng = Rng::new(0x00b1_d6e2);
	let mut sources = Vec::new();
	let mut reversed = items.to_vec();

	reversed.reverse();
	sources.push(items.join("\n") + "\n");
	sources.push(reversed.join("\n\n") + "\n");

	for _ in 0..count {
		let mut shuffled = items.to_vec();

		rng.shuffle(&mut shuffled);
		sources.push(shuffled.join("\n") + "\n");
	}

	sources
}

#[test]
fn text_engine_is_order_independent() {
	let sources = permutations(&bindgen_items(), 24);
	let expected = rscode_sort::sort_str(&sources[0]).unwrap();

	assert_stable(&expected);

	for source in &sources[1..] {
		assert_eq!(rscode_sort::sort_str(source).unwrap(), expected);
	}

	// all `unsafe extern "C"` blocks without attributes merged into one
	assert_eq!(expected.matches("unsafe extern \"C\" {").count(), 2);
	assert_eq!(expected.matches("extern").count(), 3);
}

#[test]
fn token_engine_is_order_independent() {
	let sources = permutations(&bindgen_items(), 24);
	let sort = |source: &str| rscode_sort::sort_tokens(source.parse::<TokenStream>().unwrap()).unwrap().to_string();
	let expected = sort(&sources[0]);

	for source in &sources[1..] {
		assert_eq!(sort(source), expected);
	}

	// sorting again changes nothing
	assert_eq!(sort(&expected), expected);
}

#[test]
fn engines_agree() {
	for source in permutations(&bindgen_items(), 4) {
		let text = rscode_sort::sort_str(&source).unwrap();
		let tokens = rscode_sort::sort_tokens(source.parse::<TokenStream>().unwrap()).unwrap();
		let tokens = quote::ToTokens::into_token_stream(syn::parse2::<syn::File>(tokens).unwrap()).to_string();

		assert_eq!(file_tokens(&text), tokens);
	}
}

#[test]
fn sorted_layout() {
	let expected = rscode_sort::sort_str(&permutations(&bindgen_items(), 0)[0]).unwrap();
	let lines: Vec<&str> = expected.lines().collect();

	// constants are compact and version-sorted
	let first_constant = lines.iter().position(|line| line.starts_with("pub const")).unwrap();

	assert_eq!(
		&lines[first_constant..first_constant + 8],
		[
			"pub const LIB_CONSTANT_0: u32 = 0;",
			"pub const LIB_CONSTANT_1: u32 = 1;",
			"pub const LIB_CONSTANT_2: u32 = 2;",
			"pub const LIB_CONSTANT_3: u32 = 3;",
			"pub const LIB_CONSTANT_8: u32 = 8;",
			"pub const LIB_CONSTANT_10: u32 = 10;",
			"pub const LIB_CONSTANT_16: u32 = 16;",
			"pub const LIB_CONSTANT_21: u32 = 21;",
		]
	);

	// type aliases come first, then constants, data types with their impls, extern blocks, and functions
	let position = |needle: &str| expected.find(needle).unwrap_or_else(|| panic!("{needle}"));

	assert!(position("pub type lib_handle_0") < position("pub const LIB_CONSTANT_0"));
	assert!(position("pub const LIB_CONSTANT_21") < position("pub enum lib_kind"));
	assert!(position("pub enum lib_kind") < position("pub struct lib_struct_0 "));
	assert!(position("pub struct lib_struct_0 ") < position("impl Default for lib_struct_0 "));
	assert!(position("impl Default for lib_struct_0 ") < position("pub struct lib_struct_1 "));
	assert!(position("impl Default for lib_struct_21 ") < position("unsafe extern"));
	assert!(position("pub fn lib_system") < position("fn bindgen_test_layout_lib_struct_0"));

	// merged extern block: statics, then functions
	assert!(position("pub static mut lib_global_21") < position("pub fn lib_create_0"));
	assert!(position("pub fn lib_create_8(") < position("pub fn lib_create_10("));
}
