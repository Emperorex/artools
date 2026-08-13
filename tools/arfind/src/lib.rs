use clap::Parser;
use colored::Colorize;
use crossbeam_channel::{Sender, unbounded};
use glob::Pattern;
use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

pub const DEFAULT_IGNORES: &[&str] = &[".git", "node_modules", "__pycache__"];

/// CLI arguments
#[derive(Parser, Debug)]
#[command(author, version, about = "Fast parallel find (Rust version)")]
pub struct Args {
    /// Root directory
    #[arg(default_value = ".")]
    pub path: String,

    /// Filename pattern
    #[arg(short, long, default_value = "*")]
    pub name: String,

    /// Number of worker threads
    #[arg(short = 'j', long, default_value_t = 4)]
    pub jobs: usize,

    /// Maximum recursion depth
    #[arg(long)]
    pub max_depth: Option<usize>,

    /// Additional ignored directories
    #[arg(long)]
    pub ignore: Vec<String>,

    /// Filter by type: f (file), d (directory), or l (symlink)
    #[arg(short = 't', long)]
    pub file_type: Option<String>,

    #[arg(short = 'H', long)]
    pub hidden: bool,

    #[arg(short, long)]
    pub debug: bool,

    /// Filter by file size: N or +N (larger than), -N (smaller than).
    /// Supported units: B, KB, MB, GB, TB. Examples: 100MB, +100MB, -1KB
    #[arg(long, allow_hyphen_values = true)]
    pub size: Option<String>,

    /// Only match empty files (size 0) or empty directories (no children)
    #[arg(short = 'e', long)]
    pub empty: bool,

    /// Print only the total count of matches instead of paths
    #[arg(short = 'c', long)]
    pub count: bool,
}

/// Task sent to workers
#[derive(Debug)]
pub struct Task {
    pub path: PathBuf,
    pub depth: usize,
}

/// Size filter with optional lower and upper bounds (in bytes).
/// Mirrors find's +N (greater than) and -N (less than) semantics.
pub struct SizeFilter {
    pub min: Option<u64>, // +N  → size > N
    pub max: Option<u64>, // -N  → size < N
}

/// Shared configuration and filters for the search
pub struct SearchConfig {
    pub pattern: Pattern,
    pub ignore_dirs: HashSet<String>,
    pub max_depth: Option<usize>,
    pub file_type: Option<String>,
    pub hidden: bool,
    pub debug: bool,
    pub size_filter: Option<SizeFilter>,
    pub empty_only: bool,
}

/// Shared atomic counters for runtime statistics
#[derive(Clone)]
pub struct SearchStats {
    pub total_files: Arc<AtomicUsize>,
    pub total_dirs: Arc<AtomicUsize>,
    pub matched_count: Arc<AtomicUsize>,
}

impl SearchStats {
    pub fn new() -> Self {
        Self {
            total_files: Arc::new(AtomicUsize::new(0)),
            total_dirs: Arc::new(AtomicUsize::new(0)),
            matched_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Default for SearchStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Output item representing a match with its type metadata
pub struct MatchResult {
    pub path: PathBuf,
    pub is_dir: bool,
}

pub fn parallel_find(
    root: PathBuf,
    workers: usize,
    config: Arc<SearchConfig>,
    stats: SearchStats,
    on_match: impl Fn(MatchResult) + Send + 'static,
) {
    let (task_tx, task_rx) = unbounded::<Task>();
    let (output_tx, output_rx) = unbounded::<MatchResult>();

    let active_tasks = Arc::new(AtomicUsize::new(1));

    task_tx
        .send(Task {
            path: root,
            depth: 0,
        })
        .unwrap();

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
                let task = crossbeam_channel::select! {
                    recv(task_rx) -> msg => match msg {
                        Ok(task) => task,
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

                scan_directory(task, &config, &task_tx, &output_tx, &active_tasks, &stats);
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

pub fn scan_directory(
    task: Task,
    config: &SearchConfig,
    task_tx: &Sender<Task>,
    output_tx: &Sender<MatchResult>,
    active_tasks: &AtomicUsize,
    stats: &SearchStats,
) {
    stats.total_dirs.fetch_add(1, Ordering::Relaxed);

    let entries = match fs::read_dir(&task.path) {
        Ok(entries) => entries,
        Err(err) => {
            if config.debug {
                eprintln!(
                    "{}",
                    format!("arfind: {}: {}", task.path.display(), err).red()
                );
            }
            return;
        }
    };

    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        let is_symlink = file_type.is_symlink();
        let is_dir = file_type.is_dir();

        let os_file_name = entry.file_name();
        let file_name = os_file_name.to_string_lossy();

        if !config.hidden && file_name.starts_with('.') {
            continue;
        }

        if !is_dir {
            stats.total_files.fetch_add(1, Ordering::Relaxed);
        }

        if config.pattern.matches(&file_name) {
            let type_matches = match &config.file_type {
                Some(t) => {
                    (t == "f" && !is_dir && !is_symlink)
                        || (t == "d" && is_dir)
                        || (t == "l" && is_symlink)
                }
                None => true,
            };

            if type_matches {
                // Metadata is only fetched lazily, at most once per entry, and
                // reused by both the --empty and --size checks below (they used
                // to each call entry.metadata() independently).
                let mut metadata_cache: Option<fs::Metadata> = None;

                // --empty: only pay the extra syscall (read_dir/metadata) when
                // the flag is actually set; previously this ran unconditionally
                // for every matched entry.
                if config.empty_only {
                    let is_empty = if is_dir {
                        // A directory is empty if it has no children
                        fs::read_dir(entry.path())
                            .map(|mut d| d.next().is_none())
                            .unwrap_or(false)
                    } else {
                        if metadata_cache.is_none() {
                            metadata_cache = entry.metadata().ok();
                        }
                        metadata_cache
                            .as_ref()
                            .map(|m| m.len() == 0)
                            .unwrap_or(false)
                    };

                    if !is_empty {
                        continue;
                    }
                }

                // --size: apply size filter to files only (dirs have no single size)
                let size_ok = if !is_dir && !is_symlink {
                    match &config.size_filter {
                        Some(f) => {
                            if metadata_cache.is_none() {
                                metadata_cache = entry.metadata().ok();
                            }
                            let len = metadata_cache.as_ref().map(|m| m.len()).unwrap_or(0);
                            f.min.is_none_or(|min| len > min) && f.max.is_none_or(|max| len < max)
                        }
                        None => true,
                    }
                } else {
                    true // size filter does not apply to directories or symlinks
                };

                if size_ok {
                    stats.matched_count.fetch_add(1, Ordering::Relaxed);
                    let _ = output_tx.send(MatchResult {
                        path: entry.path(),
                        is_dir,
                    });
                }
            }
        }

        if is_dir && !is_symlink {
            if config.ignore_dirs.contains(file_name.as_ref()) {
                continue;
            }

            if config.max_depth.is_none_or(|d| task.depth < d) {
                active_tasks.fetch_add(1, Ordering::SeqCst);
                let _ = task_tx.send(Task {
                    path: entry.path(),
                    depth: task.depth + 1,
                });
            }
        }
    }
}
