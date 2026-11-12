#![cfg(unix)]
//! A file with several hard links is counted once, under the name that
//! sorts first by path, whichever name the parallel scan reaches first: the
//! same directory sizes, listed names and file types on every run.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ardisk");

fn ardisk(dir: &Path, args: &[&str]) -> Output {
    let out = Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run ardisk");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Bytes per directory, from a --json scan.
fn sizes(dir: &Path, workers: &str) -> Vec<(String, u64)> {
    let out = ardisk(
        dir,
        &[
            ".",
            "--json",
            "--apparent-size",
            "--top",
            "0",
            "-j",
            workers,
        ],
    );
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut v: Vec<(String, u64)> = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["path"].as_str().unwrap().to_string(),
                e["bytes"].as_u64().unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

/// 200 files of 1000 bytes in `z/deep/er/` (found last: deepest), each
/// hard-linked as `a/` (sorts first), plus busy work so workers race.
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("z/deep/er")).unwrap();
    fs::create_dir(root.join("a")).unwrap();
    for i in 0..200 {
        let target = root.join(format!("z/deep/er/f{i:03}.txt"));
        fs::write(&target, vec![b'x'; 1000]).unwrap();
        fs::hard_link(&target, root.join(format!("a/f{i:03}.rs"))).unwrap();
    }
    for i in 0..100 {
        let busy = root.join(format!("busy/d{i:03}"));
        fs::create_dir_all(&busy).unwrap();
        fs::write(busy.join("x"), "x").unwrap();
    }
    dir
}

/// The apparent size of `dir` itself (its directory entry), as ardisk adds it.
fn entry_size(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

#[test]
fn each_file_is_credited_to_its_first_name_by_path() {
    let d = tree();
    let root = d.path();
    let got = sizes(root, "4");
    let size_of = |name: &str| got.iter().find(|(p, _)| p == name).unwrap().1;
    // a/ holds all 200 files; z/deep/er holds none of them.
    assert_eq!(size_of("a"), 200 * 1000 + entry_size(&root.join("a")));
    assert_eq!(size_of("z/deep/er"), entry_size(&root.join("z/deep/er")));
}

#[test]
fn every_run_gives_the_same_numbers() {
    let d = tree();
    let first = sizes(d.path(), "1");
    for workers in ["2", "4", "8", "16"] {
        for _ in 0..5 {
            assert_eq!(sizes(d.path(), workers), first, "-j {workers}");
        }
    }
}

#[test]
fn listings_use_the_same_name_every_time() {
    let d = tree();
    for mode in [
        &["--largest-files", "3"][..],
        &["--by-type"],
        &["--duplicates"],
    ] {
        let mut args = vec![".", "-j", "8"];
        args.extend(mode);
        let first = ardisk(d.path(), &args).stdout;
        for _ in 0..5 {
            assert_eq!(ardisk(d.path(), &args).stdout, first, "{mode:?}");
        }
    }
    // The a/ names (.rs), never the z/ ones (.txt).
    let out = ardisk(d.path(), &[".", "--largest-files", "1"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("a/f000.rs"));
    let out = String::from_utf8_lossy(&ardisk(d.path(), &[".", "--by-type"]).stdout).to_string();
    assert!(out.contains(".rs") && !out.contains(".txt"), "{out}");
}

#[test]
fn a_filtered_out_name_leaves_the_file_to_the_next_name() {
    // --include *.txt hides the a/*.rs names, so z/deep/er gets the files.
    let d = tree();
    let out = ardisk(
        d.path(),
        &[
            ".",
            "--json",
            "--apparent-size",
            "--top",
            "0",
            "--include",
            "*.txt",
        ],
    );
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["summary"]["files"], 200);
    let deep = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "z/deep/er")
        .unwrap()["bytes"]
        .as_u64()
        .unwrap();
    assert!(deep >= 200 * 1000, "{deep}");
}
