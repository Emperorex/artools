use ardisk::{
    Collect, DEFAULT_IGNORES, ScanOutput, aggregate_sizes, build_config_with_exclude,
    build_exclude_matcher,
    duplicates::{DuplicateReport, find_duplicates},
    format_size, parallel_scan_report,
    report::{
        DirectoryQuery, DirectorySummary, Filters, ReportMeta, select_directories,
        write_directories, write_duplicates, write_largest_files,
    },
};
use clap::Parser;
use clap::builder::TypedValueParser as _;
use colored::Colorize;
use glob::Pattern;
use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
    path::PathBuf,
    time::Instant,
};

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

    if !value.is_finite() {
        return Err(format!(
            "Threshold must be a finite number, got '{}' in '{}'.",
            num_part, s
        ));
    }

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

    let bytes = value * multiplier;

    if !bytes.is_finite() || bytes > u64::MAX as f64 {
        return Err(format!("Threshold is too large in '{}'.", s));
    }

    Ok(bytes as u64)
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

/// Hard upper bound for -j/--jobs.
///
/// `jobs` maps 1:1 to raw `thread::spawn` calls (see the worker loop below),
/// so an unbounded value lets a caller trivially exhaust threads/PIDs/memory
/// on the host (e.g. `-j 65535`). 128 comfortably covers even large CI/build
/// machines while keeping a hostile or accidental value from taking down the
/// process or the machine it runs on.
const MAX_JOBS: u16 = 128;

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
        value_parser = clap::value_parser!(u16).range(1..=i64::from(MAX_JOBS)).map(|v| v as usize)
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

    /// List the N largest individual files instead of the directory report.
    /// Respects --include, --exclude, --ignore and .gitignore; hard-linked
    /// files are listed once; sizes follow --apparent-size.
    #[arg(
        long,
        value_name = "N",
        value_parser = clap::value_parser!(u64).range(1..).map(|v| v as usize),
        conflicts_with_all = ["summarize", "top", "max_depth", "threshold"]
    )]
    largest_files: Option<usize>,

    /// Report groups of files with identical content (nothing is modified or
    /// deleted). Files are grouped by length, then by a hash of their first
    /// 4 KiB, then by a hash of their full content. Respects --include,
    /// --exclude, --ignore and .gitignore; hard links to one file are not
    /// duplicates; empty files are ignored. --top limits the groups shown,
    /// --summarize prints only the totals. Sizes are logical file lengths.
    #[arg(
        long,
        conflicts_with_all = ["largest_files", "max_depth", "threshold"]
    )]
    duplicates: bool,

    /// With --duplicates: ignore files smaller than this (e.g. 1MB, 100KB).
    /// Raising it is the most effective way to cut memory use and run time
    /// on large trees. Default: 1B (only empty files are skipped).
    #[arg(
        long,
        value_name = "SIZE",
        requires = "duplicates",
        value_parser = parse_threshold
    )]
    min_size: Option<u64>,

    /// Print the result as one JSON document on stdout instead of text.
    /// Works with every mode (directory report, --largest-files,
    /// --duplicates) and does not change what is analysed. Only the JSON
    /// goes to stdout; warnings and diagnostics go to stderr. Paths in the
    /// document are relative to the scanned root; see the README for the
    /// schema.
    #[arg(long)]
    json: bool,

    /// Use logical file sizes instead of physical block allocation.
    /// Matches the output of du -sh on macOS and Linux.
    #[arg(long)]
    apparent_size: bool,

    /// Additional ignored directories
    #[arg(long)]
    ignore: Vec<String>,

    /// Exclude files and directories matching a gitignore-style glob
    /// (repeatable), e.g. --exclude '*.log' --exclude 'target/**'.
    /// A pattern without '/' matches at any depth; with '/' it is anchored
    /// to PATH; a trailing '/' matches directories only; 'dir/**' excludes
    /// the contents of dir but not dir itself. Quote patterns to keep the
    /// shell from expanding them.
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,

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

    let ignore_dirs = build_ignore_dirs(args.no_ignore, args.ignore.clone());
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

    let exclude = match build_exclude_matcher(&target_path, &args.exclude) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{}", format!("error: {}", e).red());
            std::process::exit(1);
        }
    };

    let config = build_config_with_exclude(
        ignore_dirs,
        include_pattern,
        args.debug,
        args.apparent_size,
        !args.no_ignore,
        exclude,
    );

    let start_time = Instant::now();

    // Phase 1: Parallel file scanning
    // Empty files are never duplicates-of-interest, so the floor is 1 byte.
    let min_len = args.min_size.unwrap_or(1).max(1);
    let collect = if args.duplicates {
        Collect::Duplicates { min_len }
    } else {
        match args.largest_files {
            Some(n) => Collect::Largest(n),
            None => Collect::Nothing,
        }
    };
    let ScanOutput {
        raw_sizes,
        content_sizes: raw_content_sizes,
        files: collected_files,
        file_count,
    } = parallel_scan_report(target_path.clone(), args.jobs, config, collect);

    // Phase 1b (--duplicates only): narrow the candidates down to groups of
    // identical files. This is where almost all of the time goes.
    // The candidate list is moved, not copied: on a big tree it is the
    // largest allocation of the whole run.
    let (largest_files, duplicate_report) = if args.duplicates {
        (
            Vec::new(),
            Some(find_duplicates(collected_files, args.jobs)),
        )
    } else {
        (collected_files, None)
    };

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

    let root_total = aggregated_sizes.get(&target_path).copied().unwrap_or(0);
    let query = DirectoryQuery {
        top: args.top,
        max_depth: args.max_depth,
        threshold_bytes,
        include_active: args.include.is_some(),
        summarize: args.summarize,
    };

    if args.json {
        let meta = ReportMeta {
            root: target_path.clone(),
            apparent_size: args.apparent_size,
            filters: Filters {
                include: args.include.clone(),
                exclude: args.exclude.clone(),
                ignore: args.ignore.clone(),
                no_ignore: args.no_ignore,
            },
        };
        let stdout = io::stdout();
        let mut out = io::BufWriter::new(stdout.lock());
        let written = if let Some(report) = &duplicate_report {
            warn_unreadable(report, args.debug);
            write_duplicates(&mut out, &meta, args.top, args.summarize, min_len, report)
        } else if let Some(limit) = args.largest_files {
            write_largest_files(
                &mut out,
                &meta,
                limit,
                root_total,
                file_count,
                &largest_files,
            )
        } else {
            let selection =
                select_directories(&aggregated_sizes, &aggregated_content, &target_path, &query);
            let summary = DirectorySummary {
                total_bytes: root_total,
                files: file_count,
                directories: raw_sizes.len() as u64,
            };
            write_directories(&mut out, &meta, &query, &selection, summary)
        };
        match written.and_then(|lossy| out.flush().map(|()| lossy)) {
            Ok(0) => {}
            Ok(lossy) => eprintln!(
                "{}",
                format!(
                    "ardisk: {} path(s) are not valid UTF-8; invalid bytes were replaced by U+FFFD in the JSON",
                    lossy
                )
                .yellow()
            ),
            // The reader (e.g. `head`) went away; that is not an error.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
            Err(e) => {
                eprintln!("{}", format!("error: cannot write JSON: {}", e).red());
                std::process::exit(1);
            }
        }
    } else {
        if args.debug {
            let title = if args.duplicates {
                "=== Duplicate Files ==="
            } else if args.largest_files.is_some() {
                "=== Largest Files ==="
            } else {
                "=== Top Directories ==="
            };
            println!("{}", title.yellow().bold());
        }

        if let Some(report) = &duplicate_report {
            print_duplicates(report, args.top, args.summarize);
            warn_unreadable(report, args.debug);
        } else if args.largest_files.is_some() {
            // --largest-files: individual files, largest first, instead of
            // the per-directory report.
            for file in &largest_files {
                println!("{:>10}  {}", format_size(file.size), file.path.display());
            }
        } else if args.summarize {
            // --summarize: print only the root total and exit
            println!("{:>10}  {}", format_size(root_total), target_path.display());
        } else {
            let selection =
                select_directories(&aggregated_sizes, &aggregated_content, &target_path, &query);
            for (path, size) in &selection.entries {
                println!("{:>10}  {}", format_size(*size), path.display());
            }
        }
    }

    if args.debug {
        eprintln!("\n{}", "=== Operational Metrics ===".green().bold());
        eprintln!("Worker threads:        {}", args.jobs);
        eprintln!("Total scanned folders: {}", raw_sizes.len());
        eprintln!("Execution time:        {:.2?}", duration);
        if let Some(report) = &duplicate_report {
            let st = &report.stats;
            eprintln!("Duplicate candidates:  {}", st.candidates);
            eprintln!("  same length:         {}", st.same_length);
            eprintln!("  prefix-hashed:       {}", st.prefix_hashed);
            eprintln!("  fully hashed:        {}", st.full_hashed);
            eprintln!("  bytes read:          {}", format_size(st.bytes_read));
        }
    }
}

