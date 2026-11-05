//! `--color auto|always|never`, decided separately for stdout and stderr
//! (see arcommon::color). The per-stream terminal cases are tested with a
//! pseudo-terminal in arfind's color_flag.rs; this covers argrep's flag.

use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_argrep");
const ESC: u8 = 0x1b;

/// argrep in `dir` with a clean color environment, plus `env`.
fn argrep(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.current_dir(dir)
        .args(args)
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE")
        .env("TERM", "xterm-256color")
        .stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run argrep")
}

fn colored(bytes: &[u8]) -> bool {
    bytes.contains(&ESC)
}

fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
    dir
}

/// A match on stdout, and an unreadable path reported on stderr.
const HIT: &[&str] = &["needle", "a.txt"];
const MISS: &[&str] = &["needle", "missing"];

#[test]
fn auto_is_plain_into_pipes() {
    let d = tree();
    assert!(!colored(&argrep(d.path(), HIT, &[]).stdout));
    assert!(!colored(&argrep(d.path(), MISS, &[]).stderr));
}

#[test]
fn always_colors_both_streams_into_pipes() {
    let d = tree();
    let out = argrep(d.path(), &["--color", "always", "needle", "a.txt"], &[]);
    assert!(
        colored(&out.stdout),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let out = argrep(d.path(), &["--color=always", "needle", "missing"], &[]);
    assert!(
        colored(&out.stderr),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn never_wins_over_clicolor_force() {
    let d = tree();
    let force = [("CLICOLOR_FORCE", "1")];
    assert!(colored(&argrep(d.path(), HIT, &force).stdout));
    let out = argrep(d.path(), &["--color", "never", "needle", "a.txt"], &force);
    assert!(!colored(&out.stdout));
    let out = argrep(d.path(), &["--color", "never", "needle", "missing"], &force);
    assert!(!colored(&out.stderr));
}

#[test]
fn always_wins_over_no_color() {
    let d = tree();
    let out = argrep(
        d.path(),
        &["--color", "always", "needle", "a.txt"],
        &[("NO_COLOR", "1")],
    );
    assert!(colored(&out.stdout));
}

#[test]
fn the_value_is_never_taken_as_the_pattern() {
    // `--color never needle` must not search for "never".
    let d = tree();
    let out = argrep(d.path(), &["--color", "never", "needle", "a.txt"], &[]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a.txt:needle\n");
    let out = argrep(d.path(), &["--colour", "never", "needle", "a.txt"], &[]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a.txt:needle\n");
}

#[test]
fn machine_output_is_never_colored() {
    let d = tree();
    for args in [
        &["--color", "always", "-l0", "needle", "."][..],
        &["--color", "always", "--json", "needle", "."],
    ] {
        let out = argrep(d.path(), args, &[]);
        assert!(out.status.success());
        assert!(!colored(&out.stdout), "{args:?}");
    }
}

#[test]
fn bad_values_are_rejected() {
    let d = tree();
    let out = argrep(d.path(), &["--color", "sometimes", "needle", "a.txt"], &[]);
    assert_eq!(out.status.code(), Some(2));
}
