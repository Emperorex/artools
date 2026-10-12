//! End-to-end tests for `--bar`. Test processes have stdout on a pipe, so
//! the default (`auto`) draws no bars here.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

fn run(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_ardisk"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run ardisk");
    assert!(
        out.status.success(),
        "ardisk {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn sample() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for (rel, len) in [("big/f", 6000), ("mid/f", 3000), ("small/f", 100)] {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, vec![b'x'; len]).unwrap();
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn piped_output_has_no_bars_by_default() {
    let (_d, root) = sample();
    let auto = run(&[p(&root)]);
    assert_eq!(auto, run(&[p(&root), "--bar=never"]));
    assert!(!auto.contains('█') && !auto.contains('░') && !auto.contains('%'));
}

#[test]
fn bar_always_adds_bar_and_percentage_columns() {
    let (_d, root) = sample();
    let out = run(&[p(&root), "--bar", "--inodes"]);
    let lines: Vec<&str> = out.lines().collect();
    // root: 1 + 3 dirs + 3 files = 7 inodes; each subdir 2 = 29%.
    assert_eq!(
        lines[0],
        format!("         7  ██████████ 100%  {}", p(&root))
    );
    assert!(
        lines[1].starts_with("         2  ███░░░░░░░  29%  "),
        "{:?}",
        lines[1]
    );
    assert_eq!(lines.len(), 4);
}

#[test]
fn bars_follow_sizes_and_the_same_rows_as_the_plain_list() {
    let (_d, root) = sample();
    let plain = run(&[p(&root), "--apparent-size"]);
    let bars = run(&[p(&root), "--apparent-size", "--bar=always"]);
    assert_eq!(plain.lines().count(), bars.lines().count());
    for (a, b) in plain.lines().zip(bars.lines()) {
        // Same size column and path; bar and percentage in between.
        let (size_a, path_a) = a.split_at(10);
        let (size_b, rest) = b.split_at(10);
        assert_eq!(size_a, size_b);
        assert!(rest.ends_with(path_a.trim_start()), "{b:?}");
        assert_eq!(rest.matches(['█', '░']).count(), 10, "{b:?}");
    }
    // The biggest subdirectory has the fullest bar.
    let big = bars.lines().find(|l| l.ends_with("/big")).unwrap();
    let small = bars.lines().find(|l| l.ends_with("/small")).unwrap();
    assert!(big.matches('█').count() > small.matches('█').count());
}
