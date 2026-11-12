pub mod bar;
pub mod diff;
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

use arcommon::color::ErrPaint;
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
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
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

    /// Considers a file for inclusion.
    pub fn offer(&mut self, size: u64, path: PathBuf) {
        self.offer_with(size, || path);
    }

    /// Like [`offer`](Self::offer), but builds the path only for a file
    /// that is kept: most files of a big scan are rejected on size alone.
    pub fn offer_with(&mut self, size: u64, path: impl FnOnce() -> PathBuf) {
        if self.limit == 0 {
            return;
        }
        if self.heap.len() >= self.limit
            && let Some(Reverse(worst)) = self.heap.peek()
            && size < worst.size
        {
            return;
        }
        self.heap.push(Reverse(FileEntry { size, path: path() }));
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
    /// Records one file. `path` builds its full path, and is called only
    /// by the modes that report paths (`--largest-files`, `--duplicates`
    /// for files big enough): in the default mode a scan of a million
    /// files would otherwise allocate a million paths only to drop them.
    fn observe(&mut self, size: u64, len: u64, name: &str, path: impl FnOnce() -> PathBuf) {
        self.files += 1;
        match self.mode {
            Collect::Nothing => {}
            Collect::Largest(_) => self.top.offer_with(size, path),
            // Content identity is a property of the logical length, so
            // candidates are keyed by `len`, not by block allocation.
            Collect::Duplicates { min_len } => {
                if len >= min_len.max(1) {
                    self.candidates.push(FileEntry {
                        size: len,
                        path: path(),
                    });
                }
            }
            Collect::ByType => self.types.add(name, size),
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

/// One scanned directory.
///
/// A scan lists them in the order their scans started ([`ScanOutput::dirs`]),
/// so a directory always comes after its parent, and `parent` is a position
/// in that same list. That is what lets [`roll_up`] total every subtree in
/// one pass, with no sorting and no path lookups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirNode {
    pub path: PathBuf,
    /// Position of the parent directory in the list; `None` for the scan
    /// root.
    pub parent: Option<usize>,
    /// Total cost: file bytes + directory inode cost. After a scan, of what
    /// the directory holds directly; after [`roll_up`], of its whole subtree.
    pub size: u64,
    /// File bytes only (filtered by `--include`), no inode. Direct after a
    /// scan, whole subtree after [`roll_up`], like `size`.
    pub content: u64,
}

/// Task sent to workers representing a directory to scan
pub struct Task {
    pub path: PathBuf,
    /// Position of the parent directory in the scan's list (see
    /// [`DirNode`]); `None` for the scan root.
    pub parent: Option<usize>,
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
    /// The position the next directory gets, handed out when its scan
    /// starts: before any of its subdirectories is queued, so a child's
    /// position is always greater than its parent's.
    pub next_dir: AtomicUsize,
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
    /// `--inodes`: count inodes instead of bytes. Every
    /// entry that passes the filters costs 1: directories (the root
    /// included), regular files, symlinks and special files. A hard-linked
    /// inode is counted once per scan. `apparent_size` has no effect.
    pub inodes: bool,
    /// `-x` / `--one-file-system`: do not enter a directory that lives on a
    /// different device (filesystem, APFS volume, mount) than the scan root,
    /// like `find -xdev`. Such a directory is skipped entirely,
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
    (out.raw_sizes(), out.content_sizes(), out.files)
}

/// Everything a scan produces. See [`parallel_scan_report`].
#[derive(Debug)]
pub struct ScanOutput {
    /// Every directory scanned, parents before children, each with what it
    /// holds directly (not yet rolled up; see [`roll_up`]). The scan root is
    /// first. A directory that could not be read is not listed.
    pub dirs: Vec<DirNode>,
    /// Directories that could not be listed and entries whose details
    /// could not be read: what they hold is missing from the totals.
    pub read_errors: usize,
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

impl ScanOutput {
    /// Per-directory total cost (file bytes + directory inode cost) as a
    /// map, for callers that look directories up by path.
    pub fn raw_sizes(&self) -> HashMap<PathBuf, u64> {
        self.dirs.iter().map(|d| (d.path.clone(), d.size)).collect()
    }

    /// Per-directory file bytes only (filtered by `--include`), no inode,
    /// as a map.
    pub fn content_sizes(&self) -> HashMap<PathBuf, u64> {
        self.dirs
            .iter()
            .map(|d| (d.path.clone(), d.content))
            .collect()
    }
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
        next_dir: AtomicUsize::new(0),
        root_dev: if config.one_file_system {
            device_of(&root)
        } else {
            None
        },
    };

    let root_task = Task {
        path: root,
        parent: None,
        ignore_stack: Arc::new(Vec::new()),
    };
    // Each worker records its directories and files in its own ScanWorker;
    // they are merged once the walk is done, so recording needs no locking.
    let per_worker = walk::run(
        [root_task],
        workers,
        || ScanWorker::new(mode),
        |worker, task, queue| scan_directory(task, &config, &shared, queue, worker),
    );

    let mut collected = FileCollector::new(mode);
    let mut dirs = Vec::with_capacity(per_worker.iter().map(|w| w.dirs.len()).sum());
    let mut linked = Vec::new();
    let mut read_errors = 0;
    for worker in per_worker {
        collected.merge(worker.files);
        dirs.extend(worker.dirs);
        linked.extend(worker.linked);
        read_errors += worker.read_errors;
    }
    let mut dirs = in_position_order(dirs);
    credit_linked(
        linked,
        &mut dirs,
        &mut collected,
        config.progress.as_deref(),
    );
    let file_count = collected.files;
    let types = std::mem::take(&mut collected.types).into_table();
    ScanOutput {
        dirs,
        read_errors,
        files: collected.into_files(),
        file_count,
        types,
    }
}

/// What one scan worker records, merged with the other workers' once the
/// walk is done (see [`parallel_scan_report`]).
#[derive(Debug)]
pub struct ScanWorker {
    /// Files, as the scan's [`Collect`] mode asks.
    pub files: FileCollector,
    /// Each directory this worker scanned, with its position in the final
    /// list (see [`DirNode`]), in the order this worker finished them.
    pub dirs: Vec<(usize, DirNode)>,
    /// Names of files with more than one link, not yet counted anywhere:
    /// see [`credit_linked`].
    pub linked: Vec<LinkedName>,
    /// Read failures this worker met (see [`ScanOutput::read_errors`]).
    pub read_errors: usize,
}

impl ScanWorker {
    pub fn new(mode: Collect) -> Self {
        Self {
            files: FileCollector::new(mode),
            dirs: Vec::new(),
            linked: Vec::new(),
            read_errors: 0,
        }
    }
}

/// Counts one read failure, printing it with `--debug`.
fn read_error(count: &mut usize, config: &ScanConfig, path: &Path, err: &std::io::Error) {
    *count += 1;
    if config.debug {
        arcommon::fs::print_read_error("ardisk", path, err);
    }
}

/// One name of a file with more than one hard link, as found by the scan.
#[derive(Debug, Clone)]
pub struct LinkedName {
    /// `(device, inode)`: the same for every name of one file.
    pub inode: (u64, u64),
    pub path: PathBuf,
    /// Position of the directory holding this name (see [`DirNode`]).
    pub dir: usize,
    /// What the file costs: blocks, bytes or 1, as for any entry.
    pub cost: u64,
    /// Logical length, for `--duplicates`.
    pub len: u64,
    /// A regular file (counted in the file total), not e.g. a FIFO.
    pub is_file: bool,
}

/// Counts each file with several hard links once, under the name
/// that sorts first by path.
///
/// Workers meet the names of one file in an order that varies from run to
/// run, so crediting whichever name was seen first would move the file
/// between directories from one scan to the next (and make `--diff` report
/// changes that did not happen). Choosing by path gives the same answer
/// every time, with any number of workers.
fn credit_linked(
    mut linked: Vec<LinkedName>,
    dirs: &mut [DirNode],
    collected: &mut FileCollector,
    progress: Option<&progress::ScanProgress>,
) {
    // Group the names of each file, smallest path first.
    linked.sort_unstable_by(|a, b| a.inode.cmp(&b.inode).then_with(|| a.path.cmp(&b.path)));
    linked.dedup_by_key(|name| name.inode);
    let (mut files, mut amount) = (0, 0);
    for name in linked {
        let dir = &mut dirs[name.dir];
        dir.size += name.cost;
        dir.content += name.cost;
        amount += name.cost;
        if name.is_file {
            files += 1;
            let file_name = name.path.file_name().map(|n| n.to_string_lossy());
            let file_name = file_name.as_deref().unwrap_or("").to_owned();
            collected.observe(name.cost, name.len, &file_name, || name.path);
        }
    }
    if let Some(progress) = progress {
        progress.add_files(files, amount);
    }
}

/// Puts each directory at the position it was given when its scan started.
/// Workers finish in any order, but the positions are exactly `0..len` (one
/// per directory pushed), so swapping every entry into place is linear.
fn in_position_order(mut dirs: Vec<(usize, DirNode)>) -> Vec<DirNode> {
    for i in 0..dirs.len() {
        while dirs[i].0 != i {
            let home = dirs[i].0;
            dirs.swap(i, home);
        }
    }
    dirs.into_iter().map(|(_, dir)| dir).collect()
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
/// stat'ed is an error: it is not entered, so `--one-file-system` never
/// wanders into something it cannot identify, and the caller counts it.
fn same_device(entry: &fs::DirEntry, dev: u64) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        entry.metadata().map(|m| m.dev() == dev)
    }
    #[cfg(not(unix))]
    {
        let _ = (entry, dev);
        Ok(true)
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
    worker: &mut ScanWorker,
) {
    let collector = &mut worker.files;
    let dir_path = task.path.as_path();

    // A directory that can't be listed, and any entry that fails while it
    // is listed, counts as unreadable (see ScanOutput::read_errors). The
    // helper also retries an open interrupted by a signal, which macOS
    // sends often (Spotlight, sandboxd and the like).
    let read_errors = &mut worker.read_errors;
    let entries = match arcommon::fs::read_dir_entries(dir_path, |err| {
        read_error(read_errors, config, dir_path, &err)
    }) {
        Ok(entries) => entries,
        Err(err) => {
            read_error(read_errors, config, dir_path, &err);
            return;
        }
    };

    // This directory's position in the scan's list. Taken only now that it
    // could be read (an unreadable directory is not listed, so positions
    // stay gap-free), and before any subdirectory is queued.
    let position = shared.next_dir.fetch_add(1, Ordering::Relaxed);

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
                eprintln!("{}", format!("ardisk: {}", err).err_red());
            }
        }
        stack
    } else {
        Arc::clone(&task.ignore_stack)
    };

    let mut local_content_size = 0u64; // only matching file bytes
    let mut local_files = 0u64; // regular files counted, for --progress
    let mut local_dir_size = 0u64; // files + directory inode cost

    // --exclude and --skip-ignored match full paths, so with either on
    // every entry's path is built up front.
    let filters_need_path = config.exclude.is_some() || config.respect_gitignore;

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
        let is_dir = file_type.is_dir();
        // The full path, built only when something needs it now: a
        // directory is queued by its path, and --exclude / --skip-ignored
        // match on it. A file otherwise builds it later, and only if it is
        // reported by path (see `FileCollector::observe`) or has several
        // hard links.
        let entry_path = (is_dir || filters_need_path).then(|| entry.path());

        // --no-hidden: checked on the entry's own name only. Skipping a
        // directory here means it is never queued, so its contents are not
        // scanned either. Cheap, so it runs before the pattern matchers.
        if config.skip_hidden && file_name.starts_with('.') {
            continue;
        }

        // --exclude is independent of .gitignore handling: it applies with
        // or without --skip-ignored, and a `!pattern` in a .gitignore can't
        // undo it.
        // The entry is matched on its own path only; a directory that is
        // merely the parent of excluded content (e.g. `target` for
        // `target/**`) is still traversed.
        if let (Some(exclude), Some(path)) = (&config.exclude, &entry_path)
            && exclude.matched(path, is_dir).is_ignore()
        {
            continue;
        }

        if config.respect_gitignore
            && let Some(path) = &entry_path
            && is_ignored(&ignore_stack, path, is_dir)
        {
            continue;
        }

        if is_dir {
            let entry_path = entry_path.unwrap_or_else(|| entry.path());
            if config.ignore_dirs.contains(file_name.as_ref()) {
                continue;
            }
            // --one-file-system: a directory on another device is a mount
            // point (or an APFS volume / firmlink target); skip it whole.
            // `entry.metadata()` does not follow symlinks, and for a mount
            // point it reports the mounted filesystem's device.
            if let Some(root_dev) = shared.root_dev {
                match same_device(&entry, root_dev) {
                    Ok(true) => {}
                    Ok(false) => {
                        if config.debug {
                            eprintln!(
                                "{}: {}: on another filesystem, skipped (--one-file-system)",
                                "ardisk".err_yellow(),
                                arcommon::display_path(&entry_path)
                            );
                        }
                        continue;
                    }
                    Err(err) => {
                        read_error(&mut worker.read_errors, config, &entry_path, &err);
                        continue;
                    }
                }
            }
            queue.push(Task {
                path: entry_path,
                parent: Some(position),
                ignore_stack: Arc::clone(&ignore_stack),
            });
        } else {
            // If --include is set, skip files that don't match the pattern
            if let Some(pattern) = &config.include_pattern
                && !pattern.matches(&file_name)
            {
                continue;
            }

            // A file whose details can't be read is left out of the totals,
            // and counted as unreadable so the user is told.
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(err) => {
                    let path = entry_path.unwrap_or_else(|| entry.path());
                    read_error(&mut worker.read_errors, config, &path, &err);
                    continue;
                }
            };
            {
                // --one-file-system: also drop a non-directory
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
                let file_size = entry_cost(&metadata, config);
                // A file with several hard links is counted once,
                // under the name that sorts first. Which names exist is only
                // known once the walk is done, so they are set aside and
                // credited then (see `credit_linked`).
                #[cfg(unix)]
                if metadata.nlink() > 1 {
                    worker.linked.push(LinkedName {
                        inode: (metadata.dev(), metadata.ino()),
                        path: entry_path.unwrap_or_else(|| entry.path()),
                        dir: position,
                        cost: file_size,
                        len: metadata.len(),
                        is_file: metadata.is_file(),
                    });
                    continue;
                }
                local_content_size += file_size;
                local_dir_size += file_size;
                if metadata.is_file() {
                    local_files += 1;
                    collector.observe(file_size, metadata.len(), &file_name, || {
                        entry_path.unwrap_or_else(|| entry.path())
                    });
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

    // The task's own path is moved into the record: one copy of each
    // directory path for the whole run.
    let dir = DirNode {
        path: task.path,
        parent: task.parent,
        size: local_dir_size,
        content: local_content_size,
    };
    worker.dirs.push((position, dir));
}

/// Rolls every directory's `size` and `content` up into its ancestors, in
/// place: afterwards each covers the directory's whole subtree.
///
/// One backwards pass. `dirs` must list parents before their children, as
/// [`ScanOutput::dirs`] does; then by the time a directory is added to its
/// parent, everything below it has already been added to it.
pub fn roll_up(dirs: &mut [DirNode]) {
    for i in (0..dirs.len()).rev() {
        if let Some(parent) = dirs[i].parent {
            debug_assert!(parent < i, "a directory must come after its parent");
            let (size, content) = (dirs[i].size, dirs[i].content);
            dirs[parent].size += size;
            dirs[parent].content += content;
        }
    }
}

/// The map form of [`roll_up`], for callers that hold per-directory sizes
/// keyed by path (see [`ScanOutput::raw_sizes`]): propagates each
/// directory's size up to every ancestor at or below `base_path`.
///
/// It has to sort by depth and look parents up by path, so it is much
/// slower than [`roll_up`]; the `ardisk` binary does not use it.
pub fn aggregate_sizes(
    raw_sizes: &HashMap<PathBuf, u64>,
    base_path: &Path,
) -> HashMap<PathBuf, u64> {
    let mut aggregated: HashMap<PathBuf, u64> = HashMap::with_capacity(raw_sizes.len());

    // Deepest first, so a directory is complete before it is added to its
    // parent. The depth is computed once per path, not once per comparison.
    let mut paths: Vec<&PathBuf> = raw_sizes.keys().collect();
    paths.sort_by_cached_key(|p| std::cmp::Reverse(p.components().count()));

    for path in paths {
        let total = {
            let slot = aggregated.entry(path.clone()).or_insert(0);
            *slot += raw_sizes[path];
            *slot
        };

        if let Some(parent) = path.parent()
            && parent.starts_with(base_path)
        {
            // Only a parent seen for the first time needs a path of its own.
            match aggregated.get_mut(parent) {
                Some(slot) => *slot += total,
                None => {
                    aggregated.insert(parent.to_path_buf(), total);
                }
            }
        }
    }

    aggregated
}

/// How human-readable sizes are written and read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SizeUnits {
    /// The default: powers of 1024, written `KB`, `MB`, `GB`, `TB`.
    #[default]
    Binary,
    /// `--si`: powers of 1000, written `kB`, `MB`, `GB`, `TB`
    Si,
}

