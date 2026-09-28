//! Randomly generated files: sorting never loses or duplicates a comment or an item, its output parses, sorting is
//! idempotent, and the text engine orders items exactly like the token engine.

mod common;

use common::Rng;
use common::assert_stable;
use common::file_tokens;
use proc_macro2::TokenStream;

/// Generates source files with unique comment markers (`c0`, `c1`, ...).
struct Generator {
	rng: Rng,
	comments: usize,
	names: usize,

	/// One level of indentation: four spaces or a tab.
	unit: &'static str,
}

impl Generator {
	fn new(seed: u64) -> Self {
		let mut rng = Rng::new(seed);
		let unit = if rng.one_in(3) { "\t" } else { "    " };

		Self { rng, comments: 0, names: 0, unit }
	}

	fn comment(&mut self) -> String {
		let id = self.comments;

		self.comments += 1;

		match self.rng.below(4) {
			0 => format!("/* c{id} */"),
			1 => format!("/* c{id}\n   spanning lines /* nested */ */"),
			_ => format!("// c{id}"),
		}
	}

	/// A comment within a line of code: a block comment, or a line comment and its line break.
	fn inline_comment(&mut self, indent: &str) -> String {
		let id = self.comments;

		self.comments += 1;

		if self.rng.one_in(2) { format!("/* c{id} */ ") } else { format!("// c{id}\n{indent}") }
	}

	/// The header of an `extern` block, sometimes with comments among its attributes and keywords.
	fn extern_header(&mut self, indent: &str) -> String {
		let mut header = String::new();

		if self.rng.one_in(3) {
			header.push_str("#[derive(Debug)] ");

			if self.rng.one_in(2) {
				header.push_str(&self.inline_comment(indent));
			}
		}

		if self.rng.one_in(4) {
			let comment = self.comments;

			self.comments += 1;
			header.push_str(&format!("#[cfg(any(unix, /* c{comment} */ windows))] "));
		}

		header.push_str("unsafe ");

		if self.rng.one_in(4) {
			header.push_str(&self.inline_comment(indent));
		}

		header.push_str("extern ");

		if self.rng.one_in(4) {
			header.push_str(&self.inline_comment(indent));
		}

		header.push_str("\"C\" ");

		if self.rng.one_in(4) {
			header.push_str(&self.inline_comment(indent));
		}

		header
	}

	fn name(&mut self) -> String {
		const STEMS: &[&str] =
			&["alpha", "Beta", "_gamma", "delta_2", "delta_10", "r#type", "new", "_new", "x8", "X16"];

		let stem = STEMS[self.rng.below(STEMS.len())];

		self.names += 1;

		// mostly unique names, sometimes duplicates (cfg variants)
		if self.rng.one_in(6) { stem.to_owned() } else { format!("{stem}_{}", self.names) }
	}

	fn constant_name(&mut self) -> String {
		self.name().to_uppercase().replace("R#", "")
	}

	/// Text before an item: a line break, blank lines, and comments; sometimes just a space (the item shares the line).
	fn gap(&mut self, indent: &str, after_line_comment: bool) -> String {
		if !after_line_comment && self.rng.one_in(10) {
			return " ".to_owned();
		}

		let mut text = String::from("\n");

		for _ in 0..self.rng.below(3) {
			if self.rng.one_in(2) {
				text.push('\n');
			}

			let comment = self.comment();

			text.push_str(indent);
			text.push_str(&comment);
			text.push('\n');
		}

		if self.rng.one_in(4) {
			text.push_str(indent);
			text.push_str("  \n");
		}

		text.push_str(indent);
		text
	}

	fn trailing(&mut self) -> String {
		match self.rng.below(5) {
			0 => format!(" {}", self.comment()),
			_ => String::new(),
		}
	}

	/// The end of a body: a line break, sometimes with a dangling comment, then the indentation of the `}`.
	fn body_end(&mut self, indent: &str, inner: &str) -> String {
		if self.rng.one_in(5) {
			let comment = self.comment();

			format!("\n{inner}{comment}\n{indent}")
		} else {
			format!("\n{indent}")
		}
	}

	fn attrs(&mut self) -> String {
		match self.rng.below(7) {
			0 => "#[cfg(unix)] ".to_owned(),
			1 => "/// Docs.\n".to_owned(),
			2 => "#[cfg(test)] ".to_owned(),
			3 => "/** Block docs. */ ".to_owned(),
			_ => String::new(),
		}
	}

	/// Items separated by gaps, each with an optional trailing comment.
	fn items(&mut self, count: usize, indent: &str, mut item: impl FnMut(&mut Self) -> String) -> String {
		let mut text = String::new();
		let mut after_line_comment = true;

		for _ in 0..count {
			let gap = self.gap(indent, after_line_comment);
			let item = item(self);
			let trailing = self.trailing();

			after_line_comment = trailing.contains("//");
			text.push_str(&gap);
			text.push_str(&item);
			text.push_str(&trailing);
		}

		text
	}

