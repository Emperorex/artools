//! A directory with many files is split into tasks, so all workers search
//! it rather than the one that listed it. Every file is still searched
//! exactly once, with every filter, and the PATHs are searched in order.

use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_argrep");

fn argrep(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run argrep")
}

fn lines(out: &Output) -> Vec<String> {
    let mut v: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    v.sort();
    v
}

/// `big/` with 1000 files (several tasks' worth), every 10th holding
/// "needle", plus a subdirectory listed among them, and every 100th file
/// large (for --max-filesize).
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    let big = dir.path().join("big");
    fs::create_dir_all(big.join("m_sub")).unwrap();
    fs::write(big.join("m_sub/inner.txt"), "needle\n").unwrap();
    for i in 0..1000 {
        let mut text = if i % 10 == 0 { "needle\n" } else { "hay\n" }.to_string();
        if i % 100 == 0 {
            text.push_str(&"x".repeat(5000));
        }
        fs::write(big.join(format!("f{i:04}.txt")), text).unwrap();
    }
    dir
}

#[test]
fn every_file_is_searched_once() {
    let d = tree();
    for jobs in ["1", "4"] {
        let out = argrep(d.path(), &["-j", jobs, "-c", "needle", "big"]);
        assert!(out.status.success());
        let counts = lines(&out);
        assert_eq!(counts.len(), 1001, "-j{jobs}");
        let found = counts.iter().filter(|l| l.ends_with(": 1")).count();
        assert_eq!(found, 101, "-j{jobs}");

        let out = argrep(d.path(), &["-j", jobs, "-l", "needle", "big"]);
        assert_eq!(lines(&out).len(), 101, "-j{jobs}");
        assert!(lines(&out).contains(&"big/m_sub/inner.txt".to_string()));

        let out = argrep(d.path(), &["-j", jobs, "--files", "big"]);
        assert_eq!(lines(&out).len(), 1001, "-j{jobs}");
    }
}

#[test]
fn max_filesize_applies_in_every_task() {
    let d = tree();
    let out = argrep(
        d.path(),
        &["-j4", "-l", "--max-filesize", "1KB", "needle", "big"],
    );
    // The ten large files (all with "needle") are skipped.
    assert_eq!(lines(&out).len(), 101 - 10);
}

#[test]
fn quiet_still_stops_at_the_first_match() {
    let d = tree();
    let out = argrep(d.path(), &["-j4", "-q", "needle", "big"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    let out = argrep(d.path(), &["-j4", "-q", "absent", "big"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn paths_are_searched_in_the_order_given() {
    // With one worker, results come in the order the PATHs were typed.
    let d = TempDir::new().unwrap();
    for name in ["c", "a", "b"] {
        fs::create_dir(d.path().join(name)).unwrap();
        fs::write(d.path().join(name).join("f.txt"), "needle\n").unwrap();
    }
    let out = argrep(d.path(), &["-j1", "-l", "needle", "c", "a", "b"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "c/f.txt\na/f.txt\nb/f.txt\n"
    );
}
