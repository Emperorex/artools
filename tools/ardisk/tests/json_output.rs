//! End-to-end tests for `--json`: they run the real binary and check what
//! actually reaches stdout and stderr.

use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

fn ardisk(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ardisk"))
        .args(args)
        .output()
        .expect("failed to run ardisk")
}

/// Runs ardisk and requires that **all** of stdout is one JSON document.
fn json(args: &[&str]) -> (Value, String) {
    let out = ardisk(args);
    assert!(
        out.status.success(),
        "ardisk {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout is UTF-8");
    let doc: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not exactly one JSON document ({e}):\n{stdout}"));
    (doc, String::from_utf8_lossy(&out.stderr).into_owned())
}

/// Deterministic content; different seeds differ from the first byte on.
fn data(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A small tree with one duplicate pair, one unique file and an ignored dir.
fn sample() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for (rel, content) in [
        ("photos/a.raw", data(1, 20_000)),
        ("backup/a-copy.raw", data(1, 20_000)),
        ("docs/notes.txt", data(2, 3_000)),
        ("node_modules/pkg/index.js", data(3, 50_000)),
    ] {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn default_mode_prints_one_directories_document() {
    let (_d, root) = sample();
    let (doc, stderr) = json(&[p(&root), "--json", "--apparent-size"]);
    assert_eq!(stderr, "");
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["mode"], "directories");
    assert_eq!(doc["root"], p(&root));
    assert_eq!(doc["size_mode"], "apparent");

    let entries = doc["entries"].as_array().unwrap();
    assert_eq!(entries[0]["path"], ".");
    assert_eq!(entries[0]["depth"], 0);
    assert_eq!(entries[0]["bytes"], doc["summary"]["total_bytes"]);
    // node_modules is ignored by default, so only the three real files count.
    assert_eq!(doc["summary"]["files"], 3);
    assert_eq!(doc["summary"]["directories"], 4); // root + photos, backup, docs
    let paths: Vec<&str> = entries
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"photos") && paths.contains(&"backup") && paths.contains(&"docs"));
    assert!(!paths.iter().any(|p| p.contains("node_modules")));
    for e in entries {
        assert_eq!(e["kind"], "directory");
        assert!(
            !e["path"].as_str().unwrap().starts_with('/'),
            "paths are relative"
        );
    }
}

