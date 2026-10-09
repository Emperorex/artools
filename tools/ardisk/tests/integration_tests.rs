use ardisk::duplicates::{DuplicateReport, PREFIX_LEN, find_duplicates};
use ardisk::report::{
    DirectoryQuery, DirectorySelection, DirectorySummary, Filters, ReportMeta, SCHEMA_VERSION,
    render_by_type_text, select_directories, write_by_type, write_directories, write_duplicates,
    write_largest_files,
};
use ardisk::types::{TypeAccumulator, TypeTable};
use ardisk::{
    Collect, DEFAULT_IGNORES, FileEntry, aggregate_sizes, build_config, build_config_with_exclude,
    build_exclude_matcher, format_size, parallel_scan, parallel_scan_collect,
    parallel_scan_with_files,
};
use glob::Pattern;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};
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

/// Same as `dir_self_size` but using logical length (apparent size) instead
/// of block allocation, for tests that run with `--apparent-size`.
fn dir_self_apparent_size(path: &PathBuf) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn default_config(debug: bool) -> std::sync::Arc<ardisk::ScanConfig> {
    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    build_config(ignore_dirs, None, debug, false, true)
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
    let config = build_config(ignore_dirs, None, false, false, true);

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

// A hard link is two directory entries pointing at the same inode — it's the
// same filesystem object no matter which "view" of size you ask for.
// Physical size and apparent (logical) size are just two different ways of
// measuring *that one object*, so dedup must apply identically to both, not
// only to the physical-size path.
#[cfg(unix)]
#[test]
fn hard_linked_files_are_only_counted_once_with_apparent_size() {
    use std::os::unix::fs::MetadataExt;

    let (_dir, root) = make_tree(&["a/real.txt"]);
    fs::hard_link(root.join("a/real.txt"), root.join("b").join("linked.txt")).unwrap_or_else(
        |_| {
            fs::create_dir_all(root.join("b")).unwrap();
            fs::hard_link(root.join("a/real.txt"), root.join("b/linked.txt")).unwrap();
        },
    );

    let file_meta = fs::metadata(root.join("a/real.txt")).unwrap();
    assert!(
        file_meta.nlink() > 1,
        "test setup: file should be hard-linked"
    );

    let config = build_config(
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        None,
        false,
        true, // apparent_size
        true,
    );
    let (raw, _content) = parallel_scan(root.clone(), 4, config);
    let sizes = aggregate_sizes(&raw, &root);

    // Logical size is 5 bytes ("hello"), same as with --apparent-size off,
    // and must still be counted only once even though it has two names.
    let single_file_size = file_meta.len();
    let expected = single_file_size
        + dir_self_apparent_size(&root)
        + dir_self_apparent_size(&root.join("a"))
        + dir_self_apparent_size(&root.join("b"));

    assert_eq!(
        sizes[&root], expected,
        "hard-linked file should only be counted once under --apparent-size too"
    );
}

// ── sparse file size semantics ────────────────────────────────────────────────

// A sparse file's logical length (st_size) can be far larger than what is
// actually allocated on disk — the "holes" in it don't consume blocks.
// ardisk's default (physical) mode must reflect real disk usage, exactly
// like `du`, while --apparent-size must reflect the logical length, exactly
// like `du --apparent-size`. This proves both halves of that contract on the
// same file: physical < apparent, and apparent == exact logical length. We
// deliberately do NOT assert an exact physical byte count (e.g. "4096")
// since block size and hole-filling behavior vary by filesystem — only that
// it's substantially smaller than the logical size.
#[cfg(unix)]
#[test]
fn sparse_file_physical_size_is_smaller_than_logical_size() {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;

    let (_dir, root) = make_tree(&[]);
    let sparse_path = root.join("sparse.bin");

    // Write a few bytes at the start, then seek far ahead and write a few
    // more, leaving an 8 MB hole that a sparse-aware filesystem (tmpfs,
    // ext4, xfs, apfs, ...) should not allocate blocks for.
    const LOGICAL_SIZE: u64 = 8 * 1024 * 1024; // 8 MB
    {
        let mut f = fs::File::create(&sparse_path).unwrap();
        f.write_all(b"start").unwrap();
        f.seek(SeekFrom::Start(LOGICAL_SIZE - 5)).unwrap();
        f.write_all(b"end!!").unwrap();
    }

    let file_meta = fs::metadata(&sparse_path).unwrap();
    assert_eq!(
        file_meta.len(),
        LOGICAL_SIZE,
        "test setup: unexpected logical file size"
    );

    // Whether write() past EOF actually leaves a hole is an environment
    // property (filesystem, encryption, temp-dir backing store), not
    // something ardisk controls. Skip gracefully rather than failing the
    // build on a machine/tmpdir that happens to fully materialize the gap.
    let physical_bytes = file_meta.blocks() * 512;
    if physical_bytes >= file_meta.len() {
        eprintln!(
            "skipping sparse_file_physical_size_is_smaller_than_logical_size: \
             this filesystem/tmpdir did not leave the file sparse (physical {} >= \
             logical {} bytes); sparse-file behavior is environment-dependent",
            physical_bytes,
            file_meta.len()
        );
        return;
    }

    // --apparent-size mode must equal the exact logical size (st_size),
    // holes included.
    let apparent_config = build_config(
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        None,
        false,
        true, // apparent_size
        true,
    );
    let (raw_apparent, _content) = parallel_scan(root.clone(), 4, apparent_config);
    let apparent_sizes = aggregate_sizes(&raw_apparent, &root);
    let expected_apparent = LOGICAL_SIZE + dir_self_apparent_size(&root);
    assert_eq!(
        apparent_sizes[&root], expected_apparent,
        "--apparent-size must report the file's exact logical size (st_size)"
    );

    // Default (physical) mode must reflect real block allocation, not the
    // logical size: strictly smaller than apparent, and by a wide margin —
    // not just off by a rounding block — since only ~10 bytes were actually
    // written into an 8 MB logical file.
    let physical_config = default_config(false);
    let (raw_physical, _content) = parallel_scan(root.clone(), 4, physical_config);
    let physical_sizes = aggregate_sizes(&raw_physical, &root);

    assert!(
        physical_sizes[&root] < apparent_sizes[&root],
        "physical size ({}) should be smaller than apparent size ({}) for a sparse file",
        physical_sizes[&root],
        apparent_sizes[&root]
    );
    assert!(
        physical_sizes[&root] <= apparent_sizes[&root] / 2,
        "physical size ({}) should be substantially smaller than apparent size ({}) \
         for an 8 MB file with only ~10 bytes actually written",
        physical_sizes[&root],
        apparent_sizes[&root]
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
        true,
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
        true,
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
        true,
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
        true,
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
        true,
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
        true,
    );
    let config_blocks = build_config(
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect(),
        None,
        false,
        false,
        true,
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

// ── --exclude ─────────────────────────────────────────────────────────────────

/// Scans `root` with the default ignores plus the given `--exclude` patterns
/// and returns the aggregated sizes.
fn run_with_exclude(root: &Path, patterns: &[&str]) -> std::collections::HashMap<PathBuf, u64> {
    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let patterns: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
    let exclude = build_exclude_matcher(root, &patterns).unwrap();
    let config = build_config_with_exclude(ignore_dirs, None, false, true, true, exclude);
    let (raw, _content) = parallel_scan(root.to_path_buf(), 4, config);
    aggregate_sizes(&raw, root)
}

#[test]
fn exclude_basename_glob_matches_at_any_depth() {
    let (_d, root) = make_tree(&["a.log", "a.rs", "sub/b.log", "sub/b.rs"]);
    let with = run_with_exclude(&root, &["*.log"]);
    let without = run_with_exclude(&root, &[]);
    // Two 5-byte .log files disappear from the total (apparent size).
    assert_eq!(without[&root] - with[&root], 10);
    assert_eq!(without[&root.join("sub")] - with[&root.join("sub")], 5);
}

#[test]
fn exclude_contents_pattern_keeps_the_directory_but_drops_its_contents() {
    let (_d, root) = make_tree(&["target/debug/app", "target/note.txt", "src/main.rs"]);
    let with = run_with_exclude(&root, &["target/**"]);
    let without = run_with_exclude(&root, &[]);

    // `target` itself is still reported (only its own inode cost remains)...
    let target = root.join("target");
    assert!(with.contains_key(&target));
    assert_eq!(with[&target], dir_self_apparent_size(&target));
    // ...its subdirectory is pruned entirely...
    assert!(!with.contains_key(&target.join("debug")));
    // ...and unrelated trees are untouched.
    assert_eq!(with[&root.join("src")], without[&root.join("src")]);
}

#[test]
fn exclude_contents_pattern_is_anchored_to_the_scan_root() {
    let (_d, root) = make_tree(&["target/x", "sub/target/y"]);
    let with = run_with_exclude(&root, &["target/**"]);
    let without = run_with_exclude(&root, &[]);
    // Root-level target contents excluded, nested sub/target untouched.
    assert_eq!(
        without[&root.join("target")] - with[&root.join("target")],
        5
    );
    assert_eq!(with[&root.join("sub")], without[&root.join("sub")]);
}

#[test]
fn exclude_nested_glob_does_not_block_traversal_of_parents() {
    // `src/**/*.rs` is a contents pattern: src and src/a must still be walked
    // so that the non-.rs files below them are counted.
    let (_d, root) = make_tree(&["src/a/lib.rs", "src/a/data.bin", "src/top.rs"]);
    let with = run_with_exclude(&root, &["src/**/*.rs"]);
    let without = run_with_exclude(&root, &[]);
    assert!(with.contains_key(&root.join("src")));
    assert!(with.contains_key(&root.join("src/a")));
    // Only data.bin (5 bytes) remains as file content under src/a.
    assert_eq!(
        with[&root.join("src/a")],
        dir_self_apparent_size(&root.join("src/a")) + 5
    );
    assert_eq!(without[&root.join("src/a")] - with[&root.join("src/a")], 5);
}

#[test]
fn exclude_trailing_slash_prunes_directories_only() {
    // `cache/` removes the directory (any depth); a *file* named cache stays.
    let (_d, root) = make_tree(&["cache/x", "sub/cache/y", "other/cache"]);
    let with = run_with_exclude(&root, &["cache/"]);
    assert!(!with.contains_key(&root.join("cache")));
    assert!(!with.contains_key(&root.join("sub/cache")));
    let without = run_with_exclude(&root, &[]);
    assert_eq!(with[&root.join("other")], without[&root.join("other")]);
}

#[test]
fn exclude_negation_reincludes_a_file() {
    let (_d, root) = make_tree(&["a.log", "keep.log"]);
    let with = run_with_exclude(&root, &["*.log", "!keep.log"]);
    let without = run_with_exclude(&root, &[]);
    assert_eq!(without[&root] - with[&root], 5); // only a.log removed
}

#[test]
fn exclude_applies_even_with_gitignore_handling_disabled() {
    let (_d, root) = make_tree(&["a.log", "b.rs"]);
    let ignore_dirs: HashSet<String> = HashSet::new();
    let patterns = vec!["*.log".to_string()];
    let exclude = build_exclude_matcher(&root, &patterns).unwrap();
    let config = build_config_with_exclude(ignore_dirs, None, false, true, false, exclude);
    let (raw, _c) = parallel_scan(root.clone(), 2, config);
    let sizes = aggregate_sizes(&raw, &root);
    assert_eq!(sizes[&root], dir_self_apparent_size(&root) + 5);
}

#[test]
fn exclude_wins_over_include() {
    let (_d, root) = make_tree(&["a.log", "b.log", "c.rs"]);
    let patterns = vec!["a.log".to_string()];
    let exclude = build_exclude_matcher(&root, &patterns).unwrap();
    let ignore_dirs: HashSet<String> = HashSet::new();
    let config = build_config_with_exclude(
        ignore_dirs,
        Some(Pattern::new("*.log").unwrap()),
        false,
        true,
        true,
        exclude,
    );
    let (_raw, content) = parallel_scan(root.clone(), 2, config);
    assert_eq!(content[&root], 5); // only b.log
}

#[test]
fn exclude_empty_pattern_is_rejected() {
    let (_d, root) = make_tree(&["a"]);
    assert!(build_exclude_matcher(&root, &["".to_string()]).is_err());
    assert!(build_exclude_matcher(&root, &["   ".to_string()]).is_err());
}

#[test]
fn exclude_without_patterns_builds_no_matcher() {
    let (_d, root) = make_tree(&["a"]);
    assert!(build_exclude_matcher(&root, &[]).unwrap().is_none());
}

// ── --largest-files ───────────────────────────────────────────────────────────

/// Creates files with explicit byte sizes. Returns the TempDir and canonical root.
fn make_sized_tree(files: &[(&str, usize)]) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for (rel, len) in files {
        let full = dir.path().join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full, vec![b'x'; *len]).unwrap();
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

/// Runs a scan collecting the `n` largest files. Uses logical sizes so the
/// expected values are exact, default ignores, no .gitignore handling.
fn largest(
    root: &Path,
    n: usize,
    workers: usize,
    include: Option<&str>,
    exclude: &[&str],
) -> Vec<FileEntry> {
    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let patterns: Vec<String> = exclude.iter().map(|s| s.to_string()).collect();
    let matcher = build_exclude_matcher(root, &patterns).unwrap();
    let config = build_config_with_exclude(
        ignore_dirs,
        include.map(|p| Pattern::new(p).unwrap()),
        false,
        true,
        false,
        matcher,
    );
    let (_raw, _content, files) = parallel_scan_with_files(root.to_path_buf(), workers, config, n);
    files
}

fn names(root: &PathBuf, files: &[FileEntry]) -> Vec<String> {
    files
        .iter()
        .map(|f| {
            f.path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[test]
fn largest_files_returns_top_n_largest_first() {
    let (_d, root) = make_sized_tree(&[
        ("small", 10),
        ("a/medium", 500),
        ("a/b/big", 9000),
        ("c/huge", 70000),
        ("tiny", 1),
    ]);
    let files = largest(&root, 3, 4, None, &[]);
    assert_eq!(names(&root, &files), ["c/huge", "a/b/big", "a/medium"]);
    assert_eq!(
        files.iter().map(|f| f.size).collect::<Vec<_>>(),
        [70000, 9000, 500]
    );
}

#[test]
fn largest_files_with_n_above_file_count_returns_all_files() {
    let (_d, root) = make_sized_tree(&[("a", 3), ("sub/b", 2)]);
    let files = largest(&root, 100, 2, None, &[]);
    assert_eq!(names(&root, &files), ["a", "sub/b"]);
}

#[test]
fn largest_files_never_lists_directories() {
    let (_d, root) = make_sized_tree(&[("deep/er/file", 4)]);
    let files = largest(&root, 10, 2, None, &[]);
    assert_eq!(names(&root, &files), ["deep/er/file"]);
}

#[test]
fn largest_files_ties_are_ordered_by_path() {
    let (_d, root) = make_sized_tree(&[("c", 7), ("a", 7), ("b", 7), ("d", 7)]);
    // Three of four equal-size files survive: the lexicographically first ones.
    let files = largest(&root, 3, 4, None, &[]);
    assert_eq!(names(&root, &files), ["a", "b", "c"]);
}

#[test]
fn largest_files_result_is_independent_of_worker_count() {
    let spec: Vec<(String, usize)> = (0..60)
        .map(|i| (format!("d{}/f{}", i % 7, i), (i * 37) % 11 + 1))
        .collect();
    let spec_ref: Vec<(&str, usize)> = spec.iter().map(|(p, n)| (p.as_str(), *n)).collect();
    let (_d, root) = make_sized_tree(&spec_ref);
    let one = largest(&root, 10, 1, None, &[]);
    let eight = largest(&root, 10, 8, None, &[]);
    assert_eq!(one, eight);
}

#[test]
fn largest_files_respects_include() {
    let (_d, root) = make_sized_tree(&[("big.txt", 9999), ("a.mp4", 300), ("b/c.mp4", 200)]);
    let files = largest(&root, 5, 2, Some("*.mp4"), &[]);
    assert_eq!(names(&root, &files), ["a.mp4", "b/c.mp4"]);
}

#[test]
fn largest_files_respects_exclude() {
    let (_d, root) = make_sized_tree(&[
        ("huge.log", 9999),
        ("target/debug/app", 5000),
        ("keep/data", 100),
    ]);
    let files = largest(&root, 5, 2, None, &["*.log", "target/**"]);
    assert_eq!(names(&root, &files), ["keep/data"]);
}

#[test]
fn largest_files_skips_ignored_directories() {
    let (_d, root) = make_sized_tree(&[("node_modules/pkg/blob", 9999), ("src/main.rs", 10)]);
    let files = largest(&root, 5, 2, None, &[]);
    assert_eq!(names(&root, &files), ["src/main.rs"]);
}

#[cfg(unix)]
#[test]
fn largest_files_skips_symlinks() {
    let (_d, root) = make_sized_tree(&[("real", 100)]);
    std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
    let files = largest(&root, 5, 2, None, &[]);
    assert_eq!(names(&root, &files), ["real"]);
}

#[cfg(unix)]
#[test]
fn largest_files_lists_a_hard_linked_file_once() {
    let (_d, root) = make_sized_tree(&[("a/original", 4000), ("other", 10)]);
    fs::create_dir_all(root.join("b")).unwrap();
    fs::hard_link(root.join("a/original"), root.join("b/alias")).unwrap();

    let files = largest(&root, 10, 4, None, &[]);
    assert_eq!(files.len(), 2, "one entry per inode, got {:?}", files);
    assert_eq!(files[0].size, 4000);
    // Which of the two names is reported depends on scheduling.
    let first = names(&root, &files[..1]).remove(0);
    assert!(first == "a/original" || first == "b/alias", "got {first}");
}

#[cfg(unix)]
#[test]
fn largest_files_excluded_hard_link_does_not_hide_the_other_name() {
    // The excluded name must not "use up" the inode: the surviving name is
    // still a legitimate candidate.
    let (_d, root) = make_sized_tree(&[("data.log", 4000)]);
    fs::hard_link(root.join("data.log"), root.join("data.bin")).unwrap();

    let files = largest(&root, 10, 2, None, &["*.log"]);
    assert_eq!(names(&root, &files), ["data.bin"]);
    assert_eq!(files[0].size, 4000);
}

#[test]
fn largest_files_sizes_add_up_to_the_directory_content_total() {
    // With N larger than the file count the report must account for exactly
    // the bytes the directory totals count (hard links included).
    let (_d, root) = make_sized_tree(&[("a", 123), ("x/b", 456), ("x/y/c", 789)]);
    #[cfg(unix)]
    fs::hard_link(root.join("a"), root.join("x/a2")).unwrap();

    let ignore_dirs: HashSet<String> = HashSet::new();
    let config = build_config(ignore_dirs, None, false, true, false);
    let (_raw, content, files) = parallel_scan_with_files(root.clone(), 4, config, 1000);
    let agg_content = aggregate_sizes(&content, &root);

    let listed: u64 = files.iter().map(|f| f.size).sum();
    assert_eq!(listed, agg_content[&root]);
}

#[test]
fn largest_files_zero_collects_nothing_and_keeps_directory_results() {
    let (_d, root) = make_sized_tree(&[("a", 10), ("s/b", 20)]);
    let ignore_dirs: HashSet<String> = HashSet::new();
    let config = build_config(ignore_dirs, None, false, true, false);
    let (raw, _content, files) = parallel_scan_with_files(root.clone(), 2, config, 0);
    assert!(files.is_empty());
    assert!(raw.contains_key(&root.join("s")));
}

// ── --duplicates ──────────────────────────────────────────────────────────────

/// Deterministic, non-repeating-looking bytes. Different seeds differ from
/// the very first byte on.
fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| {
            (i as u8)
                .wrapping_mul(31)
                .wrapping_add(seed)
                .wrapping_add((i >> 8) as u8)
        })
        .collect()
}

fn make_files(files: &[(&str, Vec<u8>)]) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for (rel, data) in files {
        let full = dir.path().join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full, data).unwrap();
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

/// Scans `root` in duplicate-candidate mode and runs the duplicate search.
fn find_dups(
    root: &Path,
    min_len: u64,
    workers: usize,
    include: Option<&str>,
    exclude: &[&str],
) -> DuplicateReport {
    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let patterns: Vec<String> = exclude.iter().map(|s| s.to_string()).collect();
    let matcher = build_exclude_matcher(root, &patterns).unwrap();
    let config = build_config_with_exclude(
        ignore_dirs,
        include.map(|p| Pattern::new(p).unwrap()),
        false,
        false,
        false,
        matcher,
    );
    let (_raw, _content, candidates) = parallel_scan_collect(
        root.to_path_buf(),
        workers,
        config,
        Collect::Duplicates { min_len },
    );
    find_duplicates(candidates, workers)
}

/// Each group as a sorted list of root-relative paths.
fn group_names(root: &Path, report: &DuplicateReport) -> Vec<Vec<String>> {
    report
        .groups
        .iter()
        .map(|g| {
            g.paths
                .iter()
                .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
                .collect()
        })
        .collect()
}

#[test]
fn duplicates_groups_identical_files_across_directories() {
    let (_d, root) = make_files(&[
        ("a/one.bin", pattern(1, 500)),
        ("b/deeper/other-name.dat", pattern(1, 500)),
        ("c/different.bin", pattern(2, 500)),
    ]);
    let report = find_dups(&root, 1, 4, None, &[]);
    assert_eq!(
        group_names(&root, &report),
        [vec!["a/one.bin", "b/deeper/other-name.dat"]]
    );
    assert_eq!(report.groups[0].len, 500);
    assert_eq!(report.groups[0].reclaimable(), 500);
}

#[test]
fn duplicates_same_length_different_content_is_not_a_group() {
    let (_d, root) = make_files(&[("a", vec![b'a'; 64]), ("b", vec![b'b'; 64])]);
    let report = find_dups(&root, 1, 2, None, &[]);
    assert!(report.groups.is_empty());
}

#[test]
fn duplicates_same_prefix_but_different_tail_is_not_a_group() {
    // Identical for the first PREFIX_LEN bytes, different after: only the
    // full hash can tell them apart.
    let len = PREFIX_LEN as usize + 5000;
    let a = pattern(3, len);
    let mut b = a.clone();
    *b.last_mut().unwrap() ^= 0xff;
    let (_d, root) = make_files(&[("a", a), ("b", b)]);
    let report = find_dups(&root, 1, 2, None, &[]);
    assert!(report.groups.is_empty());
    assert_eq!(
        report.stats.full_hashed, 2,
        "the prefix could not tell them apart"
    );
}

#[test]
fn duplicates_large_identical_files_spanning_several_read_buffers() {
    let data = pattern(9, 300_000); // > the 128 KiB read buffer
    let (_d, root) = make_files(&[("x/big1", data.clone()), ("y/big2", data)]);
    let report = find_dups(&root, 1, 3, None, &[]);
    assert_eq!(group_names(&root, &report), [vec!["x/big1", "y/big2"]]);
    assert_eq!(report.stats.full_hashed, 2);
    assert_eq!(report.stats.bytes_read, 2 * PREFIX_LEN + 2 * 300_000);
}

#[test]
fn duplicates_a_differing_prefix_avoids_reading_the_whole_file() {
    let a = pattern(1, 10_000);
    let b = pattern(2, 10_000); // differs from byte 0
    let (_d, root) = make_files(&[("a", a), ("b", b)]);
    let report = find_dups(&root, 1, 2, None, &[]);
    assert!(report.groups.is_empty());
    assert_eq!(report.stats.full_hashed, 0);
    assert_eq!(report.stats.bytes_read, 2 * PREFIX_LEN);
}

#[test]
fn duplicates_files_with_a_unique_length_are_never_opened() {
    let (_d, root) = make_files(&[
        ("a", pattern(1, 100)),
        ("b", pattern(1, 100)),
        ("unique", pattern(1, 50_000)),
    ]);
    let report = find_dups(&root, 1, 2, None, &[]);
    assert_eq!(report.stats.candidates, 3);
    assert_eq!(report.stats.same_length, 2);
    assert_eq!(
        report.stats.bytes_read, 200,
        "only the two 100-byte files are read"
    );
}

#[test]
fn duplicates_small_files_are_final_after_the_prefix_stage() {
    // Shorter than the prefix: the prefix hash covers the whole file, so no
    // second read is needed.
    let (_d, root) = make_files(&[("a", pattern(5, 1000)), ("b", pattern(5, 1000))]);
    let report = find_dups(&root, 1, 2, None, &[]);
    assert_eq!(report.groups.len(), 1);
    assert_eq!(report.stats.full_hashed, 0);
    assert_eq!(report.stats.bytes_read, 2000);
}

#[test]
fn duplicates_groups_are_ordered_by_reclaimable_space() {
    let (_d, root) = make_files(&[
        // 3 x 1000 -> 2000 reclaimable
        ("s1", pattern(1, 1000)),
        ("s2", pattern(1, 1000)),
        ("s3", pattern(1, 1000)),
        // 2 x 5000 -> 5000 reclaimable
        ("b1", pattern(2, 5000)),
        ("b2", pattern(2, 5000)),
    ]);
    let report = find_dups(&root, 1, 2, None, &[]);
    assert_eq!(
        group_names(&root, &report),
        [vec!["b1", "b2"], vec!["s1", "s2", "s3"]]
    );
    assert_eq!(report.reclaimable(), 7000);
    assert_eq!(report.redundant_files(), 3);
}

#[test]
fn duplicates_empty_files_are_never_reported() {
    let (_d, root) = make_files(&[("e1", vec![]), ("e2", vec![]), ("e3", vec![])]);
    // Even an explicit minimum of 0 must not turn empty files into a group.
    let report = find_dups(&root, 0, 2, None, &[]);
    assert!(report.groups.is_empty());
    assert_eq!(report.stats.candidates, 0);
}

#[test]
fn duplicates_min_len_drops_small_files() {
    let (_d, root) = make_files(&[
        ("s1", pattern(1, 100)),
        ("s2", pattern(1, 100)),
        ("b1", pattern(2, 10_000)),
        ("b2", pattern(2, 10_000)),
    ]);
    let report = find_dups(&root, 1000, 2, None, &[]);
    assert_eq!(group_names(&root, &report), [vec!["b1", "b2"]]);
    assert_eq!(report.stats.candidates, 2);
}

#[test]
fn duplicates_respect_include_and_exclude() {
    let (_d, root) = make_files(&[
        ("a.jpg", pattern(1, 800)),
        ("b.jpg", pattern(1, 800)),
        ("c.png", pattern(1, 800)),
        ("d.png", pattern(1, 800)),
        ("target/e.jpg", pattern(1, 800)),
    ]);
    let included = find_dups(&root, 1, 2, Some("*.jpg"), &["target/**"]);
    assert_eq!(group_names(&root, &included), [vec!["a.jpg", "b.jpg"]]);

    let excluded = find_dups(&root, 1, 2, None, &["*.png", "target/**"]);
    assert_eq!(group_names(&root, &excluded), [vec!["a.jpg", "b.jpg"]]);
}

#[test]
fn duplicates_skip_ignored_directories() {
    let (_d, root) = make_files(&[
        ("node_modules/pkg/f", pattern(1, 800)),
        ("src/f", pattern(1, 800)),
    ]);
    let report = find_dups(&root, 1, 2, None, &[]);
    assert!(
        report.groups.is_empty(),
        "node_modules is ignored by default"
    );
}

#[cfg(unix)]
#[test]
fn duplicates_ignore_symlinks() {
    let (_d, root) = make_files(&[("real", pattern(1, 800))]);
    std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
    let report = find_dups(&root, 1, 2, None, &[]);
    assert!(report.groups.is_empty());
    assert_eq!(report.stats.candidates, 1);
}

#[cfg(unix)]
#[test]
fn duplicates_hard_links_to_one_file_are_not_duplicates() {
    let (_d, root) = make_files(&[("a/orig", pattern(1, 2000))]);
    fs::create_dir_all(root.join("b")).unwrap();
    fs::hard_link(root.join("a/orig"), root.join("b/link")).unwrap();
    let report = find_dups(&root, 1, 4, None, &[]);
    assert!(report.groups.is_empty(), "same inode: nothing to reclaim");
    assert_eq!(report.stats.candidates, 1);
}

#[cfg(unix)]
#[test]
fn duplicates_a_real_copy_of_a_hard_linked_file_is_one_group_of_two() {
    let (_d, root) = make_files(&[("a/orig", pattern(1, 2000)), ("z/copy", pattern(1, 2000))]);
    fs::hard_link(root.join("a/orig"), root.join("a/link")).unwrap();
    let report = find_dups(&root, 1, 4, None, &[]);
    assert_eq!(report.groups.len(), 1);
    let names = &group_names(&root, &report)[0];
    // Two physical copies, not three; which of the two hard-link names is
    // shown depends on scheduling.
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"z/copy".to_string()));
    assert_eq!(report.groups[0].reclaimable(), 2000);
}

#[cfg(unix)]
#[test]
fn duplicates_unreadable_files_are_reported_not_fatal() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, root) = make_files(&[
        ("ok1", pattern(1, 900)),
        ("ok2", pattern(1, 900)),
        ("locked", pattern(1, 900)),
    ]);
    let locked = root.join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(&locked).is_ok() {
        return; // running as root: permissions are not enforced
    }
    let report = find_dups(&root, 1, 2, None, &[]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(group_names(&root, &report), [vec!["ok1", "ok2"]]);
    assert_eq!(report.unreadable.len(), 1);
    assert_eq!(report.unreadable[0].0, locked);
}

#[test]
fn duplicates_a_file_whose_size_changed_is_skipped() {
    // Candidates claim 10 bytes but the files hold 5: as if they had been
    // truncated after the scan. They must not be grouped.
    let (_d, root) = make_files(&[("a", vec![b'x'; 5]), ("b", vec![b'x'; 5])]);
    let candidates = vec![
        FileEntry {
            size: 10,
            path: root.join("a"),
        },
        FileEntry {
            size: 10,
            path: root.join("b"),
        },
    ];
    let report = find_duplicates(candidates, 2);
    assert!(report.groups.is_empty());
    assert_eq!(report.unreadable.len(), 2);
    assert!(report.unreadable[0].1.contains("changed"));
}

#[test]
fn duplicates_result_is_independent_of_worker_count() {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..48usize {
        // 6 content classes of varying sizes, spread over 5 directories.
        let class = (i % 6) as u8;
        let len = 300 + usize::from(class) * 2500;
        files.push((format!("d{}/f{}", i % 5, i), pattern(class, len)));
    }
    let spec: Vec<(&str, Vec<u8>)> = files.iter().map(|(p, d)| (p.as_str(), d.clone())).collect();
    let (_d, root) = make_files(&spec);

    let one = find_dups(&root, 1, 1, None, &[]);
    let eight = find_dups(&root, 1, 8, None, &[]);
    assert_eq!(one.groups, eight.groups);
    assert_eq!(one.groups.len(), 6);
    assert_eq!(one.redundant_files(), 48 - 6);
}

#[test]
fn collect_nothing_and_duplicates_do_not_change_directory_totals() {
    let (_d, root) = make_files(&[("a", pattern(1, 700)), ("s/b", pattern(1, 700))]);
    let ignore_dirs: HashSet<String> = HashSet::new();
    let config = build_config(ignore_dirs.clone(), None, false, true, false);
    let (raw_none, _c1, none) = parallel_scan_collect(root.clone(), 2, config, Collect::Nothing);
    let config = build_config(ignore_dirs, None, false, true, false);
    let (raw_dup, _c2, dup) =
        parallel_scan_collect(root.clone(), 2, config, Collect::Duplicates { min_len: 1 });
    assert!(none.is_empty());
    assert_eq!(dup.len(), 2);
    assert_eq!(raw_none, raw_dup);
}

// ── report selection and JSON writers ─────────────────────────────────────────

fn query(top: usize) -> DirectoryQuery {
    DirectoryQuery {
        top,
        ..DirectoryQuery::default()
    }
}

fn dir_maps(root: &Path, dirs: &[(&str, u64)]) -> HashMap<PathBuf, u64> {
    dirs.iter()
        .map(|(rel, size)| {
            let p = if rel.is_empty() {
                root.to_path_buf()
            } else {
                root.join(rel)
            };
            (p, *size)
        })
        .collect()
}

fn selected(root: &Path, sel: &DirectorySelection) -> Vec<(String, u64)> {
    sel.entries
        .iter()
        .map(|(p, s)| {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            (if rel.is_empty() { ".".into() } else { rel }, *s)
        })
        .collect()
}

fn meta(root: &Path) -> ReportMeta {
    ReportMeta {
        root: root.to_path_buf(),
        apparent_size: false,
        filters: Filters::default(),
    }
}

fn to_json<F>(write: F) -> (serde_json::Value, usize)
where
    F: FnOnce(&mut Vec<u8>) -> std::io::Result<usize>,
{
    let mut buf = Vec::new();
    let lossy = write(&mut buf).unwrap();
    let text = String::from_utf8(buf).expect("JSON output is UTF-8");
    assert!(text.ends_with('\n'), "document ends with a newline");
    (serde_json::from_str(&text).expect("valid JSON"), lossy)
}

#[test]
fn select_directories_orders_by_size_then_path() {
    let root = PathBuf::from("/r");
    let all = dir_maps(&root, &[("", 100), ("b", 10), ("a", 10), ("c", 50)]);
    let sel = select_directories(&all, &all, &root, &query(10));
    assert_eq!(
        selected(&root, &sel),
        [
            (".".into(), 100),
            ("c".into(), 50),
            ("a".into(), 10),
            ("b".into(), 10)
        ]
    );
    assert!(!sel.truncated);
}

#[test]
fn select_directories_reports_truncation_only_when_something_was_left_out() {
    let root = PathBuf::from("/r");
    let all = dir_maps(&root, &[("", 100), ("a", 10), ("b", 5)]);
    assert!(select_directories(&all, &all, &root, &query(2)).truncated);
    assert!(!select_directories(&all, &all, &root, &query(3)).truncated);
    assert!(!select_directories(&all, &all, &root, &query(4)).truncated);
}

#[test]
fn select_directories_summarize_lists_nothing_but_flags_truncation() {
    let root = PathBuf::from("/r");
    let all = dir_maps(&root, &[("", 100), ("a", 10)]);
    let q = DirectoryQuery {
        top: 20,
        summarize: true,
        ..DirectoryQuery::default()
    };
    let sel = select_directories(&all, &all, &root, &q);
    assert!(sel.entries.is_empty());
    assert!(sel.truncated);
}

#[test]
fn select_directories_applies_max_depth_and_threshold() {
    let root = PathBuf::from("/r");
    let all = dir_maps(&root, &[("", 900), ("a", 500), ("a/b", 400), ("c", 7)]);
    let q = DirectoryQuery {
        top: 10,
        max_depth: Some(1),
        threshold_bytes: Some(100),
        ..DirectoryQuery::default()
    };
    let sel = select_directories(&all, &all, &root, &q);
    assert_eq!(
        selected(&root, &sel),
        [(".".into(), 900), ("a".into(), 500)]
    );
    assert!(
        !sel.truncated,
        "depth/threshold exclusions are not truncation"
    );
}

#[test]
fn select_directories_hides_directories_without_matching_content_for_include() {
    let root = PathBuf::from("/r");
    let all = dir_maps(&root, &[("", 9000), ("has", 4100), ("empty", 4096)]);
    let content = dir_maps(&root, &[("", 5), ("has", 5), ("empty", 0)]);
    let q = DirectoryQuery {
        top: 10,
        include_active: true,
        ..DirectoryQuery::default()
    };
    let sel = select_directories(&all, &content, &root, &q);
    assert_eq!(
        selected(&root, &sel),
        [(".".into(), 9000), ("has".into(), 4100)]
    );
}

#[test]
fn json_directories_document_has_the_documented_shape() {
    let (_d, root) = make_sized_tree(&[("a/f", 10), ("a/b/g", 20), ("c", 5)]);
    let all = dir_maps(&root, &[("", 300), ("a", 200), ("a/b", 100)]);
    let q = query(10);
    let sel = select_directories(&all, &all, &root, &q);
    let summary = DirectorySummary {
        total_bytes: 300,
        files: 3,
        directories: 3,
    };
    let (doc, lossy) = to_json(|w| write_directories(w, &meta(&root), &q, &sel, summary));
    assert_eq!(lossy, 0);

    assert_eq!(doc["schema_version"], SCHEMA_VERSION);
    assert_eq!(doc["mode"], "directories");
    assert_eq!(doc["root"], root.to_str().unwrap());
    assert_eq!(doc["size_mode"], "disk");
    assert_eq!(doc["filters"]["include"], serde_json::Value::Null);
    assert_eq!(doc["filters"]["exclude"], serde_json::json!([]));
    assert_eq!(doc["filters"]["no_ignore"], false);
    assert_eq!(doc["params"]["top"], 10);
    assert_eq!(doc["params"]["max_depth"], serde_json::Value::Null);
    assert_eq!(doc["summary"]["total_bytes"], 300);
    assert_eq!(doc["summary"]["files"], 3);
    assert_eq!(doc["summary"]["directories"], 3);
    assert_eq!(doc["truncated"], false);
    assert_eq!(
        doc["entries"],
        serde_json::json!([
            {"path": ".",   "bytes": 300, "depth": 0, "kind": "directory"},
            {"path": "a",   "bytes": 200, "depth": 1, "kind": "directory"},
            {"path": "a/b", "bytes": 100, "depth": 2, "kind": "directory"},
        ])
    );
    assert!(doc.get("groups").is_none(), "no keys from other modes");
}

#[test]
fn json_echoes_filters_and_apparent_size_mode() {
    let (_d, root) = make_sized_tree(&[("f", 1)]);
    let m = ReportMeta {
        root: root.clone(),
        apparent_size: true,
        filters: Filters {
            include: Some("*.rs".into()),
            exclude: vec!["*.log".into(), "target/**".into()],
            ignore: vec!["vendor".into()],
            no_ignore: true,
            no_hidden: true,
        },
    };
    let all = dir_maps(&root, &[("", 1)]);
    let q = query(5);
    let sel = select_directories(&all, &all, &root, &q);
    let summary = DirectorySummary {
        total_bytes: 1,
        files: 1,
        directories: 1,
    };
    let (doc, _) = to_json(|w| write_directories(w, &m, &q, &sel, summary));
    assert_eq!(doc["size_mode"], "apparent");
    assert_eq!(doc["filters"]["include"], "*.rs");
    assert_eq!(
        doc["filters"]["exclude"],
        serde_json::json!(["*.log", "target/**"])
    );
    assert_eq!(doc["filters"]["ignore"], serde_json::json!(["vendor"]));
    assert_eq!(doc["filters"]["no_ignore"], true);
    assert_eq!(doc["filters"]["no_hidden"], true);
}

#[test]
fn json_largest_files_document_has_the_documented_shape() {
    let (_d, root) = make_sized_tree(&[("a/big", 900), ("small", 10), ("b/mid", 90)]);
    let ignore_dirs: HashSet<String> = HashSet::new();
    let config = build_config(ignore_dirs, None, false, true, false);
    let out = ardisk::parallel_scan_report(root.clone(), 2, config, Collect::Largest(2));
    let (doc, lossy) =
        to_json(|w| write_largest_files(w, &meta(&root), 2, 1000, out.file_count, &out.files));
    assert_eq!(lossy, 0);
    assert_eq!(doc["mode"], "largest_files");
    assert_eq!(doc["params"]["limit"], 2);
    assert_eq!(doc["summary"]["files"], 3);
    assert_eq!(doc["truncated"], true, "3 files exist, 2 listed");
    assert_eq!(
        doc["entries"],
        serde_json::json!([
            {"path": "a/big", "bytes": 900, "kind": "file"},
            {"path": "b/mid", "bytes": 90,  "kind": "file"},
        ])
    );
}

#[test]
fn json_largest_files_is_not_truncated_when_everything_is_listed() {
    let (_d, root) = make_sized_tree(&[("a", 3), ("b", 2)]);
    let ignore_dirs: HashSet<String> = HashSet::new();
    let config = build_config(ignore_dirs, None, false, true, false);
    let out = ardisk::parallel_scan_report(root.clone(), 2, config, Collect::Largest(10));
    let (doc, _) =
        to_json(|w| write_largest_files(w, &meta(&root), 10, 5, out.file_count, &out.files));
    assert_eq!(doc["truncated"], false);
}

#[test]
fn scan_report_counts_only_files_that_contribute_to_the_totals() {
    let (_d, root) = make_sized_tree(&[("a.rs", 3), ("b.log", 4), ("sub/c.rs", 5)]);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("a.rs"), root.join("link")).unwrap();
        fs::hard_link(root.join("sub/c.rs"), root.join("c-again.rs")).unwrap();
    }
    let ignore_dirs: HashSet<String> = HashSet::new();
    let patterns = vec!["*.log".to_string()];
    let exclude = build_exclude_matcher(&root, &patterns).unwrap();
    let config = build_config_with_exclude(ignore_dirs, None, false, true, false, exclude);
    let out = ardisk::parallel_scan_report(root.clone(), 3, config, Collect::Nothing);
    // a.rs and c.rs (once, despite the hard link): the symlink and the
    // excluded .log file do not count.
    assert_eq!(out.file_count, 2);
    assert!(out.files.is_empty());
}

