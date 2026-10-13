//! `--exec` / `--exec-batch`: run a command on the search results.
//!
//! Safety properties, all enforced here:
//!
//! * **No shell.** The command is started directly (`execve`), with every
//!   argument passed as its own string. Nothing in a file name (spaces,
//!   quotes, `;`, `$(...)`, newlines) is ever interpreted.
//! * **No option injection.** arfind reports absolute paths, so a result
//!   always starts with `/` and can never be mistaken for an option such as
//!   `-rf`. [`ExecTemplate`] relies on that and debug-asserts it.
//! * **Never runs what it found.** A placeholder in the program position
//!   (`--exec {}`) is rejected: arfind runs a command *on* files, not the
//!   files themselves.
//! * **Snapshot first.** The caller collects and sorts all results before
//!   anything runs (see `main.rs`), so a command that moves or deletes
//!   files cannot change the tree while it is still being searched.
//! * **Nothing to do, nothing run.** Batch mode with no results starts no
//!   process, unlike plain `xargs`, which would run `rm` with no arguments.

use std::{
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    process::Command,
};

/// What a placeholder expands to. The syntax follows `fd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placeholder {
    /// `{}`: the full path.
    Path,
    /// `{/}`: the file name.
    Basename,
    /// `{//}`: the parent directory.
    Parent,
    /// `{.}`: the path without its extension.
    NoExt,
    /// `{/.}`: the file name without its extension.
    BasenameNoExt,
}

const PLACEHOLDERS: &[(&str, Placeholder)] = &[
    // Longest first, so "{//}" is not read as "{/}" followed by "/}".
    ("{//}", Placeholder::Parent),
    ("{/.}", Placeholder::BasenameNoExt),
    ("{/}", Placeholder::Basename),
    ("{.}", Placeholder::NoExt),
    ("{}", Placeholder::Path),
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    Hole(Placeholder),
}

/// One argument of the command line, split into text and placeholders.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArgTemplate(Vec<Piece>);

impl ArgTemplate {
    fn parse(arg: &str) -> Self {
        let mut pieces = Vec::new();
        let mut text = String::new();
        let mut rest = arg;
        'outer: while !rest.is_empty() {
            for (token, ph) in PLACEHOLDERS {
                if let Some(after) = rest.strip_prefix(token) {
                    if !text.is_empty() {
                        pieces.push(Piece::Text(std::mem::take(&mut text)));
                    }
                    pieces.push(Piece::Hole(*ph));
                    rest = after;
                    continue 'outer;
                }
            }
            let c = rest.chars().next().expect("rest is not empty");
            text.push(c);
            rest = &rest[c.len_utf8()..];
        }
        if !text.is_empty() {
            pieces.push(Piece::Text(text));
        }
        ArgTemplate(pieces)
    }

    fn has_placeholder(&self) -> bool {
        self.0.iter().any(|p| matches!(p, Piece::Hole(_)))
    }

    /// The argument is exactly `{}`.
    fn is_bare_path(&self) -> bool {
        self.0 == [Piece::Hole(Placeholder::Path)]
    }

    fn expand(&self, path: &Path) -> OsString {
        let mut out = OsString::new();
        for piece in &self.0 {
            match piece {
                Piece::Text(t) => out.push(t),
                Piece::Hole(ph) => out.push(expand_placeholder(*ph, path)),
            }
        }
        out
    }
}

fn expand_placeholder(ph: Placeholder, path: &Path) -> OsString {
    let name = || path.file_name().unwrap_or(path.as_os_str());
    match ph {
        Placeholder::Path => path.as_os_str().to_owned(),
        Placeholder::Basename => name().to_owned(),
        Placeholder::Parent => path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("/"))
            .as_os_str()
            .to_owned(),
        Placeholder::NoExt => {
            if path.extension().is_some() {
                path.with_extension("").into_os_string()
            } else {
                path.as_os_str().to_owned()
            }
        }
        Placeholder::BasenameNoExt => Path::new(name())
            .file_stem()
            .unwrap_or_else(|| name())
            .to_owned(),
    }
}

