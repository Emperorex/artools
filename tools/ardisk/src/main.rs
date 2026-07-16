use ardisk::{DEFAULT_IGNORES, aggregate_sizes, build_config, format_size, parallel_scan};
use clap::Parser;
use colored::Colorize;
use glob::Pattern;
use std::{collections::HashSet, fs, path::PathBuf, time::Instant};

/// Parses a human-readable size string into bytes.
/// Supported suffixes: B, KB, MB, GB, TB (case-insensitive).
/// Examples: "500B", "100KB", "10MB", "2GB", "1TB"
fn parse_threshold(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num_part, suffix) = s
        .find(|c: char| c.is_alphabetic())
        .map(|i| s.split_at(i))
        .ok_or_else(|| format!("Missing unit suffix in '{}'. Use B, KB, MB, GB, or TB.", s))?;

    let value: f64 = num_part
        .parse()
        .map_err(|_| format!("Invalid number '{}' in threshold '{}'.", num_part, s))?;

    if value < 0.0 {
        return Err(format!("Threshold must be a positive value, got '{}'.", s));
    }

    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;

    let multiplier = match suffix.to_uppercase().as_str() {
        "B" => 1.0,
        "KB" => KB,
        "MB" => MB,
        "GB" => GB,
        "TB" => TB,
        other => {
            return Err(format!(
                "Unknown unit '{}'. Use B, KB, MB, GB, or TB.",
                other
            ));
        }
    };

    Ok((value * multiplier) as u64)
}

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

    /// Only count files matching this glob pattern (e.g. "*.rs", "*.mp4")
    #[arg(long)]
    include: Option<String>,

    /// Only show directories larger than this size (e.g. 100KB, 10MB, 1GB)
    #[arg(long)]
    threshold: Option<String>,

    /// Print only the grand total for the root directory
    #[arg(short = 's', long)]
    summarize: bool,
}

fn main() {
    let args = Args::parse();

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let target_path = fs::canonicalize(&args.path).unwrap_or_else(|_| PathBuf::from(&args.path));

    let include_pattern: Option<Pattern> = match &args.include {
        Some(p) => match Pattern::new(p) {
            Ok(pat) => Some(pat),
            Err(e) => {
                eprintln!(
                    "{}",
                    format!("error: Invalid glob pattern \'{}\': {}", p, e).red()
                );
                std::process::exit(1);
            }
        },
        None => None,
    };

    let config = build_config(ignore_dirs, include_pattern, args.debug);

    let start_time = Instant::now();

    // Phase 1: Parallel file scanning
    let raw_sizes = parallel_scan(target_path.clone(), args.jobs, config);

    // Phase 2: Aggregation and rollup from bottom to top
    let aggregated_sizes = aggregate_sizes(&raw_sizes, &target_path);

    let duration = start_time.elapsed();

    // Parse --threshold if provided, exit early on invalid input
    let threshold_bytes: Option<u64> = match &args.threshold {
        Some(t) => match parse_threshold(t) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                eprintln!("{}", format!("error: {}", e).red());
                std::process::exit(1);
            }
        },
        None => None,
    };

    println!("\n{}", "=== Top Directories ===".yellow().bold());

    // --summarize: print only the root total and exit
    if args.summarize {
        let root_size = aggregated_sizes.get(&target_path).copied().unwrap_or(0);
        println!("{:>10}  {}", format_size(root_size), target_path.display());
    } else {
        // Sort results by size descending and print top folders
        let mut sorted_results: Vec<(&PathBuf, &u64)> = aggregated_sizes.iter().collect();
        sorted_results.sort_by(|a, b| b.1.cmp(a.1));

        let mut printed_count = 0;
        for (path, size) in sorted_results.iter() {
            if printed_count >= args.top {
                break;
            }

            // Suppress zero-size directories when --include is active —
            // they contain no matching files and only add noise to the output
            if threshold_bytes.is_none() && args.include.is_some() && **size == 0 {
                continue;
            }

            // Apply --threshold filter
            if threshold_bytes.is_some_and(|min| **size < min) {
                continue;
            }

            if let Ok(rel_path) = path.strip_prefix(&target_path) {
                let current_depth = rel_path.components().count();
                if args.max_depth.is_none_or(|max_d| current_depth <= max_d) {
                    println!("{:>10}  {}", format_size(**size), path.display());
                    printed_count += 1;
                }
            }
        }
    }

    if args.debug {
        eprintln!("\n{}", "=== Operational Metrics ===".green().bold());
        eprintln!("Total scanned folders: {}", raw_sizes.len());
        eprintln!("Execution time:        {:.2?}", duration);
    }
}

#[cfg(test)]
mod tests {
    use super::parse_threshold;

    // ── Valid inputs ──────────────────────────────────────────────────────────

    #[test]
    fn parse_bytes() {
        assert_eq!(parse_threshold("500B").unwrap(), 500);
    }

    #[test]
    fn parse_kilobytes() {
        assert_eq!(parse_threshold("1KB").unwrap(), 1024);
    }

    #[test]
    fn parse_megabytes() {
        assert_eq!(parse_threshold("10MB").unwrap(), 10 * 1024 * 1024);
    }

    #[test]
    fn parse_gigabytes() {
        assert_eq!(parse_threshold("2GB").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_terabytes() {
        assert_eq!(parse_threshold("1TB").unwrap(), 1024_u64.pow(4));
    }

    #[test]
    fn parse_case_insensitive() {
        assert_eq!(parse_threshold("10mb").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_threshold("10Mb").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_threshold("10MB").unwrap(), 10 * 1024 * 1024);
    }

    #[test]
    fn parse_fractional_value() {
        // 0.5 GB = 512 MB
        assert_eq!(parse_threshold("0.5GB").unwrap(), 512 * 1024 * 1024);
    }

    #[test]
    fn parse_trims_whitespace() {
        assert_eq!(parse_threshold("  10MB  ").unwrap(), 10 * 1024 * 1024);
    }

    // ── Invalid inputs ────────────────────────────────────────────────────────

    #[test]
    fn parse_missing_suffix_returns_error() {
        assert!(parse_threshold("1024").is_err());
    }

    #[test]
    fn parse_unknown_unit_returns_error() {
        assert!(parse_threshold("10PB").is_err());
    }

    #[test]
    fn parse_non_numeric_value_returns_error() {
        assert!(parse_threshold("tenMB").is_err());
    }

    #[test]
    fn parse_negative_value_returns_error() {
        assert!(parse_threshold("-10MB").is_err());
    }

    #[test]
    fn parse_empty_string_returns_error() {
        assert!(parse_threshold("").is_err());
    }
}
