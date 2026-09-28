//! Every kind of item.
#![allow(dead_code)]

// A plain comment is not part of the item.
/// Named docs.
#[derive(Debug)]
pub struct Named {
	pub a: u8,
	b: u16,
}

pub(crate) struct Tuple(u8, pub u16);

struct Unit;

pub union Union {
	a: u32,
	b: f32,
}

pub enum Enum {
	A,
	B(u8),
	C { x: i8 } = 3,
	/// Variant docs.
	#[cfg(test)]
	D,
}

pub unsafe trait Trait: Clone {
	#![allow(unused)]

	const C: u8 = 1;
	type A: Clone;
	fn required(&self);
	fn provided(&mut self) {}
	trait_macro!();
}

pub auto trait Auto {}

pub trait Alias = Clone + Send;

pub type Type<X> = Vec<X>;

pub fn function() {}

pub const unsafe fn const_unsafe() {}

pub async fn asynchronous() {}

pub const CONST: u32 = 1;

const _: () = ();

pub static STATIC: u8 = 0;

static mut STATIC_MUT: u8 = 0;

macro_rules! local_macro {
	() => {};
}

#[macro_export]
macro_rules! exported_macro {
	() => {};
}

local_macro!();

some::path::call! { tokens }

extern crate alloc;

pub extern crate core as my_core;

extern crate std as _;

extern "C" {
	pub fn foreign_fn(x: i32, ...) -> i32;
	pub static FOREIGN_STATIC: u8;
	static mut FOREIGN_MUT: u8;
	pub type Opaque;
	foreign_macro!();
}

unsafe extern "C" {
	pub safe fn safe_foreign();
}

extern {
	fn bare_abi();
}

impl Named {
	#![allow(unused)]

	pub fn new() -> Self {
		todo!()
	}

	fn by_ref(&self) {}
	fn by_mut(&mut self) {}
	fn by_value(self) {}
	fn by_mut_value(mut self) {}
	fn boxed(self: Box<Self>) {}
	pub(crate) const K: u8 = 1;
	impl_macro!();
}

impl Clone for Named {
	fn clone(&self) -> Self {
		todo!()
	}
}

impl !Send for Unit {}

unsafe impl Sync for Unit {}

impl<T> Trait for &T {}

impl<'a> From<&'a str> for Tuple {
	fn from(_: &'a str) -> Self {
		todo!()
	}
}

impl crate::inner::Local for [Unit; 2] {}

pub mod inner {
	pub(super) fn sup() {}
	pub(self) fn slf() {}
	pub(in crate::inner) fn in_path() {}
	pub trait Local {}
}

use std::collections::{self, HashMap as Map, hash_map::{Entry, *}};
pub use ::core::fmt;
use self::inner::sup as _;
use crate::{Named as Renamed, Enum::*};

pub struct r#match;

pub fn ünïcödé() {}

fn no_body();

const trait ConstTrait {
	fn f();
}

const impl ConstTrait for Unit {
	fn f() {}
}

macro decl_macro($x:expr) {
	$x
}

pub impl(crate) trait Restricted {}

static NO_VALUE: u8;

const NO_VALUE_CONST: u8;

type Bounded: Clone = u8;

use {::std::fmt as fmt2, inner::slf};
pub use {inner::sup, {::std::rc, {::std::vec::Vec as V}}, crate::{Unit as U}};
