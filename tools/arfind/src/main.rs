use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use clap::Parser;
use crossbeam_channel::{unbounded, Sender};
use glob::Pattern;

/// Default directories to ignore
const DEFAULT_IGNORES: &[&str] = &[
    ".git",
    "node_modules",
    "__pycache__",
];

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

fn main() {
    let args = Args::parse();

    let mut ignore_dirs: HashSet<String> =
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();

    ignore_dirs.extend(args.ignore);

    let pattern = Pattern::new(&args.name)
        .expect("Invalid glob pattern");

    let start_time = Instant::now();

    let total_files = Arc::new(AtomicUsize::new(0));
    let total_dirs = Arc::new(AtomicUsize::new(0));
    let matched_count = Arc::new(AtomicUsize::new(0));

    parallel_find(
        PathBuf::from(args.path),
        pattern,
        args.jobs,
        ignore_dirs,
        args.max_depth,
        Arc::clone(&total_files),
        Arc::clone(&total_dirs),
        Arc::clone(&matched_count),
    );

    let duration = start_time.elapsed();

    // Статистика виводиться тільки якщо передано прапорець --debug або -d
    if args.debug {
        eprintln!("\n=== Search Statistics ===");
        eprintln!("Files checked:         {}", total_files.load(Ordering::Relaxed));
        eprintln!("Directories checked:   {}", total_dirs.load(Ordering::Relaxed));
        eprintln!("Matches found:         {}", matched_count.load(Ordering::Relaxed));
        eprintln!("Execution time:        {:.2?}", duration);
        eprintln!("\n");
    }
}

fn parallel_find(
    root: PathBuf,
    pattern: Pattern,
    workers: usize,
    ignore_dirs: HashSet<String>,
    max_depth: Option<usize>,
    total_files: Arc<AtomicUsize>,
    total_dirs: Arc<AtomicUsize>,
    matched_count: Arc<AtomicUsize>,
) {
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

        let ignore_dirs = ignore_dirs.clone();
        let pattern = pattern.clone();
        let active_tasks = Arc::clone(&active_tasks);

        let total_files = Arc::clone(&total_files);
        let total_dirs = Arc::clone(&total_dirs);
        let matched_count = Arc::clone(&matched_count);

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

                scan_directory(
                    task,
                    &pattern,
                    &ignore_dirs,
                    max_depth,
                    &task_tx,
                    &output_tx,
                    &active_tasks,
                    &total_files,
                    &total_dirs,
                    &matched_count,
                );

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
    pattern: &Pattern,
    ignore_dirs: &HashSet<String>,
    max_depth: Option<usize>,
    task_tx: &Sender<Task>,
    output_tx: &Sender<PathBuf>,
    active_tasks: &AtomicUsize,
    total_files: &AtomicUsize,
    total_dirs: &AtomicUsize,
    matched_count: &AtomicUsize,
) {
    let entries = match fs::read_dir(&task.path) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    total_dirs.fetch_add(1, Ordering::Relaxed);

    for entry in entries.flatten() {
        let path = entry.path();

        let file_name = match path.file_name() {
            Some(name) => name.to_string_lossy(),
            None => continue,
        };

        let is_dir = path.is_dir();

        if !is_dir {
            total_files.fetch_add(1, Ordering::Relaxed);
        }

        if pattern.matches(&file_name) {
            matched_count.fetch_add(1, Ordering::Relaxed);
            let _ = output_tx.send(path.clone());
        }

        if is_dir {
            if ignore_dirs.contains(file_name.as_ref()) {
                continue;
            }

            if max_depth.map_or(true, |d| task.depth < d) {
                active_tasks.fetch_add(1, Ordering::SeqCst);

                let _ = task_tx.send(Task {
                    path,
                    depth: task.depth + 1,
                });
            }
        }
    }
}
