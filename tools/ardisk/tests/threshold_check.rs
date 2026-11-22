//! An invalid --threshold is reported before anything is scanned, like
//! the other invalid values: a typo must not cost a walk of the whole tree
//! first.

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

fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub/a.bin"), vec![0u8; 4096]).unwrap();
    dir
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn an_invalid_threshold_stops_before_the_scan() {
    let d = tree();
    // --progress reports the scan ("scanned: ...", a final line when
    // stderr is not a terminal); nothing of it may appear.
    let out = ardisk(d.path(), &[".", "--threshold", "10XB", "--progress"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_eq!(
        stderr(&out),
        "error: Unknown unit 'XB'. Use B, KB, MB, GB, or TB.\n"
    );

    // No JSON document either.
    let out = ardisk(d.path(), &[".", "--threshold", "10XB", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn it_is_checked_before_the_path() {
    // As --include and --exclude are: the value is wrong whatever PATH is.
    let d = tree();
    let out = ardisk(d.path(), &["missing", "--threshold", "10XB"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("Unknown unit 'XB'"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn with_inodes_it_must_be_a_count() {
    let d = tree();
    let out = ardisk(
        d.path(),
        &[".", "--inodes", "--threshold", "10MB", "--progress"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(
        stderr(&out).starts_with("error: Invalid threshold '10MB'. With --inodes"),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("scanned"), "{}", stderr(&out));
}

#[test]
fn a_valid_threshold_still_filters() {
    let d = tree();
    // The scan this test file relies on not seeing, when there is one.
    let out = ardisk(d.path(), &[".", "--threshold", "1B", "--progress"]);
    assert!(stderr(&out).contains("scanned:"), "{}", stderr(&out));

    let out = ardisk(d.path(), &[".", "--threshold", "1B", "--apparent-size"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("sub"), "{text}");

    let out = ardisk(d.path(), &[".", "--threshold", "1TB"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("sub"));
}
