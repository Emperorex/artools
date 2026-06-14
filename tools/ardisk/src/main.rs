use ardisk::{DEFAULT_IGNORES, aggregate_sizes, build_config, format_size, parallel_scan};
use clap::Parser;
use colored::Colorize;
use std::{collections::HashSet, fs, path::PathBuf, time::Instant};

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

    /// Number of top directories to display in the report
    #[arg(short = 'n', long, default_value_t = 20)]
    top: usize,
}

fn main() {
    let args = Args::parse();

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let target_path = fs::canonicalize(&args.path).unwrap_or_else(|_| PathBuf::from(&args.path));
    let config = build_config(ignore_dirs, args.debug);

    let start_time = Instant::now();

    // Phase 1: Parallel file scanning
    let raw_sizes = parallel_scan(target_path.clone(), args.jobs, config);

    // Phase 2: Aggregation and rollup from bottom to top
    let aggregated_sizes = aggregate_sizes(&raw_sizes, &target_path);

    let duration = start_time.elapsed();

    // Sort results by size descending and print top folders
    let mut sorted_results: Vec<(&PathBuf, &u64)> = aggregated_sizes.iter().collect();
    sorted_results.sort_by(|a, b| b.1.cmp(a.1));

    println!("\n{}", "=== Top Directories ===".yellow().bold());

    let mut printed_count = 0;
    for (path, size) in sorted_results.iter() {
        if printed_count >= args.top {
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
        eprintln!("Total scanned folders: {}", raw_sizes.len());
        eprintln!("Execution time:        {:.2?}", duration);
    }
}
