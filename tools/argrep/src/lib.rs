use arcommon::{
    ignore::{DEFAULT_IGNORED_DIRS, IgnoreStack, is_ignored},
    walk::{self, Queue},
};
use colored::Colorize;
use crossbeam_channel::unbounded;
use flate2::read::GzDecoder;
use glob::Pattern;
use regex::{Regex, RegexBuilder};
use std::{
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

/// Default cap on a single line's length, in bytes (4 MiB). Lines longer
/// than this are skipped rather than buffered whole; see
/// `SearchConfig::max_line_bytes`. Large enough for minified JS bundles and
/// long JSON-lines records, small enough that N worker threads each holding
/// one can't exhaust memory.
pub const DEFAULT_MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Default cap on the decompressed size of a single `-z` file, in bytes
/// (1 GiB). Gzip compresses repetitive input roughly 1000:1, so without a
/// cap a ~1 MB file can expand to gigabytes (a "gzip bomb").
pub const DEFAULT_MAX_DECOMPRESSED_BYTES: u64 = 1024 * 1024 * 1024;

/// Parses a human-friendly size such as `4M`, `512K`, `1G` or `1048576`
/// (binary multiples: K = 1024). `0` is valid and means "unlimited" for the
/// flags that use this. Used as a clap value_parser.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("size must not be empty".to_string());
    }
    let (digits, mult) = match t.chars().last().unwrap().to_ascii_uppercase() {
        'K' => (&t[..t.len() - 1], 1024u64),
        'M' => (&t[..t.len() - 1], 1024 * 1024),
        'G' => (&t[..t.len() - 1], 1024 * 1024 * 1024),
        _ => (t, 1),
    };
    let n: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("invalid size '{s}' (expected e.g. 4M, 512K, 1G, or a byte count)"))?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("size '{s}' is too large"))
}

/// Reads one line (through `\n`, inclusive) into `buf`, but never stores
/// more than `max_line + 2` bytes (room for a `\r\n` terminator beyond the
/// limit). Bytes past that are consumed from the reader and discarded, so
/// memory stays bounded no matter how long the line is. Returns
/// `(bytes consumed from reader, overflowed)`; consumed == 0 means EOF.
/// `max_line == 0` means unlimited.
fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_line: usize,
) -> std::io::Result<(usize, bool)> {
    let limit = if max_line == 0 {
        usize::MAX
    } else {
        max_line.saturating_add(2)
    };
    let mut total = 0usize;
    let mut overflow = false;
    loop {
        let (done, used) = {
            let avail = match reader.fill_buf() {
                Ok(a) => a,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if avail.is_empty() {
                return Ok((total, overflow));
            }
            let (chunk, done) = match avail.iter().position(|&b| b == b'\n') {
                Some(i) => (&avail[..=i], true),
                None => (avail, false),
            };
            if !overflow {
                let room = limit.saturating_sub(buf.len());
                if chunk.len() <= room {
                    buf.extend_from_slice(chunk);
                } else {
                    buf.extend_from_slice(&chunk[..room]);
                    overflow = true;
                }
            }
            (done, chunk.len())
        };
        reader.consume(used);
        total += used;
        if done {
            return Ok((total, overflow));
        }
    }
}

/// Default directories to ignore during text search: the shared defaults
/// (`arcommon::ignore::DEFAULT_IGNORED_DIRS`) plus `target`, since build
/// output is rarely worth grepping.
pub const DEFAULT_IGNORES: &[&str] = &[".git", "node_modules", "__pycache__", "target"];

// DEFAULT_IGNORES must stay a superset of the shared list.
const _: () = {
    let mut i = 0;
    while i < DEFAULT_IGNORED_DIRS.len() {
        let (a, b) = (
            DEFAULT_IGNORED_DIRS[i].as_bytes(),
            DEFAULT_IGNORES[i].as_bytes(),
        );
        assert!(
            a.len() == b.len(),
            "DEFAULT_IGNORES must start with the shared defaults"
        );
        let mut j = 0;
        while j < a.len() {
            assert!(
                a[j] == b[j],
                "DEFAULT_IGNORES must start with the shared defaults"
            );
            j += 1;
        }
        i += 1;
    }
};

/// Escapes control characters (C0 except tab, DEL, and C1) so file
/// content can be printed to a terminal without the terminal executing
/// them. Shared with arfind/ardisk via the `arcommon` crate; re-exported
/// here so existing `argrep::sanitize_for_display` users keep working.
/// Applied when content is packaged for display, never before matching:
/// the regex always sees the original text.
pub use arcommon::sanitize_for_display;

/// Path ready to print to a terminal (control characters escaped). Only
/// for human-facing output; see `arcommon::display_path`.
pub use arcommon::display_path;

/// A line after --replace, ready for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replaced {
    /// The line with every match replaced, sanitized for the terminal.
    pub text: String,
    /// Byte ranges of `text` that came from the replacement (for
    /// highlighting). Empty replacements leave no range.
    pub spans: Vec<(usize, usize)>,
}

