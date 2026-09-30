//! Formatting selected items of a file.

mod common;

use common::format_items;
use common::offset;
use common::prettyplease;
use common::rustfmt;
use common::rustfmt_available;
use common::targets;
use rscode_fmt::Edition;
use rscode_fmt::FormatError;
use rscode_fmt::FormatOptions;
use rscode_fmt::FormatTarget;
use rscode_fmt::Formatter;
use rscode_fmt::RsFormatter;
use rscode_fmt::SortOptions;

/// Asserts that only the text of the item starting at `needle` (from the start of its line) changed.
fn assert_only_item_changed(source: &str, formatted: &str, needle: &str, item_end_needle: &str) {
	let start = offset(source, needle);
	let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
	let end = offset(source, item_end_needle) + item_end_needle.len();

	assert!(formatted.starts_with(&source[..line_start]), "text before `{needle}` changed:\n{formatted}");
	assert!(formatted.ends_with(&source[end..]), "text after `{needle}` changed:\n{formatted}");
}

const MISFORMATTED: &str = "\
use b::c;
use a::b;
fn  first( ){let a=1;}

// about second
fn second( x:u8 )->u8{x+1} // trailing

fn  third( ){ }
";

#[test]
fn formats_a_top_level_item() {
	if !rustfmt_available() {
		return;
	}

	let formatted = format_items(rustfmt(), MISFORMATTED, &["fn second"]);

	assert_eq!(formatted, "\
use b::c;
use a::b;
fn  first( ){let a=1;}

// about second
fn second(x: u8) -> u8 {
    x + 1
} // trailing

fn  third( ){ }
");
	assert_only_item_changed(MISFORMATTED, &formatted, "fn second", "x+1}");
}

#[test]
fn formats_multiple_items() {
	if !rustfmt_available() {
		return;
	}

	let formatted = format_items(rustfmt(), MISFORMATTED, &["fn  third", "fn  first"]);

	assert_eq!(formatted, "\
use b::c;
use a::b;
fn first() {
    let a = 1;
}

// about second
fn second( x:u8 )->u8{x+1} // trailing

fn third() {}
");
}

#[test]
fn formats_an_item_in_an_inline_module() {
	if !rustfmt_available() {
		return;
	}

	let source = "\
mod outer {
        fn  inner( ){let a=1;}
  fn untouched( ) {}
    mod deeper {
    const  X:u8=1;
    }
}
";

	assert_eq!(format_items(rustfmt(), source, &["fn  inner"]), "\
mod outer {
    fn inner() {
        let a = 1;
    }
  fn untouched( ) {}
    mod deeper {
    const  X:u8=1;
    }
}
");
	assert_eq!(format_items(rustfmt(), source, &["const  X"]), "\
mod outer {
        fn  inner( ){let a=1;}
  fn untouched( ) {}
    mod deeper {
        const X: u8 = 1;
    }
}
");
}

#[test]
fn formats_a_method() {
	if !rustfmt_available() {
		return;
	}

	let source = "\
struct S;

impl S {
    /// Makes one.
  #[inline]
    pub fn  new( )->Self{S}
    fn other(&self){ }
}
";

	let formatted = format_items(rustfmt(), source, &["/// Makes one."]);

	assert_eq!(formatted, "\
struct S;

impl S {
    /// Makes one.
    #[inline]
    pub fn new() -> Self {
        S
    }
    fn other(&self){ }
}
");
	assert_only_item_changed(source, &formatted, "/// Makes one.", "{S}");
}

#[test]
fn formats_a_trait_item() {
	if !rustfmt_available() {
		return;
	}

	let source = "\
trait T {
    fn  required( &self )->u8;
    fn provided(&self)->u8{ 1 }
}
";

	assert_eq!(format_items(rustfmt(), source, &["fn  required"]), "\
trait T {
    fn required(&self) -> u8;
    fn provided(&self)->u8{ 1 }
}
");
	assert_eq!(format_items(rustfmt(), source, &["fn provided"]), "\
trait T {
    fn  required( &self )->u8;
    fn provided(&self) -> u8 {
        1
    }
}
");
}

#[test]
fn formats_a_foreign_item() {
	if !rustfmt_available() {
		return;
	}

	let source = "\
unsafe extern \"C\" {
    pub fn  ext( a:i32 )->i32;
    static  X:u8;
}
";

	assert_eq!(format_items(rustfmt(), source, &["pub fn  ext"]), "\
unsafe extern \"C\" {
    pub fn ext(a: i32) -> i32;
    static  X:u8;
}
");
}

