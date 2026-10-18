//! End-to-end tests for --json: real binary, every stdout line parsed as JSON.

use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

fn argrep(dir: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_argrep"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run argrep");
    if let Some(input) = stdin {
        let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
    }
    child.wait_with_output().unwrap()
}

/// Every stdout line as JSON; fails on anything that isn't one JSON object.
fn events(out: &Output) -> Vec<Value> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone())
        .unwrap()
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l:?}"));
            assert!(v.is_object() && v["type"].is_string(), "{l}");
            v
        })
        .collect()
}

fn of_type<'a>(ev: &'a [Value], t: &str) -> Vec<&'a Value> {
    ev.iter().filter(|e| e["type"] == t).collect()
}

fn name(e: &Value) -> String {
    Path::new(e["path"].as_str().unwrap())
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

#[test]
fn one_file_one_match_round_trips() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("main.rs"),
        "fn main() {\n    // TODO: fix\n}\n",
    )
    .unwrap();
    let ev = events(&argrep(dir.path(), &["TODO", ".", "--json"], None));

    let types: Vec<&str> = ev.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["begin", "match", "end", "summary"]);
    let m = &ev[1];
    assert_eq!(name(m), "main.rs");
    assert_eq!(m["line_number"], 2);
    assert_eq!(m["lines"]["text"], "    // TODO: fix");
    assert_eq!(
        m["submatches"],
        serde_json::json!([{"start": 7, "end": 11, "match": {"text": "TODO"}}])
    );
    assert_eq!(ev[2]["matched_lines"], 1);

    let s = &ev[3];
    assert_eq!(s["schema_version"], 1);
    assert_eq!(s["mode"], "content");
    assert_eq!(s["matched_lines"], 1);
    assert_eq!(s["files_with_matches"], 1);
    assert_eq!(s["files_searched"], 1);
    assert_eq!(s["errors"], 0);
}

#[test]
fn each_files_events_stay_together_with_many_workers() {
    let dir = TempDir::new().unwrap();
    // Workers take whole directories, so files only interleave when they
    // are in different directories: one file per directory, several
    // directories in flight at once.
    const LINES: usize = 500;
    for i in 0..40 {
        let sub = dir.path().join(format!("d{i:02}"));
        fs::create_dir(&sub).unwrap();
        let body: String = (0..LINES)
            .map(|n| format!("line {n} TODO\nplain\n"))
            .collect();
        fs::write(sub.join("f.txt"), body).unwrap();
    }
    let ev = events(&argrep(
        dir.path(),
        &["TODO", ".", "--json", "-j", "8"],
        None,
    ));

    // begin, then only events for that file, then end; never interleaved.
    let mut open: Option<String> = None;
    let mut closed = HashSet::new();
    for e in &ev {
        match e["type"].as_str().unwrap() {
            "begin" => {
                assert!(open.is_none(), "begin inside another file's block");
                let p = e["path"].as_str().unwrap().to_string();
                assert!(!closed.contains(&p), "file reopened: {p}");
                open = Some(p);
            }
            "match" | "context" => {
                assert_eq!(e["path"].as_str(), open.as_deref(), "interleaved event");
            }
            "end" => {
                assert_eq!(e["path"].as_str(), open.as_deref());
                assert_eq!(e["matched_lines"], LINES);
                closed.insert(open.take().unwrap());
            }
            "summary" => assert!(open.is_none()),
            t => panic!("unexpected {t}"),
        }
    }
    assert_eq!(closed.len(), 40);
    assert_eq!(ev.last().unwrap()["type"], "summary");
    assert_eq!(ev.last().unwrap()["matched_lines"], 40 * LINES);
}

#[test]
fn context_lines_are_context_events() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "one\nTODO\nthree\nfour\n").unwrap();
    let ev = events(&argrep(
        dir.path(),
        &["TODO", ".", "--json", "-C", "1"],
        None,
    ));
    let lines: Vec<(&str, u64)> = ev
        .iter()
        .filter(|e| e["line_number"].is_u64())
        .map(|e| {
            (
                e["type"].as_str().unwrap(),
                e["line_number"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(lines, [("context", 1), ("match", 2), ("context", 3)]);
}

#[test]
fn replace_keeps_the_line_and_adds_replacements() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "x TODO y TODO\n").unwrap();
    let ev = events(&argrep(
        dir.path(),
        &["TODO", ".", "--json", "-r", "DONE"],
        None,
    ));
    let m = of_type(&ev, "match")[0];
    assert_eq!(m["lines"]["text"], "x TODO y TODO");
    let reps: Vec<&str> = m["submatches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["replacement"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(reps, ["DONE", "DONE"]);
}

#[test]
fn only_matching_adds_nothing_and_keeps_context() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "before\nTODO x\n").unwrap();
    let with_o = events(&argrep(
        dir.path(),
        &["TODO", ".", "--json", "-o", "-B", "1"],
        None,
    ));
    assert_eq!(of_type(&with_o, "context").len(), 1);
    assert_eq!(of_type(&with_o, "match")[0]["lines"]["text"], "TODO x");
}

