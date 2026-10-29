//! `--json`: results as NDJSON, one JSON object per line.
//!
//! Event names and field names follow `rg --json` where they overlap, but
//! the fields are flat (no `data` wrapper) so `jq` filters stay short:
//!
//! ```text
//! {"type":"begin","path":"src/main.rs"}
//! {"type":"match","path":"src/main.rs","line_number":12,"lines":{"text":"    // TODO: fix"},
//!  "submatches":[{"start":7,"end":11,"match":{"text":"TODO"}}]}
//! {"type":"end","path":"src/main.rs","matched_lines":1}
//! {"type":"summary","schema_version":1,"mode":"content","matched_lines":1,...}
//! ```
//!
//! The schema is versioned by `schema_version` in the summary; the README
//! documents every event. Text is exactly what was read (invalid UTF-8
//! becomes U+FFFD), with control characters escaped by JSON itself, never
//! terminal-escaped. Offsets are byte offsets into `lines.text`.

use argrep::MatchResult;
use regex::Regex;
use serde::Serialize;
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

/// Bumped on any incompatible change to the events below.
pub const SCHEMA_VERSION: u32 = 1;

/// What the run lists, reported as `mode` in the summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Matching lines (the default), with `-A/-B/-C` context.
    Content,
    /// `-c`: per-file counts, as `begin`/`end` pairs without `match` events.
    Count,
    /// `-l`: `file` events for files with a match.
    FilesWithMatches,
    /// `-L`: `file` events for files without a match.
    FilesWithoutMatch,
    /// `--files`: `file` events for every file that would be searched.
    Files,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Content => "content",
            Mode::Count => "count",
            Mode::FilesWithMatches => "files_with_matches",
            Mode::FilesWithoutMatch => "files_without_match",
            Mode::Files => "files",
        }
    }
}

/// `{"text": ...}`, the shape rg uses for any piece of text.
#[derive(Serialize)]
struct Text<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct Submatch<'a> {
    start: usize,
    end: usize,
    #[serde(rename = "match")]
    matched: Text<'a>,
    /// With -r: what this match is replaced with.
    #[serde(skip_serializing_if = "Option::is_none")]
    replacement: Option<OwnedText>,
}

#[derive(Serialize)]
struct OwnedText {
    text: String,
}

/// One line of output. `type` comes first, as in rg.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event<'a> {
    Begin {
        path: &'a str,
    },
    Match {
        path: &'a str,
        line_number: usize,
        lines: Text<'a>,
        submatches: Vec<Submatch<'a>>,
    },
    Context {
        path: &'a str,
        line_number: usize,
        lines: Text<'a>,
        submatches: Vec<Submatch<'a>>,
    },
    End {
        path: &'a str,
        matched_lines: usize,
    },
    File {
        path: &'a str,
    },
    Summary {
        schema_version: u32,
        mode: &'static str,
        matched_lines: usize,
        /// `null` for -L and --files, which never list files with a match.
        files_with_matches: Option<usize>,
        files_searched: usize,
        bytes_read: usize,
        errors: usize,
        elapsed_ms: u128,
        /// Files skipped by --max-filesize; only present with that flag.
        #[serde(skip_serializing_if = "Option::is_none")]
        files_skipped_size: Option<usize>,
    },
}

/// Totals for the summary event, filled in by the caller from its stats.
pub struct Totals {
    pub matched_lines: usize,
    pub files_searched: usize,
    pub bytes_read: usize,
    pub errors: usize,
    pub elapsed_ms: u128,
    /// Files skipped by --max-filesize, or `None` without that flag.
    pub files_skipped_size: Option<usize>,
}