#[test]
fn formats_containers_with_their_items() {
	if !rustfmt_available() {
		return;
	}

	let source = "\
fn  before( ) {}
impl  S{
fn  a( ){}
    fn b( ) {}
}
fn  after( ) {}
";

	let expected = "\
fn  before( ) {}
impl S {
    fn a() {}
    fn b() {}
}
fn  after( ) {}
";

	assert_eq!(format_items(rustfmt(), source, &["impl  S"]), expected);

	// the nested target is covered by the impl
	assert_eq!(format_items(rustfmt(), source, &["fn b", "impl  S", "fn  a"]), expected);
}

#[test]
fn keeps_attached_comments_and_imports() {
	if !rustfmt_available() {
		return;
	}

	let source = "use b::c;\nuse a::b;\n    // stays as it is\nfn  f( ){}\n";

	assert_eq!(format_items(rustfmt(), source, &["fn  f"]), "use b::c;\nuse a::b;\n    // stays as it is\nfn f() {}\n");
}

#[test]
fn formats_imports_in_place() {
	if !rustfmt_available() {
		return;
	}

	// rustfmt reorders the imports of the whole file, but only the targets are spliced, in place
	let source = "use c::{z, y};\nuse a::b;\nuse b::{x,w};\nfn  f( ) {}\n";

	assert_eq!(format_items(rustfmt(), source, &["use b::"]), "use c::{z, y};\nuse a::b;\nuse b::{w, x};\nfn  f( ) {}\n");
	assert_eq!(format_items(rustfmt(), source, &["use c::"]), "use c::{y, z};\nuse a::b;\nuse b::{x,w};\nfn  f( ) {}\n");
	assert_eq!(format_items(rustfmt(), source, &["fn  f"]), "use c::{z, y};\nuse a::b;\nuse b::{x,w};\nfn f() {}\n");

	// long import lists are wrapped as usual
	let source = "use crate::expr::{ExprBreak, ExprRange, ExprRawAddr, ExprReference, ExprReturn, ExprUnary, ExprYield};\nuse a;\n";

	assert_eq!(
		format_items(rustfmt(), source, &["use crate"]),
		"use crate::expr::{\n    ExprBreak, ExprRange, ExprRawAddr, ExprReference, ExprReturn, ExprUnary, ExprYield,\n};\nuse a;\n",
	);
}

#[test]
fn formats_duplicate_imports() {
	if !rustfmt_available() {
		return;
	}

	let source = "use b;\nuse  a;\nuse b;\nuse  a;\n";
	let second = offset(source, "use b;\nuse  a;\n") + "use b;\nuse  a;\nuse b;\n".len();
	let formatted = Formatter::new(rustfmt()).format_items(source, &[FormatTarget::Item(second)]).unwrap();

	assert_eq!(formatted, "use b;\nuse  a;\nuse b;\nuse a;\n");
}

#[test]
fn formats_items_sharing_a_line() {
	if !rustfmt_available() {
		return;
	}

	assert_eq!(format_items(rustfmt(), "struct A; struct  B;\n", &["struct  B"]), "struct A; struct B;\n");
	assert_eq!(format_items(rustfmt(), "struct  A; struct  B;\n", &["struct  A"]), "struct A; struct  B;\n");
}