/// A parsed `--exec` / `--exec-batch` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecTemplate {
    program: String,
    args: Vec<ArgTemplate>,
    batch: bool,
    /// No placeholder anywhere: the path(s) go after the last argument.
    append: bool,
}

impl ExecTemplate {
    /// Parses the words after `--exec` (`batch = false`) or `--exec-batch`
    /// (`batch = true`).
    pub fn parse(words: &[String], batch: bool) -> Result<Self, String> {
        let flag = if batch { "--exec-batch" } else { "--exec" };
        let (program, rest) = words
            .split_first()
            .ok_or_else(|| format!("{flag} needs a command"))?;
        if program.trim().is_empty() {
            return Err(format!("{flag} needs a command"));
        }
        if ArgTemplate::parse(program).has_placeholder() {
            return Err(format!(
                "{flag}: the command itself cannot be a placeholder ('{program}'); \
                 arfind runs a command on the files it finds, never the files themselves"
            ));
        }
        let args: Vec<ArgTemplate> = rest.iter().map(|a| ArgTemplate::parse(a)).collect();
        if batch {
            if let Some(bad) = args
                .iter()
                .find(|a| a.has_placeholder() && !a.is_bare_path())
            {
                let shown: Vec<&str> = rest
                    .iter()
                    .zip(&args)
                    .filter(|(_, a)| *a == bad)
                    .map(|(s, _)| s.as_str())
                    .collect();
                return Err(format!(
                    "--exec-batch only supports '{{}}' as a whole argument (got '{}'); \
                     use --exec for per-file placeholders such as {{/}} or {{.}}",
                    shown.first().copied().unwrap_or("")
                ));
            }
            if args.iter().filter(|a| a.is_bare_path()).count() > 1 {
                return Err("--exec-batch: '{}' may appear only once".to_string());
            }
        }
        let append = !args.iter().any(ArgTemplate::has_placeholder);
        Ok(ExecTemplate {
            program: program.clone(),
            args,
            batch,
            append,
        })
    }

