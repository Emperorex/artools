use argrep::{
    DEFAULT_IGNORES, MatchOptions, SearchConfig, SearchStats, build_matcher, parallel_grep,
};
use clap::Parser;
use clap::builder::TypedValueParser as _;
use colored::Colorize;
use glob::Pattern;
use regex::Regex;
use std::{
    collections::HashSet,
    fs,
    io::{self, BufRead, IsTerminal},
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

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

/// Hard upper bound for -B/--before-context, -A/--after-context, and
/// -C/--context.
///
/// Each unit maps directly to a slot in a `VecDeque<(usize, String)>` that
/// is pre-allocated with `VecDeque::with_capacity(before_ctx)`, so an
/// unbounded `usize` value (e.g. `-C 18446744073709551615`, which is a
/// valid usize on 64-bit) lets a caller trigger an oversized allocation
/// attempt and abort/OOM the process. 100_000 lines of context is already
/// far beyond any realistic use case.
const MAX_CONTEXT_LINES: usize = 100_000;

/// Custom value parser for -B/-A/-C.
///
/// `usize` isn't one of clap's built-in ranged numeric types (only
/// u8/i8/u16/i16/u32/i32/u64/i64 support `.range()` via
/// `value_parser!(..).range(..)`), so the bound is enforced by hand here.
fn context_lines_parser(value: &str) -> Result<usize, String> {
    let value: usize = value
        .parse()
        .map_err(|_| "context must be a non-negative integer".to_string())?;

    if value > MAX_CONTEXT_LINES {
        return Err(format!(
            "context is too large; maximum is {MAX_CONTEXT_LINES}"
        ));
    }

    Ok(value)
}

/// Hard upper bound for -j/--jobs.
///
/// `jobs` maps 1:1 to raw `thread::spawn` calls (see the worker loop below),
/// so an unbounded value lets a caller trivially exhaust threads/PIDs/memory
/// on the host (e.g. `-j 65535`). 128 comfortably covers even large CI/build
/// machines while keeping a hostile or accidental value from taking down the
/// process or the machine it runs on.
const MAX_JOBS: u16 = 128;

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

    /// Root directory or file to start the search
    #[arg()]
    path: Option<String>,

    /// Case-insensitive search
    #[arg(short = 'i', long)]
    ignore_case: bool,

    /// Treat QUERY as a literal string instead of a regex
    #[arg(short = 'F', long = "fixed-strings")]
    fixed_strings: bool,

    /// Match only whole words, like grep -w (wraps the pattern in \b...\b)
    #[arg(short = 'w', long = "word-regexp")]
    whole_word: bool,

    /// Match only whole lines, like grep -x (wraps the pattern in ^...$)
    #[arg(short = 'x', long = "line-regexp")]
    whole_line: bool,

    /// Suppress all output; exit code alone reports whether a match was
    /// found (0 = match, 1 = no match, 2 = error). Search stops after the
    /// first match. Takes priority over -l/-c/-n if those are also set —
    /// nothing is printed either way.
    #[arg(short = 'q', long)]
    quiet: bool,

    /// Stop searching a file after NUM matching lines (must be >= 1).
    /// With -v, counts non-matching (selected) lines instead, same as
    /// grep. With -c, caps the printed count at NUM. With -A/-B/-C, any
    /// pending trailing context is still printed before stopping.
    #[arg(
        short = 'm',
        long = "max-count",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    max_count: Option<u64>,

    /// Display line numbers in the output results
    #[arg(short = 'n', long)]
    line_number: bool,

    /// Number of worker threads
    #[arg(
        short = 'j',
        long,
        default_value_t = default_jobs(),
        value_parser = clap::value_parser!(u16).range(1..=i64::from(MAX_JOBS)).map(|v| v as usize)
    )]
    jobs: usize,

    /// Show search statistics and operational errors
    #[arg(short, long)]
    debug: bool,

    /// Invert match: print lines that do NOT contain the query
    #[arg(short = 'v', long)]
    invert: bool,

    /// Print only the matched text (one occurrence per output line)
    /// instead of the whole matching line
    #[arg(short = 'o', long = "only-matching", conflicts_with = "invert")]
    only_matching: bool,

    /// Print only filenames of files that contain a match
    #[arg(
        short = 'l',
        long = "files-with-matches",
        conflicts_with = "count_per_file"
    )]
    files_with_matches: bool,

    /// Print count of matching lines per file instead of the lines themselves
    #[arg(short = 'c', long = "count")]
    count_per_file: bool,

    /// Only search files whose names match this glob (e.g. "*.rs", "*.log")
    #[arg(long)]
    include: Option<String>,

    /// Skip files whose names match this glob (e.g. "*.min.js", "*.lock").
    /// Can be given multiple times. If a file matches both --include and
    /// --exclude, --exclude wins (see README for why this differs from
    /// GNU grep's own order-dependent precedence rule).
    #[arg(long)]
    exclude: Vec<String>,

    /// Show NUM lines of leading context before matching lines
    #[arg(short = 'B', long = "before-context", value_parser = context_lines_parser)]
    before_context: Option<usize>,

    /// Show NUM lines of trailing context after matching lines
    #[arg(short = 'A', long = "after-context", value_parser = context_lines_parser)]
    after_context: Option<usize>,

    /// Show NUM lines of leading and trailing context around matching lines
    #[arg(short = 'C', long = "context", value_parser = context_lines_parser)]
    context: Option<usize>,

    /// Skip directories matching this name or glob (e.g. "node_modules",
    /// "build*"). Matched against the directory's basename only, not the
    /// full path. Can be given multiple times. Applied while walking the
    /// tree, before a matching directory is ever handed to a worker, so
    /// excluded subtrees cost no traversal time at all.
    #[arg(long = "exclude-dir", visible_alias = "ignore")]
    exclude_dir: Vec<String>,

    /// Do not respect .gitignore / .ignore files (search everything)
    #[arg(long = "no-ignore")]
    no_ignore: bool,
}

