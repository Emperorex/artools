use colored::Colorize;
use crossbeam_channel::unbounded;
use glob::Pattern;
use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};
use regex::{Regex, RegexBuilder};
use std::{
    collections::{HashSet, VecDeque},
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

/// Default directories to ignore during text search
pub const DEFAULT_IGNORES: &[&str] = &[".git", "node_modules", "__pycache__", "target"];

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
    pub debug: bool,
    /// -v: print lines that do NOT match
    pub invert: bool,
    /// -l: print only filenames, not matching lines
    pub files_with_matches: bool,
    /// -c: print count of matching lines per file
    pub count_per_file: bool,
    /// --include: only search files matching this glob pattern
    pub include_pattern: Option<Pattern>,
    /// -B: number of leading context lines before a match
    pub before_context: usize,
    /// -A: number of trailing context lines after a match
    pub after_context: usize,
    /// Do not respect .gitignore / .ignore files (search everything)
    pub respect_gitignore: bool,
    /// -q: suppress all output; only the exit code matters. Search stops
    /// as soon as one match is found (see grep_file/scan_and_grep/the
    /// worker loop in parallel_grep for the early-exit checkpoints).
    pub quiet: bool,
}

/// Shared statistics counters
#[derive(Clone)]
pub struct SearchStats {
    pub total_files: Arc<AtomicUsize>,
    pub total_dirs: Arc<AtomicUsize>,
    pub matched_lines: Arc<AtomicUsize>,
    /// Files or directories that could not be read (permission denied, I/O
    /// error mid-read, etc). Search continues past these — they don't stop
    /// the scan — but a nonzero count here means the result set is
    /// incomplete, and callers (main.rs) use it to set a nonzero exit code.
    /// Silently succeeding when some files were unreadable would be
    /// misleading for a grep-like tool, especially in automation.
    pub io_errors: Arc<AtomicUsize>,
}

impl SearchStats {
    pub fn new() -> Self {
        Self {
            total_files: Arc::new(AtomicUsize::new(0)),
            total_dirs: Arc::new(AtomicUsize::new(0)),
            matched_lines: Arc::new(AtomicUsize::new(0)),
            io_errors: Arc::new(AtomicUsize::new(0)),
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
        stats.total_files.fetch_add(1, Ordering::Relaxed);
        grep_file(dir_path, config, output_tx, stats);
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

        if file_name.starts_with('.') {
            continue; // Skip hidden files/folders by default
        }

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
            // --include: skip files whose names don't match the pattern
            if let Some(pattern) = &config.include_pattern
                && !pattern.matches(&file_name)
            {
                continue;
            }
            stats.total_files.fetch_add(1, Ordering::Relaxed);
            grep_file(&entry_path, config, output_tx, stats);
        }
    }
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

    let mut reader = BufReader::new(file);

    // Fast binary file sniffing: check the first 1024 bytes for a null byte
    let mut sniffer_buffer = [0u8; 1024];
    if let Ok(bytes_read) = reader.read(&mut sniffer_buffer) {
        if sniffer_buffer[..bytes_read].contains(&0u8) {
            return; // Skip compiled binaries or media files
        }
        if let Err(err) = reader.seek(SeekFrom::Start(0)) {
            // Rewinding after the sniff read failed — an actual I/O error,
            // not something the loop below gets a chance to see, since we
            // return before it runs. Per the #106 contract (I/O errors →
            // nonzero exit), this must be counted the same as any other
            // unreadable file, not silently skipped.
            stats.io_errors.fetch_add(1, Ordering::Relaxed);
            if config.debug {
                eprintln!("{}: {}: {}", "argrep".red(), file_path.display(), err);
            }
            return;
        }
    }

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

    loop {
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
            Ok(_) => {}
            Err(err) => {
                stats.io_errors.fetch_add(1, Ordering::Relaxed);
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
            } else if !config.count_per_file {
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
                    line_content: line.to_string(),
                    count: None,
                    is_context: false,
                    is_separator: false,
                });
                last_printed_line = line_num;
                has_printed_anything = true;
                after_remaining = after_ctx;

                if before_ctx > 0 {
                    before_buffer.push_back((line_num, line.to_string()));
                }
            }
            // -c mode: accumulate count, emit at end
        } else if has_context && !config.count_per_file && !config.files_with_matches {
            if after_remaining > 0 {
                let _ = output_tx.send(MatchResult {
                    file_path: file_path.to_path_buf(),
                    line_num,
                    line_content: line.to_string(),
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
                before_buffer.push_back((line_num, line.to_string()));
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
