//! A closed stdout (`arfind ... | head`) must end arfind quietly with
//! status 0, never with a panic and backtrace.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_arfind");

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

/// 5000 entries with ~210-byte names: about 1 MB of paths.
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    for i in 0..5000 {
        fs::write(dir.path().join(long(i)), "").unwrap();
    }
    dir
}

#[test]
fn every_output_mode_exits_quietly_on_a_closed_pipe() {
    let dir = tree();
    for args in [
        &["."][..],
        &[".", "--sort", "name"],
        &[".", "-t", "f", "--dry-run", "--exec", "echo"],
        &[".", "-t", "f", "--dry-run", "--execdir", "echo"],
    ] {
        let out = close_after_first_line(dir.path(), args, None);
        assert_quiet_exit(args, &out);
    }
}

#[test]
fn print0_exits_quietly_on_a_closed_pipe() {
    // -0 output has no newlines, so read a chunk instead of a line.
    let dir = tree();
    let mut child = Command::new(BIN)
        .current_dir(dir.path())
        .args([".", "-0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdout = child.stdout.take().unwrap();
        let mut buf = [0u8; 4096];
        std::io::Read::read_exact(&mut stdout, &mut buf).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert_quiet_exit(&["-0"], &out);
}

#[cfg(target_os = "linux")]
#[test]
fn other_write_errors_are_reported_not_panicked() {
    let dir = tree();
    let full = fs::File::create("/dev/full").unwrap();
    let out = Command::new(BIN)
        .current_dir(dir.path())
        .arg(".")
        .stdout(full)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("error: cannot write output"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(out.status.code(), Some(1));
}
