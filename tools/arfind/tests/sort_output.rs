//! End-to-end tests for --sort / --reverse.

use std::{
    fs::{self, File},
    path::Path,
    process::{Command, Output},
    time::{Duration, SystemTime},
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

/// stdout lines, relative to `root`, in the order printed.
fn lines(root: &Path, out: &Output) -> Vec<String> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let root = fs::canonicalize(root).unwrap();
    String::from_utf8(out.stdout.clone())
        .unwrap()
        .lines()
        .map(|l| {
            Path::new(l)
                .strip_prefix(&root)
                .unwrap_or(Path::new(l))
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn write(path: &Path, len: usize, days_ago: u64) {
    fs::write(path, vec![b'x'; len]).unwrap();
    let t = SystemTime::now() - Duration::from_secs(days_ago * 86_400);
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

/// Sizes and ages deliberately disagree with name and path order.
///   a/z.txt   300 B   10 days old
///   b/a.txt    10 B    1 day old
///   c.txt     100 B  100 days old
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    let d = dir.path();
    fs::create_dir(d.join("a")).unwrap();
    fs::create_dir(d.join("b")).unwrap();
    write(&d.join("a/z.txt"), 300, 10);
    write(&d.join("b/a.txt"), 10, 1);
    write(&d.join("c.txt"), 100, 100);
    dir
}

#[test]
fn each_key_ascending() {
    let dir = tree();
    let d = dir.path();
    let run = |key| lines(d, &arfind(d, &["-t", "f", "--sort", key]));
    assert_eq!(run("path"), ["a/z.txt", "b/a.txt", "c.txt"]);
    assert_eq!(run("name"), ["b/a.txt", "c.txt", "a/z.txt"]);
    assert_eq!(run("size"), ["b/a.txt", "c.txt", "a/z.txt"]);
    assert_eq!(run("time"), ["c.txt", "a/z.txt", "b/a.txt"]);
}

#[test]
fn reverse_flips_each_key() {
    let dir = tree();
    let d = dir.path();
    let run = |key| lines(d, &arfind(d, &["-t", "f", "--sort", key, "--reverse"]));
    assert_eq!(run("path"), ["c.txt", "b/a.txt", "a/z.txt"]);
    assert_eq!(run("name"), ["a/z.txt", "c.txt", "b/a.txt"]);
    assert_eq!(run("size"), ["a/z.txt", "c.txt", "b/a.txt"]);
    assert_eq!(run("time"), ["b/a.txt", "a/z.txt", "c.txt"]);
}

#[test]
fn equal_keys_are_ordered_by_path_even_reversed() {
    let dir = TempDir::new().unwrap();
    let d = dir.path();
    for name in ["d", "b", "a", "c"] {
        write(&d.join(name), 5, 3);
    }
    write(&d.join("big"), 50, 3);
    // Many workers, many runs: the parallel walk must not leak into the order.
    for _ in 0..5 {
        let out = arfind(d, &["-t", "f", "-j", "8", "--sort", "size"]);
        assert_eq!(lines(d, &out), ["a", "b", "c", "d", "big"]);
        let out = arfind(d, &["-t", "f", "-j", "8", "--sort", "size", "--reverse"]);
        assert_eq!(lines(d, &out), ["big", "a", "b", "c", "d"]);
    }
}

#[test]
fn directories_and_the_root_are_sorted_too() {
    let dir = tree();
    let d = dir.path();
    let out = arfind(d, &["--sort", "path"]);
    // The root (printed as its full path) sorts before everything under it.
    assert_eq!(
        lines(d, &out),
        ["", "a", "a/z.txt", "b", "b/a.txt", "c.txt"]
    );
}

#[test]
fn filters_still_apply() {
    let dir = tree();
    let d = dir.path();
    let out = arfind(d, &["--name", "*.txt", "--size", "+50B", "--sort", "size"]);
    assert_eq!(lines(d, &out), ["c.txt", "a/z.txt"]);
}

#[cfg(unix)]
#[test]
fn a_symlink_sorts_by_its_own_size_not_its_target() {
    let dir = TempDir::new().unwrap();
    let d = dir.path();
    write(&d.join("huge"), 10_000, 1);
    write(&d.join("mid"), 1_000, 1);
    // The link's own size is the length of "huge": 4 bytes.
    std::os::unix::fs::symlink("huge", d.join("link")).unwrap();
    let out = arfind(d, &["--name", "[hlm]*", "--sort", "size"]);
    assert_eq!(lines(d, &out), ["link", "mid", "huge"]);
}

#[test]
fn print0_output_is_sorted() {
    let dir = tree();
    let d = dir.path();
    let out = arfind(d, &["-t", "f", "--sort", "size", "--reverse", "-0"]);
    assert!(out.status.success());
    let root = fs::canonicalize(d).unwrap();
    let names: Vec<String> = out
        .stdout
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| {
            let p = Path::new(std::str::from_utf8(r).unwrap());
            p.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, ["a/z.txt", "c.txt", "b/a.txt"]);
    assert!(out.stdout.ends_with(b"\0"));
}

#[test]
fn exec_runs_in_sort_order() {
    let dir = tree();
    let d = dir.path();
    let out = arfind(
        d,
        &[
            "-t",
            "f",
            "--sort",
            "time",
            "--dry-run",
            "--exec",
            "echo",
            "{/}",
        ],
    );
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        ["echo c.txt", "echo z.txt", "echo a.txt"]
    );
    // Without --sort, --exec keeps its path order.
    let out = arfind(d, &["-t", "f", "--dry-run", "--exec", "echo", "{/}"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        ["echo z.txt", "echo a.txt", "echo c.txt"]
    );
}

#[test]
fn invalid_combinations_are_rejected() {
    let dir = tree();
    let d = dir.path();
    let reverse_alone = arfind(d, &["--reverse"]);
    assert!(!reverse_alone.status.success());
    let with_count = arfind(d, &["--sort", "name", "--count"]);
    assert!(!with_count.status.success());
    let bad_key = arfind(d, &["--sort", "mtime"]);
    assert!(!bad_key.status.success());
    let stderr = String::from_utf8_lossy(&bad_key.stderr);
    assert!(stderr.contains("name, path, size, time"), "{stderr}");
}
