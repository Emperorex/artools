use argrep::{
    DEFAULT_IGNORES, MatchOptions, SearchConfig, SearchStats, build_matcher, grep_file,
    parallel_grep,
};
use flate2::{Compression, write::GzEncoder};
use glob::Pattern;
use std::sync::Arc as StdArc;
use std::{
    collections::HashSet,
    fs::{self, File},
    io::Write as _,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::AtomicUsize, atomic::Ordering},
};
use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Creates a temp directory tree. `files` is a list of (relative_path, content) pairs.
fn make_tree(files: &[(&str, &str)]) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for (rel, content) in files {
        let full = dir.path().join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full, content).unwrap();
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

/// Gzip-compresses `content` for -z/--search-compressed test fixtures.
fn gzip_bytes(content: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(content).unwrap();
    encoder.finish().unwrap()
}

fn default_config(query: &str, ignore_case: bool) -> std::sync::Arc<argrep::SearchConfig> {
    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    StdArc::new(SearchConfig {
        regex: build_matcher(
            query,
            MatchOptions {
                fixed_strings: false,
                ignore_case,
                ..Default::default()
            },
        )
        .unwrap(),
        query: query.to_string(),
        ignore_case,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    })
}

/// Runs parallel_grep and returns (matched_file_names, matched_line_contents) sorted.
fn collect_matches(root: PathBuf, query: &str, ignore_case: bool) -> (Vec<String>, Vec<String>) {
    let config = default_config(query, ignore_case);
    let stats = SearchStats::new();

    let file_names: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let line_contents: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fn_clone = Arc::clone(&file_names);
    let lc_clone = Arc::clone(&line_contents);

    parallel_grep(root, 4, config, stats, move |item| {
        fn_clone.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
        lc_clone
            .lock()
            .unwrap()
            .push(item.line_content.trim_end().to_string());
    });

    let mut names = file_names.lock().unwrap().clone();
    let mut lines = line_contents.lock().unwrap().clone();
    names.sort();
    lines.sort();
    (names, lines)
}

// ── Basic matching ────────────────────────────────────────────────────────────

#[test]
fn finds_exact_match_in_single_file() {
    let (_dir, root) = make_tree(&[("file.txt", "hello world\nfoo bar\n")]);
    let (names, lines) = collect_matches(root, "hello", false);
    assert_eq!(names, vec!["file.txt"]);
    assert_eq!(lines, vec!["hello world"]);
}

#[test]
fn finds_matches_across_multiple_files() {
    let (_dir, root) = make_tree(&[
        ("a.txt", "needle here\n"),
        ("b.txt", "nothing here\n"),
        ("c.txt", "another needle\n"),
    ]);
    let (names, _) = collect_matches(root, "needle", false);
    assert_eq!(names, vec!["a.txt", "c.txt"]);
}

#[test]
fn no_match_returns_empty() {
    let (_dir, root) = make_tree(&[("file.txt", "hello world\n")]);
    let (names, lines) = collect_matches(root, "zzznomatch", false);
    assert!(names.is_empty());
    assert!(lines.is_empty());
}

#[test]
fn finds_multiple_matching_lines_in_one_file() {
    let (_dir, root) = make_tree(&[("file.txt", "match one\nskip\nmatch two\n")]);
    let (_, lines) = collect_matches(root, "match", false);
    assert_eq!(lines, vec!["match one", "match two"]);
}

#[test]
fn finds_match_in_subdirectory() {
    let (_dir, root) = make_tree(&[("sub/deep.txt", "hidden needle\n")]);
    let (names, lines) = collect_matches(root, "needle", false);
    assert_eq!(names, vec!["deep.txt"]);
    assert_eq!(lines, vec!["hidden needle"]);
}

// ── Case-insensitive search ───────────────────────────────────────────────────

#[test]
fn case_insensitive_matches_uppercase() {
    let (_dir, root) = make_tree(&[("file.txt", "TODO: fix this\n")]);
    let (names, lines) = collect_matches(root, "todo", true);
    assert_eq!(names, vec!["file.txt"]);
    assert_eq!(lines, vec!["TODO: fix this"]);
}

#[test]
fn case_insensitive_matches_mixed_case() {
    let (_dir, root) = make_tree(&[("file.txt", "RuSt Is Great\n")]);
    let (names, _) = collect_matches(root, "rust", true);
    assert_eq!(names, vec!["file.txt"]);
}

#[test]
fn case_sensitive_does_not_match_wrong_case() {
    let (_dir, root) = make_tree(&[("file.txt", "TODO: fix this\n")]);
    let (names, _) = collect_matches(root, "todo", false);
    assert!(names.is_empty());
}

// ── Binary file skipping ──────────────────────────────────────────────────────

#[test]
fn binary_file_with_null_byte_is_skipped() {
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    // Write a file containing a null byte — looks binary
    let binary_content = b"some text\x00binary data\nfoo\n";
    fs::write(root.join("binary.bin"), binary_content).unwrap();

    let (names, _) = collect_matches(root, "foo", false);
    assert!(names.is_empty(), "binary file should be skipped");
}

#[test]
fn text_file_without_null_bytes_is_searched() {
    let (_dir, root) = make_tree(&[("text.txt", "no null bytes here\nfoo bar\n")]);
    let (names, _) = collect_matches(root, "foo", false);
    assert_eq!(names, vec!["text.txt"]);
}

// ── -z / --search-compressed ────────────────────────────────────────────────
//
// Gzip decompression on the fly, by .gz extension. Covers: content is
// found when search_compressed is set, .gz files are treated as binary
// (skipped) by default when it's not, binary detection still applies to
// the *decompressed* bytes, a corrupted/non-gzip .gz file is an io_error
// rather than a panic, and bytes_read reflects decompressed content.

fn compressed_config(query: &str, search_compressed: bool) -> StdArc<SearchConfig> {
    StdArc::new(SearchConfig {
        regex: build_matcher(query, MatchOptions::default()).unwrap(),
        query: query.to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed,
        quiet: false,
        only_matching: false,
        max_count: None,
    })
}

#[test]
fn gzip_content_is_searched_when_search_compressed_is_set() {
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let plaintext = "line one\nneedle is here\nline three\n";
    fs::write(root.join("app.log.gz"), gzip_bytes(plaintext.as_bytes())).unwrap();

    let config = compressed_config("needle", true);
    let stats = SearchStats::new();
    let (output_tx, output_rx) = crossbeam_channel::unbounded();
    grep_file(&root.join("app.log.gz"), &config, &output_tx, &stats);

    let result = output_rx.try_recv().expect("the match must be found");
    assert_eq!(result.line_content, "needle is here");
    assert_eq!(
        result.line_num, 2,
        "line numbers must count decompressed lines, not compressed bytes"
    );
}

#[test]
fn gzip_files_are_treated_as_binary_by_default() {
    // Without -z, a .gz file is just opaque compressed bytes to grep_file
    // — it must be skipped by the ordinary binary sniff, exactly like any
    // other binary file, not silently searched as raw compressed garbage.
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let plaintext = "needle is here, but compressed\n".repeat(20); // long enough that the sniff window (1024 bytes) is well within the compressed stream
    let compressed = gzip_bytes(plaintext.as_bytes());
    assert!(
        compressed[..compressed.len().min(1024)].contains(&0u8),
        "test precondition: this fixture's compressed bytes must contain \
         a null byte within the first 1024 bytes, or this test isn't \
         actually exercising the binary sniff"
    );
    fs::write(root.join("app.log.gz"), &compressed).unwrap();

    let config = compressed_config("needle", false); // -z NOT set
    let stats = SearchStats::new();
    let (output_tx, output_rx) = crossbeam_channel::unbounded();
    grep_file(&root.join("app.log.gz"), &config, &output_tx, &stats);

    assert!(
        output_rx.try_recv().is_err(),
        "a .gz file must be skipped as binary when -z isn't set, not \
         searched as raw compressed bytes"
    );
}

#[test]
fn binary_content_inside_a_gzip_file_is_still_skipped() {
    // -z decompresses, but the result is still subject to the same
    // binary sniff as anything else — a gzip-compressed binary file must
    // not suddenly become "searchable" just because it decompresses.
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let binary_plaintext = b"some text\x00binary data\nneedle\n";
    fs::write(root.join("data.bin.gz"), gzip_bytes(binary_plaintext)).unwrap();

    let config = compressed_config("needle", true);
    let stats = SearchStats::new();
    let (output_tx, output_rx) = crossbeam_channel::unbounded();
    grep_file(&root.join("data.bin.gz"), &config, &output_tx, &stats);

    assert!(
        output_rx.try_recv().is_err(),
        "decompressed content with a null byte must still be skipped as \
         binary, the same as an uncompressed file would be"
    );
}

#[test]
fn invalid_gzip_file_is_an_io_error_not_a_panic() {
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    // A .gz-named file that isn't actually gzip-compressed data.
    fs::write(
        root.join("not_really_gzip.gz"),
        b"just plain text, no gzip header\n",
    )
    .unwrap();

    let config = compressed_config("text", true);
    let stats = SearchStats::new();
    let (output_tx, output_rx) = crossbeam_channel::unbounded();
    grep_file(
        &root.join("not_really_gzip.gz"),
        &config,
        &output_tx,
        &stats,
    );

    assert!(
        output_rx.try_recv().is_err(),
        "an invalid gzip file must never be reported as a match"
    );
    assert!(
        stats.io_errors.load(Ordering::Relaxed) > 0,
        "an invalid gzip file must be counted as an io error, not \
         silently skipped or (worse) treated as literal searchable text"
    );
}

#[test]
fn bytes_read_exactly_matches_plain_file_content_length_not_double_counted() {
    // The general form of the regression this guards against: the sniff
    // step reads up to the first 1024 bytes to check for binary content,
    // and those same bytes are then replayed through the main read loop
    // via a Cursor (see grep_file) rather than re-read from disk. If the
    // sniff's own read were *also* added to bytes_read (it was, briefly,
    // during review of the -z change below), every file's count would be
    // inflated by up to 1024 bytes — not a compression-specific bug, but
    // one the -z refactor's shared sniff/chain code path introduced for
    // every file, gzip or not. A loose ">=" assertion wouldn't catch
    // this (double-counting only makes the number bigger, which such an
    // assertion would wave through) — exact equality is the point here.
    let content = "needle and some surrounding text\nsecond line\n";
    let (_dir, root) = make_tree(&[("plain.txt", content)]);

    let config = compressed_config("needle", false); // -z not relevant here
    let stats = SearchStats::new();
    let (output_tx, _output_rx) = crossbeam_channel::unbounded();
    grep_file(&root.join("plain.txt"), &config, &output_tx, &stats);

    assert_eq!(
        stats.bytes_read.load(Ordering::Relaxed),
        content.len(),
        "bytes_read must exactly equal the file's content length — not \
         more (double-counted), not less (truncated)"
    );
}

