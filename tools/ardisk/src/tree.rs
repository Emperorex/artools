//! `--tree`: the directory report drawn as a hierarchy, like `dust` or a
//! text-mode `ncdu`.
//!
//! The tree shows exactly the directories the flat report would list (see
//! [`select_directories`](crate::report::select_directories)); only the
//! layout differs. That selection is always closed under ancestors: a
//! directory is never smaller than any of its subdirectories, and on a tie
//! the parent's path sorts first, so whenever `--top`, `--max-depth`,
//! `--threshold` or `--include` keep a directory, they keep its parent too.
//! The renderer still copes with a missing parent by hanging the directory
//! under its nearest listed ancestor and printing the path in between.

use crate::report::DirectorySelection;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

/// One printed row of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeLine {
    /// The directory this row stands for.
    pub path: PathBuf,
    /// Its size in bytes (or inode count with `--inodes`).
    pub value: u64,
    /// Box-drawing guides in front of the name, e.g. `"│   ├── "`. Empty for
    /// a top-level row.
    pub guide: String,
    /// What is printed after the guides: the full path for a top-level row,
    /// otherwise the path relative to the row's parent in the tree (usually
    /// a single name).
    pub name: String,
}

/// Arranges `selection` into tree rows, depth first. Siblings keep the
/// selection's order: biggest first, ties by path.
pub fn tree_lines(selection: &DirectorySelection, root: &Path) -> Vec<TreeLine> {
    let entries = &selection.entries;
    let index: HashMap<&Path, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, (p, _))| (p.as_path(), i))
        .collect();

    // Parent of each entry: its nearest ancestor that is also listed, but
    // never above the scan root.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    let mut tops: Vec<usize> = Vec::new();
    for (i, (path, _)) in entries.iter().enumerate() {
        let parent = if path == root {
            None
        } else {
            path.ancestors()
                .skip(1)
                .take_while(|a| a.starts_with(root))
                .find_map(|a| index.get(a).copied())
        };
        match parent {
            Some(p) => children[p].push(i),
            None => tops.push(i),
        }
    }

    let mut lines = Vec::with_capacity(entries.len());
    for &top in &tops {
        let (path, value) = &entries[top];
        lines.push(TreeLine {
            path: path.clone(),
            value: *value,
            guide: String::new(),
            name: path.display().to_string(),
        });
        walk(top, entries, &children, String::new(), &mut lines);
    }
    lines
}

fn walk(
    node: usize,
    entries: &[(PathBuf, u64)],
    children: &[Vec<usize>],
    indent: String,
    lines: &mut Vec<TreeLine>,
) {
    let parent = &entries[node].0;
    let kids = &children[node];
    for (k, &child) in kids.iter().enumerate() {
        let last = k + 1 == kids.len();
        let (path, value) = &entries[child];
        let name = path
            .strip_prefix(parent)
            .map_or_else(|_| path.display().to_string(), |r| r.display().to_string());
        lines.push(TreeLine {
            path: path.clone(),
            value: *value,
            guide: format!("{indent}{}", if last { "└── " } else { "├── " }),
            name,
        });
        let next = format!("{indent}{}", if last { "    " } else { "│   " });
        walk(child, entries, children, next, lines);
    }
}

/// Percentage of `total` as a whole number with a `%` sign (`"0%"` when the
/// total is zero).
pub fn percent(value: u64, total: u64) -> String {
    if total == 0 {
        "0%".to_string()
    } else {
        format!("{:.0}%", value as f64 * 100.0 / total as f64)
    }
}

