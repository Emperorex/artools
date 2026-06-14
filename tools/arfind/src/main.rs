use arfind::{Args, DEFAULT_IGNORES, SearchStats, build_config, parallel_find};
use clap::Parser;
use colored::Colorize;
use glob::Pattern;
use std::{collections::HashSet, fs, path::PathBuf, sync::atomic::Ordering, time::Instant};

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

    let config = build_config(
        pattern,
        ignore_dirs,
        args.max_depth,
        args.file_type,
        args.hidden,
        args.debug,
    );

    let stats = SearchStats::new();
    let start_time = Instant::now();
    let root_path = fs::canonicalize(&args.path).unwrap_or_else(|_| PathBuf::from(&args.path));

    parallel_find(root_path, args.jobs, config, stats.clone(), |item| {
        if item.is_dir {
            println!("{}", item.path.display().to_string().blue().bold());
        } else {
            println!("{}", item.path.display());
        }
    });

    let duration = start_time.elapsed();

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
