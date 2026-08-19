use clap::Parser;
use colored::Colorize;
use crossbeam_channel::{Sender, unbounded};
use glob::Pattern;
use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

pub const DEFAULT_IGNORES: &[&str] = &[".git", "node_modules", "__pycache__"];

/// CLI arguments
#[derive(Parser, Debug)]
#[command(author, version, about = "Fast parallel find (Rust version)")]
pub struct Args {
    /// Root directory
    #[arg(default_value = ".")]
    pub path: String,

    /// Filename pattern
    #[arg(short, long, default_value = "*")]
    pub name: String,

    /// Case-insensitive pattern matching
    #[arg(short = 'i', long = "case-insensitive")]
    pub case_insensitive: bool,

    /// Number of worker threads
    #[arg(short = 'j', long, default_value_t = 4)]
    pub jobs: usize,

    /// Maximum recursion depth
    #[arg(long)]
    pub max_depth: Option<usize>,

    /// Additional ignored directories
    #[arg(long)]
    pub ignore: Vec<String>,

    /// Do not respect .gitignore / .ignore files (search everything)
    #[arg(long = "no-ignore")]
    pub no_ignore: bool,

    /// Filter by type: f (file), d (directory), or l (symlink)
    #[arg(short = 't', long)]
    pub file_type: Option<String>,

    #[arg(short = 'H', long)]
    pub hidden: bool,

    #[arg(short, long)]
    pub debug: bool,

    /// Filter by file size: N or +N (larger than), -N (smaller than).
    /// Supported units: B, KB, MB, GB, TB. Examples: 100MB, +100MB, -1KB
    #[arg(long, allow_hyphen_values = true)]
    pub size: Option<String>,

    /// Only match empty files (size 0) or empty directories (no children)
    #[arg(short = 'e', long)]
    pub empty: bool,

    /// Print only the total count of matches instead of paths
    #[arg(short = 'c', long)]
    pub count: bool,
}

/// Task sent to workers
pub struct Task {
    pub path: PathBuf,
    pub depth: usize,
    /// Accumulated .gitignore/.ignore matchers from the root down to this
    /// directory's parent, in order (deepest = highest priority, mirroring
    /// git's own precedence for nested ignore files).
    pub ignore_stack: Arc<Vec<Arc<Gitignore>>>,
}

impl std::fmt::Debug for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Task")
            .field("path", &self.path)
            .field("depth", &self.depth)
            .field("ignore_stack_len", &self.ignore_stack.len())
            .finish()
    }
}

/// Size filter with optional lower and upper bounds (in bytes).
/// Mirrors find's +N (greater than) and -N (less than) semantics.
pub struct SizeFilter {
    pub min: Option<u64>, // +N  → size > N
    pub max: Option<u64>, // -N  → size < N
}

/// Shared configuration and filters for the search
pub struct SearchConfig {
    pub pattern: Pattern,
    pub ignore_dirs: HashSet<String>,
    pub max_depth: Option<usize>,
    pub file_type: Option<String>,
    pub hidden: bool,
    pub debug: bool,
    pub size_filter: Option<SizeFilter>,
    pub empty_only: bool,
    pub case_insensitive: bool,
    pub respect_gitignore: bool,
}

/// Shared atomic counters for runtime statistics
#[derive(Clone)]
pub struct SearchStats {
    pub total_files: Arc<AtomicUsize>,
    pub total_dirs: Arc<AtomicUsize>,
    pub matched_count: Arc<AtomicUsize>,
}