/// Splits `--exclude-dir`/`--ignore` entries (plus the built-in defaults,
/// unless `--no-ignore`) into exact names and glob patterns, based on
/// whether an entry contains a glob metacharacter (`*`, `?`, `[`). Plain
/// names — the common case, e.g. "node_modules" — stay on the fast
/// HashSet-lookup path; only entries that actually need glob matching
/// (e.g. "build*") get compiled into a Pattern. An explicit
/// `--exclude-dir`/`--ignore` is honored even under `--no-ignore`, since
/// that's the user asking for something specific rather than the tool's
/// automatic noise filtering.
fn build_ignore_dirs(
    no_ignore: bool,
    extra: Vec<String>,
    quiet: bool,
) -> (HashSet<String>, Vec<Pattern>) {
    let mut names: HashSet<String> = if no_ignore {
        HashSet::new()
    } else {
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect()
    };

    let mut patterns = Vec::new();
    for entry in extra {
        if entry.contains(['*', '?', '[']) {
            patterns.push(compile_glob(&entry, "--exclude-dir", quiet));
        } else {
            names.insert(entry);
        }
    }

    (names, patterns)
}

/// Compiles a single glob pattern, exiting with the same CLI-error
/// style/exit-code convention used elsewhere in this file (e.g. for
/// invalid regex) if it's malformed.
fn compile_glob(pattern: &str, flag_name: &str, quiet: bool) -> Pattern {
    match Pattern::new(pattern) {
        Ok(pat) => pat,
        Err(e) => {
            eprintln!(
                "{}",
                format!(
                    "error: Invalid glob pattern for {}: '{}': {}",
                    flag_name, pattern, e
                )
                .red()
            );
            std::process::exit(if quiet { 2 } else { 1 });
        }
    }
}

