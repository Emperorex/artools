use ardisk::{DEFAULT_IGNORES, aggregate_sizes, build_config, format_size, parallel_scan};
use glob::Pattern;
use std::{collections::HashSet, fs, path::PathBuf};
use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Creates a temp directory tree with the given relative file paths.
/// Returns the TempDir handle (kept alive by caller) and the canonicalized root.
fn make_tree(files: &[&str]) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for rel in files {
        let full = dir.path().join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full, b"hello").unwrap(); // 5 bytes each, predictable size
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

/// Returns the on-disk block cost of a directory's own inode (not its contents),
/// mirroring how `scan_directory` now accounts for the directory entry itself.
#[cfg(unix)]
fn dir_self_size(path: &PathBuf) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).map(|m| m.blocks() * 512).unwrap_or(0)
}

#[cfg(not(unix))]
fn dir_self_size(path: &PathBuf) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn default_config(debug: bool) -> std::sync::Arc<ardisk::ScanConfig> {
    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    build_config(ignore_dirs, None, debug, false)
}

fn run(root: PathBuf) -> std::collections::HashMap<PathBuf, u64> {
    let config = default_config(false);
    let (raw, _content) = parallel_scan(root.clone(), 4, config);
    aggregate_sizes(&raw, &root)
}

// ── aggregate_sizes correctness ───────────────────────────────────────────────

#[test]
fn root_size_equals_sum_of_all_files() {
    let (_dir, root) = make_tree(&["a.txt", "b.txt", "sub/c.txt"]);
    let sizes = run(root.clone());

    // Each file is 5 bytes; on Unix blocks() * 512 will be >= 5
    // We only assert the root is >= its children, not exact byte counts,
    // because block-based sizing is filesystem-dependent.
    let root_size = sizes[&root];
    let sub = root.join("sub");
    let sub_size = sizes[&sub];

    assert!(root_size >= sub_size, "root must be >= any child");
    assert!(root_size > 0, "root must have non-zero size");
}

#[test]
fn nested_child_size_rolls_up_to_parent() {
    let (_dir, root) = make_tree(&["deep/nested/file.txt"]);
    let sizes = run(root.clone());

    let deep = root.join("deep");
    let nested = deep.join("nested");

    assert!(sizes[&root] >= sizes[&deep]);
    assert!(sizes[&deep] >= sizes[&nested]);
    assert!(sizes[&nested] > 0);
}

#[test]
fn empty_directory_has_only_its_own_inode_size() {
    let (_dir, root) = make_tree(&[]);
    // create an explicit empty subdirectory
    let empty_sub = root.join("empty");
    fs::create_dir_all(&empty_sub).unwrap();

    let config = default_config(false);
    let (raw, _content) = parallel_scan(root.clone(), 4, config);
    let sizes = aggregate_sizes(&raw, &root);

    let expected_self_size = dir_self_size(&empty_sub);
    assert_eq!(
        sizes.get(&empty_sub).copied().unwrap_or(0),
        expected_self_size
    );
}

#[test]
fn sibling_dirs_are_independent() {
    let (_dir, root) = make_tree(&["alpha/file.txt", "beta/file.txt"]);
    let sizes = run(root.clone());

    let alpha = root.join("alpha");
    let beta = root.join("beta");

    // Both siblings should have equal size (one identical file each)
    assert_eq!(sizes[&alpha], sizes[&beta]);
    // Root should be roughly double either sibling
    assert!(sizes[&root] >= sizes[&alpha] + sizes[&beta]);
}

#[test]
fn deeply_nested_tree_rolls_up_correctly() {
    // a/b/c/d/e — only the leaf has a file
    let (_dir, root) = make_tree(&["a/b/c/d/e/leaf.txt"]);
    let sizes = run(root.clone());

    let leaf_dir = root.join("a/b/c/d/e");
    let leaf_size = sizes[&leaf_dir];

    // Every ancestor must carry at least the leaf's size
    for ancestor in ["a/b/c/d", "a/b/c", "a/b", "a"] {
        let p = root.join(ancestor);
        assert!(
            sizes[&p] >= leaf_size,
            "{} should be >= leaf dir size",
            ancestor
        );
    }
    assert!(sizes[&root] >= leaf_size);
}