#[test]
fn bytes_read_exactly_matches_decompressed_content_length_for_gzip() {
    // Same regression, specifically for the -z path this bug was found
    // in review of: the sniffed prefix must be counted exactly once as
    // it's replayed through the main loop, not once at the sniff site
    // AND again when the Cursor replays it into read_until.
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let plaintext = "needle and quite a lot of surrounding text so the \
                      decompressed size is clearly larger than the \
                      compressed size\n"
        .repeat(5);
    let compressed = gzip_bytes(plaintext.as_bytes());
    fs::write(root.join("app.log.gz"), &compressed).unwrap();

    let config = compressed_config("needle", true);
    let stats = SearchStats::new();
    let (output_tx, _output_rx) = crossbeam_channel::unbounded();
    grep_file(&root.join("app.log.gz"), &config, &output_tx, &stats);

    assert_eq!(
        stats.bytes_read.load(Ordering::Relaxed),
        plaintext.len(),
        "bytes_read must exactly equal the *decompressed* content length \
         ({} bytes) — neither the smaller compressed size on disk ({} \
         bytes) nor an inflated, double-counted figure",
        plaintext.len(),
        compressed.len()
    );
}

/// The core end-to-end contract of -z, stated as directly as possible:
/// identical content, once plain and once gzip-compressed, searched both
/// with and without -z. This is the test the CLI-parsing unit tests
/// (search_compressed_flag_defaults_to_false and friends) don't cover on
/// their own — they prove -z reaches SearchConfig, not that it actually
/// changes search behavior.
#[test]
fn dash_z_end_to_end_plain_vs_gzip_with_and_without_the_flag() {
    let content = "hello\nERROR something\n";
    let (_dir, root) = make_tree(&[("plain.log", content)]);
    let gz_path = root.join("compressed.log.gz");
    fs::write(&gz_path, gzip_bytes(content.as_bytes())).unwrap();

    // Plain file: always searchable, -z irrelevant to it either way.
    for search_compressed in [false, true] {
        let config = compressed_config("ERROR", search_compressed);
        let stats = SearchStats::new();
        let (output_tx, output_rx) = crossbeam_channel::unbounded();
        grep_file(&root.join("plain.log"), &config, &output_tx, &stats);
        assert!(
            output_rx.try_recv().is_ok(),
            "the plain file must match regardless of -z (search_compressed={search_compressed})"
        );
    }

    // Gzip file without -z: opaque compressed bytes, skipped as binary.
    let config = compressed_config("ERROR", false);
    let stats = SearchStats::new();
    let (output_tx, output_rx) = crossbeam_channel::unbounded();
    grep_file(&gz_path, &config, &output_tx, &stats);
    assert!(
        output_rx.try_recv().is_err(),
        "without -z, a .gz file must NOT match — it's opaque compressed \
         bytes to grep_file, not searchable text"
    );
    assert_eq!(stats.matched_lines.load(Ordering::Relaxed), 0);

    // Gzip file with -z: decompressed and searched, same as the plain file.
    let config = compressed_config("ERROR", true);
    let stats = SearchStats::new();
    let (output_tx, output_rx) = crossbeam_channel::unbounded();
    grep_file(&gz_path, &config, &output_tx, &stats);
    let result = output_rx
        .try_recv()
        .expect("with -z, the .gz file must match, same as the plain file does");
    assert_eq!(result.line_content, "ERROR something");
    assert_eq!(stats.matched_lines.load(Ordering::Relaxed), 1);
}

// A line with invalid UTF-8 bytes has no NUL byte, so the binary sniffer
// (first 1024 bytes, NUL check only) waves it through as "text" — it must
// not then silently truncate the scan. grep_file used to read lines with
// read_line() in a `while let Ok(...)` loop; read_line() returns Err on
// invalid UTF-8, and that Err was indistinguishable from EOF to the loop,
// so every line after the bad one — including real matches — was dropped
// with a clean, silent exit (exit code 0, no error, no missing lines).
#[test]
fn invalid_utf8_line_does_not_truncate_remaining_matches() {
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    // No NUL byte anywhere, so this file is not detected as binary — but
    // the middle line is not valid UTF-8 (0xFF is never a valid UTF-8 lead
    // byte), and there's a real match both before and after it.
    let mut content = Vec::new();
    content.extend_from_slice(b"match line one\n");
    content.extend_from_slice(&[0xFF, 0xFE, b'\n']);
    content.extend_from_slice(b"match line two\n");
    fs::write(root.join("mixed.txt"), &content).unwrap();

    let (names, lines) = collect_matches(root, "match", false);
    assert_eq!(
        names,
        vec!["mixed.txt", "mixed.txt"],
        "both matches around the invalid-UTF-8 line must still be found, \
         not silently dropped after the bad line"
    );
    assert_eq!(
        lines,
        vec!["match line one", "match line two"],
        "scanning must continue past an invalid-UTF-8 line instead of \
         stopping there"
    );
}

// ── I/O errors ───────────────────────────────────────────────────────────────
//
// Contract: an unreadable file must not silently vanish from the result as
// if it simply had no matches. The scan continues past it (other files are
// still searched and their matches still reported), but the caller must be
// able to tell the result set is incomplete. main.rs uses io_errors to set
// a nonzero exit code for exactly this reason — for a grep-like tool,
// "found nothing" and "couldn't read everything" must not look the same.

#[cfg(unix)]
#[test]
fn unreadable_file_is_skipped_but_other_matches_are_still_found() {
    use std::os::unix::fs::PermissionsExt;

    let (_dir, root) = make_tree(&[
        ("readable_one.txt", "needle before\n"),
        ("blocked.txt", "needle hidden here\n"),
        ("readable_two.txt", "needle after\n"),
    ]);

    let blocked_path = root.join("blocked.txt");
    fs::set_permissions(&blocked_path, fs::Permissions::from_mode(0o000)).unwrap();

    // Running as root (some CI containers do) ignores permission bits
    // entirely, so this scenario can't be exercised there — skip rather
    // than fail on an environment we can't control.
    if File::open(&blocked_path).is_ok() {
        fs::set_permissions(&blocked_path, fs::Permissions::from_mode(0o644)).unwrap();
        eprintln!(
            "skipping unreadable_file_is_skipped_but_other_matches_are_still_found: \
             running as a user that ignores file permissions (e.g. root)"
        );
        return;
    }

    let config = default_config("needle", false);
    let stats = SearchStats::new();
    let file_names: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fn_clone = Arc::clone(&file_names);

    parallel_grep(root, 4, config, stats.clone(), move |item| {
        fn_clone.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
    });

    // Restore permissions so the temp dir can be cleaned up.
    let _ = fs::set_permissions(&blocked_path, fs::Permissions::from_mode(0o644));

    let mut names = file_names.lock().unwrap().clone();
    names.sort();
    assert_eq!(
        names,
        vec!["readable_one.txt", "readable_two.txt"],
        "the readable files' matches must still be reported even though \
         one file in the tree was unreadable"
    );
    assert!(
        stats.io_errors.load(Ordering::Relaxed) > 0,
        "an unreadable file must be counted as an io error, not silently \
         treated as a file with no matches"
    );
}

// grep_file used to rewind with seek(SeekFrom::Start(0)) after the
// binary-sniff read, which meant a FIFO — readable, but not seekable —
// would fail with ESPIPE and get counted as an io_error, never actually
// searched. That was replaced (to support -z's non-seekable decompressed
// streams) with a chain-based sniff that never seeks at all, which fixes
// FIFOs as a side effect: this test now confirms they're fully readable,
// where it previously confirmed the opposite (that seeking on one failed
// and was handled gracefully). A plain BufReader<File> over a regular
// file was never affected either way — a FIFO is just the reliable way
// to exercise a non-seekable Read source at all.
#[cfg(unix)]
#[test]
fn fifo_content_is_searched_without_a_seek_error() {
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let fifo_path = root.join("pipe");

    let status = std::process::Command::new("mkfifo")
        .arg(&fifo_path)
        .status()
        .expect("mkfifo must be available on this system");
    assert!(status.success(), "mkfifo failed to create the test fifo");

    // Opening a FIFO for reading blocks until a writer connects, so write
    // from a background thread while grep_file reads on the main thread.
    let writer_path = fifo_path.clone();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open(&writer_path)
            .unwrap();
        f.write_all(b"needle in a pipe, no null bytes here\n")
            .unwrap();
        // Drop here closes the write end once the bytes are flushed to the
        // pipe buffer, which is fine: the reader only needs those bytes.
    });

    let config = default_config("needle", false);
    let stats = SearchStats::new();
    let (output_tx, output_rx) = crossbeam_channel::unbounded();

    grep_file(&fifo_path, &config, &output_tx, &stats);

    writer.join().unwrap();

    assert_eq!(
        stats.io_errors.load(Ordering::Relaxed),
        0,
        "reading a FIFO must not produce an io_error now that grep_file \
         doesn't seek at all"
    );
    let result = output_rx.try_recv().expect("the match must be found");
    assert_eq!(result.line_content, "needle in a pipe, no null bytes here");
}

// ── Symlinks ─────────────────────────────────────────────────────────────────
//
// Contract (fixed here as a regression test, not just an implementation
// detail that a future traversal optimization could quietly change):
//
//   recursive traversal:   don't follow symlinks (avoids reference cycles)
//   explicit path argument: follow the symlink
//
// Concretely: `argrep foo directory/` must not descend into a symlinked
// subdirectory it discovers while walking, but `argrep foo the-symlink`
// (or `argrep foo the-symlink-to-a-dir`), where the symlink itself is the
// path the user named, must work exactly as if they'd named the real
// target.

