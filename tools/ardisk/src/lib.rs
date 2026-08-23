use colored::Colorize;
use crossbeam_channel::unbounded;
use glob::Pattern;
use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Default directories to ignore during scanning
pub const DEFAULT_IGNORES: &[&str] = &[".git", "node_modules", "__pycache__"];

/// Task sent to workers representing a directory to scan
pub struct Task {
    pub path: PathBuf,
    /// Accumulated .gitignore/.ignore matchers from the root down to this
    /// directory's parent, in order (deepest = highest priority, mirroring
    /// git's own precedence for nested ignore files).
    pub ignore_stack: Arc<Vec<Arc<Gitignore>>>,
}

/// Configuration shared across worker threads
pub struct ScanConfig {
    pub ignore_dirs: HashSet<String>,
    /// When set, only files whose names match this glob pattern contribute to
    /// directory sizes. Directories themselves are always traversed regardless.
    pub include_pattern: Option<Pattern>,
    pub debug: bool,
    /// When true, use logical file size (metadata.len()) matching du -sh.
    /// When false (default), use physical block allocation (blocks * 512).
    pub apparent_size: bool,
    /// Do not respect .gitignore / .ignore files (scan everything)
    pub respect_gitignore: bool,
}

/// Builds a `ScanConfig` from the given parameters.
pub fn build_config(
    ignore_dirs: HashSet<String>,
    include_pattern: Option<Pattern>,
    debug: bool,
    apparent_size: bool,
    respect_gitignore: bool,
) -> Arc<ScanConfig> {
    Arc::new(ScanConfig {
        ignore_dirs,
        include_pattern,
        debug,
        apparent_size,
        respect_gitignore,
    })
}

/// Runs a parallel scan rooted at `root` and returns two maps of raw
/// per-directory sizes (immediate files only — not yet rolled up):
///
/// * `raw_sizes`     – total cost: file bytes + directory inode cost
/// * `content_sizes` – file bytes only (filtered by `--include`), no inode
pub fn parallel_scan(
    root: PathBuf,
    workers: usize,
    config: Arc<ScanConfig>,
) -> (HashMap<PathBuf, u64>, HashMap<PathBuf, u64>) {
    let (task_tx, task_rx) = unbounded::<Task>();
    let active_tasks = Arc::new(AtomicUsize::new(1));
    let raw_sizes = Arc::new(Mutex::new(HashMap::<PathBuf, u64>::new()));
    let content_sizes = Arc::new(Mutex::new(HashMap::<PathBuf, u64>::new()));
    // Global (dev, ino) dedup set so hard-linked files are only counted once
    // per invocation, mirroring GNU `du`'s behavior. Shared across all worker
    // threads for the entire traversal, not just per-directory.
    let seen_inodes = Arc::new(Mutex::new(HashSet::<(u64, u64)>::new()));

    task_tx
        .send(Task {
            path: root,
            ignore_stack: Arc::new(Vec::new()),
        })
        .unwrap();

    let mut handles = Vec::new();

    for _ in 0..workers {
        let task_rx = task_rx.clone();
        let task_tx = task_tx.clone();
        let config = Arc::clone(&config);
        let raw_sizes = Arc::clone(&raw_sizes);
        let content_sizes = Arc::clone(&content_sizes);
        let active_tasks = Arc::clone(&active_tasks);
        let seen_inodes = Arc::clone(&seen_inodes);

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

                scan_directory(
                    task,
                    &config,
                    &task_tx,
                    &active_tasks,
                    &raw_sizes,
                    &content_sizes,
                    &seen_inodes,
                );
                active_tasks.fetch_sub(1, Ordering::SeqCst);
            }
        });

        handles.push(handle);
    }

    drop(task_tx);

    for handle in handles {
        handle.join().unwrap();
    }

    let raw = Arc::try_unwrap(raw_sizes).unwrap().into_inner().unwrap();
    let content = Arc::try_unwrap(content_sizes)
        .unwrap()
        .into_inner()
        .unwrap();
    (raw, content)
}

/// Computes the on-disk contribution of a single metadata entry, honoring
/// `apparent_size` (logical length) vs. physical block allocation.
#[inline]
fn size_from_metadata(metadata: &fs::Metadata, apparent_size: bool) -> u64 {
    #[cfg(unix)]
    {
        if apparent_size {
            metadata.len()
        } else {
            metadata.blocks() * 512
        }
    }
    #[cfg(not(unix))]
    {
        let _ = apparent_size;
        metadata.len()
    }
}

