use argrep::{DEFAULT_IGNORES, SearchStats, build_config, parallel_grep};
use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
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
    build_config(query.to_string(), ignore_case, false, ignore_dirs, false)
}

/// Runs parallel_grep and returns (matched_file_names, matched_line_contents) sorted.
fn collect_matches(
    root: PathBuf,
    query: &str,
    ignore_case: bool,
) -> (Vec<String>, Vec<String>) {
    let config = default_config(query, ignore_case);
    let stats = SearchStats::new();

    let file_names: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let line_contents: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fn_clone = Arc::clone(&file_names);
    let lc_clone = Arc::clone(&line_contents);

    parallel_grep(root, 4, config, stats, move |item| {
        fn_clone
            .lock()
            .unwrap()
            .push(item.file_path.file_name().unwrap().to_string_lossy().to_string());
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
    let config = build_config("needle".to_string(), false, false, ignore_dirs, false);
    let stats = SearchStats::new();
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = Arc::clone(&results);

    parallel_grep(root, 4, config, stats, move |item| {
        r.lock().unwrap().push(
            item.file_path.file_name().unwrap().to_string_lossy().to_string(),
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
    let config = build_config("TARGET".to_string(), false, false, ignore_dirs, false);
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
        let ignore_dirs: HashSet<String> =
            DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
        let config = build_config("needle".to_string(), false, false, ignore_dirs, false);
        let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let r = Arc::clone(&results);
        parallel_grep(root.clone(), workers, config, SearchStats::new(), move |item| {
            r.lock()
                .unwrap()
                .push(item.line_content.trim_end().to_string());
        });
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
