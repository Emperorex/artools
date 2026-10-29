//! End-to-end tests for --owner / --group (Unix only).
//!
//! The first group works as any user, by filtering on the current uid/gid.
//! The second needs to give files away (lchown), which only root can do;
//! those tests return early with a note when run as a normal user.
#![cfg(unix)]

use std::{
    ffi::CString,
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
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

/// Printed paths relative to `root`, sorted ("" is the root itself).
fn listed(root: &Path, out: &Output) -> Vec<String> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let root = fs::canonicalize(root).unwrap();
    let mut v: Vec<String> = String::from_utf8(out.stdout.clone())
        .unwrap()
        .lines()
        .map(|l| {
            Path::new(l)
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    v.sort();
    v
}

/// The uid and gid new files get here (the current user's).
fn me() -> (u32, u32) {
    let probe = TempDir::new().unwrap();
    let f = probe.path().join("probe");
    fs::write(&f, "").unwrap();
    let m = fs::metadata(&f).unwrap();
    (m.uid(), m.gid())
}

fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("a.txt"), "x").unwrap();
    fs::write(dir.path().join("sub/b.txt"), "x").unwrap();
    dir
}

// ── as any user ─────────────────────────────────────────────────────────

#[test]
fn own_files_match_by_uid_and_gid() {
    let dir = tree();
    let d = dir.path();
    let (uid, gid) = me();
    let all = ["", "a.txt", "sub", "sub/b.txt"];
    assert_eq!(listed(d, &arfind(d, &["--owner", &uid.to_string()])), all);
    assert_eq!(listed(d, &arfind(d, &["--group", &gid.to_string()])), all);
    // Combined with the other filters as usual.
    let out = arfind(d, &["-t", "f", "--owner", &uid.to_string(), "--name", "b*"]);
    assert_eq!(listed(d, &out), ["sub/b.txt"]);
}

#[test]
fn negation_and_other_ids_exclude_everything_here() {
    let dir = tree();
    let d = dir.path();
    let (uid, gid) = me();
    assert!(listed(d, &arfind(d, &["--owner", &format!("!{uid}")])).is_empty());
    assert!(listed(d, &arfind(d, &["--group", &format!("!{gid}")])).is_empty());
    // A uid nobody has is accepted, like find -user 4000000, and matches
    // nothing.
    assert!(listed(d, &arfind(d, &["--owner", "4000000"])).is_empty());
    // AND: right owner, wrong group.
    let out = arfind(d, &["--owner", &uid.to_string(), "--group", "4000000"]);
    assert!(listed(d, &out).is_empty());
    // OR within one flag: one of the two values is ours.
    let out = arfind(
        d,
        &["-t", "f", "--owner", "4000000", "--owner", &uid.to_string()],
    );
    assert_eq!(listed(d, &out), ["a.txt", "sub/b.txt"]);
}

#[test]
fn names_resolve_and_user_is_an_alias() {
    let dir = tree();
    let d = dir.path();
    let name = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
    let name = name.trim();
    let out = arfind(d, &["-t", "f", "--user", name]);
    assert_eq!(listed(d, &out), ["a.txt", "sub/b.txt"]);
}

#[test]
fn unknown_names_are_usage_errors() {
    let dir = tree();
    let d = dir.path();
    for args in [
        &["--owner", "no-such-user-arfind"][..],
        &["--owner", "!no-such-user-arfind"],
        &["--group", "no-such-group-arfind"],
        &["--owner", ""],
        &["--owner", "-5"],
    ] {
        let out = arfind(d, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
    let err = String::from_utf8_lossy(&arfind(d, &["--owner", "no-such-user-arfind"]).stderr)
        .into_owned();
    assert!(err.contains("unknown user 'no-such-user-arfind'"), "{err}");
}

#[test]
fn exec_only_sees_matching_entries() {
    let dir = tree();
    let d = dir.path();
    let out = arfind(
        d,
        &["-t", "f", "--owner", "4000000", "--dry-run", "--exec", "rm"],
    );
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "nothing matched, so nothing would run"
    );
}

// ── as root: entries owned by others ────────────────────────────────────

/// Gives `path` (itself, not a symlink's target) to `uid:gid`. False if not
/// permitted, i.e. not running as root.
fn give(path: &Path, uid: u32, gid: u32) -> bool {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    unsafe { libc::lchown(c.as_ptr(), uid, gid) == 0 }
}

/// root_file 0:0, other_file 1:1, mixed 1:0, dir 1:0, link (to root_file)
/// 1:0; the tree root stays 0:0. None if not root.
fn foreign_tree() -> Option<TempDir> {
    let dir = TempDir::new().unwrap();
    let d = dir.path();
    for f in ["root_file", "other_file", "mixed"] {
        fs::write(d.join(f), "x").unwrap();
    }
    fs::create_dir(d.join("dir")).unwrap();
    std::os::unix::fs::symlink("root_file", d.join("link")).unwrap();
    let ok = give(d, 0, 0)
        && give(&d.join("root_file"), 0, 0)
        && give(&d.join("other_file"), 1, 1)
        && give(&d.join("mixed"), 1, 0)
        && give(&d.join("dir"), 1, 0)
        && give(&d.join("link"), 1, 0);
    if !ok {
        eprintln!("skipped: giving files to other users needs root");
        return None;
    }
    Some(dir)
}

#[test]
fn foreign_owners_and_groups() {
    let Some(dir) = foreign_tree() else { return };
    let d = dir.path();
    let run = |args: &[&str]| listed(d, &arfind(d, args));

    assert_eq!(run(&["-t", "f", "--owner", "root"]), ["root_file"]);
    assert_eq!(run(&["-t", "f", "--owner", "0"]), ["root_file"]);
    // Every kind is matched, and a symlink by its own owner (1), not its
    // target's (root).
    assert_eq!(
        run(&["--owner", "1"]),
        ["dir", "link", "mixed", "other_file"]
    );
    assert_eq!(
        run(&["--owner", "!root"]),
        ["dir", "link", "mixed", "other_file"]
    );
    // --owner AND --group.
    assert_eq!(
        run(&["--owner", "1", "--group", "0"]),
        ["dir", "link", "mixed"]
    );
    assert_eq!(run(&["--owner", "1", "--group", "1"]), ["other_file"]);
    // OR within a flag, negation within the other.
    assert_eq!(
        run(&["-t", "f", "--owner", "0", "--owner", "1", "--group", "!1"]),
        ["mixed", "root_file"]
    );
    // The root itself is tested too, like find's starting point.
    assert_eq!(run(&["-t", "d", "--owner", "root"]), [""]);
}

#[test]
fn filters_do_not_stop_descent() {
    let Some(dir) = foreign_tree() else { return };
    let d = dir.path();
    // dir is owned by uid 1, but a root-owned file inside it is still found.
    fs::write(d.join("dir/inner"), "x").unwrap();
    assert!(give(&d.join("dir/inner"), 0, 0));
    let out = arfind(d, &["-t", "f", "--owner", "root"]);
    assert_eq!(listed(d, &out), ["dir/inner", "root_file"]);
}
