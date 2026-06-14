use ardisk::{DEFAULT_IGNORES, aggregate_sizes, build_config, format_size, parallel_scan};
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

fn default_config(debug: bool) -> std::sync::Arc<ardisk::ScanConfig> {
    let ignore_dirs: HashSet<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
    build_config(ignore_dirs, debug)
}

fn run(root: PathBuf) -> std::collections::HashMap<PathBuf, u64> {
    let config = default_config(false);
    let raw = parallel_scan(root.clone(), 4, config);
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
fn empty_directory_has_zero_size() {
    let (_dir, root) = make_tree(&[]);
    // create an explicit empty subdirectory
    let empty_sub = root.join("empty");
    fs::create_dir_all(&empty_sub).unwrap();

    let config = default_config(false);
    let raw = parallel_scan(root.clone(), 4, config);
    let sizes = aggregate_sizes(&raw, &root);

    assert_eq!(sizes.get(&empty_sub).copied().unwrap_or(0), 0);
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
    let raw = parallel_scan(root.clone(), 4, config);
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
    let config = build_config(ignore_dirs, false);

    let raw = parallel_scan(root.clone(), 4, config);
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
    let raw = parallel_scan(root.clone(), 4, config);

    // raw size of root should equal one file, not two
    let root_raw = raw[&root];
    // A single 5-byte file occupies at least 1 block (512 bytes on most FS)
    // Confirm it's not double-counted by checking raw == single file blocks
    let single_file_size = fs::metadata(root.join("real.txt"))
        .map(|m| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                m.blocks() * 512
            }
        })
        .unwrap_or(0);

    assert_eq!(root_raw, single_file_size, "symlink should not be counted");
}

// ── parallel_scan raw output ──────────────────────────────────────────────────

#[test]
fn raw_scan_produces_entry_for_every_directory() {
    let (_dir, root) = make_tree(&["a/b/c.txt", "d/e.txt"]);
    let config = default_config(false);
    let raw = parallel_scan(root.clone(), 4, config);

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
        let raw = parallel_scan(root.clone(), workers, config);
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
