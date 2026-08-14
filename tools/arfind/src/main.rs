use arfind::{Args, DEFAULT_IGNORES, SearchConfig, SearchStats, SizeFilter, parallel_find};
use clap::Parser;
use colored::Colorize;
use glob::Pattern;
use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Instant,
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

    Ok((value * multiplier) as u64)
}

fn main() {
    let args = Args::parse();

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

    let mut ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    ignore_dirs.extend(args.ignore);

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
    });

    let stats = SearchStats::new();
    let start_time = Instant::now();
    let root_path = fs::canonicalize(&args.path).unwrap_or_else(|_| PathBuf::from(&args.path));
    let count_only = args.count;

    parallel_find(root_path, args.jobs, config, stats.clone(), move |item| {
        if !count_only {
            if item.is_dir {
                println!("{}", item.path.display().to_string().blue().bold());
            } else {
                println!("{}", item.path.display());
            }
        }
    });

    let duration = start_time.elapsed();

    // --count: print total matches after all workers finish
    if count_only {
        println!("{}", stats.matched_count.load(Ordering::Relaxed));
    }

    if args.debug {
        eprintln!("{}", "\n=== Search Statistics ===".yellow().bold());
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
}

#[cfg(test)]
mod tests {
    use super::{parse_size, parse_size_bytes};

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
}