/// Replaces every non-overlapping match of `regex` in `line` with
/// `replacement` (expanded per match: `$0`, `$1`, `${name}`, `$$`), the
/// same way ripgrep's --replace and `Regex::replace_all` do. The original
/// text between matches and the inserted text are sanitized separately,
/// so the highlight ranges stay exact.
pub fn replace_matches(regex: &Regex, line: &str, replacement: &str) -> Replaced {
    let mut text = String::with_capacity(line.len());
    let mut spans = Vec::new();
    let mut last = 0;
    let mut expanded = String::new();
    for caps in regex.captures_iter(line) {
        let m = caps.get(0).expect("group 0 always participates");
        text.push_str(&sanitize_for_display(&line[last..m.start()]));
        expanded.clear();
        caps.expand(replacement, &mut expanded);
        let start = text.len();
        text.push_str(&sanitize_for_display(&expanded));
        if text.len() > start {
            spans.push((start, text.len()));
        }
        last = m.end();
    }
    text.push_str(&sanitize_for_display(&line[last..]));
    Replaced { text, spans }
}

/// With -o and --replace: the expansion of each match on its own.
pub fn replaced_matches(regex: &Regex, line: &str, replacement: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut expanded = String::new();
    for caps in regex.captures_iter(line) {
        expanded.clear();
        caps.expand(replacement, &mut expanded);
        out.push(sanitize_for_display(&expanded).into_owned());
    }
    out
}

/// Checks that every group a replacement refers to exists in `regex`.
///
/// The regex crate silently expands an unknown group to nothing, which
/// turns two everyday mistakes into quietly wrong output: a literal dollar
/// amount (`cost $5`, with no fifth group) and a group number followed by
/// text (`$1a` is read as a group *named* `1a`). Both are reported here,
/// with the fix (`$$5`, `${1}a`).
///
/// Syntax, as in the regex crate: `$$` is a literal `$`; `${name}` and
/// `$name` refer to a group, where `name` is the longest run of
/// `[A-Za-z0-9_]`; a `$` followed by anything else is a literal `$`.
pub fn check_replacement(regex: &Regex, replacement: &str) -> Result<(), String> {
    let groups = regex.captures_len(); // including group 0
    let names: Vec<&str> = regex.capture_names().flatten().collect();
    let bytes = replacement.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        if bytes.get(i + 1) == Some(&b'$') {
            i += 2;
            continue;
        }
        let (name, braced, next) = if bytes.get(i + 1) == Some(&b'{') {
            match replacement[i + 2..].find('}') {
                Some(end) => (&replacement[i + 2..i + 2 + end], true, i + 3 + end),
                None => {
                    i += 1;
                    continue;
                }
            }
        } else {
            let len = replacement[i + 1..]
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .count();
            (&replacement[i + 1..i + 1 + len], false, i + 1 + len)
        };
        if name.is_empty() {
            i = next.max(i + 1);
            continue;
        }
        let known = match name.parse::<usize>() {
            Ok(n) => n < groups,
            Err(_) => names.contains(&name),
        };
        if !known {
            let have = match groups - 1 {
                0 => "the pattern has no capture groups".to_string(),
                1 => "the pattern has 1 capture group".to_string(),
                n => format!("the pattern has {n} capture groups"),
            };
            let mut hint = String::new();
            let digits = name.bytes().take_while(u8::is_ascii_digit).count();
            if !braced && digits > 0 && digits < name.len() {
                hint = format!(
                    "; to put text right after a group write ${{{}}}{}",
                    &name[..digits],
                    &name[digits..]
                );
            }
            return Err(format!(
                "replacement refers to group '{name}', but {have}{hint}; \
                 write $$ for a literal $"
            ));
        }
        i = next;
    }
    Ok(())
}

/// Returns true if `path`'s extension marks it as gzip-compressed, the
/// only format -z/--search-compressed currently decompresses: `.gz` or
/// `.tgz` (matching on the extension is a filename heuristic, not
/// content sniffing, same as ripgrep's own -z).
///
/// `.tar.gz`/`.tgz` match this too, and that's deliberate, not an
/// oversight: this function only identifies a gzip *stream*, and a tar
/// archive happens to be one once compressed — there's no reliable way
/// to tell "a tar archive that was gzipped" from "some other gzipped
/// content" from the extension alone without hardcoding archive-format
/// knowledge this function otherwise has no business knowing. What that
/// means in practice: `-z` decompresses the gzip layer, but does not
/// unpack tar members — the search runs over the raw tar byte stream
/// (headers, padding, and all), not over the individual files a tar tool
/// would extract. That's a real, honest limitation, not a bug: full
/// archive support (iterating tar members as separate searchable files)
/// is a different, larger feature this project isn't taking on here.
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
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("gz") | Some("tgz")
    )
}

