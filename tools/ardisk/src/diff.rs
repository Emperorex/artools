//! `--diff BEFORE AFTER`: what changed between two `--json` snapshots.
//!
//! Both snapshots must be directory reports (`ardisk PATH --json`) or both
//! inode reports (`--inodes --json`) of the same root, taken the same way
//! (same `size_mode`, same filters). Directories are matched by their
//! root-relative path; the delta is `after - before`.
//!
//! A snapshot only lists what its `--top`, `--max-depth` and `--threshold`
//! let through. A path missing from a *complete* snapshot (nothing cut) is
//! really absent there, so it is reported as new or gone. A path missing from
//! a partial snapshot may just have fallen below its cutoff, so it is counted
//! as "not comparable" instead of guessed. `ardisk PATH --json --top 0`
//! writes a complete snapshot.

use crate::{format_size_plain, paint_size, progress::group_digits, report::SCHEMA_VERSION};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
};

// ── Reading snapshots ───────────────────────────────────────────────────────

/// What a snapshot counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Unit {
    Bytes,
    Inodes,
}

#[derive(Deserialize)]
struct RawEntry {
    path: String,
    bytes: Option<u64>,
    inodes: Option<u64>,
}

/// The parts of a `--json` document that `--diff` reads; other keys are
/// ignored, as the schema asks of consumers.
#[derive(Deserialize)]
struct RawDoc {
    schema_version: u32,
    mode: String,
    root: String,
    #[serde(default)]
    size_mode: Option<String>,
    #[serde(default)]
    filters: serde_json::Value,
    #[serde(default)]
    params: serde_json::Value,
    #[serde(default)]
    summary: serde_json::Value,
    #[serde(default)]
    entries: Vec<RawEntry>,
    #[serde(default)]
    truncated: bool,
}

/// One snapshot, checked and indexed by path.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The name it was read from, for messages (`-` is stdin).
    pub name: String,
    pub unit: Unit,
    pub root: String,
    pub size_mode: Option<String>,
    pub filters: serde_json::Value,
    /// Root total from `summary` (covers the whole scan, even when the
    /// list was cut).
    pub total: u64,
    /// Listed directories: root-relative path → bytes or inodes.
    pub entries: HashMap<String, u64>,
    /// Every directory of the scan is listed: nothing was cut by `--top`,
    /// `--max-depth` or `--threshold`.
    pub complete: bool,
    /// Why not, for the warning (e.g. "lists only the top 20 directories").
    pub partial_reason: Option<String>,
}

/// Reads a snapshot from `path` (`-` for stdin).
pub fn load(path: &Path) -> Result<Snapshot, String> {
    let name = path.display().to_string();
    let doc: RawDoc = if path == Path::new("-") {
        serde_json::from_reader(BufReader::new(io::stdin().lock()))
    } else {
        let file = fs::File::open(path).map_err(|e| format!("cannot read {name}: {e}"))?;
        serde_json::from_reader(BufReader::new(file))
    }
    .map_err(|e| format!("{name} is not an ardisk --json document: {e}"))?;
    parse(name, doc)
}

/// Reads a snapshot from any reader (for tests and stdin).
pub fn load_from<R: Read>(name: &str, reader: R) -> Result<Snapshot, String> {
    let doc: RawDoc = serde_json::from_reader(reader)
        .map_err(|e| format!("{name} is not an ardisk --json document: {e}"))?;
    parse(name.to_string(), doc)
}

