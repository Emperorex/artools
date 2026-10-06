pub mod duplicates;
pub mod report;
pub mod types;

use colored::Colorize;
use crossbeam_channel::unbounded;
use glob::Pattern;
use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};
use std::{
    cmp::{Ordering as CmpOrdering, Reverse},
    collections::{BinaryHeap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};
use types::{TypeAccumulator, TypeTable};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Default directories to ignore during scanning
pub const DEFAULT_IGNORES: &[&str] = &[".git", "node_modules", "__pycache__"];

/// A single file reported by `--largest-files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub size: u64,
    pub path: PathBuf,
}

// "Greater" means "ranks higher in the report": bigger size first, and for
// equal sizes the lexicographically smaller path first, so output is
// deterministic regardless of thread scheduling.
impl Ord for FileEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.size
            .cmp(&other.size)
            .then_with(|| other.path.cmp(&self.path))
    }
}

impl PartialOrd for FileEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

/// Bounded collection of the `limit` highest-ranked files seen so far.
///
/// Each worker thread owns one (no locking on the hot path); they are merged
/// once after the scan. Memory is O(limit * workers), independent of the
/// number of files scanned.
#[derive(Debug)]
pub struct TopFiles {
    limit: usize,
    // Min-heap on rank: the root is the entry that would be evicted next.
    heap: BinaryHeap<Reverse<FileEntry>>,
}

impl TopFiles {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            heap: BinaryHeap::new(),
        }
    }

    /// Considers a file for inclusion. Takes the path by value so rejected
    /// candidates cost nothing beyond dropping it.
    pub fn offer(&mut self, size: u64, path: PathBuf) {
        if self.limit == 0 {
            return;
        }
        if self.heap.len() >= self.limit
            && let Some(Reverse(worst)) = self.heap.peek()
            && size < worst.size
        {
            return;
        }
        self.heap.push(Reverse(FileEntry { size, path }));
        if self.heap.len() > self.limit {
            self.heap.pop();
        }
    }

    pub fn merge(&mut self, other: TopFiles) {
        for Reverse(entry) in other.heap {
            self.offer(entry.size, entry.path);
        }
    }

    /// Consumes the collection, returning entries largest first.
    pub fn into_sorted_vec(self) -> Vec<FileEntry> {
        // Ascending order of Reverse<FileEntry> is descending rank.
        self.heap
            .into_sorted_vec()
            .into_iter()
            .map(|Reverse(e)| e)
            .collect()
    }
}

/// What, besides directory sizes, a scan should record about individual files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collect {
    /// Directory sizes only.
    Nothing,
    /// Keep the `n` largest files (`--largest-files`). `Largest(0)` is
    /// equivalent to `Nothing`.
    Largest(usize),
    /// Keep every regular file whose logical length is at least `min_len`
    /// (`--duplicates`). Empty files are never kept, whatever `min_len` is:
    /// they are all trivially "identical" and not worth reporting.
    Duplicates { min_len: u64 },
    /// Count files and bytes per extension (`--by-type`). No individual
    /// files are kept.
    ByType,
}

/// Per-worker sink for individual files seen during the scan.
///
/// Each worker owns one, so recording a file never takes a lock; the
/// collectors are merged once after the scan.
#[derive(Debug)]
pub struct FileCollector {
    mode: Collect,
    /// Regular files recorded so far, whatever the mode.
    files: u64,
    top: TopFiles,
    candidates: Vec<FileEntry>,
    types: TypeAccumulator,
}

impl FileCollector {
    pub fn new(mode: Collect) -> Self {
        let limit = match mode {
            Collect::Largest(n) => n,
            _ => 0,
        };
        Self {
            mode,
            files: 0,
            top: TopFiles::new(limit),
            candidates: Vec::new(),
            types: TypeAccumulator::default(),
        }
    }

    /// Records one regular file that was counted in the directory totals.
    ///
    /// `size` is the size under the active `--apparent-size` rule, `len` is
    /// the logical length (`metadata.len()`).
    fn observe(&mut self, size: u64, len: u64, path: PathBuf) {
        self.files += 1;
        match self.mode {
            Collect::Nothing => {}
            Collect::Largest(_) => self.top.offer(size, path),
            // Content identity is a property of the logical length, so
            // candidates are keyed by `len`, not by block allocation.
            Collect::Duplicates { min_len } => {
                if len >= min_len.max(1) {
                    self.candidates.push(FileEntry { size: len, path });
                }
            }
            Collect::ByType => {
                let name = path.file_name().map(|n| n.to_string_lossy());
                self.types.add(name.as_deref().unwrap_or(""), size);
            }
        }
    }

    fn merge(&mut self, other: FileCollector) {
        self.files += other.files;
        self.top.merge(other.top);
        self.candidates.extend(other.candidates);
        self.types.merge(other.types);
    }

