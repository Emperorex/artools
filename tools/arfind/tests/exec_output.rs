//! End-to-end tests for --exec / --exec-batch with real commands.
#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

fn arfind(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arfind"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run arfind")
}

fn tree(files: &[&str]) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for rel in files {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, b"x\n").unwrap();
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

fn listing(root: &Path) -> Vec<String> {
    let mut v: Vec<String> = walk(root)
        .into_iter()
        .map(|p| p.strip_prefix(root).unwrap().display().to_string())
        .collect();
    v.sort();
    v
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

const HOSTILE: &[&str] = &[
    "logs/x; touch PWNED1.log",
    "logs/$(touch PWNED2).log",
    "logs/`touch PWNED3`.log",
    "logs/-rf.log",
    "logs/new\nline.log",
    "logs/it's \"quoted\".log",
    "logs/keep.txt",
];

#[test]
fn hostile_file_names_are_never_interpreted() {
    let (_d, root) = tree(HOSTILE);
    let out = arfind(&root, &[".", "--name", "*.log", "--exec", "rm"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        listing(&root),
        ["logs/keep.txt"],
        "only the .log files are gone"
    );
}

#[test]
fn dry_run_prints_quoted_commands_and_changes_nothing() {
    let (_d, root) = tree(HOSTILE);
    let before = listing(&root);
    let out = arfind(
        &root,
        &[".", "--name", "*.log", "--dry-run", "--exec", "rm", "-f"],
    );
    assert!(out.status.success());
    assert_eq!(listing(&root), before);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(&format!(
            "rm -f '{}/logs/x; touch PWNED1.log'",
            root.display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("rm -f {}/logs/-rf.log", root.display())),
        "{text}"
    );
}

#[test]
fn arfind_options_after_the_command_are_refused_unless_terminated() {
    let (_d, root) = tree(&["a.log", "b.log"]);
    let before = listing(&root);
    for args in [
        &[".", "--name", "*.log", "--exec", "rm", "-f", "--dry-run"][..],
        &[".", "--exec", "rm", "--name", "*.log"],
        &[".", "-X", "rm", "--type=markdown"],
    ] {
        let out = arfind(&root, args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("after --exec"));
        assert_eq!(listing(&root), before, "nothing may run for {args:?}");
    }
    // With ';' the words are meant for the command.
    let out = arfind(
        &root,
        &[".", "--name", "*.log", "--exec", "echo", "--dry-run", ";"],
    );
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap().lines().count(), 2);
}

#[test]
fn search_finishes_before_commands_run() {
    // Renaming in place: if commands ran while the search was still going,
    // a renamed file could be found again and renamed twice (a.x.x).
    let names: Vec<String> = (0..200).map(|i| format!("d{}/f{i}", i % 7)).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let (_d, root) = tree(&refs);
    let out = arfind(
        &root,
        &[".", "-t", "f", "-j", "8", "--exec", "mv", "{}", "{}.x"],
    );
    assert!(out.status.success());
    let after = listing(&root);
    assert_eq!(after.len(), 200);
    assert!(
        after
            .iter()
            .all(|p| p.ends_with(".x") && !p.ends_with(".x.x")),
        "{after:?}"
    );
}

#[test]
fn commands_run_in_sorted_path_order() {
    let (_d, root) = tree(&["c", "a", "b/z", "b/y"]);
    let out = arfind(&root, &[".", "-t", "f", "--exec", "echo", "{/}"]);
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "a\ny\nz\nc\n");
}

#[test]
fn batch_runs_once_and_not_at_all_without_results() {
    let (_d, root) = tree(&["a.rs", "b.rs", "c.md"]);
    let out = arfind(&root, &[".", "--type", "rust", "-X", "echo", "files:"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert_eq!(
        text.trim_end(),
        format!("files: {0}/a.rs {0}/b.rs", root.display())
    );

    let out = arfind(
        &root,
        &[".", "--name", "*.none", "-X", "echo", "SHOULD-NOT-RUN"],
    );
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
}

#[test]
fn failures_set_the_exit_status_and_are_summarized() {
    let (_d, root) = tree(&["full", "x/also-full"]);
    fs::write(root.join("empty"), b"").unwrap();
    let out = arfind(&root, &[".", "-t", "f", "--exec", "test", "-s"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("1 of 3 commands failed"));

    let out = arfind(
        &root,
        &[".", "-t", "f", "--exec", "arfind-no-such-program-xyz"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot run"));
}

#[test]
fn unsafe_or_meaningless_commands_are_rejected_before_searching() {
    let (_d, root) = tree(&["a"]);
    for args in [
        &[".", "--exec", "{}"][..],
        &[".", "--exec", "./{/}"],
        &[".", "-X", "echo", "{/}"],
        &[".", "--dry-run"],
    ] {
        let out = arfind(&root, args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
    // clap-level conflicts are usage errors (2).
    let out = arfind(&root, &[".", "--count", "--exec", "echo"]);
    assert_eq!(out.status.code(), Some(2));
    let out = arfind(&root, &[".", "--exec", "echo", ";", "-X", "echo"]);
    assert_eq!(out.status.code(), Some(2));
}