fn parse(name: String, doc: RawDoc) -> Result<Snapshot, String> {
    if doc.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "{name} has schema_version {}; this ardisk reads version {SCHEMA_VERSION}",
            doc.schema_version
        ));
    }
    let unit = match doc.mode.as_str() {
        "directories" => Unit::Bytes,
        "inodes" => Unit::Inodes,
        other => {
            return Err(format!(
                "{name} is a '{other}' report; --diff compares directory reports \
                 (ardisk PATH --json --top 0) or inode reports (ardisk PATH --inodes --json --top 0)"
            ));
        }
    };

    let mut entries = HashMap::with_capacity(doc.entries.len());
    for (i, e) in doc.entries.into_iter().enumerate() {
        let value = match unit {
            Unit::Bytes => e.bytes,
            Unit::Inodes => e.inodes,
        };
        let key = if unit == Unit::Bytes {
            "bytes"
        } else {
            "inodes"
        };
        let value = value
            .ok_or_else(|| format!("{name}: entry {} ('{}') has no \"{key}\"", i + 1, e.path))?;
        if entries.insert(e.path.clone(), value).is_some() {
            return Err(format!("{name}: path '{}' is listed twice", e.path));
        }
    }

    let total_key = if unit == Unit::Bytes {
        "total_bytes"
    } else {
        "total_inodes"
    };
    let total = doc
        .summary
        .get(total_key)
        .and_then(|v| v.as_u64())
        .or_else(|| entries.get(".").copied())
        .unwrap_or(0);

    let param = |key: &str| doc.params.get(key).filter(|v| !v.is_null());
    let threshold = param("threshold_bytes").or_else(|| param("threshold_inodes"));
    let partial_reason = if doc.truncated {
        let top = param("top").and_then(|v| v.as_u64()).unwrap_or(0);
        Some(if top > 0 {
            if top == 1 {
                "lists only its biggest directory".to_string()
            } else {
                format!("lists only the top {top} directories")
            }
        } else {
            "lists no directories (--summarize)".to_string()
        })
    } else if let Some(depth) = param("max_depth") {
        Some(format!("was taken with --max-depth {depth}"))
    } else if threshold.is_some() {
        Some("was taken with --threshold".to_string())
    } else {
        None
    };

    Ok(Snapshot {
        name,
        unit,
        root: doc.root,
        size_mode: doc.size_mode,
        filters: doc.filters,
        total,
        entries,
        complete: partial_reason.is_none(),
        partial_reason,
    })
}

/// Refuses to compare snapshots that measure different things.
pub fn check_comparable(before: &Snapshot, after: &Snapshot) -> Result<(), String> {
    let (b, a) = (&before.name, &after.name);
    if before.unit != after.unit {
        let what = |s: &Snapshot| match s.unit {
            Unit::Bytes => "counts bytes",
            Unit::Inodes => "counts inodes (--inodes)",
        };
        return Err(format!(
            "{b} {}, {a} {}; compare snapshots of the same kind",
            what(before),
            what(after)
        ));
    }
    if before.root != after.root {
        return Err(format!(
            "the snapshots are of different directories: {} ({b}) and {} ({a})",
            before.root, after.root
        ));
    }
    if before.size_mode != after.size_mode {
        let what = |s: &Snapshot| match s.size_mode.as_deref() {
            Some("apparent") => "apparent sizes (--apparent-size)",
            _ => "disk usage",
        };
        return Err(format!(
            "{b} has {}, {a} has {}; take both snapshots the same way",
            what(before),
            what(after)
        ));
    }
    if before.filters != after.filters {
        let differing: Vec<String> = match (before.filters.as_object(), after.filters.as_object()) {
            (Some(fb), Some(fa)) => {
                let mut keys: Vec<&String> = fb.keys().chain(fa.keys()).collect();
                keys.sort();
                keys.dedup();
                keys.into_iter()
                    .filter(|k| fb.get(*k) != fa.get(*k))
                    .map(|k| {
                        let show = |v: Option<&serde_json::Value>| {
                            v.map_or("(none)".to_string(), |v| v.to_string())
                        };
                        format!("{k}: {} vs {}", show(fb.get(k)), show(fa.get(k)))
                    })
                    .collect()
            }
            _ => vec!["filters".to_string()],
        };
        return Err(format!(
            "the snapshots were taken with different filters ({}); sizes would not be comparable",
            differing.join("; ")
        ));
    }
    Ok(())
}

