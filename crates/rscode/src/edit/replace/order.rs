//! Where new items go among the items of their container, and whether blank lines separate them from their
//! neighbors, following the Cryotheum order of `cargo rscode sort` ([`rscode_sort::ItemOrder`]).
//!
//! Sorting puts the one-line items of a compact group (`use` items, `pub use` items, `mod foo;` declarations,
//! `extern crate` items, type aliases, constants, and statics, and associated types and constants) on consecutive
//! lines, and separates groups, and items with attributes, doc comments, or comments above them, by blank lines. New
//! items that are such one-liners join siblings of their group the same way: the sibling they are placed next to,
//! and the one on its other side unless a blank line separates them (such as between groups of imports). When blank
//! lines separate every one of those siblings from the next, the new items get blank lines too: the container's own
//! layout wins.

use super::Target;
use super::parse::Container;
use crate::edit::trivia;
use crate::edit::trivia::Placement;
use crate::edit::trivia::Spacing;
use crate::model::Workspace;
use crate::source::TextRange;
use rscode_fmt::RustFmtOptions;
use rscode_sort::InsertionPoint;
use rscode_sort::ItemOrder;
use rscode_sort::StyleEdition;
use std::path::Path;
use syn::parse::ParseStream;
use syn::parse::Parser;
use syn::spanned::Spanned;

/// An item of a container, with what decides how new items next to it are laid out.
#[derive(Debug, Clone)]
struct Sibling {
	range: TextRange,
	order: ItemOrder,

	/// Whether it has a line of its own, without attributes, doc comments, or comments above it.
	plain: bool,

	/// Whether blank lines follow it.
	blank_below: bool,
}

/// The items of a container, in source order.
#[derive(Debug, Clone)]
pub(super) struct Siblings {
	items: Vec<Sibling>,
}

impl Siblings {
	/// The items of the container of `target`, with `use` items ordered for `style_edition`.
	pub(super) fn of(ws: &Workspace, target: &Target<'_>, style_edition: StyleEdition) -> Self {
		let text = target.file.text();
		let mut children: Vec<_> = ws.children(target.item).map(|child| ws.item(child)).collect();

		children.sort_by_key(|data| data.range.start);

		let ranges: Vec<TextRange> = children.iter().map(|data| data.range).collect();
		let sources: Vec<&str> = ranges.iter().map(|range| text.get(range.as_range()).unwrap_or_default()).collect();
		let orders = crate::source::isolated(|| {
			(sources.iter())
				.map(|source| order_of(source, target.container, style_edition))
				.collect::<Vec<_>>()
		});
		let layouts = trivia::layouts(text, &ranges);

		let items = (children.iter().zip(orders).zip(layouts))
			.map(|((data, order), layout)| Sibling {
				range: data.range,
				order,
				plain: layout.alone
					&& !layout.comments_above
					&& data.range.start == data.attrs.after_attrs
					&& !text.get(data.range.as_range()).unwrap_or_default().contains('\n'),
				blank_below: layout.blank_below,
			})
			.collect();

		Self { items }
	}

	fn index_of(&self, range: TextRange) -> Option<usize> {
		self.items.iter().position(|item| item.range == range)
	}

	/// Whether the item at `index` (if any) joins new items of the order `new` without a blank line, when its group is
	/// laid out compactly.
	fn joins(&self, index: Option<usize>, new: &ItemOrder) -> bool {
		index
			.and_then(|index| self.items.get(index))
			.is_some_and(|item| item.plain && new.is_compact() && item.order.same_group(new))
	}

	/// Whether the sibling at `range`, with the order `new` instead of its own, still sorts among its neighbors
	/// (where it did): `cargo rscode sort` would not move it.
	pub(super) fn keeps_order(&self, range: TextRange, new: &ItemOrder) -> bool {
		let Some(index) = self.index_of(range) else {
			return true;
		};

		let old = &self.items[index].order;
		let after = |a: &ItemOrder, b: &ItemOrder| a.compare(b) == Some(std::cmp::Ordering::Greater);
		let previous = index.checked_sub(1).map(|previous| &self.items[previous].order);
		let next = self.items.get(index + 1).map(|next| &next.order);

		previous.is_none_or(|previous| !after(previous, new) || after(previous, old))
			&& next.is_none_or(|next| !after(new, next) || after(old, next))
	}

