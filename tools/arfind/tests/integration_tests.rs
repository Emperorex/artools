use arfind::{DEFAULT_IGNORES, SearchConfig, SearchStats, SizeFilter, parallel_find};
use glob::Pattern;
use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tempfile::TempDir;

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Creates a temp directory tree and returns the TempDir handle (kept alive by
/// the caller) together with its canonicalized root path.
fn make_tree(files: &[&str]) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for rel in files {
        let full = dir.path().join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full, b"").unwrap();
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

/// Runs parallel_find and collects the matched paths as strings (file names
/// only, so tests are not tied to the temp-dir prefix).
fn collect_names(
    root: PathBuf,
    pattern: &str,
    max_depth: Option<usize>,
    file_type: Option<&str>,
    hidden: bool,
    extra_ignores: &[&str],
) -> Vec<String> {
    let mut ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    for d in extra_ignores {
        ignore_dirs.insert(d.to_string());
    }

    let config = Arc::new(SearchConfig {
        pattern: Pattern::new(pattern).unwrap(),
        ignore_dirs,
        max_depth,
        file_type: file_type.map(|s| s.to_string()),
        hidden,
        debug: false,
        size_filter: None,
        empty_only: false,
        case_insensitive: false,
        respect_gitignore: false,
    });

    let stats = SearchStats::new();
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let results_clone = Arc::clone(&results);

    parallel_find(root, 4, config, stats, move |item| {
        let name = item.path.file_name().unwrap().to_string_lossy().to_string();
        results_clone.lock().unwrap().push(name);
    });

    let mut v = results.lock().unwrap().clone();
    v.sort(); // deterministic order for assertions
    v
}

// ── Basic matching ────────────────────────────────────────────────────────────

#[test]
fn finds_all_files_with_wildcard() {
    let (_dir, root) = make_tree(&["a.txt", "b.txt", "sub/c.txt"]);
    let names = collect_names(root, "*", None, None, false, &[]);
    assert!(names.contains(&"a.txt".to_string()));
    assert!(names.contains(&"b.txt".to_string()));
    assert!(names.contains(&"c.txt".to_string()));
}

#[test]
fn glob_pattern_filters_by_extension() {
    let (_dir, root) = make_tree(&["a.txt", "b.rs", "c.txt"]);
    let names = collect_names(root, "*.txt", None, None, false, &[]);
    assert_eq!(names, vec!["a.txt", "c.txt"]);
}

#[test]
fn glob_pattern_no_match_returns_empty() {
    let (_dir, root) = make_tree(&["a.rs", "b.rs"]);
    let names = collect_names(root, "*.txt", None, None, false, &[]);
    assert!(names.is_empty());
}

#[test]
fn exact_name_match() {
    let (_dir, root) = make_tree(&["Cargo.toml", "src/main.rs", "src/lib.rs"]);
    let names = collect_names(root, "Cargo.toml", None, None, false, &[]);
    assert_eq!(names, vec!["Cargo.toml"]);
}

// ── Type filtering ────────────────────────────────────────────────────────────

#[test]
fn type_f_returns_only_files() {
    let (_dir, root) = make_tree(&["file.txt", "sub/nested.txt"]);
    // "sub" is a directory that also matches "*"
    let names = collect_names(root, "*", None, Some("f"), false, &[]);
    assert!(!names.contains(&"sub".to_string()));
    assert!(names.contains(&"file.txt".to_string()));
    assert!(names.contains(&"nested.txt".to_string()));
}

#[test]
fn type_d_returns_only_directories() {
    let (_dir, root) = make_tree(&["alpha/file.txt", "beta/file.txt"]);
    let names = collect_names(root, "*", None, Some("d"), false, &[]);
    assert!(names.contains(&"alpha".to_string()));
    assert!(names.contains(&"beta".to_string()));
    assert!(!names.contains(&"file.txt".to_string()));
}

#[test]
fn type_d_with_name_pattern() {
    let (_dir, root) = make_tree(&["src/lib.rs", "tests/mod.rs"]);
    let names = collect_names(root, "src", None, Some("d"), false, &[]);
    assert_eq!(names, vec!["src"]);
}

// ── Depth limiting ────────────────────────────────────────────────────────────
//
// `--max-depth N` follows `find`'s model: depth 0 is the root itself, depth
// 1 is its direct children, depth 2 their children, and so on. At each
// depth limit, matches up to and including that depth are reported, but
// nothing deeper is ever examined.

#[test]
fn max_depth_zero_examines_root_only() {
    let (_dir, root) = make_tree(&["top.txt", "sub/deep.txt"]);
    // The root's own basename doesn't end in .txt, and at depth 0 its
    // contents are never scanned, so nothing should match.
    let names = collect_names(root, "*.txt", Some(0), None, false, &[]);
    assert!(names.is_empty());
}

#[test]
fn max_depth_zero_still_matches_the_root_itself() {
    let (_dir, root) = make_tree(&["top.txt", "sub/deep.txt"]);
    let root_name = root.file_name().unwrap().to_string_lossy().to_string();
    let names = collect_names(root, &root_name, Some(0), None, false, &[]);
    assert_eq!(names, vec![root_name]);
}

#[test]
fn max_depth_one_reaches_direct_children_only() {
    let (_dir, root) = make_tree(&["top.txt", "sub/deep.txt"]);
    let names = collect_names(root, "*.txt", Some(1), None, false, &[]);
    assert!(names.contains(&"top.txt".to_string()));
    assert!(!names.contains(&"deep.txt".to_string()));
}

#[test]
fn max_depth_two_reaches_grandchildren() {
    let (_dir, root) = make_tree(&["a/b.txt", "a/c/d.txt"]);
    let names = collect_names(root, "*.txt", Some(2), None, false, &[]);
    assert!(names.contains(&"b.txt".to_string()));
    assert!(!names.contains(&"d.txt".to_string()));
}

#[test]
fn no_max_depth_finds_deeply_nested_files() {
    let (_dir, root) = make_tree(&["a/b/c/d/e/deep.txt"]);
    let names = collect_names(root, "*.txt", None, None, false, &[]);
    assert!(names.contains(&"deep.txt".to_string()));
}

// ── Hidden file handling ──────────────────────────────────────────────────────

#[test]
fn hidden_files_skipped_by_default() {
    let (_dir, root) = make_tree(&[".env", "visible.txt"]);
    let names = collect_names(root, "*", None, None, false, &[]);
    assert!(!names.contains(&".env".to_string()));
    assert!(names.contains(&"visible.txt".to_string()));
}

#[test]
fn hidden_flag_includes_dot_files() {
    let (_dir, root) = make_tree(&[".env", "visible.txt"]);
    let names = collect_names(root, "*", None, None, true, &[]);
    assert!(names.contains(&".env".to_string()));
    assert!(names.contains(&"visible.txt".to_string()));
}

#[test]
fn hidden_dirs_skipped_by_default() {
    let (_dir, root) = make_tree(&[".hidden_dir/secret.txt", "public.txt"]);
    let names = collect_names(root, "*", None, None, false, &[]);
    assert!(!names.contains(&"secret.txt".to_string()));
    assert!(names.contains(&"public.txt".to_string()));
}

// ── Ignore dirs ───────────────────────────────────────────────────────────────

#[test]
fn default_ignore_skips_git_dir() {
    let (_dir, root) = make_tree(&[".git/HEAD", "src/main.rs"]);
    // .git is a hidden dir so we need hidden=true to even attempt it,
    // but DEFAULT_IGNORES should block it regardless
    let names = collect_names(root, "*", None, None, true, &[]);
    assert!(!names.contains(&"HEAD".to_string()));
    assert!(names.contains(&"main.rs".to_string()));
}

#[test]
fn custom_ignore_skips_specified_directory() {
    let (_dir, root) = make_tree(&["vendor/lib.rs", "src/main.rs"]);
    let names = collect_names(root, "*.rs", None, None, false, &["vendor"]);
    assert!(!names.contains(&"lib.rs".to_string()));
    assert!(names.contains(&"main.rs".to_string()));
}

// ── Stats counters ────────────────────────────────────────────────────────────

#[test]
fn stats_count_files_and_dirs_correctly() {
    use std::sync::atomic::Ordering;

    let (_dir, root) = make_tree(&["a.txt", "b.txt", "sub/c.txt"]);

    let mut ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    ignore_dirs.insert("node_modules".to_string());

    let config = Arc::new(SearchConfig {
        pattern: Pattern::new("*").unwrap(),
        ignore_dirs,
        max_depth: None,
        file_type: None,
        hidden: false,
        debug: false,
        size_filter: None,
        empty_only: false,
        case_insensitive: false,
        respect_gitignore: false,
    });

    let stats = SearchStats::new();
    let stats_clone = stats.clone();

    parallel_find(root, 4, config, stats_clone, |_| {});

    // root + sub = 2 dirs scanned
    assert_eq!(stats.total_dirs.load(Ordering::Relaxed), 2);
    // a.txt, b.txt, c.txt = 3 files
    assert_eq!(stats.total_files.load(Ordering::Relaxed), 3);
    // all match wildcard = files + dirs + the root itself =
    // 3 files + 1 subdir "sub" + 1 root
    assert_eq!(stats.matched_count.load(Ordering::Relaxed), 5);
}

// ── Root matching ────────────────────────────────────────────────────────────
//
// `arfind foo --name foo` must decide whether the root itself (`foo`) is a
// candidate, the same way `find foo -name foo` matches its own starting
// point — not just foo's children.

#[test]
fn root_itself_matches_when_name_equals_root_basename() {
    let (_dir, root) = make_tree(&["a.txt"]);
    let root_name = root.file_name().unwrap().to_string_lossy().to_string();

    let names = collect_names(root, &root_name, None, None, false, &[]);
    assert!(
        names.contains(&root_name),
        "expected root directory itself to be reported as a match"
    );
}

#[test]
fn root_itself_excluded_when_name_does_not_match() {
    let (_dir, root) = make_tree(&["a.txt"]);
    let root_name = root.file_name().unwrap().to_string_lossy().to_string();

    let names = collect_names(root, "definitely-not-the-root-name", None, None, false, &[]);
    assert!(!names.contains(&root_name));
    assert!(names.is_empty());
}

#[test]
fn root_itself_excluded_by_type_filter() {
    let (_dir, root) = make_tree(&["a.txt"]);
    let root_name = root.file_name().unwrap().to_string_lossy().to_string();

    // Root is a directory; filtering for files only should not report it.
    let names = collect_names(root, &root_name, None, Some("f"), false, &[]);
    assert!(!names.contains(&root_name));
}

#[test]
fn root_itself_matches_type_filter_for_directories() {
    let (_dir, root) = make_tree(&["a.txt"]);
    let root_name = root.file_name().unwrap().to_string_lossy().to_string();

    let names = collect_names(root, &root_name, None, Some("d"), false, &[]);
    assert!(names.contains(&root_name));
}

#[test]
fn root_itself_matched_only_once() {
    let (_dir, root) = make_tree(&["a.txt"]);
    let root_name = root.file_name().unwrap().to_string_lossy().to_string();

    let names = collect_names(root, &root_name, None, None, false, &[]);
    assert_eq!(names.iter().filter(|n| **n == root_name).count(), 1);
}

// ── Edge cases ────────────────────────────────────────────────────────────────

#[test]
fn empty_directory_returns_no_child_results() {
    let (_dir, root) = make_tree(&[]);
    let root_name = root.file_name().unwrap().to_string_lossy().to_string();

    // No children exist to match, but the root itself is a valid "*"
    // candidate, so it's the sole result.
    let names = collect_names(root, "*", None, None, false, &[]);
    assert_eq!(names, vec![root_name]);
}

#[test]
fn single_file_at_root_level() {
    let (_dir, root) = make_tree(&["lone.txt"]);
    let names = collect_names(root, "*.txt", None, None, false, &[]);
    assert_eq!(names, vec!["lone.txt"]);
}

#[test]
fn multiple_workers_produce_same_results_as_single_worker() {
    let (_dir, root) = make_tree(&[
        "a.txt",
        "b.txt",
        "c.txt",
        "sub1/d.txt",
        "sub1/e.txt",
        "sub2/f.txt",
        "sub2/sub3/g.txt",
    ]);

    let run = |workers: usize| {
        let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
        let config = Arc::new(SearchConfig {
            pattern: Pattern::new("*.txt").unwrap(),
            ignore_dirs,
            max_depth: None,
            file_type: None,
            hidden: false,
            debug: false,
            size_filter: None,
            empty_only: false,
            case_insensitive: false,
            respect_gitignore: false,
        });
        let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let results_clone = Arc::clone(&results);
        parallel_find(
            root.clone(),
            workers,
            config,
            SearchStats::new(),
            move |item| {
                results_clone
                    .lock()
                    .unwrap()
                    .push(item.path.file_name().unwrap().to_string_lossy().to_string());
            },
        );
        let mut v = results.lock().unwrap().clone();
        v.sort();
        v
    };

    assert_eq!(run(1), run(8));
}

// ── --size filter ─────────────────────────────────────────────────────────────

fn make_tree_with_content(files: &[(&str, &[u8])]) -> (TempDir, PathBuf) {
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

fn collect_with_config(root: PathBuf, config: std::sync::Arc<arfind::SearchConfig>) -> Vec<String> {
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let results_clone = Arc::clone(&results);
    parallel_find(root, 4, config, SearchStats::new(), move |item| {
        results_clone
            .lock()
            .unwrap()
            .push(item.path.file_name().unwrap().to_string_lossy().to_string());
    });
    let mut v = results.lock().unwrap().clone();
    v.sort();
    v
}

#[test]
fn size_min_filters_small_files() {
    // large.txt = 2048 bytes, small.txt = 10 bytes
    let large = vec![b'x'; 2048];
    let small = vec![b'x'; 10];
    let (_dir, root) = make_tree_with_content(&[("large.txt", &large), ("small.txt", &small)]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = Arc::new(SearchConfig {
        pattern: Pattern::new("*").unwrap(),
        ignore_dirs,
        max_depth: None,
        file_type: Some("f".to_string()),
        hidden: false,
        debug: false,
        size_filter: Some(SizeFilter {
            min: Some(1024),
            max: None,
        }), // +1KB
        empty_only: false,
        case_insensitive: false,
        respect_gitignore: false,
    });

    let names = collect_with_config(root, config);
    assert!(
        names.contains(&"large.txt".to_string()),
        "large.txt should match +1KB"
    );
    assert!(
        !names.contains(&"small.txt".to_string()),
        "small.txt should be excluded"
    );
}

#[test]
fn size_max_filters_large_files() {
    let large = vec![b'x'; 2048];
    let small = vec![b'x'; 10];
    let (_dir, root) = make_tree_with_content(&[("large.txt", &large), ("small.txt", &small)]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = Arc::new(SearchConfig {
        pattern: Pattern::new("*").unwrap(),
        ignore_dirs,
        max_depth: None,
        file_type: Some("f".to_string()),
        hidden: false,
        debug: false,
        size_filter: Some(SizeFilter {
            min: None,
            max: Some(1024),
        }), // -1KB
        empty_only: false,
        case_insensitive: false,
        respect_gitignore: false,
    });

    let names = collect_with_config(root, config);
    assert!(
        names.contains(&"small.txt".to_string()),
        "small.txt should match -1KB"
    );
    assert!(
        !names.contains(&"large.txt".to_string()),
        "large.txt should be excluded"
    );
}

#[test]
fn size_filter_does_not_affect_directories() {
    let small = vec![b'x'; 1];
    let (_dir, root) = make_tree_with_content(&[("subdir/file.txt", &small)]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    // min=99GB — no file matches, but directories should still appear
    let config = Arc::new(SearchConfig {
        pattern: Pattern::new("*").unwrap(),
        ignore_dirs,
        max_depth: None,
        file_type: Some("d".to_string()),
        hidden: false,
        debug: false,
        size_filter: Some(SizeFilter {
            min: Some(99 * 1024 * 1024 * 1024),
            max: None,
        }),
        empty_only: false,
        case_insensitive: false,
        respect_gitignore: false,
    });

    let names = collect_with_config(root, config);
    assert!(
        names.contains(&"subdir".to_string()),
        "dirs should not be filtered by --size"
    );
}

// ── --empty filter ────────────────────────────────────────────────────────────

#[test]
fn empty_finds_zero_byte_files() {
    let (_dir, root) = make_tree_with_content(&[("empty.txt", b""), ("nonempty.txt", b"hello")]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = Arc::new(SearchConfig {
        pattern: Pattern::new("*").unwrap(),
        ignore_dirs,
        max_depth: None,
        file_type: Some("f".to_string()),
        hidden: false,
        debug: false,
        size_filter: None,
        empty_only: true,
        case_insensitive: false,
        respect_gitignore: false,
    });

    let names = collect_with_config(root, config);
    assert_eq!(names, vec!["empty.txt"]);
}

#[test]
fn empty_finds_empty_directories() {
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("empty_dir")).unwrap();
    fs::create_dir_all(root.join("nonempty_dir")).unwrap();
    fs::write(root.join("nonempty_dir/file.txt"), b"hi").unwrap();

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = Arc::new(SearchConfig {
        pattern: Pattern::new("*").unwrap(),
        ignore_dirs,
        max_depth: None,
        file_type: Some("d".to_string()),
        hidden: false,
        debug: false,
        size_filter: None,
        empty_only: true,
        case_insensitive: false,
        respect_gitignore: false,
    });

    let names = collect_with_config(root, config);
    assert!(
        names.contains(&"empty_dir".to_string()),
        "empty_dir should match"
    );
    assert!(
        !names.contains(&"nonempty_dir".to_string()),
        "nonempty_dir should not match"
    );
}

#[test]
fn empty_false_returns_all_files() {
    let (_dir, root) = make_tree_with_content(&[("empty.txt", b""), ("nonempty.txt", b"hello")]);

    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let config = Arc::new(SearchConfig {
        pattern: Pattern::new("*.txt").unwrap(),
        ignore_dirs,
        max_depth: None,
        file_type: Some("f".to_string()),
        hidden: false,
        debug: false,
        size_filter: None,
        empty_only: false,
        case_insensitive: false,
        respect_gitignore: false,
    });

    let names = collect_with_config(root, config);
    assert_eq!(names, vec!["empty.txt", "nonempty.txt"]);
}

// ── gitignore support ─────────────────────────────────────────────────────────

fn config_with_gitignore(pattern: &str, respect_gitignore: bool) -> Arc<SearchConfig> {
    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    Arc::new(SearchConfig {
        pattern: Pattern::new(pattern).unwrap(),
        ignore_dirs,
        max_depth: None,
        file_type: None,
        hidden: false,
        debug: false,
        size_filter: None,
        empty_only: false,
        case_insensitive: false,
        respect_gitignore,
    })
}

#[test]
fn gitignore_excludes_matched_files() {
    let (_dir, root) = make_tree(&[".gitignore", "keep.txt", "build.log"]);
    fs::write(root.join(".gitignore"), b"*.log\n").unwrap();

    let names = collect_with_config(root, config_with_gitignore("*", true));
    assert!(names.contains(&"keep.txt".to_string()));
    assert!(!names.contains(&"build.log".to_string()));
}

#[test]
fn gitignore_excludes_matched_directories_and_their_contents() {
    let (_dir, root) = make_tree(&[".gitignore", "src/main.rs", "target/debug/binary"]);
    fs::write(root.join(".gitignore"), b"/target\n").unwrap();

    let names = collect_with_config(root, config_with_gitignore("*", true));
    assert!(names.contains(&"main.rs".to_string()));
    assert!(!names.contains(&"target".to_string()));
    assert!(!names.contains(&"binary".to_string()));
}

#[test]
fn gitignore_negation_reincludes_file() {
    let (_dir, root) = make_tree(&[".gitignore", "a.log", "important.log"]);
    fs::write(root.join(".gitignore"), b"*.log\n!important.log\n").unwrap();

    let names = collect_with_config(root, config_with_gitignore("*", true));
    assert!(!names.contains(&"a.log".to_string()));
    assert!(names.contains(&"important.log".to_string()));
}

#[test]
fn no_ignore_flag_disables_gitignore_respect() {
    let (_dir, root) = make_tree(&[".gitignore", "keep.txt", "build.log"]);
    fs::write(root.join(".gitignore"), b"*.log\n").unwrap();

    let names = collect_with_config(root, config_with_gitignore("*", false));
    assert!(names.contains(&"keep.txt".to_string()));
    assert!(
        names.contains(&"build.log".to_string()),
        "build.log should be found when respect_gitignore is false"
    );
}

#[test]
fn nested_gitignore_can_override_parent() {
    // Root ignores all *.txt, but the "keep" subdirectory re-includes them —
    // mirrors git's own precedence where a deeper .gitignore can win.
    let (_dir, root) = make_tree(&[".gitignore", "keep/.gitignore", "a.txt", "keep/b.txt"]);
    fs::write(root.join(".gitignore"), b"*.txt\n").unwrap();
    fs::write(root.join("keep/.gitignore"), b"!*.txt\n").unwrap();

    let names = collect_with_config(root, config_with_gitignore("*.txt", true));
    assert!(!names.contains(&"a.txt".to_string()));
    assert!(names.contains(&"b.txt".to_string()));
}

#[test]
fn dot_ignore_file_is_also_respected() {
    let (_dir, root) = make_tree(&[".ignore", "keep.txt", "secret.txt"]);
    fs::write(root.join(".ignore"), b"secret.txt\n").unwrap();

    let names = collect_with_config(root, config_with_gitignore("*", true));
    assert!(names.contains(&"keep.txt".to_string()));
    assert!(!names.contains(&"secret.txt".to_string()));
}