// ── Comparing ───────────────────────────────────────────────────────────────

/// How a directory changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    New,
    Gone,
    Grew,
    Shrank,
}

/// One changed directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Root-relative, as in the snapshots.
    pub path: String,
    pub before: Option<u64>,
    pub after: Option<u64>,
    pub change: Change,
}

impl Row {
    /// `after - before`; a missing side counts as 0.
    pub fn delta(&self) -> i128 {
        i128::from(self.after.unwrap_or(0)) - i128::from(self.before.unwrap_or(0))
    }
}

/// Everything that differs, before any `--top`/`--threshold`.
#[derive(Debug, Clone)]
pub struct Diff {
    pub unit: Unit,
    pub root: String,
    pub size_mode: Option<String>,
    pub total_before: u64,
    pub total_after: u64,
    /// Changed directories (never the root, see `total_*`), biggest
    /// |delta| first, ties by path.
    pub rows: Vec<Row>,
    pub unchanged: u64,
    /// Listed in only one snapshot while the other is partial.
    pub not_comparable: u64,
}

/// Compares two checked snapshots (see [`check_comparable`]).
pub fn compare(before: &Snapshot, after: &Snapshot) -> Diff {
    let mut rows = Vec::new();
    let (mut unchanged, mut not_comparable) = (0u64, 0u64);

    for (path, &b) in &before.entries {
        if path == "." {
            continue;
        }
        match after.entries.get(path) {
            Some(&a) if a == b => unchanged += 1,
            Some(&a) => rows.push(Row {
                path: path.clone(),
                before: Some(b),
                after: Some(a),
                change: if a > b { Change::Grew } else { Change::Shrank },
            }),
            None if after.complete => rows.push(Row {
                path: path.clone(),
                before: Some(b),
                after: None,
                change: Change::Gone,
            }),
            None => not_comparable += 1,
        }
    }
    for (path, &a) in &after.entries {
        if path == "." || before.entries.contains_key(path) {
            continue;
        }
        if before.complete {
            rows.push(Row {
                path: path.clone(),
                before: None,
                after: Some(a),
                change: Change::New,
            });
        } else {
            not_comparable += 1;
        }
    }

    rows.sort_by(|x, y| {
        y.delta()
            .unsigned_abs()
            .cmp(&x.delta().unsigned_abs())
            .then_with(|| x.path.cmp(&y.path))
    });

    Diff {
        unit: before.unit,
        root: before.root.clone(),
        size_mode: before.size_mode.clone(),
        total_before: before.total,
        total_after: after.total,
        rows,
        unchanged,
        not_comparable,
    }
}

/// `--top` and `--threshold` for the diff.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DiffQuery {
    /// Rows listed; 0 means all.
    pub top: usize,
    /// Smallest |delta| listed, in the snapshots' unit.
    pub threshold: Option<u64>,
}

/// The rows a report lists, and what was left out.
#[derive(Debug, Clone)]
pub struct DiffSelection<'a> {
    pub rows: Vec<&'a Row>,
    /// Changes smaller than `--threshold`.
    pub below_threshold: usize,
    /// Changes past `--top`.
    pub not_shown: usize,
}

pub fn select<'a>(diff: &'a Diff, query: &DiffQuery) -> DiffSelection<'a> {
    let (big, small): (Vec<&Row>, Vec<&Row>) = diff.rows.iter().partition(|r| {
        query
            .threshold
            .is_none_or(|t| r.delta().unsigned_abs() >= u128::from(t))
    });
    let limit = crate::report::top_limit(query.top);
    let not_shown = big.len().saturating_sub(limit);
    DiffSelection {
        rows: big.into_iter().take(limit).collect(),
        below_threshold: small.len(),
        not_shown,
    }
}

// ── Output ──────────────────────────────────────────────────────────────────