impl SearchStats {
    pub fn new() -> Self {
        Self {
            total_files: Arc::new(AtomicUsize::new(0)),
            total_dirs: Arc::new(AtomicUsize::new(0)),
            matched_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Default for SearchStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Output item representing a match with its type metadata
pub struct MatchResult {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// Tests the search root itself against the same criteria used for
/// descendants in `scan_directory`. Without this, `arfind foo --name foo`
/// would only ever inspect `foo`'s *children* and could never report `foo`
/// itself, even though it's a perfectly valid candidate — mirroring how
/// `find foo -name foo` also matches the starting point.
///
/// Unlike descendants, the root is never skipped for being "hidden": the
/// user named it explicitly on the command line, so `--hidden` doesn't come
/// into play here (matching `find`'s treatment of explicit start points).
fn check_root(root: &Path, config: &SearchConfig) -> Option<MatchResult> {
    let file_name = root.file_name()?.to_string_lossy();

    let match_options = glob::MatchOptions {
        case_sensitive: !config.case_insensitive,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };

    if !config.pattern.matches_with(&file_name, match_options) {
        return None;
    }

    let metadata = fs::symlink_metadata(root).ok()?;
    let is_symlink = metadata.file_type().is_symlink();
    // symlink_metadata reports the link itself; resolve through it for
    // is_dir() the same way read_dir's per-entry file_type() would.
    let is_dir = if is_symlink {
        fs::metadata(root).map(|m| m.is_dir()).unwrap_or(false)
    } else {
        metadata.is_dir()
    };

    if let Some(t) = &config.file_type {
        let type_matches = (t == "f" && !is_dir && !is_symlink)
            || (t == "d" && is_dir)
            || (t == "l" && is_symlink);
        if !type_matches {
            return None;
        }
    }

    if config.empty_only {
        let is_empty = if is_dir {
            fs::read_dir(root)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
        } else {
            metadata.len() == 0
        };
        if !is_empty {
            return None;
        }
    }

    if !is_dir
        && !is_symlink
        && let Some(filter) = &config.size_filter
    {
        let len = metadata.len();
        let size_ok =
            filter.min.is_none_or(|min| len > min) && filter.max.is_none_or(|max| len < max);
        if !size_ok {
            return None;
        }
    }

    Some(MatchResult {
        path: root.to_path_buf(),
        is_dir,
    })
}

pub fn parallel_find(
    root: PathBuf,
    workers: usize,
    config: Arc<SearchConfig>,
    stats: SearchStats,
    on_match: impl Fn(MatchResult) + Send + 'static,
) {
    let (task_tx, task_rx) = unbounded::<Task>();
    let (output_tx, output_rx) = unbounded::<MatchResult>();

    let active_tasks = Arc::new(AtomicUsize::new(1));

    if let Some(result) = check_root(&root, &config) {
        stats.matched_count.fetch_add(1, Ordering::Relaxed);
        let _ = output_tx.send(result);
    }

    task_tx
        .send(Task {
            path: root,
            depth: 0,
            ignore_stack: Arc::new(Vec::new()),
        })
        .unwrap();

    let mut handles = Vec::new();

    for _ in 0..workers {
        let task_rx = task_rx.clone();
        let task_tx = task_tx.clone();
        let output_tx = output_tx.clone();

        let config = Arc::clone(&config);
        let stats = stats.clone();
        let active_tasks = Arc::clone(&active_tasks);

        let handle = thread::spawn(move || {
            loop {
                let task = crossbeam_channel::select! {
                    recv(task_rx) -> msg => match msg {
                        Ok(task) => task,
                        Err(_) => break,
                    },
                    default => {
                        if active_tasks.load(Ordering::SeqCst) == 0 {
                            break;
                        }
                        thread::yield_now();
                        continue;
                    }
                };

                scan_directory(task, &config, &task_tx, &output_tx, &active_tasks, &stats);
                active_tasks.fetch_sub(1, Ordering::SeqCst);
            }
        });

        handles.push(handle);
    }

    drop(task_tx);
    drop(output_tx);

    let printer_handle = thread::spawn(move || {
        for item in output_rx {
            on_match(item);
        }
    });

    for handle in handles {
        handle.join().unwrap();
    }

    printer_handle.join().unwrap();
}

/// Builds a single combined matcher from any `.gitignore`/`.ignore` files
/// present directly in `dir`. Returns `None` if neither file exists (or
/// exists but contributes zero patterns), so callers can skip extending the
/// ignore stack for the common case of a directory with no ignore files.
fn build_dir_gitignore(dir: &Path, debug: bool) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(dir);

    for filename in [".gitignore", ".ignore"] {
        if let Some(err) = builder.add(dir.join(filename))
            && debug
        {
            eprintln!("{}", format!("arfind: {}", err).red());
        }
    }

    match builder.build() {
        Ok(gi) if gi.num_ignores() > 0 || gi.num_whitelists() > 0 => Some(gi),
        Ok(_) => None,
        Err(err) => {
            if debug {
                eprintln!("{}", format!("arfind: {}", err).red());
            }
            None
        }
    }
}

/// Checks `path` against a stack of gitignore matchers ordered root-to-leaf.
/// Later (deeper) matchers take priority over earlier ones, so a subdirectory's
/// `.gitignore` can re-include (`!pattern`) something an ancestor ignored —
/// matching git's own precedence for nested ignore files.
fn is_path_ignored(stack: &[Arc<Gitignore>], path: &Path, is_dir: bool) -> bool {
    let mut ignored = false;
    for matcher in stack {
        match matcher.matched(path, is_dir) {
            Match::Ignore(_) => ignored = true,
            Match::Whitelist(_) => ignored = false,
            Match::None => {}
        }
    }
    ignored
}

pub fn scan_directory(
    task: Task,
    config: &SearchConfig,
    task_tx: &Sender<Task>,
    output_tx: &Sender<MatchResult>,
    active_tasks: &AtomicUsize,
    stats: &SearchStats,
) {
    stats.total_dirs.fetch_add(1, Ordering::Relaxed);

    // Computed once per directory (not per-entry) since it's constant for the
    // whole search.
    let match_options = glob::MatchOptions {
        case_sensitive: !config.case_insensitive,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };

    // Extend the inherited gitignore stack with this directory's own
    // .gitignore/.ignore, if present. Computed once per directory, not
    // per-entry — only pays the extra file reads when --no-ignore isn't set.
    let ignore_stack: Arc<Vec<Arc<Gitignore>>> = if config.respect_gitignore {
        match build_dir_gitignore(&task.path, config.debug) {
            Some(gi) => {
                let mut stack = (*task.ignore_stack).clone();
                stack.push(Arc::new(gi));
                Arc::new(stack)
            }
            None => Arc::clone(&task.ignore_stack),
        }
    } else {
        Arc::clone(&task.ignore_stack)
    };

    let entries = match fs::read_dir(&task.path) {
        Ok(entries) => entries,
        Err(err) => {
            if config.debug {
                eprintln!(
                    "{}",
                    format!("arfind: {}: {}", task.path.display(), err).red()
                );
            }
            return;
        }
    };

    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        let is_symlink = file_type.is_symlink();
        let is_dir = file_type.is_dir();

        let os_file_name = entry.file_name();
        let file_name = os_file_name.to_string_lossy();

        if !config.hidden && file_name.starts_with('.') {
            continue;
        }

        let entry_path = entry.path();

        if config.respect_gitignore && is_path_ignored(&ignore_stack, &entry_path, is_dir) {
            continue;
        }

        if !is_dir {
            stats.total_files.fetch_add(1, Ordering::Relaxed);
        }

        if config.pattern.matches_with(&file_name, match_options) {
            let type_matches = match &config.file_type {
                Some(t) => {
                    (t == "f" && !is_dir && !is_symlink)
                        || (t == "d" && is_dir)
                        || (t == "l" && is_symlink)
                }
                None => true,
            };

            if type_matches {
                // Metadata is only fetched lazily, at most once per entry, and
                // reused by both the --empty and --size checks below (they used
                // to each call entry.metadata() independently).
                let mut metadata_cache: Option<fs::Metadata> = None;

                // --empty: only pay the extra syscall (read_dir/metadata) when
                // the flag is actually set; previously this ran unconditionally
                // for every matched entry.
                if config.empty_only {
                    let is_empty = if is_dir {
                        // A directory is empty if it has no children
                        fs::read_dir(&entry_path)
                            .map(|mut d| d.next().is_none())
                            .unwrap_or(false)
                    } else {
                        if metadata_cache.is_none() {
                            metadata_cache = entry.metadata().ok();
                        }
                        metadata_cache
                            .as_ref()
                            .map(|m| m.len() == 0)
                            .unwrap_or(false)
                    };

                    if !is_empty {
                        continue;
                    }
                }

                // --size: apply size filter to files only (dirs have no single size)
                let size_ok = if !is_dir && !is_symlink {
                    match &config.size_filter {
                        Some(f) => {
                            if metadata_cache.is_none() {
                                metadata_cache = entry.metadata().ok();
                            }
                            let len = metadata_cache.as_ref().map(|m| m.len()).unwrap_or(0);
                            f.min.is_none_or(|min| len > min) && f.max.is_none_or(|max| len < max)
                        }
                        None => true,
                    }
                } else {
                    true // size filter does not apply to directories or symlinks
                };

                if size_ok {
                    stats.matched_count.fetch_add(1, Ordering::Relaxed);
                    let _ = output_tx.send(MatchResult {
                        path: entry_path.clone(),
                        is_dir,
                    });
                }
            }
        }

        if is_dir && !is_symlink {
            if config.ignore_dirs.contains(file_name.as_ref()) {
                continue;
            }

            if config.max_depth.is_none_or(|d| task.depth < d) {
                active_tasks.fetch_add(1, Ordering::SeqCst);
                let _ = task_tx.send(Task {
                    path: entry_path,
                    depth: task.depth + 1,
                    ignore_stack: Arc::clone(&ignore_stack),
                });
            }
        }
    }
}
