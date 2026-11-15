//! Results go to the printer over a bounded channel: when the printer
//! stalls (a paused pager), the workers wait instead of holding every
//! result in memory.

use arcommon::walk::{RESULT_BATCH, RESULT_BUFFER};
use arfind::{SearchConfig, SearchStats, parallel_find};
use glob::Pattern;
use std::{
    fs,
    sync::{Arc, Mutex, atomic::Ordering, mpsc},
    thread,
    time::Duration,
};
use tempfile::TempDir;

#[test]
fn a_stalled_printer_holds_the_workers_back() {
    // 40 directories of 500 files: 20,000 results.
    let dir = TempDir::new().unwrap();
    for d in 0..40 {
        let sub = dir.path().join(format!("d{d}"));
        fs::create_dir(&sub).unwrap();
        for f in 0..500 {
            fs::write(sub.join(format!("f{f}")), "").unwrap();
        }
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    let workers = 4;

    // The printer takes the first result, then waits to be released.
    let (release, released) = mpsc::channel::<()>();
    let gate = Mutex::new(Some(released));
    let stats = SearchStats::new();
    let found = Arc::clone(&stats.matched_count);
    let config = Arc::new(SearchConfig::new(Pattern::new("*").unwrap()));
    let search = thread::spawn(move || {
        parallel_find([root], workers, config, stats, move |_| {
            if let Some(released) = gate.lock().unwrap().take() {
                let _ = released.recv();
            }
        });
    });

    thread::sleep(Duration::from_millis(500));
    let while_stalled = found.load(Ordering::Relaxed);
    release.send(()).unwrap();
    search.join().unwrap();

    // In flight at most: the full channel, the batch being printed, and
    // per worker one batch waiting to be sent plus one being filled.
    let limit = RESULT_BUFFER + (2 * workers + 1) * RESULT_BATCH;
    assert!(
        while_stalled <= limit,
        "{while_stalled} results found while the printer was stalled (limit {limit})"
    );
    assert_eq!(found.load(Ordering::Relaxed), 20_000 + 40 + 1);
}
