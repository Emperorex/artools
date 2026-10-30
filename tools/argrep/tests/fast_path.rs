//! The block fast path must find exactly the lines the per-line search
//! finds. Each case runs the real binary and compares its `--json` line
//! numbers with the line regex applied to every line on its own, which is
//! what argrep did before the fast path existed.

use argrep::{MatchOptions, build_matcher};
use serde_json::Value;
use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

/// Lines (1-based) that match when each line is searched on its own, the
/// way grep_file's line path does: decoded lossily, without `\n`/`\r\n`.
fn per_line(content: &[u8], pattern: &str, opts: MatchOptions) -> Vec<usize> {
    let regex = build_matcher(pattern, opts).unwrap();
    let mut lines: Vec<&[u8]> = content.split(|&b| b == b'\n').collect();
    if content.ends_with(b"\n") {
        lines.pop();
    }
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            let l = l.strip_suffix(b"\r").unwrap_or(l);
            regex.is_match(&String::from_utf8_lossy(l))
        })
        .map(|(i, _)| i + 1)
        .collect()
}

fn argrep_lines(file: &Path, pattern: &str, flags: &[&str]) -> Vec<usize> {
    let out = Command::new(env!("CARGO_BIN_EXE_argrep"))
        .args(flags)
        .args(["--json", "-e", pattern])
        .arg(file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["type"] == "match")
        .map(|e| e["line_number"].as_u64().unwrap() as usize)
        .collect()
}

fn opts_for(flags: &[&str]) -> MatchOptions {
    MatchOptions {
        fixed_strings: flags.contains(&"-F"),
        ignore_case: flags.contains(&"-i"),
        whole_word: flags.contains(&"-w"),
        whole_line: flags.contains(&"-x"),
    }
}

/// Asserts argrep and the per-line oracle agree on `content` for every
/// (pattern, flags) case; returns how many lines matched in total, so a
/// test can make sure it is not vacuous.
fn agree(content: &[u8], cases: &[(&str, &[&str])]) -> usize {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("input.txt");
    fs::write(&file, content).unwrap();
    let mut total = 0;
    for (pattern, flags) in cases {
        let expected = per_line(content, pattern, opts_for(flags));
        let got = argrep_lines(&file, pattern, flags);
        assert_eq!(got, expected, "pattern {pattern:?} {flags:?}");
        total += got.len();
    }
    total
}

const SMALL_CASES: &[(&str, &[&str])] = &[
    ("needle", &[]),
    ("NEEDLE", &["-i"]),
    ("^needle", &[]),
    ("needle$", &[]),
    ("needle", &["-x"]),
    ("needle", &["-w"]),
    (r"a\sb", &[]),
    ("a.b", &[]),
    ("[^x]+$", &[]),
    (r"\Aneedle", &[]),
    (r"needle\z", &[]),
    ("(?-m)^needle$", &[]),
    ("x*", &[]),
    ("^$", &[]),
    ("needle.*", &["-F"]),
];

#[test]
fn small_tricky_files() {
    let n = agree(
        b"needle\nhay needle hay\nneedles\nhay\na\nb\n\nNEEDLE\nneedle.*\nend needle",
        SMALL_CASES,
    );
    assert!(n > 10, "the cases must match something");
}

#[test]
fn crlf_lines() {
    let n = agree(b"hay\r\nneedle\r\nhay needle\r\n\r\nneedle", SMALL_CASES);
    assert!(n > 5);
}

#[test]
fn invalid_utf8_lines() {
    // a\xffb: `.` matches the U+FFFD it decodes to.
    let n = agree(b"hay\na\xffb\nneedle \xfe\n\xffneedle\nhay\n", SMALL_CASES);
    assert!(n > 5);
}

/// Several blocks (256 KiB each) of filler with hits placed at and around
/// block boundaries, plus a line longer than a whole block.
#[test]
fn hits_across_block_boundaries() {
    const BLOCK: usize = 256 * 1024;
    let mut content = Vec::new();
    let filler = b"lorem ipsum dolor sit amet, consectetur\n";
    let mut i = 0;
    while content.len() < 4 * BLOCK {
        content.extend_from_slice(filler);
        // Hits just before, on and after each block boundary.
        let to_boundary = BLOCK - (content.len() % BLOCK);
        if to_boundary < 2 * filler.len() {
            content.extend_from_slice(b"needle at a boundary\n");
        }
        i += 1;
        if i % 5000 == 0 {
            content.extend_from_slice(b"needle$ every so often\n");
        }
    }
    // A line longer than a block, with the hit at its very end.
    content.extend(std::iter::repeat_n(b'z', BLOCK + 1000));
    content.extend_from_slice(b"needle\n");
    content.extend(std::iter::repeat_n(b'y', BLOCK + 10));
    content.extend_from_slice(b"\nlast needle line\nend");

    let n = agree(
        &content,
        &[
            ("needle", &[]),
            ("needle$", &[]),
            ("^needle", &[]),
            ("NEEDLE", &["-i"]),
            ("boundary$", &[]),
            ("z{100}needle", &[]),
        ],
    );
    assert!(n > 20, "{n}");
}

#[test]
fn every_line_matching() {
    let content = "a\n".repeat(20_000);
    let n = agree(content.as_bytes(), &[("a", &[]), ("^a$", &[]), ("", &[])]);
    assert_eq!(n, 3 * 20_000);
}
