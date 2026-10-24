//! End-to-end tests for `--diff` and `--top 0`: real scans saved as
//! snapshots, changed on disk, then compared by the real binary.

use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

fn ardisk(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ardisk"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run ardisk")
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "ardisk failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// Saves `ardisk ROOT --json --apparent-size EXTRA...` to `file`.
fn snapshot(root: &Path, file: &Path, extra: &[&str]) {
    let mut args = vec![s(root), "--json", "--apparent-size"];
    args.extend_from_slice(extra);
    let out = ardisk(&args);
    fs::write(file, ok(&out)).unwrap();
}

fn write(path: PathBuf, len: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; len]).unwrap();
}

/// data/{logs,tmp/old,same} before; logs grows, cache appears, tmp/old goes.
struct Scenario {
    _dir: TempDir,
    root: PathBuf,
    before: PathBuf,
    after: PathBuf,
}

fn scenario(snapshot_args: &[&str]) -> Scenario {
    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap().join("data");
    write(root.join("logs/a.log"), 300_000);
    write(root.join("tmp/old/x"), 50_000);
    write(root.join("same/s"), 20_000);
    let before = dir.path().join("before.json");
    snapshot(&root, &before, snapshot_args);

    write(root.join("logs/b.log"), 900_000);
    write(root.join("cache/c"), 120_000);
    fs::remove_dir_all(root.join("tmp/old")).unwrap();
    let after = dir.path().join("after.json");
    snapshot(&root, &after, snapshot_args);
    Scenario {
        _dir: dir,
        root,
        before,
        after,
    }
}

fn diff_json(sc: &Scenario, extra: &[&str]) -> Value {
    let mut args = vec!["--diff", s(&sc.before), s(&sc.after), "--json"];
    args.extend_from_slice(extra);
    serde_json::from_str(&ok(&ardisk(&args))).unwrap()
}

fn changes(doc: &Value) -> Vec<(String, String)> {
    doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["path"].as_str().unwrap().to_string(),
                e["change"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

// ── --top 0 ─────────────────────────────────────────────────────────────

#[test]
fn top_0_lists_every_directory() {
    let dir = TempDir::new().unwrap();
    for i in 0..25 {
        write(dir.path().join(format!("d{i:02}/f")), 10);
    }
    let out = ardisk(&[s(dir.path()), "--json", "--top", "0"]);
    let doc: Value = serde_json::from_str(&ok(&out)).unwrap();
    assert_eq!(doc["params"]["top"], 0);
    assert_eq!(doc["truncated"], false);
    // 25 subdirectories plus the root, all listed.
    assert_eq!(doc["entries"].as_array().unwrap().len(), 26);
    assert_eq!(doc["summary"]["directories"], 26);

    let text = ok(&ardisk(&[s(dir.path()), "--top", "0"]));
    assert_eq!(text.lines().count(), 26);
    // The default is still 20.
    let text = ok(&ardisk(&[s(dir.path())]));
    assert_eq!(text.lines().count(), 20);
}

#[test]
fn top_0_lists_every_file_type() {
    let dir = TempDir::new().unwrap();
    for i in 0..25 {
        write(dir.path().join(format!("f.e{i}")), 10);
    }
    let doc: Value = serde_json::from_str(&ok(&ardisk(&[
        s(dir.path()),
        "--by-type",
        "--json",
        "--top",
        "0",
    ])))
    .unwrap();
    assert_eq!(doc["types"].as_array().unwrap().len(), 25);
    assert_eq!(doc["truncated"], false);
}

// ── --diff ──────────────────────────────────────────────────────────────

#[test]
fn reports_grew_new_gone_and_shrank() {
    let sc = scenario(&["--top", "0"]);
    let doc = diff_json(&sc, &["--top", "0"]);
    assert_eq!(doc["mode"], "diff");
    assert_eq!(doc["unit"], "bytes");
    assert_eq!(doc["size_mode"], "apparent");
    assert_eq!(doc["root"], s(&sc.root));
    assert_eq!(doc["before"]["complete"], true);
    assert_eq!(doc["after"]["complete"], true);

    let got = changes(&doc);
    let find = |p: &str| {
        got.iter()
            .find(|(path, _)| path == p)
            .map(|(_, c)| c.as_str())
    };
    assert_eq!(find("logs"), Some("grew"));
    assert_eq!(find("cache"), Some("new"));
    assert_eq!(find("tmp/old"), Some("gone"));
    assert_eq!(find("tmp"), Some("shrank"));
    assert_eq!(find("same"), None, "unchanged directories are not listed");
    assert_eq!(find("."), None, "the root is the total, not a row");
    assert_eq!(doc["summary"]["unchanged"], 1);
    assert_eq!(doc["summary"]["not_comparable"], 0);

    // Biggest change first; delta is after - before.
    assert_eq!(got[0].0, "logs");
    for e in doc["entries"].as_array().unwrap() {
        let before = e["before"].as_i64().unwrap_or(0);
        let after = e["after"].as_i64().unwrap_or(0);
        assert_eq!(e["delta"].as_i64().unwrap(), after - before, "{e}");
    }
    let logs = &doc["entries"][0];
    assert!(logs["delta"].as_i64().unwrap() >= 900_000, "{logs}");
    assert_eq!(
        doc["summary"]["total_delta"].as_i64().unwrap(),
        doc["after"]["total"].as_i64().unwrap() - doc["before"]["total"].as_i64().unwrap()
    );
}

#[test]
fn text_report() {
    let sc = scenario(&["--top", "0"]);
    let text = ok(&ardisk(&["--diff", s(&sc.before), s(&sc.after)]));
    let lines: Vec<&str> = text.lines().collect();
    let logs = format!("  {}/logs  (", s(&sc.root));
    assert!(
        lines[0].starts_with('+') && lines[0].contains(&logs),
        "{text}"
    );
    assert!(lines[0].contains(" → "), "{text}");
    let cache = format!("  {}/cache  (new)", s(&sc.root));
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with('+') && l.ends_with(&cache)),
        "{text}"
    );
    let old = format!("  {}/tmp/old  (gone)", s(&sc.root));
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with('-') && l.ends_with(&old)),
        "{text}"
    );
    assert!(text.contains("\n= unchanged: 1 directory\n"), "{text}");
    assert!(lines.last().unwrap().starts_with("  total  "), "{text}");
}