#[test]
fn formats_macros() {
	if !rustfmt_available() {
		return;
	}

	let source = "macro_rules!  m { ( $x:expr ) => { $x } }\nm!( 1 );\nvec ! [ 1 ];\n";
	let formatted = format_items(rustfmt(), source, &["macro_rules!"]);

	// rustfmt does not format matchers by default
	assert!(formatted.starts_with("macro_rules! m {\n    ( $x:expr ) => {\n        $x\n    };\n}\n"), "{formatted}");
	assert!(formatted.ends_with("\nm!( 1 );\nvec ! [ 1 ];\n"), "{formatted}");
	assert_eq!(format_items(rustfmt(), source, &["m!( 1 )"]), source.replace("m!( 1 )", "m!(1)"));
}

#[test]
fn is_idempotent() {
	if !rustfmt_available() {
		return;
	}

	let once = format_items(rustfmt(), MISFORMATTED, &["fn second", "fn  third"]);
	let twice = format_items(rustfmt(), &once, &["fn second", "fn third"]);

	assert_eq!(once, twice);
}

#[test]
fn keeps_crlf_line_endings() {
	if !rustfmt_available() {
		return;
	}

	let source = "fn  a( ){let x=1;}\r\nmod m {\r\n  fn  b( ){let y=2;}\r\n}\r\n";

	assert_eq!(
		format_items(rustfmt(), source, &["fn  b"]),
		"fn  a( ){let x=1;}\r\nmod m {\r\n    fn b() {\r\n        let y = 2;\r\n    }\r\n}\r\n",
	);
}

#[test]
fn keeps_lf_line_endings() {
	if !rustfmt_available() {
		return;
	}

	let mut options = rustfmt();

	options.rustfmt.config.push(("newline_style".to_owned(), "Windows".to_owned()));

	assert_eq!(format_items(options, "fn  a( ){let x=1;}\nfn  b( ) {}\n", &["fn  a"]), "fn a() {\n    let x = 1;\n}\nfn  b( ) {}\n");
}

#[test]
fn keeps_bom_and_shebang() {
	if !rustfmt_available() {
		return;
	}

	let source = "\u{feff}#!/usr/bin/env run-cargo-script\nfn  a( ){}\nfn  b( ){}\n";

	assert_eq!(format_items(rustfmt(), source, &["fn  a"]), "\u{feff}#!/usr/bin/env run-cargo-script\nfn a() {}\nfn  b( ){}\n");
	assert_eq!(format_items(rustfmt(), "\u{feff}fn  a( ){}\n", &["fn  a"]), "\u{feff}fn a() {}\n");
}

#[test]
fn uses_the_configured_indentation() {
	if !rustfmt_available() {
		return;
	}

	let mut options = rustfmt();

	options.rustfmt.config_path = Some(common::fixture("rustfmt/hard_tabs"));

	assert_eq!(
		format_items(options, "mod m {\n    fn  f( ){let x=1;}\n}\n", &["fn  f"]),
		"mod m {\n\tfn f() {\n\t\tlet x = 1;\n\t}\n}\n",
	);
}

#[test]
fn needs_the_right_edition() {
	if !rustfmt_available() {
		return;
	}

	let source = "fn  f(a: Option<u8>) { if let Some(x)=a && x>1 {} }\n";
	let mut options = rustfmt();

	assert_eq!(
		format_items(options.clone(), source, &["fn  f"]),
		"fn f(a: Option<u8>) {\n    if let Some(x) = a\n        && x > 1\n    {}\n}\n",
	);

	// let chains need the 2024 edition
	options.rustfmt.edition = Some(Edition::E2021);

	let error = Formatter::new(options.clone()).format_items(source, &targets(source, &["fn  f"])).unwrap_err();

	assert!(matches!(&error, FormatError::RustFmt { stderr } if stderr.contains("let chains")), "{error}");

	// without an edition, the latest (rustfmt itself would assume 2015)
	options.rustfmt.edition = None;

	assert_eq!(
		format_items(options, source, &["fn  f"]),
		"fn f(a: Option<u8>) {\n    if let Some(x) = a\n        && x > 1\n    {}\n}\n",
	);
}