    pub fn is_batch(&self) -> bool {
        self.batch
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    /// The argument list for one path (per-file mode).
    pub fn args_for(&self, path: &Path) -> Vec<OsString> {
        debug_assert!(path.is_absolute(), "results must be absolute paths");
        let mut out: Vec<OsString> = self.args.iter().map(|a| a.expand(path)).collect();
        if self.append {
            out.push(path.as_os_str().to_owned());
        }
        out
    }

    /// The argument lists for batch mode: all paths, split into as many
    /// invocations as needed to stay under `max_bytes` of arguments each
    /// (well below the system's ARG_MAX). Empty for no paths.
    pub fn batches_for(&self, paths: &[PathBuf], max_bytes: usize) -> Vec<Vec<OsString>> {
        if paths.is_empty() {
            return Vec::new();
        }
        let fixed: Vec<&ArgTemplate> = self.args.iter().collect();
        let fixed_bytes: usize = self.program.len()
            + 1
            + self
                .args
                .iter()
                .filter(|a| !a.is_bare_path())
                .map(|a| a.expand(Path::new("/")).len() + 1)
                .sum::<usize>();

        let mut chunks: Vec<&[PathBuf]> = Vec::new();
        let mut start = 0;
        let mut bytes = fixed_bytes;
        for (i, p) in paths.iter().enumerate() {
            let len = p.as_os_str().len() + 1;
            if i > start && bytes + len > max_bytes {
                chunks.push(&paths[start..i]);
                start = i;
                bytes = fixed_bytes;
            }
            bytes += len;
        }
        chunks.push(&paths[start..]);

        chunks
            .into_iter()
            .map(|chunk| {
                let mut out = Vec::new();
                let mut placed = false;
                for a in &fixed {
                    if a.is_bare_path() {
                        out.extend(chunk.iter().map(|p| p.as_os_str().to_owned()));
                        placed = true;
                    } else {
                        // Only literal text can reach here in batch mode.
                        out.push(a.expand(Path::new("/")));
                    }
                }
                if !placed {
                    out.extend(chunk.iter().map(|p| p.as_os_str().to_owned()));
                }
                out
            })
            .collect()
    }
}

/// Words of an `--exec` command that are spelled exactly like one of
/// arfind's own long options (`--name`, `--dry-run`, `--type=rust`, ...).
///
/// Everything after `--exec` belongs to the command, so
/// `arfind . --exec rm --dry-run` would pass `--dry-run` to `rm` and delete
/// for real. When the command was not explicitly ended with `;`, the caller
/// refuses to run if this returns anything.
pub fn misplaced_options(words: &[String], arfind_long_options: &[String]) -> Vec<String> {
    words
        .iter()
        .skip(1) // the program itself
        .filter(|w| {
            let flag = w.split_once('=').map_or(w.as_str(), |(f, _)| f);
            flag.starts_with("--") && arfind_long_options.iter().any(|o| o == flag)
        })
        .cloned()
        .collect()
}

/// Whether the command after the first `--exec` / `--exec-batch` / `-X` in
/// the raw command line was ended with a `;` argument.
pub fn command_terminated(raw_args: &[String]) -> bool {
    raw_args
        .iter()
        .skip_while(|a| !matches!(a.as_str(), "--exec" | "--exec-batch" | "-X"))
        .skip(1)
        .any(|a| a == ";")
}

/// Quotes one word for display so a `--dry-run` line can be pasted into a
/// POSIX shell. Plain words stay as they are.
pub fn shell_quote(word: &OsStr) -> String {
    let s = word.to_string_lossy();
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:,@%".contains(c));
    if safe {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// A full command line for display.
pub fn display_command(program: &str, args: &[OsString]) -> String {
    let mut line = shell_quote(OsStr::new(program));
    for a in args {
        line.push(' ');
        line.push_str(&shell_quote(a));
    }
    line
}

/// Outcome of running the commands.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExecSummary {
    /// Commands started (or, with `--dry-run`, printed).
    pub runs: usize,
    /// Commands that exited unsuccessfully or were killed by a signal.
    pub failed: usize,
}

/// Why execution stopped early.
#[derive(Debug)]
pub enum ExecError {
    /// The program could not be started at all (e.g. not found); nothing
    /// else will work either, so arfind stops at the first attempt.
    Spawn { program: String, error: io::Error },
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Spawn { program, error } => {
                write!(f, "cannot run '{program}': {error}")
            }
        }
    }
}

/// Runs (or, with `dry_run`, prints to `out`) the command for `paths`,
/// one at a time and in the given order. Each command inherits arfind's
/// stdin, stdout and stderr, so interactive programs such as editors work.
pub fn run(
    template: &ExecTemplate,
    paths: &[PathBuf],
    dry_run: bool,
    out: &mut dyn io::Write,
) -> Result<ExecSummary, ExecError> {
    let invocations: Vec<Vec<OsString>> = if template.batch {
        template.batches_for(paths, BATCH_MAX_BYTES)
    } else {
        paths.iter().map(|p| template.args_for(p)).collect()
    };

    let mut summary = ExecSummary::default();
    for args in invocations {
        summary.runs += 1;
        if dry_run {
            let _ = writeln!(out, "{}", display_command(&template.program, &args));
            continue;
        }
        match Command::new(&template.program).args(&args).status() {
            Ok(status) if status.success() => {}
            Ok(_) => summary.failed += 1,
            Err(error) => {
                return Err(ExecError::Spawn {
                    program: template.program.clone(),
                    error,
                });
            }
        }
    }
    Ok(summary)
}

