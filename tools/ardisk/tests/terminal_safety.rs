//! SAST F3: file names with control characters must never reach the
//! terminal raw. `--json` keeps the exact names (JSON escapes them).
#![cfg(unix)]

use std::{fs, process::Command};
use tempfile::TempDir;

/// Sets the window title (OSC 0) and clears the screen (CSI 2J).
const EVIL: &str = "x\x1b]0;PWNED\x07\x1b[2J.txt";
const EVIL_ESCAPED: &str = "x\\x1b]0;PWNED\\x07\\x1b[2J.txt";
const EVIL_DIR: &str = "d\x1b[31m";
const EVIL_DIR_ESCAPED: &str = "d\\x1b[31m";

fn ardisk(args: &[&str]) -> (String, String) {
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
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn assert_no_controls(what: &str, s: &str) {
    assert!(
        !s.chars().any(|c| c.is_control() && c != '\n'),
        "{what}: raw control character in output: {s:?}"
    );
}

fn evil_tree() -> (TempDir, String) {
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let d = root.join(EVIL_DIR);
    fs::create_dir(&d).unwrap();
    // Two identical files so --duplicates has a group to print.
    fs::write(d.join(EVIL), vec![b'z'; 5000]).unwrap();
    fs::write(d.join(format!("copy-{EVIL}")), vec![b'z'; 5000]).unwrap();
    // An extension with control characters, for --by-type.
    fs::write(root.join("f.\x1b[2Jx"), b"data").unwrap();
    let root = root.to_str().unwrap().to_string();
    (dir, root)
}

fn check(args: &[&str], must_contain: &str) {
    let (out, err) = ardisk(args);
    assert_no_controls(&format!("{args:?} stdout"), &out);
    assert_no_controls(&format!("{args:?} stderr"), &err);
    assert!(out.contains(must_contain), "{args:?}: {out:?}");
}

#[test]
fn every_text_report_escapes_names() {
    let (_dir, root) = evil_tree();
    let r = root.as_str();
    check(&[r], EVIL_DIR_ESCAPED);
    check(&[r, "--inodes"], EVIL_DIR_ESCAPED);
    check(&[r, "--bar=always"], EVIL_DIR_ESCAPED);
    check(&[r, "--tree"], EVIL_DIR_ESCAPED);
    check(&[r, "--largest-files", "5"], EVIL_ESCAPED);
    check(&[r, "--duplicates"], EVIL_ESCAPED);
    check(&[r, "--by-type"], "\\x1b[2jx");
}

#[test]
fn json_keeps_exact_names() {
    let (_dir, root) = evil_tree();
    let (out, _) = ardisk(&[&root, "--largest-files", "5", "--json"]);
    // JSON escapes control characters itself; no raw ESC on stdout...
    assert_no_controls("--json stdout", &out);
    // ...and a JSON parser gets the exact original name back.
    assert!(
        out.contains("\\u001b]0;PWNED\\u0007\\u001b[2J.txt"),
        "{out:?}"
    );
    assert!(
        !out.contains(EVIL_ESCAPED),
        "JSON must not double-escape: {out:?}"
    );
}
