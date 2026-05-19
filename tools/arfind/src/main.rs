use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use clap::Parser;
use crossbeam_channel::{Sender, unbounded};
use glob::Pattern;

/// Default directories to ignore
const DEFAULT_IGNORES: &[&str] = &[".git", "node_modules", "__pycache__"];

/// CLI arguments
#[derive(Parser, Debug)]
#[command(author, version, about = "Fast parallel find (Rust version)")]
struct Args {
    /// Root directory
    #[arg(default_value = ".")]
    path: String,

    /// Filename pattern
    #[arg(short, long, default_value = "*")]
    name: String,

    /// Number of worker threads
    #[arg(short = 'j', long, default_value_t = 4)]
    jobs: usize,

    /// Maximum recursion depth
    #[arg(long)]
    max_depth: Option<usize>,

    /// Additional ignored directories
    #[arg(long)]
    ignore: Vec<String>,

    /// Filter by type: f (file) or d (directory)
    #[arg(short = 't', long)]
    file_type: Option<String>,

    /// Show search statistics
    #[arg(short, long)]
    debug: bool,
}

/// Task sent to workers
#[derive(Debug)]
struct Task {
    path: PathBuf,
    depth: usize,
}

/// Shared configuration and filters for the search
struct SearchConfig {
    pattern: Pattern,
    ignore_dirs: HashSet<String>,
    max_depth: Option<usize>,
    file_type: Option<String>,
    debug: bool,
}

/// Shared atomic counters for runtime statistics
#[derive(Clone)]
struct SearchStats {
    total_files: Arc<AtomicUsize>,
    total_dirs: Arc<AtomicUsize>,
    matched_count: Arc<AtomicUsize>,
}

fn main() {
    let args = Args::parse();

    // Validate the target type flag if provided
    if let Some(ref t) = args.file_type {
        if t != "f" && t != "d" {
            eprintln!(
                "error: Invalid type '{}'. Use 'f' for files or 'd' for directories.",
                t
            );
            std::process::exit(1);
        }
    }

    let mut ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();

    ignore_dirs.extend(args.ignore);

    let pattern = Pattern::new(&args.name).expect("Invalid glob pattern");

    let config = Arc::new(SearchConfig {
        pattern,
        ignore_dirs,
        max_depth: args.max_depth,
        file_type: args.file_type,
        debug: args.debug,
    });

    let stats = SearchStats {
        total_files: Arc::new(AtomicUsize::new(0)),
        total_dirs: Arc::new(AtomicUsize::new(0)),
        matched_count: Arc::new(AtomicUsize::new(0)),
    };

    let start_time = Instant::now();

    parallel_find(PathBuf::from(args.path), args.jobs, config, stats.clone());

    let duration = start_time.elapsed();

    if args.debug {
        eprintln!("\n=== Search Statistics ===");
        eprintln!(
            "Files checked:         {}",
            stats.total_files.load(Ordering::Relaxed)
        );
        eprintln!(
            "Directories checked:   {}",
            stats.total_dirs.load(Ordering::Relaxed)
        );
        eprintln!(
            "Matches found:         {}",
            stats.matched_count.load(Ordering::Relaxed)
        );
        eprintln!("Execution time:        {:.2?}", duration);
    }
}

fn parallel_find(root: PathBuf, workers: usize, config: Arc<SearchConfig>, stats: SearchStats) {
    let (task_tx, task_rx) = unbounded::<Task>();
    let (output_tx, output_rx) = unbounded::<PathBuf>();

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
                let task = match task_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(task) => task,
                    Err(_) => {
                        if active_tasks.load(Ordering::SeqCst) == 0 {
                            break;
                        }
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

    for path in output_rx {
        println!("{}", path.display());
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn scan_directory(
    task: Task,
    config: &SearchConfig,
    task_tx: &Sender<Task>,
    output_tx: &Sender<PathBuf>,
    active_tasks: &AtomicUsize,
    stats: &SearchStats,
) {
    stats.total_dirs.fetch_add(1, Ordering::Relaxed);

    let entries = match fs::read_dir(&task.path) {
        Ok(entries) => entries,
        Err(err) => {
            if config.debug {
                eprintln!("arfind: {}: {}", task.path.display(), err);
            }
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();

        let file_name = match path.file_name() {
            Some(name) => name.to_string_lossy(),
            None => continue,
        };

        let is_dir = path.is_dir();

        if !is_dir {
            stats.total_files.fetch_add(1, Ordering::Relaxed);
        }

        // Check filename glob pattern first
        if config.pattern.matches(&file_name) {
            // Check object type filter constraint
            let type_matches = match &config.file_type {
                Some(t) => (t == "f" && !is_dir) || (t == "d" && is_dir),
                None => true,
            };

            if type_matches {
                stats.matched_count.fetch_add(1, Ordering::Relaxed);
                let _ = output_tx.send(path.clone());
            }
        }

        if is_dir {
            if config.ignore_dirs.contains(file_name.as_ref()) {
                continue;
            }

            if config.max_depth.is_none_or(|d| task.depth < d) {
                active_tasks.fetch_add(1, Ordering::SeqCst);

                let _ = task_tx.send(Task {
                    path,
                    depth: task.depth + 1,
                });
            }
        }
    }
}