	fn module_item(&mut self, depth: usize, indent: &str) -> String {
		let name = self.name();
		let attrs = self.attrs();
		let inner = format!("{indent}{}", self.unit);
		let item = match self.rng.below(24) {
			0 => format!("use {}::{};", self.name(), name),
			1 => format!("pub use {}::{{{name}, {}}};", self.name(), self.name()),
			2 => format!("const {}: u8 = 0;", self.constant_name()),
			3 => format!("static {}: u8 = 0;", self.constant_name()),
			4 => format!("static mut {}: u8 = 0;", self.constant_name()),
			5 => format!("type {name} = u8;"),
			6 => format!("struct {name};"),
			7 => format!("enum {name} {{\n{inner}B,\n{inner}A,\n{indent}}}"),
			8 => format!("trait {name} {{{}}}", self.associated(indent, true)),
			9 => format!("impl {name} {{{}}}", self.associated(indent, false)),
			10 => format!("impl Clone for {name} {{{}}}", self.associated(indent, false)),
			11 => format!("fn {name}() {{\n{inner}fn z() {{}}\n{inner}fn a() {{}}\n{indent}}}"),
			12 => format!("mod {name};"),
			13 => format!("{}{{{}}}", self.extern_header(indent), self.foreign(indent)),
			14 => format!("extern \"C\" {{ {} }}", self.single_line_items("fn", "();")),
			15 => format!("{name}!();"),
			16 if depth < 2 => {
				let count = self.rng.below(8);
				let body = self.items(count, &inner, |this| this.module_item(depth + 1, &inner));
				let end = self.body_end(indent, &inner);

				format!("mod {name} {{{body}{end}}}")
			}
			17 => format!("mod {name} {{ {} }}", self.single_line_items("fn", "() {}")),
			18 => format!("impl {name} {{ {} }}", self.single_line_items("const", ": u8 = 0;")),
			19 => format!("macro_rules! m_{} {{ () => {{}} }}", self.names),
			// macros defined by an invocation (tokio's `cfg_x! { macro_rules! .. }`)
			20 => format!("cfg_x! {{\n{inner}macro_rules! m_{} {{ () => {{}} }}\n{indent}}}", self.names),
			// a redefined helper macro and its invocations
			21 => "macro_rules! helper { ($a:ident) => {}; }".to_owned(),
			22 => format!("helper!({name});"),
			_ => format!("fn {name}() {{}}"),
		};

		format!("{attrs}{item}")
	}

	fn associated(&mut self, indent: &str, in_trait: bool) -> String {
		let inner = format!("{indent}{}", self.unit);
		let count = self.rng.below(6);
		let items = self.items(count, &inner, |this| {
			let name = this.name();
			let item = match this.rng.below(7) {
				0 => format!("type {name} = u8;"),
				1 => format!("const {}: u8 = 0;", this.constant_name()),
				2 => format!("fn {name}(&self) {{}}"),
				3 => format!("fn {name}(self: Box<Self>) {{}}"),
				4 => "m!();".to_owned(),
				5 => "fn new() -> u8 { 0 }".to_owned(),
				_ => format!("fn {name}() {{}}"),
			};

			// associated types of traits have no value
			if in_trait { item.replace(" = u8;", ";") } else { item }
		});
		let end = self.body_end(indent, &inner);

		format!("{items}{end}")
	}

	fn foreign(&mut self, indent: &str) -> String {
		let inner = format!("{indent}{}", self.unit);
		let count = self.rng.below(4);
		let items = self.items(count, &inner, |this| {
			let name = this.name();

			match this.rng.below(4) {
				0 => format!("static {}: u8;", this.constant_name()),
				1 => format!("type {name};"),
				2 => format!("/// Docs.\n{inner}pub fn {name}();"),
				_ => format!("pub fn {name}();"),
			}
		});
		let end = self.body_end(indent, &inner);

		format!("{items}{end}")
	}

	/// Items on a single line, sometimes with a block comment: `fn a(); /* c1 */ fn b();`.
	fn single_line_items(&mut self, keyword: &str, end: &str) -> String {
		let mut items: Vec<String> = (0..self.rng.below(4))
			.map(|_| {
				let name = if keyword == "const" { self.constant_name() } else { self.name() };

				format!("{keyword} {name}{end}")
			})
			.collect();

		if self.rng.one_in(3) {
			let id = self.comments;
			let position = self.rng.below(items.len() + 1);

			self.comments += 1;
			items.insert(position, format!("/* c{id} */"));
		}

		items.join(" ")
	}