#[test]
fn rejects_offsets_that_are_not_items() {
	let source = "// comment\nfn outer() {\n    struct Inner;\n}\nstruct S { field: u8 }\n";
	let formatter = Formatter::new(rustfmt());

	for needle in ["// comment", "outer", "struct Inner", "field", "{\n"] {
		let start = offset(source, needle);

		match formatter.format_items(source, &[FormatTarget::Item(start)]) {
			Err(FormatError::NoItem(error_start)) => assert_eq!(error_start, start),
			other => panic!("`{needle}`: unexpected result {other:?}"),
		}
	}

	assert!(matches!(formatter.format_items(source, &[FormatTarget::Item(10_000)]), Err(FormatError::NoItem(10_000))));
}

#[test]
fn rejects_invalid_source() {
	let formatter = Formatter::new(rustfmt());

	for targets in [vec![FormatTarget::File], vec![FormatTarget::Item(0)]] {
		match formatter.format_items("fn a() {}\nfn b( {}\n", &targets) {
			Err(FormatError::Parse { line: 2, .. }) => {}
			other => panic!("unexpected result {other:?}"),
		}
	}
}

#[test]
fn without_targets_nothing_changes() {
	let source = "fn  a( ) {}\nnot even rust";

	assert_eq!(Formatter::new(rustfmt()).format_items(source, &[]).unwrap(), source);
}

#[test]
fn without_a_formatter_nothing_changes() {
	let source = "fn  a( ) {}\nfn  b( ) {}\n";
	let options = FormatOptions::new().formatter(RsFormatter::None);

	assert_eq!(format_items(options.clone(), source, &["fn  b"]), source);
	assert_eq!(Formatter::new(options).format_str(source).unwrap(), source);
}

#[test]
fn a_file_target_formats_everything() {
	if !rustfmt_available() {
		return;
	}

	let source = "fn  a( ) {}\nfn  b( ) {}\n";
	let targets = [FormatTarget::Item(offset(source, "fn  b")), FormatTarget::File];

	assert_eq!(Formatter::new(rustfmt()).format_items(source, &targets).unwrap(), "fn a() {}\nfn b() {}\n");
}

#[test]
fn prettyplease_formats_items() {
	let source = "// header comment\nfn  a( ){let x=1;}\nimpl S {\n  fn  b( ){ /* inner */ }\n}\n";

	// comments outside of the target do not matter
	assert_eq!(
		format_items(prettyplease(), source, &["fn  a"]),
		"// header comment\nfn a() {\n    let x = 1;\n}\nimpl S {\n  fn  b( ){ /* inner */ }\n}\n",
	);

	let formatter = Formatter::new(prettyplease());

	assert!(matches!(
		formatter.format_items(source, &targets(source, &["fn  b"])),
		Err(FormatError::CommentsWouldBeLost),
	));
	assert_eq!(
		format_items(prettyplease().allow_comment_loss(true), source, &["fn  b"]),
		"// header comment\nfn  a( ){let x=1;}\nimpl S {\n    fn b() {}\n}\n",
	);
}

#[test]
fn prettyplease_separates_match_arms_spanning_lines() {
	// prettyplease drops the blank lines between arms, which are put back after it: formatting again changes nothing
	let f = "fn f(x: u8) -> u8 { match x { 0 => 1, 1 => { y(); 2 } _ => 3 } }\n";
	let source = format!("fn  g( ) {{}}\n{f}");
	let expected = "fn  g( ) {}\nfn f(x: u8) -> u8 {\n    match x {\n        0 => 1,\n\
		\n        1 => {\n            y();\n            2\n        }\n\n        _ => 3,\n    }\n}\n";
	let formatter = Formatter::new(prettyplease());
	let tokens: proc_macro2::TokenStream = f.parse().unwrap();

	assert_eq!(format_items(prettyplease(), &source, &["fn f"]), expected);
	assert_eq!(formatter.format_str(&source).unwrap(), expected.replace("fn  g( ) {}", "fn g() {}"));
	assert_eq!(formatter.format_str(expected).unwrap(), expected.replace("fn  g( ) {}", "fn g() {}"));
	assert_eq!(formatter.format_tokens(tokens).unwrap(), expected.replace("fn  g( ) {}\n", ""));
}

