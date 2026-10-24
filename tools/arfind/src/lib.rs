pub mod exec;
pub mod types;

// Shared with ardisk/argrep; re-exported so `arfind::age::...` and these
// names keep working for callers and tests.
pub use arcommon::age;
pub use arcommon::ignore::{
    DEFAULT_IGNORED_DIRS as DEFAULT_IGNORES, exclude_matcher as build_exclude_matcher,
};

use arcommon::{
    fs::{device_of, device_of_metadata},
    ignore::{IgnoreStack, is_ignored},
    jobs::{MAX_JOBS, default_jobs},
    walk::{self, Queue},
};
use clap::Parser;
use clap::builder::TypedValueParser as _;
use colored::Colorize;
use crossbeam_channel::{Sender, unbounded};
use glob::Pattern;
use ignore::gitignore::Gitignore;
use std::{
    cmp::Ordering as CmpOrdering,
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::SystemTime,
};

/// `--sort` keys. Every key sorts ascending (A→Z, smallest first, oldest
/// first); `--reverse` flips it.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    /// File name only (byte order, case-sensitive), ties broken by path
    Name,
    /// Full path (byte order): each directory's contents stay together
    Path,
    /// Size in bytes, as reported for the entry itself (a symlink's own
    /// size, a directory's entry size, not its contents)
    Size,
    /// Modification time (of a symlink itself, not its target)
    Time,
}

