//! The scan lists directories parents-first with a parent position each,
//! and `roll_up` totals every subtree in one backwards pass over that
//! list. Checked against `aggregate_sizes`, the map-based rollup.

use ardisk::{
    Collect, DirNode, ScanOutput, aggregate_sizes, build_config, parallel_scan_report, roll_up,
};
use glob::Pattern;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

/// A tree a few levels deep and wide enough that several workers are busy
/// at once: 4 x 4 x 4 directories, files of different sizes and extensions
/// at every level. Returns the handle and the canonical root.
fn tree() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let mut n = 0usize;
    for a in 0..4 {
        for b in 0..4 {
            for c in 0..4 {
                let leaf = dir.path().join(format!("a{a}/b{b}/c{c}"));
                fs::create_dir_all(&leaf).unwrap();
                for (level, at) in [
                    leaf.as_path(),
                    leaf.parent().unwrap(),
                    leaf.parent().unwrap().parent().unwrap(),
                ]
                .into_iter()
                .enumerate()
                {
                    n += 1;
                    let ext = if n.is_multiple_of(3) { "rs" } else { "txt" };
                    fs::write(
                        at.join(format!("f{a}{b}{c}_{level}.{ext}")),
                        vec![b'x'; n * 7],
                    )
                    .unwrap();
                }
            }
        }
    }
    fs::create_dir_all(dir.path().join("empty/deeper")).unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

fn scan(root: &Path, workers: usize, include: Option<&str>) -> ScanOutput {
    let config = build_config(
        HashSet::new(),
        include.map(|p| Pattern::new(p).unwrap()),
        false,
        true,
        false,
    );
    parallel_scan_report(root.to_path_buf(), workers, config, Collect::Nothing)
}

#[test]
fn the_scan_lists_every_parent_before_its_children() {
    let (_dir, root) = tree();
    for workers in [1, 2, 8] {
        let out = scan(&root, workers, None);
        let dirs = &out.dirs;
        // root + 4 + 16 + 64 + empty + empty/deeper
        assert_eq!(dirs.len(), 1 + 4 + 16 + 64 + 2, "workers={workers}");
        assert_eq!(dirs[0].path, root);
        assert_eq!(dirs[0].parent, None);
        for (i, dir) in dirs.iter().enumerate().skip(1) {
            let parent = dir.parent.expect("only the root has no parent");
            assert!(
                parent < i,
                "{} is listed before its parent",
                dir.path.display()
            );
            assert_eq!(Some(dirs[parent].path.as_path()), dir.path.parent());
        }
        let unique: HashSet<&PathBuf> = dirs.iter().map(|d| &d.path).collect();
        assert_eq!(unique.len(), dirs.len(), "each directory is listed once");
    }
}

#[test]
fn roll_up_agrees_with_the_map_rollup() {
    let (_dir, root) = tree();
    for workers in [1, 2, 8] {
        for include in [None, Some("*.rs")] {
            let out = scan(&root, workers, include);
            let expected_sizes = aggregate_sizes(&out.raw_sizes(), &root);
            let expected_content = aggregate_sizes(&out.content_sizes(), &root);

            let mut dirs = out.dirs;
            roll_up(&mut dirs);
            let sizes: HashMap<PathBuf, u64> =
                dirs.iter().map(|d| (d.path.clone(), d.size)).collect();
            let content: HashMap<PathBuf, u64> =
                dirs.iter().map(|d| (d.path.clone(), d.content)).collect();
            assert_eq!(
                sizes, expected_sizes,
                "workers={workers} include={include:?}"
            );
            assert_eq!(
                content, expected_content,
                "workers={workers} include={include:?}"
            );
            // The root holds everything.
            assert_eq!(dirs[0].content, content.values().copied().max().unwrap());
        }
    }
}

#[test]
fn roll_up_adds_each_directory_into_all_its_ancestors() {
    let node = |path: &str, parent, size, content| DirNode {
        path: PathBuf::from(path),
        parent,
        size,
        content,
    };
    // /r ── a ── b ── c
    //    └─ d
    let mut dirs = vec![
        node("/r", None, 1, 0),
        node("/r/a", Some(0), 10, 5),
        node("/r/d", Some(0), 100, 50),
        node("/r/a/b", Some(1), 1_000, 500),
        node("/r/a/b/c", Some(3), 10_000, 5_000),
    ];
    roll_up(&mut dirs);
    let totals: Vec<(u64, u64)> = dirs.iter().map(|d| (d.size, d.content)).collect();
    assert_eq!(
        totals,
        [
            (11_111, 5_555),
            (11_010, 5_505),
            (100, 50),
            (11_000, 5_500),
            (10_000, 5_000),
        ]
    );

    // Nothing to do for an empty scan (unreadable root) or a lone root.
    roll_up(&mut []);
    let mut lone = vec![node("/r", None, 7, 3)];
    roll_up(&mut lone);
    assert_eq!((lone[0].size, lone[0].content), (7, 3));
}