	/// The siblings above and below new items placed at `placement` (by index); `None` when it is not next to a
	/// sibling.
	fn neighbors(&self, placement: Placement) -> Option<(Option<usize>, Option<usize>)> {
		match placement {
			Placement::After(range) => {
				let index = self.index_of(range)?;

				Some((Some(index), Some(index + 1).filter(|&next| next < self.items.len())))
			}

			Placement::Before(range) => {
				let index = self.index_of(range)?;

				Some((index.checked_sub(1), Some(index)))
			}

			Placement::End(_) => None,
		}
	}

	/// Where a new item of the order `new` goes among the siblings of `run` (indices, in order): before the first
	/// that sorts after it, else after the last.
	pub(super) fn placement_in(&self, run: &[usize], new: &ItemOrder) -> Option<Placement> {
		let after = run.iter().find(|&&index| self.items[index].order.compare(new) == Some(std::cmp::Ordering::Greater));

		Some(match after {
			Some(&index) => Placement::Before(self.items[index].range),
			None => Placement::After(self.items[*run.last()?].range),
		})
	}

	/// The range of the sibling at `index`.
	pub(super) fn range(&self, index: usize) -> TextRange {
		self.items[index].range
	}

	/// The runs of plain siblings of the group of `new` (by index): consecutive siblings that no blank line separates.
	pub(super) fn runs(&self, new: &ItemOrder) -> Vec<Vec<usize>> {
		let mut runs: Vec<Vec<usize>> = Vec::new();

		for index in (0..self.items.len()).filter(|&index| self.joins(Some(index), new)) {
			match runs.last_mut() {
				Some(run) if run.last() == Some(&(index - 1)) && !self.items[index - 1].blank_below => run.push(index),
				_ => runs.push(vec![index]),
			}
		}

		runs
	}

	/// Where sorting would put a new item of the order `new` (see [`rscode_sort::insertion_point`]); `None` when the
	/// container has no items.
	pub(super) fn sorted_placement(&self, new: &ItemOrder) -> Option<Placement> {
		let orders: Vec<ItemOrder> = self.items.iter().map(|item| item.order.clone()).collect();

		Some(match rscode_sort::insertion_point(&orders, new)? {
			InsertionPoint::Before(index) => Placement::Before(self.items[index].range),
			InsertionPoint::After(index) => Placement::After(self.items[index].range),
		})
	}

	/// The spacing of new items with the orders `new` (`None` unless each is a one-liner of its own) placed at
	/// `placement`: no blank line next to the plain sibling of their group that they are placed next to, nor next to
	/// the one on its other side when no blank line separates them (they must all be of one compact group). When
	/// blank lines separate every plain sibling of the group from the next one, they get blank lines too.
	pub(super) fn spacing(&self, placement: Placement, new: Option<&[ItemOrder]>) -> Spacing {
		let Some(first) = new.and_then(|new| new.first()) else {
			return Spacing::default();
		};

		if new.is_some_and(|new| new.iter().any(|order| !order.same_group(first))) {
			return Spacing::default();
		}

		let Some((above, below)) = self.neighbors(placement) else {
			return Spacing::default();
		};

		let runs = self.runs(first);
		let spaced = runs.len() > 1 && runs.iter().all(|run| run.len() == 1);
		let run_of = |index: usize| runs.iter().position(|run| run.contains(&index));

		// the sibling the items are placed next to, and the one on its other side if it is of the same run
		let anchor = match placement {
			Placement::After(_) => above,
			_ => below,
		};
		let joins_anchor = |side: Option<usize>| match (side, anchor) {
			(Some(side), Some(anchor)) => run_of(side).is_some() && run_of(side) == run_of(anchor),
			_ => false,
		};

		Spacing {
			compact_above: !spaced && joins_anchor(above),
			compact_below: !spaced && joins_anchor(below),
		}
	}

	/// The spacing of new one-line items placed at `placement` in `run` (see [`Siblings::runs`]): no blank line next to
	/// the members of the run, blank lines next to other siblings.
	pub(super) fn spacing_in(&self, placement: Placement, run: &[usize]) -> Spacing {
		let Some((above, below)) = self.neighbors(placement) else {
			return Spacing::default();
		};

		Spacing {
			compact_above: above.is_some_and(|above| run.contains(&above)),
			compact_below: below.is_some_and(|below| run.contains(&below)),
		}
	}
}

