//! SAST F3: file names with control characters must never reach the
//! terminal raw. Runs the real binary on a tree whose names carry the
//! escape sequences from the report and checks every output mode.
#![cfg(unix)]

use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

/// Sets the window title (OSC 0) and clears the screen (CSI 2J).
const EVIL: &str = "x\x1b]0;PWNED\x07\x1b[2J.txt";
const EVIL_ESCAPED: &str = "x\\x1b]0;PWNED\\x07\\x1b[2J.txt";

fn argrep(dir: &Path, args: &[&str]) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_argrep"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run argrep");
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

fn evil_tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("esc")).unwrap();
    fs::write(dir.path().join("esc").join(EVIL), "secret one\nother\n").unwrap();
    // A directory with a hostile name, holding a file without a match.
    let sub = dir.path().join("esc").join("d\x1b[31m");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("plain.txt"), "nothing here\n").unwrap();
    dir
}

#[test]
fn file_names_are_escaped_in_every_output_mode() {
    let dir = evil_tree();
    for args in [
        &["secret", "esc"][..],
        &["-n", "secret", "esc"],
        &["-A", "1", "secret", "esc"],
        &["-o", "secret", "esc"],
        &["-l", "secret", "esc"],
        &["-c", "secret", "esc"],
        &["--files", "esc"],
    ] {
        let (out, err) = argrep(dir.path(), args);
        assert_no_controls(&format!("{args:?} stdout"), &out);
        assert_no_controls(&format!("{args:?} stderr"), &err);
        assert!(out.contains(EVIL_ESCAPED), "{args:?}: {out:?}");
    }
}

#[test]
fn directory_names_are_escaped() {
    let dir = evil_tree();
    let (out, err) = argrep(dir.path(), &["-L", "secret", "esc"]);
    assert_no_controls("-L stdout", &out);
    assert_no_controls("-L stderr", &err);
    assert!(out.contains("d\\x1b[31m/plain.txt"), "{out:?}");
}

#[test]
fn debug_messages_on_stderr_are_escaped() {
    let dir = evil_tree();
    let fifo = dir.path().join("esc").join("p\x1b[2J");
    let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(status.success());
    let (_, err) = argrep(dir.path(), &["--debug", "secret", "esc"]);
    assert_no_controls("--debug stderr", &err);
    assert!(err.contains("p\\x1b[2J"), "{err:?}");
}

#[test]
fn c1_controls_in_content_are_escaped() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "secret \u{9b}2J here\n").unwrap();
    let (out, _) = argrep(dir.path(), &["secret", "a.txt"]);
    assert_no_controls("stdout", &out);
    assert!(out.contains("secret \\u{9b}2J here"), "{out:?}");
}
