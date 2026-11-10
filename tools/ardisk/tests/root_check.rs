//! A PATH that can't be scanned is an error, reported before anything is
//! printed: it used to produce an empty report (or a --json document for a
//! directory that doesn't exist) and exit 0.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ardisk");

fn ardisk(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run ardisk")
}

/// Exit 1, nothing on stdout, and `ardisk: PATH: reason` on stderr.
fn assert_rejected(out: &Output, path: &str, reason: &str) {
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stdout.is_empty(),
        "no report for a path that can't be scanned:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(err.trim_end(), format!("ardisk: {path}: {reason}"));
}

#[test]
fn a_missing_path_is_an_error_in_every_mode() {
    let d = TempDir::new().unwrap();
    for mode in [
        &[][..],
        &["--json"],
        &["--tree"],
        &["--summarize"],
        &["--inodes"],
        &["--by-type"],
        &["--largest-files", "3"],
        &["--duplicates"],
        &["--duplicates", "--json"],
    ] {
        let mut args = vec!["missing"];
        args.extend(mode);
        assert_rejected(
            &ardisk(d.path(), &args),
            "missing",
            "No such file or directory",
        );
    }
}

#[test]
fn a_file_is_not_a_directory() {
    let d = TempDir::new().unwrap();
    fs::write(d.path().join("notes.txt"), "x").unwrap();
    assert_rejected(
        &ardisk(d.path(), &["notes.txt"]),
        "notes.txt",
        "Not a directory",
    );
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_is_an_error() {
    use std::os::unix::fs::PermissionsExt;
    let d = TempDir::new().unwrap();
    let locked = d.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    // Root can list it anyway; then there is nothing to test.
    let readable = fs::read_dir(&locked).is_ok();
    let out = ardisk(d.path(), &["locked"]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        eprintln!("skipped: running with permission to read any directory");
        return;
    }
    assert_rejected(&out, "locked", "Permission denied");
}

#[test]
fn a_bad_flag_is_still_reported_first() {
    let d = TempDir::new().unwrap();
    let out = ardisk(d.path(), &["missing", "--include", "["]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Invalid glob pattern"));
}

#[test]
fn diff_does_not_need_a_path() {
    // --diff reads two snapshots; the default PATH plays no part.
    let d = TempDir::new().unwrap();
    fs::create_dir(d.path().join("tree")).unwrap();
    fs::write(d.path().join("tree/f"), vec![b'x'; 1000]).unwrap();
    let snap = ardisk(d.path(), &["tree", "--json"]).stdout;
    fs::write(d.path().join("a.json"), &snap).unwrap();
    fs::write(d.path().join("b.json"), &snap).unwrap();
    fs::remove_dir_all(d.path().join("tree")).unwrap();
    let out = ardisk(d.path(), &["--diff", "a.json", "b.json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