/// Task sent to workers representing a directory or file to scan
pub struct Task {
    pub path: PathBuf,
    /// Accumulated .gitignore/.ignore matchers from the root down to this
    /// directory's parent, in order (deepest = highest priority, mirroring
    /// git's own precedence for nested ignore files).
    pub ignore_stack: IgnoreStack,
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
    /// -r/--replace: in the output, replace every match on a matching line
    /// with this template (`$0`, `$1`, `${name}`, `$$` for a literal `$`,
    /// as in ripgrep). Only what is printed changes; files are never
    /// written. Validated by [`check_replacement`] before a search starts.
    pub replace: Option<String>,
    /// --max-line-size: lines longer than this many bytes are skipped
    /// (never buffered whole) with a warning, and the file is treated as
    /// incompletely searched (counts as an I/O error, so -L won't list it
    /// and the exit code reflects it). 0 = unlimited. Bounds per-thread
    /// memory for newline-free files (minified blobs, binary-ish data).
    pub max_line_bytes: usize,
    /// --max-decompressed-size: under -z, stop reading a file after this
    /// many decompressed bytes (gzip-bomb protection), with a warning and
    /// the same incomplete-search treatment as above. 0 = unlimited. Has
    /// no effect on uncompressed files.
    pub max_decompressed_bytes: u64,
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
    /// being opened. Also counts special files (FIFOs, sockets, device
    /// nodes) found while walking, which are never opened either. Deliberately does *not* include binary files (they
    /// were opened, just not fully read) or files that failed to open
    /// (that's `io_errors`, a different failure category from "skipped by
    /// our own filtering rules"). Displayed as "Files skipped".
    pub files_skipped: Arc<AtomicUsize>,
    /// Total bytes read from file contents during scanning. Counted
    /// entirely via each read_until call in grep_file's main loop — the
    /// binary-sniff's own initial read is deliberately NOT counted
    /// separately, since those same bytes are replayed through the main
    /// loop via a Cursor and would otherwise be counted twice (a real bug
    /// caught in review; see the comment at the sniff site in grep_file).
    /// Approximated for stdin as line length + 1 per line, since
    /// BufRead::lines() strips the newline it actually consumed.
    /// Displayed as "Bytes read" — mainly useful for benchmarking
    /// throughput.
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
    /// With --replace: byte ranges of `line_content` that hold replacement
    /// text, for highlighting. `None` means "highlight the regex matches",
    /// the normal case.
    pub highlights: Option<Vec<(usize, usize)>>,
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

/// One pattern read from a `-f/--file` pattern file, with where it came
/// from (for error messages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePattern {
    pub pattern: String,
    /// 1-based line number in the file.
    pub line: usize,
}

/// Parses the contents of a `-f/--file` pattern file: one pattern per
/// line. `\n` and `\r\n` line endings both work (a trailing `\r` is never
/// part of a pattern). Empty and whitespace-only lines are skipped.
///
/// Skipping blank lines deliberately differs from GNU grep and ripgrep,
/// where an empty line is an empty pattern that matches *every* line: a
/// stray blank line at the end of a "forbidden strings" list would turn a
/// CI check into "everything is forbidden". Use `-e` to search for
/// whitespace. Lines are otherwise taken verbatim, leading and trailing
/// spaces included; `#` has no special meaning.
pub fn parse_pattern_file(contents: &str) -> Vec<FilePattern> {
    contents
        .split('\n')
        .enumerate()
        .filter_map(|(i, line)| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            (!line.trim().is_empty()).then(|| FilePattern {
                pattern: line.to_string(),
                line: i + 1,
            })
        })
        .collect()
}

