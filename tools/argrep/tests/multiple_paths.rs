//! Several PATH arguments: `argrep QUERY a b c`. Each is searched, results
//! are reported under the PATH they were found under, as typed, and a
//! PATH that doesn't exist is reported by name without stopping the rest.

use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_argrep");

fn argrep(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run argrep")
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

/// `a/x.txt`, `b/y.txt`, `c/sub/z.txt` and `top.txt`, each with "needle".
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    for rel in ["a/x.txt", "b/y.txt", "c/sub/z.txt", "top.txt"] {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, "needle\n").unwrap();
    }
    dir
}

#[test]
fn every_path_is_searched_and_reported_as_typed() {
    let d = tree();
    let out = argrep(d.path(), &["-l", "needle", "a", "./b/", "c/sub", "top.txt"]);
    assert!(out.status.success());
    assert_eq!(
        lines(&out),
        ["./b/y.txt", "a/x.txt", "c/sub/z.txt", "top.txt"]
    );
}

#[test]
fn a_missing_path_is_named_and_the_rest_still_searched() {
    let d = tree();
    let out = argrep(d.path(), &["-l", "needle", "a", "missing", "b"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(lines(&out), ["a/x.txt", "b/y.txt"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("argrep: missing: No such file or directory"),
        "{err}"
    );

    // -q: an error is exit 2, even though there was a match.
    let out = argrep(d.path(), &["-q", "needle", "missing", "a"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn files_and_e_take_every_positional_as_a_path() {
    let d = tree();
    let out = argrep(d.path(), &["--files", "a", "b"]);
    assert_eq!(lines(&out), ["a/x.txt", "b/y.txt"]);
    let out = argrep(d.path(), &["-l", "-e", "needle", "a", "b"]);
    assert_eq!(lines(&out), ["a/x.txt", "b/y.txt"]);
}

#[test]
fn nested_paths_are_searched_once_each_under_the_deeper_path() {
    // Like grep -r, a file under two given paths is found twice.
    let d = tree();
    let out = argrep(d.path(), &["-c", "needle", "c", "c/sub"]);
    assert_eq!(lines(&out), ["c/sub/z.txt: 1", "c/sub/z.txt: 1"]);
}

#[test]
fn json_reports_each_path_as_typed() {
    let d = tree();
    let out = argrep(d.path(), &["--json", "needle", "a", "b"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(r#""path":"a/x.txt""#), "{text}");
    assert!(text.contains(r#""path":"b/y.txt""#), "{text}");
    assert!(text.contains(r#""files_searched":2"#), "{text}");
}

#[test]
fn stdin_cannot_be_mixed_with_other_paths() {
    let d = tree();
    let out = argrep(d.path(), &["needle", "-", "a"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("can't be combined"));
}