fn dup_report_for(files: &[(&str, Vec<u8>)]) -> (TempDir, PathBuf, DuplicateReport) {
    let (d, root) = make_files(files);
    let report = find_dups(&root, 1, 2, None, &[]);
    (d, root, report)
}

#[test]
fn json_duplicates_document_has_the_documented_shape() {
    let data = pattern(7, 6000);
    let (_d, root, report) = dup_report_for(&[
        ("backup/one.zip", data.clone()),
        ("backup/two.zip", data.clone()),
        ("other/three.zip", data.clone()),
        ("x", pattern(8, 300)),
        ("y", pattern(8, 300)),
    ]);
    let (doc, lossy) = to_json(|w| write_duplicates(w, &meta(&root), 20, false, 1, &report));
    assert_eq!(lossy, 0);

    assert_eq!(doc["mode"], "duplicates");
    assert_eq!(
        doc["size_mode"], "apparent",
        "duplicate sizes are logical lengths, whatever --apparent-size says"
    );
    assert_eq!(
        doc["params"],
        serde_json::json!({"top": 20, "min_size_bytes": 1})
    );
    assert_eq!(doc["truncated"], false);
    assert!(doc.get("entries").is_none(), "no keys from other modes");

    let s = &doc["summary"];
    assert_eq!(s["groups"], 2);
    assert_eq!(s["duplicate_files"], 5);
    assert_eq!(s["redundant_files"], 3);
    assert_eq!(s["potentially_reclaimable_bytes"], 2 * 6000 + 300);
    assert_eq!(s["files_considered"], 5);
    assert_eq!(s["unreadable_files"], 0);

    let g = &doc["groups"][0];
    assert_eq!(g["bytes_per_file"], 6000);
    assert_eq!(
        g["files"],
        serde_json::json!(["backup/one.zip", "backup/two.zip", "other/three.zip"])
    );
    // One copy is kept: two of three are reclaimable, not all three.
    assert_eq!(g["potentially_reclaimable_bytes"], 2 * 6000);
    let expected = format!("blake3:{}", blake3::hash(&data).to_hex());
    assert_eq!(g["hash"], expected.as_str());
}

