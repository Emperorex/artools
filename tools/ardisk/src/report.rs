//! Report selection and the machine-readable `--json` output.
//!
//! `--json` is an output format, independent of the analysis mode: it
//! serializes whatever the selected mode (`directories`, `largest_files`,
//! `duplicates`, `by_type`) produced. The text and JSON outputs share the selection
//! logic in this module, so they always agree on *what* is reported.
//!
//! # Document layout (schema version 1)
//!
//! Every document is one JSON object with the same header:
//!
//! ```text
//! schema_version  integer   bumped on incompatible changes only
//! mode            string    "directories" | "largest_files" | "duplicates" | "by_type"
//! root            string    canonical absolute path that was scanned
//! size_mode       string    "disk" (block allocation) | "apparent" (logical length)
//! filters         object    include / exclude / ignore / no_ignore as given
//! params          object    mode-specific options that shape the result
//! summary         object    mode-specific totals, always over the whole scan
//! entries|groups|types  array  the listed items (see below)
//! truncated       boolean   true if more items existed than were listed
//! ```
//!
//! All paths inside `entries` / `groups` are relative to `root`, use `/` as
//! separator, and the root itself is `"."`. Sizes are plain integers (bytes).
//! A path that is not valid UTF-8 is emitted with the invalid parts replaced
//! by U+FFFD; the writers report how many paths were affected.
//!
//! Compatible additions (new keys) do not change `schema_version`; consumers
//! must ignore keys they do not know.

use crate::{FileEntry, duplicates::DuplicateReport, format_size_plain, types::TypeTable};
use serde::Serialize;
use std::{
    collections::HashMap,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// Version of the JSON document layout.
pub const SCHEMA_VERSION: u32 = 1;

// ── Selection shared by text and JSON output ────────────────────────────────

/// What the directory report lists, i.e. the knobs behind `--top`,
/// `--max-depth`, `--threshold`, `--include` and `--summarize`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DirectoryQuery {
    /// `--top`: maximum number of directories listed.
    pub top: usize,
    /// `--max-depth`: deepest level listed (the root is depth 0).
    pub max_depth: Option<usize>,
    /// `--threshold`: smallest directory listed, in bytes.
    pub threshold_bytes: Option<u64>,
    /// `--include` is active: directories without matching file content are
    /// not listed (unless a threshold is given).
    #[serde(skip)]
    pub include_active: bool,
    /// `--summarize`: list nothing, report totals only.
    #[serde(skip)]
    pub summarize: bool,
}

impl DirectoryQuery {
    fn limit(&self) -> usize {
        if self.summarize { 0 } else { self.top }
    }
}

/// The directories a report lists, largest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectorySelection {
    pub entries: Vec<(PathBuf, u64)>,
    /// More directories qualified than fit within the limit.
    pub truncated: bool,
}

/// Picks the directories to list: biggest first, ties broken by path so the
/// result does not depend on hash-map iteration order.
pub fn select_directories(
    aggregated: &HashMap<PathBuf, u64>,
    aggregated_content: &HashMap<PathBuf, u64>,
    root: &Path,
    query: &DirectoryQuery,
) -> DirectorySelection {
    let mut sorted: Vec<(&PathBuf, u64)> = aggregated.iter().map(|(p, s)| (p, *s)).collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    let limit = query.limit();
    let mut entries = Vec::new();
    let mut truncated = false;

    for (path, size) in sorted {
        // Suppress directories with no matching file content when --include
        // is active: they only add noise. The content map (file bytes only,
        // no inode cost) is checked so directory inode costs don't defeat
        // the suppression.
        if query.threshold_bytes.is_none() && query.include_active {
            let content = aggregated_content.get(path).copied().unwrap_or(0);
            if content == 0 {
                continue;
            }
        }
        if query.threshold_bytes.is_some_and(|min| size < min) {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        if query
            .max_depth
            .is_some_and(|max| rel.components().count() > max)
        {
            continue;
        }

        if entries.len() == limit {
            truncated = true;
            break;
        }
        entries.push((path.clone(), size));
    }

    DirectorySelection { entries, truncated }
}

// ── JSON ────────────────────────────────────────────────────────────────────

/// The scan filters, echoed back so a document is self-describing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Filters {
    /// `--include` pattern.
    pub include: Option<String>,
    /// `--exclude` patterns, in the order given.
    pub exclude: Vec<String>,
    /// Extra `--ignore` directory names (the built-in ones are not listed).
    pub ignore: Vec<String>,
    /// `--no-ignore`: built-in ignores and `.gitignore` rules were off.
    pub no_ignore: bool,
    /// `--no-hidden`: hidden files and directories (names starting with `.`)
    /// were skipped.
    pub no_hidden: bool,
}