#[test]
fn top_and_threshold_limit_the_rows() {
    let sc = scenario(&["--top", "0"]);
    let doc = diff_json(&sc, &["--top", "1"]);
    assert_eq!(changes(&doc).len(), 1);
    assert_eq!(doc["truncated"], true);

    // Only logs (+~900 KB) and cache (+~120 KB) move by 100 KB or more.
    let doc = diff_json(&sc, &["--threshold", "100KB"]);
    let paths: Vec<String> = changes(&doc).into_iter().map(|(p, _)| p).collect();
    assert_eq!(paths, ["logs", "cache"]);
    assert_eq!(doc["summary"]["below_threshold"], 2);
    assert_eq!(doc["params"]["threshold"], 102_400);
    let si = diff_json(&sc, &["--threshold", "100KB", "--si"]);
    assert_eq!(si["params"]["threshold"], 100_000);

    let text = ok(&ardisk(&[
        "--diff",
        s(&sc.before),
        s(&sc.after),
        "--top",
        "1",
    ]));
    assert!(
        text.contains("3 more changes not shown; raise --top, or --top 0 for all"),
        "{text}"
    );
}

#[test]
fn partial_snapshot_is_not_guessed() {
    // Default --top 20 is plenty here, but --top 1 cuts both snapshots.
    let sc = scenario(&["--top", "1"]);
    let out = ardisk(&["--diff", s(&sc.before), s(&sc.after), "--json"]);
    let doc: Value = serde_json::from_str(&ok(&out)).unwrap();
    assert_eq!(doc["before"]["complete"], false);
    // Each lists only the root, so no directory can be compared or called
    // new/gone; the totals still are.
    assert!(changes(&doc).is_empty());
    assert!(doc["summary"]["total_delta"].as_i64().unwrap() > 0);
    let err = stderr(&out);
    assert!(err.contains("lists only its biggest directory"), "{err}");
    assert!(err.contains("--top 0"), "{err}");
}

