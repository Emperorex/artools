//! Which directories argrep skips by default: only the defaults every
//! artools tool shares. A directory named `target` is searched like any
//! other (it is real source in the Linux kernel); Cargo's build output is
//! left to `.gitignore`.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

fn argrep(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_argrep"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run argrep")
}

/// Matching paths relative to the search root `dir`, sorted.
fn hits(dir: &Path, out: &Output) -> Vec<String> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let root = fs::canonicalize(dir).unwrap();
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

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn a_directory_named_target_is_searched() {
    let dir = TempDir::new().unwrap();
    let d = dir.path();
    write(&d.join("drivers/target/target_core.c"), "TODO\n");
    write(&d.join("include/target/target_core_base.h"), "TODO\n");
    write(&d.join("drivers/net/x.c"), "TODO\n");
    let out = argrep(d, &["-l", "TODO", "."]);
    assert_eq!(
        hits(d, &out),
        [
            "drivers/net/x.c",
            "drivers/target/target_core.c",
            "include/target/target_core_base.h"
        ]
    );
}

#[test]
fn cargo_build_output_is_left_to_gitignore() {
    let dir = TempDir::new().unwrap();
    let d = dir.path();
    write(&d.join(".gitignore"), "/target\n");
    write(&d.join("src/main.rs"), "TODO\n");
    write(&d.join("target/debug/build/out.rs"), "TODO\n");
    assert_eq!(hits(d, &argrep(d, &["-l", "TODO", "."])), ["src/main.rs"]);
    // Without .gitignore rules (--no-ignore) it is searched like the rest.
    let out = argrep(d, &["-l", "TODO", ".", "--no-ignore"]);
    assert_eq!(hits(d, &out), ["src/main.rs", "target/debug/build/out.rs"]);
}

#[test]
fn exclude_dir_still_skips_target_on_request() {
    let dir = TempDir::new().unwrap();
    let d = dir.path();
    write(&d.join("src/a.rs"), "TODO\n");
    write(&d.join("target/b.rs"), "TODO\n");
    let out = argrep(d, &["-l", "TODO", ".", "--exclude-dir", "target"]);
    assert_eq!(hits(d, &out), ["src/a.rs"]);
}

#[test]
fn the_shared_defaults_are_still_skipped() {
    let dir = TempDir::new().unwrap();
    let d = dir.path();
    write(&d.join("keep.txt"), "TODO\n");
    for skipped in ["node_modules/m.js", "__pycache__/c.py"] {
        write(&d.join(skipped), "TODO\n");
    }
    assert_eq!(hits(d, &argrep(d, &["-l", "TODO", "."])), ["keep.txt"]);
    let out = argrep(d, &["-l", "TODO", ".", "--no-ignore"]);
    assert_eq!(
        hits(d, &out),
        ["__pycache__/c.py", "keep.txt", "node_modules/m.js"]
    );
}