#[cfg(unix)]
#[test]
fn symlinked_subdirectory_found_during_traversal_is_not_followed() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    // The real content lives outside the tree we actually search, reachable
    // only via a symlink placed inside it.
    fs::create_dir_all(root.join("real_target")).unwrap();
    fs::write(root.join("real_target/data.txt"), "needle inside target\n").unwrap();

    fs::create_dir_all(root.join("search_here")).unwrap();
    symlink(
        root.join("real_target"),
        root.join("search_here/link_to_target"),
    )
    .unwrap();

    let (names, _) = collect_matches(root.join("search_here"), "needle", false);
    assert!(
        names.is_empty(),
        "a symlinked directory discovered during traversal must not be \
         followed — found matches: {:?}",
        names
    );
}

#[cfg(unix)]
#[test]
fn explicit_symlink_to_file_argument_is_followed() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    fs::write(root.join("target.txt"), "needle in the real file\n").unwrap();
    let link_path = root.join("link.txt");
    symlink(root.join("target.txt"), &link_path).unwrap();

    // The symlink itself is the path the user named on the command line —
    // this must be followed, unlike a symlink merely encountered while
    // walking a directory.
    let (names, lines) = collect_matches(link_path, "needle", false);
    assert_eq!(
        names,
        vec!["link.txt"],
        "an explicitly-named symlink-to-file must be followed and searched"
    );
    assert_eq!(lines, vec!["needle in the real file"]);
}

#[cfg(unix)]
#[test]
fn explicit_symlink_to_directory_argument_is_followed() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    fs::create_dir_all(root.join("real_dir")).unwrap();
    fs::write(root.join("real_dir/data.txt"), "needle via linked root\n").unwrap();
    let link_path = root.join("link_dir");
    symlink(root.join("real_dir"), &link_path).unwrap();

    // Here the symlink IS the root path argument (not something found
    // mid-traversal) — it must be followed and scanned like a real
    // directory, same as GNU grep / ripgrep treat an explicitly-given path.
    let (names, lines) = collect_matches(link_path, "needle", false);
    assert_eq!(
        names,
        vec!["data.txt"],
        "an explicitly-named symlink-to-directory must be followed and \
         its contents searched"
    );
    assert_eq!(lines, vec!["needle via linked root"]);
}

// ── Hidden file and directory skipping ───────────────────────────────────────

#[test]
fn hidden_files_are_skipped() {
    let (_dir, root) = make_tree(&[
        (".secret", "needle inside hidden file\n"),
        ("visible.txt", "no match here\n"),
    ]);
    let (names, _) = collect_matches(root, "needle", false);
    assert!(names.is_empty(), ".secret should be skipped");
}

#[test]
fn hidden_directories_are_skipped() {
    let (_dir, root) = make_tree(&[
        (".hidden/file.txt", "needle in hidden dir\n"),
        ("visible.txt", "nothing\n"),
    ]);
    let (names, _) = collect_matches(root, "needle", false);
    assert!(names.is_empty());
}

// ── --hidden ─────────────────────────────────────────────────────────────────
//
// --hidden is a separate filtering layer from --no-ignore/respect_gitignore,
// same as ripgrep: "hidden" (dot-prefixed name) and "ignored" (matched by a
// gitignore pattern, or one of the built-in DEFAULT_IGNORES names like
// .git) are independent concepts. These tests lock in that independence in
// both directions, plus the "explicit root path bypasses the check
// entirely" contract documented on SearchConfig::hidden.

fn hidden_config(query: &str, hidden: bool, no_ignore: bool) -> StdArc<SearchConfig> {
    let ignore_dirs: HashSet<String> = if no_ignore {
        HashSet::new()
    } else {
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect()
    };
    StdArc::new(SearchConfig {
        regex: build_matcher(query, MatchOptions::default()).unwrap(),
        query: query.to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: !no_ignore,
        hidden,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    })
}

fn run(root: PathBuf, config: StdArc<SearchConfig>) -> Vec<String> {
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
    });
    let mut names = results.lock().unwrap().clone();
    names.sort();
    names
}

#[test]
fn hidden_flag_includes_hidden_files() {
    let (_dir, root) = make_tree(&[
        (".secret", "needle inside hidden file\n"),
        ("visible.txt", "needle in visible file\n"),
    ]);
    let names = run(root, hidden_config("needle", true, false));
    assert_eq!(
        names,
        vec![".secret", "visible.txt"],
        "--hidden must include dotfiles that are skipped by default"
    );
}

#[test]
fn hidden_flag_includes_hidden_directories() {
    let (_dir, root) = make_tree(&[
        (".hidden/file.txt", "needle in hidden dir\n"),
        ("visible.txt", "nothing here\n"),
    ]);
    let names = run(root, hidden_config("needle", true, false));
    assert_eq!(
        names,
        vec!["file.txt"],
        "--hidden must walk into dot-prefixed directories, not just \
         dot-prefixed files"
    );
}

#[test]
fn hidden_flag_does_not_disable_default_ignore_dirs() {
    // .git is both hidden (dot-prefixed) AND one of the built-in
    // DEFAULT_IGNORES. --hidden alone only lifts the dot-prefix check; it
    // must not also clear ignore_dirs. That's --no-ignore's job.
    let (_dir, root) = make_tree(&[
        (".git/config", "needle should stay hidden\n"),
        ("visible.txt", "nothing here\n"),
    ]);
    let names = run(root, hidden_config("needle", true, false));
    assert!(
        names.is_empty(),
        "--hidden by itself must not reveal .git's contents — that's a \
         DEFAULT_IGNORES entry, a different filtering layer entirely"
    );
}

#[test]
fn hidden_and_no_ignore_together_reveal_everything() {
    // The reviewer's explicit "search literally everything" case:
    // argrep --hidden --no-ignore pattern .
    let (_dir, root) = make_tree(&[
        (".git/config", "needle in git config\n"),
        (".env", "needle in dotenv\n"),
        ("visible.txt", "needle in visible file\n"),
    ]);
    let names = run(root, hidden_config("needle", true, true));
    assert_eq!(
        names,
        vec![".env", "config", "visible.txt"],
        "--hidden + --no-ignore together must search everything, \
         including .git's own contents"
    );
}

#[test]
fn no_ignore_alone_does_not_reveal_hidden_files() {
    // The flip side of the above: --no-ignore only lifts gitignore/
    // DEFAULT_IGNORES filtering, not the separate dot-prefix hidden check.
    let (_dir, root) = make_tree(&[
        (".secret", "needle here\n"),
        ("visible.txt", "needle there\n"),
    ]);
    let names = run(root, hidden_config("needle", false, true));
    assert_eq!(
        names,
        vec!["visible.txt"],
        "--no-ignore without --hidden must still skip dotfiles"
    );
}

#[test]
fn explicit_hidden_root_path_is_always_searched_regardless_of_hidden_flag() {
    // Passing a hidden directory directly as the search root (rather than
    // discovering it while walking a parent) never goes through the
    // per-entry hidden check in scan_and_grep — same contract as grep/
    // ripgrep: an explicitly named path is always searched.
    let (_dir, root_parent) = make_tree(&[(".explicit/file.txt", "needle here\n")]);
    let hidden_root = root_parent.join(".explicit");
    let names = run(hidden_root, hidden_config("needle", false, false));
    assert_eq!(
        names,
        vec!["file.txt"],
        "a hidden directory passed explicitly as the root must be \
         searched even without --hidden"
    );
}

// ── --type / --type-not ────────────────────────────────────────────────────
//
// A small, fixed built-in type table (see TYPE_TABLE in main.rs). These
// tests exercise the filtering semantics: OR within a type's own globs,
// OR across multiple selected types, AND with --include, and --type-not
// winning over --type/--include on overlap (same "negative filter wins"
// precedent as --exclude over --include).

