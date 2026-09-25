use colored::Colorize;
use crossbeam_channel::unbounded;
use flate2::read::GzDecoder;
use glob::Pattern;
use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};
use regex::{Regex, RegexBuilder};
use std::{
    borrow::Cow,
    collections::{HashSet, VecDeque},
    fs::{self, File},
    io::{BufRead, BufReader, Cursor, Read},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

/// Default directories to ignore during text search
pub const DEFAULT_IGNORES: &[&str] = &[".git", "node_modules", "__pycache__", "target"];

/// Replaces control characters that could manipulate the terminal — bell
/// (rings it), escape (arbitrary ANSI sequences: colors, cursor moves,
/// even in vulnerable terminal emulators worse than that), carriage
/// return (overwrites the current line), and the rest of the C0 control
/// range plus DEL — with a visible `\xHH` escape, so a match inside a
/// file that happens to contain such bytes can never make argrep's own
/// output do any of that to the user's terminal. This is a real risk on
/// a search this permissive: `--hidden --no-ignore` deliberately walks
/// into caches, compiled artifacts, and other content that was never
/// meant to be printed as text, and the existing binary sniff only
/// catches a NUL byte in the first 1024 bytes — plenty of non-NUL binary
/// content passes it right through.
///
/// Deliberately scoped to C0 controls (0x00-0x1F) and DEL (0x7F) only,
/// not the 8-bit C1 range (U+0080-U+009F) some terminals also treat as
/// control codes — those only arise from a genuine multi-byte UTF-8
/// sequence decoding to one of those codepoints, which is rare enough in
/// practice not to be worth the extra complexity here.
///
/// Tab is left alone (common in ordinary text and harmless). Applied at
/// the point content is packaged for display, not at the point it's
/// matched against the query — the regex still sees the original raw
/// text, so this changes nothing about what counts as a match, only what
/// gets printed for one. Returns the input unchanged (no allocation) in
/// the overwhelmingly common case of a line with no control characters;
/// the fast-path check operates on raw bytes rather than decoded chars,
/// which is safe here specifically because every byte in 0x00-0x1F/0x7F
/// is unambiguously a literal ASCII byte in valid UTF-8 — those values
/// never occur as a continuation byte (0x80-0xBF) of a multi-byte
/// sequence, so there's no risk of a false match inside one.
pub fn sanitize_for_display(s: &str) -> Cow<'_, str> {
    let needs_escaping = s
        .bytes()
        .any(|b| matches!(b, 0x00..=0x08 | 0x0B..=0x1F | 0x7F));
    if !needs_escaping {
        return Cow::Borrowed(s);
    }

    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let code = ch as u32;
        if ch == '\t' {
            out.push(ch);
        } else if code <= 0x1F || code == 0x7F {
            out.push_str(&format!("\\x{code:02x}"));
        } else {
            out.push(ch);
        }
    }
    Cow::Owned(out)
}

/// Returns true if `path`'s extension marks it as gzip-compressed, the
/// only format -z/--search-compressed currently decompresses. `.tar.gz`/
/// `.tgz` are deliberately excluded: those are archives (multiple files
/// bundled together), not a single compressed stream, and reading one
/// with a plain gzip decompressor would search the raw, uninterpreted
/// tar format bytes rather than the files inside it — a different,
/// larger feature (archive member iteration) that ripgrep's own -z
/// doesn't attempt either, for the same reason.
///
/// xz (.xz), bzip2 (.bz2), and zstd (.zst) are intentionally not
/// supported yet: unlike gzip, decompressing them well typically means
/// depending on a crate that links a system C library (liblzma, libbz2,
/// libzstd), which this project doesn't want to take on sight-unseen.
/// Gzip covers the motivating case (rotated logs, which overwhelmingly
/// use gzip by default via `logrotate`) without that risk. Extending
/// this function — and the two call sites in grep_file that use it — is
/// the natural next step if one of those formats turns out to matter in
/// practice.
pub fn is_gzip_target(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("gz")
}

/// Task sent to workers representing a directory or file to scan
pub struct Task {
    pub path: PathBuf,
    /// Accumulated .gitignore/.ignore matchers from the root down to this
    /// directory's parent, in order (deepest = highest priority, mirroring
    /// git's own precedence for nested ignore files).
    pub ignore_stack: Arc<Vec<Arc<Gitignore>>>,
}