/// Builds a single combined regex from multiple patterns (-e/--regexp)
/// — a line matches if it matches ANY of them, same semantics as grep/
/// ripgrep's multiple -e (patterns are combined via alternation, not
/// ANDed together). Each pattern is escaped individually (if
/// fixed_strings) and wrapped in its own non-capturing group before
/// being joined with `|`, so a stray anchor or quantifier in one
/// pattern can't spill into another. -w/-x wrap the WHOLE joined
/// alternation once, not each pattern separately: `\b(?:(?:p1)|(?:p2))\b`
/// requires a word boundary at the edges of whichever alternative
/// actually matched, which is the same semantics GNU grep's own -w
/// gives when combined with multiple -e.
///
/// Deliberately a separate function from build_matcher (rather than
/// build_matcher delegating to this one for the single-pattern case) so
/// the plain single-QUERY code path — and every existing test written
/// against build_matcher's exact behavior — is completely untouched by
/// this function's addition.
pub fn build_matcher_multi(patterns: &[String], opts: MatchOptions) -> Result<Regex, String> {
    if patterns.is_empty() {
        // Unreachable via the CLI in practice (QUERY is required unless
        // --files or -e is given, and -e "being given" implies at least
        // one pattern was collected into this slice) — but an explicit
        // error beats an empty-alternation regex silently compiling into
        // something that matches nothing.
        return Err("at least one pattern is required".to_string());
    }

    let joined = patterns
        .iter()
        .map(|p| {
            let escaped = if opts.fixed_strings {
                regex::escape(p)
            } else {
                p.clone()
            };
            format!("(?:{escaped})")
        })
        .collect::<Vec<_>>()
        .join("|");

    let mut pattern = joined;
    if opts.whole_word {
        pattern = format!(r"\b(?:{})\b", pattern);
    }
    if opts.whole_line {
        pattern = format!(r"^(?:{})$", pattern);
    }

    RegexBuilder::new(&pattern)
        .case_insensitive(opts.ignore_case)
        .build()
        .map_err(|err| format!("Invalid pattern(s) '{}': {}", patterns.join("', '"), err))
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
    let (output_tx, output_rx) = unbounded::<MatchResult>();
    // Results are printed while the walk is still running.
    let printer = thread::spawn(move || {
        for item in output_rx {
            on_match(item);
        }
    });

    let root_task = Task {
        path: root,
        ignore_stack: Arc::new(Vec::new()),
    };
    walk::for_each([root_task], workers, |task, queue| {
        if config.quiet && stats.matched_lines.load(Ordering::Relaxed) > 0 {
            // -q already has its answer: drop the rest of the walk.
            queue.stop();
            return;
        }
        scan_and_grep(task, &config, queue, &output_tx, &stats);
    });

    drop(output_tx);
    printer.join().unwrap();
}