/// The root joined with a snapshot path, escaped for the terminal.
fn shown_path(root: &str, rel: &str) -> String {
    let path = if rel == "." {
        PathBuf::from(root)
    } else {
        Path::new(root).join(rel)
    };
    arcommon::display_path(&path)
}

fn amount(unit: Unit, n: u64) -> String {
    match unit {
        Unit::Bytes => format_size_plain(n),
        Unit::Inodes => group_digits(n),
    }
}

fn signed(unit: Unit, delta: i128) -> String {
    let sign = if delta < 0 { '-' } else { '+' };
    let n = u64::try_from(delta.unsigned_abs()).unwrap_or(u64::MAX);
    match unit {
        Unit::Bytes => format!("{sign}{}", format_size_plain(n)),
        Unit::Inodes => format!(
            "{sign}{} inode{}",
            group_digits(n),
            if n == 1 { "" } else { "s" }
        ),
    }
}

/// The text report: one line per change, then totals and what was left out.
///
/// ```text
/// +   120.4 MB  /data/logs  (1.1 GB → 1.2 GB)
/// +    12.1 MB  /data/cache  (new)
/// -     3.2 MB  /data/tmp/old  (gone)
/// = unchanged: 1,204 directories
///   total  1.4 GB → 1.5 GB  (+129.3 MB)
/// ```
pub fn render_text(diff: &Diff, sel: &DiffSelection) -> String {
    let mut out = String::new();
    for row in &sel.rows {
        let delta = row.delta();
        let n = u64::try_from(delta.unsigned_abs()).unwrap_or(u64::MAX);
        let sign = if delta < 0 { '-' } else { '+' };
        let size = match diff.unit {
            Unit::Bytes => paint_size(n, format!("{:>10}", format_size_plain(n))),
            Unit::Inodes => format!("{:>10}", group_digits(n)),
        };
        let note = match row.change {
            Change::New => "new".to_string(),
            Change::Gone => "gone".to_string(),
            Change::Grew | Change::Shrank => format!(
                "{} → {}",
                amount(diff.unit, row.before.unwrap_or(0)),
                amount(diff.unit, row.after.unwrap_or(0))
            ),
        };
        out.push_str(&format!(
            "{sign}{size}  {}  ({note})\n",
            shown_path(&diff.root, &row.path)
        ));
    }
    if sel.rows.is_empty() {
        out.push_str("no changes listed\n");
    }

    if sel.not_shown > 0 {
        out.push_str(&format!(
            "  ({} more change{} not shown; raise --top, or --top 0 for all)\n",
            group_digits(sel.not_shown as u64),
            if sel.not_shown == 1 { "" } else { "s" }
        ));
    }
    if sel.below_threshold > 0 {
        out.push_str(&format!(
            "  ({} smaller change{} hidden by --threshold)\n",
            group_digits(sel.below_threshold as u64),
            if sel.below_threshold == 1 { "" } else { "s" }
        ));
    }
    out.push_str(&format!(
        "= unchanged: {} director{}\n",
        group_digits(diff.unchanged),
        if diff.unchanged == 1 { "y" } else { "ies" }
    ));
    let total_delta = i128::from(diff.total_after) - i128::from(diff.total_before);
    out.push_str(&format!(
        "  total  {} → {}  ({})\n",
        amount(diff.unit, diff.total_before),
        amount(diff.unit, diff.total_after),
        if total_delta == 0 {
            "no change".to_string()
        } else {
            signed(diff.unit, total_delta)
        }
    ));
    out
}

