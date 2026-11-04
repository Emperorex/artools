//! Results are reported under PATH as typed, like find and fd, not under
//! the canonical (absolute, symlink-free) root the search walks from.
//! `--exec` arguments are the exception: they stay absolute.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_arfind");

fn arfind(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run arfind")
}

/// Output lines, sorted.
fn lines(out: &Output) -> Vec<String> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut v: Vec<String> = String::from_utf8(out.stdout.clone())
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    v.sort();
    v
}

/// `sub/x.txt` and `-rf` (a name that would pass for an option).
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub/x.txt"), "").unwrap();
    fs::write(dir.path().join("-rf"), "").unwrap();
    dir
}

#[test]
fn without_a_path_entries_have_no_dot_prefix() {
    let d = tree();
    assert_eq!(
        lines(&arfind(d.path(), &[])),
        ["-rf", ".", "sub", "sub/x.txt"]
    );
}

#[test]
fn a_typed_path_is_kept_as_typed() {
    let d = tree();
    assert_eq!(
        lines(&arfind(d.path(), &["."])),
        [".", "./-rf", "./sub", "./sub/x.txt"]
    );
    assert_eq!(lines(&arfind(d.path(), &["sub"])), ["sub", "sub/x.txt"]);
    assert_eq!(lines(&arfind(d.path(), &["sub/"])), ["sub/", "sub/x.txt"]);
}

#[cfg(unix)]
#[test]
fn a_symlinked_root_is_reported_under_the_link() {
    let d = tree();
    std::os::unix::fs::symlink("sub", d.path().join("link")).unwrap();
    assert_eq!(
        lines(&arfind(d.path(), &["link", "-t", "f"])),
        ["link/x.txt"]
    );
}

#[test]
fn nul_lists_keep_the_dot_so_no_name_passes_for_an_option() {
    let d = tree();
    let out = arfind(d.path(), &["-t", "f", "-0"]);
    let mut names: Vec<&[u8]> = out
        .stdout
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .collect();
    names.sort();
    assert_eq!(names, [&b"./-rf"[..], b"./sub/x.txt"]);
}

#[cfg(unix)]
#[test]
fn exec_arguments_stay_absolute() {
    // exec.rs relies on it: no placeholder can expand to "-rf", and {//}
    // always names a real directory.
    let d = tree();
    let root = fs::canonicalize(d.path()).unwrap();
    let out = arfind(d.path(), &["-t", "f", "--exec", "echo", "{}", "{//}"]);
    assert_eq!(
        lines(&out),
        [
            format!("{} {}", root.join("-rf").display(), root.display()),
            format!(
                "{} {}",
                root.join("sub/x.txt").display(),
                root.join("sub").display()
            ),
        ]
    );
}