#[test]
fn json_duplicates_summary_matches_the_sum_of_the_groups() {
    let (_d, root, report) = dup_report_for(&[
        ("a", pattern(1, 5000)),
        ("b", pattern(1, 5000)),
        ("c", pattern(2, 700)),
        ("d", pattern(2, 700)),
        ("e", pattern(2, 700)),
    ]);
    let (doc, _) = to_json(|w| write_duplicates(w, &meta(&root), 20, false, 1, &report));
    let listed: u64 = doc["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["potentially_reclaimable_bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(doc["summary"]["potentially_reclaimable_bytes"], listed);
    assert_eq!(listed, 5000 + 2 * 700);
}

#[test]
fn json_duplicates_top_limits_the_list_but_not_the_summary() {
    let (_d, root, report) = dup_report_for(&[
        ("a", pattern(1, 5000)),
        ("b", pattern(1, 5000)),
        ("c", pattern(2, 700)),
        ("d", pattern(2, 700)),
    ]);
    let (doc, _) = to_json(|w| write_duplicates(w, &meta(&root), 1, false, 1, &report));
    assert_eq!(doc["groups"].as_array().unwrap().len(), 1);
    assert_eq!(
        doc["groups"][0]["bytes_per_file"], 5000,
        "most reclaimable first"
    );
    assert_eq!(doc["truncated"], true);
    assert_eq!(doc["summary"]["groups"], 2);
    assert_eq!(doc["summary"]["potentially_reclaimable_bytes"], 5700);
}

#[test]
fn json_duplicates_summarize_lists_no_groups() {
    let (_d, root, report) = dup_report_for(&[("a", pattern(1, 900)), ("b", pattern(1, 900))]);
    let (doc, _) = to_json(|w| write_duplicates(w, &meta(&root), 20, true, 1, &report));
    assert_eq!(doc["groups"], serde_json::json!([]));
    assert_eq!(doc["truncated"], true);
    assert_eq!(doc["summary"]["groups"], 1);
}

#[test]
fn json_duplicates_without_duplicates_is_an_empty_but_valid_document() {
    let (_d, root, report) = dup_report_for(&[("a", pattern(1, 900)), ("b", pattern(2, 900))]);
    let (doc, _) = to_json(|w| write_duplicates(w, &meta(&root), 20, false, 1, &report));
    assert_eq!(doc["groups"], serde_json::json!([]));
    assert_eq!(doc["truncated"], false);
    assert_eq!(doc["summary"]["groups"], 0);
    assert_eq!(doc["summary"]["potentially_reclaimable_bytes"], 0);
}

#[cfg(unix)]
#[test]
fn json_replaces_invalid_utf8_in_paths_and_says_so() {
    use std::os::unix::ffi::OsStrExt;
    let (_d, root) = make_sized_tree(&[("ok", 1)]);
    let bad_name = std::ffi::OsStr::from_bytes(b"bad-\xff-name");
    let bad_path = root.join(bad_name);
    if fs::write(&bad_path, b"x").is_err() {
        return; // filesystem refuses non-UTF-8 names (e.g. macOS APFS)
    }
    let files = vec![
        FileEntry {
            size: 5,
            path: bad_path,
        },
        FileEntry {
            size: 1,
            path: root.join("ok"),
        },
    ];
    let (doc, lossy) = to_json(|w| write_largest_files(w, &meta(&root), 10, 6, 2, &files));
    assert_eq!(lossy, 1);
    let first = doc["entries"][0]["path"].as_str().unwrap();
    assert!(first.contains('\u{FFFD}'), "got {first:?}");
    assert_eq!(doc["entries"][1]["path"], "ok");
}

// ── --by-type ─────────────────────────────────────────────────────────────────

/// Scans `root` in by-type mode and returns the table. The scan does not
/// honour .gitignore files; `DEFAULT_IGNORES` apply.
fn by_type(
    root: &Path,
    workers: usize,
    apparent: bool,
    include: Option<&str>,
    exclude: &[&str],
) -> TypeTable {
    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let patterns: Vec<String> = exclude.iter().map(|s| s.to_string()).collect();
    let matcher = build_exclude_matcher(root, &patterns).unwrap();
    let config = build_config_with_exclude(
        ignore_dirs,
        include.map(|p| Pattern::new(p).unwrap()),
        false,
        apparent,
        false,
        matcher,
    );
    ardisk::parallel_scan_report(root.to_path_buf(), workers, config, Collect::ByType).types
}

/// `(label, files, bytes)` per row; `None` extensions are shown as "-".
fn type_rows(table: &TypeTable) -> Vec<(String, u64, u64)> {
    table
        .rows
        .iter()
        .map(|r| {
            (
                r.extension.clone().unwrap_or_else(|| "-".to_string()),
                r.files,
                r.bytes,
            )
        })
        .collect()
}

fn row(ext: &str, files: u64, bytes: u64) -> (String, u64, u64) {
    (ext.to_string(), files, bytes)
}

#[test]
fn by_type_counts_files_and_bytes_per_extension() {
    let (_d, root) = make_sized_tree(&[
        ("a.mp4", 5000),
        ("sub/b.mp4", 700),
        ("c.jpg", 900),
        ("sub/deep/d.rs", 30),
        ("e.rs", 20),
        ("f.rs", 10),
    ]);
    let t = by_type(&root, 3, true, None, &[]);
    assert_eq!(
        type_rows(&t),
        [row(".mp4", 2, 5700), row(".jpg", 1, 900), row(".rs", 3, 60)]
    );
    assert_eq!((t.total_files, t.total_bytes), (6, 6660));
}

#[test]
fn by_type_uses_the_last_extension_and_treats_dotfiles_as_extensionless() {
    let (_d, root) = make_sized_tree(&[
        ("foo.tar.gz", 100),
        ("archive.zip", 10),
        ("README", 1),
        (".env", 2),
        ("foo.", 4),
        (".config.json", 8),
        (".gitignore", 16),
    ]);
    let t = by_type(&root, 2, true, None, &[]);
    assert_eq!(
        type_rows(&t),
        [
            row(".gz", 1, 100),
            // README + .env + foo. + .gitignore
            row("-", 4, 23),
            row(".zip", 1, 10),
            row(".json", 1, 8),
        ]
    );
}

#[test]
fn by_type_folds_case() {
    let (_d, root) = make_sized_tree(&[("a.JPG", 1), ("b.jpg", 2), ("c.Jpg", 4)]);
    let t = by_type(&root, 2, true, None, &[]);
    assert_eq!(type_rows(&t), [row(".jpg", 3, 7)]);
}

#[test]
fn by_type_sorts_by_bytes_then_extension() {
    let (_d, root) = make_sized_tree(&[("a.zip", 10), ("b.avi", 10), ("c.mp4", 50), ("noext", 10)]);
    let t = by_type(&root, 2, true, None, &[]);
    assert_eq!(
        type_rows(&t),
        [
            row(".mp4", 1, 50),
            row("-", 1, 10),
            row(".avi", 1, 10),
            row(".zip", 1, 10)
        ]
    );
}

#[test]
fn by_type_skips_ignored_directories() {
    let (_d, root) = make_sized_tree(&[("node_modules/p/x.js", 999), ("src/y.js", 5)]);
    let t = by_type(&root, 2, true, None, &[]);
    assert_eq!(type_rows(&t), [row(".js", 1, 5)]);
}

#[cfg(unix)]
#[test]
fn by_type_ignores_symlinks_and_counts_hard_links_once() {
    let (_d, root) = make_sized_tree(&[("real.dat", 100)]);
    std::os::unix::fs::symlink(root.join("real.dat"), root.join("link.dat")).unwrap();
    fs::hard_link(root.join("real.dat"), root.join("alias.dat")).unwrap();
    let t = by_type(&root, 4, true, None, &[]);
    assert_eq!(type_rows(&t), [row(".dat", 1, 100)]);
}

#[cfg(unix)]
#[test]
fn by_type_counts_a_hard_link_under_the_name_that_passes_the_filters() {
    let (_d, root) = make_sized_tree(&[("data.log", 400)]);
    fs::hard_link(root.join("data.log"), root.join("data.bin")).unwrap();
    let t = by_type(&root, 2, true, None, &["*.log"]);
    assert_eq!(type_rows(&t), [row(".bin", 1, 400)]);
}

#[test]
fn by_type_respects_include_and_exclude() {
    let (_d, root) = make_sized_tree(&[
        ("a.rs", 10),
        ("b.md", 20),
        ("target/c.rs", 40),
        ("sub/d.rs", 80),
    ]);
    let included = by_type(&root, 2, true, Some("*.rs"), &["target/**"]);
    assert_eq!(type_rows(&included), [row(".rs", 2, 90)]);

    let excluded = by_type(&root, 2, true, None, &["*.md", "target/**"]);
    assert_eq!(type_rows(&excluded), [row(".rs", 2, 90)]);
}

#[test]
fn by_type_is_independent_of_the_worker_count() {
    let names: Vec<(String, usize)> = (0..80)
        .map(|i| {
            let ext = ["rs", "RS", "md", "png", "", "gz"][i % 6];
            let name = if ext.is_empty() {
                format!("d{}/file{}", i % 7, i)
            } else {
                format!("d{}/file{}.{}", i % 7, i, ext)
            };
            (name, (i * 13) % 50 + 1)
        })
        .collect();
    let spec: Vec<(&str, usize)> = names.iter().map(|(n, l)| (n.as_str(), *l)).collect();
    let (_d, root) = make_sized_tree(&spec);
    let one = by_type(&root, 1, true, None, &[]);
    let eight = by_type(&root, 8, true, None, &[]);
    assert_eq!(one, eight);
    assert_eq!(one.total_files, 80);
}

#[test]
fn by_type_total_equals_the_directory_content_total() {
    // One scan, two views: the per-type bytes must add up to the file bytes
    // the directory report rolls up, in both sizing modes.
    let (_d, root) = make_sized_tree(&[
        ("a.txt", 1234),
        ("sub/b.bin", 98765),
        ("sub/deep/c", 5),
        ("sub/deep/d.TXT", 4097),
    ]);
    for apparent in [true, false] {
        let config = build_config(HashSet::new(), None, false, apparent, false);
        let out = ardisk::parallel_scan_report(root.clone(), 3, config, Collect::ByType);
        let content = aggregate_sizes(&out.content_sizes, &root);
        assert_eq!(out.types.total_bytes, content[&root], "apparent={apparent}");
        assert_eq!(out.types.total_files, out.file_count);
        let per_type: u64 = out.types.rows.iter().map(|r| r.bytes).sum();
        assert_eq!(per_type, out.types.total_bytes);
    }
}

#[test]
fn by_type_does_not_change_directory_totals() {
    let (_d, root) = make_sized_tree(&[("a.rs", 70), ("s/b.md", 90)]);
    let scan = |mode| {
        let config = build_config(HashSet::new(), None, false, true, false);
        ardisk::parallel_scan_report(root.clone(), 2, config, mode)
    };
    let plain = scan(Collect::Nothing);
    let typed = scan(Collect::ByType);
    assert_eq!(plain.raw_sizes, typed.raw_sizes);
    assert_eq!(plain.file_count, typed.file_count);
    assert!(plain.types.rows.is_empty());
    assert!(typed.files.is_empty(), "no individual files are kept");
}

#[test]
fn by_type_of_an_empty_tree_is_an_empty_table() {
    let (_d, root) = make_tree(&[]);
    let t = by_type(&root, 2, true, None, &[]);
    assert!(t.rows.is_empty());
    assert_eq!((t.total_files, t.total_bytes), (0, 0));
}

#[test]
fn by_type_counts_empty_files() {
    let (_d, root) = make_sized_tree(&[("a.txt", 0), ("b.txt", 0), ("c", 0)]);
    let t = by_type(&root, 2, true, None, &[]);
    assert_eq!(type_rows(&t), [row("-", 1, 0), row(".txt", 2, 0)]);
}

// ── by-type text report ───────────────────────────────────────────────────────

fn table_of(files: &[(&str, u64)]) -> TypeTable {
    let mut acc = TypeAccumulator::default();
    for (name, bytes) in files {
        acc.add(name, *bytes);
    }
    acc.into_table()
}

const MIB: u64 = 1024 * 1024;

#[test]
fn text_report_has_the_documented_layout() {
    let t = table_of(&[
        ("a.mp4", 2 * MIB),
        ("b.mp4", MIB),
        ("c.jpg", MIB),
        ("README", 0),
    ]);
    let expected = [
        "Extension       Files     Size   Share",
        "--------------------------------------",
        ".mp4                2  3.00 MB   75.0%",
        ".jpg                1  1.00 MB   25.0%",
        "(no extension)      1      0 B    0.0%",
        "--------------------------------------",
        "Total               4  4.00 MB  100.0%",
        "",
    ]
    .join("\n");
    assert_eq!(render_by_type_text(&t, 20, false), expected);
}

#[test]
fn text_report_folds_the_tail_into_one_row_so_shares_add_up() {
    let t = table_of(&[
        ("a.aa", 4 * MIB),
        ("b.bb", 2 * MIB),
        ("c.cc", MIB),
        ("d.dd", MIB),
    ]);
    let text = render_by_type_text(&t, 2, false);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 7, "{text}");
    assert!(lines[2].starts_with(".aa"));
    assert!(lines[3].starts_with(".bb"));
    assert!(lines[4].starts_with("(2 other types)"), "{text}");
    assert!(
        lines[4].contains("2.00 MB") && lines[4].ends_with("25.0%"),
        "{text}"
    );
    assert!(lines[6].starts_with("Total") && lines[6].ends_with("100.0%"));
}

