use std::{
    collections::HashSet,
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::Instant,
};

use clap::Parser;
use colored::Colorize;
use crossbeam_channel::unbounded;

/// Default directories to ignore during text search
const DEFAULT_IGNORES: &[&str] = &[".git", "node_modules", "__pycache__", "target"];

/// CLI arguments for argrep
#[derive(Parser, Debug)]
#[command(author, version, about = "Fast parallel text search utility (Rust version)")]
struct Args {
    /// The text query/pattern to search for
    #[arg(required = true)]
    query: String,

    /// Root directory to start the search [default: .]
    #[arg(default_value = ".")]
    path: String,

    /// Case-insensitive search
    #[arg(short = 'i', long)]
    ignore_case: bool,

    /// Display line numbers in the output results
    #[arg(short = 'n', long)]
    line_number: bool,

    /// Number of worker threads
    #[arg(short = 'j', long, default_value_t = 4)]
    jobs: usize,

    /// Show search statistics and operational errors
    #[arg(short, long)]
    debug: bool,
}

/// shared runtime configuration
struct SearchConfig {
    query: String,
    normalized_query: String,
    ignore_case: bool,
    line_number: bool,
    ignore_dirs: HashSet<String>,
    debug: bool,
}

/// Shared statistics counters
#[derive(Clone)]
struct SearchStats {
    total_files: Arc<AtomicUsize>,
    total_dirs: Arc<AtomicUsize>,
    matched_lines: Arc<AtomicUsize>,
}

/// Result payload containing a single text match
struct MatchResult {
    file_path: PathBuf,
    line_num: usize,
    line_content: String,
}

fn main() {
    let args = Args::parse();

    let root_path = fs::canonicalize(&args.path)
        .unwrap_or_else(|_| PathBuf::from(&args.path));

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();

    let normalized_query = if args.ignore_case {
        args.query.to_lowercase()
    } else {
        args.query.clone()
    };

    let config = Arc::new(SearchConfig {
        query: args.query,
        normalized_query,
        ignore_case: args.ignore_case,
        line_number: args.line_number,
        ignore_dirs,
        debug: args.debug,
    });

    let stats = SearchStats {
        total_files: Arc::new(AtomicUsize::new(0)),
        total_dirs: Arc::new(AtomicUsize::new(0)),
        matched_lines: Arc::new(AtomicUsize::new(0)),
    };

    let start_time = Instant::now();

    parallel_grep(root_path, args.jobs, config.clone(), stats.clone());

    let duration = start_time.elapsed();

    if args.debug {
        eprintln!("{}", "\n=== Search Statistics ===".yellow().bold());
        eprintln!("Directories checked: {}", stats.total_dirs.load(Ordering::Relaxed).to_string().cyan());
        eprintln!("Files scanned:       {}", stats.total_files.load(Ordering::Relaxed).to_string().cyan());
        eprintln!("Total text matches:  {}", stats.matched_lines.load(Ordering::Relaxed).to_string().green().bold());
        eprintln!("Execution time:      {:.2?}", duration);
    }
}

fn parallel_grep(root: PathBuf, workers: usize, config: Arc<SearchConfig>, stats: SearchStats) {
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

                scan_and_grep(&task_path, &config, &task_tx, &output_tx, &active_tasks, &stats);
                active_tasks.fetch_sub(1, Ordering::SeqCst);
            }
        });

        handles.push(handle);
    }

    drop(task_tx);
    drop(output_tx);

    // Dedicated output processing loop (runs concurrently on the coordinator thread)
    for result in output_rx {
        let prefix = if config.line_number {
            format!("{}:{}", result.file_path.display().to_string().magenta(), result.line_num.to_string().green())
        } else {
            result.file_path.display().to_string().magenta().to_string()
        };

        // Simple color highlight for the query keyword inside the line content
        let highlighted = result.line_content.replace(&config.query, &config.query.red().bold().to_string());
        println!("{}: {}", prefix, highlighted.trim_end());
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn scan_and_grep(
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

fn grep_file(
    file_path: &Path,
    config: &SearchConfig,
    output_tx: &crossbeam_channel::Sender<MatchResult>,
    stats: &SearchStats,
) {
    let file = match File::open(file_path) {
        Ok(f) => f,
        Err(_) => return, // Silently ignore unopenable files
    };

    let mut reader = BufReader::new(file);

    // Fast binary file sniffing: check the first 1024 bytes for a null byte byte flag
    let mut sniffer_buffer = [0u8; 1024];
    if let Ok(bytes_read) = reader.read(&mut sniffer_buffer) {
        if sniffer_buffer[..bytes_read].contains(&0u8) {
            return; // Skip compiled binaries or media files
        }

        // Seek back to the beginning of the file internally by resetting the reader buffer logic
        let file = match File::open(file_path) {
            Ok(f) => f,
            Err(_) => return,
        };
        reader = BufReader::new(file);
    }

    // Process file line by line using memory-efficient buffer stream strings
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
        line.clear(); // Clear the allocation buffer to reuse heap memory efficiently
    }
}
