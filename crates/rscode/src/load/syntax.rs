//! Converting syntax to the model's plain data (ranges, names, paths, types, and readable token text).

use super::Walker;
use crate::model::DataShape;
use crate::model::FnInfo;
use crate::model::ItemDetail;
use crate::model::PathRef;
use crate::model::PathSegmentRef;
use crate::model::Receiver;
use crate::model::TypeRef;
use crate::model::Visibility;
use crate::source::ParsedFile;
use crate::source::TextRange;
use proc_macro2::Delimiter;
use proc_macro2::Ident;
use proc_macro2::Span;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use proc_macro2::extra::DelimSpan;
use quote::ToTokens;
use smol_str::SmolStr;
use syn::Block;
use syn::Fields;
use syn::ReceiverKind;
use syn::Safety;
use syn::Signature;
use syn::Type;

/// Where an item's text is.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub(super) struct Extent {
	/// From the start of the first token to the end of the last token.
	pub(super) range: TextRange,

	/// The start of the first token after the outer attributes.
	pub(super) after_attrs: usize,
}

impl Walker<'_, '_, '_> {
	pub(super) fn range(&self, span: Span) -> TextRange {
		self.parsed.range(span)
	}

	/// The range of a syntax node, from its first to its last token.
	pub(super) fn range_of(&self, node: &dyn ToTokens) -> TextRange {
		self.extent(node, 0).range
	}

	/// The extent of an item whose tokens start with `outer_attrs` outer attributes.
	///
	/// syn prints the outer attributes of an item first (inner attributes are printed inside of its braces), and
	/// every attribute (doc comments included) as two token trees: `#` and `[...]`.
	pub(super) fn extent(&self, node: &dyn ToTokens, outer_attrs: usize) -> Extent {
		let mut tokens = node.to_token_stream().into_iter();

		let Some(first) = tokens.next() else {
			return Extent::default();
		};

		let first = self.range(first.span());

		let after = match outer_attrs {
			0 => Some(first),
			count => tokens.nth(2 * count - 1).map(|token| self.range(token.span())),
		};

		let last = tokens.last().map(|token| self.range(token.span())).or(after).unwrap_or(first);

		Extent {
			range: first.cover(last),
			after_attrs: after.map_or(last.end, |after| after.start),
		}
	}

	/// The range strictly inside of a pair of delimiters.
	pub(super) fn inside(&self, span: &DelimSpan) -> TextRange {
		let open = self.range(span.open());
		let close = self.range(span.close());

		TextRange::new(open.end, close.start.max(open.end))
	}

	pub(super) fn visibility(&self, vis: &syn::Visibility, default: Visibility) -> Visibility {
		match vis {
			syn::Visibility::Public(_) => Visibility::Public,
			syn::Visibility::Restricted(restricted) if restricted.in_token.is_some() => Visibility::InPath(self.path_ref(&restricted.path)),
			syn::Visibility::Restricted(restricted) if restricted.path.is_ident("crate") => Visibility::Crate,
			syn::Visibility::Restricted(restricted) if restricted.path.is_ident("super") => Visibility::Super,
			syn::Visibility::Restricted(restricted) if restricted.path.is_ident("self") => Visibility::SelfModule,
			syn::Visibility::Restricted(restricted) => Visibility::InPath(self.path_ref(&restricted.path)),
			syn::Visibility::Inherited => default,
		}
	}

	pub(super) fn path_ref(&self, path: &syn::Path) -> PathRef {
		PathRef {
			leading_colon: path.leading_colon.is_some(),
			segments: path
				.segments
				.iter()
				.map(|segment| PathSegmentRef {
					has_arguments: !segment.arguments.is_none(),
					..self.segment(&segment.ident)
				})
				.collect(),
		}
	}

	/// A path segment without generic arguments.
	pub(super) fn segment(&self, ident: &Ident) -> PathSegmentRef {
		PathSegmentRef {
			name: ident_name(ident),
			range: self.range(ident.span()),
			has_arguments: false,
		}
	}

	pub(super) fn type_ref(&self, ty: &Type) -> TypeRef {
		let inner = match ty {
			Type::Path(path) if path.qself.is_none() => return TypeRef::Path { path: self.path_ref(&path.path) },
			Type::Reference(reference) => &reference.elem,
			Type::Ptr(pointer) => &pointer.elem,
			Type::Paren(paren) => &paren.elem,
			Type::Group(group) => &group.elem,
			Type::Slice(slice) => &slice.elem,
			Type::Array(array) => &array.elem,
			_ => return TypeRef::Other,
		};

		TypeRef::Indirect {
			inner: Box::new(self.type_ref(inner)),
		}
	}

	pub(super) fn fn_info(&self, signature: &Signature, body: Option<&Block>) -> FnInfo {
		FnInfo {
			receiver: signature.receiver().map(receiver),
			is_const: signature.constness.is_some(),
			is_async: signature.asyncness.is_some(),
			is_unsafe: matches!(signature.safety, Safety::Unsafe(_)),
			body: body.map(|body| self.range(body.brace_token.span.join())),
		}
	}

	/// Details of a struct or a variant.
	pub(super) fn fields_detail(&self, fields: &Fields) -> ItemDetail {
		let (shape, body) = match fields {
			Fields::Named(fields) => (DataShape::Named, Some(self.inside(&fields.brace_token.span))),
			Fields::Unnamed(fields) => (DataShape::Tuple, Some(self.inside(&fields.paren_token.span))),
			Fields::Unit => (DataShape::Unit, None),
		};

		ItemDetail::Data { shape, body }
	}

	pub(super) fn compact_text(&self, tokens: TokenStream) -> String {
		compact_text(self.parsed, tokens)
	}
}

