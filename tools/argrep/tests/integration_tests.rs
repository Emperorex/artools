use argrep::{
    DEFAULT_IGNORES, SearchConfig, SearchStats, grep_file, normalize_query, parallel_grep,
};
use glob::Pattern;
use std::sync::Arc as StdArc;
use std::{
    collections::HashSet,
    fs::{self, File},
    path::PathBuf,
    sync::{Arc, Mutex, atomic::Ordering},
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

fn default_config(query: &str, ignore_case: bool) -> std::sync::Arc<argrep::SearchConfig> {
    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    StdArc::new(SearchConfig {
        normalized_query: normalize_query(query, ignore_case),
        query: query.to_string(),
        ignore_case,
        line_number: false,
        ignore_dirs,
        debug: false,
        invert: false,
        files_with_matches: false,
        count_per_file: false,
        include_pattern: None,
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
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

// A FIFO is readable but not seekable: after the binary-sniff read
// succeeds, rewinding with seek(SeekFrom::Start(0)) fails with ESPIPE.
// That failure must be counted the same as any other unreadable file per
// the #106 exit-code contract, not returned from silently. A plain
// BufReader<File> over a regular file essentially never fails seek(), so
// a FIFO is the reliable way to actually exercise this path.
#[cfg(unix)]
#[test]
fn seek_failure_after_binary_sniff_is_counted_as_io_error() {
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
        f.write_all(b"some content, no null bytes here\n").unwrap();
        // Drop here closes the write end once the bytes are flushed to the
        // pipe buffer, which is fine: the reader only needs those bytes
        // for the sniff read, not a still-open writer.
    });

    let config = default_config("anything", false);
    let stats = SearchStats::new();
    let (output_tx, _output_rx) = crossbeam_channel::unbounded();

    grep_file(&fifo_path, &config, &output_tx, &stats);

    writer.join().unwrap();

    assert!(
        stats.io_errors.load(Ordering::Relaxed) > 0,
        "a failed seek() after the binary sniff must be counted as an io \
         error, not silently skipped"
    );
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
        normalized_query: normalize_query("needle", false),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        debug: false,
        invert: false,
        files_with_matches: false,
        count_per_file: false,
        include_pattern: None,
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
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
        normalized_query: normalize_query("TARGET", false),
        query: "TARGET".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        debug: false,
        invert: false,
        files_with_matches: false,
        count_per_file: false,
        include_pattern: None,
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
    });
    let stats = SearchStats::new();
    let stats_clone = stats.clone();

    parallel_grep(root, 4, config, stats_clone, |_| {});

    // root + sub = 2 dirs minimum; macOS may add extra via symlink resolution
    assert!(stats.total_dirs.load(Ordering::Relaxed) >= 2);
    assert_eq!(stats.total_files.load(Ordering::Relaxed), 3); // a, b, c
    assert_eq!(stats.matched_lines.load(Ordering::Relaxed), 2); // only a and c
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
            normalized_query: normalize_query("needle", false),
            query: "needle".to_string(),
            ignore_case: false,
            line_number: false,
            ignore_dirs,
            debug: false,
            invert: false,
            files_with_matches: false,
            count_per_file: false,
            include_pattern: None,
            before_context: 0,
            after_context: 0,
            respect_gitignore: true,
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
        normalized_query: normalize_query("match", false),
        query: "match".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        debug: false,
        invert: true,
        files_with_matches: false,
        count_per_file: false,
        include_pattern: None,
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
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
        normalized_query: normalize_query("zzznomatch", false),
        query: "zzznomatch".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        debug: false,
        invert: true,
        files_with_matches: false,
        count_per_file: false,
        include_pattern: None,
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
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
        normalized_query: normalize_query("needle", false),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        debug: false,
        invert: false,
        files_with_matches: true,
        count_per_file: false,
        include_pattern: None,
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
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
        normalized_query: normalize_query("needle", false),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        debug: false,
        invert: false,
        files_with_matches: true,
        count_per_file: false,
        include_pattern: None,
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
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
        normalized_query: normalize_query("needle", false),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        debug: false,
        invert: false,
        files_with_matches: false,
        count_per_file: true,
        include_pattern: None,
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
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
        normalized_query: normalize_query("needle", false),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        debug: false,
        invert: false,
        files_with_matches: false,
        count_per_file: true,
        include_pattern: None,
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
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
        normalized_query: normalize_query("needle", false),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        debug: false,
        invert: false,
        files_with_matches: false,
        count_per_file: false,
        include_pattern: Some(Pattern::new("*.rs").unwrap()),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
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
        normalized_query: normalize_query("needle", false),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs,
        debug: false,
        invert: false,
        files_with_matches: false,
        count_per_file: false,
        include_pattern: Some(Pattern::new("*.txt").unwrap()),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
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
        normalized_query: normalize_query("needle", false),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: ignore_dirs_all,
        debug: false,
        invert: false,
        files_with_matches: false,
        count_per_file: false,
        include_pattern: None,
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
    });
    let config_wild = StdArc::new(SearchConfig {
        normalized_query: normalize_query("needle", false),
        query: "needle".to_string(),
        ignore_case: false,
        line_number: false,
        ignore_dirs: ignore_dirs_wild,
        debug: false,
        invert: false,
        files_with_matches: false,
        count_per_file: false,
        include_pattern: Some(Pattern::new("*").unwrap()),
        before_context: 0,
        after_context: 0,
        respect_gitignore: true,
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
        normalized_query: normalize_query("MATCH", false),
        query: "MATCH".to_string(),
        ignore_case: false,
        line_number: true,
        ignore_dirs,
        debug: false,
        invert: false,
        files_with_matches: false,
        count_per_file: false,
        include_pattern: None,
        before_context: 2,
        after_context: 0,
        respect_gitignore: true,
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
        normalized_query: normalize_query("MATCH", false),
        query: "MATCH".to_string(),
        ignore_case: false,
        line_number: true,
        ignore_dirs,
        debug: false,
        invert: false,
        files_with_matches: false,
        count_per_file: false,
        include_pattern: None,
        before_context: 0,
        after_context: 2,
        respect_gitignore: true,
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
        normalized_query: normalize_query("MATCH", false),
        query: "MATCH".to_string(),
        ignore_case: false,
        line_number: true,
        ignore_dirs,
        debug: false,
        invert: false,
        files_with_matches: false,
        count_per_file: false,
        include_pattern: None,
        before_context: 1,
        after_context: 1,
        respect_gitignore: true,
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
