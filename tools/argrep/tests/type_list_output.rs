//! End-to-end tests for --type-list.

use std::process::{Command, Output};

fn argrep(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_argrep"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run argrep")
}

#[test]
fn lists_every_builtin_type_with_its_globs() {
    let out = argrep(&["--type-list"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Exactly the shared table, in its order, one type per line.
    assert_eq!(stdout, arcommon::filetypes::type_list());
    assert!(stdout.lines().any(|l| l == "rust: *.rs"));
    assert!(stdout.lines().any(|l| l == "c: *.c, *.h"));
    assert!(out.stderr.is_empty());
}

#[test]
fn every_listed_name_is_accepted_by_type() {
    let out = argrep(&["--type-list"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    for line in stdout.lines() {
        let name = line.split(':').next().unwrap();
        let out = argrep(&["--type", name, "x", dir.path().to_str().unwrap()]);
        assert_ne!(out.status.code(), Some(2), "--type {name} rejected");
    }
}

#[test]
fn needs_no_query_and_takes_no_other_arguments() {
    for args in [
        &["--type-list", "fn"][..],
        &["--type-list", "fn", "."],
        &["--type-list", "-n"],
        &["--type-list", "--type", "rust"],
        &["-i", "--type-list"],
    ] {
        let out = argrep(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn unknown_type_points_to_type_list() {
    let out = argrep(&["--type", "cobol", "x", "."]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("unknown type 'cobol'"), "{err}");
    assert!(err.contains("argrep --type-list"), "{err}");
}
