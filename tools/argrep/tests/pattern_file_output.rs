//! End-to-end tests for -f/--file.

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
        // The process may exit before reading stdin (e.g. a usage error).
        let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
    }
    child.wait_with_output().unwrap()
}

/// Matched lines without the "path:" prefix, sorted.
fn lines(o: &Output) -> Vec<String> {
    let mut v: Vec<String> = String::from_utf8(o.stdout.clone())
        .unwrap()
        .lines()
        .map(|l| {
            l.rsplit_once(".rs:")
                .map_or(l, |(_, rest)| rest)
                .to_string()
        })
        .collect();
    v.sort();
    v
}

const CODE: &str =
    "// TODO later\nlet x = 1; // FIXME\nclean line\nHACK: temp\nprice $5.00 (a+b)\n";

fn setup(files: &[(&str, &str)]) -> TempDir {
    let d = TempDir::new().unwrap();
    fs::create_dir(d.path().join("src")).unwrap();
    fs::write(d.path().join("src/code.rs"), CODE).unwrap();
    for (name, content) in files {
        fs::write(d.path().join(name), content).unwrap();
    }
    d
}

#[test]
fn patterns_from_a_file_are_ored() {
    let d = setup(&[("p.txt", "TODO\nFIXME\nHACK\n")]);
    let out = argrep(d.path(), &["-f", "p.txt", "src"], None);
    assert!(out.status.success());
    assert_eq!(
        lines(&out),
        ["// TODO later", "HACK: temp", "let x = 1; // FIXME"]
    );
}

#[test]
fn several_files_and_e_patterns_combine() {
    let d = setup(&[("a.txt", "TODO\n"), ("b.txt", "HACK\n")]);
    let out = argrep(
        d.path(),
        &["-f", "a.txt", "-f", "b.txt", "-e", "clean", "src"],
        None,
    );
    assert_eq!(lines(&out), ["// TODO later", "HACK: temp", "clean line"]);
}

#[test]
fn blank_lines_crlf_and_fixed_strings() {
    let d = setup(&[("p.txt", "\r\n$5.00\r\n\r\n   \r\n(a+b)\r\n")]);
    let out = argrep(d.path(), &["-F", "-f", "p.txt", "src"], None);
    assert_eq!(lines(&out), ["price $5.00 (a+b)"]);
}

#[test]
fn flags_apply_to_every_pattern() {
    let d = setup(&[("p.txt", "todo\nhack\n")]);
    let out = argrep(d.path(), &["-i", "-f", "p.txt", "src"], None);
    assert_eq!(lines(&out), ["// TODO later", "HACK: temp"]);
    let out = argrep(d.path(), &["-x", "-f", "p.txt", "-i", "src"], None);
    assert!(lines(&out).is_empty(), "-x needs the whole line");
}

#[test]
fn replace_works_on_any_pattern_from_the_set() {
    let d = setup(&[("p.txt", "TODO\nFIXME\n")]);
    let out = argrep(d.path(), &["-f", "p.txt", "src", "-r", "<X>"], None);
    assert_eq!(lines(&out), ["// <X> later", "let x = 1; // <X>"]);
}

#[test]
fn quiet_exit_codes_for_ci() {
    let d = setup(&[
        ("forbidden.txt", "HACK\n"),
        ("allowed.txt", "nothing-like-this\n"),
    ]);
    assert_eq!(
        argrep(d.path(), &["-f", "forbidden.txt", "src", "-q"], None)
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        argrep(d.path(), &["-f", "allowed.txt", "src", "-q"], None)
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn patterns_from_stdin_search_path_not_stdin() {
    let d = setup(&[]);
    let out = argrep(d.path(), &["-f", "-", "src"], Some("HACK\n"));
    assert!(out.status.success());
    assert_eq!(lines(&out), ["HACK: temp"]);
    // Without PATH, the default "." is searched, never stdin.
    let out = argrep(d.path(), &["-f", "-"], Some("FIXME\n"));
    assert_eq!(lines(&out), ["let x = 1; // FIXME"]);
}

#[test]
fn errors_are_clear_and_nothing_is_printed() {
    let d = setup(&[
        ("blank.txt", "\n  \n\n"),
        ("bad.txt", "ok\n(unclosed\n"),
        ("p.txt", "TODO\n"),
    ]);
    let cases: &[(&[&str], i32, &str)] = &[
        (
            &["-f", "missing.txt", "src"],
            1,
            "cannot read pattern file 'missing.txt'",
        ),
        (
            &["-f", "blank.txt", "src"],
            1,
            "no patterns found in 'blank.txt'",
        ),
        (&["-f", "bad.txt", "src"], 1, "bad.txt:2:"),
        (&["-f", "-", "-f", "-", "src"], 2, "only once"),
        (&["-f", "-", "-"], 2, "both the patterns"),
        (
            &["-f", "p.txt", "foo", "src"],
            2,
            "two positional arguments",
        ),
        (
            &["-f", "missing.txt", "src", "-q"],
            2,
            "cannot read pattern file",
        ),
    ];
    for (args, code, msg) in cases {
        let out = argrep(d.path(), args, Some(""));
        assert_eq!(out.status.code(), Some(*code), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(msg), "{args:?}: {err}");
    }
    // An empty file is fine when -e supplies patterns.
    let out = argrep(d.path(), &["-f", "blank.txt", "-e", "TODO", "src"], None);
    assert_eq!(lines(&out), ["// TODO later"]);
}

#[test]
fn non_utf8_pattern_file_is_rejected() {
    let d = setup(&[]);
    fs::write(d.path().join("bin.txt"), [0x54, 0xff, 0x0a]).unwrap();
    let out = argrep(d.path(), &["-f", "bin.txt", "src"], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not valid UTF-8"));
}
