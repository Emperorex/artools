//! The block search (see argrep's LinePrefilter) runs in every mode: it
//! skips lines that cannot match, keeps the last of them as before-context,
//! and with -v marks them as known not to match. Whatever it does, the
//! output must be exactly what reading every line gives.
//!
//! `--max-line-size` below the block size (256 KiB) turns the block search
//! off, so each case is run both ways and compared.

use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_argrep");
/// Below the block size: lines are read one by one.
const LINE_BY_LINE: &[&str] = &["--max-line-size", "100KB"];

fn argrep(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run argrep")
}

/// Lines of every kind the block search treats specially, at varying
/// distances: matches close together and far apart, CRLF endings, invalid
/// UTF-8, control characters, empty lines, and no newline at the end.
fn file() -> TempDir {
    let dir = TempDir::new().unwrap();
    let mut text = Vec::new();
    let mut seed = 12345u32;
    for i in 0..6000u32 {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let r = (seed >> 16) % 100;
        let word = match r % 9 {
            0 => "error",
            1 => "errorerror",
            2 => "err or",
            3 => "Error",
            _ => "fine",
        };
        // Long quiet stretches too, so whole blocks are skipped.
        let word = if (2000..3500).contains(&i) {
            "quiet"
        } else {
            word
        };
        // Every third line ends with the word, for `$` (and `\r` before it).
        let line = if i % 3 == 0 {
            format!("{i} tail {word}")
        } else {
            format!("{i} {word} tail")
        };
        text.extend_from_slice(line.as_bytes());
        match r {
            0..=3 => text.extend_from_slice(b" \xff\xfe bad"),
            4..=7 => text.push(b'\r'),
            8..=9 => text.extend_from_slice(b" \x1b[31mx\x07"),
            10..=12 => text.clear_last_line(),
            _ => {}
        }
        text.push(b'\n');
    }
    text.extend_from_slice(b"last error without newline");
    fs::write(dir.path().join("log.txt"), text).unwrap();
    dir
}

/// Empties the line being written (an empty line in the file).
trait ClearLastLine {
    fn clear_last_line(&mut self);
}
impl ClearLastLine for Vec<u8> {
    fn clear_last_line(&mut self) {
        let start = self.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        self.truncate(start);
    }
}

fn same_both_ways(dir: &Path, args: &[&str]) {
    let fast = argrep(dir, args);
    let slow = argrep(dir, &[LINE_BY_LINE, args].concat());
    assert_eq!(
        fast.status.code(),
        slow.status.code(),
        "exit status of {args:?}"
    );
    let strip = |o: &Output| {
        // --json reports the time taken; nothing else may differ.
        let text = String::from_utf8_lossy(&o.stdout).into_owned();
        text.split(r#""elapsed_ms":"#).next().unwrap().to_string()
    };
    assert_eq!(strip(&fast), strip(&slow), "output of {args:?}");
    assert!(
        !fast.stdout.is_empty() || args.contains(&"-q"),
        "{args:?} printed nothing"
    );
}

const PATTERNS: &[&str] = &[
    "error",
    "error$",
    "^1",
    "r o",
    "o.*r",
    r"\berror\b",
    "tail$",
];

#[test]
fn line_output_with_and_without_context() {
    let d = file();
    for pattern in PATTERNS {
        for mode in [
            &["-n"][..],
            &["-n", "-C1"],
            &["-n", "-B3"],
            &["-n", "-A2"],
            &["-C20"],
            &["-n", "-m", "5", "-A3"],
            &["-B2", "-m", "40"],
            &["--json", "-C2"],
            &["-o", "-n"],
        ] {
            same_both_ways(d.path(), &[mode, &["-e", pattern, "log.txt"]].concat());
        }
    }
}

#[test]
fn inverted_output() {
    let d = file();
    for pattern in PATTERNS {
        for mode in [
            &["-v", "-n"][..],
            &["-v", "-n", "-C2"],
            &["-v", "-m", "100"],
            &["-v", "--json"],
            &["-v", "-l"],
        ] {
            same_both_ways(d.path(), &[mode, &["-e", pattern, "log.txt"]].concat());
        }
    }
}

#[test]
fn counts_and_file_lists() {
    let d = file();
    for pattern in PATTERNS {
        for mode in [
            &["-c"][..],
            &["-c", "-v"],
            &["-c", "-v", "-m", "7"],
            &["-c", "-C1"],
            &["-l"],
            &["-q"],
            &["-v", "-q"],
        ] {
            same_both_ways(d.path(), &[mode, &["-e", pattern, "log.txt"]].concat());
        }
    }
}
