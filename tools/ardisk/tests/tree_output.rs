//! End-to-end tests for `--tree`: they run the real binary.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

fn run(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_ardisk"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run ardisk");
    assert!(
        out.status.success(),
        "ardisk {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn tree(files: &[(&str, usize)]) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for (rel, len) in files {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, vec![b'x'; *len]).unwrap();
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// The part of each row after the value and percentage columns.
fn shapes(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.starts_with('('))
        .map(|l| {
            let after_pct = l.split_once("%  ").expect("percentage column").1;
            after_pct.to_string()
        })
        .collect()
}

#[test]
fn inode_tree_has_the_documented_shape() {
    let (_d, root) = tree(&[
        ("big/a/1", 1),
        ("big/a/2", 1),
        ("big/a/3", 1),
        ("big/b/4", 1),
        ("small/5", 1),
    ]);
    let out = run(&[p(&root), "--tree", "--inodes"]);
    let expected = format!(
        "\
10  100%  {}
 7   70%  ├── big
 4   40%  │   ├── a
 2   20%  │   └── b
 2   20%  └── small
",
        p(&root)
    );
    assert_eq!(out, expected);
}

#[test]
fn siblings_are_ordered_biggest_first_with_apparent_sizes() {
    let (_d, root) = tree(&[("aa/f", 100), ("zz/f", 5_000), ("mm/f", 2_000)]);
    let out = run(&[p(&root), "--tree", "--apparent-size"]);
    let rows = shapes(&out);
    assert_eq!(rows[1], "├── zz");
    assert_eq!(rows[2], "├── mm");
    assert_eq!(rows[3], "└── aa");
}

#[test]
fn tree_lists_the_same_directories_as_the_flat_report() {
    let (_d, root) = tree(&[
        ("a/b/c/f", 3_000),
        ("a/b/g", 2_000),
        ("a/h", 1_000),
        ("d/e/f", 4_000),
        ("i", 500),
    ]);
    for extra in [
        vec![],
        vec!["--top", "3"],
        vec!["--max-depth", "1"],
        vec!["--threshold", "2KB"],
    ] {
        let mut flat_args = vec![p(&root), "--apparent-size"];
        flat_args.extend(extra.iter());
        let mut tree_args = flat_args.clone();
        tree_args.push("--tree");

        let flat = run(&flat_args);
        let flat_count = flat.lines().count();
        let tree_out = run(&tree_args);
        assert_eq!(
            shapes(&tree_out).len(),
            flat_count,
            "row count differs for {extra:?}:\n{flat}\n{tree_out}"
        );
    }
}

#[test]
fn max_depth_limits_the_tree() {
    let (_d, root) = tree(&[("a/b/c/f", 10)]);
    let out = run(&[p(&root), "--tree", "--inodes", "--max-depth", "1"]);
    assert_eq!(shapes(&out), [p(&root).to_string(), "└── a".to_string()]);
}

#[test]
fn truncation_is_announced_only_when_rows_were_cut() {
    let (_d, root) = tree(&[("a/f", 1), ("b/f", 1), ("c/f", 1)]);
    let all = run(&[p(&root), "--tree", "--inodes"]);
    assert!(!all.contains("raise --top"), "{all}");
    let cut = run(&[p(&root), "--tree", "--inodes", "--top", "2"]);
    assert!(
        cut.ends_with("(more directories not shown; raise --top to see them)\n"),
        "{cut}"
    );
}

#[test]
fn deep_guides_continue_under_non_last_children() {
    let (_d, root) = tree(&[("a/x/1", 1), ("a/x/2", 1), ("a/y/3", 1), ("b/4", 1)]);
    let out = run(&[p(&root), "--tree", "--inodes"]);
    assert_eq!(
        shapes(&out)[1..],
        ["├── a", "│   ├── x", "│   └── y", "└── b"]
    );
}

#[test]
fn tree_is_rejected_with_json_and_other_modes() {
    let (_d, root) = tree(&[("a/f", 1)]);
    for flag in ["--json", "--summarize", "--by-type", "--duplicates"] {
        let out = Command::new(env!("CARGO_BIN_EXE_ardisk"))
            .args([p(&root), "--tree", flag])
            .output()
            .unwrap();
        assert!(!out.status.success(), "--tree {flag} should be rejected");
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn tree_does_not_depend_on_the_worker_count() {
    let (_d, root) = tree(&[("a/b/1", 7), ("a/c/2", 7), ("d/3", 7), ("e/f/g/4", 7)]);
    let one = run(&[p(&root), "--tree", "--inodes", "-j", "1"]);
    let many = run(&[p(&root), "--tree", "--inodes", "-j", "8"]);
    assert_eq!(one, many);
}