#[test]
fn text_report_uses_singular_for_one_other_type() {
    let t = table_of(&[("a.aa", 2 * MIB), ("b.bb", MIB)]);
    let text = render_by_type_text(&t, 1, false);
    assert!(text.contains("(1 other type)"), "{text}");
}

#[test]
fn text_report_columns_line_up() {
    let t = table_of(&[
        ("a.mp4", 123 * MIB),
        ("b.jpg", 7),
        ("c.verylongextensionnamehere", 1000),
        ("README", 3),
    ]);
    let text = render_by_type_text(&t, 20, false);
    let widths: Vec<usize> = text.lines().map(|l| l.chars().count()).collect();
    assert!(
        widths.iter().all(|w| *w == widths[0]),
        "all lines have the same width: {widths:?}\n{text}"
    );
}

#[test]
fn text_report_shortens_very_long_extensions() {
    let long = format!("x.{}", "e".repeat(100));
    let t = table_of(&[(long.as_str(), 10)]);
    let text = render_by_type_text(&t, 20, false);
    assert!(text.contains('…'), "{text}");
    assert!(text.lines().all(|l| l.chars().count() < 80), "{text}");
}

#[test]
fn text_report_summarize_is_one_line() {
    let t = table_of(&[("a.x", 2 * MIB), ("b.y", 2 * MIB), ("c", 0)]);
    assert_eq!(
        render_by_type_text(&t, 20, true),
        "3 files in 3 types, 4.00 MB total\n"
    );
    let one = table_of(&[("a.x", 5)]);
    assert_eq!(
        render_by_type_text(&one, 20, true),
        "1 file in 1 type, 5 B total\n"
    );
}