fn main() {
    let args = Args::parse();

    let regex = match build_matcher(
        &args.query,
        MatchOptions {
            fixed_strings: args.fixed_strings,
            ignore_case: args.ignore_case,
            whole_word: args.whole_word,
            whole_line: args.whole_line,
        },
    ) {
        Ok(re) => re,
        Err(e) => {
            eprintln!("{}", format!("error: {}", e).red());
            // Under -q, exit codes are the whole interface (0=match,
            // 1=no match, 2=error), matching grep's own convention — so a
            // config error has to land on 2, not 1, which -q reserves for
            // "ran fine, found nothing". Outside -q, this tool's own
            // convention (documented in the README) uses 1 for config/IO
            // errors, so that's untouched.
            std::process::exit(if args.quiet { 2 } else { 1 });
        }
    };

    let (ignore_dirs, ignore_dir_patterns) =
        build_ignore_dirs(args.no_ignore, args.exclude_dir, args.quiet);

    let include_pattern: Option<Pattern> = args
        .include
        .as_deref()
        .map(|p| compile_glob(p, "--include", args.quiet));

    let exclude_patterns: Vec<Pattern> = args
        .exclude
        .iter()
        .map(|p| compile_glob(p, "--exclude", args.quiet))
        .collect();

    let mut before_context = args.before_context.or(args.context).unwrap_or(0);
    let mut after_context = args.after_context.or(args.context).unwrap_or(0);
    if args.only_matching && (before_context > 0 || after_context > 0) {
        // Same behavior GNU grep documents for this combination: "With
        // the -o or --only-matching option, these options have no effect
        // and a warning is given upon their use." Warn rather than error,
        // since a script combining -o with a context flag it inherited
        // from elsewhere shouldn't be treated as a hard failure.
        eprintln!(
            "{}",
            "warning: -A/-B/-C have no effect with -o/--only-matching".yellow()
        );
        before_context = 0;
        after_context = 0;
    }
    let respect_gitignore = !args.no_ignore;

    let config = Arc::new(SearchConfig {
        regex,
        query: args.query,
        ignore_case: args.ignore_case,
        line_number: args.line_number,
        ignore_dirs,
        ignore_dir_patterns,
        debug: args.debug,
        invert: args.invert,
        files_with_matches: args.files_with_matches,
        count_per_file: args.count_per_file,
        include_pattern,
        exclude_patterns,
        before_context,
        after_context,
        respect_gitignore,
        quiet: args.quiet,
        only_matching: args.only_matching,
        max_count: args.max_count.map(|v| v as usize),
    });

    let stats = SearchStats::new();
    let start_time = Instant::now();

    let use_stdin = match &args.path {
        Some(p) if p == "-" => true,
        Some(_) => false,
        None => !io::stdin().is_terminal(),
    };

    if use_stdin {
        grep_stdin(&config, &stats);
    } else {
        let raw_path = args.path.as_deref().unwrap_or(".");
        let root_path = fs::canonicalize(raw_path).unwrap_or_else(|_| PathBuf::from(raw_path));

        let line_number = config.line_number;
        let regex = config.regex.clone();
        let files_with_matches = config.files_with_matches;
        let count_per_file = config.count_per_file;

        parallel_grep(root_path, args.jobs, config, stats.clone(), move |result| {
            print_result(
                &result,
                files_with_matches,
                count_per_file,
                line_number,
                &regex,
            );
        });
    }

    let duration = start_time.elapsed();

    if args.debug {
        eprintln!("{}", "\n=== Search Statistics ===".yellow().bold());
        if use_stdin {
            eprintln!("Worker threads:      {}", "n/a (stdin mode)".cyan());
        } else {
            eprintln!("Worker threads:      {}", args.jobs.to_string().cyan());
        }
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

    // Contract: if every file was read successfully, exit 0. If any file or
    // directory could not be read (permission denied, I/O error mid-read),
    // exit nonzero — even though the scan itself continued past those and
    // printed everything it could. Silently returning 0 when part of the
    // tree was unreadable would look like "no matches" when it might really
    // mean "some files were never searched", which is misleading for a
    // grep-like tool, especially in scripts checking $?.
    let io_error_count = stats.io_errors.load(Ordering::Relaxed);
    if io_error_count > 0 {
        eprintln!(
            "{}",
            format!(
                "argrep: {} file(s)/director(ies) could not be read{}",
                io_error_count,
                if args.debug {
                    ""
                } else {
                    " (rerun with --debug for details)"
                }
            )
            .red()
        );
    }

    if args.quiet {
        // -q: the exit code alone reports the outcome, following grep's
        // own convention — 0 = at least one match, 1 = no matches, 2 = an
        // error occurred (I/O or otherwise). This intentionally differs
        // from the tool's default (non -q) contract just below, where 0
        // always means "the scan ran" and 1 means "an I/O error occurred",
        // regardless of whether anything matched.
        std::process::exit(if io_error_count > 0 {
            2
        } else if stats.matched_lines.load(Ordering::Relaxed) > 0 {
            0
        } else {
            1
        });
    }

    if io_error_count > 0 {
        std::process::exit(1);
    }
}

/// Reads lines from stdin and prints those matching the config query.
/// Used when argrep is invoked as part of a pipeline: cmd | argrep "pattern"
fn grep_stdin(config: &argrep::SearchConfig, stats: &argrep::SearchStats) {
    let stdin = io::stdin();
    let mut line_num = 0usize;
    let mut match_count = 0usize;

    let before_ctx = config.before_context;
    let after_ctx = config.after_context;
    let has_context = before_ctx > 0 || after_ctx > 0;

    let mut before_buffer: std::collections::VecDeque<(usize, String)> =
        std::collections::VecDeque::with_capacity(before_ctx);
    let mut after_remaining = 0usize;
    let mut last_printed_line = 0usize;
    let mut has_printed_anything = false;
    // -m: mirrors grep_file's handling — see the comment there for why
    // this doesn't break immediately.
    let mut reached_max_count = false;

    for line in stdin.lock().lines().map_while(Result::ok) {
        if reached_max_count && after_remaining == 0 {
            break;
        }

        line_num += 1;

        let line_matches = config.regex.is_match(&line);

        let should_emit = if config.invert {
            !line_matches
        } else {
            line_matches
        };

        if should_emit {
            match_count += 1;
            stats.matched_lines.fetch_add(1, Ordering::Relaxed);

            if config.quiet {
                // -q: exit status only, no output — takes priority over
                // -l/-c/etc. if also set, same as the file-search path.
                break;
            }

            if config.count_per_file {
                // accumulate — print after EOF
            } else if config.files_with_matches {
                // stdin has no filename — print "<stdin>" once then stop
                println!("{}", "<stdin>".magenta());
                break;
            } else if config.only_matching {
                // -o: one printed line per match occurrence, containing
                // only the matched text. Context is a no-op here too —
                // main.rs already zeroed before_context/after_context (and
                // warned) if -o was combined with -A/-B/-C, so has_context
                // is always false whenever this branch runs.
                for m in config.regex.find_iter(&line) {
                    print_stdin_line(m.as_str(), line_num, false, config);
                }
                last_printed_line = line_num;
                has_printed_anything = true;
            } else {
                if has_context {
                    let first_line_to_print = if let Some((b_num, _)) = before_buffer.front() {
                        std::cmp::min(*b_num, line_num)
                    } else {
                        line_num
                    };

                    if has_printed_anything && first_line_to_print > last_printed_line + 1 {
                        println!("{}", "--".cyan());
                    }

                    while let Some((b_num, b_content)) = before_buffer.pop_front() {
                        if b_num > last_printed_line {
                            print_stdin_line(&b_content, b_num, true, config);
                            last_printed_line = b_num;
                        }
                    }
                }

                print_stdin_line(&line, line_num, false, config);
                last_printed_line = line_num;
                has_printed_anything = true;
                after_remaining = after_ctx;

                if before_ctx > 0 {
                    before_buffer.push_back((line_num, line.clone()));
                }
            }

            if let Some(max) = config.max_count
                && match_count >= max
            {
                reached_max_count = true;
            }
        } else if has_context && !config.count_per_file && !config.files_with_matches {
            if after_remaining > 0 {
                print_stdin_line(&line, line_num, true, config);
                last_printed_line = line_num;
                after_remaining -= 1;
            }

            if before_ctx > 0 {
                if before_buffer.len() == before_ctx {
                    before_buffer.pop_front();
                }
                before_buffer.push_back((line_num, line.clone()));
            }
        }
    }

    if config.count_per_file && !config.quiet {
        println!(
            "{}: {}",
            "<stdin>".magenta(),
            match_count.to_string().green()
        );
    }
}

/// Highlights every regex match in `line` in bold red.
///
/// Uses `find_iter` rather than `str::replace`, because with a real regex
/// pattern (e.g. `foo.*bar`) the matched text isn't necessarily equal to
/// the pattern string itself, so a literal string replace would either
/// miss it or (worse) replace unrelated literal occurrences of the
/// pattern text.
fn highlight_matches(line: &str, regex: &Regex) -> String {
    let mut highlighted = String::with_capacity(line.len());
    let mut last_end = 0;

    for m in regex.find_iter(line) {
        highlighted.push_str(&line[last_end..m.start()]);
        highlighted.push_str(&m.as_str().red().bold().to_string());
        last_end = m.end();
    }
    highlighted.push_str(&line[last_end..]);

    highlighted
}

fn print_stdin_line(line: &str, line_num: usize, is_context: bool, config: &argrep::SearchConfig) {
    if is_context {
        if config.line_number {
            println!("{}-{}", line_num.to_string().green(), line.trim_end());
        } else {
            println!("{}", line.trim_end());
        }
    } else {
        let highlighted = highlight_matches(line, &config.regex);
        if config.line_number {
            println!(
                "{}:{}",
                line_num.to_string().green(),
                highlighted.trim_end()
            );
        } else {
            println!("{}", highlighted.trim_end());
        }
    }
}

/// Shared output formatter for parallel_grep results.
fn print_result(
    result: &argrep::MatchResult,
    files_with_matches: bool,
    count_per_file: bool,
    line_number: bool,
    regex: &Regex,
) {
    if result.is_separator {
        println!("{}", "--".cyan());
        return;
    }

    if files_with_matches {
        println!("{}", result.file_path.display().to_string().magenta());
    } else if count_per_file {
        println!(
            "{}: {}",
            result.file_path.display().to_string().magenta(),
            result.count.unwrap_or(0).to_string().green()
        );
    } else {
        let sep = if result.is_context { "-" } else { ":" };
        let prefix = if line_number {
            format!(
                "{}{}{}",
                result.file_path.display().to_string().magenta(),
                sep,
                result.line_num.to_string().green()
            )
        } else {
            result.file_path.display().to_string().magenta().to_string()
        };

        let content = if result.is_context {
            result.line_content.trim_end().to_string()
        } else {
            highlight_matches(&result.line_content, regex)
                .trim_end()
                .to_string()
        };

        println!("{}{}{}", prefix, sep, content);
    }
}

#[cfg(test)]
mod tests {
    use super::{Args, build_ignore_dirs, default_jobs};
    use clap::Parser;

    // ── -j / --jobs boundary ─────────────────────────────────────────────────
    // `0` workers means the task queue is never drained, silently producing
    // an empty (wrong) result with exit code 0 — worse than a crash for
    // automation. This must be rejected at CLI parse time, not left to the
    // worker pool to (fail to) handle.

    #[test]
    fn jobs_zero_is_rejected_at_parse_time() {
        let result = Args::try_parse_from(["argrep", "foo", ".", "-j", "0"]);
        assert!(result.is_err(), "-j 0 must be a CLI parse error");
    }

    #[test]
    fn jobs_one_is_accepted() {
        let args = Args::try_parse_from(["argrep", "foo", ".", "-j", "1"]).unwrap();
        assert_eq!(args.jobs, 1);
    }

    #[test]
    fn jobs_two_is_accepted() {
        let args = Args::try_parse_from(["argrep", "foo", ".", "-j", "2"]).unwrap();
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
        let args = Args::try_parse_from(["argrep", "foo", "."]).unwrap();
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

    // ── -c / -l mutual exclusivity ───────────────────────────────────────────
    // -c (count per file) and -l (filenames only) imply different, mutually
    // incompatible output contracts ("filename:count" vs just "filename").
    // Silently letting one take precedence over the other (previously -l,
    // since it's checked first and `break`s before the -c branch is ever
    // reached) is a hidden, undocumented contract. Reject the combination
    // at parse time instead so the CLI is explicit about it.

    #[test]
    fn count_and_files_with_matches_together_is_rejected_at_parse_time() {
        let result = Args::try_parse_from(["argrep", "foo", ".", "-c", "-l"]);
        assert!(result.is_err(), "-c -l together must be a CLI parse error");
    }

    #[test]
    fn count_and_files_with_matches_together_is_rejected_regardless_of_order() {
        let result = Args::try_parse_from(["argrep", "foo", ".", "-l", "-c"]);
        assert!(result.is_err(), "-l -c together must be a CLI parse error");
    }

    #[test]
    fn count_alone_is_accepted() {
        let args = Args::try_parse_from(["argrep", "foo", ".", "-c"]).unwrap();
        assert!(args.count_per_file);
        assert!(!args.files_with_matches);
    }

    #[test]
    fn files_with_matches_alone_is_accepted() {
        let args = Args::try_parse_from(["argrep", "foo", ".", "-l"]).unwrap();
        assert!(args.files_with_matches);
        assert!(!args.count_per_file);
    }

    #[test]
    fn default_ignores_included_when_not_no_ignore() {
        let (names, _patterns) = build_ignore_dirs(false, vec![], false);
        assert!(names.contains(".git"));
        assert!(names.contains("node_modules"));
        assert!(names.contains("__pycache__"));
        assert!(names.contains("target"));
    }

    #[test]
    fn no_ignore_excludes_default_ignores() {
        let (names, patterns) = build_ignore_dirs(true, vec![], false);
        assert!(names.is_empty());
        assert!(patterns.is_empty());
    }

    #[test]
    fn explicit_ignore_honored_alongside_defaults() {
        let (names, _patterns) = build_ignore_dirs(false, vec!["vendor".to_string()], false);
        assert!(names.contains("vendor"));
        assert!(names.contains(".git"));
    }

    #[test]
    fn explicit_ignore_honored_even_with_no_ignore() {
        let (names, patterns) = build_ignore_dirs(true, vec!["vendor".to_string()], false);
        assert_eq!(names.len(), 1);
        assert!(names.contains("vendor"));
        assert!(patterns.is_empty());
    }

    #[test]
    fn glob_exclude_dir_entry_becomes_a_pattern_not_a_literal_name() {
        // "build*" contains a glob metacharacter, so it should be routed
        // to the pattern list, not treated as a literal directory name.
        let (names, patterns) = build_ignore_dirs(true, vec!["build*".to_string()], false);
        assert!(names.is_empty());
        assert_eq!(patterns.len(), 1);
        assert!(patterns[0].matches("build"));
        assert!(patterns[0].matches("build-tools"));
        assert!(!patterns[0].matches("target"));
    }

    #[test]
    fn plain_and_glob_exclude_dir_entries_can_be_combined() {
        let (names, patterns) = build_ignore_dirs(
            true,
            vec!["node_modules".to_string(), "build*".to_string()],
            false,
        );
        assert!(names.contains("node_modules"));
        assert_eq!(patterns.len(), 1);
    }

    // ── -F / --fixed-strings ─────────────────────────────────────────────────

    #[test]
    fn fixed_strings_flag_defaults_to_false() {
        let args = Args::try_parse_from(["argrep", "foo", "."]).unwrap();
        assert!(!args.fixed_strings);
    }

    #[test]
    fn fixed_strings_short_flag_is_parsed() {
        let args = Args::try_parse_from(["argrep", "foo.bar", ".", "-F"]).unwrap();
        assert!(args.fixed_strings);
    }

    #[test]
    fn fixed_strings_long_flag_is_parsed() {
        let args = Args::try_parse_from(["argrep", "foo.bar", ".", "--fixed-strings"]).unwrap();
        assert!(args.fixed_strings);
    }

    // ── -w / --word-regexp and -x / --line-regexp ───────────────────────────

    #[test]
    fn whole_word_and_whole_line_default_to_false() {
        let args = Args::try_parse_from(["argrep", "foo", "."]).unwrap();
        assert!(!args.whole_word);
        assert!(!args.whole_line);
    }

    #[test]
    fn whole_word_flag_is_parsed() {
        let args = Args::try_parse_from(["argrep", "foo", ".", "-w"]).unwrap();
        assert!(args.whole_word);
    }

    #[test]
    fn whole_line_flag_is_parsed() {
        let args = Args::try_parse_from(["argrep", "foo", ".", "-x"]).unwrap();
        assert!(args.whole_line);
    }

    #[test]
    fn whole_word_and_whole_line_can_be_combined() {
        // Unlike -c/-l, -w and -x aren't mutually exclusive: "-wx" means
        // "the whole line must consist of exactly this word", which is a
        // meaningful (if narrow) constraint, not a contradiction.
        let args = Args::try_parse_from(["argrep", "foo", ".", "-w", "-x"]).unwrap();
        assert!(args.whole_word);
        assert!(args.whole_line);
    }

    // ── -q / --quiet ─────────────────────────────────────────────────────────

    #[test]
    fn quiet_flag_defaults_to_false() {
        let args = Args::try_parse_from(["argrep", "foo", "."]).unwrap();
        assert!(!args.quiet);
    }

    #[test]
    fn quiet_short_flag_is_parsed() {
        let args = Args::try_parse_from(["argrep", "foo", ".", "-q"]).unwrap();
        assert!(args.quiet);
    }

    #[test]
    fn quiet_long_flag_is_parsed() {
        let args = Args::try_parse_from(["argrep", "foo", ".", "--quiet"]).unwrap();
        assert!(args.quiet);
    }

    #[test]
    fn quiet_can_be_combined_with_other_output_flags_at_parse_time() {
        // -q takes priority over -l/-c/-n at runtime (nothing is printed
        // either way), but there's no reason to reject the combination at
        // the CLI level — same as real grep.
        let args = Args::try_parse_from(["argrep", "foo", ".", "-q", "-l"]).unwrap();
        assert!(args.quiet);
        assert!(args.files_with_matches);
    }

    // ── -o / --only-matching ─────────────────────────────────────────────────

    #[test]
    fn only_matching_flag_defaults_to_false() {
        let args = Args::try_parse_from(["argrep", "foo", "."]).unwrap();
        assert!(!args.only_matching);
    }

    #[test]
    fn only_matching_short_flag_is_parsed() {
        let args = Args::try_parse_from(["argrep", "foo", ".", "-o"]).unwrap();
        assert!(args.only_matching);
    }

    #[test]
    fn only_matching_long_flag_is_parsed() {
        let args = Args::try_parse_from(["argrep", "foo", ".", "--only-matching"]).unwrap();
        assert!(args.only_matching);
    }

    #[test]
    fn only_matching_and_invert_together_is_rejected_at_parse_time() {
        // Unlike -w/-x, -o and -v don't have a coherent combined meaning:
        // -v selects whole lines that *don't* contain any match, so there's
        // nothing for -o to extract from them. Rejected outright rather
        // than silently doing something surprising.
        let result = Args::try_parse_from(["argrep", "foo", ".", "-o", "-v"]);
        assert!(result.is_err());
    }

    #[test]
    fn only_matching_can_be_combined_with_context_flags_at_parse_time() {
        // Unlike -o/-v, -o with -A/-B/-C isn't a CLI parse error — main()
        // warns and zeroes the context out at runtime instead, matching
        // GNU grep's own documented behavior ("these options have no
        // effect and a warning is given"). The zeroing itself happens in
        // main()'s body, not at the Args level, so this test only confirms
        // parsing succeeds; it isn't a substitute for a run-time check.
        let args = Args::try_parse_from(["argrep", "foo", ".", "-o", "-C", "2"]).unwrap();
        assert!(args.only_matching);
        assert_eq!(args.context, Some(2));
    }

    #[test]
    fn only_matching_can_be_combined_with_count_and_files_flags() {
        // -c/-l both take priority over -o at runtime (same as real grep),
        // but the combination is legal, not rejected.
        let args = Args::try_parse_from(["argrep", "foo", ".", "-o", "-c"]).unwrap();
        assert!(args.only_matching);
        assert!(args.count_per_file);
    }

    // ── -m / --max-count ─────────────────────────────────────────────────────

    #[test]
    fn max_count_defaults_to_unlimited() {
        let args = Args::try_parse_from(["argrep", "foo", "."]).unwrap();
        assert_eq!(args.max_count, None);
    }

    #[test]
    fn max_count_short_flag_is_parsed() {
        let args = Args::try_parse_from(["argrep", "foo", ".", "-m", "3"]).unwrap();
        assert_eq!(args.max_count, Some(3));
    }

    #[test]
    fn max_count_long_flag_is_parsed() {
        let args = Args::try_parse_from(["argrep", "foo", ".", "--max-count", "3"]).unwrap();
        assert_eq!(args.max_count, Some(3));
    }

    #[test]
    fn max_count_zero_is_rejected_at_parse_time() {
        // Real grep's exact behavior for -m 0 (stop before any output at
        // all) is a corner case not worth replicating precisely; rejecting
        // it outright gives a clear, unambiguous contract instead: -m N
        // always means "show N matching lines", N >= 1. Same reasoning as
        // -j 0 being rejected elsewhere in this file.
        let result = Args::try_parse_from(["argrep", "foo", ".", "-m", "0"]);
        assert!(result.is_err());
    }

    #[test]
    fn max_count_can_be_combined_with_context_and_invert() {
        let args =
            Args::try_parse_from(["argrep", "foo", ".", "-m", "2", "-C", "1", "-v"]).unwrap();
        assert_eq!(args.max_count, Some(2));
        assert_eq!(args.context, Some(1));
        assert!(args.invert);
    }

    // ── --exclude / --exclude-dir ────────────────────────────────────────────

    #[test]
    fn exclude_defaults_to_empty() {
        let args = Args::try_parse_from(["argrep", "foo", "."]).unwrap();
        assert!(args.exclude.is_empty());
    }

    #[test]
    fn exclude_can_be_given_multiple_times() {
        let args = Args::try_parse_from([
            "argrep",
            "foo",
            ".",
            "--exclude",
            "*.min.js",
            "--exclude",
            "*.lock",
        ])
        .unwrap();
        assert_eq!(args.exclude, vec!["*.min.js", "*.lock"]);
    }

    #[test]
    fn exclude_dir_can_be_given_multiple_times() {
        let args = Args::try_parse_from([
            "argrep",
            "foo",
            ".",
            "--exclude-dir",
            "node_modules",
            "--exclude-dir",
            "build*",
        ])
        .unwrap();
        assert_eq!(args.exclude_dir, vec!["node_modules", "build*"]);
    }

    #[test]
    fn ignore_still_works_as_an_alias_for_exclude_dir() {
        // --exclude-dir is the new primary name (matches grep's own
        // naming), but --ignore predates it in this codebase and keeps
        // working identically, so existing scripts/muscle memory aren't
        // broken by the rename.
        let args = Args::try_parse_from(["argrep", "foo", ".", "--ignore", "vendor"]).unwrap();
        assert_eq!(args.exclude_dir, vec!["vendor"]);
    }
}