#[test]
fn reads_one_snapshot_from_stdin() {
    let sc = scenario(&["--top", "0"]);
    let after = fs::read(&sc.after).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ardisk"))
        .args(["--diff", s(&sc.before), "-", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(&after).unwrap();
    let out = child.wait_with_output().unwrap();
    let doc: Value = serde_json::from_str(&ok(&out)).unwrap();
    assert_eq!(doc["after"]["file"], "-");
    assert!(!changes(&doc).is_empty());
}

#[test]
fn inode_snapshots() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("data");
    write(root.join("a/f1"), 1);
    let before = dir.path().join("b.json");
    let out = ardisk(&[s(&root), "--inodes", "--json", "--top", "0"]);
    fs::write(&before, ok(&out)).unwrap();
    for i in 2..=6 {
        write(root.join(format!("a/f{i}")), 1);
    }
    let after = dir.path().join("a.json");
    let out = ardisk(&[s(&root), "--inodes", "--json", "--top", "0"]);
    fs::write(&after, ok(&out)).unwrap();

    let doc: Value =
        serde_json::from_str(&ok(&ardisk(&["--diff", s(&before), s(&after), "--json"]))).unwrap();
    assert_eq!(doc["unit"], "inodes");
    assert!(doc.get("size_mode").is_none());
    assert_eq!(doc["entries"][0]["path"], "a");
    assert_eq!(doc["entries"][0]["delta"], 5);
    let text = ok(&ardisk(&["--diff", s(&before), s(&after)]));
    assert!(text.contains("(2 → 7)"), "{text}");
    assert!(text.contains("(+5 inodes)"), "{text}");
    // --threshold is a count here, and --si makes no sense.
    assert!(
        ok(&ardisk(&[
            "--diff",
            s(&before),
            s(&after),
            "--threshold",
            "5"
        ]))
        .contains("+")
    );
    let bad = ardisk(&["--diff", s(&before), s(&after), "--threshold", "1MB"]);
    assert!(stderr(&bad).contains("count inodes"), "{}", stderr(&bad));
    let si = ardisk(&["--diff", s(&before), s(&after), "--si"]);
    assert_eq!(si.status.code(), Some(1));
}

// ── refusals ────────────────────────────────────────────────────────────

fn refused(args: &[&str], code: i32, needle: &str) {
    let out = ardisk(args);
    assert_eq!(out.status.code(), Some(code), "{args:?}: {}", stderr(&out));
    assert!(out.stdout.is_empty(), "{args:?}");
    let err = stderr(&out);
    assert!(err.contains(needle), "{args:?}: {err}");
}

#[test]
fn refuses_snapshots_that_do_not_compare() {
    let sc = scenario(&["--top", "0"]);
    let dir = sc.before.parent().unwrap();
    let (b, a) = (s(&sc.before), s(&sc.after));

    let other = dir.join("other.json");
    snapshot(dir, &other, &["--top", "0"]);
    refused(&["--diff", b, s(&other)], 1, "different directories");

    let disk = dir.join("disk.json");
    fs::write(&disk, ok(&ardisk(&[s(&sc.root), "--json", "--top", "0"]))).unwrap();
    refused(&["--diff", b, s(&disk)], 1, "--apparent-size");

    let filtered = dir.join("filtered.json");
    snapshot(&sc.root, &filtered, &["--top", "0", "--exclude", "*.log"]);
    refused(
        &["--diff", b, s(&filtered)],
        1,
        "different filters (exclude:",
    );

    let largest = dir.join("largest.json");
    fs::write(
        &largest,
        ok(&ardisk(&[s(&sc.root), "--json", "--largest-files", "3"])),
    )
    .unwrap();
    refused(&["--diff", b, s(&largest)], 1, "'largest_files' report");

    let garbage = dir.join("garbage.json");
    fs::write(&garbage, "not json").unwrap();
    refused(
        &["--diff", b, s(&garbage)],
        1,
        "not an ardisk --json document",
    );
    refused(&["--diff", b, "/no/such/file.json"], 1, "cannot read");
    refused(&["--diff", "-", "-"], 1, "only one of the two snapshots");
    refused(&["--diff", b, a, "--threshold", "lots"], 1, "lots");
}

#[test]
fn diff_is_a_mode_of_its_own() {
    let sc = scenario(&["--top", "0"]);
    let (b, a) = (s(&sc.before), s(&sc.after));
    for extra in [
        &["."][..],
        &["--tree"],
        &["--inodes"],
        &["--exclude", "x"],
        &["--apparent-size"],
        &["--max-depth", "1"],
        &["-j", "2"],
        &["--progress"],
    ] {
        let mut args = vec!["--diff", b, a];
        args.extend_from_slice(extra);
        refused(&args, 2, "cannot be used with");
    }
    refused(&["--diff", b], 2, "BEFORE");
}

// ── --summarize snapshots (review regression) ───────────────────────────

/// A summary-only snapshot lists no directories. It must never be taken
/// as a complete listing, or every directory of the other snapshot would
/// be reported as new or gone.
#[test]
fn summary_only_snapshots_report_no_new_or_gone() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("data");
    write(root.join("a/f"), 1000);
    write(root.join("b/g"), 2000);
    let s1 = dir.path().join("s1.json");
    snapshot(&root, &s1, &["--summarize"]);
    let doc: Value = serde_json::from_str(&fs::read_to_string(&s1).unwrap()).unwrap();
    assert_eq!(doc["params"]["summarize"], true);

    write(root.join("c/h"), 3000);
    fs::remove_dir_all(root.join("a")).unwrap();
    let s2 = dir.path().join("s2.json");
    snapshot(&root, &s2, &["--summarize"]);
    let full = dir.path().join("full.json");
    snapshot(&root, &full, &["--top", "0"]);

    for (b, a) in [(&s1, &s2), (&s1, &full), (&full, &s2)] {
        let out = ardisk(&["--diff", s(b), s(a), "--json"]);
        let doc: Value = serde_json::from_str(&ok(&out)).unwrap();
        assert!(changes(&doc).is_empty(), "{b:?} vs {a:?}: {doc}");
        assert_eq!(doc["summary"]["new"], 0);
        assert_eq!(doc["summary"]["gone"], 0);
        // The totals are still compared (s1 is from before the change).
        if b == &s1 {
            assert_ne!(doc["summary"]["total_delta"], 0);
        }
        let err = stderr(&out);
        assert!(
            err.contains("lists no directories (taken with --summarize)"),
            "{err}"
        );
    }
}

