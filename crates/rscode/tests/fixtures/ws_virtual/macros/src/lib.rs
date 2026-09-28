//! Attribute macros.

extern crate proc_macro;

use proc_macro::TokenStream;

#[proc_macro_attribute]
pub fn describe(_attr: TokenStream, item: TokenStream) -> TokenStream {
	item
}
