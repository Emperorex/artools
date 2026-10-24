use arfind::{
    Args, DEFAULT_IGNORES, MatchResult, SearchConfig, SearchStats, SizeFilter, SortKey,
    age::{MtimeFilter, check_window},
    build_exclude_matcher,
    exec::{self, ExecTemplate},
    parallel_find, sort_results,
    types::NameKindFilter,
};
use clap::{CommandFactory, Parser};
use colored::Colorize;
use glob::Pattern;
use std::{
    collections::HashSet,
    fs,
    io::{self, BufWriter, Write},
    path::PathBuf,
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Instant, SystemTime},
};

/// Parses a size string with optional +/- prefix into a SizeFilter.
/// +N → min bound (greater than N), -N → max bound (less than N), N → exact (greater than N-1, less than N+1)
/// Supported units: B, KB, MB, GB, TB (case-insensitive).
/// Examples: "+100MB", "-1KB", "10MB"
fn parse_size(s: &str) -> Result<SizeFilter, String> {
    let s = s.trim();

    let (prefix, rest) = if let Some(stripped) = s.strip_prefix('+') {
        ('+', stripped)
    } else if let Some(stripped) = s.strip_prefix('-') {
        ('-', stripped)
    } else {
        ('=', s)
    };

    let bytes = parse_size_bytes(rest)?;

    Ok(match prefix {
        '+' => SizeFilter {
            min: Some(bytes),
            max: None,
        },
        '-' => SizeFilter {
            min: None,
            max: Some(bytes),
        },
        _ => SizeFilter {
            min: Some(bytes),
            max: None,
        }, // no prefix → same as +N
    })
}

fn parse_size_bytes(s: &str) -> Result<u64, String> {
    let idx = s
        .find(|c: char| c.is_alphabetic())
        .ok_or_else(|| format!("Missing unit in '{}'. Use B, KB, MB, GB, or TB.", s))?;

    let (num_part, suffix) = s.split_at(idx);
    let value: f64 = num_part
        .parse()
        .map_err(|_| format!("Invalid number '{}' in size '{}'.", num_part, s))?;

    if !value.is_finite() {
        return Err(format!(
            "Size must be a finite number, got '{}' in '{}'.",
            num_part, s
        ));
    }

    if value < 0.0 {
        return Err(format!("Size must be positive, got '{}'.", s));
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
        return Err(format!("Size is too large in '{}'.", s));
    }

    Ok(bytes as u64)
}

/// Builds the set of directory names to skip, given `--no-ignore` and any
/// explicit `--ignore` values. `--no-ignore` disables the built-in defaults
/// (`.git`, `node_modules`, `__pycache__`), but an explicit `--ignore` is
/// still honored either way, since that's the user asking for something
/// specific rather than the tool's automatic noise filtering.
///
/// Entries containing a glob metacharacter (`*`, `?`, `[`) become patterns;
/// plain names stay on the fast exact-lookup path (same split as argrep).
fn build_ignore_dirs(
    no_ignore: bool,
    extra: Vec<String>,
) -> Result<(HashSet<String>, Vec<Pattern>), String> {
    arcommon::ignore::split_dir_excludes(DEFAULT_IGNORES, no_ignore, extra, true)
        .map_err(|(entry, e)| format!("Invalid glob pattern for --exclude-dir: '{}': {}", entry, e))
}

/// Runs the search and returns every result, in traversal order.
fn collect(
    root: PathBuf,
    jobs: usize,
    config: Arc<SearchConfig>,
    stats: &SearchStats,
) -> Vec<MatchResult> {
    let found: Arc<Mutex<Vec<MatchResult>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&found);
    parallel_find(root, jobs, config, stats.clone(), move |item| {
        sink.lock().unwrap().push(item);
    });
    std::mem::take(&mut *found.lock().unwrap())
}