#[test]
fn prettyplease_keeps_crlf_line_endings() {
	assert_eq!(
		format_items(prettyplease(), "fn  a( ){let x=1;}\r\nfn  b( ) {}\r\n", &["fn  a"]),
		"fn a() {\r\n    let x = 1;\r\n}\r\nfn  b( ) {}\r\n",
	);
}

#[test]
fn prettyplease_keeps_crlf_line_endings_of_whole_files() {
	let source = "fn  a( ) {\r\n    let s = \"line one\r\nline two\";\r\n}\r\n";
	let formatted = Formatter::new(prettyplease()).format_str(source).unwrap();

	assert_eq!(formatted, "fn a() {\r\n    let s = \"line one\r\nline two\";\r\n}\r\n");
	assert_eq!(Formatter::new(prettyplease()).format_str("fn  a( ) {}\n").unwrap(), "fn a() {}\n");
}

#[test]
fn prettyplease_refuses_to_change_the_meaning_of_code() {
	// prettyplease 0.3.0 leaves out `safe`, field defaults, and `..` without a base, without failing
	let cases = [
		("unsafe extern \"C\" {\n    pub safe static  X: u8;\n}\nfn  keep( ) {}\n", "pub safe static", "safe static"),
		("struct S {\n    a: u8 = 1,\n}\nfn  keep( ) {}\n", "struct S", "= 1"),
		("fn  f( ) -> S {\n    S { .. }\n}\n", "fn  f", ".."),
	];

	for (source, target, lost) in cases {
		for targets in [vec![FormatTarget::File], targets(source, &[target])] {
			match Formatter::new(prettyplease()).format_items(source, &targets) {
				Err(FormatError::PrettyPlease(message)) => {
					assert!(message.contains("would change the meaning") && message.contains(lost), "{source}: {message}");
				}
				other => panic!("{source}: unexpected result {other:?}"),
			}
		}
	}
}

#[test]
fn prettyplease_refuses_unsupported_syntax() {
	let source = "const trait T {}\nfn  a( ) {}\n";

	assert!(matches!(
		Formatter::new(prettyplease()).format_items(source, &targets(source, &["fn  a"])),
		Err(FormatError::PrettyPlease(_)),
	));
}

#[test]
fn formats_whole_files_with_prettyplease() {
	let formatter = Formatter::new(prettyplease());

	assert_eq!(formatter.format_str("fn  a( ) {}\n\n\nfn b() {}").unwrap(), "fn a() {}\nfn b() {}\n");
	assert!(matches!(formatter.format_str("fn a() {} // c"), Err(FormatError::CommentsWouldBeLost)));
}

#[test]
fn formats_whole_files_with_rustfmt() {
	if !rustfmt_available() {
		return;
	}

	let formatter = Formatter::new(rustfmt());

	assert_eq!(formatter.format_str("use b;\nuse a;\nfn  a( ) {} // c\n").unwrap(), "use a;\nuse b;\nfn a() {} // c\n");
	assert_eq!(formatter.format_str("").unwrap(), "\n");
}

#[test]
fn whole_files_keep_their_line_breaks() {
	if !rustfmt_available() {
		return;
	}

	let formatter = Formatter::new(rustfmt());

	// rustfmt itself gives `\n`: it sees its input with `\r\n` turned into `\n`
	assert_eq!(formatter.format_str("fn  a( ) {}\r\nfn b() {}\r\n").unwrap(), "fn a() {}\r\nfn b() {}\r\n");

	// without line breaks, `\n` (rustfmt would give the platform's)
	assert_eq!(formatter.format_str("fn  a( ) {}").unwrap(), "fn a() {}\n");

	// unless rustfmt is configured otherwise
	let mut options = rustfmt();

	options.rustfmt.config.push(("newline_style".to_owned(), "Unix".to_owned()));

	assert_eq!(Formatter::new(options).format_str("fn  a( ) {}\r\n").unwrap(), "fn a() {}\n");
}

