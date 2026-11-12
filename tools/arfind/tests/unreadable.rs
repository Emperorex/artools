#![cfg(unix)]
//! A directory that can't be read is reported as in argrep: the rest is
//! still listed, a summary goes to stderr, and the exit code is 1. With
//! --debug, the directory and the reason are named.
//!
//! Root can read any directory, so these tests skip themselves when run
//! as root.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_arfind");
const SUMMARY: &str = "arfind: 1 file(s)/director(ies) could not be read";

fn arfind(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run arfind")
}

/// `ok/a.txt` and `locked/` (mode 000, holding `b.txt`). None if the
/// locked directory is readable anyway (running as root).
fn tree() -> Option<(TempDir, PathBuf)> {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("ok")).unwrap();
    fs::write(dir.path().join("ok/a.txt"), "").unwrap();
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("b.txt"), "").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        eprintln!("skipped: running with permission to read any directory");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        return None;
    }
    Some((dir, locked))
}

/// Puts the permissions back so the temporary directory can be removed.
fn unlock(locked: &Path) {
    fs::set_permissions(locked, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn the_rest_is_listed_then_reported_with_exit_1() {
    let Some((d, locked)) = tree() else { return };
    let out = arfind(d.path(), &["-t", "f"]);
    let count = arfind(d.path(), &["-t", "f", "--count"]);
    unlock(&locked);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok/a.txt\n");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(SUMMARY), "{err}");
    assert!(err.contains("rerun with --debug"), "{err}");

    // --count still prints what it found, and says the count is partial.
    assert_eq!(count.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&count.stdout), "1\n");
}

#[test]
fn debug_names_the_directory_and_the_reason() {
    let Some((d, locked)) = tree() else { return };
    let out = arfind(d.path(), &["-t", "f", "--debug"]);
    unlock(&locked);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("arfind: locked: Permission denied"), "{err}");
    assert!(
        err.contains(SUMMARY) && !err.contains("rerun with --debug"),
        "{err}"
    );
}

#[test]
fn a_readable_tree_exits_0_without_a_summary() {
    let Some((d, locked)) = tree() else { return };
    let out = arfind(d.path(), &["ok"]);
    unlock(&locked);
    assert!(out.status.success());
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