impl SortKey {
    /// Whether this key needs each result's metadata.
    pub fn needs_metadata(self) -> bool {
        matches!(self, SortKey::Size | SortKey::Time)
    }
}

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
    #[arg(
        short = 'j',
        long,
        default_value_t = default_jobs(),
        value_parser = clap::value_parser!(u16).range(1..=i64::from(MAX_JOBS)).map(|v| v as usize)
    )]
    pub jobs: usize,

    /// Maximum recursion depth
    #[arg(long)]
    pub max_depth: Option<usize>,

    /// Don't descend into directories matching this name or glob (e.g.
    /// "vendor", "build*"), matched against the directory's name only, like
    /// argrep's --exclude-dir. The directory itself is still listed; its
    /// contents are not searched. Repeatable; honored even with --no-ignore.
    /// Alias: --ignore.
    #[arg(
        long = "exclude-dir",
        visible_alias = "ignore",
        value_name = "NAME|GLOB"
    )]
    pub exclude_dir: Vec<String>,

    /// Exclude files and directories matching a gitignore-style glob
    /// (repeatable), like ardisk's --exclude: '*.log' matches at any depth,
    /// 'src/gen' or 'target/**' are anchored to PATH, a trailing '/'
    /// matches directories only, and 'dir/**' excludes the contents of dir
    /// but not dir itself. Excluded entries are neither listed nor
    /// searched. Quote patterns to keep the shell from expanding them.
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Stay on the filesystem of PATH, like find -xdev: directories on
    /// other filesystems, mounts or APFS volumes are listed but not
    /// descended into. Unix only.
    #[arg(short = 'x', long = "one-file-system")]
    pub one_file_system: bool,

    /// Only match entries last modified at least this long ago, e.g. 30d,
    /// 6mo, 1y. Units: s, min, h, d, w, mo (30 days), y (365 days).
    #[arg(long, value_name = "AGE", value_parser = age::parse_age)]
    pub older_than: Option<age::Age>,

    /// Only match entries modified less than this long ago, e.g. 7d, 12h.
    /// Together with --older-than it selects a window.
    #[arg(long, value_name = "AGE", value_parser = age::parse_age)]
    pub newer_than: Option<age::Age>,

    /// Do not respect .gitignore / .ignore files (search everything)
    #[arg(long = "no-ignore")]
    pub no_ignore: bool,

    /// Filter by entry kind: f (file), d (directory), or l (symlink)
    #[arg(short = 't', long)]
    pub file_type: Option<String>,

    /// Only match files of this built-in type (e.g. rust, python, markdown);
    /// the same names and globs as argrep's --type. Repeatable: a file of
    /// ANY selected type matches. OR'ed with --extension, AND'ed with
    /// --name. Directories are not listed while it is active (they are
    /// still searched).
    #[arg(long = "type", value_name = "NAME", value_parser = types::type_name_parser)]
    pub types: Vec<String>,

    /// Skip files of this built-in type; wins over --type and --extension.
    /// Repeatable. Never hides directories.
    #[arg(long = "type-not", value_name = "NAME", value_parser = types::type_name_parser)]
    pub type_not: Vec<String>,

    /// Only match files with this extension, e.g. rs, .md, tar.gz
    /// (case-insensitive). Repeatable; OR'ed with --type.
    #[arg(long, value_name = "EXT", value_parser = types::extension_parser)]
    pub extension: Vec<String>,

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

    /// End each path with a NUL byte instead of a newline and print it
    /// exactly as stored (no escaping, no color)
    /// `arfind . --name '*.log' -0 | xargs -0 rm`. Pairs with argrep -0.
    #[arg(
        short = '0',
        long = "print0",
        conflicts_with_all = ["count", "exec", "exec_batch", "execdir", "dry_run"]
    )]
    pub print0: bool,

    /// Run a command once for each result instead of printing it, like
    /// find -exec / fd -x. Everything after --exec up to a ';' argument (or
    /// the end of the line) is the command. Placeholders: {} path, {/} file
    /// name, {//} parent directory, {.} path without extension, {/.} file
    /// name without extension; without any, the path is appended. No shell
    /// is involved, so file names are never interpreted. The search
    /// finishes before the first command runs; commands run one at a time,
    /// in path order.
    #[arg(
        long,
        value_name = "CMD",
        num_args = 1..,
        allow_hyphen_values = true,
        value_terminator = ";",
        conflicts_with_all = ["count", "exec_batch", "execdir"]
    )]
    pub exec: Option<Vec<String>>,

    /// Like --exec, but run each command from the result's own directory
    /// with ./NAME as the path, like find -execdir. Safer when others can
    /// write to the tree: a parent directory swapped for a symlink after
    /// the search is detected and that file is skipped instead of acted
    /// on outside the tree. The program is resolved to an absolute path
    /// up front, and a PATH with relative entries is refused.
    #[arg(
        long,
        value_name = "CMD",
        num_args = 1..,
        allow_hyphen_values = true,
        value_terminator = ";",
        conflicts_with_all = ["count", "exec_batch"]
    )]
    pub execdir: Option<Vec<String>>,

    /// Run a command once with all results as arguments, like fd -X or
    /// find -exec ... + (split into several runs if the list is very long).
    /// '{}' as a whole argument marks where the paths go; otherwise they are
    /// appended. Nothing runs when there are no results.
    #[arg(
        short = 'X',
        long = "exec-batch",
        value_name = "CMD",
        num_args = 1..,
        allow_hyphen_values = true,
        value_terminator = ";",
        conflicts_with_all = ["count", "execdir"]
    )]
    pub exec_batch: Option<Vec<String>>,

    /// With --exec/--exec-batch/--execdir: print the commands that would
    /// run (shell quoted) instead of running them.
    #[arg(long)]
    pub dry_run: bool,

    /// Sort results by name, path, size or time (ascending: A-Z, smallest
    /// first, oldest first) instead of printing them in traversal order.
    /// Output starts once the search is complete. Ties are broken by path,
    /// so the order is the same on every run. Also sets the order in which
    /// --exec/--execdir run and -X passes paths.
    #[arg(long, value_name = "KEY", value_enum, conflicts_with = "count")]
    pub sort: Option<SortKey>,

    /// With --sort: reverse the order (Z-A, largest first, newest first).
    #[arg(long, requires = "sort")]
    pub reverse: bool,
}

/// Task sent to workers
pub struct Task {
    pub path: PathBuf,
    pub depth: usize,
    /// Accumulated .gitignore/.ignore matchers from the root down to this
    /// directory's parent, in order (deepest = highest priority, mirroring
    /// git's own precedence for nested ignore files).
    pub ignore_stack: IgnoreStack,
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
    /// `--exclude-dir` entries containing glob metacharacters; plain names
    /// live in `ignore_dirs`. Like `ignore_dirs`, a matching directory is
    /// listed but not descended into.
    pub ignore_dir_patterns: Vec<Pattern>,
    /// `--exclude`: gitignore-style patterns rooted at the search root; a
    /// matching entry is neither listed nor searched. See
    /// [`build_exclude_matcher`].
    pub exclude: Option<Gitignore>,
    /// `-x` / `--one-file-system`: don't descend into directories on a
    /// different device than the search root (they are still listed).
    pub one_file_system: bool,
    /// `--older-than` / `--newer-than`: modification-time test for every
    /// listed entry. Never stops traversal.
    pub mtime: age::MtimeFilter,
    /// `--type`, `--type-not`, `--extension`.
    pub name_kind: types::NameKindFilter,
    /// Attach each result's own metadata (lstat) to its [`MatchResult`],
    /// for `--sort size`/`--sort time`. Fetched by the workers, reusing the
    /// metadata the filters already read.
    pub collect_metadata: bool,
}