fn type_config(
    query: &str,
    type_globs: &[&str],
    type_not_globs: &[&str],
    include: Option<&str>,
) -> StdArc<SearchConfig> {
    StdArc::new(SearchConfig {
        regex: build_matcher(query, MatchOptions::default()).unwrap(),
        query: query.to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: include.map(|p| Pattern::new(p).unwrap()),
        exclude_patterns: Vec::new(),
        type_patterns: type_globs
            .iter()
            .map(|g| Pattern::new(g).unwrap())
            .collect(),
        type_not_patterns: type_not_globs
            .iter()
            .map(|g| Pattern::new(g).unwrap())
            .collect(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    })
}

#[test]
fn type_filters_to_a_single_extension() {
    let (_dir, root) = make_tree(&[
        ("main.rs", "needle in rust\n"),
        ("script.py", "needle in python\n"),
        ("readme.md", "needle in markdown\n"),
    ]);
    let names = run(root, type_config("needle", &["*.rs"], &[], None));
    assert_eq!(names, vec!["main.rs"]);
}

#[test]
fn type_with_multiple_extensions_matches_any_of_them() {
    // "python" expands to *.py AND *.pyi — a file matching either must be
    // included (OR within one type's own glob list).
    let (_dir, root) = make_tree(&[
        ("mod.py", "needle here\n"),
        ("mod.pyi", "needle here too\n"),
        ("mod.rs", "needle irrelevant\n"),
    ]);
    let names = run(root, type_config("needle", &["*.py", "*.pyi"], &[], None));
    assert_eq!(names, vec!["mod.py", "mod.pyi"]);
}

#[test]
fn multiple_types_are_unioned() {
    // --type rust --type python: a file matching EITHER type is included
    // (OR across types, not AND).
    let (_dir, root) = make_tree(&[
        ("a.rs", "needle\n"),
        ("b.py", "needle\n"),
        ("c.js", "needle\n"),
    ]);
    let names = run(
        root,
        type_config("needle", &["*.rs", "*.py", "*.pyi"], &[], None),
    );
    assert_eq!(names, vec!["a.rs", "b.py"]);
}

#[test]
fn type_not_excludes_matching_files() {
    let (_dir, root) = make_tree(&[("app.js", "needle in js\n"), ("app.py", "needle in py\n")]);
    let names = run(
        root,
        type_config("needle", &[], &["*.js", "*.jsx", "*.mjs", "*.cjs"], None),
    );
    assert_eq!(names, vec!["app.py"]);
}

#[test]
fn type_not_wins_over_type_on_the_same_file() {
    // Contradictory but not a CLI error (see type_and_type_not_can_be_combined
    // in main.rs) — same "negative filter wins" precedent as --exclude
    // over --include.
    let (_dir, root) = make_tree(&[("main.rs", "needle\n")]);
    let names = run(root, type_config("needle", &["*.rs"], &["*.rs"], None));
    assert!(
        names.is_empty(),
        "--type-not must win when the same file matches both --type and \
         --type-not"
    );
}

#[test]
fn type_is_anded_with_include() {
    // A file must satisfy BOTH --include and --type when both are given,
    // not just one of them.
    let (_dir, root) = make_tree(&[
        ("src/main.rs", "needle in src\n"),
        ("vendor/lib.rs", "needle in vendor\n"),
    ]);
    let names = run(root, type_config("needle", &["*.rs"], &[], Some("main.rs")));
    assert_eq!(
        names,
        vec!["main.rs"],
        "--type rust --include main.rs must only match files satisfying \
         both filters"
    );
}

#[test]
fn empty_type_patterns_means_no_type_filter() {
    // No --type given at all (type_patterns empty) must not restrict
    // anything — this is the "type_patterns.is_empty()" escape hatch in
    // scan_and_grep, distinct from an empty *result* of a type that
    // matched nothing.
    let (_dir, root) = make_tree(&[
        ("a.rs", "needle\n"),
        ("b.py", "needle\n"),
        ("c.txt", "needle\n"),
    ]);
    let names = run(root, type_config("needle", &[], &[], None));
    assert_eq!(names, vec!["a.rs", "b.py", "c.txt"]);
}

// ── Ignore dirs ───────────────────────────────────────────────────────────────

#[test]
fn target_dir_is_ignored_by_default() {
    let (_dir, root) = make_tree(&[
        ("target/release/binary.txt", "needle in target\n"),
        ("src/main.rs", "clean source\n"),
    ]);
    let (names, _) = collect_matches(root, "needle", false);
    assert!(names.is_empty(), "target/ should be ignored");
}

#[test]
fn custom_ignore_dir_is_excluded() {
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("vendor")).unwrap();
    fs::write(root.join("vendor/lib.txt"), "needle in vendor\n").unwrap();
    fs::write(root.join("main.txt"), "clean\n").unwrap();

    let mut ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    ignore_dirs.insert("vendor".to_string());
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "needle",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let stats = SearchStats::new();
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);

    parallel_grep(root, 4, config, stats, move |item| {
        r.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
    });

    assert!(results.lock().unwrap().is_empty());
}

// ── Stats counters ────────────────────────────────────────────────────────────