/// Facts about the run that are common to every mode.
#[derive(Debug, Clone)]
pub struct ReportMeta {
    /// Canonical path that was scanned.
    pub root: PathBuf,
    /// `--apparent-size` was given.
    pub apparent_size: bool,
    pub filters: Filters,
}

/// Totals of a directory scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DirectorySummary {
    /// Size of the root, as printed by `--summarize`.
    pub total_bytes: u64,
    /// Regular files counted (hard links once; symlinks and filtered-out
    /// files not at all).
    pub files: u64,
    /// Directories visited, including the root.
    pub directories: u64,
}

#[derive(Serialize)]
struct Header<'a> {
    schema_version: u32,
    mode: &'static str,
    root: String,
    size_mode: &'static str,
    filters: &'a Filters,
}

/// Turns paths into root-relative, `/`-separated strings and counts how many
/// had to be converted lossily.
struct PathFmt<'a> {
    root: &'a Path,
    lossy: usize,
}

impl PathFmt<'_> {
    fn absolute(&mut self, path: &Path) -> String {
        self.string(path.as_os_str())
    }

    fn string(&mut self, s: &std::ffi::OsStr) -> String {
        match s.to_str() {
            Some(v) => v.to_string(),
            None => {
                self.lossy += 1;
                s.to_string_lossy().into_owned()
            }
        }
    }

    fn rel(&mut self, path: &Path) -> String {
        let Ok(rel) = path.strip_prefix(self.root) else {
            return self.absolute(path);
        };
        if rel.as_os_str().is_empty() {
            return ".".to_string();
        }
        let mut out = String::new();
        let mut lossy = false;
        for (i, component) in rel.components().enumerate() {
            if i > 0 {
                out.push('/');
            }
            match component.as_os_str().to_str() {
                Some(s) => out.push_str(s),
                None => {
                    lossy = true;
                    out.push_str(&component.as_os_str().to_string_lossy());
                }
            }
        }
        if lossy {
            self.lossy += 1;
        }
        out
    }
}

fn size_mode(apparent: bool) -> &'static str {
    if apparent { "apparent" } else { "disk" }
}

fn header<'a>(
    meta: &'a ReportMeta,
    mode: &'static str,
    apparent: bool,
    fmt: &mut PathFmt,
) -> Header<'a> {
    Header {
        schema_version: SCHEMA_VERSION,
        mode,
        root: fmt.absolute(&meta.root),
        size_mode: size_mode(apparent),
        filters: &meta.filters,
    }
}

fn emit<W: Write, T: Serialize>(w: &mut W, doc: &T) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *w, doc)?;
    writeln!(w)
}

#[derive(Serialize)]
struct DirectoryItem {
    path: String,
    bytes: u64,
    depth: usize,
    kind: &'static str,
}

#[derive(Serialize)]
struct DirectoriesDoc<'a> {
    #[serde(flatten)]
    header: Header<'a>,
    params: &'a DirectoryQuery,
    summary: DirectorySummary,
    entries: Vec<DirectoryItem>,
    truncated: bool,
}

/// Writes the `directories` document. Returns how many paths were not valid
/// UTF-8 and were converted lossily.
pub fn write_directories<W: Write>(
    w: &mut W,
    meta: &ReportMeta,
    query: &DirectoryQuery,
    selection: &DirectorySelection,
    summary: DirectorySummary,
) -> io::Result<usize> {
    let mut fmt = PathFmt {
        root: &meta.root,
        lossy: 0,
    };
    let header = header(meta, "directories", meta.apparent_size, &mut fmt);
    let entries = selection
        .entries
        .iter()
        .map(|(path, bytes)| DirectoryItem {
            path: fmt.rel(path),
            bytes: *bytes,
            depth: path
                .strip_prefix(&meta.root)
                .map_or(0, |r| r.components().count()),
            kind: "directory",
        })
        .collect();
    let doc = DirectoriesDoc {
        header,
        params: query,
        summary,
        entries,
        truncated: selection.truncated,
    };
    emit(w, &doc)?;
    Ok(fmt.lossy)
}

#[derive(Serialize)]
struct LargestParams {
    limit: usize,
}

#[derive(Serialize)]
struct LargestSummary {
    total_bytes: u64,
    files: u64,
}

#[derive(Serialize)]
struct FileItem {
    path: String,
    bytes: u64,
    kind: &'static str,
}

#[derive(Serialize)]
struct LargestDoc<'a> {
    #[serde(flatten)]
    header: Header<'a>,
    params: LargestParams,
    summary: LargestSummary,
    entries: Vec<FileItem>,
    truncated: bool,
}