/// Turns search results into JSON events.
///
/// Results for one file must arrive together (the search delivers them as
/// one block in JSON mode); a change of path closes the previous file.
pub struct JsonPrinter<W: Write> {
    out: W,
    mode: Mode,
    regex: Regex,
    replacement: Option<String>,
    /// The file whose `begin` was written, and its matched lines so far.
    open: Option<(PathBuf, usize)>,
    files_with_matches: usize,
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl<W: Write> JsonPrinter<W> {
    pub fn new(out: W, mode: Mode, regex: Regex, replacement: Option<String>) -> Self {
        JsonPrinter {
            out,
            mode,
            regex,
            replacement,
            open: None,
            files_with_matches: 0,
        }
    }

    fn write(&mut self, event: &Event<'_>) {
        // A closed pipe (e.g. `| head`) ends the search quietly, as for
        // text output; see arcommon::out.
        arcommon::out::check(
            serde_json::to_writer(&mut self.out, event)
                .map_err(io::Error::from)
                .and_then(|()| self.out.write_all(b"\n")),
        );
    }

    /// Closes the open file, if any, with its `end` event.
    fn close_file(&mut self) {
        if let Some((path, matched)) = self.open.take() {
            if matched > 0 {
                self.files_with_matches += 1;
            }
            self.write(&Event::End {
                path: &path_text(&path),
                matched_lines: matched,
            });
        }
    }

    /// Makes `path` the open file, writing `end` for the previous one and
    /// `begin` for this one when it changes.
    fn open_file(&mut self, path: &Path) {
        if self.open.as_ref().is_some_and(|(p, _)| p == path) {
            return;
        }
        self.close_file();
        self.write(&Event::Begin {
            path: &path_text(path),
        });
        self.open = Some((path.to_path_buf(), 0));
    }

    /// Every match of the pattern in `text`, with its replacement under -r.
    fn submatches<'t>(
        regex: &Regex,
        replacement: Option<&str>,
        text: &'t str,
    ) -> Vec<Submatch<'t>> {
        regex
            .captures_iter(text)
            .map(|caps| {
                let m = caps.get(0).expect("group 0 always participates");
                Submatch {
                    start: m.start(),
                    end: m.end(),
                    matched: Text { text: m.as_str() },
                    replacement: replacement.map(|r| {
                        let mut replaced = String::new();
                        caps.expand(r, &mut replaced);
                        OwnedText { text: replaced }
                    }),
                }
            })
            .collect()
    }

    /// Handles one search result.
    pub fn result(&mut self, r: &MatchResult) {
        if r.is_separator {
            return; // "--" between context groups means nothing here
        }
        match self.mode {
            Mode::FilesWithMatches | Mode::FilesWithoutMatch | Mode::Files => {
                if self.mode == Mode::FilesWithMatches {
                    self.files_with_matches += 1;
                }
                self.write(&Event::File {
                    path: &path_text(&r.file_path),
                });
            }
            Mode::Count => {
                self.open_file(&r.file_path);
                if let Some((_, matched)) = &mut self.open {
                    *matched = r.count.unwrap_or(0);
                }
                self.close_file();
            }
            Mode::Content => {
                self.open_file(&r.file_path);
                if !r.is_context
                    && let Some((_, matched)) = &mut self.open
                {
                    *matched += 1;
                }
                let path = path_text(&r.file_path);
                let submatches =
                    Self::submatches(&self.regex, self.replacement.as_deref(), &r.line_content);
                let lines = Text {
                    text: &r.line_content,
                };
                let event = if r.is_context {
                    Event::Context {
                        path: &path,
                        line_number: r.line_num,
                        lines,
                        submatches,
                    }
                } else {
                    Event::Match {
                        path: &path,
                        line_number: r.line_num,
                        lines,
                        submatches,
                    }
                };
                self.write(&event);
            }
        }
    }

    /// Closes the last file and writes the summary. Call once at the end.
    pub fn finish(&mut self, totals: Totals) {
        self.close_file();
        let files_with_matches = match self.mode {
            // Files with a match are never listed by -L or --files.
            Mode::FilesWithoutMatch | Mode::Files => None,
            _ => Some(self.files_with_matches),
        };
        self.write(&Event::Summary {
            schema_version: SCHEMA_VERSION,
            mode: self.mode.name(),
            matched_lines: totals.matched_lines,
            files_with_matches,
            files_searched: totals.files_searched,
            bytes_read: totals.bytes_read,
            errors: totals.errors,
            elapsed_ms: totals.elapsed_ms,
            files_skipped_size: totals.files_skipped_size,
        });
        arcommon::out::check(self.out.flush());
    }
}

