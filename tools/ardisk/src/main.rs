use arcommon::jobs::{MAX_JOBS, default_jobs};
use ardisk::{
    Collect, DEFAULT_IGNORES, ScanConfig, ScanOutput, SizeUnits,
    age::{Age, MtimeFilter, check_window, parse_age},
    aggregate_sizes,
    bar::render_list_with_bars,
    build_exclude_matcher,
    duplicates::{DuplicateReport, find_duplicates},
    format_size, format_size_plain, paint_size, parallel_scan_report,
    progress::{Reporter, ScanProgress, Status, group_digits},
    report::{
        DirectoryQuery, DirectorySummary, Filters, InodeSummary, ReportMeta, render_by_type_text,
        select_directories, top_limit, write_by_type, write_directories, write_duplicates,
        write_inodes, write_largest_files,
    },
    tree::{render_tree_text, tree_lines},
};
use clap::Parser;
use clap::builder::TypedValueParser as _;
use colored::Colorize;
use glob::Pattern;
use std::{
    collections::HashSet,
    fs,
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::Arc,
    time::{Instant, SystemTime},
};

/// Parses `--threshold` under `--inodes`: a plain number of inodes.
fn parse_count(s: &str) -> Result<u64, String> {
    s.trim().parse::<u64>().map_err(|_| {
        format!(
            "Invalid threshold '{}'. With --inodes use a whole number, e.g. 10000.",
            s.trim()
        )
    })
}