/// Writes the `largest_files` document. `files` are the listed files,
/// largest first; `file_count` is the number of files in the whole scan.
pub fn write_largest_files<W: Write>(
    w: &mut W,
    meta: &ReportMeta,
    limit: usize,
    total_bytes: u64,
    file_count: u64,
    files: &[FileEntry],
) -> io::Result<usize> {
    let mut fmt = PathFmt {
        root: &meta.root,
        lossy: 0,
    };
    let header = header(meta, "largest_files", meta.apparent_size, &mut fmt);
    let entries = files
        .iter()
        .map(|f| FileItem {
            path: fmt.rel(&f.path),
            bytes: f.size,
            kind: "file",
        })
        .collect();
    let doc = LargestDoc {
        header,
        params: LargestParams { limit },
        summary: LargestSummary {
            total_bytes,
            files: file_count,
        },
        entries,
        truncated: file_count > files.len() as u64,
    };
    emit(w, &doc)?;
    Ok(fmt.lossy)
}

#[derive(Serialize)]
struct DuplicatesParams {
    top: usize,
    min_size_bytes: u64,
}

#[derive(Serialize)]
struct DuplicatesSummary {
    /// Number of duplicate groups found.
    groups: usize,
    /// Files that belong to some group (every copy, including the one you
    /// would keep).
    duplicate_files: usize,
    /// `duplicate_files` minus one per group: the copies that could go.
    redundant_files: usize,
    /// Sum over groups of `bytes_per_file * (files - 1)`.
    potentially_reclaimable_bytes: u64,
    /// Files that were examined (regular files of at least `min_size_bytes`).
    files_considered: usize,
    /// Files that could not be read and took no part in the search.
    unreadable_files: usize,
}

#[derive(Serialize)]
struct DuplicateGroupItem {
    /// `"blake3:"` followed by the hex digest of the shared content.
    hash: String,
    bytes_per_file: u64,
    files: Vec<String>,
    potentially_reclaimable_bytes: u64,
}

#[derive(Serialize)]
struct DuplicatesDoc<'a> {
    #[serde(flatten)]
    header: Header<'a>,
    params: DuplicatesParams,
    summary: DuplicatesSummary,
    groups: Vec<DuplicateGroupItem>,
    truncated: bool,
}

/// Writes the `duplicates` document.
///
/// `top` limits how many groups are listed (most reclaimable first), or
/// `0` with `summarize`; the summary always covers every group found.
/// Sizes in this mode are logical file lengths, so `size_mode` is always
/// `"apparent"`.
pub fn write_duplicates<W: Write>(
    w: &mut W,
    meta: &ReportMeta,
    top: usize,
    summarize: bool,
    min_size_bytes: u64,
    report: &DuplicateReport,
) -> io::Result<usize> {
    let mut fmt = PathFmt {
        root: &meta.root,
        lossy: 0,
    };
    let limit = if summarize { 0 } else { top };
    let header = header(meta, "duplicates", true, &mut fmt);
    let groups = report
        .groups
        .iter()
        .take(limit)
        .map(|g| DuplicateGroupItem {
            hash: format!("blake3:{}", g.digest_hex()),
            bytes_per_file: g.len,
            files: g.paths.iter().map(|p| fmt.rel(p)).collect(),
            potentially_reclaimable_bytes: g.reclaimable(),
        })
        .collect();
    let doc = DuplicatesDoc {
        header,
        params: DuplicatesParams {
            top,
            min_size_bytes,
        },
        summary: DuplicatesSummary {
            groups: report.groups.len(),
            duplicate_files: report.groups.iter().map(|g| g.paths.len()).sum(),
            redundant_files: report.redundant_files(),
            potentially_reclaimable_bytes: report.reclaimable(),
            files_considered: report.stats.candidates,
            unreadable_files: report.unreadable.len(),
        },
        groups,
        truncated: report.groups.len() > limit,
    };
    emit(w, &doc)?;
    Ok(fmt.lossy)
}

// ── By type ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ByTypeParams {
    top: usize,
}

#[derive(Serialize)]
struct ByTypeSummary {
    /// Sum of the sizes of all counted files (directory entries themselves
    /// are not included), i.e. the sum of `bytes` over every type.
    total_bytes: u64,
    /// Number of files counted.
    files: u64,
    /// Number of distinct types found (the length of the full list).
    types: usize,
}

#[derive(Serialize)]
struct TypeItem<'a> {
    /// Lower-cased extension with its leading dot, or `null` for files
    /// without an extension.
    extension: Option<&'a str>,
    files: u64,
    bytes: u64,
}