pub fn scan_and_grep(
    task: Task,
    config: &SearchConfig,
    queue: &Queue<Task>,
    output_tx: &crossbeam_channel::Sender<MatchResult>,
    stats: &SearchStats,
) {
    let dir_path = task.path.as_path();

    // Anything other than a directory as the root is an explicitly named
    // file: a regular file, but also a FIFO, /dev/stdin, a device node,
    // etc. Those are read exactly as named, same as grep does, because the
    // user asked for that path. (Subdirectory tasks only ever carry real
    // directories, so in practice this is a root-only check.) fs::metadata
    // follows symlinks, like the old is_file() did. A path that can't be
    // stat'ed falls through to read_dir below and is reported as an I/O
    // error there.
    let root_is_non_dir = matches!(fs::metadata(dir_path), Ok(m) if !m.is_dir());
    if root_is_non_dir {
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
                eprintln!("{}: {}: {}", "argrep".red(), display_path(dir_path), err);
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
            arcommon::ignore::stack_for_dir(&task.ignore_stack, dir_path, |name| {
                entries.iter().any(|e| e.file_name() == name)
            });
        if config.debug {
            for err in errors {
                eprintln!("{}", format!("argrep: {}", err).red());
            }
        }
        stack
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

        // Only directories and regular files are traversed. FIFOs, sockets
        // and device nodes found while walking are skipped: opening a FIFO
        // blocks until a writer appears (one stray `mkfifo` would hang a
        // worker, and so the whole search, forever), and reading a device
        // node is never what a recursive text search means. Same rule as
        // ripgrep. Naming one explicitly as PATH still reads it (see the
        // root check above). Counted as discovered + skipped, so the
        // --stats accounting identity still holds.
        if !is_dir && !file_type.is_file() {
            stats.files_discovered.fetch_add(1, Ordering::Relaxed);
            stats.files_skipped.fetch_add(1, Ordering::Relaxed);
            if config.debug {
                eprintln!(
                    "{}: {}: not a regular file, skipped",
                    "argrep".red(),
                    display_path(&entry.path())
                );
            }
            continue;
        }

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

        if config.respect_gitignore && is_ignored(&ignore_stack, &entry_path, is_dir) {
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
            queue.push(Task {
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
        highlights: None,
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
                eprintln!("{}: {}: {}", "argrep".red(), display_path(file_path), err);
            }
            return;
        }
    };

    // -z/--search-compressed: transparently decompress gzip files. The
    // resulting content stream is boxed so the rest of this function
    // doesn't need to know or care which case it's in.
    //
    // Under -z the decoder is wrapped in Read::take(cap + 1): one byte more
    // than the cap, so "read more than cap bytes" is distinguishable from
    // "the file was exactly cap bytes and ended cleanly" (Take reports both
    // as plain EOF otherwise). See the check in the main loop.
    let decompress = config.search_compressed && is_gzip_target(file_path);
    let decompressed_cap = config.max_decompressed_bytes;
    let content: Box<dyn Read> = if decompress {
        let decoder = GzDecoder::new(file);
        if decompressed_cap > 0 {
            Box::new(decoder.take(decompressed_cap.saturating_add(1)))
        } else {
            Box::new(decoder)
        }
    } else {
        Box::new(file)
    };
    let mut decompressed_total: u64 = 0;
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
                eprintln!("{}: {}: {}", "argrep".red(), display_path(file_path), err);
            }
            return;
        }
    };
    // Deliberately NOT counted into stats.bytes_read here: these bytes are
    // about to be replayed through the Cursor below and will pass through
    // the main read loop's own read_until calls just like the rest of the
    // stream, each of which already adds its byte count to bytes_read (see
    // the `Ok(n) => stats.bytes_read.fetch_add(n, ...)` arm further down).
    // Counting them here too would double-count the first `sniffed` bytes
    // of every file searched — caught in review before merge, see
    // bytes_read_exactly_matches_decompressed_content_length_for_gzip and
    // its plain-file sibling in integration_tests.rs for the regression
    // tests this depends on.
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
        let line_overflowed = match read_line_capped(
            &mut reader,
            &mut line_bytes,
            config.max_line_bytes,
        ) {
            Ok((0, _)) => break, // EOF
            Ok((n, overflowed)) => {
                stats.bytes_read.fetch_add(n, Ordering::Relaxed);
                if decompress && decompressed_cap > 0 {
                    decompressed_total += n as u64;
                    if decompressed_total > decompressed_cap {
                        // Read past the cap: stop, and make sure nobody
                        // mistakes this for a clean, complete search.
                        stats.io_errors.fetch_add(1, Ordering::Relaxed);
                        had_io_error = true;
                        eprintln!(
                            "{}: {}: decompressed size exceeds --max-decompressed-size ({} bytes); rest of file not searched",
                            "argrep".red(),
                            display_path(file_path),
                            decompressed_cap
                        );
                        break;
                    }
                }
                overflowed
            }
            Err(err) => {
                stats.io_errors.fetch_add(1, Ordering::Relaxed);
                had_io_error = true;
                if config.debug {
                    eprintln!("{}: {}: {}", "argrep".red(), display_path(file_path), err);
                }
                break;
            }
        };
        line_num += 1;

        if line_overflowed {
            // Line longer than --max-line-size: only a truncated prefix was
            // kept, and searching a prefix would give wrong answers (-v,
            // -x, $ anchors), so the whole line is skipped. Warn once per
            // file; later oversized lines in the same file are still
            // skipped, just not re-announced.
            if !had_io_error {
                eprintln!(
                    "{}: {}: line {} exceeds --max-line-size ({} bytes); skipping oversized lines",
                    "argrep".red(),
                    display_path(file_path),
                    line_num,
                    config.max_line_bytes
                );
                stats.io_errors.fetch_add(1, Ordering::Relaxed);
            }
            had_io_error = true;
            continue;
        }

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
                    highlights: None,
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
                    if let Some(replacement) = &config.replace {
                        // -o with -r: each match's expansion on its own line.
                        for text in replaced_matches(&config.regex, line, replacement) {
                            let len = text.len();
                            let _ = output_tx.send(MatchResult {
                                file_path: file_path.to_path_buf(),
                                line_num,
                                line_content: text,
                                count: None,
                                is_context: false,
                                is_separator: false,
                                highlights: Some(if len > 0 { vec![(0, len)] } else { vec![] }),
                            });
                        }
                    } else {
                        for m in config.regex.find_iter(line) {
                            let _ = output_tx.send(MatchResult {
                                file_path: file_path.to_path_buf(),
                                line_num,
                                line_content: sanitize_for_display(m.as_str()).to_string(),
                                count: None,
                                is_context: false,
                                is_separator: false,
                                highlights: None,
                            });
                        }
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
                                highlights: None,
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
                                    highlights: None,
                                });
                                last_printed_line = b_num;
                            }
                        }
                    }

                    // Normal mode: emit matching line (with -r, the
                    // replaced line; context lines stay as they are).
                    let (line_content, highlights) = match &config.replace {
                        Some(replacement) => {
                            let r = replace_matches(&config.regex, line, replacement);
                            (r.text, Some(r.spans))
                        }
                        None => (clean_line.to_string(), None),
                    };
                    let _ = output_tx.send(MatchResult {
                        file_path: file_path.to_path_buf(),
                        line_num,
                        line_content,
                        count: None,
                        is_context: false,
                        is_separator: false,
                        highlights,
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
                    highlights: None,
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
            highlights: None,
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
            highlights: None,
        });
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

