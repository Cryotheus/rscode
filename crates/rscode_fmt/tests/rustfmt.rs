//! Running rustfmt.

mod common;

use common::fixture;
use common::rustfmt;
use common::rustfmt_available;
use rscode_fmt::Edition;
use rscode_fmt::FormatError;
use rscode_fmt::FormatOptions;
use rscode_fmt::Formatter;
use std::path::PathBuf;

const SOURCE: &str = "fn main() { let some_long_variable_name = foo(aaaaaaa, bbbbbbbb, ccccccc); }\n";

fn format(options: FormatOptions, source: &str) -> Result<String, FormatError> {
	Formatter::new(options).format_str(source)
}

#[test]
fn formats_with_the_default_configuration() {
	if !rustfmt_available() {
		return;
	}

	assert_eq!(format(rustfmt(), SOURCE).unwrap(), "fn main() {\n    let some_long_variable_name = foo(aaaaaaa, bbbbbbbb, ccccccc);\n}\n");
}

#[test]
fn applies_config_overrides() {
	if !rustfmt_available() {
		return;
	}

	let mut options = rustfmt();

	options.rustfmt.config = vec![("max_width".to_owned(), "40".to_owned()), ("hard_tabs".to_owned(), "true".to_owned())];

	assert_eq!(format(options, SOURCE).unwrap(), "fn main() {\n\tlet some_long_variable_name =\n\t\tfoo(aaaaaaa, bbbbbbbb, ccccccc);\n}\n");
}

/// rustfmt merging imports that sorting separated (around an import with attributes) changes how they sort: formatting
/// sorts again, so that formatting the result changes nothing.
#[test]
fn settles_when_rustfmt_merges_imports() {
	if !rustfmt_available() {
		return;
	}

	for granularity in ["Crate", "Module", "One", "Item", "Preserve"] {
		let mut options = rustfmt().sort(Some(rscode_fmt::SortOptions::new()));

		options.rustfmt.config = vec![("imports_granularity".to_owned(), granularity.to_owned())];

		for source in ["use a::b;\n#[cfg(unix)]\nuse a::c;\nuse a::a;\n", "use a::{c, z};\n#[cfg(unix)]\nuse a::d;\n"] {
			let once = format(options.clone(), source).unwrap();

			assert_eq!(format(options.clone(), &once).unwrap(), once, "{granularity}: {source:?}");
		}
	}
}

#[test]
fn reports_invalid_config_overrides() {
	if !rustfmt_available() {
		return;
	}

	let mut options = rustfmt();

	options.rustfmt.config = vec![("not_an_option".to_owned(), "1".to_owned())];

	assert!(matches!(format(options.clone(), SOURCE), Err(FormatError::RustFmt { stderr }) if stderr.contains("not_an_option")));

	// cannot be passed to rustfmt at all
	options.rustfmt.config = vec![("max_width".to_owned(), "40,hard_tabs=true".to_owned())];

	assert!(matches!(format(options, SOURCE), Err(FormatError::InvalidRustFmtConfig(_))));
}

#[test]
fn reads_a_config_file() {
	if !rustfmt_available() {
		return;
	}

	let mut options = rustfmt();

	options.rustfmt.config_path = Some(fixture("rustfmt/hard_tabs/rustfmt.toml"));

	assert_eq!(format(options, "fn a() { let x = 1; }").unwrap(), "fn a() {\n\tlet x = 1;\n}\n");
}

#[test]
fn searches_for_the_config_from_a_directory() {
	if !rustfmt_available() {
		return;
	}

	let mut options = rustfmt();

	// the configuration is in the directory itself
	options.rustfmt.config_path = Some(fixture("rustfmt/hard_tabs"));

	assert_eq!(format(options.clone(), "fn a() { let x = 1; }").unwrap(), "fn a() {\n\tlet x = 1;\n}\n");

	// the configuration is in an ancestor, as for the files of a project
	let directory = fixture("rustfmt/hard_tabs/nested/deeper");
	let source = std::fs::read_to_string(directory.join("unformatted.rs")).unwrap();

	options.rustfmt.config_path = Some(directory);

	assert_eq!(format(options, &source).unwrap(), "fn nested() {\n\tlet x = 1;\n}\n");
}

#[test]
fn reports_missing_config_files() {
	if !rustfmt_available() {
		return;
	}

	let mut options = rustfmt();

	options.rustfmt.config_path = Some(fixture("rustfmt/does-not-exist.toml"));

	assert!(matches!(format(options, SOURCE), Err(FormatError::RustFmt { .. })));
}