#[test]
fn text_report_for_no_files_is_header_and_zero_total() {
    let text = render_by_type_text(&TypeTable::default(), 20, false);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    assert!(lines[0].starts_with("Extension"));
    assert!(
        lines[3].starts_with("Total") && lines[3].ends_with("0.0%"),
        "{text}"
    );
}

// ── by-type JSON ──────────────────────────────────────────────────────────────

#[test]
fn json_by_type_document_has_the_documented_shape() {
    let (_d, root) = make_sized_tree(&[("a.mp4", 1)]);
    let t = table_of(&[
        ("a.mp4", 600),
        ("b.MP4", 400),
        ("c.jpg", 500),
        ("README", 7),
    ]);
    let (doc, lossy) = to_json(|w| write_by_type(w, &meta(&root), 20, false, &t));
    assert_eq!(lossy, 0);

    assert_eq!(doc["schema_version"], SCHEMA_VERSION);
    assert_eq!(doc["mode"], "by_type");
    assert_eq!(doc["root"], root.to_str().unwrap());
    assert_eq!(doc["size_mode"], "disk");
    assert_eq!(doc["params"], serde_json::json!({"top": 20}));
    assert_eq!(
        doc["summary"],
        serde_json::json!({"total_bytes": 1507, "files": 4, "types": 3})
    );
    assert_eq!(doc["truncated"], false);
    assert_eq!(
        doc["types"],
        serde_json::json!([
            {"extension": ".mp4", "files": 2, "bytes": 1000},
            {"extension": ".jpg", "files": 1, "bytes": 500},
            {"extension": null,   "files": 1, "bytes": 7},
        ])
    );
    assert!(doc.get("entries").is_none() && doc.get("groups").is_none());
    assert!(
        doc["types"][0].get("share").is_none(),
        "shares are not stored; compute them from bytes"
    );
}