    /// `Largest`: the files, largest first. `Duplicates`: all candidates in
    /// arbitrary order (see [`duplicates::find_duplicates`]). `Nothing` and
    /// `ByType`: empty.
    fn into_files(self) -> Vec<FileEntry> {
        match self.mode {
            Collect::Nothing | Collect::ByType => Vec::new(),
            Collect::Largest(_) => self.top.into_sorted_vec(),
            Collect::Duplicates { .. } => self.candidates,
        }
    }
}

/// Task sent to workers representing a directory to scan
pub struct Task {
    pub path: PathBuf,
    /// Accumulated .gitignore/.ignore matchers from the root down to this
    /// directory's parent, in order (deepest = highest priority, mirroring
    /// git's own precedence for nested ignore files).
    pub ignore_stack: Arc<Vec<Arc<Gitignore>>>,
}

/// Mutable state shared by all worker threads for the duration of one scan.
///
/// Grouped into one struct so `scan_directory` stays within clippy's
/// argument-count limit and new shared state doesn't change its signature.
pub struct ScanShared {
    /// Queue of directories still to scan.
    pub task_tx: crossbeam_channel::Sender<Task>,
    /// Directories queued or in progress; the scan is done when this hits 0.
    pub active_tasks: AtomicUsize,
    /// Per-directory total cost: file bytes + directory inode cost.
    pub raw_sizes: Mutex<HashMap<PathBuf, u64>>,
    /// Per-directory file bytes only (filtered by `--include`), no inode.
    pub content_sizes: Mutex<HashMap<PathBuf, u64>>,
    /// Global (dev, ino) dedup set so hard-linked files are only counted once
    /// per invocation, mirroring GNU `du`'s behavior. Shared across all worker
    /// threads for the entire traversal, not just per-directory.
    pub seen_inodes: Mutex<HashSet<(u64, u64)>>,
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
    /// User-supplied `--exclude` patterns, compiled with gitignore semantics
    /// and rooted at the scan root. See [`build_exclude_matcher`].
    pub exclude: Option<Gitignore>,
}

/// Builds a `ScanConfig` from the given parameters.
pub fn build_config(
    ignore_dirs: HashSet<String>,
    include_pattern: Option<Pattern>,
    debug: bool,
    apparent_size: bool,
    respect_gitignore: bool,
) -> Arc<ScanConfig> {
    build_config_with_exclude(
        ignore_dirs,
        include_pattern,
        debug,
        apparent_size,
        respect_gitignore,
        None,
    )
}

/// Like [`build_config`], but also installs an `--exclude` matcher
/// (see [`build_exclude_matcher`]).
pub fn build_config_with_exclude(
    ignore_dirs: HashSet<String>,
    include_pattern: Option<Pattern>,
    debug: bool,
    apparent_size: bool,
    respect_gitignore: bool,
    exclude: Option<Gitignore>,
) -> Arc<ScanConfig> {
    Arc::new(ScanConfig {
        ignore_dirs,
        include_pattern,
        debug,
        apparent_size,
        respect_gitignore,
        exclude,
    })
}

/// Compiles `--exclude` patterns into a single matcher rooted at `root`.
///
/// Patterns use gitignore syntax and semantics:
///
/// * no `/` in the pattern (`*.log`, `target`) -> matches the name at any depth
/// * a `/` at the start or in the middle (`/foo`, `src/gen`, `target/**`)
///   -> anchored to `root`
/// * a trailing `/` (`build/`) -> directories only
/// * `X/**` -> everything *inside* `X`, but not `X` itself
/// * `!pattern` -> re-include (cannot rescue entries under an excluded dir)
///
/// Each entry is matched on its own path only (never "path or any parent"),
/// because the scanner applies the matcher at every level. That is what keeps
/// a contents-only pattern such as `target/**` or `src/**/*.rs` from
/// accidentally blocking traversal of the directory that holds the contents.
///
/// Returns `Ok(None)` for an empty pattern list.
pub fn build_exclude_matcher(
    root: &Path,
    patterns: &[String],
) -> Result<Option<Gitignore>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        if pattern.trim().is_empty() {
            return Err("--exclude pattern must not be empty".to_string());
        }
        builder
            .add_line(None, pattern)
            .map_err(|e| format!("Invalid --exclude pattern '{}': {}", pattern, e))?;
    }
    builder
        .build()
        .map(Some)
        .map_err(|e| format!("Invalid --exclude patterns: {}", e))
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
    let (raw, content, _files) = parallel_scan_with_files(root, workers, config, 0);
    (raw, content)
}

