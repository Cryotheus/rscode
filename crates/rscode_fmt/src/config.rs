//! What rustfmt is configured with that rscode needs to know: the style edition rustfmt formats with, which decides
//! how it orders `use` items, and its style of line breaks.
//!
//! rustfmt takes its style edition from (highest precedence first): a `--config style_edition=..` override,
//! `--style-edition`, the `style_edition` of its configuration file, the deprecated `version` of that file (`Two` is
//! 2024), and the edition it formats for. Its configuration file is the one given with `--config-path`, or else the
//! nearest `.rustfmt.toml` or `rustfmt.toml` of the working directory and its ancestors, then of the home directory,
//! then of the user's configuration directory (`rustfmt/`).

use crate::Edition;
use crate::RustFmtOptions;
use std::path::Path;
use std::path::PathBuf;

/// The names of rustfmt's configuration files, in the order rustfmt looks for them in each directory.
const FILE_NAMES: [&str; 2] = [".rustfmt.toml", "rustfmt.toml"];

/// The configuration file rustfmt reads, given its `config_path` option (see [`RustFmtOptions::config_path`]).
fn config_file(config_path: Option<&Path>) -> Option<PathBuf> {
	let directory = match config_path {
		Some(path) if !path.is_dir() => return Some(path.to_path_buf()),
		Some(directory) => std::path::absolute(directory).ok()?,
		None => std::env::current_dir().ok()?,
	};

	let searched = directory.ancestors().map(Path::to_path_buf);

	searched.chain(user_directories()).find_map(|directory| in_directory(&directory))
}

/// The configuration file in a directory.
fn in_directory(directory: &Path) -> Option<PathBuf> {
	FILE_NAMES.iter().map(|name| directory.join(name)).find(|path| path.is_file())
}

/// Whether rustfmt's `newline_style` is `Auto`, its default: set to it or to nothing, by a `--config` override or else
/// by the configuration file.
pub(crate) fn newline_style_is_auto(options: &RustFmtOptions) -> bool {
	let overridden = options.config.iter().rev().find(|(key, _)| key == "newline_style");
	let style = match overridden {
		Some((_, style)) => Some(style.clone()),
		None => config_file(options.config_path.as_deref())
			.and_then(|path| std::fs::read_to_string(path).ok())
			.and_then(|text| toml_string(&text, "newline_style").map(str::to_owned)),
	};

	// rustfmt reads the values of its options ignoring case
	style.is_none_or(|style| style.eq_ignore_ascii_case("Auto"))
}

/// An edition as rustfmt reads it (editions after 2024 order `use` items like 2024).
fn parse_edition(value: &str) -> Option<Edition> {
	match value.parse::<Edition>() {
		Ok(edition) => Some(edition),
		Err(_) => value.parse::<u16>().ok().filter(|&year| year > 2024).map(|_| Edition::E2024),
	}
}

/// See [`RustFmtOptions::style_edition_in_effect`].
pub(crate) fn style_edition(options: &RustFmtOptions) -> Edition {
	// `--config` overrides are applied after everything else
	let overridden = options.config.iter().rev().find(|(key, _)| key == "style_edition");

	if let Some(edition) = overridden.and_then(|(_, value)| parse_edition(value)) {
		return edition;
	}

	if let Some(style_edition) = options.style_edition {
		return style_edition;
	}

	let configured = config_file(options.config_path.as_deref()).and_then(|path| std::fs::read_to_string(path).ok());

	if let Some(text) = configured {
		let style_edition = toml_string(&text, "style_edition").and_then(parse_edition);
		let version = toml_string(&text, "version").and_then(|version| match version {
			"One" => Some(Edition::E2015),
			"Two" => Some(Edition::E2024),
			_ => None,
		});

		if let Some(edition) = style_edition.or(version) {
			return edition;
		}
	}

	options.edition.unwrap_or_default()
}

/// The string value of a top-level key of a TOML document (`key = "value"` or `key = 'value'`), which is all that
/// rustfmt's configuration files have for the keys of interest.
fn toml_string<'a>(text: &'a str, key: &str) -> Option<&'a str> {
	for line in text.lines() {
		let line = line.trim();

		// keys after a table header are not top-level
		if line.starts_with('[') {
			return None;
		}

		let Some((name, value)) = line.split_once('=') else {
			continue;
		};

		let name = name.trim();

		if name != key && name.strip_prefix('"').and_then(|name| name.strip_suffix('"')) != Some(key) {
			continue;
		}

		let value = value.trim_start();
		let quote = value.chars().next().filter(|&quote| quote == '"' || quote == '\'')?;
		let value = &value[1..];

		return value.find(quote).map(|end| &value[..end]);
	}

	None
}

/// The user's configuration directory (as the `dirs` crate, which rustfmt uses, finds it).
fn user_config_dir() -> Option<PathBuf> {
	let from_env = |name: &str| std::env::var_os(name).map(PathBuf::from).filter(|path| path.is_absolute());

	if cfg!(windows) {
		from_env("APPDATA")
	} else if cfg!(target_os = "macos") {
		std::env::home_dir().map(|home| home.join("Library/Application Support"))
	} else {
		from_env("XDG_CONFIG_HOME").or_else(|| std::env::home_dir().map(|home| home.join(".config")))
	}
}