#[test]
fn json_by_type_top_limits_the_list_but_not_the_summary() {
    let (_d, root) = make_sized_tree(&[("a", 1)]);
    let t = table_of(&[("a.x", 30), ("b.y", 20), ("c.z", 10)]);
    let (doc, _) = to_json(|w| write_by_type(w, &meta(&root), 2, false, &t));
    assert_eq!(doc["types"].as_array().unwrap().len(), 2);
    assert_eq!(doc["truncated"], true);
    assert_eq!(doc["summary"]["types"], 3);
    assert_eq!(doc["summary"]["total_bytes"], 60);
    assert_eq!(doc["summary"]["files"], 3);

    let (doc, _) = to_json(|w| write_by_type(w, &meta(&root), 3, false, &t));
    assert_eq!(doc["truncated"], false);
}

#[test]
fn json_by_type_summarize_lists_nothing_but_keeps_the_summary() {
    let (_d, root) = make_sized_tree(&[("a", 1)]);
    let t = table_of(&[("a.x", 30)]);
    let (doc, _) = to_json(|w| write_by_type(w, &meta(&root), 20, true, &t));
    assert_eq!(doc["types"], serde_json::json!([]));
    assert_eq!(doc["truncated"], true);
    assert_eq!(doc["summary"]["total_bytes"], 30);
}