#[test]
fn stats_counts_are_accurate() {
    use std::sync::atomic::Ordering;

    let (_dir, root) = make_tree(&[
        ("a.txt", "TARGET: found it\n"),
        ("b.txt", "nothing relevant here\n"),
        ("sub/c.txt", "TARGET: found it again\n"),
    ]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "TARGET",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "TARGET".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let stats = SearchStats::new();
    let stats_clone = stats.clone();

    parallel_grep(root, 4, config, stats_clone, |_| {});

    // root + sub = 2 dirs minimum; macOS may add extra via symlink resolution
    assert!(stats.total_dirs.load(Ordering::Relaxed) >= 2);
    assert_eq!(stats.total_files.load(Ordering::Relaxed), 3); // a, b, c
    assert_eq!(stats.matched_lines.load(Ordering::Relaxed), 2); // only a and c
}

// ── --stats counters (files_discovered / files_skipped / bytes_read) ───────────

fn stats_config(query: &str, exclude: &[&str]) -> StdArc<SearchConfig> {
    StdArc::new(SearchConfig {
        regex: build_matcher(query, MatchOptions::default()).unwrap(),
        query: query.to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: exclude.iter().map(|g| Pattern::new(g).unwrap()).collect(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    })
}

#[test]
fn files_discovered_counts_every_file_regardless_of_filtering() {
    let (_dir, root) = make_tree(&[
        ("a.txt", "needle\n"),
        ("b.min.js", "needle\n"),
        ("sub/c.txt", "needle\n"),
    ]);
    let config = stats_config("needle", &["*.min.js"]);
    let stats = SearchStats::new();
    parallel_grep(root, 4, config, stats.clone(), |_| {});

    assert_eq!(
        stats.files_discovered.load(Ordering::Relaxed),
        3,
        "files_discovered must count b.min.js too, even though \
         --exclude filters it out before it's ever opened"
    );
}

#[test]
fn files_skipped_counts_excluded_files_not_binary_ones() {
    let (_dir, root) = make_tree(&[
        ("a.txt", "needle\n"),
        ("b.min.js", "needle\n"),
        ("c.min.js", "needle\n"),
    ]);
    let config = stats_config("needle", &["*.min.js"]);
    let stats = SearchStats::new();
    parallel_grep(root, 4, config, stats.clone(), |_| {});

    assert_eq!(
        stats.files_skipped.load(Ordering::Relaxed),
        2,
        "files_skipped must count both files excluded by --exclude"
    );
    assert_eq!(stats.total_files.load(Ordering::Relaxed), 1); // a.txt only
}

#[test]
fn files_discovered_equals_searched_plus_skipped_when_nothing_errors() {
    // The accounting identity documented on SearchStats::files_discovered:
    // with no io_errors, every discovered file ends up either searched or
    // skipped — never both, never neither.
    let (_dir, root) = make_tree(&[
        ("a.txt", "needle\n"),
        ("b.min.js", "needle\n"),
        ("sub/c.txt", "nothing\n"),
        ("sub/d.min.js", "nothing\n"),
    ]);
    let config = stats_config("needle", &["*.min.js"]);
    let stats = SearchStats::new();
    parallel_grep(root, 4, config, stats.clone(), |_| {});

    assert_eq!(stats.io_errors.load(Ordering::Relaxed), 0);
    assert_eq!(
        stats.files_discovered.load(Ordering::Relaxed),
        stats.total_files.load(Ordering::Relaxed) + stats.files_skipped.load(Ordering::Relaxed)
    );
}

#[test]
fn binary_files_count_as_searched_not_skipped() {
    // A binary file is opened (it passes every name-based filter) and
    // only then found to be binary and abandoned — that's a different
    // event from being filtered out by --exclude/--include/--type/
    // hidden/gitignore before ever being opened, so it must land in
    // files_searched (total_files), not files_skipped.
    let (dir, root) = make_tree(&[("a.txt", "needle\n")]);
    let binary_path = dir.path().join("b.bin");
    fs::write(
        &binary_path,
        [0x00u8, 0x01, 0x02, b'n', b'e', b'e', b'd', b'l', b'e'],
    )
    .unwrap();

    let config = stats_config("needle", &[]);
    let stats = SearchStats::new();
    parallel_grep(root, 4, config, stats.clone(), |_| {});

    assert_eq!(stats.files_discovered.load(Ordering::Relaxed), 2);
    assert_eq!(
        stats.total_files.load(Ordering::Relaxed),
        2,
        "the binary file was opened and attempted, so it counts as \
         searched even though grep_file bailed out early"
    );
    assert_eq!(stats.files_skipped.load(Ordering::Relaxed), 0);
    assert_eq!(stats.matched_lines.load(Ordering::Relaxed), 1); // only a.txt
}

#[test]
fn bytes_read_is_nonzero_after_reading_file_content() {
    let (_dir, root) = make_tree(&[("a.txt", "needle and some more text\n")]);
    let config = stats_config("needle", &[]);
    let stats = SearchStats::new();
    parallel_grep(root, 4, config, stats.clone(), |_| {});

    assert!(
        stats.bytes_read.load(Ordering::Relaxed) >= "needle and some more text\n".len(),
        "bytes_read must reflect at least the content actually read"
    );
}

// ── --files (files_only) ────────────────────────────────────────────────────
//
// --files lists the files the traversal engine would search, without
// ever opening them — every filter (hidden, gitignore, --exclude,
// --include, --type/--type-not) still applies exactly as it would for a
// real search. These tests exercise that at the library level
// (SearchConfig.files_only), independent of the CLI-level QUERY/PATH
// handling covered in main.rs's own tests.

fn files_only_config(
    hidden: bool,
    no_ignore: bool,
    exclude: &[&str],
    type_globs: &[&str],
) -> StdArc<SearchConfig> {
    StdArc::new(SearchConfig {
        // The query/regex are never used in files_only mode (grep_file is
        // never called) — an always-matches placeholder is fine here,
        // same as main.rs uses Regex::new("") for the same reason.
        regex: build_matcher("", MatchOptions::default()).unwrap(),
        query: String::new(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: if no_ignore {
            HashSet::new()
        } else {
            DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect()
        },
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: exclude.iter().map(|g| Pattern::new(g).unwrap()).collect(),
        type_patterns: type_globs
            .iter()
            .map(|g| Pattern::new(g).unwrap())
            .collect(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: !no_ignore,
        hidden,
        files_only: true,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    })
}

#[test]
fn files_lists_files_that_pass_every_filter() {
    let (_dir, root) = make_tree(&[
        ("a.rs", "content\n"),
        ("b.py", "content\n"),
        ("sub/c.rs", "content\n"),
    ]);
    let names = run(root, files_only_config(false, false, &[], &[]));
    assert_eq!(names, vec!["a.rs", "b.py", "c.rs"]);
}

#[test]
fn files_never_reads_file_content() {
    let (_dir, root) = make_tree(&[("a.txt", "some content that would otherwise be read\n")]);
    let config = files_only_config(false, false, &[], &[]);
    let stats = SearchStats::new();
    let names_count = Arc::new(Mutex::new(0usize));
    let n = Arc::clone(&names_count);
    parallel_grep(root, 2, config, stats.clone(), move |_| {
        *n.lock().unwrap() += 1;
    });

    assert_eq!(
        *names_count.lock().unwrap(),
        1,
        "the file must still be listed"
    );
    assert_eq!(
        stats.bytes_read.load(Ordering::Relaxed),
        0,
        "--files must never open/read file content — bytes_read must stay \
         at zero"
    );
    assert_eq!(
        stats.matched_lines.load(Ordering::Relaxed),
        0,
        "nothing is ever matched in --files mode, since content is never \
         read"
    );
}

#[test]
fn files_respects_hidden_flag() {
    let (_dir, root) = make_tree(&[(".secret", "x\n"), ("visible.txt", "x\n")]);
    let without_hidden = run(root.clone(), files_only_config(false, false, &[], &[]));
    assert_eq!(without_hidden, vec!["visible.txt"]);

    let with_hidden = run(root, files_only_config(true, false, &[], &[]));
    assert_eq!(with_hidden, vec![".secret", "visible.txt"]);
}

#[test]
fn files_respects_no_ignore_flag() {
    let (_dir, root) = make_tree(&[(".git/config", "x\n"), ("visible.txt", "x\n")]);
    let without_no_ignore = run(root.clone(), files_only_config(true, false, &[], &[]));
    assert_eq!(
        without_no_ignore,
        vec!["visible.txt"],
        "--hidden alone must not reveal .git — that's --no-ignore's job, \
         same independence rule as a real search"
    );

    let with_both = run(root, files_only_config(true, true, &[], &[]));
    assert_eq!(with_both, vec!["config", "visible.txt"]);
}

#[test]
fn files_respects_type_filter() {
    let (_dir, root) = make_tree(&[("a.rs", "x\n"), ("b.py", "x\n"), ("c.rs", "x\n")]);
    let names = run(root, files_only_config(false, false, &[], &["*.rs"]));
    assert_eq!(names, vec!["a.rs", "c.rs"]);
}

#[test]
fn files_respects_exclude_filter() {
    let (_dir, root) = make_tree(&[("keep.rs", "x\n"), ("skip.generated.rs", "x\n")]);
    let names = run(
        root,
        files_only_config(false, false, &["*.generated.rs"], &[]),
    );
    assert_eq!(names, vec!["keep.rs"]);
}

#[test]
fn files_updates_discovered_and_skipped_stats_consistently() {
    // The files_discovered/files_skipped accounting identity documented
    // on SearchStats holds in --files mode too — filtering runs exactly
    // the same way, only the content-reading step is skipped.
    let (_dir, root) = make_tree(&[("a.rs", "x\n"), ("b.generated.rs", "x\n"), ("c.py", "x\n")]);
    let config = files_only_config(false, false, &["*.generated.rs"], &["*.rs"]);
    let stats = SearchStats::new();
    parallel_grep(root, 2, config, stats.clone(), |_| {});

    assert_eq!(stats.files_discovered.load(Ordering::Relaxed), 3);
    // a.rs passes; b.generated.rs is excluded; c.py fails the --type
    // rust filter — so total_files ("searched"/listed) is 1, skipped is 2.
    assert_eq!(stats.total_files.load(Ordering::Relaxed), 1);
    assert_eq!(stats.files_skipped.load(Ordering::Relaxed), 2);
}

// ── Terminal-safety: control characters in matched content ─────────────────────
//
// A file that passes the binary sniff (no NUL in the first 1024 bytes) can
// still contain other control bytes — bell, escape, carriage return, etc.
// Printing those raw to the terminal can ring the bell or, worse, inject
// arbitrary ANSI sequences. These tests confirm grep_file's line_content
// (what actually gets printed) is sanitized, without that sanitization
// affecting whether a line counts as a match in the first place.

#[test]
fn matched_line_containing_bell_character_is_escaped_in_output() {
    let (_dir, root) = make_tree(&[("a.txt", "before\x07needle\x07after\n")]);
    let config = stats_config("needle", &[]);
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(item.line_content.clone());
    });

    let lines = results.lock().unwrap().clone();
    assert_eq!(
        lines,
        vec!["before\\x07needle\\x07after".to_string()],
        "the raw BEL byte must never reach stdout — it must be escaped \
         as literal text instead, so argrep's own output can't ring the \
         terminal bell"
    );
}

#[test]
fn only_matching_output_escapes_control_characters_in_matched_text() {
    // The matched substring itself (not just the surrounding line) must
    // be sanitized under -o, since that's the entire content that gets
    // printed in that mode.
    let (_dir, root) = make_tree(&[("a.txt", "x needle\x1b[31m here\n")]);
    let config = StdArc::new(SearchConfig {
        regex: build_matcher("needle\x1b\\[31m", MatchOptions::default()).unwrap(),
        query: "needle\u{1b}[31m".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: true,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(item.line_content.clone());
    });

    let lines = results.lock().unwrap().clone();
    assert_eq!(lines, vec!["needle\\x1b[31m".to_string()]);
}

#[test]
fn context_lines_with_control_characters_are_also_escaped() {
    // Context lines (-A/-B/-C) go through a different code path than the
    // matching line itself (the before_buffer, and the after-context
    // branch) — both must sanitize independently, not just the match.
    let (_dir, root) = make_tree(&[(
        "a.txt",
        "before\x07context\nneedle here\nafter\x07context\n",
    )]);
    let config = StdArc::new(SearchConfig {
        regex: build_matcher("needle", MatchOptions::default()).unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 1,
        after_context: 1,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(item.line_content.clone());
    });

    let lines = results.lock().unwrap().clone();
    assert_eq!(
        lines,
        vec![
            "before\\x07context".to_string(),
            "needle here".to_string(),
            "after\\x07context".to_string(),
        ]
    );
}

#[test]
fn control_characters_do_not_affect_whether_a_line_matches() {
    // Sanitization is a display-only concern — the regex still sees the
    // original raw text when deciding what matches, so a query that
    // spans a control byte must still work exactly as if it didn't.
    //
    // Uses fixed_strings mode deliberately: the regex crate treats
    // "\x07" in a *pattern* as a hex escape for the BEL byte itself, so
    // testing the "literal backslash-x-0-7 text must not match a real
    // BEL byte" case with the default regex mode would be testing the
    // wrong thing (that pattern would already match a real BEL byte via
    // the regex engine's own hex-escape support, nothing to do with
    // sanitize_for_display at all). Fixed-strings mode removes that
    // ambiguity: the pattern text is matched completely literally.
    fn fixed_string_config(query: &str) -> StdArc<SearchConfig> {
        StdArc::new(SearchConfig {
            regex: build_matcher(
                query,
                MatchOptions {
                    fixed_strings: true,
                    ..Default::default()
                },
            )
            .unwrap(),
            query: query.to_string(),
            ignore_case: false,
            line_number: false,
            ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
            ignore_dir_patterns: Vec::new(),
            debug: false,
            invert: false,
            files_with_matches: false,
            files_without_match: false,
            count_per_file: false,
            include_pattern: None,
            exclude_patterns: Vec::new(),
            type_patterns: Vec::new(),
            type_not_patterns: Vec::new(),
            before_context: 0,
            after_context: 0,
            respect_gitignore: true,
            hidden: false,
            files_only: false,
            search_compressed: false,
            quiet: false,
            only_matching: false,
            max_count: None,
        })
    }

    let (_dir, root) = make_tree(&[("a.txt", "need\x07le\n")]);
    let config = fixed_string_config("need\\x07le"); // literal backslash-x-0-7 text, NOT the byte
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(item.line_content.clone());
    });
    assert!(
        results.lock().unwrap().is_empty(),
        "the fixed-string query 'need\\x07le' (literal backslash-x-0-7 \
         text, 10 characters) must NOT match a real single BEL byte — \
         matching happens against the original raw text, not the \
         sanitized display form"
    );

    let (_dir2, root2) = make_tree(&[("a.txt", "need\x07le\n")]);
    let config2 = fixed_string_config("need\u{7}le"); // the actual BEL byte, in the pattern itself
    let results2: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r2 = Arc::clone(&results2);
    parallel_grep(root2, 2, config2, SearchStats::new(), move |item| {
        r2.lock().unwrap().push(item.line_content.clone());
    });
    assert_eq!(
        results2.lock().unwrap().clone(),
        vec!["need\\x07le".to_string()],
        "a query containing the real BEL byte must still match the real \
         BEL byte in the file, and the emitted content must still come \
         back sanitized for display"
    );
}

// ── Parallelism stability ─────────────────────────────────────────────────────

#[test]
fn multiple_workers_find_same_matches_as_single_worker() {
    let (_dir, root) = make_tree(&[
        ("a.txt", "needle\n"),
        ("b.txt", "nothing\n"),
        ("sub1/c.txt", "needle here\n"),
        ("sub2/d.txt", "needle too\n"),
    ]);

    let run = |workers: usize| {
        let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
        let config = StdArc::new(SearchConfig {
            regex: build_matcher(
                "needle",
                MatchOptions {
                    fixed_strings: false,
                    ignore_case: false,
                    ..Default::default()
                },
            )
            .unwrap(),
            query: "needle".to_string(),
            ignore_case: false,
            line_number: false,
            ignore_dirs,
            ignore_dir_patterns: Vec::new(),
            debug: false,
            invert: false,
            files_with_matches: false,
            files_without_match: false,
            count_per_file: false,
            include_pattern: None,
            exclude_patterns: Vec::new(),
            type_patterns: Vec::new(),
            type_not_patterns: Vec::new(),
            before_context: 0,
            after_context: 0,
            respect_gitignore: true,
            hidden: false,
            files_only: false,
            search_compressed: false,
            quiet: false,
            only_matching: false,
            max_count: None,
        });
        let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let r = Arc::clone(&results);
        parallel_grep(
            root.clone(),
            workers,
            config,
            SearchStats::new(),
            move |item| {
                r.lock()
                    .unwrap()
                    .push(item.line_content.trim_end().to_string());
            },
        );
        let mut v = results.lock().unwrap().clone();
        v.sort();
        v
    };

    assert_eq!(run(1), run(8));
}

// ── Edge cases ────────────────────────────────────────────────────────────────

#[test]
fn empty_file_produces_no_matches() {
    let (_dir, root) = make_tree(&[("empty.txt", "")]);
    let (names, _) = collect_matches(root, "anything", false);
    assert!(names.is_empty());
}

#[test]
fn empty_directory_produces_no_matches() {
    let (_dir, root) = make_tree(&[]);
    let (names, _) = collect_matches(root, "anything", false);
    assert!(names.is_empty());
}