#[derive(Serialize)]
struct ByTypeDoc<'a> {
    #[serde(flatten)]
    header: Header<'a>,
    params: ByTypeParams,
    summary: ByTypeSummary,
    types: Vec<TypeItem<'a>>,
    truncated: bool,
}

/// Writes the `by_type` document: the `top` types with the most bytes
/// (none with `summarize`); `summary` always covers every type.
pub fn write_by_type<W: Write>(
    w: &mut W,
    meta: &ReportMeta,
    top: usize,
    summarize: bool,
    table: &TypeTable,
) -> io::Result<usize> {
    let mut fmt = PathFmt {
        root: &meta.root,
        lossy: 0,
    };
    let limit = if summarize { 0 } else { top };
    let header = header(meta, "by_type", meta.apparent_size, &mut fmt);
    let types = table
        .rows
        .iter()
        .take(limit)
        .map(|r| TypeItem {
            extension: r.extension.as_deref(),
            files: r.files,
            bytes: r.bytes,
        })
        .collect();
    let doc = ByTypeDoc {
        header,
        params: ByTypeParams { top },
        summary: ByTypeSummary {
            total_bytes: table.total_bytes,
            files: table.total_files,
            types: table.rows.len(),
        },
        types,
        truncated: table.rows.len() > limit,
    };
    emit(w, &doc)?;
    Ok(fmt.lossy)
}

/// Longest type label shown in the text table; longer ones are cut with `…`
/// (the JSON output always has the full extension).
const MAX_LABEL_CHARS: usize = 32;

fn label_of(extension: &Option<String>) -> String {
    match extension {
        None => "(no extension)".to_string(),
        Some(ext) if ext.chars().count() > MAX_LABEL_CHARS => {
            let cut: String = ext.chars().take(MAX_LABEL_CHARS - 1).collect();
            format!("{cut}…")
        }
        Some(ext) => ext.clone(),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Renders the `--by-type` text report, ending with a newline.
///
/// The `top` biggest types get a row each; the rest are folded into one
/// `(N other types)` row, so the shares always add up to the total. With
/// `summarize` only a one-line summary is produced. Sizes are plain text so
/// the columns line up on a terminal.
pub fn render_by_type_text(table: &TypeTable, top: usize, summarize: bool) -> String {
    if summarize {
        return format!(
            "{} file{} in {} type{}, {} total\n",
            table.total_files,
            plural(table.total_files as usize),
            table.rows.len(),
            plural(table.rows.len()),
            format_size_plain(table.total_bytes),
        );
    }

    let shown = &table.rows[..table.rows.len().min(top)];
    let rest = &table.rows[shown.len()..];

    // (label, files, size, share) per line.
    let share = |bytes: u64| format!("{:.1}%", table.share_percent(bytes));
    let mut lines: Vec<(String, String, String, String)> = shown
        .iter()
        .map(|r| {
            (
                label_of(&r.extension),
                r.files.to_string(),
                format_size_plain(r.bytes),
                share(r.bytes),
            )
        })
        .collect();
    if !rest.is_empty() {
        let files: u64 = rest.iter().map(|r| r.files).sum();
        let bytes: u64 = rest.iter().map(|r| r.bytes).sum();
        lines.push((
            format!("({} other type{})", rest.len(), plural(rest.len())),
            files.to_string(),
            format_size_plain(bytes),
            share(bytes),
        ));
    }
    let total = (
        "Total".to_string(),
        table.total_files.to_string(),
        format_size_plain(table.total_bytes),
        share(table.total_bytes),
    );

    let width = |pick: fn(&(String, String, String, String)) -> usize, head: &str| {
        lines
            .iter()
            .chain(std::iter::once(&total))
            .map(pick)
            .max()
            .unwrap_or(0)
            .max(head.chars().count())
    };
    let w_label = width(|l| l.0.chars().count(), "Extension");
    let w_files = width(|l| l.1.len(), "Files");
    let w_size = width(|l| l.2.len(), "Size");
    let w_share = width(|l| l.3.len(), "Share");

    let row = |l: &(String, String, String, String)| {
        format!(
            "{:<w_label$}  {:>w_files$}  {:>w_size$}  {:>w_share$}\n",
            l.0, l.1, l.2, l.3
        )
    };
    let rule = "-".repeat(w_label + w_files + w_size + w_share + 6);

    let mut out = row(&(
        "Extension".to_string(),
        "Files".to_string(),
        "Size".to_string(),
        "Share".to_string(),
    ));
    out.push_str(&rule);
    out.push('\n');
    for l in &lines {
        out.push_str(&row(l));
    }
    out.push_str(&rule);
    out.push('\n');
    out.push_str(&row(&total));
    out
}
