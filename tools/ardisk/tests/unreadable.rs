#![cfg(unix)]
//! A directory that can't be read is reported as in argrep and arfind: the
//! report covers the rest, a summary goes to stderr, the exit code is 1,
//! and --json says how many could not be read. With --debug, the
//! directory and the reason are named.
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

const BIN: &str = env!("CARGO_BIN_EXE_ardisk");
const SUMMARY: &str = "ardisk: 1 file(s)/director(ies) could not be read";

fn ardisk(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run ardisk")
}

/// `ok/a.txt` (1000 bytes) and `locked/` (mode 000, holding 50 KB). None
/// if the locked directory is readable anyway (running as root).
fn tree() -> Option<(TempDir, PathBuf)> {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("ok")).unwrap();
    fs::write(dir.path().join("ok/a.txt"), vec![b'x'; 1000]).unwrap();
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("big"), vec![b'x'; 50_000]).unwrap();
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
fn every_mode_reports_the_rest_then_exits_1() {
    let Some((d, locked)) = tree() else { return };
    let runs: Vec<(Vec<&str>, Output)> = [
        vec!["."],
        vec![".", "--tree"],
        vec![".", "--inodes"],
        vec![".", "--summarize"],
        vec![".", "--largest-files", "3"],
        vec![".", "--by-type"],
        vec![".", "--duplicates"],
    ]
    .into_iter()
    .map(|args| {
        let out = ardisk(d.path(), &args);
        (args, out)
    })
    .collect();
    unlock(&locked);

    for (args, out) in runs {
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            !out.stdout.is_empty(),
            "{args:?}: the rest is still reported"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(SUMMARY), "{args:?}: {err}");
    }
}

#[test]
fn json_counts_what_could_not_be_read() {
    let Some((d, locked)) = tree() else { return };
    let partial = ardisk(d.path(), &[".", "--json", "--apparent-size"]);
    let complete = ardisk(d.path(), &["ok", "--json"]);
    unlock(&locked);

    assert_eq!(partial.status.code(), Some(1));
    let doc: serde_json::Value = serde_json::from_slice(&partial.stdout).unwrap();
    assert_eq!(doc["errors"], 1);
    // The locked directory's 50 KB are not in the total.
    assert!(doc["summary"]["total_bytes"].as_u64().unwrap() < 50_000);

    assert!(complete.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&complete.stdout).unwrap();
    assert_eq!(doc["errors"], 0);
}

#[test]
fn debug_names_the_directory_and_the_reason() {
    let Some((d, locked)) = tree() else { return };
    // No PATH, so the directory is shown as `locked`, not `./locked`.
    let out = ardisk(d.path(), &["--debug"]);
    unlock(&locked);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("ardisk: locked: Permission denied"), "{err}");
    assert!(
        err.contains(SUMMARY) && !err.contains("rerun with --debug"),
        "{err}"
    );
}
