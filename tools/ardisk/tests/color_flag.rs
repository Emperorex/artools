//! `--color auto|always|never`, decided separately for stdout and stderr
//! (see arcommon::color). The per-stream terminal cases are tested with a
//! pseudo-terminal in arfind's color_flag.rs; this covers ardisk's flag.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ardisk");
const ESC: u8 = 0x1b;

/// ardisk in `dir` with a clean color environment, plus `env`.
fn ardisk(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.current_dir(dir)
        .args(args)
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE")
        .env("TERM", "xterm-256color");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run ardisk")
}

fn colored(bytes: &[u8]) -> bool {
    bytes.contains(&ESC)
}

/// A 2 MB file, so its size is printed in the colored MB tier.
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("d")).unwrap();
    fs::write(dir.path().join("d/big"), vec![0u8; 2_000_000]).unwrap();
    dir
}

/// The report on stdout; an invalid glob reported on stderr.
const REPORT: &[&str] = &["d"];
const BAD_GLOB: &[&str] = &["d", "--include", "["];

#[test]
fn auto_is_plain_into_pipes() {
    let d = tree();
    assert!(!colored(&ardisk(d.path(), REPORT, &[]).stdout));
    assert!(!colored(&ardisk(d.path(), BAD_GLOB, &[]).stderr));
}

#[test]
fn always_colors_both_streams_into_pipes() {
    let d = tree();
    let out = ardisk(d.path(), &["d", "--color", "always"], &[]);
    assert!(
        colored(&out.stdout),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let out = ardisk(d.path(), &["d", "--include", "[", "--color=always"], &[]);
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
    assert!(colored(&ardisk(d.path(), REPORT, &force).stdout));
    assert!(!colored(
        &ardisk(d.path(), &["d", "--color", "never"], &force).stdout
    ));
    let out = ardisk(
        d.path(),
        &["d", "--include", "[", "--color", "never"],
        &force,
    );
    assert!(!colored(&out.stderr));
}

#[test]
fn always_wins_over_no_color() {
    let d = tree();
    let out = ardisk(d.path(), &["d", "--colour", "always"], &[("NO_COLOR", "1")]);
    assert!(colored(&out.stdout));
}

#[test]
fn json_is_never_colored() {
    let d = tree();
    let out = ardisk(d.path(), &["d", "--json", "--color", "always"], &[]);
    assert!(out.status.success());
    assert!(!colored(&out.stdout));
}
