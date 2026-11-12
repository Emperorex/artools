//! By default ardisk counts every file: .git, node_modules,
//! __pycache__ and gitignored entries included. --skip-ignored brings back
//! the filtered view; --ignore, --exclude and --no-hidden apply either way.

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

/// Total bytes (apparent sizes, so exact) of a --json scan of `dir`.
fn total(dir: &Path, extra: &[&str]) -> u64 {
    let mut args = vec![".", "--json", "--apparent-size", "--by-type"];
    args.extend(extra);
    let out = ardisk(dir, &args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    doc["summary"]["total_bytes"].as_u64().unwrap()
}

/// A repository-like tree. Sizes are distinct powers of two times 1000,
/// so each total says exactly which files were counted.
fn repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    for (rel, len) in [
        ("src/main.rs", 1_000),
        (".git/objects/pack", 2_000),
        ("node_modules/pkg/index.js", 4_000),
        ("__pycache__/m.pyc", 8_000),
        ("target/app", 16_000),
        ("vendor/lib.c", 32_000),
        (".cache/c", 64_000),
        (".gitignore", 0),
    ] {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, vec![b'x'; len]).unwrap();
    }
    fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
    dir
}

const GITIGNORE: u64 = "target/\n".len() as u64;
const ALL: u64 = 1_000 + 2_000 + 4_000 + 8_000 + 16_000 + 32_000 + 64_000 + GITIGNORE;

#[test]
fn everything_is_counted_by_default() {
    let d = repo();
    assert_eq!(total(d.path(), &[]), ALL);
}

#[test]
fn skip_ignored_leaves_out_what_a_repository_ignores() {
    let d = repo();
    // .git, node_modules, __pycache__ (built in) and target/ (.gitignore).
    assert_eq!(
        total(d.path(), &["--skip-ignored"]),
        ALL - 2_000 - 4_000 - 8_000 - 16_000
    );
}

#[test]
fn explicit_filters_apply_either_way() {
    let d = repo();
    assert_eq!(total(d.path(), &["--ignore", "vendor"]), ALL - 32_000);
    assert_eq!(total(d.path(), &["--exclude", "vendor/"]), ALL - 32_000);
    // Hidden: .git, .cache and .gitignore itself.
    assert_eq!(
        total(d.path(), &["--no-hidden"]),
        ALL - 2_000 - 64_000 - GITIGNORE
    );
}

#[test]
fn no_ignore_is_still_accepted_as_the_default() {
    let d = repo();
    assert_eq!(total(d.path(), &["--no-ignore"]), ALL);
    let out = ardisk(d.path(), &[".", "--no-ignore", "--skip-ignored"]);
    assert_eq!(out.status.code(), Some(2), "the two contradict each other");
}

#[test]
fn diff_explains_a_snapshot_from_before_the_change() {
    let d = repo();
    let snap = |extra: &[&str]| {
        let mut args = vec![".", "--json", "--top", "0"];
        args.extend(extra);
        let out = ardisk(d.path(), &args);
        assert!(out.status.success());
        out.stdout
    };
    // A filtered snapshot, as older versions took by default.
    fs::write(d.path().join("old.json"), snap(&["--skip-ignored"])).unwrap();
    fs::write(d.path().join("new.json"), snap(&[])).unwrap();
    let out = ardisk(d.path(), &["--diff", "old.json", "new.json"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no_ignore: false vs true"), "{err}");
    assert!(err.contains("--skip-ignored"), "{err}");
}