/// The stderr warning when a snapshot lists only part of the tree, or
/// `None` when both are complete. Given whenever one is partial, even if no
/// path was left uncompared: changes outside a cut list are simply not
/// visible, and an empty diff must not read as "nothing changed".
pub fn partial_warning(diff: &Diff, before: &Snapshot, after: &Snapshot) -> Option<String> {
    let partial: Vec<String> = [before, after]
        .iter()
        .filter_map(|s| s.partial_reason.as_ref().map(|r| format!("{} {r}", s.name)))
        .collect();
    if partial.is_empty() {
        return None;
    }
    let what = if diff.not_comparable > 0 {
        format!(
            "{} director{} listed in only one snapshot could not be compared, and changes \
             outside the lists are not shown",
            group_digits(diff.not_comparable),
            if diff.not_comparable == 1 { "y" } else { "ies" },
        )
    } else {
        "changes outside the lists are not shown".to_string()
    };
    Some(format!(
        "ardisk: {}: {what}. Save both snapshots with --top 0 (and without --max-depth or \
         --threshold) for a full comparison.",
        partial.join(" and ")
    ))
}

#[derive(Serialize)]
struct SideJson<'a> {
    file: &'a str,
    total: u64,
    complete: bool,
}

#[derive(Serialize)]
struct SummaryJson {
    total_delta: i64,
    new: usize,
    gone: usize,
    grew: usize,
    shrank: usize,
    unchanged: u64,
    not_comparable: u64,
    below_threshold: usize,
}

#[derive(Serialize)]
struct EntryJson<'a> {
    path: &'a str,
    change: Change,
    before: Option<u64>,
    after: Option<u64>,
    delta: i64,
}

#[derive(Serialize)]
struct DiffDoc<'a> {
    schema_version: u32,
    mode: &'static str,
    root: &'a str,
    unit: Unit,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_mode: Option<&'a str>,
    before: SideJson<'a>,
    after: SideJson<'a>,
    params: &'a DiffQuery,
    summary: SummaryJson,
    entries: Vec<EntryJson<'a>>,
    truncated: bool,
}

fn clamp(v: i128) -> i64 {
    i64::try_from(v).unwrap_or(if v < 0 { i64::MIN } else { i64::MAX })
}

