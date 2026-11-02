//! Output buffering: to a pipe or file, argrep buffers its output instead
//! of writing every line on its own (one syscall each), but nothing may be
//! lost or reordered, and live inputs must still stream.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    time::Duration,
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

/// 50 files with 200 matching lines each: 10,000 lines, far more than one
/// buffer's worth.
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    for f in 0..50 {
        let text: String = (0..200).map(|i| format!("hit {f} {i}\n")).collect();
        fs::write(dir.path().join(format!("f{f:02}.txt")), text).unwrap();
    }
    dir
}

#[test]
fn every_line_arrives_through_a_pipe() {
    let dir = tree();
    let out = argrep(dir.path(), &["hit", "."]);
    assert!(out.status.success());
    assert_eq!(out.stdout.iter().filter(|&&b| b == b'\n').count(), 10_000);
    assert!(out.stdout.ends_with(b"\n"));
}

#[test]
fn stats_come_after_all_results_in_a_shared_stream() {
    let dir = tree();
    let log = dir.path().join("both.log");
    let file = fs::File::create(&log).unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_argrep"))
        .current_dir(dir.path())
        .args(["hit", ".", "--include", "*.txt", "--stats"])
        .env("NO_COLOR", "1")
        .stdout(file.try_clone().unwrap())
        .stderr(file)
        .status()
        .unwrap();
    assert!(status.success());
    let text = fs::read_to_string(&log).unwrap();
    let stats = text
        .find("=== Search Statistics ===")
        .expect("stats printed");
    let last_hit = text.rfind(":hit ").expect("results printed");
    assert!(last_hit < stats, "results must be flushed before the stats");
    assert_eq!(text[..stats].matches(":hit ").count(), 10_000);
}

#[test]
fn nothing_is_lost_on_the_error_exit() {
    // One file has a line over --max-line-size: argrep reports it and
    // exits with 1 through process::exit, after printing every match.
    let dir = tree();
    fs::write(dir.path().join("long.txt"), "x".repeat(100) + "\n").unwrap();
    let out = argrep(dir.path(), &["hit", ".", "--max-line-size", "50"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout.iter().filter(|&&b| b == b'\n').count(), 10_000);
}

#[test]
fn null_separated_and_json_output_are_complete() {
    let dir = tree();
    let out = argrep(dir.path(), &["-l0", "hit", "."]);
    assert!(out.status.success());
    assert_eq!(out.stdout.iter().filter(|&&b| b == 0).count(), 50);
    assert!(out.stdout.ends_with(b"\0"));

    let out = argrep(dir.path(), &["--json", "hit", "."]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.matches(r#""type":"match""#).count(), 10_000);
    assert!(
        text.trim_end()
            .lines()
            .last()
            .unwrap()
            .contains(r#""type":"summary""#)
    );
}

/// Reads one line from the child's stdout on another thread, so a test can
/// wait for it with a timeout while the child is still running.
fn first_line(child: &mut Child) -> mpsc::Receiver<String> {
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        if BufReader::new(stdout).read_line(&mut line).is_ok() {
            let _ = tx.send(line);
        }
    });
    rx
}

#[test]
fn stdin_input_still_streams_line_by_line() {
    // `tail -f log | argrep ERROR | ...`: a match must come out while the
    // input is still open, not when it ends.
    let mut child = Command::new(env!("CARGO_BIN_EXE_argrep"))
        .arg("ERROR")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"ok\nERROR one\n").unwrap();
    stdin.flush().unwrap();
    let rx = first_line(&mut child);
    let line = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the match must arrive while stdin is still open");
    assert_eq!(line, "ERROR one\n");
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

/// A named FIFO is searched like a file (so output is buffered), but its
/// data only ends when the writer closes it.
#[cfg(unix)]
fn fifo_search(extra: &[&str]) -> (Child, fs::File, mpsc::Receiver<String>, TempDir) {
    let dir = TempDir::new().unwrap();
    let fifo = dir.path().join("live");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_argrep"))
        .arg("hit")
        .arg(&fifo)
        .args(extra)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // Opening for writing waits until argrep has opened it for reading.
    let mut writer = fs::OpenOptions::new().write(true).open(&fifo).unwrap();
    writer.write_all(b"hit one\n").unwrap();
    writer.flush().unwrap();
    let rx = first_line(&mut child);
    (child, writer, rx, dir)
}

#[cfg(unix)]
#[test]
fn output_to_a_pipe_is_buffered_by_default() {
    // The point of the change: one match is held back until the search
    // ends, instead of costing a write of its own.
    let (mut child, writer, rx, _dir) = fifo_search(&[]);
    assert!(
        rx.recv_timeout(Duration::from_secs(2)).is_err(),
        "output should be buffered while the search runs"
    );
    drop(writer);
    let line = rx.recv_timeout(Duration::from_secs(20)).unwrap();
    assert!(line.ends_with(":hit one\n"), "{line}");
    assert!(child.wait().unwrap().success());
}

#[cfg(unix)]
#[test]
fn line_buffered_writes_each_line_at_once() {
    let (mut child, writer, rx, _dir) = fifo_search(&["--line-buffered"]);
    let line = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("--line-buffered must write the match right away");
    assert!(line.ends_with(":hit one\n"), "{line}");
    drop(writer);
    assert!(child.wait().unwrap().success());
}
