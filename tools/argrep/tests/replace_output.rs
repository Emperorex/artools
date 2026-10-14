//! End-to-end tests for -r/--replace: real binary, real files.

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

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

const SRC: &str = "let a = x.unwrap();\nok\nlet b = y.unwrap();   \n";

fn project() -> TempDir {
    let d = TempDir::new().unwrap();
    fs::create_dir(d.path().join("src")).unwrap();
    fs::write(d.path().join("src/m.rs"), SRC).unwrap();
    d
}

#[test]
fn prints_replaced_lines_and_never_touches_files() {
    let d = project();
    let before = fs::metadata(d.path().join("src/m.rs"))
        .unwrap()
        .modified()
        .unwrap();
    let out = argrep(
        d.path(),
        &[r"unwrap\(\)", "src", "-n", "-r", r#"expect("...")"#],
        None,
    );
    assert!(out.status.success());
    let file = fs::canonicalize(d.path().join("src/m.rs")).unwrap();
    let f = file.display();
    assert_eq!(
        stdout(&out),
        format!("{f}:1:let a = x.expect(\"...\");\n{f}:3:let b = y.expect(\"...\");   \n")
    );
    assert_eq!(fs::read_to_string(d.path().join("src/m.rs")).unwrap(), SRC);
    let after = fs::metadata(d.path().join("src/m.rs"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(before, after);
}

#[test]
fn works_as_a_filter_on_stdin() {
    let d = TempDir::new().unwrap();
    let out = argrep(
        d.path(),
        &["ERROR", "-r", "WARN"],
        Some("INFO start\nERROR disk\nERROR net\n"),
    );
    assert_eq!(stdout(&out), "WARN disk\nWARN net\n");
}

#[test]
fn only_matching_prints_one_replacement_per_match() {
    let d = TempDir::new().unwrap();
    let out = argrep(
        d.path(),
        &[r"(\w+)@(\w+)", "-o", "-r", "$2"],
        Some("a@b c@d\nnone\ne@f\n"),
    );
    assert_eq!(stdout(&out), "b\nd\nf\n");
}

#[test]
fn context_lines_stay_unchanged() {
    let d = TempDir::new().unwrap();
    let out = argrep(
        d.path(),
        &["foo", "-C", "1", "-n", "-r", "BAR"],
        Some("a\nfoo foo\nb foo-ish\n"),
    );
    // The context line before is original; both matching lines are replaced.
    assert_eq!(stdout(&out), "1-a\n2:BAR BAR\n3:b BAR-ish\n");
}

#[test]
fn empty_replacement_deletes_matches_and_keeps_exact_spacing() {
    let d = TempDir::new().unwrap();
    let out = argrep(d.path(), &["foo", "-r", ""], Some("foo bar foo\n"));
    assert_eq!(stdout(&out), " bar \n");
}

#[test]
fn exit_status_follows_argreps_usual_convention() {
    // Like plain argrep (outside -q), 0 means "searched fine", matches or
    // not; -r does not change that.
    let d = TempDir::new().unwrap();
    let hit = argrep(d.path(), &["x", "-r", "y"], Some("x\n"));
    assert_eq!(hit.status.code(), Some(0));
    assert_eq!(stdout(&hit), "y\n");
    let miss = argrep(d.path(), &["zzz", "-r", "y"], Some("x\n"));
    assert_eq!(miss.status.code(), Some(0));
    assert!(miss.stdout.is_empty());
}

#[test]
fn unknown_group_references_are_rejected_before_searching() {
    let d = TempDir::new().unwrap();
    for rep in ["cost $5", "$1a", "${2}", "$name"] {
        let out = argrep(d.path(), &[r"(\w+)", "-r", rep], Some("abc\n"));
        assert_eq!(out.status.code(), Some(1), "{rep}");
        assert!(out.stdout.is_empty(), "{rep}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("$$"), "{rep}");
    }
    // Escaped, they are fine.
    let out = argrep(d.path(), &[r"(\w+)", "-r", "cost $$5 ${1}a"], Some("abc\n"));
    assert_eq!(stdout(&out), "cost $5 abca\n");
}

#[test]
fn conflicting_modes_are_usage_errors() {
    let d = TempDir::new().unwrap();
    for flag in ["-l", "-L", "-c", "-v", "-q"] {
        let out = argrep(d.path(), &["x", "-r", "y", flag], Some("x\n"));
        assert_eq!(out.status.code(), Some(2), "{flag}");
    }
}