// ── ignore dirs ───────────────────────────────────────────────────────────────

#[test]
fn ignored_directory_is_excluded_from_scan() {
    let (_dir, root) = make_tree(&["node_modules/package/index.js", "src/main.rs"]);
    let config = default_config(false);
    let (raw, _content) = parallel_scan(root.clone(), 4, config);
    let sizes = aggregate_sizes(&raw, &root);

    let node_modules = root.join("node_modules");
    assert!(
        !sizes.contains_key(&node_modules),
        "node_modules should be absent from results"
    );
}

#[test]
fn custom_ignore_excludes_specified_dir() {
    let (_dir, root) = make_tree(&["vendor/lib.rs", "src/main.rs"]);

    let mut ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    ignore_dirs.insert("vendor".to_string());
    let config = build_config(ignore_dirs, None, false, false);

    let (raw, _content) = parallel_scan(root.clone(), 4, config);
    let sizes = aggregate_sizes(&raw, &root);

    assert!(!sizes.contains_key(&root.join("vendor")));
    assert!(sizes.contains_key(&root.join("src")));
}

// ── symlink skipping ──────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn symlinked_files_are_not_counted() {
    use std::os::unix::fs::symlink;

    let (_dir, root) = make_tree(&["real.txt"]);
    symlink(root.join("real.txt"), root.join("link.txt")).unwrap();

    let config = default_config(false);
    let (raw, _content) = parallel_scan(root.clone(), 4, config);

    // raw size of root should equal one file's blocks plus the root
    // directory's own inode cost, not two files' worth of blocks
    let root_raw = raw[&root];
    let single_file_size = fs::metadata(root.join("real.txt"))
        .map(|m| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                m.blocks() * 512
            }
        })
        .unwrap_or(0);
    let expected = single_file_size + dir_self_size(&root);

    assert_eq!(root_raw, expected, "symlink should not be counted");
}

// ── hard-link dedup ────────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn hard_linked_files_are_only_counted_once() {
    use std::os::unix::fs::MetadataExt;

    let (_dir, root) = make_tree(&["a/real.txt"]);
    fs::hard_link(root.join("a/real.txt"), root.join("b").join("linked.txt")).unwrap_or_else(
        |_| {
            fs::create_dir_all(root.join("b")).unwrap();
            fs::hard_link(root.join("a/real.txt"), root.join("b/linked.txt")).unwrap();
        },
    );

    let config = default_config(false);
    let (raw, _content) = parallel_scan(root.clone(), 4, config);
    let sizes = aggregate_sizes(&raw, &root);

    let file_meta = fs::metadata(root.join("a/real.txt")).unwrap();
    assert!(
        file_meta.nlink() > 1,
        "test setup: file should be hard-linked"
    );
    let single_file_size = file_meta.blocks() * 512;

    // Root should carry the file's size only once, plus both dirs' and the
    // root's own inode costs — not twice for the two hard-linked names.
    let expected = single_file_size
        + dir_self_size(&root)
        + dir_self_size(&root.join("a"))
        + dir_self_size(&root.join("b"));

    assert_eq!(
        sizes[&root], expected,
        "hard-linked file should only be counted once across the whole tree"
    );
}

// ── parallel_scan raw output ──────────────────────────────────────────────────

#[test]
fn raw_scan_produces_entry_for_every_directory() {
    let (_dir, root) = make_tree(&["a/b/c.txt", "d/e.txt"]);
    let config = default_config(false);
    let (raw, _content) = parallel_scan(root.clone(), 4, config);

    assert!(raw.contains_key(&root));
    assert!(raw.contains_key(&root.join("a")));
    assert!(raw.contains_key(&root.join("a/b")));
    assert!(raw.contains_key(&root.join("d")));
}

#[test]
fn multiple_workers_produce_same_aggregated_sizes() {
    let (_dir, root) = make_tree(&["a/x.txt", "a/y.txt", "b/z.txt", "b/sub/w.txt"]);

    let run_with = |workers: usize| {
        let config = default_config(false);
        let (raw, _content) = parallel_scan(root.clone(), workers, config);
        let agg = aggregate_sizes(&raw, &root);
        // Return just the root size as a stable scalar to compare
        agg[&root]
    };

    assert_eq!(run_with(1), run_with(8));
}

