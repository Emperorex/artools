use argrep::{DEFAULT_IGNORES, SearchConfig, SearchStats, normalize_query, parallel_grep};
use clap::Parser;
use colored::Colorize;
use glob::Pattern;
use std::{
    collections::HashSet,
    fs,
    io::{self, BufRead, IsTerminal},
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

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

    /// Display line numbers in the output results
    #[arg(short = 'n', long)]
    line_number: bool,

    /// Number of worker threads
    #[arg(short = 'j', long, default_value_t = 4)]
    jobs: usize,

    /// Show search statistics and operational errors
    #[arg(short, long)]
    debug: bool,

    /// Invert match: print lines that do NOT contain the query
    #[arg(short = 'v', long)]
    invert: bool,

    /// Print only filenames of files that contain a match
    #[arg(short = 'l', long = "files-with-matches")]
    files_with_matches: bool,

    /// Print count of matching lines per file instead of the lines themselves
    #[arg(short = 'c', long = "count")]
    count_per_file: bool,

    /// Only search files whose names match this glob (e.g. "*.rs", "*.log")
    #[arg(long)]
    include: Option<String>,

    /// Show NUM lines of leading context before matching lines
    #[arg(short = 'B', long = "before-context")]
    before_context: Option<usize>,

    /// Show NUM lines of trailing context after matching lines
    #[arg(short = 'A', long = "after-context")]
    after_context: Option<usize>,

    /// Show NUM lines of leading and trailing context around matching lines
    #[arg(short = 'C', long = "context")]
    context: Option<usize>,

    /// Additional ignored directories
    #[arg(long)]
    ignore: Vec<String>,

    /// Do not respect .gitignore / .ignore files (search everything)
    #[arg(long = "no-ignore")]
    no_ignore: bool,
}

/// Builds the set of directory names to skip, given `--no-ignore` and any
/// explicit `--ignore` values. `--no-ignore` disables the built-in defaults
/// (`.git`, `node_modules`, `__pycache__`, `target`), but an explicit
/// `--ignore` is still honored either way, since that's the user asking for
/// something specific rather than the tool's automatic noise filtering.
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

    let include_pattern: Option<Pattern> = match &args.include {
        Some(p) => match Pattern::new(p) {
            Ok(pat) => Some(pat),
            Err(e) => {
                eprintln!(
                    "{}",
                    format!("error: Invalid glob pattern '{}': {}", p, e).red()
                );
                std::process::exit(1);
            }
        },
        None => None,
    };

    let before_context = args.before_context.or(args.context).unwrap_or(0);
    let after_context = args.after_context.or(args.context).unwrap_or(0);
    let respect_gitignore = !args.no_ignore;

    let config = Arc::new(SearchConfig {
        normalized_query: normalize_query(&args.query, args.ignore_case),
        query: args.query,
        ignore_case: args.ignore_case,
        line_number: args.line_number,
        ignore_dirs,
        debug: args.debug,
        invert: args.invert,
        files_with_matches: args.files_with_matches,
        count_per_file: args.count_per_file,
        include_pattern,
        before_context,
        after_context,
        respect_gitignore,
    });

    let stats = SearchStats::new();
    let start_time = Instant::now();

    let use_stdin = match &args.path {
        Some(p) if p == "-" => true,
        Some(_) => false,
        None => !io::stdin().is_terminal(),
    };

    if use_stdin {
        grep_stdin(&config);
    } else {
        let raw_path = args.path.as_deref().unwrap_or(".");
        let root_path = fs::canonicalize(raw_path).unwrap_or_else(|_| PathBuf::from(raw_path));

        let line_number = config.line_number;
        let query = config.query.clone();
        let files_with_matches = config.files_with_matches;
        let count_per_file = config.count_per_file;

        parallel_grep(root_path, args.jobs, config, stats.clone(), move |result| {
            print_result(
                &result,
                files_with_matches,
                count_per_file,
                line_number,
                &query,
            );
        });
    }

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

/// Reads lines from stdin and prints those matching the config query.
/// Used when argrep is invoked as part of a pipeline: cmd | argrep "pattern"
fn grep_stdin(config: &argrep::SearchConfig) {
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

    for line in stdin.lock().lines().map_while(Result::ok) {
        line_num += 1;

        let line_matches = if config.ignore_case {
            line.to_lowercase().contains(&config.normalized_query)
        } else {
            line.contains(&config.query)
        };

        let should_emit = if config.invert {
            !line_matches
        } else {
            line_matches
        };

        if should_emit {
            match_count += 1;

            if config.count_per_file {
                // accumulate — print after EOF
            } else if config.files_with_matches {
                // stdin has no filename — print "<stdin>" once then stop
                println!("{}", "<stdin>".magenta());
                break;
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

    if config.count_per_file {
        println!(
            "{}: {}",
            "<stdin>".magenta(),
            match_count.to_string().green()
        );
    }
}

fn print_stdin_line(line: &str, line_num: usize, is_context: bool, config: &argrep::SearchConfig) {
    if is_context {
        if config.line_number {
            println!("{}-{}", line_num.to_string().green(), line.trim_end());
        } else {
            println!("{}", line.trim_end());
        }
    } else {
        let highlighted = line.replace(&config.query, &config.query.red().bold().to_string());
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
    query: &str,
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
            result
                .line_content
                .replace(query, &query.red().bold().to_string())
                .trim_end()
                .to_string()
        };

        println!("{}{}{}", prefix, sep, content);
    }
}

#[cfg(test)]
mod tests {
    use super::build_ignore_dirs;

    #[test]
    fn default_ignores_included_when_not_no_ignore() {
        let dirs = build_ignore_dirs(false, vec![]);
        assert!(dirs.contains(".git"));
        assert!(dirs.contains("node_modules"));
        assert!(dirs.contains("__pycache__"));
        assert!(dirs.contains("target"));
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