impl SizeUnits {
    /// Bytes in one `KB`/`kB`.
    pub fn base(self) -> u64 {
        match self {
            SizeUnits::Binary => 1024,
            SizeUnits::Si => 1000,
        }
    }

    /// Labels for one base, base², base³, base⁴.
    fn labels(self) -> [&'static str; 4] {
        match self {
            SizeUnits::Binary => ["KB", "MB", "GB", "TB"],
            SizeUnits::Si => ["kB", "MB", "GB", "TB"],
        }
    }
}

/// The units every size in this process is written in. Set once by the
/// binary from `--si` before anything is printed; a global because sizes
/// are formatted in many places (reports, tree, bars, progress) that take
/// plain `fn(u64) -> String` formatters, the same way `colored` keeps its
/// on/off switch.
static SI_UNITS: AtomicBool = AtomicBool::new(false);

/// Chooses the units for all later size output.
pub fn set_size_units(units: SizeUnits) {
    SI_UNITS.store(units == SizeUnits::Si, Ordering::Relaxed);
}

/// The units set by [`set_size_units`] (binary unless changed).
pub fn size_units() -> SizeUnits {
    if SI_UNITS.load(Ordering::Relaxed) {
        SizeUnits::Si
    } else {
        SizeUnits::Binary
    }
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
    size_text_in(bytes, size_units())
}

