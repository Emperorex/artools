//! End-to-end tests for coloring results by entry kind.
//!
//! Color is only produced on a terminal; `CLICOLOR_FORCE=1` turns it on for
//! a pipe, which is what these tests read.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

const BOLD_BLUE: &str = "\x1b[1;34m";
const BOLD_CYAN: &str = "\x1b[1;36m";
const YELLOW: &str = "\x1b[33m";
const BOLD_MAGENTA: &str = "\x1b[1;35m";
const RESET: &str = "\x1b[0m";

fn arfind(dir: &Path, args: &[&str], force_color: bool) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_arfind"));
    cmd.current_dir(dir).args(args).env_remove("NO_COLOR");
    cmd.env_remove("CLICOLOR").env_remove("CLICOLOR_FORCE");
    if force_color {
        cmd.env("CLICOLOR_FORCE", "1");
    }
    cmd.output().expect("failed to run arfind")
}

/// The output line for the entry whose path ends with `/name`.
fn line_for<'a>(stdout: &'a str, name: &str) -> &'a str {
    let suffix = format!("/{name}");
    stdout
        .lines()
        .find(|l| l.trim_end_matches(RESET).ends_with(&suffix))
        .unwrap_or_else(|| panic!("no line for {name} in:\n{stdout}"))
}

/// file, dir, link -> file, dirlink -> dir (and on Unix a FIFO and a socket).
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    let d = dir.path();
    fs::write(d.join("file"), "x").unwrap();
    fs::create_dir(d.join("dir")).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("file", d.join("link")).unwrap();
        std::os::unix::fs::symlink("dir", d.join("dirlink")).unwrap();
        let fifo =
            std::ffi::CString::new(d.join("fifo").into_os_string().into_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        // The listener is dropped at once; the socket file stays.
        std::os::unix::net::UnixListener::bind(d.join("sock")).unwrap();
    }
    dir
}

#[test]
fn each_kind_gets_its_color() {
    let dir = tree();
    let out = arfind(dir.path(), &[], true);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();

    let dir_line = line_for(&stdout, "dir");
    assert!(dir_line.starts_with(BOLD_BLUE), "{dir_line:?}");
    let file_line = line_for(&stdout, "file");
    assert!(
        !file_line.contains('\x1b'),
        "files stay uncolored: {file_line:?}"
    );

    #[cfg(unix)]
    {
        let link = line_for(&stdout, "link");
        assert!(link.starts_with(BOLD_CYAN), "{link:?}");
        // A symlink to a directory is a symlink, as in ls.
        let dirlink = line_for(&stdout, "dirlink");
        assert!(dirlink.starts_with(BOLD_CYAN), "{dirlink:?}");
        let fifo = line_for(&stdout, "fifo");
        assert!(fifo.starts_with(YELLOW), "{fifo:?}");
        let sock = line_for(&stdout, "sock");
        assert!(sock.starts_with(BOLD_MAGENTA), "{sock:?}");
    }
    for l in stdout.lines().filter(|l| l.contains('\x1b')) {
        assert!(l.ends_with(RESET), "color must be reset per line: {l:?}");
    }
}

#[test]
fn sorted_output_is_colored_the_same_way() {
    let dir = tree();
    let unsorted = arfind(dir.path(), &[], true);
    let sorted = arfind(dir.path(), &["--sort", "path"], true);
    let mut a: Vec<String> = String::from_utf8(unsorted.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    a.sort();
    let mut b: Vec<String> = String::from_utf8(sorted.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    b.sort();
    assert_eq!(a, b);
}

#[test]
fn no_color_when_piped() {
    let dir = tree();
    let out = arfind(dir.path(), &[], false);
    assert!(out.status.success());
    assert!(!out.stdout.contains(&0x1b), "no ANSI codes in piped output");
}

#[test]
fn print0_is_never_colored() {
    let dir = tree();
    let out = arfind(dir.path(), &["-0"], true);
    assert!(out.status.success());
    assert!(!out.stdout.contains(&0x1b), "no ANSI codes with -0");
}

#[cfg(unix)]
#[test]
fn escaped_names_keep_their_color() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("e\x1b[2Jdir")).unwrap();
    let out = arfind(dir.path(), &["-t", "d", "--name", "e*"], true);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let line = line_for(&stdout, "e\\x1b[2Jdir");
    assert!(line.starts_with(BOLD_BLUE), "{line:?}");
    // The only ESC bytes are the color codes, not the name's own.
    assert_eq!(line.matches('\x1b').count(), 2, "{line:?}");
}