/// The directories searched when neither the working directory nor its ancestors have a configuration file.
fn user_directories() -> impl Iterator<Item = PathBuf> {
	[std::env::home_dir(), user_config_dir().map(|directory| directory.join("rustfmt"))]
		.into_iter()
		.flatten()
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;

	/// A directory tree for configuration files, removed when dropped.
	struct Tree(PathBuf);

	impl Tree {
		fn new(name: &str) -> Self {
			let path = std::env::temp_dir().join(format!("rscode-fmt-config-{}-{name}", std::process::id()));

			let _ = fs::remove_dir_all(&path);
			fs::create_dir_all(path.join("project/src/nested")).unwrap();

			Self(path)
		}

		fn options(&self, relative: &str, edition: Option<Edition>) -> RustFmtOptions {
			RustFmtOptions {
				edition,
				config_path: Some(self.0.join(relative)),
				..RustFmtOptions::default()
			}
		}

		fn write(&self, relative: &str, text: &str) {
			fs::write(self.0.join(relative), text).unwrap();
		}
	}

	impl Drop for Tree {
		fn drop(&mut self) {
			let _ = fs::remove_dir_all(&self.0);
		}
	}

	#[test]
	fn follows_rustfmt_precedence() {
		let tree = Tree::new("precedence");

		// without configuration (unless the user has some), the edition
		let nested = "project/src/nested";

		if !user_directories().any(|directory| in_directory(&directory).is_some()) {
			assert_eq!(style_edition(&tree.options(nested, Some(Edition::E2021))), Edition::E2021);
			assert_eq!(style_edition(&tree.options(nested, None)), Edition::E2024);
		}

		// the nearest configuration file of the directory or its ancestors
		tree.write("rustfmt.toml", "style_edition = \"2015\"\n");

		assert_eq!(style_edition(&tree.options(nested, Some(Edition::E2024))), Edition::E2015);

		tree.write("project/rustfmt.toml", "version = \"Two\"\n");

		assert_eq!(style_edition(&tree.options(nested, Some(Edition::E2021))), Edition::E2024);

		// `.rustfmt.toml` first, and `style_edition` over `version`
		tree.write("project/.rustfmt.toml", "version = \"Two\"\nstyle_edition = \"2018\"\n");

		assert_eq!(style_edition(&tree.options(nested, Some(Edition::E2024))), Edition::E2018);

		// an explicit file
		assert_eq!(style_edition(&tree.options("rustfmt.toml", Some(Edition::E2024))), Edition::E2015);

		// options over the file, and `--config` overrides over everything
		let mut options = tree.options(nested, Some(Edition::E2015));

		options.style_edition = Some(Edition::E2021);

		assert_eq!(style_edition(&options), Edition::E2021);

		options.config.push(("style_edition".to_owned(), "2024".to_owned()));

		assert_eq!(style_edition(&options), Edition::E2024);
	}

	#[test]
	fn newline_style_is_auto_unless_configured() {
		let tree = Tree::new("newline");
		let nested = "project/src/nested";

		// without configuration (unless the user has some)
		if !user_directories().any(|directory| in_directory(&directory).is_some()) {
			assert!(newline_style_is_auto(&tree.options(nested, None)));
		}

		tree.write("project/rustfmt.toml", "newline_style = \"Unix\"\n");

		assert!(!newline_style_is_auto(&tree.options(nested, None)));

		tree.write("project/rustfmt.toml", "newline_style = \"auto\"\n");

		assert!(newline_style_is_auto(&tree.options(nested, None)));

		// `--config` overrides over the file
		let mut options = tree.options(nested, None);

		options.config.push(("newline_style".to_owned(), "Native".to_owned()));

		assert!(!newline_style_is_auto(&options));
	}

	#[test]
	fn parses_editions_like_rustfmt() {
		assert_eq!(parse_edition("2021"), Some(Edition::E2021));
		assert_eq!(parse_edition("2027"), Some(Edition::E2024));
		assert_eq!(parse_edition("2019"), None);
		assert_eq!(parse_edition("latest"), None);
	}

	#[test]
	fn reads_top_level_strings() {
		let text = "# comment\nmax_width = 100\nstyle_edition = \"2024\" # the latest\n\"version\" = 'Two'\n[table]\nedition = \"2021\"\n";

		assert_eq!(toml_string(text, "style_edition"), Some("2024"));
		assert_eq!(toml_string(text, "version"), Some("Two"));
		assert_eq!(toml_string(text, "max_width"), None);
		assert_eq!(toml_string(text, "edition"), None);
		assert_eq!(toml_string("style_edition=\"2015\"", "style_edition"), Some("2015"));
		assert_eq!(toml_string("style_edition = \"2015", "style_edition"), None);
	}
}