/// The name of an identifier, without `r#`.
pub(super) fn ident_name(ident: &Ident) -> SmolStr {
	let text = ident.to_string();

	SmolStr::new(text.strip_prefix("r#").unwrap_or(&text))
}

pub(super) fn receiver(receiver: &syn::Receiver) -> Receiver {
	let (reference, mutable, typed) = match &receiver.kind {
		ReceiverKind::Reference(_, _, mutability) => (true, mutability.is_some(), false),
		ReceiverKind::Typed(..) => (false, receiver.mutability.is_some(), true),
		_ => (false, receiver.mutability.is_some(), false),
	};

	Receiver { reference, mutable, typed }
}

/// The source text of tokens, compacted to one line.
///
/// Whitespace and comments between two tokens become a single space, except that there is no space around `<`,
/// `>` (unless part of `->` or `=>`), and `::`, just inside of delimiters, or before `,` and `;`; and there is always
/// a space after `,`. For example `Foo < T >`, `fmt :: Display`, and `HashMap<K,V>` become `Foo<T>`,
/// `fmt::Display`, and `HashMap<K, V>`.
pub(super) fn compact_text(parsed: &ParsedFile, tokens: TokenStream) -> String {
	let mut pieces = Vec::new();

	flatten(parsed, tokens, &mut pieces);

	let mut text = String::new();

	for index in 0..pieces.len() {
		if index > 0 && needs_space(&pieces, index) {
			text.push(' ');
		}

		text.push_str(&pieces[index].text);
	}

	text
}