#[cfg(test)]
mod is_gzip_target_tests {
    // Locks in is_gzip_target's actual, documented behavior directly, so
    // the doc comment and the implementation can't silently drift apart
    // again — this is exactly the kind of contradiction a reviewer
    // caught between the README and the code before this test existed:
    // the doc comment claimed .tar.gz was "deliberately excluded" while
    // the implementation (extension == "gz") always matched it anyway.

    use super::is_gzip_target;
    use std::path::Path;

    #[test]
    fn plain_gz_extension_matches() {
        assert!(is_gzip_target(Path::new("app.log.gz")));
        assert!(is_gzip_target(Path::new("data.gz")));
    }

    #[test]
    fn tar_gz_matches_too_deliberately() {
        // Documented, not a bug: -z decompresses the gzip layer of a
        // .tar.gz, it just doesn't unpack the tar members underneath.
        assert!(is_gzip_target(Path::new("archive.tar.gz")));
    }

    #[test]
    fn tgz_matches_as_an_alternate_spelling_of_tar_gz() {
        assert!(is_gzip_target(Path::new("archive.tgz")));
        assert!(is_gzip_target(Path::new("backup.tgz")));
    }

    #[test]
    fn unrelated_extensions_do_not_match() {
        assert!(!is_gzip_target(Path::new("plain.txt")));
        assert!(!is_gzip_target(Path::new("archive.zip")));
        assert!(!is_gzip_target(Path::new("no_extension_at_all")));
    }
}

#[cfg(test)]
mod build_matcher_multi_tests {
    // -e/--regexp: a line matches if it matches ANY of several patterns
    // (alternation, not AND). These tests exercise build_matcher_multi
    // directly rather than only through grep_file/parallel_grep, since
    // the interesting part — how patterns combine, how fixed_strings/
    // whole_word/whole_line apply across all of them at once — is fully
    // determined by the regex this function builds.

    use super::{MatchOptions, build_matcher_multi};

    #[test]
    fn empty_pattern_list_is_an_error() {
        assert!(build_matcher_multi(&[], MatchOptions::default()).is_err());
    }

    #[test]
    fn single_pattern_behaves_like_a_plain_search() {
        let re = build_matcher_multi(&["needle".to_string()], MatchOptions::default()).unwrap();
        assert!(re.is_match("a needle in a haystack"));
        assert!(!re.is_match("nothing here"));
    }

    #[test]
    fn multiple_patterns_match_if_any_one_matches() {
        let re = build_matcher_multi(
            &["TODO".to_string(), "FIXME".to_string(), "HACK".to_string()],
            MatchOptions::default(),
        )
        .unwrap();
        assert!(re.is_match("// TODO: clean this up"));
        assert!(re.is_match("// FIXME later"));
        assert!(re.is_match("// HACK around the bug"));
        assert!(!re.is_match("// nothing notable here"));
    }

    #[test]
    fn one_pattern_matching_does_not_require_all_to_match() {
        // The core "OR, not AND" contract: a line containing only ONE of
        // several -e patterns must still match, not be rejected for
        // lacking the others.
        let re = build_matcher_multi(
            &["error".to_string(), "panic".to_string()],
            MatchOptions::default(),
        )
        .unwrap();
        assert!(
            re.is_match("a panic occurred"),
            "should match on 'panic' alone"
        );
        assert!(
            re.is_match("an error occurred"),
            "should match on 'error' alone"
        );
    }

