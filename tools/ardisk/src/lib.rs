use colored::Colorize;
use crossbeam_channel::unbounded;
use glob::Pattern;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Default directories to ignore during scanning
pub const DEFAULT_IGNORES: &[&str] = &[".git", "node_modules", "__pycache__"];

/// Task sent to workers representing a directory to scan
pub struct Task {
    pub path: PathBuf,
}

/// Configuration shared across worker threads
pub struct ScanConfig {
    pub ignore_dirs: HashSet<String>,
    /// When set, only files whose names match this glob pattern contribute to
    /// directory sizes. Directories themselves are always traversed regardless.
    pub include_pattern: Option<Pattern>,
    pub debug: bool,
    /// When true, use logical file size (metadata.len()) matching du -sh.
    /// When false (default), use physical block allocation (blocks * 512).
    pub apparent_size: bool,
}

/// Builds a `ScanConfig` from the given parameters.
pub fn build_config(
    ignore_dirs: HashSet<String>,
    include_pattern: Option<Pattern>,
    debug: bool,
    apparent_size: bool,
) -> Arc<ScanConfig> {
    Arc::new(ScanConfig {
        ignore_dirs,
        include_pattern,
        debug,
        apparent_size,
    })
}

/// Runs a parallel scan rooted at `root` and returns raw per-directory sizes
/// (immediate files only — not yet rolled up to ancestors).
pub fn parallel_scan(
    root: PathBuf,
    workers: usize,
    config: Arc<ScanConfig>,
) -> HashMap<PathBuf, u64> {
    let (task_tx, task_rx) = unbounded::<Task>();
    let active_tasks = Arc::new(AtomicUsize::new(1));
    let raw_sizes = Arc::new(Mutex::new(HashMap::<PathBuf, u64>::new()));

    task_tx.send(Task { path: root }).unwrap();

    let mut handles = Vec::new();

    for _ in 0..workers {
        let task_rx = task_rx.clone();
        let task_tx = task_tx.clone();
        let config = Arc::clone(&config);
        let raw_sizes = Arc::clone(&raw_sizes);
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

                scan_directory(&task.path, &config, &task_tx, &active_tasks, &raw_sizes);
                active_tasks.fetch_sub(1, Ordering::SeqCst);
            }
        });

        handles.push(handle);
    }

    drop(task_tx);

    for handle in handles {
        handle.join().unwrap();
    }

    Arc::try_unwrap(raw_sizes).unwrap().into_inner().unwrap()
}

pub fn scan_directory(
    dir_path: &Path,
    config: &ScanConfig,
    task_tx: &crossbeam_channel::Sender<Task>,
    active_tasks: &AtomicUsize,
    raw_sizes: &Mutex<HashMap<PathBuf, u64>>,
) {
    let entries = match fs::read_dir(dir_path) {
        Ok(entries) => entries,
        Err(err) => {
            if config.debug {
                eprintln!("{}: {}: {}", "ardisk".red(), dir_path.display(), err);
            }
            return;
        }
    };

    let mut local_dir_size = 0u64;

    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if file_type.is_symlink() {
            continue;
        }

        let os_file_name = entry.file_name();
        let file_name = os_file_name.to_string_lossy();

        if file_type.is_dir() {
            if config.ignore_dirs.contains(file_name.as_ref()) {
                continue;
            }
            active_tasks.fetch_add(1, Ordering::SeqCst);
            let _ = task_tx.send(Task { path: entry.path() });
        } else {
            // If --include is set, skip files that don't match the pattern
            if let Some(pattern) = &config.include_pattern
                && !pattern.matches(&file_name)
            {
                continue;
            }

            if let Ok(metadata) = entry.metadata() {
                #[cfg(unix)]
                {
                    if config.apparent_size {
                        // Logical size — matches du -sh on macOS/Linux
                        local_dir_size += metadata.len();
                    } else {
                        // Physical block allocation — actual on-disk usage
                        local_dir_size += metadata.blocks() * 512;
                    }
                }
                #[cfg(not(unix))]
                {
                    local_dir_size += metadata.len();
                }
            }
        }
    }

    let mut guard = raw_sizes.lock().unwrap();
    guard.insert(dir_path.to_path_buf(), local_dir_size);
}

/// Propagates weights from deeply nested folders up the tree
/// using a single-pass dynamic programming bottom-up rollup.
pub fn aggregate_sizes(
    raw_sizes: &HashMap<PathBuf, u64>,
    base_path: &Path,
) -> HashMap<PathBuf, u64> {
    let mut aggregated = HashMap::new();

    let mut paths: Vec<&PathBuf> = raw_sizes.keys().collect();
    paths.sort_by_key(|p| std::cmp::Reverse(p.components().count()));

    for path in paths {
        let size = raw_sizes[path];

        *aggregated.entry(path.to_path_buf()).or_insert(0u64) += size;

        if let Some(parent) = path.parent()
            && parent.starts_with(base_path)
        {
            let child_accumulated_size = *aggregated.get(path).unwrap_or(&0u64);
            *aggregated.entry(parent.to_path_buf()).or_insert(0u64) += child_accumulated_size;
        }
    }

    aggregated
}

/// Formats raw bytes into human-readable strings (e.g., KB, MB, GB, TB)
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;

    if bytes >= TB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
            .magenta()
            .bold()
            .to_string()
    } else if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
            .cyan()
            .to_string()
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
            .green()
            .to_string()
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64).to_string()
    } else {
        format!("{} B", bytes).to_string()
    }
}