#[test]
fn file_list_modes() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("yes.txt"), "TODO\n").unwrap();
    fs::write(dir.path().join("no.txt"), "nothing\n").unwrap();

    let ev = events(&argrep(dir.path(), &["-l", "TODO", ".", "--json"], None));
    assert_eq!(
        of_type(&ev, "file")
            .iter()
            .map(|e| name(e))
            .collect::<Vec<_>>(),
        ["yes.txt"]
    );
    assert_eq!(ev.last().unwrap()["mode"], "files_with_matches");
    assert_eq!(ev.last().unwrap()["files_with_matches"], 1);

    let ev = events(&argrep(dir.path(), &["-L", "TODO", ".", "--json"], None));
    assert_eq!(
        of_type(&ev, "file")
            .iter()
            .map(|e| name(e))
            .collect::<Vec<_>>(),
        ["no.txt"]
    );
    assert_eq!(ev.last().unwrap()["mode"], "files_without_match");
    assert!(ev.last().unwrap()["files_with_matches"].is_null());

    let ev = events(&argrep(dir.path(), &["--files", ".", "--json"], None));
    assert_eq!(of_type(&ev, "file").len(), 2);
    assert_eq!(ev.last().unwrap()["mode"], "files");
    assert!(of_type(&ev, "begin").is_empty());
}

#[test]
fn count_mode_reports_every_file() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("yes.txt"), "TODO\nTODO\n").unwrap();
    fs::write(dir.path().join("no.txt"), "nothing\n").unwrap();
    let ev = events(&argrep(dir.path(), &["-c", "TODO", ".", "--json"], None));
    let mut counts: Vec<(String, u64)> = of_type(&ev, "end")
        .iter()
        .map(|e| (name(e), e["matched_lines"].as_u64().unwrap()))
        .collect();
    counts.sort();
    assert_eq!(counts, [("no.txt".into(), 0), ("yes.txt".into(), 2)]);
    assert!(of_type(&ev, "match").is_empty());
    assert_eq!(ev.last().unwrap()["mode"], "count");
    assert_eq!(ev.last().unwrap()["files_with_matches"], 1);
}

#[test]
fn stdin_is_one_input_named_stdin() {
    let dir = TempDir::new().unwrap();
    let ev = events(&argrep(
        dir.path(),
        &["TODO", "--json"],
        Some("a TODO\nb\n"),
    ));
    let types: Vec<&str> = ev.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["begin", "match", "end", "summary"]);
    assert_eq!(ev[0]["path"], "<stdin>");
    assert_eq!(ev[1]["line_number"], 1);
    assert_eq!(ev[3]["files_searched"], 1);
}

#[cfg(unix)]
#[test]
fn names_and_text_are_exact_and_json_escaped() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("e\x1b[31m.txt"), "TODO\x07\n").unwrap();
    let out = argrep(dir.path(), &["TODO", ".", "--json"], None);
    let raw = String::from_utf8(out.stdout.clone()).unwrap();
    assert!(
        !raw.contains('\x1b') && !raw.contains('\x07'),
        "raw control bytes: {raw:?}"
    );
    let ev = events(&out);
    assert_eq!(
        name(&ev[0]),
        "e\x1b[31m.txt",
        "decoded name is the exact name"
    );
    assert_eq!(of_type(&ev, "match")[0]["lines"]["text"], "TODO\x07");
}

#[test]
fn no_match_still_ends_with_a_summary() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "nothing\n").unwrap();
    let ev = events(&argrep(dir.path(), &["TODO", ".", "--json"], None));
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["type"], "summary");
    assert_eq!(ev[0]["matched_lines"], 0);
}

#[test]
fn quiet_and_null_are_rejected() {
    let dir = TempDir::new().unwrap();
    for extra in [&["-q"][..], &["-l", "-0"]] {
        let mut args = vec!["TODO", ".", "--json"];
        args.extend_from_slice(extra);
        let out = argrep(dir.path(), &args, None);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty());
    }
}
