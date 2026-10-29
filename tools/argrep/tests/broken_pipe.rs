//! A closed stdout (`argrep ... | head`) must end argrep quietly with
//! status 0, never with a panic and backtrace. Same rule for every output
//! mode.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_argrep");

/// Spawns the tool with stdout on a pipe, reads one line, closes the pipe
/// while the tool still has far more to write, and returns how it ended.
/// The output must be much larger than a pipe buffer (64 KiB), so the tool
/// is guaranteed to be blocked in a write when the reader goes away.
fn close_after_first_line(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
    let mut child = Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start");
    let feeder = stdin.map(|data| {
        let mut input = child.stdin.take().unwrap();
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = input.write_all(&data);
        })
    });
    {
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        assert!(!first.is_empty(), "the tool printed nothing");
        // Dropping the reader closes the pipe.
    }
    let out = child.wait_with_output().unwrap();
    if let Some(f) = feeder {
        let _ = f.join();
    }
    out
}

fn assert_quiet_exit(args: &[&str], out: &Output) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "{args:?} panicked:\n{stderr}");
    assert!(stderr.is_empty(), "{args:?} wrote to stderr:\n{stderr}");
    assert_eq!(out.status.code(), Some(0), "{args:?}: {stderr}");
}

/// A long name, so a few thousand entries make megabytes of output.
fn long(i: usize) -> String {
    format!("{i:05}-{}", "x".repeat(200))
}

/// ~5 MB of matching lines across 2000 files, plus one big file.
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    let line = format!("TODO {}\n", "y".repeat(200));
    fs::write(dir.path().join("big.txt"), line.repeat(20_000)).unwrap();
    for i in 0..2000 {
        fs::write(dir.path().join(long(i)), "TODO\n").unwrap();
    }
    dir
}

#[test]
fn every_output_mode_exits_quietly_on_a_closed_pipe() {
    let dir = tree();
    for args in [
        &["TODO", "."][..],
        &["-n", "TODO", "."],
        &["-C", "1", "TODO", "."],
        &["-o", "TODO", "."],
        &["-r", "DONE", "TODO", "."],
        &["-l", "TODO", "."],
        &["-l0", "TODO", "."],
        &["-c", "TODO", "."],
        &["--files", "."],
        &["--json", "TODO", "."],
    ] {
        let out = close_after_first_line(dir.path(), args, None);
        assert_quiet_exit(args, &out);
    }
}

#[test]
fn stdin_mode_exits_quietly_on_a_closed_pipe() {
    let dir = TempDir::new().unwrap();
    let input = format!("TODO {}\n", "z".repeat(100)).repeat(50_000);
    let out = close_after_first_line(dir.path(), &["TODO"], Some(input.as_bytes()));
    assert_quiet_exit(&["TODO"], &out);
}

#[cfg(target_os = "linux")]
#[test]
fn other_write_errors_are_reported_not_panicked() {
    let dir = tree();
    let full = fs::File::create("/dev/full").unwrap();
    let out = Command::new(BIN)
        .current_dir(dir.path())
        .args(["TODO", "."])
        .stdout(full)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(stderr.contains("error: cannot write output"), "{stderr}");
    assert_eq!(out.status.code(), Some(1));
}