#[test]
fn passes_the_style_edition() {
	if !rustfmt_available() {
		return;
	}

	// the 2024 style edition sorts `u8` after `U8`; earlier style editions sort case-insensitively
	let source = "use a::{u8, U8};\n";
	let mut options = rustfmt();

	options.rustfmt.style_edition = Some(Edition::E2021);

	assert_eq!(format(options.clone(), source).unwrap(), "use a::{u8, U8};\n");

	options.rustfmt.style_edition = Some(Edition::E2024);

	assert_eq!(format(options, source).unwrap(), "use a::{U8, u8};\n");
}

#[test]
fn formats_tokens() {
	if !rustfmt_available() {
		return;
	}

	let tokens = quote::quote! {
		/// Docs.
		pub struct Foo { a: u8 }
	};

	// `quote!` turns doc comments into attributes with raw string literals
	assert_eq!(
		Formatter::new(rustfmt()).format_tokens(tokens).unwrap(),
		"#[doc = r\" Docs.\"]\npub struct Foo {\n    a: u8,\n}\n",
	);
}

#[test]
fn formats_tokens_keeping_the_grouping_of_groups_without_delimiters() {
	if !rustfmt_available() {
		return;
	}

	// `⟦a + b⟧ * 2`, as produced by `syn::Expr::Group` or a `macro_rules!` expression fragment, is not `a + b * 2`
	let sum = proc_macro2::Group::new(proc_macro2::Delimiter::None, quote::quote!(a + b));
	let variable = proc_macro2::Group::new(proc_macro2::Delimiter::None, quote::quote!(a));
	let tokens = quote::quote!(fn f(a: u8, b: u8) -> u8 { m!(#sum * 2); #variable * #sum * 2 });

	assert_eq!(
		Formatter::new(rustfmt()).format_tokens(tokens).unwrap(),
		"fn f(a: u8, b: u8) -> u8 {\n    m!((a + b) * 2);\n    a * (a + b) * 2\n}\n",
	);
}

#[test]
fn the_default_options_use_the_latest_edition() {
	if !rustfmt_available() {
		return;
	}

	// `async fn` does not parse in the 2015 edition, which rustfmt assumes without one
	let source = "async fn f() { let x: Box<dyn Fn()> = todo!(); x().await }\n";
	let tokens: proc_macro2::TokenStream = source.parse().unwrap();

	assert!(Formatter::default().format_str(source).unwrap().starts_with("async fn f() {"));
	assert!(Formatter::new(FormatOptions::new()).format_tokens(tokens).unwrap().starts_with("async fn f() {"));
}

#[test]
fn the_convenience_function_uses_the_latest_edition() {
	if !rustfmt_available() {
		return;
	}

	// `async fn` does not parse in the 2015 edition
	let formatted = rscode_fmt::format_str("async fn  f( ) {}\n").unwrap();

	assert!(formatted.starts_with("async fn f() {}"), "{formatted}");
}

#[test]
fn reports_missing_programs() {
	let mut options = rustfmt();

	options.rustfmt.program = Some(PathBuf::from("/nonexistent/rustfmt"));

	match format(options, SOURCE) {
		Err(FormatError::RustFmtSpawn { program, source }) => {
			assert_eq!(program, "/nonexistent/rustfmt");
			assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
		}
		other => panic!("unexpected result: {other:?}"),
	}
}

/// A relative program path is relative to the current directory, even when rustfmt runs in the configuration
/// directory.
#[cfg(unix)]
#[test]
fn resolves_relative_programs_from_the_current_directory() {
	use std::os::unix::fs::PermissionsExt;
	use std::path::Component;
	use std::path::Path;

	if !rustfmt_available() {
		return;
	}

	let wrapper = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("rustfmt-wrapper");

	std::fs::write(&wrapper, "#!/bin/sh\nexec rustfmt \"$@\"\n").unwrap();
	std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

	// `../../..` up to the root, then down to the wrapper
	let current = std::env::current_dir().unwrap();
	let mut relative: PathBuf = current.components().filter(|component| matches!(component, Component::Normal(_))).map(|_| "..").collect();

	relative.push(wrapper.strip_prefix("/").unwrap());

	assert!(relative.is_relative());
	assert!(Path::new(&relative).exists());

	let mut options = rustfmt();

	options.rustfmt.program = Some(relative);
	options.rustfmt.config_path = Some(fixture("rustfmt/hard_tabs/nested"));

	assert_eq!(format(options, "fn a() { let x = 1; }").unwrap(), "fn a() {\n\tlet x = 1;\n}\n");
}