#[test]
fn whole_files_keep_their_bom() {
	if !rustfmt_available() {
		return;
	}

	// rustfmt drops the byte order mark of its input, but not of the files it formats
	let formatted = "\u{feff}fn a() {}\n";

	for options in [rustfmt(), prettyplease(), FormatOptions::new().formatter(RsFormatter::None)] {
		let formatter = Formatter::new(options);

		assert_eq!(formatter.format_str(formatted).unwrap(), formatted);
		assert_eq!(formatter.format_str("\u{feff}use b;\nuse a;\nfn a() {}\n").unwrap().chars().next(), Some('\u{feff}'));
	}
}

#[test]
fn targets_imports_with_redundant_in_visibilities() {
	if !rustfmt_available() {
		return;
	}

	// rustfmt writes `pub(in crate)` as `pub(crate)`
	let source = "pub(in crate) use  a;\npub(in self) use  b;\npub(in super) use  c;\npub(in crate::m) use  d;\nfn  f( ) {}\n";

	assert_eq!(
		format_items(rustfmt(), source, &["pub(in crate) use", "pub(in self)", "pub(in super)", "pub(in crate::m)"]),
		"pub(crate) use a;\npub(self) use b;\npub(super) use c;\npub(in crate::m) use d;\nfn  f( ) {}\n",
	);
}

#[test]
fn targets_next_to_imports_of_nothing() {
	if !rustfmt_available() {
		return;
	}

	// rustfmt removes `use` items that import nothing (unless they have attributes or a visibility)
	let source = "use a::{};\nuse b;\nuse {};\n#[cfg(x)]\nuse c::{d::{}};\nfn  f( ) {}\nmod m {\n    fn  g( ) {}\n}\n";

	assert_eq!(
		format_items(rustfmt(), source, &["fn  f", "fn  g"]),
		"use a::{};\nuse b;\nuse {};\n#[cfg(x)]\nuse c::{d::{}};\nfn f() {}\nmod m {\n    fn g() {}\n}\n",
	);

	// targeting them: removed ones are removed with their lines
	assert_eq!(
		format_items(rustfmt(), source, &["use a", "use {}", "#[cfg(x)]"]),
		"use b;\n#[cfg(x)]\nuse c::d::{};\nfn  f( ) {}\nmod m {\n    fn  g( ) {}\n}\n",
	);

	// prettyplease keeps them
	assert_eq!(
		format_items(prettyplease(), source, &["use a", "fn  f"]),
		"use a::{};\nuse b;\nuse {};\n#[cfg(x)]\nuse c::{d::{}};\nfn f() {}\nmod m {\n    fn  g( ) {}\n}\n",
	);
}

#[test]
fn formats_deeply_nested_code_with_the_recommended_stack_size() {
	// a spawned thread's default stack (2 MiB) overflows, aborting the process, at a few hundred levels
	let depth = 2000;
	let source = format!("fn f() -> u8 {{ {}1{} }}\nfn  g( ) {{}}\n", "(".repeat(depth), ")".repeat(depth));
	let formatted = std::thread::Builder::new()
		.stack_size(rscode_fmt::RECOMMENDED_STACK_SIZE)
		.spawn(move || {
			let target = [FormatTarget::Item(offset(&source, "fn  g"))];
			let whole_file = Formatter::new(prettyplease()).format_str(&source).unwrap();
			let item = Formatter::new(prettyplease()).format_items(&source, &target).unwrap();
			let unformatted = Formatter::new(FormatOptions::new().formatter(RsFormatter::None)).format_items(&source, &target).unwrap();

			(whole_file, item, unformatted, source)
		})
		.unwrap()
		.join()
		.unwrap();

	assert!(formatted.0.ends_with(")\n}\nfn g() {}\n"));
	assert_eq!(formatted.1, formatted.3.replace("fn  g( )", "fn g()"));
	assert_eq!(formatted.2, formatted.3);
}

