//! Grouping files by type for `--by-type`.
//!
//! A file's *type* is its extension: the part of the file name after the
//! last `.`, compared case-insensitively. Compound extensions are not
//! recognized, so `foo.tar.gz` is a `.gz` file.
//!
//! The scan keeps one counter pair per extension per worker thread, so memory
//! is proportional to the number of distinct extensions, not to the number of
//! files.

use std::{borrow::Cow, collections::HashMap};

/// Returns the extension of `file_name` without the dot, or `None` if the
/// file has none.
///
/// | name          | result    |
/// |---------------|-----------|
/// | `archive.zip` | `zip`     |
/// | `foo.tar.gz`  | `gz`      |
/// | `README`      | none      |
/// | `.env`        | none (a leading dot marks a hidden file, not an extension) |
/// | `foo.`        | none      |
/// | `.config.json`| `json`    |
///
/// The returned slice keeps the original case; see [`TypeAccumulator::add`]
/// for how case is folded when counting.
pub fn extension_of(file_name: &str) -> Option<&str> {
    let dot = file_name.rfind('.')?;
    if dot == 0 || dot + 1 == file_name.len() {
        None
    } else {
        Some(&file_name[dot + 1..])
    }
}

/// File count and total size of one type.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TypeStats {
    pub files: u64,
    pub bytes: u64,
}

impl TypeStats {
    fn add(&mut self, bytes: u64) {
        self.files += 1;
        self.bytes += bytes;
    }

    fn merge(&mut self, other: TypeStats) {
        self.files += other.files;
        self.bytes += other.bytes;
    }
}

/// Per-worker counters, merged after the scan.
#[derive(Debug, Default)]
pub struct TypeAccumulator {
    no_extension: TypeStats,
    /// Keyed by the lower-cased extension without the dot.
    by_extension: HashMap<String, TypeStats>,
}

impl TypeAccumulator {
    /// Counts one file of `bytes` bytes. Extensions are folded to lower case,
    /// so `PHOTO.JPG` and `photo.jpg` are the same type.
    pub fn add(&mut self, file_name: &str, bytes: u64) {
        let Some(ext) = extension_of(file_name) else {
            self.no_extension.add(bytes);
            return;
        };
        // Allocate only when the extension is not already lower-case ASCII,
        // which is the overwhelmingly common case.
        let key: Cow<str> = if ext.bytes().any(|b| b.is_ascii_uppercase() || !b.is_ascii()) {
            Cow::Owned(ext.to_lowercase())
        } else {
            Cow::Borrowed(ext)
        };
        match self.by_extension.get_mut(key.as_ref()) {
            Some(stats) => stats.add(bytes),
            None => {
                let mut stats = TypeStats::default();
                stats.add(bytes);
                self.by_extension.insert(key.into_owned(), stats);
            }
        }
    }

    pub fn merge(&mut self, other: TypeAccumulator) {
        self.no_extension.merge(other.no_extension);
        for (ext, stats) in other.by_extension {
            self.by_extension.entry(ext).or_default().merge(stats);
        }
    }

    /// Builds the report table: biggest type first, ties ordered by
    /// extension (files without an extension first).
    pub fn into_table(self) -> TypeTable {
        let mut rows: Vec<TypeRow> = Vec::with_capacity(self.by_extension.len() + 1);
        if self.no_extension.files > 0 {
            rows.push(TypeRow {
                extension: None,
                files: self.no_extension.files,
                bytes: self.no_extension.bytes,
            });
        }
        rows.extend(self.by_extension.into_iter().map(|(ext, s)| TypeRow {
            extension: Some(format!(".{ext}")),
            files: s.files,
            bytes: s.bytes,
        }));
        rows.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then_with(|| a.extension.cmp(&b.extension))
        });
        let total_files = rows.iter().map(|r| r.files).sum();
        let total_bytes = rows.iter().map(|r| r.bytes).sum();
        TypeTable {
            rows,
            total_files,
            total_bytes,
        }
    }
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRow {
    /// Lower-cased extension including the leading dot (`".jpg"`), or `None`
    /// for files without an extension.
    pub extension: Option<String>,
    pub files: u64,
    pub bytes: u64,
}

/// The result of a `--by-type` scan.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TypeTable {
    /// Every type found, biggest first.
    pub rows: Vec<TypeRow>,
    /// Sum of `files` over all rows.
    pub total_files: u64,
    /// Sum of `bytes` over all rows: file sizes only, without the size of
    /// the directory entries themselves.
    pub total_bytes: u64,
}