#[test]
fn match_on_last_line_without_newline() {
    let (_dir, root) = make_tree(&[("file.txt", "first line\nneedle no newline")]);
    let (names, lines) = collect_matches(root, "needle", false);
    assert_eq!(names, vec!["file.txt"]);
    assert_eq!(lines, vec!["needle no newline"]);
}

// ── -v invert match ───────────────────────────────────────────────────────────

#[test]
fn invert_returns_non_matching_lines() {
    let (_dir, root) = make_tree(&[("file.txt", "match this\nskip this\nmatch again\n")]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "match",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "match".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: true,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock()
            .unwrap()
            .push(item.line_content.trim_end().to_string());
    });
    let mut lines = results.lock().unwrap().clone();
    lines.sort();
    assert_eq!(lines, vec!["skip this"]);
}

#[test]
fn invert_with_no_matches_returns_all_lines() {
    let (_dir, root) = make_tree(&[("file.txt", "line one\nline two\n")]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "zzznomatch",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "zzznomatch".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: true,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock()
            .unwrap()
            .push(item.line_content.trim_end().to_string());
    });
    let mut lines = results.lock().unwrap().clone();
    lines.sort();
    assert_eq!(lines, vec!["line one", "line two"]);
}

// ── -l files-with-matches ─────────────────────────────────────────────────────

#[test]
fn files_with_matches_returns_only_filenames() {
    let (_dir, root) = make_tree(&[
        ("a.txt", "needle here\n"),
        ("b.txt", "nothing\n"),
        ("c.txt", "needle again\n"),
    ]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "needle",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: true,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
    });
    let mut names = results.lock().unwrap().clone();
    names.sort();
    assert_eq!(names, vec!["a.txt", "c.txt"]);
}

#[test]
fn files_with_matches_emits_each_file_once() {
    // File has multiple matching lines — should still appear only once with -l
    let (_dir, root) = make_tree(&[("file.txt", "needle\nneedle again\nneedle third\n")]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "needle",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: true,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
    });
    assert_eq!(
        results.lock().unwrap().len(),
        1,
        "file should appear exactly once"
    );
}

// ── -L / --files-without-match ──────────────────────────────────────────────
//
// Opposite of -l: reports only files that contain zero matching lines.
// Covers the four interaction questions raised in review: -l/-c conflict
// (enforced at the CLI level, tested in main.rs's unit tests, not here),
// -v combining meaningfully, unreadable files never being reported either
// way, and the file being disqualified as soon as it produces one match
// rather than reading it to the end unnecessarily.

#[test]
fn files_without_match_returns_only_unmatched_filenames() {
    let (_dir, root) = make_tree(&[
        ("a.txt", "needle here\n"),
        ("b.txt", "nothing\n"),
        ("c.txt", "needle again\n"),
        ("d.txt", "still nothing\n"),
    ]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "needle",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: true,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
    });
    let mut names = results.lock().unwrap().clone();
    names.sort();
    assert_eq!(
        names,
        vec!["b.txt", "d.txt"],
        "-L must list exactly the files with zero matching lines, and \
         exclude every file that has at least one"
    );
}

#[test]
fn files_without_match_emits_nothing_when_every_file_matches() {
    let (_dir, root) = make_tree(&[("a.txt", "needle here\n"), ("b.txt", "needle there\n")]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "needle",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: true,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
    });
    assert!(
        results.lock().unwrap().is_empty(),
        "-L must report nothing when every file in the tree has a match"
    );
}

/// The inverse of invert_with_files_with_matches_returns_files_with_a_non_matching_line:
/// with -L -v, a file qualifies only if it has zero "selected" (inverted)
/// lines — i.e. every line in it actually matched the query.
#[test]
fn files_without_match_with_invert_reports_files_where_every_line_matches() {
    let (_dir, root) = make_tree(&[
        ("all_needle.txt", "needle\nneedle\n"), // every line matches -> zero inverted matches -> -L -v includes it
        ("mixed.txt", "needle\nother\n"), // one non-matching line -> has an inverted match -> excluded
        ("no_needle.txt", "nothing\nnothing else\n"), // both lines are inverted matches -> excluded
    ]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "needle",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: true,
        files_with_matches: false,
        files_without_match: true,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
    });
    let mut names = results.lock().unwrap().clone();
    names.sort();
    assert_eq!(
        names,
        vec!["all_needle.txt"],
        "-L -v must list only files where every line matched the query \
         (so nothing was left over to select under -v)"
    );
}

/// Calls grep_file directly (rather than parallel_grep over a tree) so the
/// early-exit behavior can be checked precisely: a file with several
/// matching lines must stop being read after the *first* one, not scan to
/// EOF and then decide not to report it.
#[test]
fn files_without_match_stops_reading_after_first_match() {
    let (_dir, root) = make_tree(&[("file.txt", "needle\nneedle\nneedle\n")]);
    let file_path = root.join("file.txt");

    let config = StdArc::new(SearchConfig {
        regex: build_matcher("needle", MatchOptions::default()).unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: true,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let stats = SearchStats::new();
    let (output_tx, output_rx) = crossbeam_channel::unbounded();

    grep_file(&file_path, &config, &output_tx, &stats);

    assert!(
        output_rx.try_recv().is_err(),
        "a file that has a match must never be reported by -L"
    );
    assert_eq!(
        stats.matched_lines.load(Ordering::Relaxed),
        1,
        "-L must stop reading (and stop counting matches) as soon as the \
         first matching line disqualifies the file — it must not scan the \
         other two matching lines in this file"
    );
}

/// A file that fails to open must not be reported by -L in either
/// direction: not as "matched" (it was never searched) and not as
/// "no match" either (its content is simply unknown). Mirrors
/// unreadable_file_is_skipped_but_other_matches_are_still_found above,
/// but for -L specifically, since a naive implementation could easily
/// treat "couldn't read it" and "read it and found nothing" as the same
/// case.
#[cfg(unix)]
#[test]
fn files_without_match_excludes_unreadable_files() {
    use std::os::unix::fs::PermissionsExt;

    let (_dir, root) = make_tree(&[
        ("blocked.txt", "irrelevant content\n"),
        ("clean.txt", "nothing matches here\n"),
    ]);

    let blocked_path = root.join("blocked.txt");
    fs::set_permissions(&blocked_path, fs::Permissions::from_mode(0o000)).unwrap();

    if File::open(&blocked_path).is_ok() {
        fs::set_permissions(&blocked_path, fs::Permissions::from_mode(0o644)).unwrap();
        eprintln!(
            "skipping files_without_match_excludes_unreadable_files: \
             running as a user that ignores file permissions (e.g. root)"
        );
        return;
    }

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher("needle", MatchOptions::default()).unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: true,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let stats = SearchStats::new();
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, stats.clone(), move |item| {
        r.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
    });

    let _ = fs::set_permissions(&blocked_path, fs::Permissions::from_mode(0o644));

    let mut names = results.lock().unwrap().clone();
    names.sort();
    assert_eq!(
        names,
        vec!["clean.txt"],
        "an unreadable file must never show up in -L output, even though \
         it technically produced no matches — its content is unknown, not \
         confirmed empty of matches"
    );
    assert!(
        stats.io_errors.load(Ordering::Relaxed) > 0,
        "the unreadable file must still be counted as an io error so the \
         exit code reflects an incomplete search"
    );
}

/// -q takes priority over -L, same as every other output mode: nothing is
/// ever sent through the callback. No special-cased exit-code interaction
/// is needed here — matched_lines (which main.rs's -q exit code is based
/// on) reflects whether any line matched anywhere, independent of -l/-L.
#[test]
fn files_without_match_quiet_produces_no_output() {
    let (_dir, root) = make_tree(&[("a.txt", "nothing relevant here\n")]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher("zzznomatch", MatchOptions::default()).unwrap(),
        query: "zzznomatch".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: true,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: true,
        only_matching: false,
        max_count: None,
    });

    let call_count = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&call_count);
    let stats = SearchStats::new();
    parallel_grep(root, 2, config, stats.clone(), move |_item| {
        c.fetch_add(1, Ordering::Relaxed);
    });

    assert_eq!(
        call_count.load(Ordering::Relaxed),
        0,
        "-q -L must never send a MatchResult, even for a file -L would \
         otherwise report"
    );
    assert_eq!(
        stats.matched_lines.load(Ordering::Relaxed),
        0,
        "no line matched anywhere, so the -q exit code must be 1 (no \
         match), unaffected by -L being set"
    );
}

// ── -c count per file ─────────────────────────────────────────────────────────

#[test]
fn count_per_file_returns_correct_counts() {
    let (_dir, root) = make_tree(&[
        ("a.txt", "needle\nneedle\nother\n"),
        ("b.txt", "nothing\n"),
        ("c.txt", "needle\n"),
    ]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "needle",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: true,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<(String, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push((
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
            item.count.unwrap_or(0),
        ));
    });
    let mut counts = results.lock().unwrap().clone();
    counts.sort_by_key(|(name, _)| name.clone());

    assert_eq!(
        counts,
        vec![
            ("a.txt".to_string(), 2),
            ("b.txt".to_string(), 0),
            ("c.txt".to_string(), 1),
        ]
    );
}

#[test]
fn count_per_file_emits_result_for_every_file() {
    // Even files with 0 matches should emit a count result
    let (_dir, root) = make_tree(&[("match.txt", "needle\n"), ("nomatch.txt", "nothing\n")]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "needle",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: true,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
    });
    assert_eq!(
        results.lock().unwrap().len(),
        2,
        "both files should emit a count"
    );
}

// ── -v combined with -c / -l / context ──────────────────────────────────────
//
// should_emit already applies invert before any of the -c/-l/context
// branches run (see grep_file), so these should already work correctly —
// these tests exist to pin that down, not to change behavior.

#[test]
fn invert_with_count_counts_non_matching_lines() {
    let (_dir, root) = make_tree(&[
        ("a.txt", "needle\nother\nneedle\n"),  // 1 line without "needle"
        ("b.txt", "needle\nneedle\n"),         // 0 lines without "needle"
        ("c.txt", "nothing\nstill nothing\n"), // 2 lines without "needle"
    ]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "needle",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: true,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: true,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<(String, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push((
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
            item.count.unwrap_or(0),
        ));
    });
    let mut counts = results.lock().unwrap().clone();
    counts.sort_by_key(|(name, _)| name.clone());

    assert_eq!(
        counts,
        vec![
            ("a.txt".to_string(), 1),
            ("b.txt".to_string(), 0),
            ("c.txt".to_string(), 2),
        ],
        "-v -c must count lines that DON'T match the query, per file"
    );
}