impl SearchConfig {
    /// A configuration that matches everything under the defaults (`*`,
    /// built-in ignores, hidden entries skipped, `.gitignore` respected).
    /// Convenient for library users and tests; adjust fields as needed.
    pub fn new(pattern: Pattern) -> Self {
        SearchConfig {
            pattern,
            ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
            max_depth: None,
            file_type: None,
            hidden: false,
            debug: false,
            size_filter: None,
            empty_only: false,
            case_insensitive: false,
            respect_gitignore: true,
            ignore_dir_patterns: Vec::new(),
            exclude: None,
            one_file_system: false,
            mtime: age::MtimeFilter::default(),
            name_kind: types::NameKindFilter::default(),
            collect_metadata: false,
        }
    }
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
    /// The entry's own size and modification time (not following a
    /// symlink), for `--sort`. Only set when
    /// [`SearchConfig::collect_metadata`] is on, and `None` if they could
    /// not be read (e.g. the entry vanished). Just these two values rather
    /// than the whole `fs::Metadata` (176 bytes on Linux): every result
    /// crosses a channel and sorted results are held in memory.
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
}

impl MatchResult {
    /// A result for `path`, carrying size and mtime from `metadata` when
    /// `keep` is set.
    fn new(path: PathBuf, is_dir: bool, metadata: Option<&fs::Metadata>, keep: bool) -> Self {
        let metadata = metadata.filter(|_| keep);
        MatchResult {
            path,
            is_dir,
            size: metadata.map(|m| m.len()),
            modified: metadata.and_then(|m| m.modified().ok()),
        }
    }
}

/// Orders paths the way `Path::cmp` does (component by component), for the
/// clean absolute paths arfind reports, but as one byte scan: with the
/// separator ranked below every other byte, byte order and component order
/// agree (`/a/x` < `/a-b` < `/a.b/c`, as `a` < `a-b` < `a.b`).
/// `Path::cmp` re-parses both paths into components on every call, which
/// dominated `--sort` time on large trees.
pub fn path_order(a: &Path, b: &Path) -> CmpOrdering {
    let x = a.as_os_str().as_encoded_bytes();
    let y = b.as_os_str().as_encoded_bytes();
    let same = x.iter().zip(y).take_while(|(p, q)| p == q).count();
    let rank = |c: u8| {
        if std::path::is_separator(char::from(c)) {
            0
        } else {
            u16::from(c) + 1
        }
    };
    match (x.get(same), y.get(same)) {
        (Some(&p), Some(&q)) => rank(p).cmp(&rank(q)),
        _ => x.len().cmp(&y.len()),
    }
}

/// Sorts results for `--sort`. Every key is ascending, `reverse` flips it.
/// Ties, and entries of equal key, are ordered by path (always ascending),
/// so the output is deterministic despite the parallel walk. Entries whose
/// size or time could not be read sort last in either direction.
pub fn sort_results(results: &mut Vec<MatchResult>, key: SortKey, reverse: bool) {
    fn by<T: Ord>(a: Option<T>, b: Option<T>, reverse: bool) -> CmpOrdering {
        match (a, b) {
            (Some(a), Some(b)) if reverse => b.cmp(&a),
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => CmpOrdering::Less,
            (None, Some(_)) => CmpOrdering::Greater,
            (None, None) => CmpOrdering::Equal,
        }
    }
    match key {
        SortKey::Path => {
            if reverse {
                results.sort_unstable_by(|a, b| path_order(&b.path, &a.path));
            } else {
                results.sort_unstable_by(|a, b| path_order(&a.path, &b.path));
            }
        }
        SortKey::Name => {
            // Where each file name starts in its path, found once per entry
            // rather than by re-parsing both paths in every comparison.
            let mut keyed: Vec<(Option<usize>, MatchResult)> = std::mem::take(results)
                .into_iter()
                .map(|m| {
                    let start = m
                        .path
                        .file_name()
                        .map(|n| m.path.as_os_str().len() - n.len());
                    (start, m)
                })
                .collect();
            fn name((start, m): &(Option<usize>, MatchResult)) -> Option<&[u8]> {
                start.map(|s| &m.path.as_os_str().as_encoded_bytes()[s..])
            }
            keyed.sort_by(|a, b| {
                by(name(a), name(b), reverse).then_with(|| path_order(&a.1.path, &b.1.path))
            });
            results.extend(keyed.into_iter().map(|(_, m)| m));
        }
        SortKey::Size => results
            .sort_by(|a, b| by(a.size, b.size, reverse).then_with(|| path_order(&a.path, &b.path))),
        SortKey::Time => results.sort_by(|a, b| {
            by(a.modified, b.modified, reverse).then_with(|| path_order(&a.path, &b.path))
        }),
    }
}