impl JsonPrinter<io::Stdout> {
    pub fn stdout(mode: Mode, regex: Regex, replacement: Option<String>) -> Self {
        Self::new(io::stdout(), mode, regex, replacement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn result(path: &str, line: usize, text: &str, context: bool) -> MatchResult {
        MatchResult {
            file_path: PathBuf::from(path),
            line_num: line,
            line_content: text.to_string(),
            count: None,
            is_context: context,
            is_separator: false,
            highlights: None,
        }
    }

    fn events(mode: Mode, replacement: Option<&str>, results: &[MatchResult]) -> Vec<Value> {
        let mut buf = Vec::new();
        {
            let mut p = JsonPrinter::new(
                &mut buf,
                mode,
                Regex::new("TODO").unwrap(),
                replacement.map(str::to_string),
            );
            for r in results {
                p.result(r);
            }
            p.finish(Totals {
                matched_lines: 2,
                files_searched: 3,
                bytes_read: 100,
                errors: 0,
                elapsed_ms: 5,
                files_skipped_size: None,
            });
        }
        String::from_utf8(buf)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line is one JSON value"))
            .collect()
    }

    #[test]
    fn content_events_group_by_file() {
        let ev = events(
            Mode::Content,
            None,
            &[
                result("a.rs", 1, "x", true),
                result("a.rs", 2, "// TODO: one TODO", false),
                result("b.rs", 9, "TODO", false),
            ],
        );
        let types: Vec<&str> = ev.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "begin", "context", "match", "end", "begin", "match", "end", "summary"
            ]
        );
        assert_eq!(ev[2]["line_number"], 2);
        assert_eq!(ev[2]["lines"]["text"], "// TODO: one TODO");
        assert_eq!(
            ev[2]["submatches"],
            json!([
                {"start": 3, "end": 7, "match": {"text": "TODO"}},
                {"start": 13, "end": 17, "match": {"text": "TODO"}}
            ])
        );
        assert_eq!(
            ev[3],
            json!({"type": "end", "path": "a.rs", "matched_lines": 1})
        );
        let summary = &ev[7];
        assert_eq!(summary["schema_version"], 1);
        assert_eq!(summary["mode"], "content");
        assert_eq!(summary["files_with_matches"], 2);
        assert_eq!(summary["matched_lines"], 2);
    }

    #[test]
    fn replacement_is_reported_per_submatch_and_text_stays_original() {
        let ev = events(
            Mode::Content,
            Some("DONE"),
            &[result("a", 1, "a TODO", false)],
        );
        assert_eq!(ev[1]["lines"]["text"], "a TODO");
        assert_eq!(ev[1]["submatches"][0]["replacement"]["text"], "DONE");
    }

    #[test]
    fn type_is_the_first_field() {
        let mut buf = Vec::new();
        let mut p = JsonPrinter::new(&mut buf, Mode::Content, Regex::new("x").unwrap(), None);
        p.result(&result("a", 1, "x", false));
        drop(p);
        for line in String::from_utf8(buf).unwrap().lines() {
            assert!(line.starts_with(r#"{"type":""#), "{line}");
        }
    }

    #[test]
    fn control_characters_are_json_escaped_not_terminal_escaped() {
        let mut buf = Vec::new();
        let mut p = JsonPrinter::new(&mut buf, Mode::Content, Regex::new("x").unwrap(), None);
        p.result(&result("e\u{1b}[2J", 1, "x\u{7}", false));
        drop(p);
        let out = String::from_utf8(buf).unwrap();
        assert!(!out.contains('\u{1b}') && !out.contains('\u{7}'), "{out:?}");
        assert!(
            out.contains(r"e\u001b[2J") && out.contains(r"x\u0007"),
            "{out}"
        );
    }

    #[test]
    fn file_list_modes_emit_file_events() {
        let r = result("a.rs", 0, "", false);
        let ev = events(Mode::FilesWithMatches, None, std::slice::from_ref(&r));
        assert_eq!(ev[0], json!({"type": "file", "path": "a.rs"}));
        assert_eq!(ev[1]["files_with_matches"], 1);
        let ev = events(Mode::FilesWithoutMatch, None, &[r]);
        assert_eq!(ev[1]["mode"], "files_without_match");
        assert!(ev[1]["files_with_matches"].is_null());
    }

    #[test]
    fn count_mode_reports_counts_in_end_events() {
        let mut r = result("a.rs", 0, "", false);
        r.count = Some(4);
        let mut zero = result("b.rs", 0, "", false);
        zero.count = Some(0);
        let ev = events(Mode::Count, None, &[r, zero]);
        assert_eq!(
            ev[1],
            json!({"type": "end", "path": "a.rs", "matched_lines": 4})
        );
        assert_eq!(
            ev[3],
            json!({"type": "end", "path": "b.rs", "matched_lines": 0})
        );
        assert_eq!(ev[4]["files_with_matches"], 1);
    }
}
