//! `--progress`: a live counter on stderr while the scan runs.
//!
//! Workers add their totals once per directory (a few relaxed atomic adds),
//! so the counter costs next to nothing. A separate thread reads it and
//! prints:
//!
//! * on a terminal, one line rewritten in place about ten times a second,
//!   erased again when the scan ends so it never mixes with the report;
//! * when stderr is redirected (a log file, CI), a plain line every two
//!   seconds and a final summary line, with no control characters.
//!
//! Nothing is ever written to stdout, so `--json` output stays clean.

use crate::format_size_plain;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// How often the line is redrawn on a terminal.
const TTY_INTERVAL: Duration = Duration::from_millis(100);
/// How often a line is printed when stderr is not a terminal.
const LOG_INTERVAL: Duration = Duration::from_secs(2);

/// Running totals, updated by the scan workers.
#[derive(Debug, Default)]
pub struct ScanProgress {
    dirs: AtomicU64,
    files: AtomicU64,
    amount: AtomicU64,
}

/// A consistent-enough copy of the counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Directories scanned so far.
    pub dirs: u64,
    /// Files counted so far (after filters; hard links once).
    pub files: u64,
    /// Bytes of those files, or inodes with `--inodes`.
    pub amount: u64,
}

impl ScanProgress {
    /// Records one finished directory.
    pub fn add_dir(&self, files: u64, amount: u64) {
        self.dirs.fetch_add(1, Ordering::Relaxed);
        self.files.fetch_add(files, Ordering::Relaxed);
        self.amount.fetch_add(amount, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            dirs: self.dirs.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
            amount: self.amount.load(Ordering::Relaxed),
        }
    }
}

/// `1234567` -> `"1,234,567"`.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `"3.2s"`, `"2m05s"`, `"1h02m"`.
pub fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{:.1}s", d.as_secs_f64())
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, secs % 3600 / 60)
    }
}

/// The counter text, e.g. `12,345 dirs, 456,789 files, 12.30 GB, 3.2s`.
/// With `inodes`, the size is left out (the files count says enough).
pub fn describe(s: Snapshot, elapsed: Duration, inodes: bool) -> String {
    let mut text = format!(
        "{} dir{}, {} file{}",
        group_digits(s.dirs),
        if s.dirs == 1 { "" } else { "s" },
        group_digits(s.files),
        if s.files == 1 { "" } else { "s" },
    );
    if !inodes {
        text.push_str(", ");
        text.push_str(&format_size_plain(s.amount));
    }
    text.push_str(", ");
    text.push_str(&format_elapsed(elapsed));
    text
}

/// Where status lines go, and how.
#[derive(Debug, Clone, Copy)]
struct Sink {
    tty: bool,
}

impl Sink {
    /// Replaces the current status (terminal) or prints it as a line (log).
    fn show(&self, text: &str) {
        let mut err = io::stderr().lock();
        let _ = if self.tty {
            write!(err, "\r\x1b[2K{text}")
        } else {
            writeln!(err, "{text}")
        };
        let _ = err.flush();
    }

    /// Erases the status line on a terminal; no-op otherwise.
    fn clear(&self) {
        if self.tty {
            let mut err = io::stderr().lock();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
        }
    }
}

