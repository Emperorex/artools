//! The built-in file types for `--type` / `--type-not`, shared by arfind and
//! argrep so `--type rust` selects exactly the same files in both.
//!
//! A small, hand-picked set rather than ripgrep's database of hundreds of
//! types. User-defined types (`--type-add`) and a `--type-list` flag are
//! left for a later change.

/// Built-in types: name and file-name globs.
pub const TYPE_TABLE: &[(&str, &[&str])] = &[
    ("rust", &["*.rs"]),
    ("python", &["*.py", "*.pyi"]),
    ("javascript", &["*.js", "*.jsx", "*.mjs", "*.cjs"]),
    ("typescript", &["*.ts", "*.tsx"]),
    ("json", &["*.json"]),
    ("yaml", &["*.yaml", "*.yml"]),
    ("toml", &["*.toml"]),
    ("markdown", &["*.md"]),
    ("shell", &["*.sh", "*.bash", "*.zsh"]),
    ("go", &["*.go"]),
    ("java", &["*.java"]),
    ("c", &["*.c", "*.h"]),
    // "h" is deliberately only under "c", not also under "cpp": a bare .h
    // file is far more commonly plain C than C++ (C++ headers more often
    // use .hpp/.hh/.hxx). `--type c --type cpp` together covers both.
    ("cpp", &["*.cpp", "*.cc", "*.cxx", "*.hpp"]),
    ("html", &["*.html", "*.htm"]),
    ("css", &["*.css"]),
    ("sql", &["*.sql"]),
    ("xml", &["*.xml"]),
];

/// The globs of a built-in type, or `None` for an unknown name.
pub fn type_globs(name: &str) -> Option<&'static [&'static str]> {
    TYPE_TABLE
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, globs)| *globs)
}

/// All type names, comma-separated, for error messages and help text.
pub fn known_type_names() -> String {
    TYPE_TABLE
        .iter()
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The built-in types for `--type-list`: one `name: glob, glob` line per
/// type, in table order, each ending with a newline.
pub fn type_list() -> String {
    TYPE_TABLE
        .iter()
        .map(|(name, globs)| format!("{name}: {}\n", globs.join(", ")))
        .collect()
}

/// clap value parser for `--type` / `--type-not`: validates the name at
/// parse time, so an unknown type is a usage error listing the known ones.
pub fn type_name_parser(s: &str) -> Result<String, String> {
    if type_globs(s).is_some() {
        Ok(s.to_string())
    } else {
        Err(format!(
            "unknown type '{s}' (known types: {})",
            known_type_names()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_globs_compile() {
        let mut names: Vec<&str> = TYPE_TABLE.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), TYPE_TABLE.len());
        for (_, globs) in TYPE_TABLE {
            assert!(!globs.is_empty());
            for g in *globs {
                glob::Pattern::new(g).unwrap();
            }
        }
    }

    #[test]
    fn type_list_has_one_line_per_type_in_table_order() {
        let list = type_list();
        let lines: Vec<&str> = list.lines().collect();
        assert_eq!(lines.len(), TYPE_TABLE.len());
        assert_eq!(lines[0], "rust: *.rs");
        assert!(lines.contains(&"python: *.py, *.pyi"));
        for ((name, globs), line) in TYPE_TABLE.iter().zip(&lines) {
            assert_eq!(*line, format!("{name}: {}", globs.join(", ")));
        }
        assert!(list.ends_with('\n'));
    }

    #[test]
    fn lookup_and_parser() {
        assert_eq!(type_globs("rust"), Some(&["*.rs"][..]));
        assert_eq!(type_globs("cobol"), None);
        assert_eq!(type_name_parser("rust").unwrap(), "rust");
        let err = type_name_parser("cobol").unwrap_err();
        assert!(
            err.contains("unknown type 'cobol'") && err.contains("rust"),
            "{err}"
        );
    }
}
