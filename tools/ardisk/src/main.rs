use ardisk::{DEFAULT_IGNORES, aggregate_sizes, build_config, format_size, parallel_scan};
use clap::Parser;
use clap::builder::TypedValueParser as _;
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

/// CPU-aware default worker count, used as the -j/--jobs default.
///
/// Half of available_parallelism(), clamped to [1, 16]: using every core by
/// default competes with the rest of the system (and with the other
/// worker-based tools in this repo if run concurrently), while an unclamped
/// value could default to an unreasonably high thread count on large
/// build/CI machines. available_parallelism() failing (sandboxed or
/// restricted environments) falls back to 1, which the clamp still turns
/// into a valid default.
fn default_jobs() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    (cpus / 2).clamp(1, 16)
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
    #[arg(
        short = 'j',
        long,
        default_value_t = default_jobs(),
        value_parser = clap::value_parser!(u16).range(1..).map(|v| v as usize)
    )]
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

    /// Use logical file sizes instead of physical block allocation.
    /// Matches the output of du -sh on macOS and Linux.
    #[arg(long)]
    apparent_size: bool,

    /// Additional ignored directories
    #[arg(long)]
    ignore: Vec<String>,

    /// Do not respect .gitignore / .ignore files (scan everything)
    #[arg(long = "no-ignore")]
    no_ignore: bool,
}

/// Builds the set of directory names to skip, given `--no-ignore` and any
/// explicit `--ignore` values. `--no-ignore` disables the built-in defaults
/// (`.git`, `node_modules`, `__pycache__`), but an explicit `--ignore` is
/// still honored either way, since that's the user asking for something
/// specific rather than the tool's automatic noise filtering.
fn build_ignore_dirs(no_ignore: bool, extra: Vec<String>) -> HashSet<String> {
    let mut ignore_dirs: HashSet<String> = if no_ignore {
        HashSet::new()
    } else {
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect()
    };
    ignore_dirs.extend(extra);
    ignore_dirs
}

fn main() {
    let args = Args::parse();

    let ignore_dirs = build_ignore_dirs(args.no_ignore, args.ignore);
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

    let config = build_config(
        ignore_dirs,
        include_pattern,
        args.debug,
        args.apparent_size,
        !args.no_ignore,
    );

    let start_time = Instant::now();

    // Phase 1: Parallel file scanning
    let (raw_sizes, raw_content_sizes) = parallel_scan(target_path.clone(), args.jobs, config);

    // Phase 2: Aggregation and rollup from bottom to top
    let aggregated_sizes = aggregate_sizes(&raw_sizes, &target_path);
    let aggregated_content = aggregate_sizes(&raw_content_sizes, &target_path);

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

    if args.debug {
        println!("{}", "=== Top Directories ===".yellow().bold());
    }

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

            // Suppress directories with no matching file content when
            // --include is active — they only add noise to the output.
            // We check the content map (file bytes only, no inode cost)
            // so that directory inode costs don't defeat the suppression.
            if threshold_bytes.is_none() && args.include.is_some() {
                let content = aggregated_content.get(*path).copied().unwrap_or(0);
                if content == 0 {
                    continue;
                }
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
        eprintln!("Worker threads:        {}", args.jobs);
        eprintln!("Total scanned folders: {}", raw_sizes.len());
        eprintln!("Execution time:        {:.2?}", duration);
    }
}

#[cfg(test)]
mod tests {
    use super::{Args, build_ignore_dirs, default_jobs, parse_threshold};
    use clap::Parser;

    // ── -j / --jobs boundary ─────────────────────────────────────────────────
    // `0` workers means the task queue is never drained, silently producing
    // an empty (wrong) result with exit code 0 — worse than a crash for
    // automation. This must be rejected at CLI parse time, not left to the
    // worker pool to (fail to) handle.

    #[test]
    fn jobs_zero_is_rejected_at_parse_time() {
        let result = Args::try_parse_from(["ardisk", ".", "-j", "0"]);
        assert!(result.is_err(), "-j 0 must be a CLI parse error");
    }

    #[test]
    fn jobs_one_is_accepted() {
        let args = Args::try_parse_from(["ardisk", ".", "-j", "1"]).unwrap();
        assert_eq!(args.jobs, 1);
    }

    #[test]
    fn jobs_two_is_accepted() {
        let args = Args::try_parse_from(["ardisk", ".", "-j", "2"]).unwrap();
        assert_eq!(args.jobs, 2);
    }

    #[test]
    fn default_jobs_matches_half_available_parallelism_clamped() {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let expected = (cpus / 2).clamp(1, 16);
        assert_eq!(default_jobs(), expected);
    }

    #[test]
    fn jobs_default_is_cpu_aware() {
        let args = Args::try_parse_from(["ardisk", "."]).unwrap();
        assert_eq!(
            args.jobs,
            default_jobs(),
            "default -j must match the CPU-aware formula, not a hardcoded value"
        );
        assert!(
            (1..=16).contains(&args.jobs),
            "default -j must stay within the clamped [1, 16] range regardless \
             of how many cores the machine reports: got {}",
            args.jobs
        );
    }

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

    // ── build_ignore_dirs ────────────────────────────────────────────────────

    #[test]
    fn default_ignores_included_when_not_no_ignore() {
        let dirs = build_ignore_dirs(false, vec![]);
        assert!(dirs.contains(".git"));
        assert!(dirs.contains("node_modules"));
        assert!(dirs.contains("__pycache__"));
    }

    #[test]
    fn no_ignore_excludes_default_ignores() {
        let dirs = build_ignore_dirs(true, vec![]);
        assert!(dirs.is_empty());
    }

    #[test]
    fn explicit_ignore_honored_alongside_defaults() {
        let dirs = build_ignore_dirs(false, vec!["vendor".to_string()]);
        assert!(dirs.contains("vendor"));
        assert!(dirs.contains(".git"));
    }

    #[test]
    fn explicit_ignore_honored_even_with_no_ignore() {
        let dirs = build_ignore_dirs(true, vec!["vendor".to_string()]);
        assert_eq!(dirs.len(), 1);
        assert!(dirs.contains("vendor"));
    }
}