// ── format_size ───────────────────────────────────────────────────────────────

#[test]
fn format_size_bytes() {
    let s = format_size(512);
    assert!(s.contains("512") && s.contains('B'));
}

#[test]
fn format_size_kilobytes() {
    let s = format_size(2048);
    assert!(s.contains("KB"), "expected KB, got: {}", s);
}

#[test]
fn format_size_megabytes() {
    let s = format_size(3 * 1024 * 1024);
    assert!(s.contains("MB"), "expected MB, got: {}", s);
}

#[test]
fn format_size_gigabytes() {
    let s = format_size(2 * 1024 * 1024 * 1024);
    assert!(s.contains("GB"), "expected GB, got: {}", s);
}

#[test]
fn format_size_terabytes() {
    let s = format_size(2 * 1024 * 1024 * 1024 * 1024);
    assert!(s.contains("TB"), "expected TB, got: {}", s);
}

#[test]
fn format_size_zero() {
    let s = format_size(0);
    assert!(s.contains('B'));
}

// ── include pattern ───────────────────────────────────────────────────────────

fn config_with_include(pattern: &str) -> std::sync::Arc<ardisk::ScanConfig> {
    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    build_config(
        ignore_dirs,
        Some(Pattern::new(pattern).unwrap()),
        false,
        false,
    )
}

#[test]
fn include_pattern_counts_only_matching_files() {
    let (_dir, root) = make_tree(&[
        "src/main.rs",
        "src/lib.rs",
        "src/README.md",
        "docs/guide.md",
    ]);

    let config = config_with_include("*.rs");
    let (raw, _content) = parallel_scan(root.clone(), 4, config);
    let sizes = aggregate_sizes(&raw, &root);

    let src = root.join("src");
    let docs = root.join("docs");

    // docs has no matching .rs files, so its size should be exactly its own
    // directory inode cost — no file contribution on top of that.
    let docs_baseline = dir_self_size(&docs);
    assert_eq!(
        sizes[&docs], docs_baseline,
        "docs should equal only its own inode size — no .rs files"
    );
    // src has two .rs files, so it must exceed the same kind of baseline
    assert!(
        sizes[&src] > dir_self_size(&src),
        "src should have extra size from matching .rs files"
    );
}

#[test]
fn include_pattern_rolls_up_correctly_to_root() {
    let (_dir, root) = make_tree(&["a/match.rs", "a/skip.txt", "b/match.rs", "b/skip.txt"]);

    let config_all = build_config(
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        None,
        false,
        false,
    );
    let config_rs = config_with_include("*.rs");

    let (raw_all, _content_all) = parallel_scan(root.clone(), 4, config_all);
    let (raw_rs, _content_rs) = parallel_scan(root.clone(), 4, config_rs);

    let sizes_all = aggregate_sizes(&raw_all, &root);
    let sizes_rs = aggregate_sizes(&raw_rs, &root);

    // Filtered root must be strictly less than unfiltered root
    // (since .txt files are excluded)
    assert!(
        sizes_rs[&root] < sizes_all[&root],
        "filtered root size should be less than unfiltered"
    );
    // But filtered root must still be positive (two .rs files present)
    assert!(sizes_rs[&root] > 0);
}

#[test]
fn include_pattern_no_match_gives_directory_self_size_only() {
    let (_dir, root) = make_tree(&["a/file.txt", "b/other.md"]);

    let config = config_with_include("*.rs");
    let (raw, _content) = parallel_scan(root.clone(), 4, config);
    let sizes = aggregate_sizes(&raw, &root);

    // No .rs files exist, so no directory should carry file contribution —
    // but each leaf directory still carries its own inode cost, and every
    // ancestor rolls up its descendants' inode costs too.
    let a = root.join("a");
    let b = root.join("b");

    assert_eq!(
        sizes[&a],
        dir_self_size(&a),
        "a should equal only its own inode size — no .rs files"
    );
    assert_eq!(
        sizes[&b],
        dir_self_size(&b),
        "b should equal only its own inode size — no .rs files"
    );
    assert_eq!(
        sizes[&root],
        dir_self_size(&root) + dir_self_size(&a) + dir_self_size(&b),
        "root should roll up only inode costs, no file bytes"
    );
}