/// Argument bytes per batch invocation: far below ARG_MAX on Linux (2 MiB)
/// and macOS (1 MiB), leaving room for the environment.
pub const BATCH_MAX_BYTES: usize = 128 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &[&str]) -> Vec<String> {
        s.iter().map(|w| w.to_string()).collect()
    }

    fn strs(v: &[OsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn path_is_appended_when_there_is_no_placeholder() {
        let t = ExecTemplate::parse(&words(&["wc", "-l"]), false).unwrap();
        assert_eq!(strs(&t.args_for(Path::new("/a/b.rs"))), ["-l", "/a/b.rs"]);
    }

    #[test]
    fn placeholders_follow_fd() {
        let t = ExecTemplate::parse(
            &words(&["echo", "{}", "{/}", "{//}", "{.}", "{/.}", "pre-{/.}.bak"]),
            false,
        )
        .unwrap();
        assert_eq!(
            strs(&t.args_for(Path::new("/src/dir/file.tar.gz"))),
            [
                "/src/dir/file.tar.gz",
                "file.tar.gz",
                "/src/dir",
                "/src/dir/file.tar",
                "file.tar",
                "pre-file.tar.bak"
            ]
        );
        assert_eq!(
            strs(&t.args_for(Path::new("/Makefile"))),
            [
                "/Makefile",
                "Makefile",
                "/",
                "/Makefile",
                "Makefile",
                "pre-Makefile.bak"
            ]
        );
    }

    #[test]
    fn hostile_file_names_stay_single_arguments() {
        let t = ExecTemplate::parse(&words(&["rm"]), false).unwrap();
        for name in [
            "/d/x; rm -rf ~",
            "/d/$(touch pwned)",
            "/d/a b\nc",
            "/d/'q'\"q\"",
            "/d/-rf",
        ] {
            let args = t.args_for(Path::new(name));
            assert_eq!(args.len(), 1);
            assert_eq!(args[0], OsStr::new(name));
        }
    }

    #[test]
    fn the_program_cannot_be_a_placeholder() {
        for bad in [&["{}"][..], &["{/}"], &["./{}"], &["{.}.sh"]] {
            assert!(ExecTemplate::parse(&words(bad), false).is_err(), "{bad:?}");
            assert!(ExecTemplate::parse(&words(bad), true).is_err(), "{bad:?}");
        }
        assert!(ExecTemplate::parse(&[], false).is_err());
        assert!(ExecTemplate::parse(&words(&[" "]), false).is_err());
    }

    #[test]
    fn batch_puts_all_paths_in_place_of_braces_or_at_the_end() {
        let paths: Vec<PathBuf> = ["/a", "/b", "/c"].iter().map(PathBuf::from).collect();
        let t = ExecTemplate::parse(&words(&["tar", "czf", "out.tgz"]), true).unwrap();
        assert_eq!(
            t.batches_for(&paths, BATCH_MAX_BYTES)
                .iter()
                .map(|b| strs(b))
                .collect::<Vec<_>>(),
            [vec!["czf", "out.tgz", "/a", "/b", "/c"]]
        );
        let t = ExecTemplate::parse(&words(&["cp", "{}", "/backup"]), true).unwrap();
        assert_eq!(
            strs(&t.batches_for(&paths, BATCH_MAX_BYTES)[0]),
            ["/a", "/b", "/c", "/backup"]
        );
    }

    #[test]
    fn batch_rejects_per_file_placeholders() {
        assert!(ExecTemplate::parse(&words(&["echo", "{/}"]), true).is_err());
        assert!(ExecTemplate::parse(&words(&["echo", "x{}"]), true).is_err());
        assert!(ExecTemplate::parse(&words(&["echo", "{}", "{}"]), true).is_err());
    }

    #[test]
    fn batch_with_no_paths_runs_nothing() {
        let t = ExecTemplate::parse(&words(&["rm"]), true).unwrap();
        assert!(t.batches_for(&[], BATCH_MAX_BYTES).is_empty());
        let mut out = Vec::new();
        let s = run(&t, &[], false, &mut out).unwrap();
        assert_eq!(s, ExecSummary::default());
    }

    #[test]
    fn batches_split_under_the_byte_limit_and_keep_order() {
        let paths: Vec<PathBuf> = (0..100)
            .map(|i| PathBuf::from(format!("/f{i:03}")))
            .collect();
        let t = ExecTemplate::parse(&words(&["x"]), true).unwrap();
        let batches = t.batches_for(&paths, 60);
        assert!(batches.len() > 1);
        let flat: Vec<String> = batches.iter().flat_map(|b| strs(b)).collect();
        let want: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(flat, want);
        for b in &batches {
            let bytes: usize = 2 + b.iter().map(|a| a.len() + 1).sum::<usize>();
            assert!(bytes <= 60 || b.len() == 1, "{bytes}");
        }
    }

    #[test]
    fn arfind_options_inside_the_command_are_detected() {
        let known: Vec<String> = ["--dry-run", "--name", "--type"].map(String::from).to_vec();
        assert_eq!(
            misplaced_options(&words(&["rm", "-f", "--dry-run"]), &known),
            ["--dry-run"]
        );
        assert_eq!(
            misplaced_options(&words(&["rm", "--type=rust", "--name", "x"]), &known),
            ["--type=rust", "--name"]
        );
        // Short options and unknown long ones belong to the command.
        assert!(misplaced_options(&words(&["wc", "-l", "--lines"]), &known).is_empty());
        // The program position is not checked.
        assert!(misplaced_options(&words(&["--name"]), &known).is_empty());
    }

    #[test]
    fn terminator_detection() {
        let raw = |v: &[&str]| words(v);
        assert!(command_terminated(&raw(&[
            "arfind",
            ".",
            "--exec",
            "rsync",
            "--dry-run",
            ";"
        ])));
        assert!(command_terminated(&raw(&[
            "arfind", "-X", "tar", "cf", "x", ";", "-t", "f"
        ])));
        assert!(!command_terminated(&raw(&[
            "arfind",
            ".",
            "--exec",
            "rm",
            "--dry-run"
        ])));
        assert!(!command_terminated(&raw(&["arfind", ";", "--exec", "rm"])));
    }

    #[test]
    fn shell_quoting_for_dry_run() {
        assert_eq!(shell_quote(OsStr::new("/a/b.rs")), "/a/b.rs");
        assert_eq!(shell_quote(OsStr::new("a b")), "'a b'");
        assert_eq!(shell_quote(OsStr::new("it's")), r"'it'\''s'");
        assert_eq!(shell_quote(OsStr::new("$(x);")), "'$(x);'");
        assert_eq!(shell_quote(OsStr::new("")), "''");
        assert_eq!(
            display_command("wc", &[OsString::from("-l"), OsString::from("/a b")]),
            "wc -l '/a b'"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_counts_failures_and_reports_missing_programs() {
        let paths = [PathBuf::from("/"), PathBuf::from("/definitely/missing")];
        let t = ExecTemplate::parse(&words(&["test", "-e"]), false).unwrap();
        let s = run(&t, &paths, false, &mut Vec::new()).unwrap();
        assert_eq!(s, ExecSummary { runs: 2, failed: 1 });

        let t = ExecTemplate::parse(&words(&["arfind-no-such-program-xyz"]), false).unwrap();
        assert!(matches!(
            run(&t, &paths, false, &mut Vec::new()),
            Err(ExecError::Spawn { .. })
        ));
    }

    #[test]
    fn dry_run_prints_and_runs_nothing() {
        let t = ExecTemplate::parse(&words(&["arfind-no-such-program-xyz", "-v"]), false).unwrap();
        let mut out = Vec::new();
        let s = run(
            &t,
            &[PathBuf::from("/a b"), PathBuf::from("/c")],
            true,
            &mut out,
        )
        .unwrap();
        assert_eq!(s, ExecSummary { runs: 2, failed: 0 });
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "arfind-no-such-program-xyz -v '/a b'\narfind-no-such-program-xyz -v /c\n"
        );
    }
}