fn size_text_in(bytes: u64, units: SizeUnits) -> (SizeTier, String) {
    let kb = units.base();
    let (mb, gb, tb) = (kb * kb, kb * kb * kb, kb * kb * kb * kb);
    let [lk, lm, lg, lt] = units.labels();

    if bytes >= tb {
        (
            SizeTier::Tb,
            format!("{:.2} {lt}", bytes as f64 / tb as f64),
        )
    } else if bytes >= gb {
        (
            SizeTier::Gb,
            format!("{:.2} {lg}", bytes as f64 / gb as f64),
        )
    } else if bytes >= mb {
        (
            SizeTier::Mb,
            format!("{:.2} {lm}", bytes as f64 / mb as f64),
        )
    } else if bytes >= kb {
        (
            SizeTier::Kb,
            format!("{:.2} {lk}", bytes as f64 / kb as f64),
        )
    } else {
        (SizeTier::Bytes, format!("{} B", bytes))
    }
}

/// [`format_size_plain`] in explicit units, independent of the global
/// setting.
pub fn format_size_in(bytes: u64, units: SizeUnits) -> String {
    size_text_in(bytes, units).1
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

    // ── Size units (--si) ────────────────────────────────────────────────────

    #[test]
    fn binary_and_si_units_format_differently() {
        use super::{SizeUnits::*, format_size_in};
        assert_eq!(format_size_in(999, Binary), "999 B");
        assert_eq!(format_size_in(999, Si), "999 B");
        assert_eq!(format_size_in(1000, Binary), "1000 B");
        assert_eq!(format_size_in(1000, Si), "1.00 kB");
        assert_eq!(format_size_in(1024, Binary), "1.00 KB");
        assert_eq!(format_size_in(1024, Si), "1.02 kB");
        assert_eq!(format_size_in(1_500_000, Binary), "1.43 MB");
        assert_eq!(format_size_in(1_500_000, Si), "1.50 MB");
        assert_eq!(format_size_in(5_000_000_000, Si), "5.00 GB");
        assert_eq!(format_size_in(5_000_000_000, Binary), "4.66 GB");
        assert_eq!(format_size_in(2_000_000_000_000, Si), "2.00 TB");
        assert_eq!(format_size_in(1 << 40, Binary), "1.00 TB");
    }

    #[test]
    fn binary_is_the_default_units() {
        assert_eq!(super::SizeUnits::default(), super::SizeUnits::Binary);
        assert_eq!(super::SizeUnits::Si.base(), 1000);
        assert_eq!(super::SizeUnits::Binary.base(), 1024);
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
    fn top_files_builds_a_path_only_for_a_file_it_keeps() {
        let built = std::cell::Cell::new(0);
        let mut top = super::TopFiles::new(2);
        for (size, name) in [(50, "a"), (40, "b"), (1, "c"), (2, "d"), (45, "e")] {
            top.offer_with(size, || {
                built.set(built.get() + 1);
                name.into()
            });
        }
        // c and d are smaller than both kept files: rejected unbuilt.
        assert_eq!(built.get(), 3);
        assert_eq!(tf_names(top), [(50, "a".into()), (45, "e".into())]);

        // Nothing is ever built when nothing is kept.
        let mut none = super::TopFiles::new(0);
        none.offer_with(10, || panic!("path built for --largest-files 0"));
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
