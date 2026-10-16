//! Named file types (`--type` / `--type-not`) and `--extension`.
//!
//! The type table itself lives in `arcommon::filetypes`, shared with argrep,
//! so `arfind . --type rust` and `argrep PATTERN . --type rust` select exactly
//! the same files.

use glob::Pattern;

// The table and lookups are shared with argrep (arcommon::filetypes), so
// `--type rust` selects the same files in both tools.
pub use arcommon::filetypes::{TYPE_TABLE, known_type_names, type_globs};

/// clap value parser for `--type` / `--type-not`: unknown names are a usage
/// error listing the known types. The entry kinds `f`, `d` and `l` get a
/// pointer to `-t`, since that is what someone typing `--type f` wants.
pub fn type_name_parser(s: &str) -> Result<String, String> {
    arcommon::filetypes::type_name_parser(s).map_err(|err| {
        if matches!(s, "f" | "d" | "l") {
            format!("{err}; to filter by entry kind use -t {s} (--file-type {s})")
        } else {
            err
        }
    })
}

/// Normalizes an `--extension` value: trims, drops one leading dot, lowers
/// the case. `"RS"`, `".rs"` and `"rs"` all become `"rs"`; compound
/// extensions such as `tar.gz` are kept as they are.
pub fn extension_parser(s: &str) -> Result<String, String> {
    let ext = s.trim();
    let ext = ext.strip_prefix('.').unwrap_or(ext);
    if ext.is_empty() || ext.contains(['/', '\\']) {
        return Err(format!(
            "invalid extension '{s}': expected something like rs, .md or tar.gz"
        ));
    }
    Ok(ext.to_lowercase())
}

/// Whether `file_name` has extension `ext` (already normalized by
/// [`extension_parser`]). Case-insensitive. A leading dot marks a hidden
/// file, not an extension: `.env` has no extension, `.config.json` is
/// `json`.
pub fn has_extension(file_name: &str, ext: &str) -> bool {
    let name = file_name.strip_prefix('.').unwrap_or(file_name);
    let name = name.to_lowercase();
    name.len() > ext.len() + 1
        && name.ends_with(ext)
        && name.as_bytes()[name.len() - ext.len() - 1] == b'.'
}

/// The `--type`, `--type-not` and `--extension` filters, resolved.
///
/// They only ever select **non-directory** entries: with `--type` or
/// `--extension` active, directories are not listed (but still searched),
/// and `--type-not` never hides a directory.
#[derive(Debug, Clone, Default)]
pub struct NameKindFilter {
    /// Globs of every `--type`; a match on any of them passes.
    pub include: Vec<Pattern>,
    /// `--extension` values; a match on any of them passes too (OR with
    /// `include`).
    pub extensions: Vec<String>,
    /// Globs of every `--type-not`; a match on any of them rejects, and
    /// wins over `include` and `extensions`.
    pub exclude: Vec<Pattern>,
}

impl NameKindFilter {
    /// Builds the filter from validated type names and extensions.
    pub fn new(types: &[String], type_not: &[String], extensions: &[String]) -> Self {
        let expand = |names: &[String]| -> Vec<Pattern> {
            names
                .iter()
                .flat_map(|n| type_globs(n).expect("validated by type_name_parser"))
                .map(|g| Pattern::new(g).expect("TYPE_TABLE globs are valid"))
                .collect()
        };
        NameKindFilter {
            include: expand(types),
            extensions: extensions.to_vec(),
            exclude: expand(type_not),
        }
    }

    pub fn is_active(&self) -> bool {
        !self.include.is_empty() || !self.extensions.is_empty() || !self.exclude.is_empty()
    }

    /// Whether an entry with this name passes. `is_dir` entries pass only
    /// when no positive filter (`--type`, `--extension`) is active.
    pub fn matches(&self, file_name: &str, is_dir: bool) -> bool {
        let positive = !self.include.is_empty() || !self.extensions.is_empty();
        if is_dir {
            return !positive;
        }
        if self.exclude.iter().any(|p| p.matches(file_name)) {
            return false;
        }
        !positive
            || self.include.iter().any(|p| p.matches(file_name))
            || self.extensions.iter().any(|e| has_extension(file_name, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_unknown_types() {
        assert_eq!(type_name_parser("rust").unwrap(), "rust");
        let err = type_name_parser("cobol").unwrap_err();
        assert!(err.contains("known types: rust, python"), "{err}");
        let err = type_name_parser("f").unwrap_err();
        assert!(err.contains("-t f"), "{err}");
    }

    #[test]
    fn type_table_has_unique_names_and_valid_globs() {
        let mut names: Vec<&str> = TYPE_TABLE.iter().map(|(n, _)| *n).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), TYPE_TABLE.len());
        for (_, globs) in TYPE_TABLE {
            for g in *globs {
                Pattern::new(g).unwrap();
            }
        }
    }

    #[test]
    fn extension_normalization() {
        assert_eq!(extension_parser("rs").unwrap(), "rs");
        assert_eq!(extension_parser(".RS").unwrap(), "rs");
        assert_eq!(extension_parser(" tar.GZ ").unwrap(), "tar.gz");
        for bad in ["", ".", "  ", "a/b"] {
            assert!(extension_parser(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn extension_matching() {
        assert!(has_extension("main.rs", "rs"));
        assert!(has_extension("MAIN.RS", "rs"));
        assert!(has_extension("a.tar.gz", "gz"));
        assert!(has_extension("a.tar.gz", "tar.gz"));
        assert!(has_extension(".config.json", "json"));
        assert!(!has_extension(".env", "env"));
        assert!(!has_extension("rs", "rs"));
        assert!(!has_extension("ars", "rs"));
        assert!(!has_extension("main.rs.bak", "rs"));
        assert!(!has_extension("x.", "rs"));
    }

    fn filter(t: &[&str], not: &[&str], ext: &[&str]) -> NameKindFilter {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        NameKindFilter::new(&s(t), &s(not), &s(ext))
    }

    #[test]
    fn types_and_extensions_are_ored_and_type_not_wins() {
        let f = filter(&["rust"], &[], &["toml"]);
        assert!(f.matches("main.rs", false));
        assert!(f.matches("Cargo.toml", false));
        assert!(!f.matches("README.md", false));
        assert!(
            !f.matches("src", true),
            "directories never match a positive filter"
        );

        let f = filter(&["c", "cpp"], &["c"], &[]);
        assert!(f.matches("a.cpp", false));
        assert!(!f.matches("a.c", false), "--type-not wins");
        assert!(!f.matches("a.h", false));
    }

    #[test]
    fn type_not_alone_keeps_directories_and_other_files() {
        let f = filter(&[], &["markdown"], &[]);
        assert!(f.matches("docs", true));
        assert!(f.matches("main.rs", false));
        assert!(!f.matches("README.md", false));
        assert!(f.is_active());
        assert!(!filter(&[], &[], &[]).is_active());
    }

    #[test]
    fn types_are_case_sensitive_like_argrep() {
        let f = filter(&["rust"], &[], &[]);
        assert!(!f.matches("MAIN.RS", false));
    }
}