impl TypeTable {
    /// Share of `bytes` in the total, as a percentage (0 for an empty table).
    pub fn share_percent(&self, bytes: u64) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            bytes as f64 * 100.0 / self.total_bytes as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_rule_from_the_spec() {
        assert_eq!(extension_of("foo.tar.gz"), Some("gz"));
        assert_eq!(extension_of("archive.zip"), Some("zip"));
        assert_eq!(extension_of("README"), None);
        assert_eq!(extension_of(".env"), None);
        assert_eq!(extension_of("foo."), None);
    }

    #[test]
    fn extension_edge_cases() {
        assert_eq!(extension_of(""), None);
        assert_eq!(extension_of("."), None);
        assert_eq!(extension_of(".."), None);
        assert_eq!(extension_of("..."), None);
        assert_eq!(extension_of(".gitignore"), None);
        assert_eq!(extension_of(".config.json"), Some("json"));
        assert_eq!(extension_of("a.b.c.d"), Some("d"));
        assert_eq!(extension_of("..foo"), Some("foo"));
        assert_eq!(extension_of("Makefile.in"), Some("in"));
        assert_eq!(extension_of("файл.ТХТ"), Some("ТХТ"));
        assert_eq!(extension_of("name with spaces.txt"), Some("txt"));
    }

    fn table(files: &[(&str, u64)]) -> TypeTable {
        let mut acc = TypeAccumulator::default();
        for (name, bytes) in files {
            acc.add(name, *bytes);
        }
        acc.into_table()
    }

    fn rows(t: &TypeTable) -> Vec<(Option<&str>, u64, u64)> {
        t.rows
            .iter()
            .map(|r| (r.extension.as_deref(), r.files, r.bytes))
            .collect()
    }

    #[test]
    fn counts_files_and_bytes_per_extension() {
        let t = table(&[("a.rs", 10), ("b.rs", 5), ("c.md", 100), ("README", 1)]);
        assert_eq!(
            rows(&t),
            [(Some(".md"), 1, 100), (Some(".rs"), 2, 15), (None, 1, 1),]
        );
        assert_eq!(t.total_files, 4);
        assert_eq!(t.total_bytes, 116);
    }

    #[test]
    fn extensions_are_case_insensitive() {
        let t = table(&[
            ("A.JPG", 1),
            ("b.jpg", 2),
            ("c.Jpg", 4),
            ("d.ТХТ", 8),
            ("e.тхт", 16),
        ]);
        assert_eq!(rows(&t), [(Some(".тхт"), 2, 24), (Some(".jpg"), 3, 7)]);
    }

    #[test]
    fn dotfiles_and_trailing_dots_have_no_extension() {
        let t = table(&[(".env", 1), (".gitignore", 2), ("foo.", 4), ("LICENSE", 8)]);
        assert_eq!(rows(&t), [(None, 4, 15)]);
    }

    #[test]
    fn sorted_by_bytes_descending_then_extension_ascending() {
        let t = table(&[
            ("a.zip", 10),
            ("b.avi", 10),
            ("c.mp4", 50),
            ("noext", 10),
            ("d.bin", 1),
        ]);
        assert_eq!(
            rows(&t),
            [
                (Some(".mp4"), 1, 50),
                (None, 1, 10),
                (Some(".avi"), 1, 10),
                (Some(".zip"), 1, 10),
                (Some(".bin"), 1, 1),
            ]
        );
    }

    #[test]
    fn empty_files_are_counted() {
        let t = table(&[("a.txt", 0), ("b.txt", 0)]);
        assert_eq!(rows(&t), [(Some(".txt"), 2, 0)]);
    }

    #[test]
    fn empty_accumulator_gives_an_empty_table() {
        let t = TypeAccumulator::default().into_table();
        assert!(t.rows.is_empty());
        assert_eq!((t.total_files, t.total_bytes), (0, 0));
        assert_eq!(t.share_percent(0), 0.0);
    }

    #[test]
    fn merge_equals_single_accumulation() {
        let items = [
            ("a.rs", 3),
            ("B.RS", 4),
            ("c.md", 5),
            ("README", 6),
            (".env", 7),
            ("d.md", 8),
        ];
        let whole = table(&items);

        let (mut left, mut right) = (TypeAccumulator::default(), TypeAccumulator::default());
        for (i, (name, bytes)) in items.iter().enumerate() {
            if i % 2 == 0 {
                left.add(name, *bytes);
            } else {
                right.add(name, *bytes);
            }
        }
        left.merge(right);
        assert_eq!(left.into_table(), whole);
    }

    #[test]
    fn share_percent_is_relative_to_the_total() {
        let t = table(&[("a.x", 25), ("b.y", 75)]);
        assert_eq!(t.share_percent(25), 25.0);
        assert_eq!(t.share_percent(75), 75.0);
        let sum: f64 = t.rows.iter().map(|r| t.share_percent(r.bytes)).sum();
        assert!((sum - 100.0).abs() < 1e-9);
    }
}
