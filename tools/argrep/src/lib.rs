use colored::Colorize;
use crossbeam_channel::unbounded;
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

/// Default directories to ignore during text search
pub const DEFAULT_IGNORES: &[&str] = &[".git", "node_modules", "__pycache__", "target"];

/// Shared runtime configuration
pub struct SearchConfig {
    pub query: String,
    pub normalized_query: String,
    pub ignore_case: bool,
    pub line_number: bool,
    pub ignore_dirs: HashSet<String>,
    pub debug: bool,
}

/// Shared statistics counters
#[derive(Clone)]
pub struct SearchStats {
    pub total_files: Arc<AtomicUsize>,
    pub total_dirs: Arc<AtomicUsize>,
    pub matched_lines: Arc<AtomicUsize>,
}

impl SearchStats {
    pub fn new() -> Self {
        Self {
            total_files: Arc::new(AtomicUsize::new(0)),
            total_dirs: Arc::new(AtomicUsize::new(0)),
            matched_lines: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Default for SearchStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Result payload containing a single text match
pub struct MatchResult {
    pub file_path: PathBuf,
    pub line_num: usize,
    pub line_content: String,
}

/// Builds a `SearchConfig` from the given parameters.
pub fn build_config(
    query: String,
    ignore_case: bool,
    line_number: bool,
    ignore_dirs: HashSet<String>,
    debug: bool,
) -> Arc<SearchConfig> {
    let normalized_query = if ignore_case {
        query.to_lowercase()
    } else {
        query.clone()
    };

    Arc::new(SearchConfig {
        query,
        normalized_query,
        ignore_case,
        line_number,
        ignore_dirs,
        debug,
    })
}

/// Runs a parallel grep across all text files under `root`,
/// calling `on_match` for every matched line.
pub fn parallel_grep(
    root: PathBuf,
    workers: usize,
    config: Arc<SearchConfig>,
    stats: SearchStats,
    on_match: impl Fn(MatchResult) + Send + 'static,
) {
    let (task_tx, task_rx) = unbounded::<PathBuf>();
    let (output_tx, output_rx) = unbounded::<MatchResult>();
    let active_tasks = Arc::new(AtomicUsize::new(1));

    task_tx.send(root).unwrap();

    let mut handles = Vec::new();

    for _ in 0..workers {
        let task_rx = task_rx.clone();
        let task_tx = task_tx.clone();
        let output_tx = output_tx.clone();
        let config = Arc::clone(&config);
        let stats = stats.clone();
        let active_tasks = Arc::clone(&active_tasks);

        let handle = thread::spawn(move || {
            loop {
                let task_path = crossbeam_channel::select! {
                    recv(task_rx) -> msg => match msg {
                        Ok(path) => path,
                        Err(_) => break,
                    },
                    default => {
                        if active_tasks.load(Ordering::SeqCst) == 0 {
                            break;
                        }
                        thread::yield_now();
                        continue;
                    }
                };

                scan_and_grep(
                    &task_path,
                    &config,
                    &task_tx,
                    &output_tx,
                    &active_tasks,
                    &stats,
                );
                active_tasks.fetch_sub(1, Ordering::SeqCst);
            }
        });

        handles.push(handle);
    }

    drop(task_tx);
    drop(output_tx);

    let printer_handle = thread::spawn(move || {
        for item in output_rx {
            on_match(item);
        }
    });

    for handle in handles {
        handle.join().unwrap();
    }

    printer_handle.join().unwrap();
}

pub fn scan_and_grep(
    dir_path: &Path,
    config: &SearchConfig,
    task_tx: &crossbeam_channel::Sender<PathBuf>,
    output_tx: &crossbeam_channel::Sender<MatchResult>,
    active_tasks: &AtomicUsize,
    stats: &SearchStats,
) {
    stats.total_dirs.fetch_add(1, Ordering::Relaxed);

    let entries = match fs::read_dir(dir_path) {
        Ok(entries) => entries,
        Err(err) => {
            if config.debug {
                eprintln!("{}: {}: {}", "argrep".red(), dir_path.display(), err);
            }
            return;
        }
    };

    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if file_type.is_symlink() {
            continue; // Skip symlinks to prevent reference cycle traps
        }

        let os_file_name = entry.file_name();
        let file_name = os_file_name.to_string_lossy();

        if file_name.starts_with('.') {
            continue; // Skip hidden files/folders by default
        }

        if file_type.is_dir() {
            if config.ignore_dirs.contains(file_name.as_ref()) {
                continue;
            }
            active_tasks.fetch_add(1, Ordering::SeqCst);
            let _ = task_tx.send(entry.path());
        } else {
            stats.total_files.fetch_add(1, Ordering::Relaxed);
            grep_file(&entry.path(), config, output_tx, stats);
        }
    }
}

pub fn grep_file(
    file_path: &Path,
    config: &SearchConfig,
    output_tx: &crossbeam_channel::Sender<MatchResult>,
    stats: &SearchStats,
) {
    let file = match File::open(file_path) {
        Ok(f) => f,
        Err(err) => {
            if config.debug {
                eprintln!("{}: {}: {}", "argrep".red(), file_path.display(), err);
            }
            return;
        }
    };

    let mut reader = BufReader::new(file);

    // Fast binary file sniffing: check the first 1024 bytes for a null byte
    let mut sniffer_buffer = [0u8; 1024];
    if let Ok(bytes_read) = reader.read(&mut sniffer_buffer) {
        if sniffer_buffer[..bytes_read].contains(&0u8) {
            return; // Skip compiled binaries or media files
        }
        // Seek back to the start using the underlying file handle
        if reader.seek(SeekFrom::Start(0)).is_err() {
            return;
        }
    }

    // Process file line by line, reusing a single heap allocation
    let mut line = String::new();
    let mut line_num = 0;

    while let Ok(bytes) = reader.read_line(&mut line) {
        if bytes == 0 {
            break; // EOF
        }
        line_num += 1;

        let is_match = if config.ignore_case {
            line.to_lowercase().contains(&config.normalized_query)
        } else {
            line.contains(&config.query)
        };

        if is_match {
            stats.matched_lines.fetch_add(1, Ordering::Relaxed);
            let _ = output_tx.send(MatchResult {
                file_path: file_path.to_path_buf(),
                line_num,
                line_content: line.clone(),
            });
        }
        line.clear();
    }
}