#[test]
fn json_by_type_without_files_is_an_empty_valid_document() {
    let (_d, root) = make_sized_tree(&[("a", 1)]);
    let (doc, _) = to_json(|w| write_by_type(w, &meta(&root), 20, false, &TypeTable::default()));
    assert_eq!(doc["types"], serde_json::json!([]));
    assert_eq!(doc["truncated"], false);
    assert_eq!(
        doc["summary"],
        serde_json::json!({"total_bytes": 0, "files": 0, "types": 0})
    );
}

#[test]
fn json_by_type_reports_apparent_size_mode() {
    let (_d, root) = make_sized_tree(&[("a", 1)]);
    let mut m = meta(&root);
    m.apparent_size = true;
    let (doc, _) = to_json(|w| write_by_type(w, &m, 20, false, &TypeTable::default()));
    assert_eq!(doc["size_mode"], "apparent");
}

// ── --no-hidden ──────────────────────────────────────────────────────────────

/// Scan config with `--no-hidden` and the ignore rules switched as asked.
fn hidden_config(skip_hidden: bool, respect_ignores: bool) -> std::sync::Arc<ardisk::ScanConfig> {
    let ignore_dirs: HashSet<String> = if respect_ignores {
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect()
    } else {
        HashSet::new()
    };
    std::sync::Arc::new(ardisk::ScanConfig {
        ignore_dirs,
        include_pattern: None,
        debug: false,
        apparent_size: true,
        respect_gitignore: respect_ignores,
        exclude: None,
        skip_hidden,
        inodes: false,
    })
}

/// Sizes are distinct powers of two so a total identifies exactly which files
/// were counted.
fn hidden_tree() -> (TempDir, PathBuf) {
    make_sized_tree(&[
        ("src/main.rs", 1),
        ("README.md", 2),
        (".env", 4),
        (".cache/data/huge.bin", 8),
        ("foo/.cache/x.bin", 16),
        ("foo/visible.txt", 32),
        (".git/objects/pack", 64),
        ("node_modules/pkg/i.js", 128),
    ])
}

fn total_of(root: &Path, skip_hidden: bool, respect_ignores: bool, workers: usize) -> u64 {
    let (_raw, content, _) = parallel_scan_collect(
        root.to_path_buf(),
        workers,
        hidden_config(skip_hidden, respect_ignores),
        Collect::Nothing,
    );
    content.values().sum()
}

#[test]
fn hidden_is_included_by_default_like_du() {
    let (_d, root) = hidden_tree();
    // default ignores drop .git and node_modules, hidden stays: 1+2+4+8+16+32
    assert_eq!(total_of(&root, false, true, 2), 63);
}

#[test]
fn no_hidden_skips_dotfiles_and_dot_directories_recursively() {
    let (_d, root) = hidden_tree();
    // only src/main.rs, README.md, foo/visible.txt
    assert_eq!(total_of(&root, true, true, 2), 1 + 2 + 32);
}

#[test]
fn no_hidden_is_independent_of_no_ignore() {
    let (_d, root) = hidden_tree();
    // --no-ignore alone: everything
    assert_eq!(total_of(&root, false, false, 2), 255);
    // both: hidden still skipped, node_modules now visible
    assert_eq!(total_of(&root, true, false, 2), 1 + 2 + 32 + 128);
}

#[test]
fn no_hidden_does_not_enter_hidden_directories() {
    let (_d, root) = hidden_tree();
    let (raw, _content, _) = parallel_scan_collect(
        root.clone(),
        2,
        hidden_config(true, false),
        Collect::Nothing,
    );
    assert!(raw.contains_key(&root.join("foo")));
    assert!(!raw.contains_key(&root.join(".cache")));
    assert!(!raw.contains_key(&root.join(".cache/data")));
    assert!(!raw.contains_key(&root.join("foo/.cache")));
    assert!(!raw.contains_key(&root.join(".git")));
}

#[test]
fn no_hidden_never_skips_the_scan_root_even_if_it_is_hidden() {
    let (_d, root) = hidden_tree();
    let hidden_root = root.join(".cache");
    let (raw, content, _) = parallel_scan_collect(
        hidden_root.clone(),
        2,
        hidden_config(true, true),
        Collect::Nothing,
    );
    // The root is scanned; its non-hidden subdirectory is too.
    assert!(raw.contains_key(&hidden_root));
    assert_eq!(content.values().sum::<u64>(), 8);

    // Non-hidden children of a hidden root are scanned; hidden ones are not.
    let (_d2, r2) =
        make_sized_tree(&[(".top/a.txt", 5), (".top/.b/c.txt", 7), (".top/d/e.txt", 9)]);
    let top = r2.join(".top");
    let (_raw, content, _) =
        parallel_scan_collect(top.clone(), 2, hidden_config(true, true), Collect::Nothing);
    let total: u64 = content.values().sum();
    assert_eq!(total, 5 + 9);
}