#[test]
fn formats_many_imports_in_linear_time() {
	// one container with thousands of targeted imports: matching each import in the output once per target was
	// quadratic (and took minutes)
	let count = 4000;
	let source: String = (0..count).map(|index| format!("#[cfg(feature = \"f{index}\")]\npub use  crate::m{index}::{{A, B, C}};\n")).collect();
	let targets: Vec<FormatTarget> =
		source.match_indices("#[cfg").map(|(start, _)| FormatTarget::Item(start)).collect();
	let started = std::time::Instant::now();
	let formatted = Formatter::new(prettyplease()).format_items(&source, &targets).unwrap();

	assert_eq!(formatted, source.replace("use  crate", "use crate"));
	assert!(started.elapsed().as_secs() < 60, "took {:?}", started.elapsed());
}

#[cfg(unix)]
mod fake_rustfmt {
	use super::*;
	use std::os::unix::fs::PermissionsExt;
	use std::path::PathBuf;

	/// A "rustfmt" that ignores its input and prints `output`.
	fn fake(name: &str, output: &str) -> FormatOptions {
		let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("fake-rustfmt-{name}"));
		let script = format!("#!/bin/sh\ncat > /dev/null\ncat <<'EOF'\n{output}\nEOF\n");

		std::fs::write(&path, script).unwrap();
		std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

		let mut options = rustfmt();

		options.rustfmt.program = Some(path);
		options
	}

	fn format(options: FormatOptions, source: &str, needle: &str) -> Result<String, FormatError> {
		Formatter::new(options).format_items(source, &targets(source, &[needle]))
	}

	#[test]
	fn detects_renamed_items() {
		let error = format(fake("renamed", "fn a() {}\nfn c() {}"), "fn a() {}\nfn b() {}\n", "fn b").unwrap_err();

		assert!(matches!(&error, FormatError::StructureMismatch(message) if message.contains("items of the file changed")), "{error}");
	}

	#[test]
	fn detects_missing_items() {
		let error = format(fake("missing", "fn a() {}"), "fn a() {}\nfn b() {}\n", "fn b").unwrap_err();

		assert!(matches!(&error, FormatError::StructureMismatch(message) if message.contains("items of the file changed")), "{error}");

		// any change to the containers of the target is detected, even if the target is still found
		let error = format(fake("missing-sibling", "mod m {\n    fn a() {}\n}"), "mod m {\n    fn a() {}\n    fn b() {}\n}\n", "fn a").unwrap_err();

		assert!(matches!(&error, FormatError::StructureMismatch(message) if message.contains("items of mod `m` changed")), "{error}");
	}

	#[test]
	fn detects_swapped_items() {
		// the blocks have the same kind and name, but different items
		let source = "impl S {\n    fn a() {}\n}\nimpl T {}\nimpl S {\n    fn b() {}\n}\n";
		let options = fake("swapped", "impl S {\n    fn b() {}\n}\nimpl T {}\nimpl S {\n    fn a() {}\n}");
		let error = format(options.clone(), source, "impl S").unwrap_err();

		assert!(matches!(&error, FormatError::StructureMismatch(message) if message.contains("items of impl `S` changed")), "{error}");

		// other targets are unaffected
		assert_eq!(format(options, source, "impl T").unwrap(), source);
	}

	#[test]
	fn detects_changed_kinds() {
		let error = format(fake("kind", "fn a() {}\nstruct b;"), "fn a() {}\nfn b() {}\n", "fn b").unwrap_err();

		assert!(matches!(error, FormatError::StructureMismatch(_)), "{error}");
	}

	#[test]
	fn detects_invalid_output() {
		let error = format(fake("invalid", "fn a() {"), "fn a() {}\n", "fn a").unwrap_err();

		assert!(matches!(&error, FormatError::StructureMismatch(message) if message.contains("does not parse")), "{error}");

		let error = Formatter::new(fake("invalid-file", "fn a() {")).format_str("fn a() {}\n").unwrap_err();

		assert!(matches!(error, FormatError::StructureMismatch(_)), "{error}");
	}

	#[test]
	fn uses_the_matching_item() {
		// items are matched by position, not text
		let formatted = format(fake("positions", "fn a() {\n    1\n}\nfn b() {\n    2\n}"), "fn a() { 1 }\nfn b() { 2 }\n", "fn b");

		assert_eq!(formatted.unwrap(), "fn a() { 1 }\nfn b() {\n    2\n}\n");
	}
}

