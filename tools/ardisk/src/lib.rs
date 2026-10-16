pub mod bar;
pub mod duplicates;
pub mod progress;
pub mod report;
pub mod tree;
pub mod types;

// Shared with arfind/argrep; re-exported so `ardisk::age::...` and these
// names keep working for callers and tests.
pub use arcommon::age;
pub use arcommon::ignore::{
    DEFAULT_IGNORED_DIRS as DEFAULT_IGNORES, exclude_matcher as build_exclude_matcher,
};

use arcommon::{
    fs::device_of,
    ignore::{IgnoreStack, is_ignored},
    walk::{self, Queue},
};

use colored::Colorize;
use glob::Pattern;
use ignore::gitignore::Gitignore;
use std::{
    cmp::{Ordering as CmpOrdering, Reverse},
    collections::{BinaryHeap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use types::{TypeAccumulator, TypeTable};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

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
    pub ignore_stack: IgnoreStack,
}

/// Mutable state shared by all worker threads for the duration of one scan.
///
/// Grouped into one struct so `scan_directory` stays within clippy's
/// argument-count limit and new shared state doesn't change its signature.
/// The queue of directories still to scan is the walk's own
/// ([`arcommon::walk::Queue`]).
pub struct ScanShared {
    /// Per-directory total cost: file bytes + directory inode cost.
    pub raw_sizes: Mutex<HashMap<PathBuf, u64>>,
    /// Per-directory file bytes only (filtered by `--include`), no inode.
    pub content_sizes: Mutex<HashMap<PathBuf, u64>>,
    /// Global (dev, ino) dedup set so hard-linked files are only counted once
    /// per invocation, mirroring GNU `du`'s behavior. Shared across all worker
    /// threads for the entire traversal, not just per-directory.
    pub seen_inodes: Mutex<HashSet<(u64, u64)>>,
    /// Device of the scan root when `--one-file-system` is on: directories
    /// on any other device are not entered.
    pub root_dev: Option<u64>,
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
    /// `--no-hidden`: skip files and directories whose name starts with `.`.
    /// A hidden directory is not entered, so nothing below it is scanned,
    /// whatever the names down there are. Independent of the ignore rules
    /// (`ignore_dirs`, `respect_gitignore`) and of `exclude`: an entry is
    /// scanned only if every filter lets it through. The scan root itself is
    /// never skipped, even if its own name starts with a dot.
    pub skip_hidden: bool,
    /// `--inodes`: count inodes instead of bytes, like `du --inodes`. Every
    /// entry that passes the filters costs 1: directories (the root
    /// included), regular files, symlinks and special files. A hard-linked
    /// inode is counted once per scan. `apparent_size` has no effect.
    pub inodes: bool,
    /// `-x` / `--one-file-system`: do not enter a directory that lives on a
    /// different device (filesystem, APFS volume, mount) than the scan root,
    /// like `du -x` and `find -xdev`. Such a directory is skipped entirely,
    /// the mount point included. Unix only; ignored elsewhere.
    pub one_file_system: bool,
    /// `--older-than` / `--newer-than`: only non-directory entries whose
    /// modification time passes this filter are counted (like
    /// `include_pattern`, it never stops directories from being traversed).
    pub mtime: age::MtimeFilter,
    /// `--progress`: running totals the scan updates once per directory.
    pub progress: Option<Arc<progress::ScanProgress>>,
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
        skip_hidden: false,
        inodes: false,
        one_file_system: false,
        mtime: age::MtimeFilter::default(),
        progress: None,
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
    let shared = ScanShared {
        raw_sizes: Mutex::new(HashMap::new()),
        content_sizes: Mutex::new(HashMap::new()),
        seen_inodes: Mutex::new(HashSet::new()),
        root_dev: if config.one_file_system {
            device_of(&root)
        } else {
            None
        },
    };

    let root_task = Task {
        path: root,
        ignore_stack: Arc::new(Vec::new()),
    };
    // Each worker collects files into its own FileCollector; they are merged
    // once the walk is done, so collecting needs no locking.
    let per_worker = walk::run(
        [root_task],
        workers,
        || FileCollector::new(mode),
        |collector, task, queue| scan_directory(task, &config, &shared, queue, collector),
    );

    let mut collected = FileCollector::new(mode);
    for c in per_worker {
        collected.merge(c);
    }
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

/// Whether a directory entry lives on device `dev`. An entry that cannot be
/// stat'ed counts as foreign, so `--one-file-system` never wanders into
/// something it cannot identify.
fn same_device(entry: &fs::DirEntry, dev: u64) -> bool {
    #[cfg(unix)]
    {
        entry.metadata().is_ok_and(|m| m.dev() == dev)
    }
    #[cfg(not(unix))]
    {
        let _ = (entry, dev);
        true
    }
}

/// What one entry adds to its directory's total: one inode with `--inodes`,
/// otherwise its size.
#[inline]
fn entry_cost(metadata: &fs::Metadata, config: &ScanConfig) -> u64 {
    if config.inodes {
        1
    } else {
        size_from_metadata(metadata, config.apparent_size)
    }
}

pub fn scan_directory(
    task: Task,
    config: &ScanConfig,
    shared: &ScanShared,
    queue: &Queue<Task>,
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
                    eprintln!(
                        "{}: {}: {}",
                        "ardisk".red(),
                        arcommon::display_path(dir_path),
                        err
                    );
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
    let ignore_stack: IgnoreStack = if config.respect_gitignore {
        let (stack, errors) =
            arcommon::ignore::stack_for_dir(&task.ignore_stack, dir_path, |name| {
                entries.iter().any(|e| e.file_name() == name)
            });
        if config.debug {
            for err in errors {
                eprintln!("{}", format!("ardisk: {}", err).red());
            }
        }
        stack
    } else {
        Arc::clone(&task.ignore_stack)
    };

    let mut local_content_size = 0u64; // only matching file bytes
    let mut local_files = 0u64; // regular files counted, for --progress
    let mut local_dir_size = 0u64; // files + directory inode cost

    for entry in entries {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        // Symlinks have no size of their own in the byte reports, but they do
        // occupy an inode, so only `--inodes` looks at them.
        if file_type.is_symlink() && !config.inodes {
            continue;
        }

        let os_file_name = entry.file_name();
        let file_name = os_file_name.to_string_lossy();
        let entry_path = entry.path();
        let is_dir = file_type.is_dir();

        // --no-hidden: checked on the entry's own name only. Skipping a
        // directory here means it is never queued, so its contents are not
        // scanned either. Cheap, so it runs before the pattern matchers.
        if config.skip_hidden && file_name.starts_with('.') {
            continue;
        }

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

        if config.respect_gitignore && is_ignored(&ignore_stack, &entry_path, is_dir) {
            continue;
        }

        if is_dir {
            if config.ignore_dirs.contains(file_name.as_ref()) {
                continue;
            }
            // --one-file-system: a directory on another device is a mount
            // point (or an APFS volume / firmlink target); skip it whole.
            // `entry.metadata()` does not follow symlinks, and for a mount
            // point it reports the mounted filesystem's device.
            if let Some(root_dev) = shared.root_dev
                && !same_device(&entry, root_dev)
            {
                if config.debug {
                    eprintln!(
                        "{}: {}: on another filesystem, skipped (--one-file-system)",
                        "ardisk".yellow(),
                        arcommon::display_path(&entry_path)
                    );
                }
                continue;
            }
            queue.push(Task {
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
                // --one-file-system: like `du -x`, also drop a non-directory
                // that is on another device (e.g. a bind-mounted file).
                #[cfg(unix)]
                if shared.root_dev.is_some_and(|dev| metadata.dev() != dev) {
                    continue;
                }
                // --older-than / --newer-than: by modification time. A link
                // count > 1 does not matter here: all names of an inode
                // share one mtime.
                if config.mtime.is_active() && !config.mtime.matches(metadata.modified().ok()) {
                    continue;
                }
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
                    let file_size = entry_cost(&metadata, config);
                    local_content_size += file_size;
                    local_dir_size += file_size;
                    if metadata.is_file() {
                        local_files += 1;
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
        local_dir_size += entry_cost(&dir_metadata, config);
    }

    // Same measure as the report's totals (files plus directory entries),
    // so the final counter matches the root size printed afterwards.
    if let Some(progress) = &config.progress {
        progress.add_dir(local_files, local_dir_size);
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
    paint_size(bytes, size_text(bytes).1)
}

/// Colours `text` (normally `bytes` formatted, possibly padded) the way
/// [`format_size`] colours that size.
pub fn paint_size(bytes: u64, text: String) -> String {
    let (tier, _) = size_text(bytes);
    match tier {
        SizeTier::Tb => text.magenta().bold().to_string(),
        SizeTier::Gb => text.cyan().to_string(),
        SizeTier::Mb => text.green().to_string(),
        SizeTier::Kb | SizeTier::Bytes => text,
    }
}

#[cfg(test)]
mod tests {
    //
    // Presence must be determined by the caller (from a directory listing it
    // already has) rather than by attempting to open ".gitignore"/".ignore"
    // speculatively — most directories have neither file, so at scale that
    // was a wasted open() per directory plus, with --debug on, a flood of
    // harmless "No such file" noise.

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
