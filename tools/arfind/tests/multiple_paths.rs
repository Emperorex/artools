//! Several PATH arguments: `arfind a b c`. Each root is searched with its
//! own --exclude anchoring and -x device, results are reported under the
//! PATH they were found under, as typed, and a PATH that doesn't exist is
//! reported by name without stopping the rest.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_arfind");

fn arfind(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run arfind")
}

/// stdout lines, sorted (roots are searched in parallel).
fn lines(out: &Output) -> Vec<String> {
    let mut v: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    v.sort();
    v
}

/// `a/k.txt`, `a/build/o`, `b/src/m.rs`, `b/build/o`.
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    for rel in ["a/k.txt", "a/build/o", "b/src/m.rs", "b/build/o"] {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, "").unwrap();
    }
    dir
}

#[test]
fn every_root_is_searched_and_reported_as_typed() {
    let d = tree();
    let out = arfind(d.path(), &["-t", "f", "a", "./b/"]);
    assert!(out.status.success());
    assert_eq!(
        lines(&out),
        ["./b/build/o", "./b/src/m.rs", "a/build/o", "a/k.txt"]
    );
    // Each root itself is a candidate too, as with one root.
    let out = arfind(d.path(), &["-t", "d", "--name", "?", "a", "b"]);
    assert_eq!(lines(&out), ["a", "b"]);
}

#[test]
fn an_anchored_exclude_applies_under_every_root() {
    // `build/**` is anchored: relative to the root it is matched under.
    let d = tree();
    let out = arfind(d.path(), &["-t", "f", "a", "b", "--exclude", "build/**"]);
    assert_eq!(lines(&out), ["a/k.txt", "b/src/m.rs"]);
}

#[test]
fn a_missing_root_is_named_and_the_rest_still_searched() {
    let d = tree();
    let out = arfind(d.path(), &["-t", "f", "a", "missing", "b"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        lines(&out),
        ["a/build/o", "a/k.txt", "b/build/o", "b/src/m.rs"]
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("arfind: missing: No such file or directory"),
        "{err}"
    );

    // On its own, too: no longer a silent success.
    let out = arfind(d.path(), &["missing"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn sort_count_and_nul_lists_cover_all_roots() {
    let d = tree();
    let out = arfind(d.path(), &["-t", "f", "--sort", "path", "b", "a"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "a/build/o\na/k.txt\nb/build/o\nb/src/m.rs\n"
    );
    let out = arfind(d.path(), &["-t", "f", "--count", "a", "b"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "4\n");
    let out = arfind(d.path(), &["-t", "f", "-0", "a", "b"]);
    let mut names: Vec<&[u8]> = out
        .stdout
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [&b"a/build/o"[..], b"a/k.txt", b"b/build/o", b"b/src/m.rs"]
    );
}

#[cfg(unix)]
#[test]
fn exec_batch_runs_once_over_every_root() {
    let d = tree();
    let root = fs::canonicalize(d.path()).unwrap();
    let out = arfind(d.path(), &["-t", "f", "a", "b", "-X", "echo"]);
    let expected: Vec<String> = ["a/build/o", "a/k.txt", "b/build/o", "b/src/m.rs"]
        .iter()
        .map(|p| root.join(p).display().to_string())
        .collect();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        expected.join(" ") + "\n"
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_root_is_reported_under_the_link() {
    let d = tree();
    std::os::unix::fs::symlink("b", d.path().join("lnk")).unwrap();
    let out = arfind(d.path(), &["-t", "f", "lnk", "a"]);
    assert_eq!(
        lines(&out),
        ["a/build/o", "a/k.txt", "lnk/build/o", "lnk/src/m.rs"]
    );
}