/// Shared runtime configuration
pub struct SearchConfig {
    /// The raw pattern as given on the command line (regex, unless -F
    /// was used, in which case it was escaped before being compiled into
    /// `regex` — kept here only for display purposes, not for matching).
    pub query: String,
    /// Compiled matcher used for every line. Case-insensitivity (-i) and
    /// fixed-string mode (-F) are both baked in at construction time via
    /// `build_matcher`, rather than handled ad hoc at match time.
    pub regex: Regex,
    pub ignore_case: bool,
    pub line_number: bool,
    pub ignore_dirs: HashSet<String>,
    /// --exclude-dir (alias --ignore) entries that contain glob
    /// metacharacters (*, ?, [), compiled via glob::Pattern. Plain literal
    /// names (the common case — "node_modules", "target", etc.) stay in
    /// the faster `ignore_dirs` HashSet above instead; only entries that
    /// actually need glob matching (e.g. "build*") end up here. Both are
    /// checked — a directory is skipped if it matches *either*.
    pub ignore_dir_patterns: Vec<Pattern>,
    pub debug: bool,
    /// -v: print lines that do NOT match
    pub invert: bool,
    /// -l: print only filenames, not matching lines
    pub files_with_matches: bool,
    /// -L: print only filenames of files that contain NO match — the
    /// opposite of -l. Mutually exclusive with -l and -c at the CLI
    /// level (see Args in main.rs for why). A file is reported only if
    /// it was opened successfully, read all the way to EOF, and never
    /// produced a single matching line; see grep_file's handling for the
    /// exact bookkeeping.
    pub files_without_match: bool,
    /// -c: print count of matching lines per file
    pub count_per_file: bool,
    /// --include: only search files matching this glob pattern
    pub include_pattern: Option<Pattern>,
    /// --exclude: skip files matching any of these glob patterns. Checked
    /// *before* --include, and wins if both match the same file — a
    /// deliberate simplification of GNU grep's own order-dependent
    /// "last matching flag wins" precedence (see the README for why).
    pub exclude_patterns: Vec<Pattern>,
    /// --type: only search files matching at least one glob from the
    /// selected type(s)' expansion (e.g. "rust" -> "*.rs"). Empty means no
    /// type filter is in effect. ANDed with `include_pattern` when both
    /// are set — a file must satisfy every positive filter that's active,
    /// same relationship ripgrep's -g/--glob and -t/--type have.
    pub type_patterns: Vec<Pattern>,
    /// --type-not: skip files matching any glob from the selected type(s)'
    /// expansion. Checked alongside `exclude_patterns` (same "any negative
    /// filter excludes, and wins over the positive filters" precedence).
    pub type_not_patterns: Vec<Pattern>,
    /// -B: number of leading context lines before a match
    pub before_context: usize,
    /// -A: number of trailing context lines after a match
    pub after_context: usize,
    /// Do not respect .gitignore / .ignore files (search everything)
    pub respect_gitignore: bool,
    /// --hidden: search hidden files and directories (dotfiles) that are
    /// skipped by default. Independent of `respect_gitignore` — this is a
    /// separate filtering layer, same as ripgrep: "hidden" (dot-prefixed
    /// name) and "ignored" (matched by a .gitignore/.ignore pattern) are
    /// different concepts, and a file can be one, the other, both, or
    /// neither. Only affects entries *discovered* while walking a
    /// directory; a hidden path given directly on the command line as the
    /// search root is always searched regardless of this flag, since it
    /// never goes through the per-entry hidden check in scan_and_grep.
    pub hidden: bool,
    /// --files: list files that would be searched, without ever opening
    /// or reading them. Every filter still runs exactly as it would for a
    /// real search (hidden, gitignore, --include/--exclude, --type/
    /// --type-not, --exclude-dir) — only the actual content read is
    /// skipped, at the two call sites in scan_and_grep that would
    /// otherwise call grep_file. `total_files`/`files_discovered`/
    /// `files_skipped` are still tracked (a file is still "searched" in
    /// the sense of "listed"); `bytes_read`/`matched_lines` stay at zero,
    /// since nothing is ever opened. See main.rs's Args::files doc
    /// comment for the CLI-level rules (QUERY becomes optional, a lone
    /// positional is treated as PATH).
    pub files_only: bool,
    /// -z/--search-compressed: decompress gzip-compressed files (by
    /// extension: `.gz`) on the fly before searching their content.
    /// Currently the only supported compression format — see
    /// `is_gzip_target`'s doc comment for why xz/bz2/zst aren't included
    /// yet. Has no effect combined with `--files`, since content is never
    /// read in that mode either way; not rejected as a conflict, same as
    /// -i/-F/-w/-x under --files, since there's nothing contradictory
    /// about it, just nothing for it to do.
    pub search_compressed: bool,
    /// -q: suppress all output; only the exit code matters. Search stops
    /// as soon as one match is found (see grep_file/scan_and_grep/the
    /// worker loop in parallel_grep for the early-exit checkpoints).
    pub quiet: bool,
    /// -o: print only the matched text, one occurrence per output line,
    /// instead of the whole matching line. Mutually exclusive with -v at
    /// the CLI level, and forces before_context/after_context to 0 (with
    /// a warning) if either was requested — same as GNU grep's own
    /// documented behavior for -o combined with -v or context.
    pub only_matching: bool,
    /// -m: stop searching a file after this many matching lines (after
    /// -v inversion, if -v is also set — same as grep, which counts
    /// *selected* lines, not raw regex matches). With -o, a line with
    /// multiple occurrences still only counts once toward this limit,
    /// but every occurrence on that (already-counted) line is still
    /// printed. With -A/-B/-C, any pending trailing context is still
    /// flushed before the file search actually stops, also matching
    /// grep's documented behavior. None means unlimited (the previous,
    /// only behavior before this field existed).
    pub max_count: Option<usize>,
}

