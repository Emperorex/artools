//! End-to-end tests for -0/--null: NUL-terminated file names for safe
//! pipelines (`argrep -l0 ... | xargs -0 ...`).

use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

fn argrep(dir: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_argrep"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run argrep");
    if let Some(input) = stdin {
        let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
    }
    child.wait_with_output().unwrap()
}

/// The NUL-separated names in `out`, as bytes, sorted. Fails unless the
/// output is a sequence of NUL-terminated records (no newlines between).
fn records(out: &Output) -> Vec<Vec<u8>> {
    let stdout = &out.stdout;
    assert!(
        stdout.is_empty() || stdout.ends_with(b"\0"),
        "every record must end with NUL: {stdout:?}"
    );
    let mut recs: Vec<Vec<u8>> = stdout
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    recs.sort();
    recs
}

fn name_of(rec: &[u8]) -> String {
    let s = String::from_utf8(rec.to_vec()).unwrap();
    Path::new(&s)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// Matching files with awkward names, plus one file without a match.
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    let d = dir.path();
    fs::write(d.join("with space.txt"), "TODO\n").unwrap();
    fs::write(d.join("-dash.txt"), "TODO\n").unwrap();
    fs::write(d.join("plain.txt"), "TODO\n").unwrap();
    fs::write(d.join("clean.txt"), "nothing\n").unwrap();
    #[cfg(unix)]
    {
        fs::write(d.join("new\nline.txt"), "TODO\n").unwrap();
        fs::write(d.join("esc\x1b[2J.txt"), "TODO\n").unwrap();
    }
    dir
}

#[test]
fn files_with_matches_are_nul_terminated_and_exact() {
    let dir = tree();
    for flag in ["-0", "--null"] {
        let out = argrep(dir.path(), &["-l", flag, "TODO", "."], None);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let names: Vec<String> = records(&out).iter().map(|r| name_of(r)).collect();
        let mut want = vec!["-dash.txt", "plain.txt", "with space.txt"];
        if cfg!(unix) {
            // Exact bytes: no `\x1b` escaping, the newline stays a newline.
            want.extend(["esc\x1b[2J.txt", "new\nline.txt"]);
        }
        want.sort();
        let mut names = names;
        names.sort();
        assert_eq!(names, want, "{flag}");
    }
}

#[test]
fn files_without_match_and_files_listing_use_nul_too() {
    let dir = tree();
    let out = argrep(dir.path(), &["-L", "-0", "TODO", "."], None);
    assert_eq!(
        records(&out).iter().map(|r| name_of(r)).collect::<Vec<_>>(),
        ["clean.txt"]
    );

    let out = argrep(dir.path(), &["--files", "-0", "."], None);
    let n = records(&out).len();
    assert_eq!(n, if cfg!(unix) { 6 } else { 4 });
}

#[test]
fn short_flags_combine() {
    let dir = tree();
    let out = argrep(dir.path(), &["-l0", "TODO", "."], None);
    assert!(out.status.success());
    assert!(!records(&out).is_empty());
}

#[test]
fn without_null_names_stay_escaped_and_newline_separated() {
    let dir = tree();
    let out = argrep(dir.path(), &["-l", "TODO", "."], None);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(!stdout.contains('\0'));
    if cfg!(unix) {
        assert!(stdout.contains("esc\\x1b[2J.txt"), "{stdout:?}");
        assert!(stdout.contains("new\\x0aline.txt"), "{stdout:?}");
    }
}

#[test]
fn stdin_is_named_with_a_nul_too() {
    let dir = TempDir::new().unwrap();
    let out = argrep(dir.path(), &["-l", "-0", "TODO"], Some("a TODO here\n"));
    assert_eq!(out.stdout, b"<stdin>\0");
    let out = argrep(dir.path(), &["-L", "-0", "TODO"], Some("nothing\n"));
    assert_eq!(out.stdout, b"<stdin>\0");
}

#[test]
fn null_without_a_file_list_mode_is_a_usage_error() {
    let dir = tree();
    for args in [
        &["-0", "TODO", "."][..],
        &["-0", "-c", "TODO", "."],
        &["-0", "-o", "TODO", "."],
    ] {
        let out = argrep(dir.path(), args, None);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("-0/--null only applies to -l, -L and --files"),
            "{err}"
        );
    }
}

#[test]
fn quiet_still_prints_nothing() {
    let dir = tree();
    let out = argrep(dir.path(), &["-q", "-l", "-0", "TODO", "."], None);
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
}

#[cfg(unix)]
#[test]
fn xargs_dash_zero_gets_every_name_back() {
    let dir = tree();
    let out = Command::new("sh")
        .current_dir(dir.path())
        .arg("-c")
        .arg(format!(
            "'{}' -l0 TODO . | xargs -0 -n1 printf '%s\\0'",
            env!("CARGO_BIN_EXE_argrep")
        ))
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Five matching files, each delivered to the command as one argument.
    assert_eq!(records(&out).len(), 5);
    assert!(records(&out).iter().any(|r| name_of(r) == "new\nline.txt"));
}
