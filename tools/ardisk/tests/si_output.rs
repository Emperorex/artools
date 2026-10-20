//! End-to-end tests for --si: sizes in powers of 1000 instead of 1024.

use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn ardisk(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ardisk"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run ardisk");
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// One 1,500,000-byte file: 1.43 MB in binary units, 1.50 MB in SI.
fn tree() -> (TempDir, String) {
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir(root.join("big")).unwrap();
    fs::write(root.join("big/f.bin"), vec![b'x'; 1_500_000]).unwrap();
    let root = root.to_str().unwrap().to_string();
    (dir, root)
}

fn p(s: &str) -> &Path {
    Path::new(s)
}

#[test]
fn largest_files_are_shown_in_si_units() {
    let (_d, root) = tree();
    let (ok, default, _) = ardisk(&[&root, "--largest-files", "1"]);
    assert!(ok);
    assert!(default.contains("1.43 MB"), "{default}");
    let (ok, si, _) = ardisk(&[&root, "--largest-files", "1", "--si"]);
    assert!(ok);
    assert!(si.contains("1.50 MB"), "{si}");
}

#[test]
fn every_text_report_uses_si_labels() {
    let (_d, root) = tree();
    fs::write(p(&root).join("small.txt"), vec![b'y'; 3000]).unwrap();
    for args in [
        &["--largest-files", "5"][..],
        &["--by-type"],
        &["-s"],
        &["--tree"],
        &["--bar=always"],
    ] {
        let mut all = vec![root.as_str(), "--si"];
        all.extend_from_slice(args);
        let (ok, out, err) = ardisk(&all);
        assert!(ok, "{args:?}: {err}");
        assert!(
            !out.contains(" KB"),
            "{args:?} printed a binary label: {out}"
        );
    }
    // Logical size, so the number doesn't depend on the filesystem's
    // block size (by default ardisk reports disk usage).
    let (_, out, _) = ardisk(&[&root, "--si", "--largest-files", "5", "--apparent-size"]);
    assert!(out.contains("3.00 kB"), "{out}");
}

#[test]
fn threshold_is_read_in_the_same_units_as_shown() {
    let (_d, root) = tree();
    // 1.45 MB: in SI that is 1,450,000 bytes, below the 1.5 MB file, so the
    // directory is listed; in binary it is 1,520,435 bytes, above it.
    let (ok, si, _) = ardisk(&[&root, "--si", "--threshold", "1.45MB"]);
    assert!(ok);
    assert!(si.contains("/big"), "{si}");
    let (ok, binary, _) = ardisk(&[&root, "--threshold", "1.45MB"]);
    assert!(ok);
    assert!(!binary.contains("/big"), "{binary}");
}

#[test]
fn min_size_is_read_in_si_units_too() {
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    // Two identical 1,010,000-byte files: above 1MB in SI, below in binary.
    for name in ["a.bin", "b.bin"] {
        fs::write(root.join(name), vec![b'z'; 1_010_000]).unwrap();
    }
    let root = root.to_str().unwrap();
    let (_, si, _) = ardisk(&[root, "--duplicates", "--min-size", "1MB", "--si"]);
    assert!(si.contains("2 identical files"), "{si}");
    let (_, binary, _) = ardisk(&[root, "--duplicates", "--min-size", "1MB"]);
    assert!(!binary.contains("identical files,"), "{binary}");
}

#[test]
fn json_is_plain_bytes_either_way() {
    let (_d, root) = tree();
    let strip = |s: String| {
        let mut v: serde_json::Value = serde_json::from_str(&s).unwrap();
        v.as_object_mut().unwrap().remove("meta");
        v
    };
    let (_, a, _) = ardisk(&[&root, "--json"]);
    let (_, b, _) = ardisk(&[&root, "--json", "--si"]);
    assert_eq!(strip(a), strip(b));
}

#[test]
fn progress_counter_uses_si_units() {
    let (_d, root) = tree();
    let (ok, out, err) = ardisk(&[&root, "-s", "--progress", "--si"]);
    assert!(ok);
    // The counter's final size is the report's total, in the same units.
    let total = out
        .trim_start()
        .split("  ")
        .next()
        .unwrap()
        .trim()
        .to_string();
    assert!(total.ends_with(" MB"), "{out}");
    let last = err.lines().last().unwrap();
    assert!(last.contains(&format!(", {total}, ")), "{last} vs {total}");
}

#[test]
fn si_with_inodes_is_rejected() {
    let (_d, root) = tree();
    let (ok, _, err) = ardisk(&[&root, "--si", "--inodes"]);
    assert!(!ok);
    assert!(err.contains("--si"), "{err}");
}
