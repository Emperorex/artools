use std::{
    collections::{HashMap, HashSet},
    fs, hint,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Instant,
};

use clap::Parser;
use colored::Colorize;
use crossbeam_channel::unbounded;

/// Default directories to ignore
const DEFAULT_IGNORES: &[&str] = &[".git", "node_modules", "__pycache__"];

/// CLI arguments for ardisk
#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Fast parallel disk usage analyzer (Rust version)"
)]
struct Args {
    /// Target directory to analyze
    #[arg(default_value = ".")]
    path: String,

    /// Number of worker threads
    #[arg(short = 'j', long, default_value_t = 4)]
    jobs: usize,

    /// Show errors and detailed execution metrics
    #[arg(short, long)]
    debug: bool,

    /// Maximum depth of directories to display in the report
    #[arg(long)]
    max_depth: Option<usize>,
}

/// Task sent to workers representing a directory to scan
struct Task {
    path: PathBuf,
}

/// Configuration shared across worker threads
struct ScanConfig {
    ignore_dirs: HashSet<String>,
    debug: bool,
}

fn main() {
    let args = Args::parse();

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let target_path = fs::canonicalize(&args.path).unwrap_or_else(|_| PathBuf::from(&args.path));

    let config = Arc::new(ScanConfig {
        ignore_dirs,
        debug: args.debug,
    });

    // Shared thread-safe storage for folder sizes (only tracks immediate files inside that exact folder)
    let raw_sizes = Arc::new(Mutex::new(HashMap::<PathBuf, u64>::new()));

    let start_time = Instant::now();

    // Phase 1: Parallel file scanning
    parallel_scan(
        target_path.clone(),
        args.jobs,
        config,
        Arc::clone(&raw_sizes),
    );

    // Phase 2: Post-processing (aggregation and rollup from bottom to top)
    let raw_sizes_map = Arc::try_unwrap(raw_sizes).unwrap().into_inner().unwrap();
    let aggregated_sizes = aggregate_sizes(&raw_sizes_map, &target_path);

    let duration = start_time.elapsed();

    // Sort results by size descending and print top folders
    let mut sorted_results: Vec<(&PathBuf, &u64)> = aggregated_sizes.iter().collect();
    sorted_results.sort_by(|a, b| b.1.cmp(a.1));

    println!("\n{}", "=== Top Directories ===".yellow().bold());

    let mut printed_count = 0;
    for (path, size) in sorted_results.iter() {
        if printed_count >= 20 {
            break;
        }

        if let Ok(rel_path) = path.strip_prefix(&target_path) {
            let current_depth = rel_path.components().count();

            if args.max_depth.is_none_or(|max_d| current_depth <= max_d) {
                println!("{:>10}  {}", format_size(**size), path.display());
                printed_count += 1;
            }
        }
    }

    if args.debug {
        eprintln!("\n{}", "=== Operational Metrics ===".green().bold());
        eprintln!("Total scanned folders: {}", raw_sizes_map.len());
        eprintln!("Execution time:        {:.2?}", duration);
    }
}

fn parallel_scan(
    root: PathBuf,
    workers: usize,
    config: Arc<ScanConfig>,
    raw_sizes: Arc<Mutex<HashMap<PathBuf, u64>>>,
) {
    let (task_tx, task_rx) = unbounded::<Task>();
    let active_tasks = Arc::new(AtomicUsize::new(1));

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
                let task = match task_rx.try_recv() {
                    Ok(task) => task,
                    Err(_) => {
                        // Defensive check: exit only if no active workers are processing directories
                        // AND the lock-free task queue is completely drained.
                        if active_tasks.load(Ordering::SeqCst) == 0 && task_rx.is_empty() {
                            break;
                        }
                        hint::spin_loop();
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
}

fn scan_directory(
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
            // Highly optimized cross-platform file size retrieval without redundant path allocation
            #[cfg(unix)]
            {
                // On Unix (macOS APFS / Linux Ext4), directory entries often pre-cache metadata bits.
                // Standard entry.metadata() utilizes this cache on modern Unix systems when available.
                if let Ok(metadata) = entry.metadata() {
                    local_dir_size += metadata.len();
                }
            }

            #[cfg(not(unix))]
            {
                // On Windows (NTFS), the WIN32_FIND_DATA structure already populated during read_dir
                // contains the exact file size, meaning entry.metadata() returns it instantly from RAM.
                if let Ok(metadata) = entry.metadata() {
                    local_dir_size += metadata.len();
                }
            }
        }
    }

    // Lock the mutex briefly to store this specific directory's raw file size
    let mut guard = raw_sizes.lock().unwrap();
    guard.insert(dir_path.to_path_buf(), local_dir_size);
}

/// Propagates weights from deeply nested folders up to their ancestors
fn aggregate_sizes(raw_sizes: &HashMap<PathBuf, u64>, base_path: &Path) -> HashMap<PathBuf, u64> {
    let mut aggregated = HashMap::new();
    // Видалено: if *size == 0 { continue; } для правильного підрахунку вкладених папок
    for (path, size) in raw_sizes {
        let mut current: &Path = path.as_path();
        while current.starts_with(base_path) {
            *aggregated.entry(current.to_path_buf()).or_insert(0u64) += size;
            if let Some(parent) = current.parent() {
                if parent == current {
                    break;
                } // Запобігання зацикленню
                current = parent;
            } else {
                break;
            }
        }
    }
    aggregated
}

/// Formats raw bytes into human-readable strings (e.g., KB, MB, GB, TB)
fn format_size(bytes: u64) -> String {
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