mod sorting {
	use super::*;

	fn sorted(options: FormatOptions) -> FormatOptions {
		options.sort(Some(SortOptions::new()))
	}

	#[test]
	fn sorts_and_formats_a_targeted_container() {
		let source = "fn  z( ) {}\nimpl S {\n    fn b(&self) {}\n    fn a(&self) {}\n}\nfn  y( ) {}\n";
		let formatted = format_items(sorted(prettyplease()), source, &["impl S"]);

		assert!(formatted.starts_with("fn  z( ) {}\nimpl S {\n"), "{formatted}");
		assert!(formatted.ends_with("}\nfn  y( ) {}\n"), "{formatted}");
		assert!(formatted.find("fn a").unwrap() < formatted.find("fn b").unwrap(), "{formatted}");
	}

	#[test]
	fn sorts_without_formatting() {
		let source = "fn z() {}\nmod m {\n    fn  b( ) {}\n    fn  a( ) {}\n}\n";
		let formatted = format_items(sorted(FormatOptions::new().formatter(RsFormatter::None)), source, &["mod m"]);

		assert!(formatted.starts_with("fn z() {}\nmod m {\n"), "{formatted}");
		assert!(formatted.find("fn  a( ) {}").unwrap() < formatted.find("fn  b( ) {}").unwrap(), "{formatted}");
	}

	#[test]
	fn does_not_sort_other_items() {
		// a function is not a container: only formatted (the sorter is not even called)
		let source = "fn b() {}\nfn  a( ) {}\n";

		assert_eq!(format_items(sorted(prettyplease()), source, &["fn  a"]), "fn b() {}\nfn a() {}\n");
	}

	#[test]
	fn sorts_whole_files() {
		if !rustfmt_available() {
			return;
		}

		let formatted = Formatter::new(sorted(rustfmt())).format_str("fn  b( ) {}\nfn  a( ) {}\n").unwrap();

		assert!(formatted.find("fn a() {}").unwrap() < formatted.find("fn b() {}").unwrap(), "{formatted}");
	}

	#[test]
	fn sorts_nested_targets_without_recursion() {
		let source = "mod outer {\n    fn z() {}\n    mod inner {\n        fn d() {}\n        fn c() {}\n    }\n    fn y() {}\n}\n";
		let options = FormatOptions::new().formatter(RsFormatter::None).sort(Some(SortOptions::new().recursive(false)));

		// only the outer module is sorted
		let formatted = format_items(options.clone(), source, &["mod outer"]);

		assert!(formatted.find("fn y").unwrap() < formatted.find("fn z").unwrap(), "{formatted}");
		assert!(formatted.find("fn d").unwrap() < formatted.find("fn c").unwrap(), "{formatted}");

		// both are sorted when both are targeted
		let formatted = format_items(options, source, &["mod outer", "mod inner"]);

		assert!(formatted.find("fn y").unwrap() < formatted.find("fn z").unwrap(), "{formatted}");
		assert!(formatted.find("fn c").unwrap() < formatted.find("fn d").unwrap(), "{formatted}");
	}
}
