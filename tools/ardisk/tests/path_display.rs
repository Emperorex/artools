//! Directories are reported under PATH as typed, not under the
//! canonical (absolute, symlink-free) root the scan walks from. --json
//! keeps its own format.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ardisk");

fn ardisk(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run ardisk")
}

/// The path column of each report line (everything after the two-space
/// gap following the size), sorted.
fn paths(out: &Output) -> Vec<String> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut v: Vec<String> = String::from_utf8(out.stdout.clone())
        .unwrap()
        .lines()
        .map(|l| l.trim_start().split_once("  ").unwrap().1.to_string())
        .collect();
    v.sort();
    v
}

/// `sub/d/f`, with some content so every directory has a size.
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("sub/d")).unwrap();
    fs::write(dir.path().join("sub/d/f"), vec![b'x'; 10_000]).unwrap();
    dir
}

#[test]
fn a_typed_path_is_kept_as_typed() {
    let d = tree();
    assert_eq!(paths(&ardisk(d.path(), &["sub"])), ["sub", "sub/d"]);
    assert_eq!(paths(&ardisk(d.path(), &["."])), [".", "./sub", "./sub/d"]);
}

#[test]
fn without_a_path_directories_have_no_dot_prefix() {
    let d = tree();
    assert_eq!(paths(&ardisk(d.path(), &[])), [".", "sub", "sub/d"]);
}

#[test]
fn largest_files_follow_the_typed_path() {
    let d = tree();
    assert_eq!(
        paths(&ardisk(d.path(), &["sub", "--largest-files", "1"])),
        ["sub/d/f"]
    );
}

#[test]
fn json_still_records_the_canonical_root() {
    // Snapshots are compared with --diff, possibly from another directory,
    // so the root they record must not depend on how PATH was typed.
    let d = tree();
    let out = ardisk(d.path(), &["sub", "--json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let root = fs::canonicalize(d.path().join("sub")).unwrap();
    assert_eq!(json["root"], root.to_str().unwrap());
}