	fn file(&mut self) -> String {
		let mut text = String::new();

		if self.rng.one_in(3) {
			text.push_str(&format!("{}\n", self.comment()));
		}

		if self.rng.one_in(3) {
			text.push_str("#![allow(dead_code)]\n");
		}

		let count = self.rng.below(10);

		text.push_str(&self.items(count, "", |this| this.module_item(0, "")));
		text.push('\n');

		if self.rng.one_in(3) {
			text.push_str(&format!("{}\n", self.comment()));
		}

		text
	}
}

/// The segment (the number of macro items before it) and a description of every item of a file: items never move
/// across macros. `extern` blocks, which merge, are left out.
fn segments(source: &str) -> Vec<(usize, String)> {
	let file = syn::parse_file(source).unwrap();
	let mut segment = 0;
	let mut described = Vec::new();

	for item in &file.items {
		let description = match item {
			syn::Item::Macro(item) => {
				segment += 1;
				quote::ToTokens::to_token_stream(item).to_string()
			}
			syn::Item::ForeignMod(_) => continue,
			syn::Item::Impl(item) => {
				let self_ty = quote::ToTokens::to_token_stream(&item.self_ty).to_string();

				match &item.trait_ {
					Some((path, _)) => format!("impl {} for {self_ty}", quote::ToTokens::to_token_stream(path)),
					None => format!("impl {self_ty}"),
				}
			}
			// everything else is identified by its tokens before the first group (its attributes and name)
			other => quote::ToTokens::to_token_stream(other)
				.into_iter()
				.take_while(|token| !matches!(token, proc_macro2::TokenTree::Group(group) if group.delimiter() == proc_macro2::Delimiter::Brace))
				.map(|token| token.to_string())
				.collect::<Vec<_>>()
				.join(" "),
		};

		described.push((segment, description));
	}

	described.sort();
	described
}

/// How many times each comment marker appears in the text.
fn comment_counts(text: &str, comments: usize) -> Vec<usize> {
	let mut counts = vec![0; comments];
	let bytes = text.as_bytes();

	for (index, _) in text.match_indices(" c") {
		let digits: String = text[index + 2..].chars().take_while(char::is_ascii_digit).collect();

		// `c<digits>` followed by the end of the comment
		let end = index + 2 + digits.len();

		if !digits.is_empty()
			&& (end == bytes.len() || matches!(bytes[end], b'\n' | b'\r' | b' '))
			&& let Ok(id) = digits.parse::<usize>()
			&& id < comments
		{
			counts[id] += 1;
		}
	}

	counts
}

/// Checks 1000 files, or `RSCODE_SORT_SEEDS` files.
#[test]
fn random_files() {
	let seeds: u64 = std::env::var("RSCODE_SORT_SEEDS").ok().and_then(|seeds| seeds.parse().ok()).unwrap_or(1000);
	let mut checked = 0;

	for seed in 1..=seeds {
		let mut generator = Generator::new(seed);
		let source = generator.file();

		if syn::parse_file(&source).is_err() {
			// the generator may produce names that are not valid everywhere
			continue;
		}

		let output = rscode_sort::sort_str(&source).unwrap_or_else(|error| panic!("seed {seed}: {error}\n{source}"));
		let again = rscode_sort::sort_str(&output).unwrap_or_else(|error| panic!("seed {seed}: {error}\n{output}"));

		assert_eq!(again, output, "seed {seed} is not idempotent\n--- source:\n{source}\n--- first:\n{output}");
		assert_stable(&output);

		// no comment lost or duplicated
		let before = comment_counts(&source, generator.comments);
		let after = comment_counts(&output, generator.comments);

		assert_eq!(before, after, "seed {seed}\n--- source:\n{source}\n--- output:\n{output}");

		// nothing moves across a macro
		assert_eq!(segments(&source), segments(&output), "seed {seed}\n--- source:\n{source}\n--- output:\n{output}");

		// the text engine orders exactly like the token engine
		let tokens = rscode_sort::sort_tokens(source.parse::<TokenStream>().unwrap()).unwrap();
		let tokens = quote::ToTokens::into_token_stream(syn::parse2::<syn::File>(tokens).unwrap()).to_string();

		assert_eq!(file_tokens(&output), tokens, "seed {seed}\n--- source:\n{source}\n--- output:\n{output}");

		// CRLF line endings are kept
		let crlf = source.replace('\n', "\r\n");
		let crlf_output = rscode_sort::sort_str(&crlf).unwrap();

		assert_eq!(crlf_output, output.replace('\n', "\r\n"), "seed {seed}");

		checked += 1;
	}

	assert!(checked * 4 > seeds * 3, "only {checked} of {seeds} generated files parsed");
}
