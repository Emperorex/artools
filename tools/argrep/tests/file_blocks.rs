//! When several files are searched in parallel, each file's results come
//! out as one block, not mixed with lines of the files other workers are
//! searching: in text output as in --json. A file that may never end (a
//! FIFO) is not held back.

use serde_json::Value;
use std::{
    fs,
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

/// `files` files, each in a directory of its own (a directory's files are
/// searched by one worker, so only files in different directories can be
/// searched at the same time), each with `lines` lines of `width` bytes,
/// every fifth holding "needle".
fn tree(files: usize, lines: usize, width: usize) -> TempDir {
    let dir = TempDir::new().unwrap();
    for f in 0..files {
        let sub = dir.path().join(format!("d{f:02}"));
        fs::create_dir(&sub).unwrap();
        let text: String = (0..lines)
            .map(|i| {
                let word = if i % 5 == 0 { "needle" } else { "hay" };
                format!("{word} {i:>w$}\n", w = width)
            })
            .collect();
        fs::write(sub.join("f.txt"), text).unwrap();
    }
    dir
}

/// How many times the output moves from one file to another, and how many
/// files it names. `path_of` returns a line's file, or None to skip it.
fn switches<'a>(
    lines: impl Iterator<Item = &'a str>,
    path_of: impl Fn(&str) -> Option<String>,
) -> (usize, usize) {
    let mut seen = std::collections::HashSet::new();
    let mut last = None;
    let mut n = 0;
    for path in lines.filter_map(path_of) {
        if last.as_ref() != Some(&path) {
            n += 1;
            last = Some(path.clone());
        }
        seen.insert(path);
    }
    (n, seen.len())
}

fn text_path(line: &str) -> Option<String> {
    // "dNN/f.txt:..." or "dNN/f.txt-..." (context); "--" separators.
    (line != "--").then(|| line[..9].to_string())
}

fn json_path(line: &str) -> Option<String> {
    let event: Value = serde_json::from_str(line).unwrap();
    event["path"].as_str().map(str::to_string)
}

fn text(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn each_file_is_one_block() {
    let d = tree(16, 20_000, 1);
    for args in [
        &["-j8", "-C1", "needle", "."][..],
        &["-j8", "-n", "needle", "."],
        &["-j8", "-v", "needle", "."],
    ] {
        let out = text(&argrep(d.path(), args));
        assert_eq!(switches(out.lines(), text_path), (16, 16), "{args:?}");
    }
}

#[test]
fn a_file_with_a_lot_of_output_is_still_one_block() {
    // Several MB of output per file, well over what a worker collects
    // before it sends: the rest of the file then follows directly.
    let d = tree(8, 4000, 1000);
    let out = text(&argrep(d.path(), &["-j8", "-v", "needle", "."]));
    assert_eq!(out.lines().count(), 8 * 3200);
    assert_eq!(switches(out.lines(), text_path), (8, 8));

    let out = text(&argrep(d.path(), &["-j8", "--json", "-v", "needle", "."]));
    // begin, matches, end: only the summary at the end has no path.
    assert_eq!(switches(out.lines(), json_path), (8, 8));
}

#[test]
fn lines_keep_their_order_within_a_file() {
    let d = tree(8, 20_000, 1);
    let out = text(&argrep(d.path(), &["-j8", "-n", "needle", "."]));
    let mut last = std::collections::HashMap::new();
    for line in out.lines() {
        let mut parts = line.splitn(3, ':');
        let (path, n) = (parts.next().unwrap(), parts.next().unwrap());
        let n: usize = n.parse().unwrap();
        let prev = last.insert(path.to_string(), n).unwrap_or(0);
        assert!(n > prev, "{path}: line {n} after line {prev}");
    }
}

#[cfg(unix)]
#[test]
fn a_fifo_streams_while_it_is_still_open() {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::mpsc;
    use std::time::Duration;

    let d = tree(2, 10, 1);
    let fifo = d.path().join("live");
    let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(status.success());

    let mut child = Command::new(BIN)
        .current_dir(d.path())
        .args(["--line-buffered", "needle", "live", "d00", "d01"])
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut writer = fs::OpenOptions::new().write(true).open(&fifo).unwrap();
    writeln!(writer, "needle from the fifo").unwrap();
    writer.flush().unwrap();

    // The line must come out while the FIFO is still open.
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut found = false;
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(line) if line == "live:needle from the fifo" => {
                found = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    drop(writer);
    let status = child.wait().unwrap();
    assert!(found, "the FIFO's match was held back until it closed");
    assert!(status.success());
}

#[test]
fn large_blocks_stay_whole_among_many_small_files() {
    // Files with several messages' worth of output (sent holding the order
    // lock) next to many files with a line or two each (sent as single
    // messages, several files per message): nothing may come between the
    // messages of a large block.
    let d = tree(6, 4000, 1);
    for f in 0..300 {
        let sub = d.path().join(format!("s{f:03}"));
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("f.txt"), "needle\nhay\n").unwrap();
    }
    let path_of = |line: &str| {
        (line != "--").then(|| line.split_once(':').map_or(line, |(p, _)| p).to_string())
    };
    for args in [&["-j8", "-n", "needle", "."][..], &["-j8", "needle", "."]] {
        let out = text(&argrep(d.path(), args));
        assert_eq!(switches(out.lines(), path_of), (306, 306), "{args:?}");
    }
}
