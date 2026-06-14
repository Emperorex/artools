use argrep::{DEFAULT_IGNORES, SearchStats, build_config, parallel_grep};
use clap::Parser;
use colored::Colorize;
use std::{collections::HashSet, fs, path::PathBuf, sync::atomic::Ordering, time::Instant};

/// CLI arguments for argrep
#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Fast parallel text search utility (Rust version)"
)]
struct Args {
    /// The text query/pattern to search for
    #[arg(required = true)]
    query: String,

    /// Root directory to start the search
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

fn main() {
    let args = Args::parse();

    let root_path = fs::canonicalize(&args.path).unwrap_or_else(|_| PathBuf::from(&args.path));
    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();

    let config = build_config(
        args.query,
        args.ignore_case,
        args.line_number,
        ignore_dirs,
        args.debug,
    );

    let stats = SearchStats::new();
    let start_time = Instant::now();

    let line_number = config.line_number;
    let query = config.query.clone();

    parallel_grep(root_path, args.jobs, config, stats.clone(), move |result| {
        let prefix = if line_number {
            format!(
                "{}:{}",
                result.file_path.display().to_string().magenta(),
                result.line_num.to_string().green()
            )
        } else {
            result.file_path.display().to_string().magenta().to_string()
        };

        // Simple color highlight for the query keyword inside the line content
        let highlighted = result
            .line_content
            .replace(&query, &query.red().bold().to_string());
        println!("{}: {}", prefix, highlighted.trim_end());
    });

    let duration = start_time.elapsed();

    if args.debug {
        eprintln!("{}", "\n=== Search Statistics ===".yellow().bold());
        eprintln!(
            "Directories checked: {}",
            stats.total_dirs.load(Ordering::Relaxed).to_string().cyan()
        );
        eprintln!(
            "Files scanned:       {}",
            stats.total_files.load(Ordering::Relaxed).to_string().cyan()
        );
        eprintln!(
            "Total text matches:  {}",
            stats
                .matched_lines
                .load(Ordering::Relaxed)
                .to_string()
                .green()
                .bold()
        );
        eprintln!("Execution time:      {:.2?}", duration);
    }
}