/// Like [`parallel_scan`], but additionally collects the `largest_files`
/// biggest individual files (largest first; `0` disables collection).
///
/// Candidates are exactly the files that contribute to the directory totals:
/// symlinks, `--exclude`d/`.gitignore`d paths and files rejected by
/// `--include` are never candidates, hard-linked files are listed once (under
/// whichever path the scan reached first), and sizes use the same
/// `--apparent-size` / block-allocation rule as the directory report.
pub fn parallel_scan_with_files(
    root: PathBuf,
    workers: usize,
    config: Arc<ScanConfig>,
    largest_files: usize,
) -> (HashMap<PathBuf, u64>, HashMap<PathBuf, u64>, Vec<FileEntry>) {
    let mode = if largest_files > 0 {
        Collect::Largest(largest_files)
    } else {
        Collect::Nothing
    };
    parallel_scan_collect(root, workers, config, mode)
}

/// The general form of [`parallel_scan`]: additionally records individual
/// files according to `mode` and returns them as the third tuple element
/// (see [`Collect`] for what that contains).
///
/// The same filters apply as for the directory totals: symlinks,
/// `--exclude`d/`.gitignore`d paths and files rejected by `--include` are
/// never recorded, and a hard-linked file is recorded once (under whichever
/// path the scan reached first). Only regular files are recorded.
pub fn parallel_scan_collect(
    root: PathBuf,
    workers: usize,
    config: Arc<ScanConfig>,
    mode: Collect,
) -> (HashMap<PathBuf, u64>, HashMap<PathBuf, u64>, Vec<FileEntry>) {
    let out = parallel_scan_report(root, workers, config, mode);
    (out.raw_sizes, out.content_sizes, out.files)
}

/// Everything a scan produces. See [`parallel_scan_report`].
#[derive(Debug)]
pub struct ScanOutput {
    /// Per-directory total cost: file bytes + directory inode cost.
    pub raw_sizes: HashMap<PathBuf, u64>,
    /// Per-directory file bytes only (filtered by `--include`), no inode.
    pub content_sizes: HashMap<PathBuf, u64>,
    /// Individual files recorded according to the [`Collect`] mode.
    pub files: Vec<FileEntry>,
    /// Regular files that contributed to the totals: symlinks and paths
    /// rejected by `--include`/`--exclude`/ignore rules are not counted, and
    /// a hard-linked file counts once.
    pub file_count: u64,
    /// Files and bytes per extension; empty unless the mode is
    /// [`Collect::ByType`].
    pub types: TypeTable,
}