/// Parses a human-readable size string into bytes in `units`: with the
/// default binary units `1KB` is 1024 bytes, with `--si` it is 1000.
/// Supported suffixes: B, KB, MB, GB, TB (case-insensitive).
/// Examples: "500B", "100KB", "10MB", "2GB", "1TB"
fn parse_size_in(s: &str, units: SizeUnits) -> Result<u64, String> {
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

    let kb = units.base() as f64;
    let multiplier = match suffix.to_uppercase().as_str() {
        "B" => 1.0,
        "KB" => kb,
        "MB" => kb * kb,
        "GB" => kb * kb * kb,
        "TB" => kb * kb * kb * kb,
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

/// Parses a size in the default binary units (see [`parse_size_in`]).
fn parse_threshold(s: &str) -> Result<u64, String> {
    parse_size_in(s, SizeUnits::Binary)
}

/// A size as typed on the command line (`--min-size 1.5GB`). Checked when
/// parsed, turned into bytes only once `--si` is known, since the same
/// text means a different number of bytes in binary and SI units.
#[derive(Debug, Clone, PartialEq)]
struct SizeArg(String);

impl SizeArg {
    fn parse(s: &str) -> Result<SizeArg, String> {
        parse_threshold(s)?;
        Ok(SizeArg(s.to_string()))
    }

    fn bytes(&self, units: SizeUnits) -> u64 {
        // Valid in binary units, so valid in SI: the same text is never
        // more bytes with --si.
        parse_size_in(&self.0, units).expect("validated when parsed")
    }
}

/// When to draw usage bars in the flat directory list (`--bar`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum BarMode {
    /// Only when stdout is a terminal.
    Auto,
    Always,
    Never,
}

impl BarMode {
    fn enabled(self, stdout_is_terminal: bool) -> bool {
        match self {
            BarMode::Auto => stdout_is_terminal,
            BarMode::Always => true,
            BarMode::Never => false,
        }
    }
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
        value_parser = clap::value_parser!(u16).range(1..=i64::from(MAX_JOBS)).map(|v| v as usize)
    )]
    jobs: usize,

    /// Show errors and detailed execution metrics
    #[arg(short, long)]
    debug: bool,

    /// Maximum depth of directories to display in the report
    #[arg(long)]
    max_depth: Option<usize>,

    /// Number of top directories to display in the report (also caps
    /// duplicate groups, file types and --diff rows). 0 means no limit:
    /// `--json --top 0` saves a complete snapshot for --diff.
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
        // `requires` alone is not enough: clap skips a missing-requirement
        // error when the required flag conflicts with another flag that is
        // present, so --min-size would be silently accepted (and ignored)
        // next to every mode that conflicts with --duplicates.
        conflicts_with_all = ["largest_files", "by_type", "inodes", "max_depth", "threshold"],
        value_parser = SizeArg::parse
    )]
    min_size: Option<SizeArg>,

    /// Use SI units, powers of 1000 (1 kB = 1000 bytes), as `du --si` and
    /// drive vendors do, instead of the default powers of 1024. Applies
    /// to every size printed and to sizes given in --threshold and
    /// --min-size. JSON output stays in plain bytes either way.
    #[arg(long, conflicts_with = "inodes")]
    si: bool,

    /// Print the result as one JSON document on stdout instead of text.
    /// Works with every mode (directory report, --largest-files,
    /// --duplicates) and does not change what is analysed. Only the JSON
    /// goes to stdout; warnings and diagnostics go to stderr. Paths in the
    /// document are relative to the scanned root; see the README for the
    /// schema.
    #[arg(long)]
    json: bool,

    /// Show which file types take up the space: files and total size per
    /// extension, biggest first, with each type's share of the total. The
    /// extension is the part of the name after the last '.', compared
    /// case-insensitively ('foo.tar.gz' is '.gz'; '.env', 'foo.' and 'README'
    /// have none). Respects --include, --exclude, --ignore and .gitignore;
    /// hard-linked files count once; sizes follow --apparent-size. --top
    /// limits the rows (the rest are summed into one line), --summarize
    /// prints only the totals.
    #[arg(
        long,
        conflicts_with_all = ["largest_files", "duplicates", "max_depth", "threshold"]
    )]
    by_type: bool,

    /// Show a live counter of scanned directories, files and size on stderr
    /// while the scan runs, so you can tell a long scan is alive. On a
    /// terminal the line updates in place and disappears when the report
    /// is printed; when stderr is redirected, a line is logged every two
    /// seconds plus a final summary. Never touches stdout, so it is safe
    /// with --json.
    #[arg(long)]
    progress: bool,

    /// Only count files last modified at least this long ago, e.g. 30d, 6mo,
    /// 1y. Units: s, min, h, d, w, mo (30 days), y (365 days). Applies to
    /// files (and symlinks with --inodes), never stops directories from
    /// being scanned, and works with every mode: e.g. --largest-files 20
    /// --older-than 1y lists big files nobody has touched for a year.
    #[arg(long, value_name = "AGE", value_parser = parse_age)]
    older_than: Option<Age>,

    /// Only count files modified less than this long ago, e.g. 7d, 12h.
    /// Same units as --older-than; together they select a window
    /// (--newer-than 1y --older-than 30d).
    #[arg(long, value_name = "AGE", value_parser = parse_age)]
    newer_than: Option<Age>,

    /// Stay on the filesystem of PATH, like du -x and find -xdev: do not
    /// enter directories on other filesystems, mounts or APFS volumes (on
    /// macOS that includes firmlinked locations such as /Users when
    /// scanning /). Works with every mode. Unix only.
    #[arg(short = 'x', long = "one-file-system")]
    one_file_system: bool,

    /// Draw a usage bar and percentage next to each directory in the list,
    /// relative to the scanned root: `████░░░░░░  42%`. WHEN is auto (the
    /// default: only when stdout is a terminal, so piped output keeps the
    /// plain `SIZE  PATH` lines), always, or never. `--bar` alone means
    /// always.
    #[arg(
        long,
        value_name = "WHEN",
        value_enum,
        default_value_t = BarMode::Auto,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "always",
        conflicts_with_all = ["tree", "largest_files", "duplicates", "by_type", "summarize", "json"]
    )]
    bar: BarMode,

    /// Show the directory report as a tree (like dust or ncdu, in plain
    /// text): each directory under its parent, biggest first, with its share
    /// of the total. Lists the same directories as the flat report, so
    /// --top, --max-depth, --threshold and --include work as usual; combine
    /// with --inodes to draw inode counts. Text only.
    #[arg(
        long,
        conflicts_with_all = ["largest_files", "duplicates", "by_type", "summarize", "json"]
    )]
    tree: bool,

    /// Count inodes instead of bytes, like `du --inodes`: useful when a
    /// filesystem runs out of inodes rather than space. Every directory
    /// (the scanned one included), file, symlink and special file counts as
    /// one; a hard-linked inode counts once. Shows the directories with the
    /// most inodes. --threshold is then a plain number (e.g. 10000). Respects
    /// --include (files and symlinks by name), --exclude, --ignore,
    /// --no-hidden and .gitignore; works with --top, --max-depth, --summarize
    /// and --json.
    #[arg(
        long,
        conflicts_with_all = ["largest_files", "duplicates", "by_type", "apparent_size", "min_size"]
    )]
    inodes: bool,

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

    /// Skip hidden files and directories (names starting with '.'). A hidden
    /// directory is not entered, so everything below it is skipped too. The
    /// path you scan is never skipped, even if it is hidden. Independent of
    /// --no-ignore: hidden entries are included by default, like du; use
    /// --no-ignore to count .git, node_modules and other ignored entries.
    #[arg(long = "no-hidden")]
    no_hidden: bool,

    /// Compare two snapshots saved with --json and show what grew, shrank,
    /// appeared or disappeared, biggest change first. No scan is run. Both
    /// must be directory reports (or both --inodes reports) of the same
    /// root, taken with the same options; save them with --top 0 so every
    /// directory is listed. Works with --top, --threshold (smallest change
    /// shown), --si and --json. Use - to read one snapshot from stdin.
    #[arg(
        long,
        num_args = 2,
        value_names = ["BEFORE", "AFTER"],
        conflicts_with_all = [
            "path", "debug", "max_depth", "include", "summarize", "largest_files",
            "duplicates", "min_size", "by_type", "progress", "older_than", "newer_than",
            "one_file_system", "bar", "tree", "inodes", "apparent_size", "ignore",
            "exclude", "no_ignore", "no_hidden", "jobs",
        ]
    )]
    diff: Option<Vec<PathBuf>>,
}