/// Writes the `diff` document (see the README for the layout).
pub fn write_json<W: Write>(
    w: &mut W,
    diff: &Diff,
    sel: &DiffSelection,
    query: &DiffQuery,
    before: &Snapshot,
    after: &Snapshot,
) -> io::Result<()> {
    let count = |c: Change| diff.rows.iter().filter(|r| r.change == c).count();
    let doc = DiffDoc {
        schema_version: SCHEMA_VERSION,
        mode: "diff",
        root: &diff.root,
        unit: diff.unit,
        size_mode: diff.size_mode.as_deref(),
        before: SideJson {
            file: &before.name,
            total: before.total,
            complete: before.complete,
        },
        after: SideJson {
            file: &after.name,
            total: after.total,
            complete: after.complete,
        },
        params: query,
        summary: SummaryJson {
            total_delta: clamp(i128::from(diff.total_after) - i128::from(diff.total_before)),
            new: count(Change::New),
            gone: count(Change::Gone),
            grew: count(Change::Grew),
            shrank: count(Change::Shrank),
            unchanged: diff.unchanged,
            not_comparable: diff.not_comparable,
            below_threshold: sel.below_threshold,
        },
        entries: sel
            .rows
            .iter()
            .map(|r| EntryJson {
                path: &r.path,
                change: r.change,
                before: r.before,
                after: r.after,
                delta: clamp(r.delta()),
            })
            .collect(),
        truncated: sel.not_shown > 0,
    };
    serde_json::to_writer_pretty(&mut *w, &doc)?;
    writeln!(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(mode: &str, truncated: bool, params: &str, entries: &[(&str, u64)]) -> String {
        let key = if mode == "inodes" { "inodes" } else { "bytes" };
        let total_key = if mode == "inodes" {
            "total_inodes"
        } else {
            "total_bytes"
        };
        let total = entries
            .iter()
            .find(|(p, _)| *p == ".")
            .map_or(0, |(_, n)| *n);
        let items: Vec<String> = entries
            .iter()
            .map(|(p, n)| format!(r#"{{"path":"{p}","{key}":{n},"depth":0,"kind":"directory"}}"#))
            .collect();
        format!(
            r#"{{"schema_version":1,"mode":"{mode}","root":"/data","size_mode":"disk",
               "filters":{{"include":null,"exclude":[]}},"params":{params},
               "summary":{{"{total_key}":{total}}},"entries":[{}],"truncated":{truncated}}}"#,
            items.join(",")
        )
    }

    fn snap(name: &str, json: &str) -> Snapshot {
        load_from(name, json.as_bytes()).unwrap()
    }

    const FULL: &str = r#"{"top":0,"max_depth":null,"threshold_bytes":null}"#;

    #[test]
    fn classifies_new_gone_grew_shrank_and_unchanged() {
        let b = snap(
            "b",
            &doc(
                "directories",
                false,
                FULL,
                &[
                    (".", 100),
                    ("logs", 50),
                    ("tmp", 30),
                    ("same", 10),
                    ("old", 5),
                ],
            ),
        );
        let a = snap(
            "a",
            &doc(
                "directories",
                false,
                FULL,
                &[
                    (".", 170),
                    ("logs", 120),
                    ("tmp", 20),
                    ("same", 10),
                    ("cache", 15),
                ],
            ),
        );
        check_comparable(&b, &a).unwrap();
        let d = compare(&b, &a);
        let got: Vec<(&str, Change, i128)> = d
            .rows
            .iter()
            .map(|r| (r.path.as_str(), r.change, r.delta()))
            .collect();
        assert_eq!(
            got,
            [
                ("logs", Change::Grew, 70),
                ("cache", Change::New, 15),
                ("tmp", Change::Shrank, -10),
                ("old", Change::Gone, -5),
            ]
        );
        assert_eq!(d.unchanged, 1);
        assert_eq!(d.not_comparable, 0);
        assert_eq!((d.total_before, d.total_after), (100, 170));
    }

    #[test]
    fn missing_from_a_partial_snapshot_is_not_comparable() {
        let top2 = r#"{"top":2,"max_depth":null,"threshold_bytes":null}"#;
        let b = snap(
            "b",
            &doc("directories", true, top2, &[(".", 100), ("big", 60)]),
        );
        let a = snap(
            "a",
            &doc(
                "directories",
                false,
                FULL,
                &[(".", 100), ("big", 50), ("small", 9)],
            ),
        );
        assert!(!b.complete && a.complete);
        let d = compare(&b, &a);
        assert_eq!(d.rows.len(), 1, "only 'big' is in both");
        assert_eq!(d.rows[0].change, Change::Shrank);
        assert_eq!(
            d.not_comparable, 1,
            "'small' may have been below b's cutoff"
        );
        let w = partial_warning(&d, &b, &a).unwrap();
        assert!(
            w.contains("ardisk: b lists only the top 2 directories: 1 directory"),
            "{w}"
        );
        assert!(w.contains("--top 0"), "{w}");
        // Both complete: no warning.
        assert!(partial_warning(&d, &a, &a).is_none());
        assert!(w.contains("--top 0"), "{w}");
    }

    #[test]
    fn max_depth_or_threshold_make_a_snapshot_partial() {
        let depth = r#"{"top":0,"max_depth":1,"threshold_bytes":null}"#;
        let thr = r#"{"top":0,"max_depth":null,"threshold_bytes":1000}"#;
        let s = snap("d", &doc("directories", false, depth, &[(".", 1)]));
        assert_eq!(
            s.partial_reason.as_deref(),
            Some("was taken with --max-depth 1")
        );
        let s = snap("t", &doc("directories", false, thr, &[(".", 1)]));
        assert!(!s.complete);
    }

    #[test]
    fn refuses_other_modes_versions_roots_units_and_filters() {
        let err = load_from(
            "x",
            br#"{"schema_version":1,"mode":"largest_files","root":"/"}"#.as_slice(),
        )
        .unwrap_err();
        assert!(err.contains("'largest_files' report"), "{err}");
        let err = load_from(
            "x",
            br#"{"schema_version":2,"mode":"directories","root":"/"}"#.as_slice(),
        )
        .unwrap_err();
        assert!(err.contains("schema_version 2"), "{err}");
        let err = load_from("x", b"not json".as_slice()).unwrap_err();
        assert!(err.contains("not an ardisk --json document"), "{err}");

        let base = doc("directories", false, FULL, &[(".", 1)]);
        let b = snap("b", &base);
        let inodes = snap("a", &doc("inodes", false, FULL, &[(".", 1)]));
        assert!(
            check_comparable(&b, &inodes)
                .unwrap_err()
                .contains("same kind")
        );
        let other_root = snap("a", &base.replace("\"/data\"", "\"/other\""));
        assert!(
            check_comparable(&b, &other_root)
                .unwrap_err()
                .contains("different directories")
        );
        let apparent = snap("a", &base.replace("\"disk\"", "\"apparent\""));
        assert!(
            check_comparable(&b, &apparent)
                .unwrap_err()
                .contains("--apparent-size")
        );
        let filtered = snap(
            "a",
            &base.replace("\"exclude\":[]", "\"exclude\":[\"*.log\"]"),
        );
        let err = check_comparable(&b, &filtered).unwrap_err();
        assert!(err.contains(r#"exclude: [] vs ["*.log"]"#), "{err}");
    }

    #[test]
    fn missing_value_or_duplicate_path_is_an_error() {
        let no_bytes =
            r#"{"schema_version":1,"mode":"directories","root":"/","entries":[{"path":"."}]}"#;
        assert!(
            load_from("x", no_bytes.as_bytes())
                .unwrap_err()
                .contains("no \"bytes\"")
        );
        let twice = doc("directories", false, FULL, &[(".", 1), ("a", 1), ("a", 2)]);
        assert!(
            load_from("x", twice.as_bytes())
                .unwrap_err()
                .contains("listed twice")
        );
    }

    #[test]
    fn select_applies_threshold_then_top() {
        let b = snap(
            "b",
            &doc(
                "directories",
                false,
                FULL,
                &[(".", 0), ("a", 0), ("b", 0), ("c", 0)],
            ),
        );
        let a = snap(
            "a",
            &doc(
                "directories",
                false,
                FULL,
                &[(".", 0), ("a", 300), ("b", 200), ("c", 5)],
            ),
        );
        let d = compare(&b, &a);
        let q = DiffQuery {
            top: 1,
            threshold: Some(100),
        };
        let s = select(&d, &q);
        assert_eq!(
            s.rows.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            ["a"]
        );
        assert_eq!((s.below_threshold, s.not_shown), (1, 1));
        let all = select(
            &d,
            &DiffQuery {
                top: 0,
                threshold: None,
            },
        );
        assert_eq!(all.rows.len(), 3);
        assert_eq!(all.not_shown, 0);
    }

    #[test]
    fn text_report_shape() {
        let b = snap(
            "b",
            &doc(
                "directories",
                false,
                FULL,
                &[(".", 2048), ("logs", 1024), ("old", 1024)],
            ),
        );
        let a = snap(
            "a",
            &doc(
                "directories",
                false,
                FULL,
                &[(".", 4096), ("logs", 3072), ("new", 1024)],
            ),
        );
        let d = compare(&b, &a);
        let text = render_text(
            &d,
            &select(
                &d,
                &DiffQuery {
                    top: 20,
                    threshold: None,
                },
            ),
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "+   2.00 KB  /data/logs  (1.00 KB → 3.00 KB)");
        assert_eq!(lines[1], "+   1.00 KB  /data/new  (new)");
        assert_eq!(lines[2], "-   1.00 KB  /data/old  (gone)");
        assert_eq!(lines[3], "= unchanged: 0 directories");
        assert_eq!(lines[4], "  total  2.00 KB → 4.00 KB  (+2.00 KB)");
    }
}
