//! Results are reported under PATH as typed, like grep and rg, not under
//! the canonical (absolute, symlink-free) root the search walks from.

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

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

/// `src/a.rs` and `-rf` (a name that would pass for an option), both
/// containing "needle".
fn project() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/a.rs"), "needle\n").unwrap();
    fs::write(dir.path().join("-rf"), "needle\n").unwrap();
    dir
}

#[test]
fn a_relative_path_stays_relative() {
    let d = project();
    assert_eq!(
        stdout(&argrep(d.path(), &["needle", "src"])),
        "src/a.rs:needle\n"
    );
    assert_eq!(
        stdout(&argrep(d.path(), &["-l", "needle", "./src/"])),
        "./src/a.rs\n"
    );
}

#[test]
fn a_typed_dot_keeps_its_prefix() {
    let d = project();
    let out = stdout(&argrep(d.path(), &["-l", "needle", "."]));
    let mut lines: Vec<&str> = out.lines().collect();
    lines.sort();
    assert_eq!(lines, ["./-rf", "./src/a.rs"]);
}

#[test]
fn a_named_file_is_reported_as_named() {
    let d = project();
    assert_eq!(
        stdout(&argrep(d.path(), &["-c", "needle", "src/a.rs"])),
        "src/a.rs: 1\n"
    );
}

#[test]
fn an_absolute_path_stays_absolute() {
    let d = project();
    let root = fs::canonicalize(d.path()).unwrap();
    let src = root.join("src");
    let out = stdout(&argrep(d.path(), &["-l", "needle", src.to_str().unwrap()]));
    assert_eq!(out, format!("{}\n", src.join("a.rs").display()));
}

#[cfg(unix)]
#[test]
fn a_symlinked_root_is_reported_under_the_link() {
    let d = project();
    std::os::unix::fs::symlink("src", d.path().join("link")).unwrap();
    assert_eq!(
        stdout(&argrep(d.path(), &["-l", "needle", "link"])),
        "link/a.rs\n"
    );
}

#[test]
fn without_a_path_there_is_no_dot_prefix_except_in_nul_lists() {
    let d = project();
    // --files is the one mode that walks "." rather than reading stdin
    // when no PATH is given.
    let out = stdout(&argrep(d.path(), &["--files"]));
    let mut lines: Vec<&str> = out.lines().collect();
    lines.sort();
    assert_eq!(lines, ["-rf", "src/a.rs"]);

    // -0 output is for `xargs -0`: "./-rf", never a bare "-rf".
    let out = argrep(d.path(), &["--files", "-0"]);
    let mut names: Vec<&[u8]> = out
        .stdout
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .collect();
    names.sort();
    assert_eq!(names, [&b"./-rf"[..], b"./src/a.rs"]);
}

#[test]
fn json_paths_follow_the_typed_path() {
    let d = project();
    let out = stdout(&argrep(d.path(), &["--json", "needle", "src"]));
    assert!(
        out.contains(r#""path":"src/a.rs""#),
        "unexpected JSON:\n{out}"
    );
}