#[test]
fn no_hidden_applies_to_every_collection_mode() {
    let (_d, root) = make_sized_tree(&[
        ("a/one.bin", 10),
        ("a/.two.bin", 10),
        (".h/three.bin", 10),
        ("b/four.bin", 10),
    ]);
    let names = |files: &[FileEntry]| -> Vec<String> {
        let mut v: Vec<String> = files
            .iter()
            .map(|f| f.path.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        v.sort();
        v
    };

    let out = ardisk::parallel_scan_report(
        root.clone(),
        2,
        hidden_config(true, true),
        Collect::Largest(10),
    );
    assert_eq!(names(&out.files), ["a/one.bin", "b/four.bin"]);
    assert_eq!(out.file_count, 2);

    let out = ardisk::parallel_scan_report(
        root.clone(),
        2,
        hidden_config(true, true),
        Collect::Duplicates { min_len: 1 },
    );
    assert_eq!(names(&out.files), ["a/one.bin", "b/four.bin"]);

    let out =
        ardisk::parallel_scan_report(root.clone(), 2, hidden_config(true, true), Collect::ByType);
    assert_eq!(out.types.total_files, 2);
    assert_eq!(out.types.total_bytes, 20);

    // and without the flag all four are seen
    let out = ardisk::parallel_scan_report(
        root.clone(),
        2,
        hidden_config(false, true),
        Collect::Largest(10),
    );
    assert_eq!(out.files.len(), 4);
}

#[test]
fn no_hidden_composes_with_ignore_exclude_and_include() {
    let (_d, root) = make_sized_tree(&[
        ("keep/a.log", 1),
        ("keep/b.txt", 2),
        ("keep/.c.txt", 4),
        ("vendor/d.txt", 8),
        ("gen/e.txt", 16),
        (".h/f.txt", 32),
    ]);
    let exclude = build_exclude_matcher(&root, &["gen/**".to_string()]).unwrap();
    let config = std::sync::Arc::new(ardisk::ScanConfig {
        ignore_dirs: ["vendor".to_string()].into_iter().collect(),
        include_pattern: Some(Pattern::new("*.txt").unwrap()),
        debug: false,
        apparent_size: true,
        respect_gitignore: true,
        exclude,
        skip_hidden: true,
        inodes: false,
    });
    let (_raw, content, _) = parallel_scan_collect(root.clone(), 2, config, Collect::Nothing);
    // a.log: --include; .c.txt and .h: hidden; vendor: --ignore; gen/*: --exclude
    assert_eq!(content.get(&root).copied().unwrap_or(0), 0);
    assert_eq!(content.get(&root.join("keep")).copied(), Some(2));
    assert_eq!(content.get(&root.join("vendor")).copied(), None);
}

#[cfg(unix)]
#[test]
fn skipped_hidden_hard_link_does_not_consume_the_inode() {
    let (_d, root) = make_sized_tree(&[("real.bin", 100)]);
    // The hidden name is walked first or second depending on directory order;
    // either way the visible name must be counted exactly once.
    fs::hard_link(root.join("real.bin"), root.join(".alias.bin")).unwrap();
    for workers in [1, 4] {
        let out = ardisk::parallel_scan_report(
            root.clone(),
            workers,
            hidden_config(true, true),
            Collect::Largest(10),
        );
        assert_eq!(out.file_count, 1, "workers={workers}");
        assert_eq!(out.files.len(), 1);
        assert_eq!(out.files[0].path, root.join("real.bin"));
        assert_eq!(out.content_sizes.get(&root).copied(), Some(100));
    }
    // Without the flag the pair still counts once.
    let out = ardisk::parallel_scan_report(
        root.clone(),
        2,
        hidden_config(false, true),
        Collect::Largest(10),
    );
    assert_eq!(out.file_count, 1);
}

#[test]
fn no_hidden_result_does_not_depend_on_worker_count() {
    let (_d, root) = hidden_tree();
    let baseline = total_of(&root, true, false, 1);
    for workers in [2, 3, 8] {
        assert_eq!(total_of(&root, true, false, workers), baseline);
    }
}

#[test]
fn only_a_leading_dot_makes_a_name_hidden() {
    let (_d, root) = make_sized_tree(&[
        ("a.b", 1),
        ("trailing.", 2),
        ("dir.d/x", 4),
        (".lead", 8),
        ("..double", 16),
    ]);
    assert_eq!(total_of(&root, true, true, 2), 1 + 2 + 4);
}

// ── --inodes ─────────────────────────────────────────────────────────────────

fn inode_config(
    respect_ignores: bool,
    skip_hidden: bool,
    include: Option<&str>,
) -> std::sync::Arc<ardisk::ScanConfig> {
    let ignore_dirs: HashSet<String> = if respect_ignores {
        DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect()
    } else {
        HashSet::new()
    };
    std::sync::Arc::new(ardisk::ScanConfig {
        ignore_dirs,
        include_pattern: include.map(|p| Pattern::new(p).unwrap()),
        debug: false,
        apparent_size: false,
        respect_gitignore: respect_ignores,
        exclude: None,
        skip_hidden,
        inodes: true,
    })
}

/// Aggregated per-directory inode counts.
fn inode_counts(
    root: &Path,
    config: std::sync::Arc<ardisk::ScanConfig>,
    workers: usize,
) -> HashMap<PathBuf, u64> {
    let (raw, _content, _) =
        parallel_scan_collect(root.to_path_buf(), workers, config, Collect::Nothing);
    aggregate_sizes(&raw, root)
}

#[test]
fn inodes_count_directories_and_files_including_the_root() {
    // root, a, a/b, c = 4 directories + 4 files, whatever the file sizes are
    let (_d, root) = make_sized_tree(&[("a/1", 10), ("a/2", 0), ("a/b/3", 5_000_000), ("c/4", 1)]);
    let n = inode_counts(&root, inode_config(true, false, None), 2);
    assert_eq!(n[&root], 8);
    assert_eq!(n[&root.join("a")], 5); // a, 1, 2, b, 3
    assert_eq!(n[&root.join("a/b")], 2);
    assert_eq!(n[&root.join("c")], 2);
}

#[test]
fn inodes_count_an_empty_directory_as_one() {
    let (_d, root) = make_sized_tree(&[("f", 1)]);
    fs::create_dir(root.join("empty")).unwrap();
    let n = inode_counts(&root, inode_config(true, false, None), 1);
    assert_eq!(n[&root.join("empty")], 1);
    assert_eq!(n[&root], 3);
}

#[cfg(unix)]
#[test]
fn inodes_count_symlinks_and_hard_links_like_du() {
    let (_d, root) = make_sized_tree(&[("a/real", 10)]);
    std::os::unix::fs::symlink("a", root.join("dir-link")).unwrap();
    std::os::unix::fs::symlink("nowhere", root.join("dangling")).unwrap();
    fs::hard_link(root.join("a/real"), root.join("a/alias")).unwrap();
    for workers in [1, 4] {
        let n = inode_counts(&root, inode_config(true, false, None), workers);
        // root + a + real (alias is the same inode) + 2 symlinks; the
        // symlinked directory is not entered.
        assert_eq!(n[&root], 5, "workers={workers}");
        assert_eq!(n[&root.join("a")], 2);
    }
}

#[cfg(unix)]
#[test]
fn byte_reports_still_skip_symlinks() {
    let (_d, root) = make_sized_tree(&[("a/real", 10)]);
    std::os::unix::fs::symlink("a/real", root.join("link")).unwrap();
    let out = ardisk::parallel_scan_report(
        root.clone(),
        2,
        build_config(HashSet::new(), None, false, true, false),
        Collect::Nothing,
    );
    assert_eq!(out.file_count, 1);
    assert_eq!(out.content_sizes.values().sum::<u64>(), 10);
}

#[test]
fn inodes_honor_ignore_rules_hidden_and_include() {
    let (_d, root) = make_sized_tree(&[
        ("src/a.rs", 1),
        ("src/b.txt", 1),
        (".git/x", 1),
        (".cache/y", 1),
        ("node_modules/z", 1),
    ]);
    // default ignores drop .git and node_modules
    let n = inode_counts(&root, inode_config(true, false, None), 2);
    assert_eq!(n[&root], 1 + 3 + 2); // root, src+2 files, .cache+1 file
    // --no-ignore sees everything
    let n = inode_counts(&root, inode_config(false, false, None), 2);
    assert_eq!(n[&root], 1 + 3 + 2 + 2 + 2);
    // --no-hidden on top of the defaults
    let n = inode_counts(&root, inode_config(true, true, None), 2);
    assert_eq!(n[&root], 1 + 3);
    // --include limits files, directories still cost their own inode
    let n = inode_counts(&root, inode_config(true, false, Some("*.rs")), 2);
    assert_eq!(n[&root], 1 + 2 + 1); // root, src+a.rs, .cache (no match)
}

#[test]
fn inodes_do_not_depend_on_the_worker_count() {
    let (_d, root) = hidden_tree();
    let baseline = inode_counts(&root, inode_config(false, false, None), 1);
    for workers in [2, 8] {
        assert_eq!(
            inode_counts(&root, inode_config(false, false, None), workers),
            baseline
        );
    }
}
