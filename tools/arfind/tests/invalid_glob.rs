//! An invalid glob is a usage error reported like any other bad value:
//! `error: Invalid glob pattern for --FLAG: 'PATTERN': reason`, exit 1,
//! nothing on stdout, and never a panic with a backtrace.

use std::process::{Command, Output};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_arfind");

fn arfind(args: &[&str]) -> Output {
    let dir = TempDir::new().unwrap();
    Command::new(BIN)
        .current_dir(dir.path())
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run arfind")
}

fn assert_bad_glob(out: &Output, flag: &str, pattern: &str) {
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!err.contains("panicked"), "{err}");
    assert!(
        err.starts_with(&format!(
            "error: Invalid glob pattern for {flag}: '{pattern}': "
        )),
        "{err}"
    );
}

#[test]
fn an_invalid_name_glob_is_an_error_not_a_panic() {
    for pattern in ["[", "a[b", "[!"] {
        assert_bad_glob(&arfind(&[".", "--name", pattern]), "--name", pattern);
    }
    // -i does not change that.
    assert_bad_glob(&arfind(&[".", "-i", "--name", "["]), "--name", "[");
}

#[test]
fn exclude_dir_reports_the_same_way() {
    assert_bad_glob(&arfind(&[".", "--exclude-dir", "["]), "--exclude-dir", "[");
}

#[test]
fn a_valid_glob_still_works() {
    let out = arfind(&[".", "--name", "[a-z]*"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