/// Writes one result as the unsorted output does: escaped, directories in
/// bold blue, one per line; or with `-0` the exact path bytes and a NUL.
fn write_item(out: &mut impl Write, item: &MatchResult, print0: bool) -> io::Result<()> {
    if print0 {
        out.write_all(&arcommon::fs::path_bytes(&item.path))?;
        out.write_all(b"\0")
    } else if item.is_dir {
        writeln!(out, "{}", arcommon::display_path(&item.path).blue().bold())
    } else {
        writeln!(out, "{}", arcommon::display_path(&item.path))
    }
}

/// Prints `error: ...` in red and exits with status 1.
fn fail(message: &str) -> ! {
    eprintln!("{}", format!("error: {}", message).red());
    std::process::exit(1);
}

fn main() {
    let args = Args::parse();

    if args.type_list {
        print!("{}", arcommon::filetypes::type_list());
        return;
    }

    if let Some(ref t) = args.file_type
        && t != "f"
        && t != "d"
        && t != "l"
    {
        eprintln!(
            "{}",
            format!(
                "error: Invalid type '{}'. Use 'f' for files, 'd' for directories, or 'l' for symlinks.",
                t
            )
            .red()
        );
        std::process::exit(1);
    }

    let (ignore_dirs, ignore_dir_patterns) =
        build_ignore_dirs(args.no_ignore, args.exclude_dir).unwrap_or_else(|e| fail(&e));

    // --exclude patterns are anchored at the canonical root, the same path
    // the search reports entries under.
    let canonical_root = fs::canonicalize(&args.path).unwrap_or_else(|_| PathBuf::from(&args.path));
    let exclude =
        build_exclude_matcher(&canonical_root, &args.exclude).unwrap_or_else(|e| fail(&e));

    check_window(args.older_than.as_ref(), args.newer_than.as_ref()).unwrap_or_else(|e| fail(&e));

    if args.one_file_system && !cfg!(unix) {
        eprintln!(
            "{}",
            "arfind: --one-file-system is only supported on Unix-like systems; searching normally"
                .yellow()
        );
    }

    let pattern = Pattern::new(&args.name).expect("Invalid glob pattern");

    let size_filter: Option<SizeFilter> = match &args.size {
        Some(s) => match parse_size(s) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("{}", format!("error: {}", e).red());
                std::process::exit(1);
            }
        },
        None => None,
    };

    let config = Arc::new(SearchConfig {
        pattern,
        ignore_dirs,
        max_depth: args.max_depth,
        file_type: args.file_type,
        hidden: args.hidden,
        debug: args.debug,
        size_filter,
        empty_only: args.empty,
        case_insensitive: args.case_insensitive,
        respect_gitignore: !args.no_ignore,
        ignore_dir_patterns,
        exclude,
        one_file_system: args.one_file_system,
        name_kind: NameKindFilter::new(&args.types, &args.type_not, &args.extension),
        mtime: MtimeFilter::new(
            SystemTime::now(),
            args.older_than.as_ref().map(|a| a.duration),
            args.newer_than.as_ref().map(|a| a.duration),
        ),
        collect_metadata: args.sort.is_some_and(SortKey::needs_metadata),
    });

    // Parsed before searching, so a bad command fails fast.
    let exec = match (&args.exec, &args.exec_batch, &args.execdir) {
        (Some(words), _, _) => Some(ExecTemplate::parse(words, false)),
        (None, Some(words), _) => Some(ExecTemplate::parse(words, true)),
        (None, None, Some(words)) => Some(ExecTemplate::parse_execdir(
            words,
            std::env::var_os("PATH").as_deref(),
        )),
        (None, None, None) => None,
    }
    .transpose()
    .unwrap_or_else(|e| fail(&e));
    // Everything after --exec belongs to the command, so an arfind option
    // typed after it (e.g. `--exec rm --dry-run`) would silently go to the
    // command. Refuse unless the user ended the command with ';' and so
    // made clear those words are meant for it.
    if let Some(words) = args
        .exec
        .as_ref()
        .or(args.exec_batch.as_ref())
        .or(args.execdir.as_ref())
    {
        let raw: Vec<String> = std::env::args().collect();
        let known: Vec<String> = Args::command()
            .get_arguments()
            .flat_map(|a| {
                a.get_long_and_visible_aliases()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|l| format!("--{l}"))
            })
            .collect();
        let misplaced = exec::misplaced_options(words, &known);
        if !misplaced.is_empty() && !exec::command_terminated(&raw) {
            fail(&format!(
                "{} after --exec would be passed to the command, not to arfind. \
                 Put arfind options before --exec, or end the command with ';' \
                 if they are meant for the command (e.g. --exec rsync --dry-run {{}} host: ';')",
                misplaced.join(", ")
            ));
        }
    }
    if args.dry_run && exec.is_none() {
        fail("--dry-run only applies to --exec, --exec-batch or --execdir");
    }

    let stats = SearchStats::new();
    let start_time = Instant::now();
    // Passed as given — parallel_find canonicalizes internally, but keeps
    // the original argument around too, since resolving "." or ".." away
    // before matching would silently break `--name .`/`--name ..` against
    // the root itself.
    let root_path = PathBuf::from(&args.path);
    let count_only = args.count;

    let mut exit_code = 0;

    if let Some(template) = exec {
        // --exec/--exec-batch: collect everything first, so commands that
        // move or delete files cannot affect the search, then run them in
        // a stable order: by path, or as --sort says.
        let mut found = collect(root_path, args.jobs, config, &stats);
        sort_results(&mut found, args.sort.unwrap_or(SortKey::Path), args.reverse);
        let paths: Vec<PathBuf> = found.into_iter().map(|m| m.path).collect();

        let stdout = io::stdout();
        let mut out = stdout.lock();
        match exec::run(&template, &paths, args.dry_run, &mut out) {
            Ok(summary) if summary.failed > 0 => {
                eprintln!(
                    "{}",
                    format!(
                        "arfind: {} of {} command{} failed",
                        summary.failed,
                        summary.runs,
                        if summary.runs == 1 { "" } else { "s" }
                    )
                    .red()
                );
                exit_code = 1;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("{}", format!("error: {}", e).red());
                exit_code = 1;
            }
        }
    } else if let Some(key) = args.sort {
        // --sort: the whole result set is needed before the first line.
        let mut found = collect(root_path, args.jobs, config, &stats);
        sort_results(&mut found, key, args.reverse);
        let mut out = BufWriter::new(io::stdout().lock());
        for item in &found {
            if write_item(&mut out, item, args.print0).is_err() {
                break; // e.g. `| head`: the reader is gone
            }
        }
        let _ = out.flush();
    } else if args.print0 {
        // -0: exact bytes, NUL-terminated. Buffered, since a listing can be
        // millions of paths, and flushed once at the end: stdout's own
        // buffer is line-based, and NUL-separated output never ends a line.
        let out = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
        let sink = Arc::clone(&out);
        parallel_find(root_path, args.jobs, config, stats.clone(), move |item| {
            let mut w = sink.lock().unwrap();
            let _ = w.write_all(&arcommon::fs::path_bytes(&item.path));
            let _ = w.write_all(b"\0");
        });
        // Flushed before the statistics and the exit status below.
        let _ = out.lock().unwrap().flush();
    } else {
        parallel_find(root_path, args.jobs, config, stats.clone(), move |item| {
            if !count_only {
                if item.is_dir {
                    println!("{}", arcommon::display_path(&item.path).blue().bold());
                } else {
                    println!("{}", arcommon::display_path(&item.path));
                }
            }
        });
    }

    let duration = start_time.elapsed();

    // --count: print total matches after all workers finish
    if count_only {
        println!("{}", stats.matched_count.load(Ordering::Relaxed));
    }

    if args.debug {
        eprintln!("{}", "\n=== Search Statistics ===".yellow().bold());
        eprintln!("Worker threads:        {}", args.jobs.to_string().cyan());
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

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

#[cfg(test)]
mod tests {
    use super::{build_ignore_dirs, parse_size, parse_size_bytes};

    // ── parse_size_bytes ──────────────────────────────────────────────────────

    #[test]
    fn bytes_unit() {
        assert_eq!(parse_size_bytes("500B").unwrap(), 500);
    }

    #[test]
    fn kilobytes_unit() {
        assert_eq!(parse_size_bytes("1KB").unwrap(), 1024);
    }

    #[test]
    fn megabytes_unit() {
        assert_eq!(parse_size_bytes("10MB").unwrap(), 10 * 1024 * 1024);
    }

    #[test]
    fn gigabytes_unit() {
        assert_eq!(parse_size_bytes("2GB").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn terabytes_unit() {
        assert_eq!(parse_size_bytes("1TB").unwrap(), 1024_u64.pow(4));
    }

    #[test]
    fn case_insensitive_unit() {
        assert_eq!(parse_size_bytes("10mb").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_size_bytes("10Mb").unwrap(), 10 * 1024 * 1024);
    }

    #[test]
    fn fractional_value() {
        assert_eq!(parse_size_bytes("0.5GB").unwrap(), 512 * 1024 * 1024);
    }

    #[test]
    fn missing_unit_returns_error() {
        assert!(parse_size_bytes("1024").is_err());
    }

    #[test]
    fn unknown_unit_returns_error() {
        assert!(parse_size_bytes("10PB").is_err());
    }

    #[test]
    fn non_numeric_returns_error() {
        assert!(parse_size_bytes("tenMB").is_err());
    }

    // ── parse_size prefix handling ────────────────────────────────────────────

    #[test]
    fn plus_prefix_sets_min_only() {
        let f = parse_size("+100MB").unwrap();
        assert_eq!(f.min, Some(100 * 1024 * 1024));
        assert_eq!(f.max, None);
    }

    #[test]
    fn minus_prefix_sets_max_only() {
        let f = parse_size("-1KB").unwrap();
        assert_eq!(f.min, None);
        assert_eq!(f.max, Some(1024));
    }

    #[test]
    fn no_prefix_means_greater_than() {
        let f = parse_size("10MB").unwrap();
        let bytes = 10 * 1024 * 1024u64;
        assert_eq!(f.min, Some(bytes));
        assert_eq!(f.max, None);
    }

    #[test]
    fn invalid_size_returns_error() {
        assert!(parse_size("+badMB").is_err());
        assert!(parse_size("10XB").is_err());
        assert!(parse_size("").is_err());
    }

    // ── build_ignore_dirs ────────────────────────────────────────────────────

    #[test]
    fn default_ignores_included_when_not_no_ignore() {
        let (dirs, _) = build_ignore_dirs(false, vec![]).unwrap();
        assert!(dirs.contains(".git"));
        assert!(dirs.contains("node_modules"));
        assert!(dirs.contains("__pycache__"));
    }

    #[test]
    fn no_ignore_excludes_default_ignores() {
        let (dirs, _) = build_ignore_dirs(true, vec![]).unwrap();
        assert!(!dirs.contains(".git"));
        assert!(!dirs.contains("node_modules"));
        assert!(!dirs.contains("__pycache__"));
        assert!(dirs.is_empty());
    }

    #[test]
    fn explicit_ignore_honored_alongside_defaults() {
        let (dirs, _) = build_ignore_dirs(false, vec!["vendor".to_string()]).unwrap();
        assert!(dirs.contains(".git"));
        assert!(dirs.contains("vendor"));
    }

    #[test]
    fn explicit_ignore_honored_even_with_no_ignore() {
        let (dirs, _) = build_ignore_dirs(true, vec!["vendor".to_string()]).unwrap();
        assert!(!dirs.contains(".git"));
        assert!(dirs.contains("vendor"));
        assert_eq!(dirs.len(), 1);
    }

    #[test]
    fn exclude_dir_globs_become_patterns_and_names_stay_names() {
        let (names, patterns) =
            build_ignore_dirs(true, vec!["vendor".into(), "build*".into(), "tmp?".into()]).unwrap();
        assert_eq!(names.len(), 1);
        assert!(names.contains("vendor"));
        let globs: Vec<&str> = patterns.iter().map(|p| p.as_str()).collect();
        assert_eq!(globs, ["build*", "tmp?"]);
    }

    #[test]
    fn invalid_exclude_dir_glob_is_an_error() {
        assert!(build_ignore_dirs(false, vec!["[oops".into()]).is_err());
    }
}
