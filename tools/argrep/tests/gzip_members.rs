//! -z decompresses every member of a gzip file, as `gzip -d` does: a file
//! can hold several (`gzip -c b >> a.gz` appends one). What follows the
//! last member without being one, such as zero padding, is ignored.

use flate2::{Compression, write::GzEncoder};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_argrep");

fn argrep(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run argrep")
}

fn gzip(text: &str) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(text.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

/// `name` in a new directory, holding one gzip member per text, then `tail`.
fn file(name: &str, members: &[&str], tail: &[u8]) -> TempDir {
    let dir = TempDir::new().unwrap();
    let mut bytes: Vec<u8> = members.iter().flat_map(|m| gzip(m)).collect();
    bytes.extend_from_slice(tail);
    fs::write(dir.path().join(name), bytes).unwrap();
    dir
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn every_member_is_searched() {
    let d = file("log.gz", &["needle one\nx\n", "y\nneedle two\n"], b"");
    let out = argrep(d.path(), &["-z", "needle", "log.gz"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), "log.gz:needle one\nlog.gz:needle two\n");

    // Line numbers continue across members, as in the decompressed text.
    let out = argrep(d.path(), &["-z", "-n", "two", "log.gz"]);
    assert_eq!(stdout(&out), "log.gz:4:needle two\n");
}

#[test]
fn a_line_split_between_members_is_one_line() {
    let d = file("log.gz", &["nee", "dle\n"], b"");
    let out = argrep(d.path(), &["-z", "-c", "needle", "log.gz"]);
    assert_eq!(stdout(&out), "log.gz: 1\n");
}

#[test]
fn empty_members_are_skipped() {
    let d = file("log.gz", &["", "needle\n", "", ""], b"");
    let out = argrep(d.path(), &["-z", "needle", "log.gz"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), "log.gz:needle\n");
}

#[test]
fn many_members_are_all_searched() {
    // Enough members of varying size that some boundaries fall where the
    // read buffer splits the next header.
    let texts: Vec<String> = (0..3000)
        .map(|i| format!("needle {i} {}\n", "x".repeat(i % 37)))
        .collect();
    let members: Vec<&str> = texts.iter().map(String::as_str).collect();
    let d = file("log.gz", &members, b"");
    let out = argrep(d.path(), &["-z", "-c", "needle", "log.gz"]);
    assert_eq!(stdout(&out), "log.gz: 3000\n");
}

#[test]
fn padding_after_the_last_member_is_ignored() {
    let d = file("log.gz", &["needle\n", "needle\n"], &[0; 512]);
    let out = argrep(d.path(), &["-z", "-c", "needle", "log.gz"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stderr.is_empty());
    assert_eq!(stdout(&out), "log.gz: 2\n");
}

#[test]
fn other_data_after_the_last_member_is_ignored() {
    let d = file("log.gz", &["needle\n"], b"trailing data, not gzip");
    let out = argrep(d.path(), &["-z", "-c", "needle", "log.gz"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), "log.gz: 1\n");
}

#[test]
fn a_damaged_member_is_an_error() {
    // A gzip header, then no valid compressed data.
    let d = file("log.gz", &["needle\n"], b"\x1f\x8b\x08\x00broken");
    let out = argrep(d.path(), &["-z", "needle", "log.gz"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), "log.gz:needle\n");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("could not be read"), "{err}");
}

#[test]
fn the_decompressed_size_limit_counts_all_members() {
    // Each member is under the limit; together they are over it.
    let member = "x\n".repeat(400);
    let d = file("log.gz", &[&member, &member, "needle\n"], b"");
    let out = argrep(
        d.path(),
        &["-z", "--max-decompressed-size", "1000", "needle", "log.gz"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--max-decompressed-size"), "{err}");
}