/// Shared statistics counters
#[derive(Clone)]
pub struct SearchStats {
    /// Files that were actually opened and read (or at least attempted —
    /// this includes files that turned out binary, or hit a read error
    /// partway through; it does not include files filtered out before
    /// ever being opened, see `files_skipped`). Displayed as "Files
    /// searched" by --stats.
    pub total_files: Arc<AtomicUsize>,
    /// Directories visited during traversal (displayed as "Directories").
    pub total_dirs: Arc<AtomicUsize>,
    /// Matching lines found (displayed as "Matches").
    pub matched_lines: Arc<AtomicUsize>,
    /// Files or directories that could not be read (permission denied, I/O
    /// error mid-read, etc). Search continues past these — they don't stop
    /// the scan — but a nonzero count here means the result set is
    /// incomplete, and callers (main.rs) use it to set a nonzero exit code.
    /// Silently succeeding when some files were unreadable would be
    /// misleading for a grep-like tool, especially in automation.
    pub io_errors: Arc<AtomicUsize>,
    /// Every file-type directory entry encountered while walking the tree,
    /// counted *before* any filtering (hidden, gitignore, --exclude-dir
    /// doesn't apply here since that's directories, --include/--exclude/
    /// --type/--type-not). An explicitly-named single-file root path also
    /// counts as one discovered file. `files_discovered` is always equal
    /// to `total_files` (searched) + `files_skipped` — nothing else
    /// removes a file from consideration. Displayed as "Files discovered".
    pub files_discovered: Arc<AtomicUsize>,
    /// Files filtered out by name/path rules — hidden, gitignore/
    /// .ignore, --exclude, --include, --type, --type-not — before ever
    /// being opened. Deliberately does *not* include binary files (they
    /// were opened, just not fully read) or files that failed to open
    /// (that's `io_errors`, a different failure category from "skipped by
    /// our own filtering rules"). Displayed as "Files skipped".
    pub files_skipped: Arc<AtomicUsize>,
    /// Total bytes read from file contents during scanning (the sniff
    /// read plus every read_until call in grep_file; approximated for
    /// stdin as line length + 1 per line, since BufRead::lines() strips
    /// the newline it actually consumed). Displayed as "Bytes read" —
    /// mainly useful for benchmarking throughput.
    pub bytes_read: Arc<AtomicUsize>,
}

