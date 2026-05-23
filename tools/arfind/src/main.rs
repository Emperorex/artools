use clap::Parser;
use colored::Colorize;
use crossbeam_channel::{Sender, unbounded};
use glob::Pattern;
use std::{
    collections::HashSet,
    fs, hint,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Instant,
};

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

    /// Filter by type: f (file), d (directory), or l (symlink)
    #[arg(short = 't', long)]
    file_type: Option<String>,
    #[arg(short = 'H', long)]
    hidden: bool,
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
    hidden: bool,
    debug: bool,
}

/// Shared atomic counters for runtime statistics
#[derive(Clone)]
struct SearchStats {
    total_files: Arc<AtomicUsize>,
    total_dirs: Arc<AtomicUsize>,
    matched_count: Arc<AtomicUsize>,
}

/// Output item representing a match with its type metadata
struct MatchResult {
    path: PathBuf,
    is_dir: bool,
}

fn main() {
    let args = Args::parse();

    // Validate the target type flag if provided (collapsed nested if block for clippy)
    if let Some(ref t) = args.file_type
        && t != "f"
        && t != "d"
        && t != "l"
    {
        eprintln!("{}", format!("error: Invalid type '{}'. Use 'f' for files, 'd' for directories, or 'l' for symlinks.", t).red());
        std::process::exit(1);
    }

    let mut ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();

    ignore_dirs.extend(args.ignore);

    let pattern = Pattern::new(&args.name).expect("Invalid glob pattern");

    let config = Arc::new(SearchConfig {
        pattern,
        ignore_dirs,
        max_depth: args.max_depth,
        file_type: args.file_type,
        hidden: args.hidden,
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
        eprintln!("{}", "\n=== Search Statistics ===".yellow().bold());
        eprintln!(
            "Files checked:         {}",
            stats.total_files.load(Ordering::Relaxed).to_string().cyan()
        );
        eprintln!(
            "Directories checked:   {}",
            stats.total_dirs.load(Ordering::Relaxed).to_string().cyan()
        );
        eprintln!(
            "Matches found:         {}",
            stats
                .matched_count
                .load(Ordering::Relaxed)
                .to_string()
                .green()
                .bold()
        );
        eprintln!("Execution time:        {:.2?}", duration);
    }
}

fn parallel_find(root: PathBuf, workers: usize, config: Arc<SearchConfig>, stats: SearchStats) {
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
                // Highly optimized lock-free extraction loop (no recv_timeout sleep)
                let task = match task_rx.try_recv() {
                    Ok(task) => task,
                    Err(_) => {
                        // Check if all other threads have finished their scan work
                        if active_tasks.load(Ordering::SeqCst) == 0 {
                            break;
                        }
                        // Low-latency CPU backoff strategy to prevent core thrashing
                        hint::spin_loop();
                        continue;
                    }
                };

                scan_directory(task, &config, &task_tx, &output_tx, &active_tasks, &stats);

                // Atomically decrement task weight as soon as scanning finishes
                active_tasks.fetch_sub(1, Ordering::SeqCst);
            }
        });

        handles.push(handle);
    }

    drop(task_tx);
    drop(output_tx);

    // Apply colors natively based on the type of result discovered
    for item in output_rx {
        if item.is_dir {
            println!("{}", item.path.display().to_string().blue().bold());
        } else {
            println!("{}", item.path.display());
        }
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn scan_directory(
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
            Err(_) => continue, // Skip unreadable directory entries
        };

        let is_symlink = file_type.is_symlink();
        let is_dir = file_type.is_dir();

        // OPTIMIZATION: Get the file name WITHOUT allocating a full PathBuf on the heap
        let os_file_name = entry.file_name();
        let file_name = os_file_name.to_string_lossy();

        if !config.hidden && file_name.starts_with('.') {
            continue;
        }

        if !is_dir && !is_symlink {
            stats.total_files.fetch_add(1, Ordering::Relaxed);
        }

        // Evaluate filename criteria matching
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
                stats.matched_count.fetch_add(1, Ordering::Relaxed);

                // LAZY ALLOCATION: Create PathBuf ONLY when we are 100% sure we want to output it
                let _ = output_tx.send(MatchResult {
                    path: entry.path(),
                    is_dir,
                });
            }
        }

        if is_dir && !is_symlink {
            if config.ignore_dirs.contains(file_name.as_ref()) {
                continue;
            }

            if config.max_depth.is_none_or(|d| task.depth < d) {
                active_tasks.fetch_add(1, Ordering::SeqCst);

                // LAZY ALLOCATION: Create PathBuf ONLY for directories we are actually going to traverse
                let _ = task_tx.send(Task {
                    path: entry.path(),
                    depth: task.depth + 1,
                });
            }
        }
    }
}