/// Builds a single combined matcher from any `.gitignore`/`.ignore` files
/// present directly in `dir`. Returns `None` if neither file exists (or
/// exists but contributes zero patterns), so callers can skip extending the
/// ignore stack for the common case of a directory with no ignore files.
///
/// `present_files` must list only filenames the caller has already
/// confirmed exist in `dir` (from a directory listing it already has) —
/// this function never attempts to open a file that isn't there. Most
/// directories have neither `.gitignore` nor `.ignore`, so at scale that
/// avoids both a wasted open() per directory and, when `--debug` is on, a
/// flood of harmless "No such file" pseudo-errors for the common case.
fn build_dir_gitignore(dir: &Path, present_files: &[&str], debug: bool) -> Option<Gitignore> {
    if present_files.is_empty() {
        return None;
    }

    let mut builder = GitignoreBuilder::new(dir);

    for filename in present_files {
        if let Some(err) = builder.add(dir.join(filename))
            && debug
        {
            eprintln!("{}", format!("ardisk: {}", err).red());
        }
    }

    match builder.build() {
        Ok(gi) if gi.num_ignores() > 0 || gi.num_whitelists() > 0 => Some(gi),
        Ok(_) => None,
        Err(err) => {
            if debug {
                eprintln!("{}", format!("ardisk: {}", err).red());
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
    config: &ScanConfig,
    task_tx: &crossbeam_channel::Sender<Task>,
    active_tasks: &AtomicUsize,
    raw_sizes: &Mutex<HashMap<PathBuf, u64>>,
    content_sizes: &Mutex<HashMap<PathBuf, u64>>,
    seen_inodes: &Mutex<HashSet<(u64, u64)>>,
) {
    let dir_path = task.path.as_path();

    // Retry on EINTR — macOS interrupts syscalls with signals from system
    // processes (Spotlight, sandboxd, etc.). Safe to retry unconditionally.
    let entries: Vec<_> = loop {
        match fs::read_dir(dir_path) {
            Ok(entries) => break entries.flatten().collect(),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                if config.debug {
                    eprintln!("{}: {}: {}", "ardisk".red(), dir_path.display(), err);
                }
                return;
            }
        }
    };

    // Extend the inherited gitignore stack with this directory's own
    // .gitignore/.ignore, if present. We already have this directory's full
    // listing above, so check presence against that instead of attempting
    // to open files that, for the overwhelming majority of directories,
    // aren't there — avoids both a wasted syscall and (with --debug) a
    // flood of harmless "No such file" noise for every directory scanned.
    let ignore_stack: Arc<Vec<Arc<Gitignore>>> = if config.respect_gitignore {
        let present_ignore_files: Vec<&str> = [".gitignore", ".ignore"]
            .into_iter()
            .filter(|name| {
                entries
                    .iter()
                    .any(|e| e.file_name().as_os_str() == std::ffi::OsStr::new(name))
            })
            .collect();

        match build_dir_gitignore(dir_path, &present_ignore_files, config.debug) {
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

    let mut local_content_size = 0u64; // only matching file bytes
    let mut local_dir_size = 0u64; // files + directory inode cost

    for entry in entries {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if file_type.is_symlink() {
            continue;
        }

        let os_file_name = entry.file_name();
        let file_name = os_file_name.to_string_lossy();
        let entry_path = entry.path();
        let is_dir = file_type.is_dir();

        if config.respect_gitignore && is_path_ignored(&ignore_stack, &entry_path, is_dir) {
            continue;
        }

        if is_dir {
            if config.ignore_dirs.contains(file_name.as_ref()) {
                continue;
            }
            active_tasks.fetch_add(1, Ordering::SeqCst);
            let _ = task_tx.send(Task {
                path: entry_path,
                ignore_stack: Arc::clone(&ignore_stack),
            });
        } else {
            // If --include is set, skip files that don't match the pattern
            if let Some(pattern) = &config.include_pattern
                && !pattern.matches(&file_name)
            {
                continue;
            }

            if let Ok(metadata) = entry.metadata() {
                // Hard-link dedup: if this inode has multiple links, only
                // count its size the first time we see it across the whole
                // traversal (GNU `du` semantics for a single invocation).
                // Skip the lookup entirely for the common case (nlink == 1)
                // to avoid needless mutex contention.
                let already_counted = {
                    #[cfg(unix)]
                    {
                        if metadata.nlink() > 1 {
                            let key = (metadata.dev(), metadata.ino());
                            let mut seen = seen_inodes.lock().unwrap();
                            !seen.insert(key)
                        } else {
                            false
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        false
                    }
                };

                if !already_counted {
                    let file_size = size_from_metadata(&metadata, config.apparent_size);
                    local_content_size += file_size;
                    local_dir_size += file_size;
                }
            }
        }
    }

    // Count the directory's own inode/entry size, if possible.
    // This goes only into the total map, NOT the content map, so that
    // main.rs can still suppress dirs with no matching file content.
    if let Ok(dir_metadata) = fs::metadata(dir_path) {
        local_dir_size += size_from_metadata(&dir_metadata, config.apparent_size);
    }

    raw_sizes
        .lock()
        .unwrap()
        .insert(dir_path.to_path_buf(), local_dir_size);
    content_sizes
        .lock()
        .unwrap()
        .insert(dir_path.to_path_buf(), local_content_size);
}

/// Propagates weights from deeply nested folders up the tree
/// using a single-pass dynamic programming bottom-up rollup.
pub fn aggregate_sizes(
    raw_sizes: &HashMap<PathBuf, u64>,
    base_path: &Path,
) -> HashMap<PathBuf, u64> {
    let mut aggregated = HashMap::new();

    let mut paths: Vec<&PathBuf> = raw_sizes.keys().collect();
    paths.sort_by_key(|p| std::cmp::Reverse(p.components().count()));

    for path in paths {
        let size = raw_sizes[path];

        *aggregated.entry(path.to_path_buf()).or_insert(0u64) += size;

        if let Some(parent) = path.parent()
            && parent.starts_with(base_path)
        {
            let child_accumulated_size = *aggregated.get(path).unwrap_or(&0u64);
            *aggregated.entry(parent.to_path_buf()).or_insert(0u64) += child_accumulated_size;
        }
    }

    aggregated
}

/// Formats raw bytes into human-readable strings (e.g., KB, MB, GB, TB)
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;

    if bytes >= TB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
            .magenta()
            .bold()
            .to_string()
    } else if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
            .cyan()
            .to_string()
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
            .green()
            .to_string()
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64).to_string()
    } else {
        format!("{} B", bytes).to_string()
    }
}

#[cfg(test)]
mod tests {
    // ── build_dir_gitignore ──────────────────────────────────────────────────
    //
    // Presence must be determined by the caller (from a directory listing it
    // already has) rather than by attempting to open ".gitignore"/".ignore"
    // speculatively — most directories have neither file, so at scale that
    // was a wasted open() per directory plus, with --debug on, a flood of
    // harmless "No such file" noise.

    #[test]
    fn build_dir_gitignore_touches_nothing_when_no_files_present() {
        use super::build_dir_gitignore;
        use std::path::Path;

        let result = build_dir_gitignore(Path::new("/definitely/does/not/exist"), &[], true);
        assert!(result.is_none());
    }

    #[test]
    fn build_dir_gitignore_builds_from_present_gitignore() {
        use super::build_dir_gitignore;
        use std::fs;

        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join(".gitignore"), b"*.log\n").unwrap();

        let gi = build_dir_gitignore(dir.path(), &[".gitignore"], false);
        assert!(gi.is_some());
    }

    #[test]
    fn build_dir_gitignore_ignores_unlisted_files_even_if_present() {
        use super::build_dir_gitignore;
        use std::fs;

        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join(".gitignore"), b"*.log\n").unwrap();
        fs::write(dir.path().join(".ignore"), b"*.tmp\n").unwrap();

        let gi = build_dir_gitignore(dir.path(), &[".gitignore"], false).unwrap();
        assert!(matches!(
            gi.matched(dir.path().join("a.log"), false),
            super::Match::Ignore(_)
        ));
        assert!(matches!(
            gi.matched(dir.path().join("a.tmp"), false),
            super::Match::None
        ));
    }

    #[test]
    fn build_dir_gitignore_returns_none_for_empty_ignore_file() {
        use super::build_dir_gitignore;
        use std::fs;

        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join(".gitignore"), b"# just a comment\n").unwrap();

        let gi = build_dir_gitignore(dir.path(), &[".gitignore"], false);
        assert!(gi.is_none());
    }
}