/// With --summarize and nothing matching --include, no directory qualifies
/// and the list is not even marked truncated; params.summarize still makes
/// the snapshot partial.
#[test]
fn summary_with_nothing_listed_is_still_partial() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("data");
    write(root.join("a/f.txt"), 1000);
    let before = dir.path().join("b.json");
    snapshot(&root, &before, &["--summarize", "--include", "*.log"]);
    let doc: Value = serde_json::from_str(&fs::read_to_string(&before).unwrap()).unwrap();
    assert_eq!(doc["truncated"], false);
    assert_eq!(doc["entries"].as_array().unwrap().len(), 0);

    write(root.join("a/new.log"), 5000);
    let after = dir.path().join("a.json");
    snapshot(&root, &after, &["--top", "0", "--include", "*.log"]);
    let out = ardisk(&["--diff", s(&before), s(&after), "--json"]);
    let doc: Value = serde_json::from_str(&ok(&out)).unwrap();
    assert_eq!(doc["before"]["complete"], false);
    assert!(changes(&doc).is_empty(), "{doc}");
    assert_eq!(doc["summary"]["not_comparable"], 1);
}

#[test]
fn inode_snapshots_record_summarize_too() {
    let dir = TempDir::new().unwrap();
    write(dir.path().join("a/f"), 1);
    for (extra, want) in [(&["--summarize"][..], true), (&[][..], false)] {
        let mut args = vec![s(dir.path()), "--inodes", "--json"];
        args.extend_from_slice(extra);
        let doc: Value = serde_json::from_str(&ok(&ardisk(&args))).unwrap();
        assert_eq!(doc["params"]["summarize"], want, "{extra:?}");
    }
}