#[test]
fn invert_with_files_with_matches_returns_files_with_a_non_matching_line() {
    let (_dir, root) = make_tree(&[
        ("all_needle.txt", "needle\nneedle\n"), // every line matches -> no inverted match
        ("mixed.txt", "needle\nother\n"),       // one line doesn't match -> inverted match
        ("no_needle.txt", "nothing\nnothing else\n"), // no line matches -> inverted match
    ]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "needle",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: true,
        files_with_matches: true,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
    });
    let mut names = results.lock().unwrap().clone();
    names.sort();
    assert_eq!(
        names,
        vec!["mixed.txt", "no_needle.txt"],
        "-v -l must list files with at least one line that DOESN'T match, \
         and must exclude a file where every line matches"
    );
}

#[test]
fn invert_with_context_builds_context_around_inverted_matches() {
    // Lines 1 and 4 literally contain the query and are NOT inverted
    // matches; they must show up only as context around the surrounding
    // inverted matches (lines 2,3,5,6,7), never re-filtered back out for
    // containing the query themselves.
    let (_dir, root) = make_tree(&[(
        "file.txt",
        "MATCH one\nplain a\nplain b\nMATCH two\nplain c\nplain d\nplain e\n",
    )]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "MATCH",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "MATCH".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: true,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 1,
        after_context: 1,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock()
            .unwrap()
            .push((item.line_content.trim_end().to_string(), item.is_context));
    });

    // Single file, single worker sending in order, so the receive order
    // matches emission order — no sorting needed, which lets us assert on
    // the actual context/match shape rather than just set membership.
    let lines = results.lock().unwrap().clone();
    assert_eq!(
        lines,
        vec![
            ("MATCH one".to_string(), true), // leading context for "plain a"
            ("plain a".to_string(), false),  // inverted match
            ("plain b".to_string(), false),  // inverted match
            ("MATCH two".to_string(), true), // trailing context for "plain b"
            ("plain c".to_string(), false),  // inverted match
            ("plain d".to_string(), false),  // inverted match
            ("plain e".to_string(), false),  // inverted match
        ],
        "context must be built around the inverted matches (physical \
         neighbors), including a literal query-matching line shown purely \
         as context — not re-filtered against the original query"
    );
}

// ── --include pattern ─────────────────────────────────────────────────────────

#[test]
fn include_pattern_searches_only_matching_files() {
    let (_dir, root) = make_tree(&[
        ("main.rs", "needle in rust\n"),
        ("readme.md", "needle in markdown\n"),
        ("config.toml", "needle in toml\n"),
    ]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "needle",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: Some(Pattern::new("*.rs").unwrap()),
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
    });
    assert_eq!(results.lock().unwrap().clone(), vec!["main.rs"]);
}

#[test]
fn include_pattern_no_files_match_returns_empty() {
    let (_dir, root) = make_tree(&[("main.rs", "needle\n"), ("lib.rs", "needle\n")]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "needle",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: Some(Pattern::new("*.txt").unwrap()),
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(item.file_path.display().to_string());
    });
    assert!(results.lock().unwrap().is_empty());
}

#[test]
fn include_wildcard_matches_all_files() {
    let (_dir, root) = make_tree(&[("a.rs", "needle\n"), ("b.txt", "needle\n")]);

    let ignore_dirs_all: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let ignore_dirs_wild: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();

    let config_all = StdArc::new(SearchConfig {
        regex: build_matcher(
            "needle",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: ignore_dirs_all,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });
    let config_wild = StdArc::new(SearchConfig {
        regex: build_matcher(
            "needle",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: ignore_dirs_wild,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: Some(Pattern::new("*").unwrap()),
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });

    let run = |config: std::sync::Arc<argrep::SearchConfig>| {
        let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let r = Arc::clone(&results);
        parallel_grep(root.clone(), 4, config, SearchStats::new(), move |item| {
            r.lock().unwrap().push(
                item.file_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
            );
        });
        let mut v = results.lock().unwrap().clone();
        v.sort();
        v
    };

    assert_eq!(
        run(config_all),
        run(config_wild),
        "'*' include should match same as no filter"
    );
}

// ── Context lines (-A, -B, -C) ────────────────────────────────────────────────

#[test]
fn before_context_includes_leading_lines() {
    let (_dir, root) = make_tree(&[("file.txt", "line1\nline2\nline3\nMATCH\nline5\n")]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "MATCH",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "MATCH".to_string(),
        ignore_case: false,
        line_number: true,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 2,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });

    let results: Arc<Mutex<Vec<(usize, String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        if !item.is_separator {
            r.lock().unwrap().push((
                item.line_num,
                item.line_content.trim_end().to_string(),
                item.is_context,
            ));
        }
    });

    let res = results.lock().unwrap().clone();
    assert_eq!(
        res,
        vec![
            (2, "line2".to_string(), true),
            (3, "line3".to_string(), true),
            (4, "MATCH".to_string(), false),
        ]
    );
}

#[test]
fn after_context_includes_trailing_lines() {
    let (_dir, root) = make_tree(&[("file.txt", "line1\nMATCH\nline3\nline4\nline5\n")]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "MATCH",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "MATCH".to_string(),
        ignore_case: false,
        line_number: true,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 2,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });

    let results: Arc<Mutex<Vec<(usize, String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        if !item.is_separator {
            r.lock().unwrap().push((
                item.line_num,
                item.line_content.trim_end().to_string(),
                item.is_context,
            ));
        }
    });

    let res = results.lock().unwrap().clone();
    assert_eq!(
        res,
        vec![
            (2, "MATCH".to_string(), false),
            (3, "line3".to_string(), true),
            (4, "line4".to_string(), true),
        ]
    );
}

#[test]
fn context_both_and_group_separator() {
    let (_dir, root) = make_tree(&[(
        "file.txt",
        "line1\nline2\nMATCH1\nline4\nline5\nline6\nline7\nline8\nMATCH2\nline10\n",
    )]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "MATCH",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "MATCH".to_string(),
        ignore_case: false,
        line_number: true,
        ignore_dirs,
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 1,
        after_context: 1,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });

    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 4, config, SearchStats::new(), move |item| {
        if item.is_separator {
            r.lock().unwrap().push("--".to_string());
        } else {
            let sep = if item.is_context { "-" } else { ":" };
            r.lock().unwrap().push(format!(
                "{}{}{}",
                item.line_num,
                sep,
                item.line_content.trim_end()
            ));
        }
    });

    let res = results.lock().unwrap().clone();
    assert_eq!(
        res,
        vec![
            "2-line2",
            "3:MATCH1",
            "4-line4",
            "--",
            "8-line8",
            "9:MATCH2",
            "10-line10",
        ]
    );
}

/// QUERY is a regex by default: `.` should match any character, not just
/// a literal dot.
#[test]
fn query_is_a_regex_by_default() {
    let (_dir, root) = make_tree(&[("a.txt", "foo.bar\nfooXbar\nfooZZbar\n")]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "foo.bar",
            MatchOptions {
                fixed_strings: false,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "foo.bar".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });

    let matches: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let m = Arc::clone(&matches);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        m.lock()
            .unwrap()
            .push(item.line_content.trim_end().to_string());
    });

    let mut res = matches.lock().unwrap().clone();
    res.sort();
    // "." matches any single character, so both "foo.bar" and "fooXbar"
    // match; "fooZZbar" doesn't (two characters where the regex expects one).
    assert_eq!(res, vec!["foo.bar", "fooXbar"]);
}

/// -F (fixed_strings=true) should treat the query as a literal string,
/// so a pattern containing "." should only match that exact text.
#[test]
fn fixed_strings_mode_matches_literally() {
    let (_dir, root) = make_tree(&[("a.txt", "foo.bar\nfooXbar\nfooZZbar\n")]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "foo.bar",
            MatchOptions {
                fixed_strings: true,
                ignore_case: false,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "foo.bar".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });

    let matches: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let m = Arc::clone(&matches);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        m.lock()
            .unwrap()
            .push(item.line_content.trim_end().to_string());
    });

    let res = matches.lock().unwrap().clone();
    // Only the literal "foo.bar" text matches; "fooXbar" does not, since
    // "." is escaped and matched literally in fixed-strings mode.
    assert_eq!(res, vec!["foo.bar"]);
}

/// An invalid regex pattern should be reported as an error, not panic.
#[test]
fn invalid_regex_returns_error() {
    let result = build_matcher(
        "foo(bar",
        MatchOptions {
            fixed_strings: false,
            ignore_case: false,
            ..Default::default()
        },
    );
    assert!(result.is_err());
}

/// -w should require the match to be a whole word, not a substring of a
/// larger word.
#[test]
fn whole_word_matches_only_word_boundaries() {
    let (_dir, root) = make_tree(&[("a.txt", "cat\nthe cat sat\ncategory\nconcatenate\n")]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "cat",
            MatchOptions {
                whole_word: true,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "cat".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });

    let matches: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let m = Arc::clone(&matches);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        m.lock()
            .unwrap()
            .push(item.line_content.trim_end().to_string());
    });

    let mut res = matches.lock().unwrap().clone();
    res.sort();
    // "cat" and "the cat sat" contain "cat" as a whole word; "category" and
    // "concatenate" only contain it as a substring of a larger word.
    assert_eq!(res, vec!["cat", "the cat sat"]);
}

/// -x should require the match to span the entire line — this exercises
/// the file-search path specifically (via parallel_grep/grep_file), since
/// that's the code path where the trailing line terminator read by
/// read_until has to be stripped before matching for `$` to behave as
/// expected (the stdin path gets this for free from BufRead::lines()).
#[test]
fn whole_line_matches_only_exact_line() {
    let (_dir, root) = make_tree(&[("a.txt", "ERROR\nERROR: disk full\nan ERROR occurred\n")]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher(
            "ERROR",
            MatchOptions {
                whole_line: true,
                ..Default::default()
            },
        )
        .unwrap(),
        query: "ERROR".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });

    let matches: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let m = Arc::clone(&matches);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        m.lock()
            .unwrap()
            .push(item.line_content.trim_end().to_string());
    });

    let res = matches.lock().unwrap().clone();
    // Only the line that is *exactly* "ERROR" matches; the other two
    // contain it as a prefix/substring rather than filling the whole line.
    assert_eq!(res, vec!["ERROR"]);
}

