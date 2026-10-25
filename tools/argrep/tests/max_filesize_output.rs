//! End-to-end tests for --max-filesize: files over the limit are skipped
//! before they are opened.

use flate2::{Compression, write::GzEncoder};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

fn argrep(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_argrep"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run argrep")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// File names (not paths) printed one per line, sorted.
fn names(out: &Output) -> Vec<String> {
    assert!(out.status.success(), "{}", stderr(out));
    let mut v: Vec<String> = stdout(out)
        .lines()
        .map(|l| {
            let path = l.split(':').next().unwrap();
            Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    v.sort();
    v
}

/// A file of exactly `len` bytes that contains "TODO".
fn write_todo(path: &Path, len: usize) {
    let mut data = b"TODO\n".to_vec();
    data.resize(len, b'x');
    fs::write(path, data).unwrap();
}

/// small.txt 100 B, edge.txt exactly 1000 B, big.txt 1001 B; all match.
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    write_todo(&dir.path().join("small.txt"), 100);
    write_todo(&dir.path().join("edge.txt"), 1000);
    write_todo(&dir.path().join("big.txt"), 1001);
    dir
}

#[test]
fn files_over_the_limit_are_skipped() {
    let dir = tree();
    let d = dir.path();
    // Exactly the limit is still searched; one byte more is not.
    let out = argrep(d, &["TODO", ".", "--max-filesize", "1000"]);
    assert_eq!(names(&out), ["edge.txt", "small.txt"]);
    let out = argrep(d, &["TODO", ".", "--max-filesize", "999B"]);
    assert_eq!(names(&out), ["small.txt"]);
    // A walked file is skipped quietly, and it is not an error.
    assert!(out.stderr.is_empty(), "{}", stderr(&out));
}

#[test]
fn no_limit_by_default_or_with_0() {
    let dir = tree();
    let all = ["big.txt", "edge.txt", "small.txt"];
    assert_eq!(names(&argrep(dir.path(), &["TODO", "."])), all);
    let out = argrep(dir.path(), &["TODO", ".", "--max-filesize", "0"]);
    assert_eq!(names(&out), all);
}

#[test]
fn applies_to_every_mode_that_reads_content() {
    let dir = tree();
    let d = dir.path();
    // -l and -c read content: big.txt is not listed.
    let out = argrep(d, &["-l", "TODO", ".", "--max-filesize", "1000"]);
    assert_eq!(names(&out), ["edge.txt", "small.txt"]);
    let out = argrep(d, &["-c", "TODO", ".", "--max-filesize", "1000"]);
    assert_eq!(names(&out), ["edge.txt", "small.txt"]);
    // -L must not list a skipped file as "no match": it was not searched.
    let out = argrep(d, &["-L", "NOPE", ".", "--max-filesize", "1000"]);
    assert_eq!(names(&out), ["edge.txt", "small.txt"]);
}

#[test]
fn files_listing_ignores_the_limit() {
    let dir = tree();
    let out = argrep(dir.path(), &["--files", ".", "--max-filesize", "1B"]);
    assert_eq!(names(&out), ["big.txt", "edge.txt", "small.txt"]);
}

#[test]
fn an_explicitly_named_file_is_skipped_with_a_warning() {
    let dir = tree();
    let d = dir.path();
    let out = argrep(d, &["TODO", "big.txt", "--max-filesize", "1000"]);
    assert!(out.status.success(), "a skip is not an error");
    assert!(out.stdout.is_empty());
    let err = stderr(&out);
    assert!(
        err.contains("big.txt: 1001 bytes, over --max-filesize 1000 bytes; not searched"),
        "{err}"
    );
    // Under -q the answer is "no match", not an error.
    let out = argrep(d, &["-q", "TODO", "big.txt", "--max-filesize", "1000"]);
    assert_eq!(out.status.code(), Some(1));
    // Under the limit it is searched as usual.
    let out = argrep(d, &["TODO", "small.txt", "--max-filesize", "1000"]);
    assert!(
        stdout(&out).ends_with("small.txt:TODO\n"),
        "{}",
        stdout(&out)
    );
}

#[cfg(unix)]
#[test]
fn a_named_symlink_is_measured_by_its_target() {
    let dir = tree();
    let d = dir.path();
    std::os::unix::fs::symlink("big.txt", d.join("link")).unwrap();
    let out = argrep(d, &["TODO", "link", "--max-filesize", "1000"]);
    assert!(stderr(&out).contains("1001 bytes"), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
}

#[test]
fn compressed_files_are_measured_on_disk() {
    let dir = TempDir::new().unwrap();
    let d = dir.path();
    // ~2 MB of text that gzip squeezes to a couple of KB.
    let mut text = vec![b'y'; 2_000_000];
    text.extend_from_slice(b"\nTODO inside\n");
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&text).unwrap();
    let packed = gz.finish().unwrap();
    assert!(packed.len() < 50_000, "{}", packed.len());
    fs::write(d.join("logs.gz"), &packed).unwrap();

    // The limit is on the compressed size, so the file is searched...
    let out = argrep(d, &["-z", "TODO", ".", "--max-filesize", "50K"]);
    assert_eq!(names(&out), ["logs.gz"]);
    // ...and skipped once the limit is below it.
    let limit = (packed.len() - 1).to_string();
    let out = argrep(d, &["-z", "TODO", ".", "--max-filesize", &limit]);
    assert!(names(&out).is_empty());
}

#[test]
fn stats_and_json_report_the_skips() {
    let dir = tree();
    let d = dir.path();
    let out = argrep(d, &["TODO", ".", "--max-filesize", "1000", "--stats"]);
    let err = stderr(&out);
    assert!(err.contains("Files discovered:   3"), "{err}");
    assert!(err.contains("Files searched:     2"), "{err}");
    assert!(
        err.contains("Files skipped:      1 (1 over --max-filesize)"),
        "{err}"
    );

    let summary = |args: &[&str]| -> Value {
        let out = argrep(d, args);
        let last = stdout(&out).lines().last().unwrap().to_string();
        serde_json::from_str(&last).unwrap()
    };
    let s = summary(&["TODO", ".", "--json", "--max-filesize", "1000"]);
    assert_eq!(s["type"], "summary");
    assert_eq!(s["files_skipped_size"], 1);
    assert_eq!(s["files_searched"], 2);
    // Without the flag the key is absent, so existing output is unchanged.
    let s = summary(&["TODO", ".", "--json"]);
    assert!(s.get("files_skipped_size").is_none(), "{s}");
}

#[test]
fn bad_sizes_are_usage_errors() {
    let dir = tree();
    for bad in ["lots", "1.5M", "-1", "MB"] {
        let out = argrep(dir.path(), &["TODO", ".", "--max-filesize", bad]);
        assert_eq!(out.status.code(), Some(2), "{bad}");
    }
}