    #[test]
    fn fixed_strings_escapes_each_pattern_individually() {
        // Each pattern must be escaped on its own, before joining — if
        // the patterns were joined first and escaped as one blob, the
        // "|" this function inserts as a separator would itself get
        // escaped into a literal pipe character, breaking the OR.
        let re = build_matcher_multi(
            &["a.b".to_string(), "c(d)".to_string()],
            MatchOptions {
                fixed_strings: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(re.is_match("literal a.b text"));
        assert!(
            !re.is_match("aXb"),
            "'.' must be literal, not \"any char\", under fixed_strings"
        );
        assert!(re.is_match("literal c(d) text"));
        assert!(
            !re.is_match("cXd"),
            "parens must be literal, not a capture group"
        );
    }

    #[test]
    fn whole_word_wraps_the_entire_alternation_once() {
        let re = build_matcher_multi(
            &["cat".to_string(), "dog".to_string()],
            MatchOptions {
                whole_word: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(re.is_match("the cat sat"));
        assert!(re.is_match("the dog ran"));
        assert!(
            !re.is_match("category"),
            "-w must still require a word boundary, even with multiple -e patterns"
        );
    }

    #[test]
    fn whole_line_wraps_the_entire_alternation_once() {
        let re = build_matcher_multi(
            &["cat".to_string(), "dog".to_string()],
            MatchOptions {
                whole_line: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(re.is_match("cat"));
        assert!(re.is_match("dog"));
        assert!(
            !re.is_match("the cat sat"),
            "-x must still require the whole line to match"
        );
    }

    #[test]
    fn an_anchor_in_one_pattern_does_not_spill_into_another() {
        // Each pattern is wrapped in its own (?:...) before joining, so
        // "^foo" only anchors "foo", not "foo|bar" as a whole — without
        // that grouping, "^foo|bar" would parse as "(^foo)|(bar)" in
        // standard regex precedence anyway, but this test pins down that
        // build_matcher_multi actually produces that grouping rather
        // than accidentally relying on it.
        let re = build_matcher_multi(
            &["^foo".to_string(), "bar".to_string()],
            MatchOptions::default(),
        )
        .unwrap();
        assert!(re.is_match("foo at the start"));
        assert!(
            re.is_match("something bar anywhere"),
            "'bar' must match anywhere, unanchored"
        );
        assert!(!re.is_match("something foo not at start"));
    }

    #[test]
    fn invalid_pattern_among_several_is_reported_as_an_error() {
        let result = build_matcher_multi(
            &["fine".to_string(), "[unclosed".to_string()],
            MatchOptions::default(),
        );
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod pattern_file_tests {
    use super::{FilePattern, parse_pattern_file};

    fn pats(s: &str) -> Vec<(String, usize)> {
        parse_pattern_file(s)
            .into_iter()
            .map(|FilePattern { pattern, line }| (pattern, line))
            .collect()
    }

    #[test]
    fn one_pattern_per_line_with_line_numbers() {
        assert_eq!(
            pats("TODO\nFIXME\nHACK\n"),
            [("TODO".into(), 1), ("FIXME".into(), 2), ("HACK".into(), 3)]
        );
    }

    #[test]
    fn blank_and_whitespace_only_lines_are_skipped() {
        assert_eq!(
            pats("\nTODO\n   \n\t\n\nFIXME\n\n"),
            [("TODO".into(), 2), ("FIXME".into(), 6)]
        );
        assert!(parse_pattern_file("").is_empty());
        assert!(parse_pattern_file("\n\n  \n").is_empty());
    }

    #[test]
    fn crlf_and_missing_final_newline() {
        assert_eq!(
            pats("a\r\nb\r\nc"),
            [("a".into(), 1), ("b".into(), 2), ("c".into(), 3)]
        );
    }

    #[test]
    fn patterns_are_verbatim_otherwise() {
        assert_eq!(
            pats(" lead\ntrail \n# not a comment\n\\d+\n"),
            [
                (" lead".into(), 1),
                ("trail ".into(), 2),
                ("# not a comment".into(), 3),
                ("\\d+".into(), 4)
            ]
        );
    }
}

#[cfg(test)]
mod replace_tests {
    use super::{
        MatchOptions, build_matcher, build_matcher_multi, check_replacement, replace_matches,
        replaced_matches,
    };

    fn re(p: &str) -> regex::Regex {
        build_matcher(p, MatchOptions::default()).unwrap()
    }

    #[test]
    fn replaces_every_non_overlapping_match_and_records_spans() {
        let r = replace_matches(&re("foo"), "foo bar foo", "X");
        assert_eq!(r.text, "X bar X");
        assert_eq!(r.spans, [(0, 1), (6, 7)]);
    }

    #[test]
    fn groups_names_whole_match_and_dollar_escape() {
        let r = replace_matches(&re(r"(\w+)@(\w+)"), "alice@host x", "$2 at $1");
        assert_eq!(r.text, "host at alice x");
        let r = replace_matches(&re(r"(?P<u>\w+)@(?P<h>\w+)"), "a@b", "${h}/${u}");
        assert_eq!(r.text, "b/a");
        assert_eq!(replace_matches(&re("o"), "foo", "[$0]").text, "f[o][o]");
        assert_eq!(
            replace_matches(&re(r"(\d+)"), "cost 5", "$$$1").text,
            "cost $5"
        );
        assert_eq!(replace_matches(&re(r"(\d)"), "7", "${1}a").text, "7a");
    }

    #[test]
    fn empty_replacement_deletes_and_leaves_no_span() {
        let r = replace_matches(&re("foo"), "foo bar foo", "");
        assert_eq!(r.text, " bar ");
        assert!(r.spans.is_empty());
    }

    #[test]
    fn fixed_strings_and_multi_patterns() {
        let f = build_matcher(
            "unwrap()",
            MatchOptions {
                fixed_strings: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            replace_matches(&f, "x.unwrap()", "expect(\"..\")").text,
            "x.expect(\"..\")"
        );
        // -e patterns are wrapped in non-capturing groups, so the user's
        // group numbers are kept across alternatives.
        let m = build_matcher_multi(
            &[r"(\d+)px".to_string(), r"(\d+)em".to_string()],
            MatchOptions::default(),
        )
        .unwrap();
        assert_eq!(replace_matches(&m, "4px 2em", "<$1|$2>").text, "<4|> <|2>");
    }

    #[test]
    fn unicode_and_control_characters() {
        assert_eq!(replace_matches(&re("ö"), "töt", "oe").text, "toet");
        // The inserted text is sanitized like everything else printed.
        let r = replace_matches(&re("x"), "axb", "\u{1b}[31m");
        assert_eq!(r.text, "a\\x1b[31mb");
        assert_eq!(r.spans, [(1, 9)]);
    }

    #[test]
    fn only_matching_variant_returns_one_expansion_per_match() {
        assert_eq!(
            replaced_matches(&re(r"(\w+)@(\w+)"), "a@b c@d", "$2"),
            ["b", "d"]
        );
    }

    #[test]
    fn replacement_validation() {
        let one = re(r"(\w+)");
        assert!(check_replacement(&one, "$1 $0 ${1} $$ $$5 plain").is_ok());
        assert!(check_replacement(&one, "trailing $").is_ok());
        assert!(check_replacement(&one, "$-x ${unclosed").is_ok());
        let err = check_replacement(&one, "cost $5").unwrap_err();
        assert!(err.contains("group '5'") && err.contains("$$"), "{err}");
        let err = check_replacement(&one, "$1a").unwrap_err();
        assert!(err.contains("${1}a"), "{err}");
        assert!(check_replacement(&one, "${2}").is_err());
        assert!(
            check_replacement(&re("x"), "$1")
                .unwrap_err()
                .contains("no capture groups")
        );
        let named = re(r"(?P<host>\w+)");
        assert!(check_replacement(&named, "${host} $host $1").is_ok());
        assert!(check_replacement(&named, "$hostname").is_err());
    }
}

#[cfg(test)]
mod size_limit_tests {
    use super::{parse_size, read_line_capped};
    use std::io::Cursor;

    #[test]
    fn parse_size_handles_suffixes() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("100"), Ok(100));
        assert_eq!(parse_size("2k"), Ok(2048));
        assert_eq!(parse_size("4M"), Ok(4 * 1024 * 1024));
        assert_eq!(parse_size("1G"), Ok(1024 * 1024 * 1024));
        assert!(parse_size("").is_err());
        assert!(parse_size("M").is_err());
        assert!(parse_size("1.5M").is_err());
        assert!(parse_size("99999999999999999999G").is_err());
    }

    #[test]
    fn short_lines_are_read_whole() {
        let mut r = Cursor::new(b"abc\ndef\n".to_vec());
        let mut buf = Vec::new();
        assert_eq!(read_line_capped(&mut r, &mut buf, 10).unwrap(), (4, false));
        assert_eq!(buf, b"abc\n");
    }

    #[test]
    fn long_line_is_bounded_but_fully_consumed() {
        let mut data = vec![b'A'; 100_000];
        data.extend_from_slice(b"\nnext\n");
        let mut r = Cursor::new(data);
        let mut buf = Vec::new();
        let (n, overflow) = read_line_capped(&mut r, &mut buf, 16).unwrap();
        assert_eq!(n, 100_001);
        assert!(overflow);
        assert!(
            buf.len() <= 18,
            "buffer must stay bounded, got {}",
            buf.len()
        );
        buf.clear();
        assert_eq!(read_line_capped(&mut r, &mut buf, 16).unwrap(), (5, false));
        assert_eq!(buf, b"next\n");
    }

    #[test]
    fn line_exactly_at_limit_is_not_overflow() {
        let mut r = Cursor::new(b"abcd\r\n".to_vec());
        let mut buf = Vec::new();
        assert_eq!(read_line_capped(&mut r, &mut buf, 4).unwrap(), (6, false));
    }

    #[test]
    fn zero_means_unlimited() {
        let mut r = Cursor::new(vec![b'x'; 50_000]);
        let mut buf = Vec::new();
        let (n, overflow) = read_line_capped(&mut r, &mut buf, 0).unwrap();
        assert_eq!((n, overflow, buf.len()), (50_000, false, 50_000));
    }
}