/// Calls `tick` every `interval` until told to stop.
///
/// `recv_timeout` doubles as the tick and the stop signal, so stopping never
/// waits a full interval. Only a timeout means "keep going": a stop signal
/// ends the loop, and so does a disconnected channel, which is what happens
/// when the [`Reporter`] is dropped without [`Reporter::finish`] (e.g. while
/// unwinding from a panic). `recv_timeout` returns `Disconnected` at once,
/// so treating it as "keep going" would spin and flood stderr.
fn tick_until_stopped(stopped: &Receiver<()>, interval: Duration, mut tick: impl FnMut()) {
    loop {
        match stopped.recv_timeout(interval) {
            Err(RecvTimeoutError::Timeout) => tick(),
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// The thread that prints the counter. Stop it with [`Reporter::finish`].
pub struct Reporter {
    stop: Sender<()>,
    handle: JoinHandle<()>,
    progress: Arc<ScanProgress>,
    started: Instant,
    inodes: bool,
    sink: Sink,
}

impl Reporter {
    /// Starts reporting `progress` to stderr.
    pub fn start(progress: Arc<ScanProgress>, inodes: bool) -> Reporter {
        let sink = Sink {
            tty: io::stderr().is_terminal(),
        };
        let interval = if sink.tty { TTY_INTERVAL } else { LOG_INTERVAL };
        let started = Instant::now();
        let (stop, stopped) = bounded::<()>(1);
        let shared = Arc::clone(&progress);
        let handle = thread::spawn(move || {
            tick_until_stopped(&stopped, interval, || {
                let text = describe(shared.snapshot(), started.elapsed(), inodes);
                sink.show(&format!("scanning: {text}"));
            });
        });
        Reporter {
            stop,
            handle,
            progress,
            started,
            inodes,
            sink,
        }
    }

    /// Stops the counter. On a terminal the line is erased; in a log a final
    /// `scanned: ...` line is printed. Returns a [`Status`] for later phases.
    pub fn finish(self) -> Status {
        let _ = self.stop.send(());
        let _ = self.handle.join();
        if self.sink.tty {
            self.sink.clear();
        } else {
            let text = describe(
                self.progress.snapshot(),
                self.started.elapsed(),
                self.inodes,
            );
            self.sink.show(&format!("scanned: {text}"));
        }
        Status { sink: self.sink }
    }
}

/// Shows one-off status messages after the scan (e.g. while duplicates are
/// being compared), in the same style as the counter.
#[derive(Debug, Clone, Copy)]
pub struct Status {
    sink: Sink,
}

impl Status {
    pub fn show(&self, text: &str) {
        self.sink.show(text);
    }

    pub fn clear(&self) {
        self.sink.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the ticker on a thread with a 5 ms interval, applies `end` to
    /// the stop channel after ~50 ms, and returns how many ticks happened.
    /// Fails (instead of hanging) if the ticker doesn't return.
    fn ticks_until(end: impl FnOnce(Sender<()>)) -> usize {
        use std::sync::atomic::AtomicUsize;
        let (stop, stopped) = bounded::<()>(1);
        let ticks = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ticks);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            tick_until_stopped(&stopped, Duration::from_millis(5), || {
                counter.fetch_add(1, Ordering::Relaxed);
            });
            let _ = done_tx.send(());
        });
        thread::sleep(Duration::from_millis(50));
        end(stop);
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the progress thread kept running");
        ticks.load(Ordering::Relaxed)
    }

    #[test]
    fn ticker_ticks_until_stopped() {
        let n = ticks_until(|stop| stop.send(()).unwrap());
        assert!(n >= 2, "expected regular ticks, got {n}");
    }

    #[test]
    fn ticker_ends_when_the_sender_is_dropped() {
        // Reporter dropped without finish(): the channel disconnects and the
        // thread must end instead of spinning.
        let n = ticks_until(drop);
        assert!(n < 100, "ticked {n} times in ~50ms: spinning, not ticking");
    }

    #[test]
    fn reporter_dropped_without_finish_does_not_keep_running() {
        let progress = Arc::new(ScanProgress::default());
        let reporter = Reporter::start(Arc::clone(&progress), false);
        // What dropping the Reporter does to the thread: the sender goes
        // away. The handle is kept so the thread's end can be observed.
        let Reporter { stop, handle, .. } = reporter;
        drop(stop);
        let handle_finished = handle;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle_finished.is_finished() {
            assert!(Instant::now() < deadline, "progress thread kept running");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn digits_are_grouped_by_thousands() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(12_345), "12,345");
        assert_eq!(group_digits(1_234_567), "1,234,567");
        assert_eq!(group_digits(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn elapsed_formats() {
        assert_eq!(format_elapsed(Duration::from_millis(3_240)), "3.2s");
        assert_eq!(format_elapsed(Duration::from_secs(125)), "2m05s");
        assert_eq!(format_elapsed(Duration::from_secs(3_720)), "1h02m");
    }

    #[test]
    fn describe_shows_counts_size_and_time() {
        let s = Snapshot {
            dirs: 12_345,
            files: 456_789,
            amount: 1536,
        };
        assert_eq!(
            describe(s, Duration::from_millis(1_500), false),
            "12,345 dirs, 456,789 files, 1.50 KB, 1.5s"
        );
        assert_eq!(
            describe(s, Duration::from_millis(1_500), true),
            "12,345 dirs, 456,789 files, 1.5s"
        );
        let one = Snapshot {
            dirs: 1,
            files: 1,
            amount: 0,
        };
        assert_eq!(describe(one, Duration::ZERO, true), "1 dir, 1 file, 0.0s");
    }

    #[test]
    fn counters_add_up_across_threads() {
        let p = Arc::new(ScanProgress::default());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let p = Arc::clone(&p);
                thread::spawn(move || {
                    for _ in 0..1000 {
                        p.add_dir(2, 10);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            p.snapshot(),
            Snapshot {
                dirs: 8000,
                files: 16_000,
                amount: 80_000
            }
        );
    }
}