impl SearchStats {
    pub fn new() -> Self {
        Self {
            total_files: Arc::new(AtomicUsize::new(0)),
            total_dirs: Arc::new(AtomicUsize::new(0)),
            matched_lines: Arc::new(AtomicUsize::new(0)),
            io_errors: Arc::new(AtomicUsize::new(0)),
            files_discovered: Arc::new(AtomicUsize::new(0)),
            files_skipped: Arc::new(AtomicUsize::new(0)),
            bytes_read: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Default for SearchStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Result payload containing a single text match
pub struct MatchResult {
    pub file_path: PathBuf,
    pub line_num: usize,
    pub line_content: String,
    /// Set when --count is active: total matching lines in this file
    pub count: Option<usize>,
    /// True if this result is a context line around a match (not the match itself)
    pub is_context: bool,
    /// True if this result is a group separator ("--") between non-adjacent matches
    pub is_separator: bool,
}

/// Options controlling how `build_matcher` compiles the CLI query into a
/// matcher.
///
/// Grouped into a named-field struct rather than a growing list of bool
/// parameters, since positional bools stop being readable past two or
/// three (`build_matcher(q, true, false, true, false)` — which is which?),
/// and this is very likely to grow further (case folding modes, PCRE-style
/// extensions, etc.). This is *not* the full `SearchMatcher` /
/// `RegexMatcher` trait hierarchy discussed in review — that's more
/// machinery than four flags on one regex-backed matcher currently
/// justifies. Revisit if a genuinely different matcher backend (e.g. a
/// non-regex fast path) shows up.
#[derive(Debug, Clone, Copy, Default)]
pub struct MatchOptions {
    /// -F: escape regex metacharacters before compiling, so `query` is
    /// matched literally.
    pub fixed_strings: bool,
    /// -i: case-insensitive matching.
    pub ignore_case: bool,
    /// -w: require the match to be a whole word (not a substring of a
    /// larger word).
    pub whole_word: bool,
    /// -x: require the match to span the entire line.
    pub whole_line: bool,
}

/// Builds the compiled matcher used for every line, from the raw CLI
/// pattern and flags.
///
/// - `fixed_strings` (-F): the pattern is escaped via `regex::escape`
///   before compilation, so any regex metacharacters in it (`.`, `*`,
///   `(`, etc.) are matched literally instead of being interpreted —
///   grep's `-F` semantics. Without it, `query` is compiled as a regex
///   directly.
/// - `ignore_case` (-i): passed to the regex engine's own case-insensitive
///   mode rather than lowercasing each line at match time, which is both
///   simpler and avoids a per-line allocation.
/// - `whole_word` (-w): wraps the pattern in `\b(?:...)\b`, the same
///   approach ripgrep uses. Applied *after* -F escaping, so a literal
///   query is word-wrapped as a whole rather than its escaped form being
///   reinterpreted.
/// - `whole_line` (-x): wraps the (possibly already word-wrapped) pattern
///   in `^(?:...)$`. Note this only does what you'd expect because
///   `grep_file`'s read loop strips the trailing line terminator before
///   matching — the regex crate's `$` (without multi-line mode) means
///   true end-of-haystack, not "before a trailing \n" like Perl/Python,
///   so matching against an un-stripped line would silently make -x
///   never match in the file-search path while still working over stdin
///   (where `BufRead::lines()` already strips it). If that stripping is
///   ever removed, -x needs to be revisited alongside it.
///
/// Returns a human-readable error (not a panic) on invalid regex syntax,
/// so callers can report it as a normal CLI usage error.
pub fn build_matcher(query: &str, opts: MatchOptions) -> Result<Regex, String> {
    let mut pattern = if opts.fixed_strings {
        regex::escape(query)
    } else {
        query.to_string()
    };

    if opts.whole_word {
        pattern = format!(r"\b(?:{})\b", pattern);
    }
    if opts.whole_line {
        pattern = format!(r"^(?:{})$", pattern);
    }

    RegexBuilder::new(&pattern)
        .case_insensitive(opts.ignore_case)
        .build()
        .map_err(|err| format!("Invalid pattern '{}': {}", query, err))
}

/// Runs a parallel grep across all text files under `root`,
/// calling `on_match` for every matched line.
pub fn parallel_grep(
    root: PathBuf,
    workers: usize,
    config: Arc<SearchConfig>,
    stats: SearchStats,
    on_match: impl Fn(MatchResult) + Send + 'static,
) {
    let (task_tx, task_rx) = unbounded::<Task>();
    let (output_tx, output_rx) = unbounded::<MatchResult>();
    let active_tasks = Arc::new(AtomicUsize::new(1));

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

                if !(config.quiet && stats.matched_lines.load(Ordering::Relaxed) > 0) {
                    scan_and_grep(task, &config, &task_tx, &output_tx, &active_tasks, &stats);
                }
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
            eprintln!("{}", format!("argrep: {}", err).red());
        }
    }

    match builder.build() {
        Ok(gi) if gi.num_ignores() > 0 || gi.num_whitelists() > 0 => Some(gi),
        Ok(_) => None,
        Err(err) => {
            if debug {
                eprintln!("{}", format!("argrep: {}", err).red());
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

pub fn scan_and_grep(
    task: Task,
    config: &SearchConfig,
    task_tx: &crossbeam_channel::Sender<Task>,
    output_tx: &crossbeam_channel::Sender<MatchResult>,
    active_tasks: &AtomicUsize,
    stats: &SearchStats,
) {
    let dir_path = task.path.as_path();

    if dir_path.is_file() {
        // An explicitly-named single-file root always counts as one
        // discovered (and searched) file — see files_discovered's field
        // doc for why this must stay in sync with the entries-loop below.
        stats.files_discovered.fetch_add(1, Ordering::Relaxed);
        stats.total_files.fetch_add(1, Ordering::Relaxed);
        if config.files_only {
            emit_file_listing(dir_path, output_tx);
        } else {
            grep_file(dir_path, config, output_tx, stats);
        }
        return;
    }

    stats.total_dirs.fetch_add(1, Ordering::Relaxed);

    let entries: Vec<_> = match fs::read_dir(dir_path) {
        Ok(entries) => entries.flatten().collect(),
        Err(err) => {
            stats.io_errors.fetch_add(1, Ordering::Relaxed);
            if config.debug {
                eprintln!("{}: {}: {}", "argrep".red(), dir_path.display(), err);
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

    for entry in entries {
        if config.quiet && stats.matched_lines.load(Ordering::Relaxed) > 0 {
            // Don't enqueue more subdirectories or scan more files in this
            // directory once -q already has its answer — the rest of this
            // listing would just be wasted I/O.
            return;
        }

        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if file_type.is_symlink() {
            continue; // Skip symlinks to prevent reference cycle traps
        }

        let os_file_name = entry.file_name();
        let file_name = os_file_name.to_string_lossy();
        let is_dir = file_type.is_dir();

        // files_discovered counts every file-type entry seen, before any
        // filtering below — see the field doc on SearchStats for the
        // accounting identity this is meant to satisfy.
        if !is_dir {
            stats.files_discovered.fetch_add(1, Ordering::Relaxed);
        }

        if !config.hidden && file_name.starts_with('.') {
            if !is_dir {
                stats.files_skipped.fetch_add(1, Ordering::Relaxed);
            }
            continue; // Skip hidden files/folders by default (--hidden overrides)
        }

        let entry_path = entry.path();

        if config.respect_gitignore && is_path_ignored(&ignore_stack, &entry_path, is_dir) {
            if !is_dir {
                stats.files_skipped.fetch_add(1, Ordering::Relaxed);
            }
            continue;
        }

        if is_dir {
            if config.ignore_dirs.contains(file_name.as_ref())
                || config
                    .ignore_dir_patterns
                    .iter()
                    .any(|p| p.matches(&file_name))
            {
                continue;
            }
            active_tasks.fetch_add(1, Ordering::SeqCst);
            let _ = task_tx.send(Task {
                path: entry_path,
                ignore_stack: Arc::clone(&ignore_stack),
            });
        } else {
            // --exclude / --type-not: skip files matching any exclude
            // glob, whether it came from --exclude directly or from
            // --type-not's expanded globs. Checked before --include/
            // --type and wins on overlap — see the field docs on
            // SearchConfig.exclude_patterns/type_not_patterns for why.
            if config
                .exclude_patterns
                .iter()
                .any(|p| p.matches(&file_name))
                || config
                    .type_not_patterns
                    .iter()
                    .any(|p| p.matches(&file_name))
            {
                stats.files_skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            // --include: skip files whose names don't match the pattern
            if let Some(pattern) = &config.include_pattern
                && !pattern.matches(&file_name)
            {
                stats.files_skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            // --type: skip files that don't match any of the selected
            // type(s)' globs. An empty type_patterns list means no --type
            // was given, so nothing is filtered here.
            if !config.type_patterns.is_empty()
                && !config.type_patterns.iter().any(|p| p.matches(&file_name))
            {
                stats.files_skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            stats.total_files.fetch_add(1, Ordering::Relaxed);
            if config.files_only {
                emit_file_listing(&entry_path, output_tx);
            } else {
                grep_file(&entry_path, config, output_tx, stats);
            }
        }
    }
}

/// --files: emits a bare-filename result for `file_path` without ever
/// opening it — the content-free counterpart to grep_file, used at both
/// of scan_and_grep's call sites when `config.files_only` is set. Same
/// output shape as -l's own bare-filename MatchResult (line_num 0, no
/// content), which is what lets print_result in main.rs reuse that exact
/// print branch for --files too.
fn emit_file_listing(file_path: &Path, output_tx: &crossbeam_channel::Sender<MatchResult>) {
    let _ = output_tx.send(MatchResult {
        file_path: file_path.to_path_buf(),
        line_num: 0,
        line_content: String::new(),
        count: None,
        is_context: false,
        is_separator: false,
    });
}

pub fn grep_file(
    file_path: &Path,
    config: &SearchConfig,
    output_tx: &crossbeam_channel::Sender<MatchResult>,
    stats: &SearchStats,
) {
    let file = match File::open(file_path) {
        Ok(f) => f,
        Err(err) => {
            stats.io_errors.fetch_add(1, Ordering::Relaxed);
            if config.debug {
                eprintln!("{}: {}: {}", "argrep".red(), file_path.display(), err);
            }
            return;
        }
    };

    // -z/--search-compressed: transparently decompress gzip files. The
    // resulting content stream is boxed so the rest of this function
    // doesn't need to know or care which case it's in.
    let content: Box<dyn Read> = if config.search_compressed && is_gzip_target(file_path) {
        Box::new(GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut buffered = BufReader::new(content);

    // Fast binary file sniffing: check the first 1024 bytes for a null
    // byte. A decompressed stream generally isn't seekable (GzDecoder
    // doesn't implement Seek), so rather than rewinding back to the start
    // after sniffing — which also happens to make this handle pipes/FIFOs
    // that were never seekable to begin with — the sniffed bytes are held
    // onto and replayed via Read::chain ahead of the rest of the stream.
    // This works identically for a plain file, a decompressed one, or a
    // FIFO, so there's exactly one code path below instead of two nearly
    // identical ones.
    let mut sniffer_buffer = [0u8; 1024];
    let sniffed = match buffered.read(&mut sniffer_buffer) {
        Ok(n) => n,
        Err(err) => {
            stats.io_errors.fetch_add(1, Ordering::Relaxed);
            if config.debug {
                eprintln!("{}: {}: {}", "argrep".red(), file_path.display(), err);
            }
            return;
        }
    };
    stats.bytes_read.fetch_add(sniffed, Ordering::Relaxed);
    if sniffer_buffer[..sniffed].contains(&0u8) {
        return; // Skip compiled binaries or media files (post-decompression, for -z)
    }

    let mut reader = Cursor::new(sniffer_buffer[..sniffed].to_vec()).chain(buffered);

    // Process file line by line, reusing a single heap allocation.
    //
    // We read raw bytes (read_until) rather than read_line, and decode each
    // line with from_utf8_lossy instead of relying on String's strict UTF-8
    // validation. read_line() returns Err on invalid UTF-8, and looping with
    // `while let Ok(...)` silently treats that Err exactly like EOF: the
    // rest of the file — every remaining line, including real matches — is
    // dropped with no error and a clean exit. A grep-like tool cannot afford
    // that. Real-world text files (logs with a stray non-UTF-8 byte, files
    // in a legacy encoding, etc.) still deserve every other line searched;
    // invalid sequences become U+FFFD in that one line rather than aborting
    // the scan, matching how tools like ripgrep treat non-UTF-8 files by
    // default.
    let mut line_bytes: Vec<u8> = Vec::new();
    let mut line_num = 0usize;
    let mut match_count = 0usize;

    let before_ctx = config.before_context;
    let after_ctx = config.after_context;
    let has_context = before_ctx > 0 || after_ctx > 0;

    let mut before_buffer: VecDeque<(usize, String)> = VecDeque::with_capacity(before_ctx);
    let mut after_remaining = 0usize;
    let mut last_printed_line = 0usize;
    let mut has_printed_anything = false;
    // -m: set once match_count reaches config.max_count. Doesn't break
    // immediately — grep's own documented behavior is "when grep stops
    // after NUM matching lines, it outputs any trailing context lines",
    // so this only actually stops the loop once after_remaining (the
    // pending -A/-C tail, 0 if no context was requested) has been fully
    // flushed. See the check at the top of the loop below.
    let mut reached_max_count = false;
    // -L: set if a read_until call fails partway through the file. A
    // partial read must not be reported as "this file has no matches" —
    // it only means the *part we managed to read* had none. Distinct
    // from the File::open failure case above (which returns before ever
    // reaching this point, so it's excluded from -L output automatically).
    let mut had_io_error = false;

    loop {
        if reached_max_count && after_remaining == 0 {
            break;
        }

        if config.quiet && stats.matched_lines.load(Ordering::Relaxed) > 0 {
            // Another file (possibly scanned by a different worker thread)
            // already produced a match, so there's no point reading any
            // further into this one — -q only cares that at least one
            // match exists anywhere.
            break;
        }

        line_bytes.clear();
        match reader.read_until(b'\n', &mut line_bytes) {
            Ok(0) => break, // EOF
            Ok(n) => {
                stats.bytes_read.fetch_add(n, Ordering::Relaxed);
            }
            Err(err) => {
                stats.io_errors.fetch_add(1, Ordering::Relaxed);
                had_io_error = true;
                if config.debug {
                    eprintln!("{}: {}: {}", "argrep".red(), file_path.display(), err);
                }
                break;
            }
        }
        line_num += 1;

        let raw_line = String::from_utf8_lossy(&line_bytes);
        // Strip the trailing line terminator (read_until keeps it, unlike
        // stdin's `BufRead::lines()`, which already strips it). This matters
        // beyond cosmetics: the regex crate's `$` anchor (without multi-line
        // mode) means true end-of-haystack, not "before a trailing \n" like
        // Perl/Python — so leaving the terminator in would silently break
        // -x (^...$ whole-line matching) here while working fine on the
        // stdin path, a classic case of "looks the same, matches
        // differently" that's easy to miss without directly comparing the
        // two code paths.
        let line = raw_line
            .strip_suffix('\n')
            .map(|s| s.strip_suffix('\r').unwrap_or(s))
            .unwrap_or(&raw_line);

        let line_matches = config.regex.is_match(line);

        // Sanitized once per line, reused everywhere this line's content
        // gets packaged into a MatchResult below — matching itself always
        // uses the original `line`/`m.as_str()`, never this. See
        // sanitize_for_display's doc comment for why.
        let clean_line = sanitize_for_display(line);

        // Apply -v inversion
        let should_emit = if config.invert {
            !line_matches
        } else {
            line_matches
        };

        if should_emit {
            match_count += 1;
            stats.matched_lines.fetch_add(1, Ordering::Relaxed);

            if config.quiet {
                // -q: exit status only. No output, ever — this takes
                // priority over -l/-c/etc. if they're also set, same as
                // real grep. Stop reading this file immediately; the
                // surrounding scan_and_grep/worker loop are responsible
                // for winding down the rest of the search.
                break;
            }

            if config.files_with_matches {
                // -l: emit the file once and stop reading further
                let _ = output_tx.send(MatchResult {
                    file_path: file_path.to_path_buf(),
                    line_num: 0,
                    line_content: String::new(),
                    count: None,
                    is_context: false,
                    is_separator: false,
                });
                break;
            } else if config.files_without_match {
                // -L: this file just produced a match, so it's
                // disqualified from "no match" output. Nothing to emit —
                // stop reading; the rest of the file's content is
                // irrelevant to -L either way.
                break;
            } else if !config.count_per_file {
                if config.only_matching {
                    // -o: one output row per match occurrence on this
                    // line, containing only the matched text rather than
                    // the whole line. No context bookkeeping here — by
                    // the time SearchConfig is built, main.rs has already
                    // zeroed before_context/after_context and warned if
                    // -o was combined with -A/-B/-C, matching GNU grep's
                    // own documented behavior for that combination
                    // ("these options have no effect").
                    for m in config.regex.find_iter(line) {
                        let _ = output_tx.send(MatchResult {
                            file_path: file_path.to_path_buf(),
                            line_num,
                            line_content: sanitize_for_display(m.as_str()).to_string(),
                            count: None,
                            is_context: false,
                            is_separator: false,
                        });
                    }
                    last_printed_line = line_num;
                    has_printed_anything = true;
                } else {
                    if has_context {
                        let first_line_to_print = if let Some((b_num, _)) = before_buffer.front() {
                            std::cmp::min(*b_num, line_num)
                        } else {
                            line_num
                        };

                        if has_printed_anything && first_line_to_print > last_printed_line + 1 {
                            let _ = output_tx.send(MatchResult {
                                file_path: file_path.to_path_buf(),
                                line_num: 0,
                                line_content: String::new(),
                                count: None,
                                is_context: false,
                                is_separator: true,
                            });
                        }

                        while let Some((b_num, b_content)) = before_buffer.pop_front() {
                            if b_num > last_printed_line {
                                let _ = output_tx.send(MatchResult {
                                    file_path: file_path.to_path_buf(),
                                    line_num: b_num,
                                    line_content: b_content,
                                    count: None,
                                    is_context: true,
                                    is_separator: false,
                                });
                                last_printed_line = b_num;
                            }
                        }
                    }

                    // Normal mode: emit matching line
                    let _ = output_tx.send(MatchResult {
                        file_path: file_path.to_path_buf(),
                        line_num,
                        line_content: clean_line.to_string(),
                        count: None,
                        is_context: false,
                        is_separator: false,
                    });
                    last_printed_line = line_num;
                    has_printed_anything = true;
                    after_remaining = after_ctx;

                    if before_ctx > 0 {
                        before_buffer.push_back((line_num, clean_line.to_string()));
                    }
                }
            }
            // -c mode: accumulate count, emit at end

            if let Some(max) = config.max_count
                && match_count >= max
            {
                reached_max_count = true;
            }
        } else if has_context
            && !config.count_per_file
            && !config.files_with_matches
            && !config.files_without_match
        {
            if after_remaining > 0 {
                let _ = output_tx.send(MatchResult {
                    file_path: file_path.to_path_buf(),
                    line_num,
                    line_content: clean_line.to_string(),
                    count: None,
                    is_context: true,
                    is_separator: false,
                });
                last_printed_line = line_num;
                after_remaining -= 1;
            }

            if before_ctx > 0 {
                if before_buffer.len() == before_ctx {
                    before_buffer.pop_front();
                }
                before_buffer.push_back((line_num, clean_line.to_string()));
            }
        }
    }

    // -c: emit one result per file with the total count
    if config.count_per_file {
        let _ = output_tx.send(MatchResult {
            file_path: file_path.to_path_buf(),
            line_num: 0,
            line_content: String::new(),
            count: Some(match_count),
            is_context: false,
            is_separator: false,
        });
    }

    // -L: only reached if the loop ran all the way to EOF without the
    // early-exit break above — i.e. every line was read and none of them
    // matched. `had_io_error` additionally guards against a mid-file read
    // failure being mistaken for "the rest of the file had no matches"
    // (a file that failed to open at all never reaches this point in the
    // first place, so that case is already excluded). -q suppresses this
    // like every other output path; the file's outcome still feeds into
    // the same global matched_lines/exit-code contract as everything
    // else, so -q + -L needs no special-cased exit code.
    if config.files_without_match && !config.quiet && match_count == 0 && !had_io_error {
        let _ = output_tx.send(MatchResult {
            file_path: file_path.to_path_buf(),
            line_num: 0,
            line_content: String::new(),
            count: None,
            is_context: false,
            is_separator: false,
        });
    }
}

#[cfg(test)]
mod ignore_tests {
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

#[cfg(test)]
mod sanitize_tests {
    // ── sanitize_for_display ─────────────────────────────────────────────────
    //
    // The fix for the terminal-bell/escape-injection report: a file that
    // passes the binary sniff (no NUL in the first 1024 bytes) can still
    // contain other control bytes, and printing those raw to the user's
    // terminal can ring the bell, move the cursor, or worse. These tests
    // cover the escaping rules and, just as importantly, that ordinary
    // text (including non-ASCII) is left completely alone.

    use super::sanitize_for_display;

    #[test]
    fn plain_text_is_returned_unchanged_and_unallocated() {
        let input = "just an ordinary line, nothing weird here";
        let result = sanitize_for_display(input);
        assert_eq!(result, input);
        assert!(
            matches!(result, std::borrow::Cow::Borrowed(_)),
            "the common case (no control chars) must not allocate"
        );
    }

    #[test]
    fn bell_character_is_escaped() {
        // The exact byte from the bug report: BEL rings the terminal.
        let input = "before\x07after";
        assert_eq!(sanitize_for_display(input), "before\\x07after");
    }

    #[test]
    fn escape_character_is_escaped() {
        // ESC is the start of arbitrary ANSI sequences — colors, cursor
        // moves, and on vulnerable terminals worse than that.
        let input = "\x1b[31mfake red\x1b[0m";
        assert_eq!(sanitize_for_display(input), "\\x1b[31mfake red\\x1b[0m");
    }

    #[test]
    fn carriage_return_is_escaped() {
        // A raw \r would overwrite the current terminal line.
        let input = "visible\rhidden";
        assert_eq!(sanitize_for_display(input), "visible\\x0dhidden");
    }

    #[test]
    fn null_byte_is_escaped() {
        let input = "a\x00b";
        assert_eq!(sanitize_for_display(input), "a\\x00b");
    }

    #[test]
    fn delete_character_is_escaped() {
        let input = "a\x7fb";
        assert_eq!(sanitize_for_display(input), "a\\x7fb");
    }

    #[test]
    fn tab_is_left_alone() {
        // Tabs are common in ordinary text (indentation, TSV data) and
        // harmless to print — must not be escaped like the other C0
        // controls around it.
        let input = "col1\tcol2\tcol3";
        let result = sanitize_for_display(input);
        assert_eq!(result, input);
        assert!(matches!(result, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn non_ascii_text_is_left_alone() {
        // Multi-byte UTF-8 must not be mistaken for control bytes — this
        // is the case the byte-level fast-path check has to get right,
        // since continuation bytes (0x80-0xBF) sit right next to the
        // ASCII control range this function targets.
        let input = "héllo wörld —日本語 — emoji 🎉 here";
        let result = sanitize_for_display(input);
        assert_eq!(result, input);
        assert!(matches!(result, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn multiple_control_characters_are_each_escaped() {
        let input = "\x01\x02\x03";
        assert_eq!(sanitize_for_display(input), "\\x01\\x02\\x03");
    }

    #[test]
    fn control_character_alongside_non_ascii_text_only_escapes_the_control_byte() {
        let input = "café\x07bar";
        assert_eq!(sanitize_for_display(input), "café\\x07bar");
    }

    #[test]
    fn escaping_preserves_character_boundaries_not_byte_boundaries() {
        // A naive byte-for-byte escape pass (rather than iterating by
        // char) risks slicing a multi-byte UTF-8 character in half. This
        // input interleaves a 3-byte character with a control byte to
        // make sure that can't happen.
        let input = "日\x07本";
        let result = sanitize_for_display(input);
        assert_eq!(result, "日\\x07本");
        assert!(
            std::str::from_utf8(result.as_bytes()).is_ok(),
            "result must always be valid UTF-8"
        );
    }
}