#[test]
fn largest_files_mode_prints_one_largest_files_document() {
    let (_d, root) = sample();
    let (doc, stderr) = json(&[
        p(&root),
        "--json",
        "--largest-files",
        "2",
        "--apparent-size",
    ]);
    assert_eq!(stderr, "");
    assert_eq!(doc["mode"], "largest_files");
    assert_eq!(doc["params"]["limit"], 2);
    assert_eq!(doc["summary"]["files"], 3);
    assert_eq!(doc["truncated"], true);
    let entries = doc["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["bytes"], 20_000);
    assert_eq!(entries[0]["kind"], "file");
    assert_eq!(
        entries[0]["path"], "backup/a-copy.raw",
        "ties are ordered by path"
    );
    assert_eq!(entries[1]["path"], "photos/a.raw");
}

#[test]
fn duplicates_mode_prints_one_duplicates_document() {
    let (_d, root) = sample();
    let (doc, stderr) = json(&[p(&root), "--json", "--duplicates"]);
    assert_eq!(stderr, "");
    assert_eq!(doc["mode"], "duplicates");
    assert_eq!(doc["size_mode"], "apparent");
    assert_eq!(doc["summary"]["groups"], 1);
    assert_eq!(doc["summary"]["duplicate_files"], 2);
    assert_eq!(doc["summary"]["redundant_files"], 1);
    assert_eq!(doc["summary"]["potentially_reclaimable_bytes"], 20_000);

    let group = &doc["groups"][0];
    assert_eq!(group["bytes_per_file"], 20_000);
    assert_eq!(group["potentially_reclaimable_bytes"], 20_000);
    assert_eq!(
        group["files"],
        serde_json::json!(["backup/a-copy.raw", "photos/a.raw"])
    );
    let expected = format!("blake3:{}", blake3::hash(&data(1, 20_000)).to_hex());
    assert_eq!(group["hash"], expected.as_str());
}

#[test]
fn json_does_not_switch_on_other_modes() {
    let (_d, root) = sample();
    let (doc, _) = json(&[p(&root), "--json"]);
    assert_eq!(doc["mode"], "directories");
    assert!(doc.get("groups").is_none());
}

#[test]
fn stdout_stays_valid_json_when_debug_and_warnings_are_active() {
    let (_d, root) = sample();
    for extra in [vec![], vec!["--largest-files", "3"], vec!["--duplicates"]] {
        let mut args = vec![p(&root), "--json", "--debug"];
        args.extend(extra.iter());
        let (doc, stderr) = json(&args);
        assert!(doc.get("schema_version").is_some());
        assert!(
            stderr.contains("Operational Metrics"),
            "diagnostics belong on stderr for {args:?}, got {stderr:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn unreadable_files_are_warned_about_on_stderr_not_stdout() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, root) = sample();
    let locked = root.join("photos/locked.bin");
    fs::write(&locked, data(1, 20_000)).unwrap(); // same length as the pair
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(&locked).is_ok() {
        return; // running as root: permissions are not enforced
    }
    let (doc, stderr) = json(&[p(&root), "--json", "--duplicates"]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(doc["summary"]["unreadable_files"], 1);
    assert_eq!(doc["summary"]["groups"], 1);
    assert!(
        stderr.contains("could not be read"),
        "stderr was {stderr:?}"
    );
}

#[test]
fn summarize_keeps_the_summary_and_empties_the_list() {
    let (_d, root) = sample();
    let (dirs, _) = json(&[p(&root), "--json", "--summarize"]);
    assert_eq!(dirs["entries"], serde_json::json!([]));
    assert_eq!(dirs["truncated"], true);
    assert!(dirs["summary"]["total_bytes"].as_u64().unwrap() > 0);

    let (dups, _) = json(&[p(&root), "--json", "--duplicates", "--summarize"]);
    assert_eq!(dups["groups"], serde_json::json!([]));
    assert_eq!(dups["summary"]["groups"], 1);
}

#[test]
fn top_and_max_depth_shape_the_directory_list_and_truncated_says_so() {
    let (_d, root) = sample();
    let (doc, _) = json(&[p(&root), "--json", "--top", "2"]);
    assert_eq!(doc["entries"].as_array().unwrap().len(), 2);
    assert_eq!(doc["truncated"], true);
    assert_eq!(doc["params"]["top"], 2);

    let (doc, _) = json(&[p(&root), "--json", "--max-depth", "0"]);
    let entries = doc["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["path"], ".");
    assert_eq!(doc["params"]["max_depth"], 0);
    assert_eq!(doc["truncated"], false);
}

#[test]
fn filters_are_applied_and_echoed() {
    let (_d, root) = sample();
    let (doc, _) = json(&[
        p(&root),
        "--json",
        "--largest-files",
        "10",
        "--exclude",
        "backup/**",
        "--include",
        "*.raw",
    ]);
    assert_eq!(doc["filters"]["include"], "*.raw");
    assert_eq!(doc["filters"]["exclude"], serde_json::json!(["backup/**"]));
    let paths: Vec<&str> = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["photos/a.raw"]);
}

#[test]
fn output_does_not_depend_on_the_worker_count() {
    let (_d, root) = sample();
    for extra in [vec![], vec!["--largest-files", "5"], vec!["--duplicates"]] {
        let mut one = vec![p(&root), "--json", "-j", "1"];
        let mut many = vec![p(&root), "--json", "-j", "8"];
        one.extend(extra.iter());
        many.extend(extra.iter());
        assert_eq!(json(&one).0, json(&many).0, "differs for {extra:?}");
    }
}

#[test]
fn relative_root_argument_is_reported_as_an_absolute_path() {
    let (_d, root) = sample();
    let out = Command::new(env!("CARGO_BIN_EXE_ardisk"))
        .current_dir(&root)
        .args([".", "--json"])
        .output()
        .unwrap();
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["root"], p(&root));
}

#[test]
fn errors_produce_no_json_on_stdout() {
    let (_d, root) = sample();
    // Rejected by the argument parser.
    let out = ardisk(&[p(&root), "--json", "--largest-files", "0"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    // Rejected after parsing.
    let out = ardisk(&[p(&root), "--json", "--exclude", "   "]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let out = ardisk(&[p(&root), "--json", "--include", "[bad"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
}

#[test]
fn without_json_the_output_is_still_text() {
    let (_d, root) = sample();
    let out = ardisk(&[p(&root)]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(serde_json::from_str::<Value>(&stdout).is_err());
    assert!(stdout.contains(p(&root)));
}

#[cfg(unix)]
#[test]
fn non_utf8_file_names_keep_stdout_valid_and_are_flagged_on_stderr() {
    use std::os::unix::ffi::OsStrExt;
    let (_d, root) = sample();
    let bad = root.join(std::ffi::OsStr::from_bytes(b"bad-\xff-name"));
    if fs::write(bad, data(9, 500)).is_err() {
        return; // filesystem refuses non-UTF-8 names
    }
    let (doc, stderr) = json(&[p(&root), "--json", "--largest-files", "10"]);
    let has_replacement = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["path"].as_str().unwrap().contains('\u{FFFD}'));
    assert!(has_replacement);
    assert!(stderr.contains("not valid UTF-8"), "stderr was {stderr:?}");
}

// ── --by-type ─────────────────────────────────────────────────────────────────

/// Extension edge cases: case folding, compound extensions, dotfiles, no
/// extension, a trailing dot and an ignored directory.
fn typed_sample() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for (rel, len) in [
        ("media/clip.mp4", 5000),
        ("media/B.MP4", 1200),
        ("media/a.jpg", 900),
        ("backup.tar.gz", 800),
        ("README", 40),
        (".env", 10),
        ("foo.", 3),
        ("node_modules/pkg/big.bin", 70_000),
    ] {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, data(1, len)).unwrap();
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

#[test]
fn by_type_json_prints_one_by_type_document() {
    let (_d, root) = typed_sample();
    let (doc, stderr) = json(&[p(&root), "--by-type", "--json", "--apparent-size"]);
    assert_eq!(stderr, "");
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["mode"], "by_type");
    assert_eq!(doc["root"], p(&root));
    assert_eq!(doc["size_mode"], "apparent");
    assert_eq!(
        doc["types"],
        serde_json::json!([
            {"extension": ".mp4", "files": 2, "bytes": 6200},
            {"extension": ".jpg", "files": 1, "bytes": 900},
            {"extension": ".gz",  "files": 1, "bytes": 800},
            {"extension": null,   "files": 3, "bytes": 53},
        ])
    );
    assert_eq!(
        doc["summary"],
        serde_json::json!({"total_bytes": 7953, "files": 7, "types": 4})
    );
    assert_eq!(doc["truncated"], false);
    assert!(doc.get("entries").is_none());
}

#[test]
fn by_type_json_top_truncates_and_summarize_empties_the_list() {
    let (_d, root) = typed_sample();
    let (doc, _) = json(&[p(&root), "--by-type", "--json", "--top", "2"]);
    assert_eq!(doc["types"].as_array().unwrap().len(), 2);
    assert_eq!(doc["truncated"], true);
    assert_eq!(doc["summary"]["types"], 4);

    let (doc, _) = json(&[p(&root), "--by-type", "--json", "--summarize"]);
    assert_eq!(doc["types"], serde_json::json!([]));
    assert_eq!(doc["summary"]["files"], 7);
}

#[test]
fn by_type_default_sizing_is_disk_blocks() {
    let (_d, root) = typed_sample();
    let (doc, _) = json(&[p(&root), "--by-type", "--json"]);
    assert_eq!(doc["size_mode"], "disk");
}

#[test]
fn by_type_applies_filters_and_echoes_them() {
    let (_d, root) = typed_sample();
    let (doc, _) = json(&[
        p(&root),
        "--by-type",
        "--json",
        "--apparent-size",
        "--exclude",
        "media/**",
        "--exclude",
        "*.gz",
    ]);
    assert_eq!(
        doc["filters"]["exclude"],
        serde_json::json!(["media/**", "*.gz"])
    );
    assert_eq!(
        doc["types"],
        serde_json::json!([{"extension": null, "files": 3, "bytes": 53}])
    );

    let (doc, _) = json(&[
        p(&root),
        "--by-type",
        "--json",
        "--apparent-size",
        "--include",
        "*.mp4",
    ]);
    assert_eq!(doc["filters"]["include"], "*.mp4");
    assert_eq!(
        doc["types"],
        serde_json::json!([{"extension": ".mp4", "files": 1, "bytes": 5000}])
    );
}

#[test]
fn by_type_text_output_is_a_table_with_a_total() {
    let (_d, root) = typed_sample();
    let out = ardisk(&[p(&root), "--by-type", "--apparent-size"]);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();

    assert!(
        lines[0].starts_with("Extension") && lines[0].ends_with("Share"),
        "{stdout}"
    );
    assert!(lines[1].chars().all(|c| c == '-'));
    assert!(
        lines[2].starts_with(".mp4") && lines[2].contains("2"),
        "{stdout}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("(no extension)")),
        "{stdout}"
    );
    let total = lines.last().unwrap();
    assert!(
        total.starts_with("Total") && total.ends_with("100.0%"),
        "{stdout}"
    );
    assert!(!stdout.contains("node_modules"));
}

#[test]
fn by_type_summarize_text_is_one_line() {
    let (_d, root) = typed_sample();
    let out = ardisk(&[p(&root), "--by-type", "--summarize", "--apparent-size"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout, "7 files in 4 types, 7.77 KB total\n");
}

#[test]
fn by_type_stdout_stays_valid_json_with_debug() {
    let (_d, root) = typed_sample();
    let (doc, stderr) = json(&[p(&root), "--by-type", "--json", "--debug"]);
    assert_eq!(doc["mode"], "by_type");
    assert!(
        stderr.contains("Operational Metrics"),
        "stderr was {stderr:?}"
    );
}

#[test]
fn by_type_output_does_not_depend_on_the_worker_count() {
    let (_d, root) = typed_sample();
    let one = json(&[p(&root), "--by-type", "--json", "-j", "1"]).0;
    let many = json(&[p(&root), "--by-type", "--json", "-j", "8"]).0;
    assert_eq!(one, many);
}

#[test]
fn by_type_conflicts_are_usage_errors_without_output() {
    let (_d, root) = typed_sample();
    for extra in [
        vec!["--duplicates"],
        vec!["--largest-files", "3"],
        vec!["--max-depth", "1"],
        vec!["--threshold", "1KB"],
    ] {
        let mut args = vec![p(&root), "--by-type", "--json"];
        args.extend(extra.iter());
        let out = ardisk(&args);
        assert_eq!(out.status.code(), Some(2), "{extra:?}");
        assert!(out.stdout.is_empty(), "{extra:?}");
    }
}

#[test]
fn by_type_of_an_empty_directory_is_an_empty_document() {
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let (doc, _) = json(&[p(&root), "--by-type", "--json"]);
    assert_eq!(doc["types"], serde_json::json!([]));
    assert_eq!(
        doc["summary"],
        serde_json::json!({"total_bytes": 0, "files": 0, "types": 0})
    );
}

/// Visible and hidden files with distinct sizes.
fn hidden_sample() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for (rel, len) in [
        ("src/main.rs", 1_000),
        (".env", 2_000),
        (".cache/data/huge.bin", 40_000),
        ("foo/.cache/x.bin", 8_000),
        ("foo/visible.txt", 500),
    ] {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, data(5, len)).unwrap();
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

#[test]
fn hidden_entries_are_counted_by_default_and_skipped_with_no_hidden() {
    let (_d, root) = hidden_sample();
    let (all, _) = json(&[p(&root), "--json", "--apparent-size", "--summarize"]);
    assert_eq!(all["filters"]["no_hidden"], false);
    assert_eq!(all["summary"]["files"], 5);

    let (vis, _) = json(&[
        p(&root),
        "--json",
        "--apparent-size",
        "--summarize",
        "--no-hidden",
    ]);
    assert_eq!(vis["filters"]["no_hidden"], true);
    assert_eq!(vis["summary"]["files"], 2);
}

#[test]
fn no_hidden_works_in_every_mode() {
    let (_d, root) = hidden_sample();

    let (doc, _) = json(&[
        p(&root),
        "--json",
        "--apparent-size",
        "--largest-files",
        "10",
        "--no-hidden",
    ]);
    let paths: Vec<&str> = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["src/main.rs", "foo/visible.txt"]);
    assert_eq!(doc["filters"]["no_hidden"], true);

    let (doc, _) = json(&[
        p(&root),
        "--json",
        "--apparent-size",
        "--by-type",
        "--no-hidden",
    ]);
    assert_eq!(doc["summary"]["files"], 2);
    assert_eq!(doc["summary"]["total_bytes"], 1_500);

    let (doc, _) = json(&[p(&root), "--json", "--duplicates", "--no-hidden"]);
    assert_eq!(doc["mode"], "duplicates");
    assert_eq!(doc["filters"]["no_hidden"], true);
    assert_eq!(doc["summary"]["groups"], 0);

    let (doc, _) = json(&[p(&root), "--json", "--no-hidden"]);
    let paths: Vec<&str> = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(
        paths
            .iter()
            .all(|p| !p.split('/').any(|c| c.starts_with('.') && c != "."))
    );
    assert!(paths.contains(&"foo"));
}

#[test]
fn hidden_duplicates_are_found_by_default_and_hidden_by_no_hidden() {
    let dir = TempDir::new().unwrap();
    for rel in ["a/x.bin", ".h/y.bin"] {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, data(7, 9_000)).unwrap();
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    let (doc, _) = json(&[p(&root), "--json", "--duplicates"]);
    assert_eq!(doc["summary"]["groups"], 1);
    let (doc, _) = json(&[p(&root), "--json", "--duplicates", "--no-hidden"]);
    assert_eq!(doc["summary"]["groups"], 0);
}

#[test]
fn no_hidden_and_no_ignore_are_independent_flags() {
    let dir = TempDir::new().unwrap();
    for (rel, len) in [
        (".git/pack", 100),
        ("node_modules/i.js", 200),
        ("src/a.rs", 400),
        (".env", 800),
    ] {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, data(1, len)).unwrap();
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    let total = |extra: &[&str]| -> u64 {
        let mut args = vec![p(&root), "--json", "--by-type", "--apparent-size"];
        args.extend(extra);
        json(&args).0["summary"]["total_bytes"].as_u64().unwrap()
    };
    assert_eq!(total(&[]), 400 + 800);
    assert_eq!(total(&["--no-hidden"]), 400);
    assert_eq!(total(&["--no-ignore"]), 100 + 200 + 400 + 800);
    assert_eq!(total(&["--no-hidden", "--no-ignore"]), 200 + 400);
}

#[test]
fn a_hidden_scan_root_is_still_scanned_with_no_hidden() {
    let (_d, root) = hidden_sample();
    let hidden_root = root.join(".cache");
    let (doc, _) = json(&[
        p(&hidden_root),
        "--json",
        "--apparent-size",
        "--by-type",
        "--no-hidden",
    ]);
    assert_eq!(doc["summary"]["files"], 1);
    assert_eq!(doc["summary"]["total_bytes"], 40_000);
}

#[test]
fn no_hidden_output_does_not_depend_on_the_worker_count() {
    let (_d, root) = hidden_sample();
    for extra in [vec![], vec!["--largest-files", "5"], vec!["--by-type"]] {
        let mut one = vec![p(&root), "--json", "--no-hidden", "-j", "1"];
        let mut many = vec![p(&root), "--json", "--no-hidden", "-j", "8"];
        one.extend(extra.iter());
        many.extend(extra.iter());
        assert_eq!(json(&one).0, json(&many).0, "differs for {extra:?}");
    }
}

fn inode_sample() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for rel in ["a/1", "a/2", "a/b/3", "c/4", ".h/5", "node_modules/6"] {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, data(1, 10)).unwrap();
    }
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

#[test]
fn inodes_mode_prints_one_inodes_document() {
    let (_d, root) = inode_sample();
    let (doc, stderr) = json(&[p(&root), "--json", "--inodes"]);
    assert_eq!(stderr, "");
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["mode"], "inodes");
    assert!(doc.get("size_mode").is_none(), "no sizes involved");
    // root, a, a/b, c, .h + 5 files (node_modules is ignored)
    assert_eq!(doc["summary"]["total_inodes"], 10);
    assert_eq!(doc["summary"]["files"], 5);
    assert_eq!(doc["summary"]["directories"], 5);
    let entries = doc["entries"].as_array().unwrap();
    assert_eq!(entries[0]["path"], ".");
    assert_eq!(entries[0]["inodes"], 10);
    assert_eq!(entries[1]["path"], "a");
    assert_eq!(entries[1]["inodes"], 5);
    assert!(entries[0].get("bytes").is_none());
    assert_eq!(doc["params"]["threshold_inodes"], serde_json::Value::Null);
}

#[test]
fn inodes_text_output_is_plain_integers() {
    let (_d, root) = inode_sample();
    let out = ardisk(&[p(&root), "--inodes", "--no-ignore", "-s"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.trim(), format!("12  {}", p(&root)));
}

#[test]
fn inodes_threshold_top_depth_and_hidden() {
    let (_d, root) = inode_sample();
    let (doc, _) = json(&[p(&root), "--json", "--inodes", "--threshold", "5"]);
    let paths: Vec<&str> = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, [".", "a"]);
    assert_eq!(doc["params"]["threshold_inodes"], 5);

    let (doc, _) = json(&[
        p(&root),
        "--json",
        "--inodes",
        "--max-depth",
        "0",
        "--no-hidden",
    ]);
    assert_eq!(doc["entries"].as_array().unwrap().len(), 1);
    assert_eq!(doc["summary"]["total_inodes"], 8);
    assert_eq!(doc["filters"]["no_hidden"], true);
}

#[test]
fn inodes_rejects_sizes_and_incompatible_modes() {
    let (_d, root) = inode_sample();
    for extra in [
        vec!["--by-type"],
        vec!["--duplicates"],
        vec!["--largest-files", "3"],
        vec!["--apparent-size"],
        vec!["--threshold", "1MB"],
        vec!["--threshold", "-1"],
    ] {
        let mut args = vec![p(&root), "--json", "--inodes"];
        args.extend(extra.iter());
        let out = ardisk(&args);
        assert!(!out.status.success(), "{args:?} should be rejected");
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn inodes_output_does_not_depend_on_the_worker_count() {
    let (_d, root) = inode_sample();
    let one = json(&[p(&root), "--json", "--inodes", "--no-ignore", "-j", "1"]).0;
    let many = json(&[p(&root), "--json", "--inodes", "--no-ignore", "-j", "8"]).0;
    assert_eq!(one, many);
}