/// "s" unless `n` is exactly one.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Prints the `--duplicates` report: the `top` groups with the most
/// reclaimable space (or only the totals with `--summarize`), then a summary.
fn print_duplicates(report: &DuplicateReport, top: usize, summarize: bool) {
    if !summarize {
        for group in report.groups.iter().take(top) {
            println!(
                "{} identical files, {} each, {} reclaimable",
                group.paths.len(),
                format_size(group.len),
                format_size(group.reclaimable())
            );
            for path in &group.paths {
                println!("  {}", path.display());
            }
            println!();
        }
    }

    let shown = if summarize {
        0
    } else {
        report.groups.len().min(top)
    };
    let more = if summarize || shown == report.groups.len() {
        String::new()
    } else {
        format!(" (showing the top {} group{})", shown, plural(shown))
    };
    let (groups, redundant) = (report.groups.len(), report.redundant_files());
    println!(
        "{} duplicate group{}, {} redundant file{}, {} reclaimable{}",
        groups,
        plural(groups),
        redundant,
        plural(redundant),
        format_size(report.reclaimable()),
        more
    );
}

/// Warns on stderr about files the duplicate search had to skip.
fn warn_unreadable(report: &DuplicateReport, debug: bool) {
    if !report.unreadable.is_empty() {
        eprintln!(
            "{}",
            format!(
                "ardisk: skipped {} file(s) that could not be read{}",
                report.unreadable.len(),
                if debug {
                    ":"
                } else {
                    " (use --debug to list them)"
                }
            )
            .yellow()
        );
        if debug {
            for (path, reason) in &report.unreadable {
                eprintln!("  {}: {}", path.display(), reason);
            }
        }
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

    // ── --exclude ─────────────────────────────────────────────────────────

    #[test]
    fn exclude_is_repeatable() {
        let args = Args::try_parse_from([
            "ardisk",
            ".",
            "--exclude",
            "*.log",
            "--exclude",
            "target/**",
        ])
        .unwrap();
        assert_eq!(args.exclude, vec!["*.log", "target/**"]);
    }

    // ── --json ────────────────────────────────────────────────────────────

    #[test]
    fn json_defaults_to_off() {
        let args = Args::try_parse_from(["ardisk", "."]).unwrap();
        assert!(!args.json);
    }

    #[test]
    fn json_is_accepted_with_every_mode() {
        for extra in [
            vec![],
            vec!["--largest-files", "5"],
            vec!["--duplicates"],
            vec!["--duplicates", "--min-size", "1MB", "--top", "3"],
            vec!["--summarize"],
            vec!["--max-depth", "2", "--threshold", "1MB"],
            vec![
                "--exclude",
                "target/**",
                "--include",
                "*.rs",
                "--apparent-size",
            ],
        ] {
            let mut argv = vec!["ardisk", ".", "--json"];
            argv.extend(extra.iter());
            let args = Args::try_parse_from(argv.clone())
                .unwrap_or_else(|e| panic!("{argv:?} should parse: {e}"));
            assert!(args.json);
        }
    }

    #[test]
    fn json_does_not_enable_any_mode_by_itself() {
        let args = Args::try_parse_from(["ardisk", ".", "--json"]).unwrap();
        assert!(!args.duplicates);
        assert_eq!(args.largest_files, None);
    }

    // ── --duplicates ──────────────────────────────────────────────────────

    #[test]
    fn duplicates_flag_parses() {
        let args = Args::try_parse_from(["ardisk", ".", "--duplicates"]).unwrap();
        assert!(args.duplicates);
        assert_eq!(args.min_size, None);
    }

    #[test]
    fn duplicates_defaults_to_off() {
        let args = Args::try_parse_from(["ardisk", "."]).unwrap();
        assert!(!args.duplicates);
    }

    #[test]
    fn min_size_requires_duplicates() {
        assert!(Args::try_parse_from(["ardisk", ".", "--min-size", "1MB"]).is_err());
    }

    #[test]
    fn min_size_is_parsed_like_threshold() {
        let args =
            Args::try_parse_from(["ardisk", ".", "--duplicates", "--min-size", "1MB"]).unwrap();
        assert_eq!(args.min_size, Some(parse_threshold("1MB").unwrap()));
    }

    #[test]
    fn min_size_rejects_garbage() {
        assert!(
            Args::try_parse_from(["ardisk", ".", "--duplicates", "--min-size", "lots"]).is_err()
        );
    }

    #[test]
    fn duplicates_conflicts_with_other_report_modes() {
        for extra in [
            vec!["--largest-files", "3"],
            vec!["--max-depth", "2"],
            vec!["--threshold", "1MB"],
        ] {
            let mut argv = vec!["ardisk", ".", "--duplicates"];
            argv.extend(extra.iter());
            assert!(
                Args::try_parse_from(argv).is_err(),
                "--duplicates must conflict with {:?}",
                extra
            );
        }
    }

    #[test]
    fn duplicates_combines_with_top_summarize_include_and_exclude() {
        let args = Args::try_parse_from([
            "ardisk",
            ".",
            "--duplicates",
            "--top",
            "5",
            "--summarize",
            "--include",
            "*.jpg",
            "--exclude",
            "tmp/**",
            "-j",
            "2",
        ])
        .unwrap();
        assert!(args.duplicates && args.summarize);
        assert_eq!(args.top, 5);
    }

    // ── --largest-files ───────────────────────────────────────────────────

    #[test]
    fn largest_files_parses_a_positive_count() {
        let args = Args::try_parse_from(["ardisk", ".", "--largest-files", "20"]).unwrap();
        assert_eq!(args.largest_files, Some(20));
    }

    #[test]
    fn largest_files_defaults_to_none() {
        let args = Args::try_parse_from(["ardisk", "."]).unwrap();
        assert_eq!(args.largest_files, None);
    }

    #[test]
    fn largest_files_zero_is_rejected() {
        assert!(Args::try_parse_from(["ardisk", ".", "--largest-files", "0"]).is_err());
    }

    #[test]
    fn largest_files_rejects_non_numeric_and_negative() {
        assert!(Args::try_parse_from(["ardisk", ".", "--largest-files", "x"]).is_err());
        assert!(Args::try_parse_from(["ardisk", ".", "--largest-files", "-3"]).is_err());
    }

    #[test]
    fn largest_files_conflicts_with_directory_report_flags() {
        for extra in [
            vec!["--summarize"],
            vec!["--top", "5"],
            vec!["--max-depth", "2"],
            vec!["--threshold", "1MB"],
        ] {
            let mut argv = vec!["ardisk", ".", "--largest-files", "3"];
            argv.extend(extra.iter());
            assert!(
                Args::try_parse_from(argv).is_err(),
                "--largest-files must conflict with {:?}",
                extra
            );
        }
    }

    #[test]
    fn largest_files_combines_with_include_exclude_and_apparent_size() {
        let args = Args::try_parse_from([
            "ardisk",
            ".",
            "--largest-files",
            "5",
            "--include",
            "*.mp4",
            "--exclude",
            "tmp/**",
            "--apparent-size",
        ])
        .unwrap();
        assert_eq!(args.largest_files, Some(5));
    }

    #[test]
    fn exclude_defaults_to_empty() {
        let args = Args::try_parse_from(["ardisk", "."]).unwrap();
        assert!(args.exclude.is_empty());
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
