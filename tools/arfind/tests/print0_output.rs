//! End-to-end tests for -0/--print0: NUL-terminated paths for `xargs -0`.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

fn arfind(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arfind"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run arfind")
}

/// NUL-terminated records in stdout, as file names, sorted.
fn names(out: &Output) -> Vec<String> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = &out.stdout;
    assert!(
        stdout.is_empty() || stdout.ends_with(b"\0"),
        "every path must end with NUL: {stdout:?}"
    );
    let mut v: Vec<String> = stdout
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| {
            let s = String::from_utf8(r.to_vec()).unwrap();
            Path::new(&s)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    v.sort();
    v
}

fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    let d = dir.path();
    fs::create_dir(d.join("sub dir")).unwrap();
    for name in ["with space.log", "-dash.log", "sub dir/x.log", "keep.txt"] {
        fs::write(d.join(name), "x").unwrap();
    }
    #[cfg(unix)]
    for name in ["new\nline.log", "e\x1b[2J.log"] {
        fs::write(d.join(name), "x").unwrap();
    }
    dir
}

#[test]
fn paths_are_nul_terminated_and_exact() {
    let dir = tree();
    for flag in ["-0", "--print0"] {
        let out = arfind(dir.path(), &[".", "--name", "*.log", flag]);
        let mut want = vec!["-dash.log", "with space.log", "x.log"];
        if cfg!(unix) {
            // Exact bytes: no `\x1b` escaping, the newline stays a newline.
            want.extend(["e\x1b[2J.log", "new\nline.log"]);
        }
        want.sort();
        assert_eq!(names(&out), want, "{flag}");
    }
}

#[test]
fn directories_are_listed_too_and_without_color() {
    let dir = tree();
    let out = arfind(dir.path(), &[".", "-t", "d", "-0"]);
    assert!(names(&out).contains(&"sub dir".to_string()));
    assert!(!out.stdout.contains(&0x1b), "no ANSI color in -0 output");
}

#[test]
fn without_print0_names_stay_escaped_and_newline_separated() {
    let dir = tree();
    let out = arfind(dir.path(), &[".", "--name", "*.log"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(!stdout.contains('\0'));
    if cfg!(unix) {
        assert!(stdout.contains("new\\x0aline.log"), "{stdout:?}");
    }
}

#[test]
fn print0_conflicts_with_count_and_exec_modes() {
    let dir = tree();
    for extra in [
        &["-c"][..],
        &["--exec", "echo", ";"],
        &["-X", "echo", ";"],
        &["--execdir", "echo", ";"],
    ] {
        let mut args = vec![".", "-0"];
        args.extend_from_slice(extra);
        let out = arfind(dir.path(), &args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn many_paths_are_all_written() {
    // Output is buffered and flushed once at the end; nothing may be lost.
    let dir = TempDir::new().unwrap();
    for i in 0..3000 {
        fs::write(dir.path().join(format!("f{i:04}.txt")), "").unwrap();
    }
    let out = arfind(dir.path(), &[".", "-t", "f", "-0"]);
    assert_eq!(names(&out).len(), 3000);
}

#[cfg(unix)]
#[test]
fn xargs_dash_zero_gets_every_path_back() {
    let dir = tree();
    let out = Command::new("sh")
        .current_dir(dir.path())
        .arg("-c")
        .arg(format!(
            "'{}' . --name '*.log' -0 | xargs -0 -n1 printf '%s\\0'",
            env!("CARGO_BIN_EXE_arfind")
        ))
        .output()
        .unwrap();
    let got = names(&out);
    assert_eq!(got.len(), 5, "{got:?}");
    assert!(got.iter().any(|n| n == "new\nline.log"));
}