/// Extracts the name `find`'s `-name` test would compare the root against.
/// Unlike `Path::file_name()` — which returns `None` for `.` and `..`,
/// since it only reports "normal" components — this treats them as valid
/// names in their own right, mirroring POSIX `basename` semantics. `find`
/// never canonicalizes its starting argument before testing it, which is
/// exactly why `find . -name .` matches: the argument's own name really is
/// ".". If arfind canonicalized first, "." would resolve to the real
/// directory name (e.g. "arfind") and the comparison would silently fail.
fn root_match_name(original_root: &Path) -> Option<String> {
    use std::path::Component;
    match original_root.components().next_back() {
        Some(Component::Normal(s)) => Some(s.to_string_lossy().into_owned()),
        Some(Component::CurDir) => Some(".".to_string()),
        Some(Component::ParentDir) => Some("..".to_string()),
        Some(Component::RootDir) => Some("/".to_string()),
        _ => None,
    }
}

/// Tests the search root itself against the same criteria used for
/// descendants in `scan_directory`. Without this, `arfind foo --name foo`
/// would only ever inspect `foo`'s *children* and could never report `foo`
/// itself, even though it's a perfectly valid candidate — mirroring how
/// `find foo -name foo` also matches the starting point.
///
/// Takes both the resolved `canonical_root` (used for metadata and as the
/// reported match path, so output stays consistent with the rest of the
/// tool) and the `original_root` exactly as given on the command line
/// (used only to compute the name tested against `--name`) — see
/// `root_match_name` for why these must be kept separate.
///
/// The root is also counted in `stats` (`total_dirs`/`total_files`) the same
/// way `scan_directory` counts every entry it examines — independent of
/// whether it ends up matching. Both counters share one meaning: "examined
/// as a search candidate," not "had its contents read" (a directory beyond
/// `--max-depth` is still counted here or in `scan_directory`'s entry loop,
/// even though its own contents are never scanned).
///
/// Unlike descendants, the root is never skipped for being "hidden": the
/// user named it explicitly on the command line, so `--hidden` doesn't come
/// into play here (matching `find`'s treatment of explicit start points).
fn check_root(
    canonical_root: &Path,
    original_root: &Path,
    config: &SearchConfig,
    stats: &SearchStats,
) -> Option<MatchResult> {
    let metadata = fs::symlink_metadata(canonical_root).ok()?;
    let is_symlink = metadata.file_type().is_symlink();
    // symlink_metadata reports the link itself; resolve through it for
    // is_dir() the same way read_dir's per-entry file_type() would.
    let is_dir = if is_symlink {
        fs::metadata(canonical_root)
            .map(|m| m.is_dir())
            .unwrap_or(false)
    } else {
        metadata.is_dir()
    };

    if is_dir {
        stats.total_dirs.fetch_add(1, Ordering::Relaxed);
    } else {
        stats.total_files.fetch_add(1, Ordering::Relaxed);
    }

    let file_name = root_match_name(original_root)?;

    let match_options = glob::MatchOptions {
        case_sensitive: !config.case_insensitive,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };

    if !config.pattern.matches_with(&file_name, match_options) {
        return None;
    }

    if let Some(t) = &config.file_type {
        let type_matches = (t == "f" && !is_dir && !is_symlink)
            || (t == "d" && is_dir)
            || (t == "l" && is_symlink);
        if !type_matches {
            return None;
        }
    }

    // Like the entries below, the root is tested by its own name.
    if !config.name_kind.matches(&file_name, is_dir) {
        return None;
    }

    if config.empty_only {
        let is_empty = if is_dir {
            fs::read_dir(canonical_root)
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

    if !config.mtime.matches(metadata.modified().ok()) {
        return None;
    }

    Some(MatchResult::new(
        canonical_root.to_path_buf(),
        is_dir,
        Some(&metadata),
        config.collect_metadata,
    ))
}

pub fn parallel_find(
    root: PathBuf,
    workers: usize,
    config: Arc<SearchConfig>,
    stats: SearchStats,
    on_match: impl Fn(MatchResult) + Send + 'static,
) {
    // Resolve symlinks and relative components (e.g. "." or "..") to a real
    // absolute path for traversal and for the paths reported in output —
    // but keep the original `root` argument around too, exactly as given.
    // `find`'s `-name` test against the starting point uses that argument's
    // own literal name, and canonicalizing loses it (see `root_match_name`).
    let canonical_root = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());

    let (output_tx, output_rx) = unbounded::<MatchResult>();
    // Results are printed while the walk is still running.
    let printer = thread::spawn(move || {
        for item in output_rx {
            on_match(item);
        }
    });

    if let Some(result) = check_root(&canonical_root, &root, &config, &stats) {
        stats.matched_count.fetch_add(1, Ordering::Relaxed);
        let _ = output_tx.send(result);
    }

    // --one-file-system: everything is compared with the root's device.
    let root_dev = if config.one_file_system {
        device_of(&canonical_root)
    } else {
        None
    };

    let root_task = Task {
        path: canonical_root,
        depth: 0,
        ignore_stack: Arc::new(Vec::new()),
    };
    walk::for_each([root_task], workers, |task, queue| {
        scan_directory(task, &config, queue, &output_tx, &stats, root_dev);
    });

    drop(output_tx);
    printer.join().unwrap();
}

pub fn scan_directory(
    task: Task,
    config: &SearchConfig,
    queue: &Queue<Task>,
    output_tx: &Sender<MatchResult>,
    stats: &SearchStats,
    root_dev: Option<u64>,
) {
    // `--max-depth N` follows `find`'s semantics: depth 0 is the root itself
    // (already tested by `check_root`, before this function is ever called),
    // depth 1 is the root's direct children, depth 2 their children, and so
    // on. `task.depth` is the depth of the directory *being scanned*, so once
    // it has reached the limit, its contents are one level too deep to
    // report and must not be examined at all — not read, not matched, and
    // not recursed into.
    if config
        .max_depth
        .is_some_and(|max_depth| task.depth >= max_depth)
    {
        return;
    }

    // Note: this directory itself is not counted here. It was already
    // counted in `stats.total_dirs` either by `check_root` (if it's the
    // search root) or by the entries loop of the parent directory that
    // discovered it (see the `is_dir` branch below) — both count a
    // directory the moment it's examined as a candidate, not when its
    // contents get read.

    // Computed once per directory (not per-entry) since it's constant for the
    // whole search.
    let match_options = glob::MatchOptions {
        case_sensitive: !config.case_insensitive,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };

    let entries: Vec<_> = match fs::read_dir(&task.path) {
        Ok(entries) => entries.flatten().collect(),
        Err(err) => {
            if config.debug {
                eprintln!(
                    "{}",
                    format!("arfind: {}: {}", arcommon::display_path(&task.path), err).red()
                );
            }
            return;
        }
    };

    // Extend the inherited gitignore stack with this directory's own
    // .gitignore/.ignore, if present. We already have this directory's full
    // listing above, so check presence against that instead of attempting
    // to open files that, for the overwhelming majority of directories,
    // aren't there — avoids both a wasted syscall and (with --debug) a
    // flood of harmless "No such file" noise for every directory searched.
    let ignore_stack: IgnoreStack = if config.respect_gitignore {
        let (stack, errors) =
            arcommon::ignore::stack_for_dir(&task.ignore_stack, &task.path, |name| {
                entries.iter().any(|e| e.file_name() == name)
            });
        if config.debug {
            for err in errors {
                eprintln!("{}", format!("arfind: {}", err).red());
            }
        }
        stack
    } else {
        Arc::clone(&task.ignore_stack)
    };

    for entry in entries {
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

        // --exclude: independent of .gitignore handling, so it also applies
        // with --no-ignore. Matched on the entry's own path only: for
        // `target/**`, `target` itself is still listed and entered.
        if let Some(exclude) = &config.exclude
            && exclude.matched(&entry_path, is_dir).is_ignore()
        {
            continue;
        }

        if config.respect_gitignore && is_ignored(&ignore_stack, &entry_path, is_dir) {
            continue;
        }

        if !is_dir {
            stats.total_files.fetch_add(1, Ordering::Relaxed);
        } else {
            // Counted here, at discovery, not when (or whether) we later
            // read its contents — see the note above `scan_directory`'s
            // max-depth check. A directory beyond `--max-depth` still gets
            // counted: it was genuinely examined as a candidate.
            stats.total_dirs.fetch_add(1, Ordering::Relaxed);
        }

        if config.pattern.matches_with(&file_name, match_options) {
            let type_matches = match &config.file_type {
                Some(t) => {
                    (t == "f" && !is_dir && !is_symlink)
                        || (t == "d" && is_dir)
                        || (t == "l" && is_symlink)
                }
                None => true,
            } && config.name_kind.matches(&file_name, is_dir);

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

                // --older-than / --newer-than: the entry's own mtime (a
                // symlink's, not its target's, like find without -L).
                let age_ok = !config.mtime.is_active() || {
                    if metadata_cache.is_none() {
                        metadata_cache = entry.metadata().ok();
                    }
                    config
                        .mtime
                        .matches(metadata_cache.as_ref().and_then(|m| m.modified().ok()))
                };

                if size_ok && age_ok {
                    stats.matched_count.fetch_add(1, Ordering::Relaxed);
                    // --sort size/time: read here, in the workers, and
                    // reuse what the filters above already fetched.
                    // DirEntry::metadata does not follow symlinks.
                    if config.collect_metadata && metadata_cache.is_none() {
                        metadata_cache = entry.metadata().ok();
                    }
                    let _ = output_tx.send(MatchResult::new(
                        entry_path.clone(),
                        is_dir,
                        metadata_cache.as_ref(),
                        config.collect_metadata,
                    ));
                }
            }
        }

        if is_dir && !is_symlink {
            if config.ignore_dirs.contains(file_name.as_ref())
                || config
                    .ignore_dir_patterns
                    .iter()
                    .any(|p| p.matches(&file_name))
            {
                continue;
            }

            // -x: a directory on another device is a mount point (or another
            // APFS volume). It was listed above like any directory; it is
            // just not entered, as with find -xdev.
            if let Some(dev) = root_dev
                && entry.metadata().ok().as_ref().and_then(device_of_metadata) != Some(dev)
            {
                if config.debug {
                    eprintln!(
                        "{}",
                        format!(
                            "arfind: {}: on another filesystem, not descending (--one-file-system)",
                            arcommon::display_path(&entry_path)
                        )
                        .yellow()
                    );
                }
                continue;
            }

            if config.max_depth.is_none_or(|d| task.depth < d) {
                queue.push(Task {
                    path: entry_path,
                    depth: task.depth + 1,
                    ignore_stack: Arc::clone(&ignore_stack),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Args, MatchResult, SortKey, default_jobs, path_order, root_match_name, sort_results,
    };
    use clap::Parser;
    use std::path::{Path, PathBuf};

    // ── --sort ───────────────────────────────────────────────────────────────

    #[test]
    fn sort_flags_parse() {
        let a = Args::try_parse_from(["arfind", ".", "--sort", "size", "--reverse"]).unwrap();
        assert_eq!(a.sort, Some(SortKey::Size));
        assert!(a.reverse);
        for (v, k) in [
            ("name", SortKey::Name),
            ("path", SortKey::Path),
            ("time", SortKey::Time),
        ] {
            let a = Args::try_parse_from(["arfind", ".", "--sort", v]).unwrap();
            assert_eq!(a.sort, Some(k));
            assert!(!a.reverse);
        }
        assert!(Args::try_parse_from(["arfind", ".", "--sort", "mtime"]).is_err());
        assert!(Args::try_parse_from(["arfind", ".", "--reverse"]).is_err());
        assert!(Args::try_parse_from(["arfind", ".", "--sort", "name", "-c"]).is_err());
        // Works with -0 and the exec options.
        assert!(Args::try_parse_from(["arfind", ".", "--sort", "name", "-0"]).is_ok());
        assert!(Args::try_parse_from(["arfind", ".", "--sort", "time", "--exec", "echo"]).is_ok());
    }

    #[test]
    fn only_size_and_time_need_metadata() {
        assert!(SortKey::Size.needs_metadata());
        assert!(SortKey::Time.needs_metadata());
        assert!(!SortKey::Name.needs_metadata());
        assert!(!SortKey::Path.needs_metadata());
    }

    fn bare(path: &str) -> MatchResult {
        MatchResult {
            path: PathBuf::from(path),
            is_dir: false,
            size: None,
            modified: None,
        }
    }

    fn paths(v: &[MatchResult]) -> Vec<&str> {
        v.iter().map(|m| m.path.to_str().unwrap()).collect()
    }

    #[test]
    fn name_sort_uses_the_file_name_and_breaks_ties_by_path() {
        let mut v = vec![bare("/r/b/x"), bare("/r/z/a"), bare("/r/a/x"), bare("/r/m")];
        sort_results(&mut v, SortKey::Name, false);
        assert_eq!(paths(&v), ["/r/z/a", "/r/m", "/r/a/x", "/r/b/x"]);
        // Reverse flips the names; equal names stay in path order.
        sort_results(&mut v, SortKey::Name, true);
        assert_eq!(paths(&v), ["/r/a/x", "/r/b/x", "/r/m", "/r/z/a"]);
    }

    #[test]
    fn path_order_agrees_with_path_cmp() {
        // Bytes just below and above '/', spaces, prefixes, multi-byte.
        let paths = [
            "/", "/a", "/a/b", "/a/b/c", "/a b", "/a!", "/a-b", "/a.b", "/a.b/c", "/a0", "/aB",
            "/a/x", "/a-b/x", "/ab", "/b", "/b/a", "/é", "/é/x", "/z", "/a/b-c", "/a/b.c/d",
        ];
        for a in paths {
            for b in paths {
                let (a, b) = (Path::new(a), Path::new(b));
                assert_eq!(path_order(a, b), a.cmp(b), "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn keeps_size_and_time_only_when_asked() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, "abc").unwrap();
        let m = std::fs::symlink_metadata(&f).unwrap();
        let kept = MatchResult::new(f.clone(), false, Some(&m), true);
        assert_eq!(kept.size, Some(3));
        assert_eq!(kept.modified, m.modified().ok());
        let dropped = MatchResult::new(f, false, Some(&m), false);
        assert!(dropped.size.is_none() && dropped.modified.is_none());
    }

    #[test]
    fn path_sort_and_reverse() {
        let mut v = vec![bare("/r/b"), bare("/r/a/z"), bare("/r/a")];
        sort_results(&mut v, SortKey::Path, false);
        assert_eq!(paths(&v), ["/r/a", "/r/a/z", "/r/b"]);
        sort_results(&mut v, SortKey::Path, true);
        assert_eq!(paths(&v), ["/r/b", "/r/a/z", "/r/a"]);
    }

    #[test]
    fn missing_metadata_sorts_last_in_both_directions() {
        let dir = tempfile::TempDir::new().unwrap();
        let small = dir.path().join("small");
        let big = dir.path().join("big");
        std::fs::write(&small, "x").unwrap();
        std::fs::write(&big, "xxxxxxxx").unwrap();
        let with_meta = |p: &Path| {
            let m = std::fs::symlink_metadata(p).unwrap();
            MatchResult::new(p.to_path_buf(), false, Some(&m), true)
        };
        let mut v = vec![bare("/gone"), with_meta(&big), with_meta(&small)];
        sort_results(&mut v, SortKey::Size, false);
        assert_eq!(v[0].path, small);
        assert_eq!(v[1].path, big);
        assert_eq!(v[2].path, Path::new("/gone"));
        sort_results(&mut v, SortKey::Size, true);
        assert_eq!(v[0].path, big);
        assert_eq!(v[1].path, small);
        assert_eq!(v[2].path, Path::new("/gone"));
        sort_results(&mut v, SortKey::Time, true);
        assert_eq!(v[2].path, Path::new("/gone"));
    }

    // ── -j / --jobs boundary ─────────────────────────────────────────────────
    // `0` workers means the task queue is never drained. arfind additionally
    // prints the root before spawning any workers, so `-j 0` looks like it
    // did something even though traversal never runs — this must be
    // rejected at CLI parse time so all three tools behave identically.

    #[test]
    fn jobs_zero_is_rejected_at_parse_time() {
        let result = Args::try_parse_from(["arfind", ".", "-j", "0"]);
        assert!(result.is_err(), "-j 0 must be a CLI parse error");
    }

    #[test]
    fn jobs_one_is_accepted() {
        let args = Args::try_parse_from(["arfind", ".", "-j", "1"]).unwrap();
        assert_eq!(args.jobs, 1);
    }

    #[test]
    fn jobs_two_is_accepted() {
        let args = Args::try_parse_from(["arfind", ".", "-j", "2"]).unwrap();
        assert_eq!(args.jobs, 2);
    }

    #[test]
    fn jobs_default_is_cpu_aware() {
        let args = Args::try_parse_from(["arfind", "."]).unwrap();
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

    #[test]
    fn new_flags_parse() {
        let a = Args::try_parse_from([
            "arfind",
            ".",
            "-x",
            "--exclude",
            "*.log",
            "--exclude",
            "target/**",
            "--exclude-dir",
            "build*",
            "--ignore",
            "vendor",
            "--older-than",
            "30d",
            "--newer-than",
            "1y",
        ])
        .unwrap();
        assert!(a.one_file_system);
        assert_eq!(a.exclude, ["*.log", "target/**"]);
        assert_eq!(a.exclude_dir, ["build*", "vendor"]);
        assert_eq!(a.older_than.unwrap().text, "30d");
        assert_eq!(a.newer_than.unwrap().text, "1y");

        let a = Args::try_parse_from(["arfind", ".", "--one-file-system"]).unwrap();
        assert!(a.one_file_system && a.exclude.is_empty() && a.older_than.is_none());
    }

    #[test]
    fn type_and_extension_flags_parse() {
        let a = Args::try_parse_from([
            "arfind",
            ".",
            "--type",
            "rust",
            "--type",
            "toml",
            "--type-not",
            "markdown",
            "--extension",
            ".RS",
            "--extension",
            "tar.gz",
            "-t",
            "f",
        ])
        .unwrap();
        assert_eq!(a.types, ["rust", "toml"]);
        assert_eq!(a.type_not, ["markdown"]);
        assert_eq!(a.extension, ["rs", "tar.gz"]);
        assert_eq!(a.file_type.as_deref(), Some("f"));
        assert!(Args::try_parse_from(["arfind", ".", "--type", "cobol"]).is_err());
        assert!(Args::try_parse_from(["arfind", ".", "--type", "f"]).is_err());
        assert!(Args::try_parse_from(["arfind", ".", "--file-type", "f"]).is_ok());
    }

    #[test]
    fn ambiguous_or_unitless_ages_are_rejected() {
        for bad in ["1m", "7", "1.5d", "soon"] {
            assert!(
                Args::try_parse_from(["arfind", ".", "--older-than", bad]).is_err(),
                "{bad}"
            );
            assert!(
                Args::try_parse_from(["arfind", ".", "--newer-than", bad]).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn root_match_name_current_dir_is_dot() {
        assert_eq!(root_match_name(Path::new(".")), Some(".".to_string()));
    }

    #[test]
    fn root_match_name_parent_dir_is_dotdot() {
        assert_eq!(root_match_name(Path::new("..")), Some("..".to_string()));
    }

    #[test]
    fn root_match_name_normal_relative_path() {
        assert_eq!(root_match_name(Path::new("foo")), Some("foo".to_string()));
        assert_eq!(root_match_name(Path::new("./foo")), Some("foo".to_string()));
    }

    #[test]
    fn root_match_name_absolute_path() {
        assert_eq!(
            root_match_name(Path::new("/tmp/foo")),
            Some("foo".to_string())
        );
    }

    #[test]
    fn root_match_name_trailing_dot_on_absolute_path() {
        // `Path::components()` normalizes away `.` components except when
        // they're the very first component of the path (documented Rust
        // behavior), so "/tmp/foo/." collapses to "/tmp/foo" before
        // `root_match_name` ever sees it — the trailing "." is gone, not
        // preserved as a literal name. This isn't the scenario the fix
        // targets (that's a bare "." or ".." as the whole argument, as
        // covered by the tests above); this test just documents that
        // Rust's own path parsing, not our matching logic, is what
        // resolves this particular edge case.
        assert_eq!(
            root_match_name(Path::new("/tmp/foo/.")),
            Some("foo".to_string())
        );
    }

    #[test]
    fn root_match_name_filesystem_root() {
        assert_eq!(root_match_name(Path::new("/")), Some("/".to_string()));
    }

    // ── build_dir_gitignore ──────────────────────────────────────────────────
    //
    // Presence must be determined by the caller (from a directory listing it
    // already has) rather than by attempting to open ".gitignore"/".ignore"
    // speculatively — most directories have neither file, so at scale that
    // was a wasted open() per directory plus, with --debug on, a flood of
    // harmless "No such file" noise.
}