/// One token, or one delimiter of a group.
struct Piece {
	text: String,
	range: TextRange,
	kind: PieceKind,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum PieceKind {
	Open,
	Close,
	Punct(char),
	Word,
}

fn flatten(parsed: &ParsedFile, tokens: TokenStream, pieces: &mut Vec<Piece>) {
	for token in tokens {
		match token {
			TokenTree::Group(group) => {
				let (open, close) = match group.delimiter() {
					Delimiter::Parenthesis => ("(", ")"),
					Delimiter::Brace => ("{", "}"),
					Delimiter::Bracket => ("[", "]"),
					Delimiter::None => {
						flatten(parsed, group.stream(), pieces);
						continue;
					}
				};

				pieces.push(Piece {
					text: open.to_owned(),
					range: parsed.range(group.span_open()),
					kind: PieceKind::Open,
				});

				flatten(parsed, group.stream(), pieces);

				pieces.push(Piece {
					text: close.to_owned(),
					range: parsed.range(group.span_close()),
					kind: PieceKind::Close,
				});
			}

			TokenTree::Punct(punct) => pieces.push(Piece {
				text: punct.as_char().to_string(),
				range: parsed.range(punct.span()),
				kind: PieceKind::Punct(punct.as_char()),
			}),

			other => pieces.push(Piece {
				text: other.to_string(),
				range: parsed.range(other.span()),
				kind: PieceKind::Word,
			}),
		}
	}
}

fn needs_space(pieces: &[Piece], index: usize) -> bool {
	let previous = &pieces[index - 1];
	let current = &pieces[index];

	match (previous.kind, current.kind) {
		(_, PieceKind::Punct(',' | ';')) | (PieceKind::Punct(','), PieceKind::Close) => false,
		(PieceKind::Punct(','), _) => true,
		(PieceKind::Open | PieceKind::Punct('<'), _) | (_, PieceKind::Close) => false,

		// generic arguments (`Vec<T>`, `for<'a>`, `::<T>`), but not qualified paths (`&mut <T as Tr>::A`)
		(PieceKind::Word | PieceKind::Punct(':'), PieceKind::Punct('<')) if !matches!(previous.text.as_str(), "mut" | "const" | "dyn") => {
			false
		}

		_ => {
			let separated = previous.range.end < current.range.start;

			separated && !is_angle_close(pieces, index) && !is_path_colon(pieces, index) && !is_path_colon(pieces, index - 1)
		}
	}
}

fn adjacent(first: &Piece, second: &Piece) -> bool {
	first.range.end == second.range.start
}

/// Whether a piece is a `>` closing generic arguments (rather than part of `->` or `=>`).
fn is_angle_close(pieces: &[Piece], index: usize) -> bool {
	let is_arrow = index > 0
		&& matches!(pieces[index - 1].kind, PieceKind::Punct('-' | '='))
		&& adjacent(&pieces[index - 1], &pieces[index]);

	pieces[index].kind == PieceKind::Punct('>') && !is_arrow
}

/// Whether a piece is one of the two colons of `::`.
fn is_path_colon(pieces: &[Piece], index: usize) -> bool {
	let is_colon = |piece: &Piece| piece.kind == PieceKind::Punct(':');
	let piece = &pieces[index];
	let with_previous = index > 0 && is_colon(&pieces[index - 1]) && adjacent(&pieces[index - 1], piece);
	let with_next = pieces.get(index + 1).is_some_and(|next| is_colon(next) && adjacent(piece, next));

	is_colon(piece) && (with_previous || with_next)
}

#[cfg(test)]
mod tests {
	use super::*;
	use syn::Item;

	/// The compact text of the self type and trait of the first item, an `impl` block.
	fn impl_texts(source: &str) -> (String, Option<String>) {
		let parsed = ParsedFile::parse(source).unwrap();

		let Item::Impl(item) = &parsed.file.items[0] else {
			panic!("not an impl");
		};

		let self_ty = compact_text(&parsed, item.self_ty.to_token_stream());
		let trait_ = item.trait_.as_ref().map(|(path, _)| compact_text(&parsed, path.to_token_stream()));

		(self_ty, trait_)
	}

	#[test]
	fn compacts_token_text() {
		assert_eq!(impl_texts("impl Foo < T > {}").0, "Foo<T>");
		assert_eq!(impl_texts("impl<T> fmt :: Display for Vec<Vec<u8> > {}"), ("Vec<Vec<u8>>".to_owned(), Some("fmt::Display".to_owned())));
		assert_eq!(impl_texts("impl HashMap<K,V> {}").0, "HashMap<K, V>");
		assert_eq!(impl_texts("impl<'a> Tr for &'a mut [u8; 4] {}").0, "&'a mut [u8; 4]");
		assert_eq!(impl_texts("impl Tr for dyn Fn( u8 ) -> Box< dyn Error+Send > {}").0, "dyn Fn(u8) -> Box<dyn Error+Send>");
		assert_eq!(impl_texts("impl Tr for (A,B,) {}").0, "(A, B,)");
		assert_eq!(impl_texts("impl ::std::ops::Add<Output = u8> for S {}").1.unwrap(), "::std::ops::Add<Output = u8>");
		assert_eq!(impl_texts("impl Tr for <T as\n\tIterator /* c */ >::Item {}").0, "<T as Iterator>::Item");
		assert_eq!(impl_texts("impl Tr for r#type {}").0, "r#type");
		assert_eq!(impl_texts("impl Tr for S<{ N + 1 }> {}").0, "S<{N + 1}>");
		assert_eq!(impl_texts("impl Tr for &mut <T as Tr>::A {}").0, "&mut <T as Tr>::A");
		assert_eq!(impl_texts("impl Tr for fn() -> <T as Tr>::A {}").0, "fn() -> <T as Tr>::A");
		assert_eq!(impl_texts("impl Tr for dyn for <'a> Fn(&'a u8) {}").0, "dyn for<'a> Fn(&'a u8)");
	}

	#[test]
	fn strips_raw_prefixes() {
		let ident: Ident = syn::parse_str("r#type").unwrap();

		assert_eq!(ident_name(&ident), "type");
		assert_eq!(ident_name(&syn::parse_str::<Ident>("plain").unwrap()), "plain");
	}
}
