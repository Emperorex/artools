use colored::Colorize;
use crossbeam_channel::unbounded;
use glob::Pattern;
use std::{
    collections::{HashSet, VecDeque},
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
    /// -v: print lines that do NOT match
    pub invert: bool,
    /// -l: print only filenames, not matching lines
    pub files_with_matches: bool,
    /// -c: print count of matching lines per file
    pub count_per_file: bool,
    /// --include: only search files matching this glob pattern
    pub include_pattern: Option<Pattern>,
    /// -B: number of leading context lines before a match
    pub before_context: usize,
    /// -A: number of trailing context lines after a match
    pub after_context: usize,
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
    /// Set when --count is active: total matching lines in this file
    pub count: Option<usize>,
    /// True if this result is a context line around a match (not the match itself)
    pub is_context: bool,
    /// True if this result is a group separator ("--") between non-adjacent matches
    pub is_separator: bool,
}

/// Normalizes a query string for case-insensitive matching.
/// Call this when constructing `SearchConfig` with `ignore_case: true`.
pub fn normalize_query(query: &str, ignore_case: bool) -> String {
    if ignore_case {
        query.to_lowercase()
    } else {
        query.to_string()
    }
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
    if dir_path.is_file() {
        stats.total_files.fetch_add(1, Ordering::Relaxed);
        grep_file(dir_path, config, output_tx, stats);
        return;
    }

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
            // --include: skip files whose names don't match the pattern
            if let Some(pattern) = &config.include_pattern
                && !pattern.matches(&file_name)
            {
                continue;
            }
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
        if reader.seek(SeekFrom::Start(0)).is_err() {
            return;
        }
    }

    // Process file line by line, reusing a single heap allocation
    let mut line = String::new();
    let mut line_num = 0usize;
    let mut match_count = 0usize;

    let before_ctx = config.before_context;
    let after_ctx = config.after_context;
    let has_context = before_ctx > 0 || after_ctx > 0;

    let mut before_buffer: VecDeque<(usize, String)> = VecDeque::with_capacity(before_ctx);
    let mut after_remaining = 0usize;
    let mut last_printed_line = 0usize;
    let mut has_printed_anything = false;

    while let Ok(bytes) = reader.read_line(&mut line) {
        if bytes == 0 {
            break; // EOF
        }
        line_num += 1;

        let line_matches = if config.ignore_case {
            line.to_lowercase().contains(&config.normalized_query)
        } else {
            line.contains(&config.query)
        };

        // Apply -v inversion
        let should_emit = if config.invert {
            !line_matches
        } else {
            line_matches
        };

        if should_emit {
            match_count += 1;
            stats.matched_lines.fetch_add(1, Ordering::Relaxed);

            if config.files_with_matches {
                // -l: emit the file once and stop reading further
                let _ = output_tx.send(MatchResult {
                    file_path: file_path.to_path_buf(),
                    line_num: 0,
                    line_content: String::new(),
                    count: None,
                    is_context: false,
                    is_separator: false,
                });
                break;
            } else if !config.count_per_file {
                if has_context {
                    let first_line_to_print = if let Some((b_num, _)) = before_buffer.front() {
                        std::cmp::min(*b_num, line_num)
                    } else {
                        line_num
                    };

                    if has_printed_anything && first_line_to_print > last_printed_line + 1 {
                        let _ = output_tx.send(MatchResult {
                            file_path: file_path.to_path_buf(),
                            line_num: 0,
                            line_content: String::new(),
                            count: None,
                            is_context: false,
                            is_separator: true,
                        });
                    }

                    while let Some((b_num, b_content)) = before_buffer.pop_front() {
                        if b_num > last_printed_line {
                            let _ = output_tx.send(MatchResult {
                                file_path: file_path.to_path_buf(),
                                line_num: b_num,
                                line_content: b_content,
                                count: None,
                                is_context: true,
                                is_separator: false,
                            });
                            last_printed_line = b_num;
                        }
                    }
                }

                // Normal mode: emit matching line
                let _ = output_tx.send(MatchResult {
                    file_path: file_path.to_path_buf(),
                    line_num,
                    line_content: line.clone(),
                    count: None,
                    is_context: false,
                    is_separator: false,
                });
                last_printed_line = line_num;
                has_printed_anything = true;
                after_remaining = after_ctx;

                if before_ctx > 0 {
                    before_buffer.push_back((line_num, line.clone()));
                }
            }
            // -c mode: accumulate count, emit at end
        } else if has_context && !config.count_per_file && !config.files_with_matches {
            if after_remaining > 0 {
                let _ = output_tx.send(MatchResult {
                    file_path: file_path.to_path_buf(),
                    line_num,
                    line_content: line.clone(),
                    count: None,
                    is_context: true,
                    is_separator: false,
                });
                last_printed_line = line_num;
                after_remaining -= 1;
            }

            if before_ctx > 0 {
                if before_buffer.len() == before_ctx {
                    before_buffer.pop_front();
                }
                before_buffer.push_back((line_num, line.clone()));
            }
        }

        line.clear();
    }

    // -c: emit one result per file with the total count
    if config.count_per_file {
        let _ = output_tx.send(MatchResult {
            file_path: file_path.to_path_buf(),
            line_num: 0,
            line_content: String::new(),
            count: Some(match_count),
            is_context: false,
            is_separator: false,
        });
    }
}
