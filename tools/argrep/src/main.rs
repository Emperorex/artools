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
}

fn main() {
    let args = Args::parse();

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();

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
    });

    let stats = SearchStats::new();
    let start_time = Instant::now();

    // If stdin is a pipe (not a terminal), read from it directly instead of
    // traversing the filesystem — mirrors grep behaviour with piped input.
    if !io::stdin().is_terminal() {
        grep_stdin(&config);
    } else {
        let root_path = fs::canonicalize(&args.path).unwrap_or_else(|_| PathBuf::from(&args.path));

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
                let highlighted =
                    line.replace(&config.query, &config.query.red().bold().to_string());
                if config.line_number {
                    println!("{}:{}", line_num.to_string().green(), highlighted);
                } else {
                    println!("{}", highlighted);
                }
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

/// Shared output formatter for parallel_grep results.
fn print_result(
    result: &argrep::MatchResult,
    files_with_matches: bool,
    count_per_file: bool,
    line_number: bool,
    query: &str,
) {
    if files_with_matches {
        println!("{}", result.file_path.display().to_string().magenta());
    } else if count_per_file {
        println!(
            "{}: {}",
            result.file_path.display().to_string().magenta(),
            result.count.unwrap_or(0).to_string().green()
        );
    } else {
        let prefix = if line_number {
            format!(
                "{}:{}",
                result.file_path.display().to_string().magenta(),
                result.line_num.to_string().green()
            )
        } else {
            result.file_path.display().to_string().magenta().to_string()
        };
        let highlighted = result
            .line_content
            .replace(query, &query.red().bold().to_string());
        println!("{}: {}", prefix, highlighted.trim_end());
    }
}