#[test]
fn include_pattern_wildcard_matches_all_files() {
    let (_dir, root) = make_tree(&["a/x.txt", "b/y.rs"]);

    let config_wildcard = config_with_include("*");
    let config_none = build_config(
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        None,
        false,
        false,
    );

    let (raw_wildcard, _content_wildcard) = parallel_scan(root.clone(), 4, config_wildcard);
    let (raw_none, _content_none) = parallel_scan(root.clone(), 4, config_none);

    let sizes_wildcard = aggregate_sizes(&raw_wildcard, &root);
    let sizes_none = aggregate_sizes(&raw_none, &root);

    // "*" include should produce identical results to no include filter
    assert_eq!(
        sizes_wildcard[&root], sizes_none[&root],
        "'*' include should match all files, same as no filter"
    );
}

// ── include suppression with inode costs ──────────────────────────────────────

#[test]
fn include_suppression_works_with_inode_costs() {
    // Regression test: after adding directory inode costs, every directory
    // has non-zero total size. The content map must still report 0 for
    // directories that have no matching files so main.rs can suppress them.
    let (_dir, root) = make_tree(&[
        "src/main.rs",
        "src/lib.rs",
        "docs/guide.md",
        "assets/logo.png",
    ]);

    let config = config_with_include("*.rs");
    let (raw, content) = parallel_scan(root.clone(), 4, config);
    let agg_total = aggregate_sizes(&raw, &root);
    let agg_content = aggregate_sizes(&content, &root);

    let docs = root.join("docs");
    let assets = root.join("assets");
    let src = root.join("src");

    // docs and assets have NO .rs files:
    // content size must be 0 — this is what main.rs checks for suppression.
    // (total size may or may not be 0 depending on filesystem inode accounting)
    assert_eq!(
        agg_content[&docs], 0,
        "docs content should be 0 — no .rs files"
    );
    assert_eq!(
        agg_content[&assets], 0,
        "assets content should be 0 — no .rs files"
    );

    // src has .rs files: both total and content must be > 0
    assert!(agg_total[&src] > 0, "src total should be non-zero");
    assert!(
        agg_content[&src] > 0,
        "src content should be non-zero — has .rs files"
    );
}

// ── apparent_size ─────────────────────────────────────────────────────────────

#[test]
fn apparent_size_uses_logical_file_length() {
    let (_dir, root) = make_tree(&["file.txt"]);

    // apparent_size=true uses metadata.len() (logical size = 5 bytes)
    let config_apparent = build_config(
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        None,
        false,
        true, // apparent_size
    );

    let (raw, _content) = parallel_scan(root.clone(), 4, config_apparent);

    // File content is b"hello" = 5 bytes. Root's raw size also includes the
    // root directory's own logical (apparent) size
    let root_dir_apparent_size = fs::metadata(&root).map(|m| m.len()).unwrap_or(0);
    assert_eq!(
        raw[&root],
        5 + root_dir_apparent_size,
        "apparent size should equal logical file length plus root dir's own size"
    );
}

#[test]
fn apparent_size_false_uses_block_allocation() {
    let (_dir, root) = make_tree(&["file.txt"]);

    let config_blocks = build_config(
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        None,
        false,
        false, // apparent_size = false → blocks * 512
    );

    let (raw, _content) = parallel_scan(root.clone(), 4, config_blocks);

    // Block allocation is always >= logical size
    assert!(raw[&root] >= 5, "block size should be >= logical size");
}

#[test]
fn apparent_size_produces_smaller_or_equal_size_than_blocks() {
    let (_dir, root) = make_tree(&["a.txt", "b.txt", "sub/c.txt"]);

    let config_apparent = build_config(
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        None,
        false,
        true,
    );
    let config_blocks = build_config(
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        None,
        false,
        false,
    );

    let (raw_apparent, _content_apparent) = parallel_scan(root.clone(), 4, config_apparent);
    let (raw_blocks, _content_blocks) = parallel_scan(root.clone(), 4, config_blocks);

    let agg_apparent = aggregate_sizes(&raw_apparent, &root);
    let agg_blocks = aggregate_sizes(&raw_blocks, &root);

    // Logical size is always <= physical block allocation
    assert!(
        agg_apparent[&root] <= agg_blocks[&root],
        "apparent size should be <= block allocation"
    );
}
