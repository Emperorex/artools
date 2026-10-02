use ardisk::{
    DEFAULT_IGNORES, FileEntry, aggregate_sizes, build_config, build_config_with_exclude,
    build_exclude_matcher, format_size, parallel_scan, parallel_scan_with_files,
};
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
fn run_with_exclude(root: &PathBuf, patterns: &[&str]) -> std::collections::HashMap<PathBuf, u64> {
    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    let patterns: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
    let exclude = build_exclude_matcher(root, &patterns).unwrap();
    let config = build_config_with_exclude(ignore_dirs, None, false, true, true, exclude);
    let (raw, _content) = parallel_scan(root.clone(), 4, config);
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
    root: &PathBuf,
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
    let (_raw, _content, files) = parallel_scan_with_files(root.clone(), workers, config, n);
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