/// Builds the set of directory names to skip, given `--no-ignore` and any
/// explicit `--ignore` values. `--no-ignore` disables the built-in defaults
/// (`.git`, `node_modules`, `__pycache__`), but an explicit `--ignore` is
/// still honored either way, since that's the user asking for something
/// specific rather than the tool's automatic noise filtering.
fn build_ignore_dirs(no_ignore: bool, extra: Vec<String>) -> HashSet<String> {
    // ardisk's --ignore takes plain names only (globs go to --exclude).
    let (names, _) = arcommon::ignore::split_dir_excludes(DEFAULT_IGNORES, no_ignore, extra, false)
        .expect("no glob is compiled when globs are not allowed");
    names
}

fn main() {
    let args = Args::parse();

    let units = if args.si {
        SizeUnits::Si
    } else {
        SizeUnits::Binary
    };
    // Before anything is printed: progress, reports and errors all format
    // sizes through this setting.
    ardisk::set_size_units(units);

    if let Some(files) = &args.diff {
        run_diff(&args, &files[0], &files[1], units);
        return;
    }

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

    if let Err(e) = check_window(args.older_than.as_ref(), args.newer_than.as_ref()) {
        eprintln!("{}", format!("error: {}", e).red());
        std::process::exit(1);
    }

    if args.one_file_system && !cfg!(unix) {
        eprintln!(
            "{}",
            "ardisk: --one-file-system is only supported on Unix-like systems; scanning normally"
                .yellow()
        );
    }

    let scan_progress = args.progress.then(|| Arc::new(ScanProgress::default()));

    let config = Arc::new(ScanConfig {
        ignore_dirs,
        include_pattern,
        debug: args.debug,
        apparent_size: args.apparent_size,
        respect_gitignore: !args.no_ignore,
        exclude,
        skip_hidden: args.no_hidden,
        inodes: args.inodes,
        one_file_system: args.one_file_system,
        progress: scan_progress.clone(),
        mtime: MtimeFilter::new(
            SystemTime::now(),
            args.older_than.as_ref().map(|a| a.duration),
            args.newer_than.as_ref().map(|a| a.duration),
        ),
    });

    let start_time = Instant::now();

    // Phase 1: Parallel file scanning
    // Empty files are never duplicates-of-interest, so the floor is 1 byte.
    let min_len = args
        .min_size
        .as_ref()
        .map_or(1, |size| size.bytes(units))
        .max(1);
    let collect = if args.duplicates {
        Collect::Duplicates { min_len }
    } else if args.by_type {
        Collect::ByType
    } else {
        match args.largest_files {
            Some(n) => Collect::Largest(n),
            None => Collect::Nothing,
        }
    };
    // Lets later phases (comparing duplicates) report on stderr, too.
    let status: Option<Status>;
    let ScanOutput {
        raw_sizes,
        content_sizes: raw_content_sizes,
        files: collected_files,
        file_count,
        types,
    } = {
        let reporter = scan_progress
            .as_ref()
            .map(|p| Reporter::start(Arc::clone(p), args.inodes));
        let out = parallel_scan_report(target_path.clone(), args.jobs, config, collect);
        status = reporter.map(Reporter::finish);
        out
    };

    // Phase 1b (--duplicates only): narrow the candidates down to groups of
    // identical files. This is where almost all of the time goes.
    // The candidate list is moved, not copied: on a big tree it is the
    // largest allocation of the whole run.
    let (largest_files, duplicate_report) = if args.duplicates {
        if let Some(status) = &status {
            status.show(&format!(
                "comparing {} candidate file{}...",
                group_digits(collected_files.len() as u64),
                if collected_files.len() == 1 { "" } else { "s" }
            ));
        }
        let report = find_duplicates(collected_files, args.jobs);
        if let Some(status) = &status {
            status.clear();
        }
        (Vec::new(), Some(report))
    } else {
        (collected_files, None)
    };

    // Phase 2: Aggregation and rollup from bottom to top
    let aggregated_sizes = aggregate_sizes(&raw_sizes, &target_path);
    let aggregated_content = aggregate_sizes(&raw_content_sizes, &target_path);

    let duration = start_time.elapsed();

    // Parse --threshold if provided, exit early on invalid input. With
    // --inodes it is a plain count, otherwise a size.
    let threshold_bytes: Option<u64> = match &args.threshold {
        Some(t) => match if args.inodes {
            parse_count(t)
        } else {
            parse_size_in(t, units)
        } {
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
        include_active: args.include.is_some()
            || args.older_than.is_some()
            || args.newer_than.is_some(),
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
                no_hidden: args.no_hidden,
                one_file_system: args.one_file_system,
                older_than: args.older_than.as_ref().map(|a| a.text.clone()),
                newer_than: args.newer_than.as_ref().map(|a| a.text.clone()),
            },
        };
        let stdout = io::stdout();
        let mut out = io::BufWriter::new(stdout.lock());
        let written = if args.inodes {
            let selection =
                select_directories(&aggregated_sizes, &aggregated_content, &target_path, &query);
            let summary = InodeSummary {
                total_inodes: root_total,
                files: file_count,
                directories: raw_sizes.len() as u64,
            };
            write_inodes(&mut out, &meta, &query, &selection, summary)
        } else if let Some(report) = &duplicate_report {
            warn_unreadable(report, args.debug);
            write_duplicates(&mut out, &meta, args.top, args.summarize, min_len, report)
        } else if args.by_type {
            write_by_type(&mut out, &meta, args.top, args.summarize, &types)
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
            let title = if args.tree {
                "=== Directory Tree ==="
            } else if args.inodes {
                "=== Inodes per Directory ==="
            } else if args.duplicates {
                "=== Duplicate Files ==="
            } else if args.by_type {
                "=== File Types ==="
            } else if args.largest_files.is_some() {
                "=== Largest Files ==="
            } else {
                "=== Top Directories ==="
            };
            arcommon::outln!("{}", title.yellow().bold());
        }

        let show_bars = args.bar.enabled(io::stdout().is_terminal());

        if let Some(report) = &duplicate_report {
            print_duplicates(report, args.top, args.summarize);
            warn_unreadable(report, args.debug);
        } else if args.by_type {
            arcommon::out!("{}", render_by_type_text(&types, args.top, args.summarize));
        } else if args.largest_files.is_some() {
            // --largest-files: individual files, largest first, instead of
            // the per-directory report.
            for file in &largest_files {
                arcommon::outln!(
                    "{:>10}  {}",
                    format_size(file.size),
                    arcommon::display_path(&file.path)
                );
            }
        } else if args.tree {
            let selection =
                select_directories(&aggregated_sizes, &aggregated_content, &target_path, &query);
            let lines = tree_lines(&selection, &target_path);
            let text = if args.inodes {
                render_tree_text(
                    &lines,
                    root_total,
                    selection.truncated,
                    |n| n.to_string(),
                    |_, t| t,
                )
            } else {
                render_tree_text(
                    &lines,
                    root_total,
                    selection.truncated,
                    format_size_plain,
                    paint_size,
                )
            };
            arcommon::out!("{text}");
        } else if args.inodes {
            if args.summarize {
                arcommon::outln!(
                    "{:>10}  {}",
                    root_total,
                    arcommon::display_path(&target_path)
                );
            } else {
                let selection = select_directories(
                    &aggregated_sizes,
                    &aggregated_content,
                    &target_path,
                    &query,
                );
                if show_bars {
                    arcommon::out!(
                        "{}",
                        render_list_with_bars(
                            &selection.entries,
                            root_total,
                            |n| n.to_string(),
                            |_, t| t
                        )
                    );
                } else {
                    for (path, count) in &selection.entries {
                        arcommon::outln!("{:>10}  {}", count, arcommon::display_path(path));
                    }
                }
            }
        } else if args.summarize {
            // --summarize: print only the root total and exit
            arcommon::outln!(
                "{:>10}  {}",
                format_size(root_total),
                arcommon::display_path(&target_path)
            );
        } else {
            let selection =
                select_directories(&aggregated_sizes, &aggregated_content, &target_path, &query);
            if show_bars {
                arcommon::out!(
                    "{}",
                    render_list_with_bars(
                        &selection.entries,
                        root_total,
                        format_size_plain,
                        paint_size
                    )
                );
            } else {
                for (path, size) in &selection.entries {
                    arcommon::outln!(
                        "{:>10}  {}",
                        format_size(*size),
                        arcommon::display_path(path)
                    );
                }
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

/// `--diff BEFORE AFTER`: compares two `--json` snapshots; no scan.
fn run_diff(args: &Args, before: &std::path::Path, after: &std::path::Path, units: SizeUnits) {
    use ardisk::diff::{
        DiffQuery, Unit, check_comparable, compare, load, partial_warning, render_text, select,
        write_json,
    };
    let fail = |msg: &str| -> ! {
        eprintln!("{}", format!("error: {msg}").red());
        std::process::exit(1);
    };
    let stdin = std::path::Path::new("-");
    if before == stdin && after == stdin {
        fail("only one of the two snapshots can be read from stdin (-)");
    }
    let before = load(before).unwrap_or_else(|e| fail(&e));
    let after = load(after).unwrap_or_else(|e| fail(&e));
    check_comparable(&before, &after).unwrap_or_else(|e| fail(&e));

    if before.unit == Unit::Inodes && args.si {
        fail("--si applies to sizes, but these snapshots count inodes");
    }
    let threshold = args.threshold.as_ref().map(|t| {
        match before.unit {
            Unit::Bytes => parse_size_in(t, units),
            Unit::Inodes => t.trim().parse::<u64>().map_err(|_| {
                format!(
                    "Invalid threshold '{}'. These snapshots count inodes, so use a whole \
                     number, e.g. 1000.",
                    t.trim()
                )
            }),
        }
        .unwrap_or_else(|e| fail(&e))
    });

    let diff = compare(&before, &after);
    let query = DiffQuery {
        top: args.top,
        threshold,
    };
    let selection = select(&diff, &query);

    if args.json {
        let stdout = io::stdout();
        let mut out = io::BufWriter::new(stdout.lock());
        match write_json(&mut out, &diff, &selection, &query, &before, &after)
            .and_then(|()| out.flush())
        {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
            Err(e) => fail(&format!("cannot write JSON: {e}")),
        }
    } else {
        arcommon::out!("{}", render_text(&diff, &selection));
    }
    if let Some(warning) = partial_warning(&diff, &before, &after) {
        eprintln!("{}", warning.yellow());
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
        for group in report.groups.iter().take(top_limit(top)) {
            arcommon::outln!(
                "{} identical files, {} each, {} reclaimable",
                group.paths.len(),
                format_size(group.len),
                format_size(group.reclaimable())
            );
            for path in &group.paths {
                arcommon::outln!("  {}", arcommon::display_path(path));
            }
            arcommon::outln!();
        }
    }

    let shown = if summarize {
        0
    } else {
        report.groups.len().min(top_limit(top))
    };
    let more = if summarize || shown == report.groups.len() {
        String::new()
    } else {
        format!(" (showing the top {} group{})", shown, plural(shown))
    };
    let (groups, redundant) = (report.groups.len(), report.redundant_files());
    arcommon::outln!(
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
                eprintln!("  {}: {}", arcommon::display_path(path), reason);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Args, SizeUnits, build_ignore_dirs, default_jobs, parse_threshold};
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

    // ── --by-type ─────────────────────────────────────────────────────────

    #[test]
    fn by_type_defaults_to_off() {
        let args = Args::try_parse_from(["ardisk", "."]).unwrap();
        assert!(!args.by_type);
    }

    #[test]
    fn by_type_parses() {
        let args = Args::try_parse_from(["ardisk", ".", "--by-type"]).unwrap();
        assert!(args.by_type);
    }

    #[test]
    fn by_type_conflicts_with_other_report_modes() {
        for extra in [
            vec!["--duplicates"],
            vec!["--largest-files", "3"],
            vec!["--max-depth", "2"],
            vec!["--threshold", "1MB"],
        ] {
            let mut argv = vec!["ardisk", ".", "--by-type"];
            argv.extend(extra.iter());
            assert!(
                Args::try_parse_from(argv).is_err(),
                "--by-type must conflict with {:?}",
                extra
            );
        }
    }

    #[test]
    fn bar_defaults_to_auto_and_bare_flag_means_always() {
        use super::BarMode;
        let parse = |argv: &[&str]| Args::try_parse_from(argv).unwrap();
        assert_eq!(parse(&["ardisk", "."]).bar, BarMode::Auto);
        assert_eq!(parse(&["ardisk", ".", "--bar"]).bar, BarMode::Always);
        assert_eq!(parse(&["ardisk", "--bar", "."]).bar, BarMode::Always);
        assert_eq!(parse(&["ardisk", "--bar", "."]).path, ".");
        assert_eq!(parse(&["ardisk", ".", "--bar=never"]).bar, BarMode::Never);
        assert_eq!(parse(&["ardisk", ".", "--bar=auto"]).bar, BarMode::Auto);
        assert_eq!(
            parse(&["ardisk", ".", "--bar", "--inodes"]).bar,
            BarMode::Always
        );
        assert!(Args::try_parse_from(["ardisk", ".", "--bar=sometimes"]).is_err());
    }

    #[test]
    fn bar_mode_enabled() {
        use super::BarMode;
        assert!(BarMode::Auto.enabled(true));
        assert!(!BarMode::Auto.enabled(false));
        assert!(BarMode::Always.enabled(false));
        assert!(!BarMode::Never.enabled(true));
    }

    #[test]
    fn explicit_bar_conflicts_with_non_list_outputs_but_default_does_not() {
        for extra in [
            vec!["--tree"],
            vec!["--json"],
            vec!["--summarize"],
            vec!["--by-type"],
            vec!["--duplicates"],
            vec!["--largest-files", "3"],
        ] {
            let mut argv = vec!["ardisk", ".", "--bar"];
            argv.extend(extra.iter());
            assert!(Args::try_parse_from(&argv).is_err(), "{argv:?}");
            let mut plain = vec!["ardisk", "."];
            plain.extend(extra.iter());
            assert!(Args::try_parse_from(&plain).is_ok(), "{plain:?}");
        }
    }

    #[test]
    fn age_filters_parse_and_work_with_every_mode() {
        let args = Args::try_parse_from(["ardisk", "."]).unwrap();
        assert!(args.older_than.is_none() && args.newer_than.is_none());
        for extra in [
            vec![],
            vec!["--tree"],
            vec!["--inodes"],
            vec!["--by-type"],
            vec!["--duplicates"],
            vec!["--largest-files", "3"],
            vec!["--json", "-s"],
            vec!["--include", "*.log", "-x"],
        ] {
            let mut argv = vec!["ardisk", ".", "--older-than", "1y", "--newer-than", "5y"];
            argv.extend(extra.iter());
            let args = Args::try_parse_from(&argv)
                .unwrap_or_else(|e| panic!("{argv:?} should parse: {e}"));
            assert_eq!(args.older_than.unwrap().text, "1y");
            assert_eq!(args.newer_than.unwrap().text, "5y");
        }
        assert!(Args::try_parse_from(["ardisk", ".", "--older-than", "1m"]).is_err());
        assert!(Args::try_parse_from(["ardisk", ".", "--newer-than", "soon"]).is_err());
    }

    #[test]
    fn one_file_system_has_short_and_long_forms_and_works_with_every_mode() {
        assert!(
            !Args::try_parse_from(["ardisk", "."])
                .unwrap()
                .one_file_system
        );
        for flag in ["-x", "--one-file-system"] {
            for extra in [
                vec![],
                vec!["--tree"],
                vec!["--inodes"],
                vec!["--by-type"],
                vec!["--duplicates"],
                vec!["--largest-files", "3"],
                vec!["--json", "-s"],
                vec!["--no-hidden", "--no-ignore"],
            ] {
                let mut argv = vec!["ardisk", ".", flag];
                argv.extend(extra.iter());
                let args = Args::try_parse_from(&argv)
                    .unwrap_or_else(|e| panic!("{argv:?} should parse: {e}"));
                assert!(args.one_file_system);
            }
        }
    }

    #[test]
    fn tree_parses_and_combines_with_directory_report_options() {
        assert!(!Args::try_parse_from(["ardisk", "."]).unwrap().tree);
        for extra in [
            vec![],
            vec!["--top", "50"],
            vec!["--max-depth", "2"],
            vec!["--threshold", "1GB"],
            vec!["--inodes"],
            vec!["--inodes", "--threshold", "1000"],
            vec!["--apparent-size"],
            vec!["--no-hidden", "--no-ignore"],
            vec!["--include", "*.rs", "--exclude", "*.log", "--ignore", "x"],
        ] {
            let mut argv = vec!["ardisk", ".", "--tree"];
            argv.extend(extra.iter());
            let args = Args::try_parse_from(&argv)
                .unwrap_or_else(|e| panic!("{argv:?} should parse: {e}"));
            assert!(args.tree);
        }
    }

    #[test]
    fn tree_conflicts_with_other_modes_summarize_and_json() {
        for extra in [
            vec!["--by-type"],
            vec!["--duplicates"],
            vec!["--largest-files", "3"],
            vec!["--summarize"],
            vec!["--json"],
        ] {
            let mut argv = vec!["ardisk", ".", "--tree"];
            argv.extend(extra.iter());
            assert!(Args::try_parse_from(&argv).is_err(), "{argv:?}");
        }
    }

    #[test]
    fn inodes_parses_and_combines_with_directory_report_options() {
        assert!(!Args::try_parse_from(["ardisk", "."]).unwrap().inodes);
        for extra in [
            vec![],
            vec!["--json"],
            vec!["--summarize"],
            vec!["--top", "5"],
            vec!["--max-depth", "2"],
            vec!["--threshold", "1000"],
            vec!["--no-hidden", "--no-ignore"],
            vec!["--include", "*.rs", "--exclude", "*.log", "--ignore", "x"],
        ] {
            let mut argv = vec!["ardisk", ".", "--inodes"];
            argv.extend(extra.iter());
            let args = Args::try_parse_from(&argv)
                .unwrap_or_else(|e| panic!("{argv:?} should parse: {e}"));
            assert!(args.inodes);
        }
    }

    #[test]
    fn inodes_conflicts_with_every_other_mode_and_with_sizes() {
        for extra in [
            vec!["--by-type"],
            vec!["--duplicates"],
            vec!["--largest-files", "3"],
            vec!["--apparent-size"],
            vec!["--duplicates", "--min-size", "1MB"],
        ] {
            let mut argv = vec!["ardisk", ".", "--inodes"];
            argv.extend(extra.iter());
            assert!(Args::try_parse_from(&argv).is_err(), "{argv:?}");
        }
    }

    #[test]
    fn inode_threshold_is_a_plain_whole_number() {
        assert_eq!(super::parse_count(" 10000 "), Ok(10_000));
        assert_eq!(super::parse_count("0"), Ok(0));
        for bad in ["", "1MB", "1.5", "-3", "1e3", "ten"] {
            assert!(super::parse_count(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn no_hidden_is_off_by_default_and_parses() {
        assert!(!Args::try_parse_from(["ardisk", "."]).unwrap().no_hidden);
        assert!(
            Args::try_parse_from(["ardisk", ".", "--no-hidden"])
                .unwrap()
                .no_hidden
        );
    }

    #[test]
    fn no_hidden_combines_with_every_mode_and_filter() {
        for extra in [
            vec!["--by-type"],
            vec!["--duplicates"],
            vec!["--largest-files", "5"],
            vec!["--json"],
            vec!["--no-ignore"],
            vec!["--ignore", "vendor"],
            vec!["--exclude", "*.log"],
            vec!["--include", "*.rs"],
        ] {
            let mut argv = vec!["ardisk", ".", "--no-hidden"];
            argv.extend(extra.iter());
            let args = Args::try_parse_from(&argv)
                .unwrap_or_else(|e| panic!("{argv:?} should parse: {e}"));
            assert!(args.no_hidden);
        }
    }

    #[test]
    fn by_type_combines_with_filters_top_summarize_and_json() {
        let args = Args::try_parse_from([
            "ardisk",
            ".",
            "--by-type",
            "--top",
            "5",
            "--summarize",
            "--json",
            "--include",
            "*.rs",
            "--exclude",
            "target/**",
            "--apparent-size",
            "-j",
            "2",
        ])
        .unwrap();
        assert!(args.by_type && args.summarize && args.json);
        assert_eq!(args.top, 5);
    }

    #[test]
    fn min_size_is_rejected_with_every_mode_other_than_duplicates() {
        for extra in [
            vec!["--by-type"],
            vec!["--largest-files", "3"],
            vec!["--max-depth", "2"],
            vec!["--threshold", "1MB"],
            vec!["--summarize"],
            vec![],
        ] {
            let mut argv = vec!["ardisk", ".", "--min-size", "1MB"];
            argv.extend(extra.iter());
            assert!(
                Args::try_parse_from(argv).is_err(),
                "--min-size without --duplicates must be an error (with {:?})",
                extra
            );
        }
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
        let size = args.min_size.expect("--min-size given");
        assert_eq!(
            size.bytes(SizeUnits::Binary),
            parse_threshold("1MB").unwrap()
        );
        assert_eq!(size.bytes(SizeUnits::Si), 1_000_000);
    }

    #[test]
    fn sizes_are_read_in_the_chosen_units() {
        use super::parse_size_in;
        assert_eq!(parse_size_in("1KB", SizeUnits::Binary), Ok(1024));
        assert_eq!(parse_size_in("1kb", SizeUnits::Si), Ok(1000));
        assert_eq!(parse_size_in("1.5GB", SizeUnits::Si), Ok(1_500_000_000));
        assert_eq!(parse_size_in("2TB", SizeUnits::Si), Ok(2_000_000_000_000));
        assert_eq!(parse_size_in("10B", SizeUnits::Si), Ok(10));
        assert!(parse_size_in("10", SizeUnits::Si).is_err());
    }

    #[test]
    fn si_flag_parses_and_conflicts_with_inodes() {
        assert!(Args::try_parse_from(["ardisk", ".", "--si"]).unwrap().si);
        assert!(!Args::try_parse_from(["ardisk", "."]).unwrap().si);
        assert!(Args::try_parse_from(["ardisk", ".", "--si", "--inodes"]).is_err());
        for extra in [
            &["--json"][..],
            &["--largest-files", "3"],
            &["--duplicates", "--min-size", "1MB"],
            &["--by-type"],
            &["--tree"],
            &["--threshold", "1GB"],
        ] {
            let mut args = vec!["ardisk", ".", "--si"];
            args.extend_from_slice(extra);
            assert!(Args::try_parse_from(&args).is_ok(), "{args:?}");
        }
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