/// Renders the rows as text:
///
/// ```text
///   1.20 GB  100%  /home/me/project
/// 900.00 MB   73%  ├── target
/// 850.00 MB   69%  │   └── debug
/// 300.00 MB   24%  └── data
/// ```
///
/// `plain` formats a value; `paint` may then colour the already padded text
/// (it receives the raw value too), so ANSI codes never upset the alignment.
/// The percentage is relative to `total`, normally the root's own value.
/// `truncated` adds a closing hint that more directories qualified.
pub fn render_tree_text(
    lines: &[TreeLine],
    total: u64,
    truncated: bool,
    plain: impl Fn(u64) -> String,
    paint: impl Fn(u64, String) -> String,
) -> String {
    let values: Vec<String> = lines.iter().map(|l| plain(l.value)).collect();
    let shares: Vec<String> = lines.iter().map(|l| percent(l.value, total)).collect();
    let vw = values.iter().map(|v| v.chars().count()).max().unwrap_or(0);
    let sw = shares.iter().map(|s| s.len()).max().unwrap_or(0);

    let mut out = String::new();
    for ((line, value), share) in lines.iter().zip(values).zip(shares) {
        let padded = format!("{value:>vw$}");
        out.push_str(&format!(
            "{}  {share:>sw$}  {}{}\n",
            paint(line.value, padded),
            line.guide,
            line.name
        ));
    }
    if truncated {
        out.push_str("(more directories not shown; raise --top to see them)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(root: &Path, items: &[(&str, u64)], truncated: bool) -> DirectorySelection {
        DirectorySelection {
            entries: items
                .iter()
                .map(|(rel, v)| {
                    let p = if rel.is_empty() {
                        root.to_path_buf()
                    } else {
                        root.join(rel)
                    };
                    (p, *v)
                })
                .collect(),
            truncated,
        }
    }

    fn text(s: &DirectorySelection, root: &Path) -> String {
        let lines = tree_lines(s, root);
        let total = s.entries.first().map_or(0, |e| e.1);
        render_tree_text(&lines, total, s.truncated, |v| v.to_string(), |_, t| t)
    }

    #[test]
    fn draws_a_dust_like_tree_biggest_first() {
        let root = Path::new("/r");
        let s = sel(
            root,
            &[
                ("", 100),
                ("a", 60),
                ("a/x", 40),
                ("b", 30),
                ("a/y", 15),
                ("a/x/deep", 10),
                ("b/z", 5),
            ],
            false,
        );
        assert_eq!(
            text(&s, root),
            "\
100  100%  /r
 60   60%  ├── a
 40   40%  │   ├── x
 10   10%  │   │   └── deep
 15   15%  │   └── y
 30   30%  └── b
  5    5%      └── z
"
        );
    }

    #[test]
    fn only_the_root() {
        let root = Path::new("/r");
        let s = sel(root, &[("", 7)], false);
        assert_eq!(text(&s, root), "7  100%  /r\n");
    }

    #[test]
    fn empty_selection_prints_nothing() {
        let root = Path::new("/r");
        assert_eq!(text(&sel(root, &[], false), root), "");
    }

    #[test]
    fn missing_parent_hangs_under_nearest_listed_ancestor() {
        let root = Path::new("/r");
        let s = sel(root, &[("", 10), ("a/b/c", 4), ("d", 3)], false);
        assert_eq!(
            text(&s, root),
            "\
10  100%  /r
 4   40%  ├── a/b/c
 3   30%  └── d
"
        );
    }

    #[test]
    fn without_the_root_each_orphan_is_a_top_level_row() {
        let root = Path::new("/r");
        let s = sel(root, &[("a", 5), ("a/b", 4), ("c", 3)], false);
        let lines = tree_lines(&s, root);
        let names: Vec<(&str, &str)> = lines
            .iter()
            .map(|l| (l.guide.as_str(), l.name.as_str()))
            .collect();
        assert_eq!(names, [("", "/r/a"), ("└── ", "b"), ("", "/r/c")]);
    }

    #[test]
    fn truncated_adds_a_hint() {
        let root = Path::new("/r");
        let out = text(&sel(root, &[("", 1)], true), root);
        assert!(out.ends_with("raise --top to see them)\n"), "{out}");
    }

    #[test]
    fn paint_sees_the_padded_text_so_alignment_survives_colour() {
        let root = Path::new("/r");
        let s = sel(root, &[("", 1000), ("a", 5)], false);
        let lines = tree_lines(&s, root);
        let out = render_tree_text(
            &lines,
            1000,
            false,
            |v| v.to_string(),
            |_, t| format!("<{t}>"),
        );
        assert_eq!(out, "<1000>  100%  /r\n<   5>    0%  └── a\n");
    }

    #[test]
    fn percent_rounds_and_handles_zero_total() {
        assert_eq!(percent(1, 3), "33%");
        assert_eq!(percent(2, 3), "67%");
        assert_eq!(percent(0, 0), "0%");
        assert_eq!(percent(5, 5), "100%");
    }
}