/// Whether an item has outer attributes (or doc comments), by its first token.
fn has_attributes(item: &impl quote::ToTokens) -> bool {
	let tokens = item.to_token_stream();

	matches!(tokens.into_iter().next(), Some(proc_macro2::TokenTree::Punct(punct)) if punct.as_char() == '#')
}

/// The order of a module item, from its source (see [`order_of`]).
pub(super) fn item_order(source: &str, style_edition: StyleEdition) -> ItemOrder {
	crate::source::isolated(|| order_of(source, Container::Module, style_edition))
}

/// The orders of the items of `source` (for `container`), when each is on a line of its own without attributes and
/// no other lines are there (such as comments); `None` otherwise, and for containers whose items are never compact.
pub(super) fn new_orders(source: &str, container: Container, style_edition: StyleEdition) -> Option<Vec<ItemOrder>> {
	crate::source::isolated(|| {
		let items: Vec<(ItemOrder, bool, proc_macro2::Span)> = match container {
			Container::Module => {
				let file = syn::parse_file(source).ok().filter(|file| file.attrs.is_empty())?;

				(file.items.iter())
					.map(|item| (ItemOrder::of_item(item, style_edition), has_attributes(item), item.span()))
					.collect()
			}

			Container::Impl => (parse_all::<syn::ImplItem>(source)?.iter())
				.map(|item| (ItemOrder::of_impl_item(item), has_attributes(item), item.span()))
				.collect(),

			Container::Trait => (parse_all::<syn::TraitItem>(source)?.iter())
				.map(|item| (ItemOrder::of_trait_item(item), has_attributes(item), item.span()))
				.collect(),

			Container::Extern | Container::Enum | Container::Fields | Container::ThreadLocal | Container::Entries => {
				return None;
			}
		};

		let lines = source.lines().filter(|line| !line.trim().is_empty()).count();
		let mut previous = 0;

		for (_, attributes, span) in &items {
			let (start, end) = (span.start().line, span.end().line);

			if *attributes || start != end || start <= previous {
				return None;
			}

			previous = start;
		}

		(lines == items.len()).then(|| items.into_iter().map(|(order, ..)| order).collect())
	})
}

/// The order of an item of a container, from its source (on the parsing thread). Source that does not parse on its
/// own is ordered like syntax that `syn` does not model: it keeps its place.
fn order_of(source: &str, container: Container, style_edition: StyleEdition) -> ItemOrder {
	let unknown = proc_macro2::TokenStream::new;

	match container {
		Container::Impl => {
			let item = syn::parse_str(source).unwrap_or_else(|_| syn::ImplItem::Verbatim(unknown()));

			ItemOrder::of_impl_item(&item)
		}

		Container::Trait => {
			let item = syn::parse_str(source).unwrap_or_else(|_| syn::TraitItem::Verbatim(unknown()));

			ItemOrder::of_trait_item(&item)
		}

		_ => ItemOrder::of_item(&syn::parse_str(source).unwrap_or_else(|_| syn::Item::Verbatim(unknown())), style_edition),
	}
}

/// Parses `source` as a sequence of `T`s, on the calling thread.
fn parse_all<T: syn::parse::Parse>(source: &str) -> Option<Vec<T>> {
	let parser = |input: ParseStream<'_>| {
		let mut items = Vec::new();

		while !input.is_empty() {
			items.push(input.parse()?);
		}

		Ok(items)
	};

	parser.parse_str(source).ok()
}

/// The spacing of new items placed at `placement` in `target` (see [`Siblings::spacing`]).
pub(super) fn spacing(ws: &Workspace, target: &Target<'_>, placement: Placement, source: &str) -> Spacing {
	// (groups do not depend on the style edition, only the order of `use` items does)
	let style_edition = StyleEdition::default();

	match new_orders(source, target.container, style_edition) {
		Some(new) => Siblings::of(ws, target, style_edition).spacing(placement, Some(&new)),
		None => Spacing::default(),
	}
}

/// The style edition rustfmt formats the file of `target` with, which decides how `use` items are ordered (as
/// `format_items` sorts them).
pub(super) fn style_edition(ws: &Workspace, target: &Target<'_>) -> StyleEdition {
	let rustfmt = RustFmtOptions {
		edition: Some(ws.krate(target.item.krate()).edition()),
		config_path: target.file.path().parent().map(Path::to_path_buf),
		..RustFmtOptions::default()
	};

	rustfmt.style_edition_in_effect().into()
}
