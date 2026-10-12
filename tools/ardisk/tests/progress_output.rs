//! End-to-end tests for `--progress`. In tests stderr is a pipe, so the
//! reporter runs in its log mode: plain lines, no control characters.

use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

fn ardisk(args: &[&str]) -> Output {
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
    out
}

fn sample() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for (rel, len, seed) in [
        ("a/one.bin", 4000, 1u8),
        ("a/b/two.bin", 4000, 1),
        ("c/three.txt", 1500, 2),
        ("c/d/e/four.txt", 10, 3),
    ] {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, vec![seed; len]).unwrap();
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

#[test]
fn progress_never_changes_stdout() {
    let (_d, root) = sample();
    for extra in [
        vec![],
        vec!["--tree"],
        vec!["--inodes"],
        vec!["--by-type"],
        vec!["--largest-files", "3"],
        vec!["--duplicates"],
        vec!["--json"],
        vec!["--json", "--duplicates"],
    ] {
        let mut plain = vec![p(&root), "--apparent-size"];
        plain.extend(extra.iter());
        let mut with = plain.clone();
        with.push("--progress");
        // --apparent-size conflicts with --inodes; drop it there.
        if extra.contains(&"--inodes") {
            plain.retain(|a| *a != "--apparent-size");
            with.retain(|a| *a != "--apparent-size");
        }
        assert_eq!(
            ardisk(&with).stdout,
            ardisk(&plain).stdout,
            "stdout differs for {extra:?}"
        );
    }
}

#[test]
fn without_progress_stderr_stays_empty() {
    let (_d, root) = sample();
    assert_eq!(stderr(&ardisk(&[p(&root)])), "");
}

#[test]
fn final_line_matches_the_report_and_has_no_control_characters() {
    let (_d, root) = sample();
    let out = ardisk(&[p(&root), "--json", "--summarize", "--progress"]);
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    let err = stderr(&out);
    assert!(!err.contains('\x1b') && !err.contains('\r'), "{err:?}");

    let last = err.lines().last().expect("a final line");
    let dirs = doc["summary"]["directories"].as_u64().unwrap();
    let files = doc["summary"]["files"].as_u64().unwrap();
    assert!(
        last.starts_with(&format!("scanned: {dirs} dirs, {files} files, ")),
        "{last:?}"
    );
}

#[test]
fn inode_mode_leaves_out_the_size() {
    let (_d, root) = sample();
    let err = stderr(&ardisk(&[p(&root), "--inodes", "-s", "--progress"]));
    let last = err.lines().last().unwrap();
    assert!(!last.contains('B'), "no size in inode mode: {last:?}");
}

#[test]
fn duplicates_announce_the_comparison_phase() {
    let (_d, root) = sample();
    let err = stderr(&ardisk(&[p(&root), "--duplicates", "--progress"]));
    let lines: Vec<&str> = err.lines().collect();
    assert!(lines[0].starts_with("scanned: "), "{err}");
    assert_eq!(lines[1], "comparing 4 candidate files...");
}
