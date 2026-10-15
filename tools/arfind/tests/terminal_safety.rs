//! SAST F3: file names with control characters must never reach the
//! terminal raw.
#![cfg(unix)]

use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

/// Sets the window title (OSC 0) and clears the screen (CSI 2J).
const EVIL: &str = "x\x1b]0;PWNED\x07\x1b[2J.txt";
const EVIL_ESCAPED: &str = "x\\x1b]0;PWNED\\x07\\x1b[2J.txt";

fn arfind(dir: &Path, args: &[&str]) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_arfind"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run arfind");
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
    let esc = dir.path().join("esc");
    fs::create_dir_all(esc.join("d\x1b[31m")).unwrap();
    fs::write(esc.join(EVIL), "x\n").unwrap();
    fs::write(esc.join("d\x1b[31m").join("ok.txt"), "x\n").unwrap();
    dir
}

#[test]
fn listed_file_and_directory_names_are_escaped() {
    let dir = evil_tree();
    let (out, err) = arfind(dir.path(), &["esc"]);
    assert_no_controls("stdout", &out);
    assert_no_controls("stderr", &err);
    assert!(out.contains(EVIL_ESCAPED), "{out:?}");
    assert!(out.contains("d\\x1b[31m"), "{out:?}");
}

#[test]
fn dry_run_lines_are_safe_and_pasteable() {
    let dir = evil_tree();
    let (out, _) = arfind(
        dir.path(),
        &["esc", "--name", "x*", "--dry-run", "--exec", "echo"],
    );
    assert_no_controls("--dry-run stdout", &out);
    let line = out
        .lines()
        .find(|l| l.contains("PWNED"))
        .expect("dry-run line for the hostile file");
    assert!(line.starts_with("echo $'"), "{line:?}");
    assert!(
        line.ends_with("/esc/x\\x1b]0;PWNED\\x07\\x1b[2J.txt'"),
        "{line:?}"
    );

    // Pasted into a shell, the line reproduces the exact name.
    let shell = Command::new("bash").arg("-c").arg(line).output();
    if let Ok(shell) = shell {
        let echoed = String::from_utf8(shell.stdout).unwrap();
        assert!(
            echoed.trim_end().ends_with(&format!("/esc/{EVIL}")),
            "{echoed:?}"
        );
    }
}

#[test]
fn exec_still_receives_the_exact_name() {
    // Escaping is for display only: the command gets the real bytes.
    let dir = evil_tree();
    let out = Command::new(env!("CARGO_BIN_EXE_arfind"))
        .current_dir(dir.path())
        .args(["esc", "--name", "x*", "--exec", "test", "-f", ";"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