/// Like [`parallel_scan_collect`], but returns a [`ScanOutput`] that also
/// carries the number of files counted.
pub fn parallel_scan_report(
    root: PathBuf,
    workers: usize,
    config: Arc<ScanConfig>,
    mode: Collect,
) -> ScanOutput {
    let (task_tx, task_rx) = unbounded::<Task>();
    let shared = Arc::new(ScanShared {
        task_tx,
        active_tasks: AtomicUsize::new(1),
        raw_sizes: Mutex::new(HashMap::new()),
        content_sizes: Mutex::new(HashMap::new()),
        seen_inodes: Mutex::new(HashSet::new()),
    });

    shared
        .task_tx
        .send(Task {
            path: root,
            ignore_stack: Arc::new(Vec::new()),
        })
        .unwrap();

    let mut handles = Vec::new();

    for _ in 0..workers {
        let task_rx = task_rx.clone();
        let config = Arc::clone(&config);
        let shared = Arc::clone(&shared);

        let handle = thread::spawn(move || {
            let mut collector = FileCollector::new(mode);
            loop {
                let task = crossbeam_channel::select! {
                    recv(task_rx) -> msg => match msg {
                        Ok(task) => task,
                        Err(_) => break,
                    },
                    default => {
                        if shared.active_tasks.load(Ordering::SeqCst) == 0 {
                            break;
                        }
                        thread::yield_now();
                        continue;
                    }
                };

                scan_directory(task, &config, &shared, &mut collector);
                shared.active_tasks.fetch_sub(1, Ordering::SeqCst);
            }
            collector
        });

        handles.push(handle);
    }

    let mut collected = FileCollector::new(mode);
    for handle in handles {
        collected.merge(handle.join().unwrap());
    }

    // All workers have finished and dropped their clones of `shared`.
    let Ok(shared) = Arc::try_unwrap(shared) else {
        unreachable!("all workers were joined, so no other Arc<ScanShared> can exist");
    };
    let raw = shared.raw_sizes.into_inner().unwrap();
    let content = shared.content_sizes.into_inner().unwrap();
    let file_count = collected.files;
    let types = std::mem::take(&mut collected.types).into_table();
    ScanOutput {
        raw_sizes: raw,
        content_sizes: content,
        files: collected.into_files(),
        file_count,
        types,
    }
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
    shared: &ScanShared,
    collector: &mut FileCollector,
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

        // --exclude is independent of .gitignore handling: it applies even
        // with --no-ignore, and a `!pattern` in a .gitignore can't undo it.
        // The entry is matched on its own path only; a directory that is
        // merely the parent of excluded content (e.g. `target` for
        // `target/**`) is still traversed.
        if let Some(exclude) = &config.exclude
            && exclude.matched(&entry_path, is_dir).is_ignore()
        {
            continue;
        }

        if config.respect_gitignore && is_path_ignored(&ignore_stack, &entry_path, is_dir) {
            continue;
        }

        if is_dir {
            if config.ignore_dirs.contains(file_name.as_ref()) {
                continue;
            }
            shared.active_tasks.fetch_add(1, Ordering::SeqCst);
            let _ = shared.task_tx.send(Task {
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
                            let mut seen = shared.seen_inodes.lock().unwrap();
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
                    if metadata.is_file() {
                        collector.observe(file_size, metadata.len(), entry_path);
                    }
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

    shared
        .raw_sizes
        .lock()
        .unwrap()
        .insert(dir_path.to_path_buf(), local_dir_size);
    shared
        .content_sizes
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

/// How big a size is, which decides its colour in [`format_size`].
enum SizeTier {
    Bytes,
    Kb,
    Mb,
    Gb,
    Tb,
}

fn size_text(bytes: u64) -> (SizeTier, String) {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;

    if bytes >= TB {
        (SizeTier::Tb, format!("{:.2} TB", bytes as f64 / TB as f64))
    } else if bytes >= GB {
        (SizeTier::Gb, format!("{:.2} GB", bytes as f64 / GB as f64))
    } else if bytes >= MB {
        (SizeTier::Mb, format!("{:.2} MB", bytes as f64 / MB as f64))
    } else if bytes >= KB {
        (SizeTier::Kb, format!("{:.2} KB", bytes as f64 / KB as f64))
    } else {
        (SizeTier::Bytes, format!("{} B", bytes))
    }
}

/// Formats raw bytes into human-readable strings (e.g., KB, MB, GB, TB),
/// without colour. Use this where text has to line up in columns: escape
/// codes count towards `{:>N}` padding.
pub fn format_size_plain(bytes: u64) -> String {
    size_text(bytes).1
}

/// Formats raw bytes into human-readable strings (e.g., KB, MB, GB, TB),
/// coloured by magnitude when the terminal supports it.
pub fn format_size(bytes: u64) -> String {
    let (tier, text) = size_text(bytes);
    match tier {
        SizeTier::Tb => text.magenta().bold().to_string(),
        SizeTier::Gb => text.cyan().to_string(),
        SizeTier::Mb => text.green().to_string(),
        SizeTier::Kb | SizeTier::Bytes => text,
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

    // ── TopFiles ─────────────────────────────────────────────────────────────

    fn tf_names(top: super::TopFiles) -> Vec<(u64, String)> {
        top.into_sorted_vec()
            .into_iter()
            .map(|e| (e.size, e.path.to_string_lossy().into_owned()))
            .collect()
    }

    #[test]
    fn top_files_keeps_the_largest_and_evicts_the_rest() {
        let mut top = super::TopFiles::new(2);
        for (size, name) in [(5, "a"), (50, "b"), (1, "c"), (20, "d")] {
            top.offer(size, name.into());
        }
        assert_eq!(tf_names(top), [(50, "b".into()), (20, "d".into())]);
    }

    #[test]
    fn top_files_ties_prefer_the_smaller_path_regardless_of_arrival_order() {
        for order in [["a", "b", "c"], ["c", "b", "a"], ["b", "c", "a"]] {
            let mut top = super::TopFiles::new(2);
            for name in order {
                top.offer(10, name.into());
            }
            assert_eq!(tf_names(top), [(10, "a".into()), (10, "b".into())]);
        }
    }

    #[test]
    fn top_files_limit_zero_stores_nothing() {
        let mut top = super::TopFiles::new(0);
        top.offer(10, "a".into());
        assert!(top.into_sorted_vec().is_empty());
    }

    #[test]
    fn top_files_merge_equals_single_collection() {
        let items = [(3, "a"), (9, "b"), (9, "c"), (1, "d"), (7, "e"), (7, "f")];
        let mut whole = super::TopFiles::new(3);
        let (mut left, mut right) = (super::TopFiles::new(3), super::TopFiles::new(3));
        for (i, (size, name)) in items.iter().enumerate() {
            whole.offer(*size, (*name).into());
            if i % 2 == 0 {
                left.offer(*size, (*name).into());
            } else {
                right.offer(*size, (*name).into());
            }
        }
        left.merge(right);
        assert_eq!(tf_names(left), tf_names(whole));
    }
}