/// -q must produce zero output, regardless of how many files match — this
/// is what main.rs relies on to guarantee it never prints a matched line
/// under -q, and it's asserted here at the parallel_grep level rather than
/// by capturing process stdout.
#[test]
fn quiet_mode_produces_no_output() {
    let (_dir, root) = make_tree(&[
        ("a.txt", "needle here\n"),
        ("b.txt", "needle there\n"),
        ("c.txt", "needle everywhere\n"),
    ]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher("needle", MatchOptions::default()).unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: true,
        only_matching: false,
        max_count: None,
    });

    let call_count = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&call_count);
    let stats = SearchStats::new();
    parallel_grep(root, 2, config, stats.clone(), move |_item| {
        c.fetch_add(1, Ordering::Relaxed);
    });

    assert_eq!(
        call_count.load(Ordering::Relaxed),
        0,
        "-q must never send a MatchResult, no matter how many files match"
    );
    // But the match still has to be tracked somewhere, since main.rs
    // decides the -q exit code (0/1/2) from stats.matched_lines rather
    // than from any printed output.
    assert!(stats.matched_lines.load(Ordering::Relaxed) > 0);
}

/// -q with no matches anywhere: still zero output, and matched_lines stays
/// at 0 so main.rs's exit-code logic reports "no match" (exit 1) rather
/// than "match" (exit 0).
#[test]
fn quiet_mode_with_no_matches_reports_zero() {
    let (_dir, root) = make_tree(&[("a.txt", "nothing relevant here\n")]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher("zzznomatch", MatchOptions::default()).unwrap(),
        query: "zzznomatch".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: true,
        only_matching: false,
        max_count: None,
    });

    let call_count = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&call_count);
    let stats = SearchStats::new();
    parallel_grep(root, 2, config, stats.clone(), move |_item| {
        c.fetch_add(1, Ordering::Relaxed);
    });

    assert_eq!(call_count.load(Ordering::Relaxed), 0);
    assert_eq!(stats.matched_lines.load(Ordering::Relaxed), 0);
}

/// -o should print one output row per match occurrence, containing only
/// the matched text — mirrors the exact example from the feature request:
/// `echo 'foo bar foo' | argrep -o 'foo'` → "foo\nfoo".
#[test]
fn only_matching_emits_one_row_per_occurrence() {
    let (_dir, root) = make_tree(&[("a.txt", "foo bar foo\nno match here\n")]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher("foo", MatchOptions::default()).unwrap(),
        query: "foo".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: true,
        max_count: None,
    });

    let matches: Arc<Mutex<Vec<(usize, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let m = Arc::clone(&matches);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        m.lock()
            .unwrap()
            .push((item.line_num, item.line_content.clone()));
    });

    let res = matches.lock().unwrap().clone();
    // Two separate rows for the two "foo" occurrences on line 1, both
    // tagged with line_num 1 (matches real grep -o -n behavior: the same
    // line number repeats once per occurrence on that line), and nothing
    // at all for the line that didn't match.
    assert_eq!(res, vec![(1, "foo".to_string()), (1, "foo".to_string())]);
}

/// -c takes priority over -o (same as real grep: -c counts matching
/// *lines*, not occurrences, even with -o) — this is already implied by
/// the code structure (the count_per_file branch is checked before -o
/// gets a chance to run), but worth asserting explicitly since it's easy
/// to regress if the branch order ever gets shuffled.
#[test]
fn count_per_file_takes_priority_over_only_matching() {
    let (_dir, root) = make_tree(&[("a.txt", "foo bar foo\n")]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher("foo", MatchOptions::default()).unwrap(),
        query: "foo".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: true,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: true,
        max_count: None,
    });

    let matches: Arc<Mutex<Vec<Option<usize>>>> = Arc::new(Mutex::new(Vec::new()));
    let m = Arc::clone(&matches);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        m.lock().unwrap().push(item.count);
    });

    let res = matches.lock().unwrap().clone();
    // One summary row with count == 1 (one matching *line*), not two rows
    // for the two "foo" occurrences that -o alone would have produced.
    assert_eq!(res, vec![Some(1)]);
}

/// -m should stop a file after NUM *matching lines*, per the feature
/// request's own recommendation to start with grep's line-based semantics
/// rather than counting individual regex/occurrence matches.
#[test]
fn max_count_stops_after_n_matching_lines() {
    let (_dir, root) = make_tree(&[("a.txt", "match1\nno\nmatch2\nno\nmatch3\nno\nmatch4\n")]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher("match", MatchOptions::default()).unwrap(),
        query: "match".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: Some(2),
    });

    let matches: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let m = Arc::clone(&matches);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        m.lock()
            .unwrap()
            .push(item.line_content.trim_end().to_string());
    });

    let res = matches.lock().unwrap().clone();
    assert_eq!(res, vec!["match1", "match2"]);
}

/// -m with -c: the printed count is capped at NUM, same as grep's
/// documented "does not output a count greater than NUM".
#[test]
fn max_count_caps_the_count_per_file_total() {
    let (_dir, root) = make_tree(&[("a.txt", "match\nmatch\nmatch\nmatch\n")]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher("match", MatchOptions::default()).unwrap(),
        query: "match".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: true,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: Some(2),
    });

    let matches: Arc<Mutex<Vec<Option<usize>>>> = Arc::new(Mutex::new(Vec::new()));
    let m = Arc::clone(&matches);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        m.lock().unwrap().push(item.count);
    });

    let res = matches.lock().unwrap().clone();
    // 4 lines actually match, but -c must not report more than -m's limit.
    assert_eq!(res, vec![Some(2)]);
}

/// -m with -o: a matching line with several occurrences still only counts
/// once toward the limit, and every occurrence on that (already-counted)
/// line is printed — the limit is on lines, not individual matches.
#[test]
fn max_count_with_only_matching_counts_lines_not_occurrences() {
    let (_dir, root) = make_tree(&[(
        "a.txt",
        "foo foo foo\nfoo\nfoo\n", // line 1 has 3 occurrences by itself
    )]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher("foo", MatchOptions::default()).unwrap(),
        query: "foo".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: true,
        max_count: Some(1),
    });

    let matches: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let m = Arc::clone(&matches);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        m.lock().unwrap().push(item.line_content.clone());
    });

    let res = matches.lock().unwrap().clone();
    // -m 1 stops after the *first matching line*, but that one line still
    // gets all 3 of its own occurrences printed — not capped to 1 row.
    assert_eq!(res, vec!["foo", "foo", "foo"]);
}

/// -m with -C: once the limit is reached, any pending trailing context is
/// still flushed before the file search stops, matching grep's documented
/// "outputs any trailing context lines" behavior.
#[test]
fn max_count_still_flushes_trailing_context() {
    let (_dir, root) = make_tree(&[("a.txt", "before\nmatch1\nafter1\nmore\nmatch2\nafter2\n")]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher("match", MatchOptions::default()).unwrap(),
        query: "match".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 1,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: Some(1),
    });

    let matches: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let m = Arc::clone(&matches);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        m.lock()
            .unwrap()
            .push((item.line_content.trim_end().to_string(), item.is_context));
    });

    let res = matches.lock().unwrap().clone();
    // -m 1 stops after "match1", but "after1" (the -A 1 trailing context
    // for that one match) is still printed before the file search ends —
    // "match2" and "after2" never get read at all.
    assert_eq!(
        res,
        vec![("match1".to_string(), false), ("after1".to_string(), true),]
    );
}

/// --exclude should skip files matching the glob, same basename-matching
/// semantics as --include.
#[test]
fn exclude_skips_matching_files() {
    let (_dir, root) = make_tree(&[("a.min.js", "needle\n"), ("a.js", "needle\n")]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher("needle", MatchOptions::default()).unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: vec![Pattern::new("*.min.js").unwrap()],
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });

    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
    });

    let res = results.lock().unwrap().clone();
    assert_eq!(res, vec!["a.js"]);
}

/// --exclude wins over --include when both match the same file — the
/// deliberate simplification of GNU grep's order-dependent precedence
/// documented on SearchConfig.exclude_patterns and in the README.
#[test]
fn exclude_wins_over_include_on_overlap() {
    let (_dir, root) = make_tree(&[("a.txt", "needle\n")]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher("needle", MatchOptions::default()).unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: Vec::new(),
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        // Both --include and --exclude match "a.txt" here.
        include_pattern: Some(Pattern::new("*.txt").unwrap()),
        exclude_patterns: vec![Pattern::new("*.txt").unwrap()],
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });

    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(item.file_path.display().to_string());
    });

    assert!(results.lock().unwrap().is_empty());
}

/// --exclude-dir should support glob patterns (e.g. "build*"), matched
/// against the directory's basename only — not the full relative path.
#[test]
fn exclude_dir_glob_skips_matching_directories() {
    let (_dir, root) = make_tree(&[
        ("build/output.txt", "needle\n"),
        ("build-tools/notes.txt", "needle\n"),
        ("src/main.txt", "needle\n"),
    ]);

    let config = StdArc::new(SearchConfig {
        regex: build_matcher("needle", MatchOptions::default()).unwrap(),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        ignore_dir_patterns: vec![Pattern::new("build*").unwrap()],
        debug: false,
        invert: false,
        files_with_matches: false,
        files_without_match: false,
        count_per_file: false,
        include_pattern: None,
        exclude_patterns: Vec::new(),
        type_patterns: Vec::new(),
        type_not_patterns: Vec::new(),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
        hidden: false,
        files_only: false,
        search_compressed: false,
        quiet: false,
        only_matching: false,
        max_count: None,
    });

    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);
    parallel_grep(root, 2, config, SearchStats::new(), move |item| {
        r.lock().unwrap().push(
            item.file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
    });

    let res = results.lock().unwrap().clone();
    // Both "build" and "build-tools" match "build*" and are skipped
    // entirely (never even traversed); only src/main.txt is searched.
    assert_eq!(res, vec!["main.txt"]);
}
