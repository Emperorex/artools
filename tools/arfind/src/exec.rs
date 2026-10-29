//! `--exec` / `--exec-batch`: run a command on the search results.
//!
//! Safety properties, all enforced here:
//!
//! * **No shell.** The command is started directly (`execve`), with every
//!   argument passed as its own string. Nothing in a file name (spaces,
//!   quotes, `;`, `$(...)`, newlines) is ever interpreted.
//! * **No option injection.** arfind reports absolute paths, so `{}`,
//!   `{//}` and `{.}` always start with `/` and can never be mistaken for an
//!   option such as `-rf` ([`ExecTemplate::args_for`] debug-asserts this).
//!   `{/}` and `{/.}` are bare file names and can start with `-`, so when
//!   one of them begins an argument and its expansion starts with `-`, it
//!   is written as `./-rf`, the same way `find -execdir` does. The command
//!   then sees a relative path, never an option.
//! * **Never runs what it found.** A placeholder in the program position
//!   (`--exec {}`) is rejected: arfind runs a command *on* files, not the
//!   files themselves.
//! * **Snapshot first.** The caller collects and sorts all results before
//!   anything runs (see `main.rs`), so a command that moves or deletes
//!   files cannot change the tree while it is still being searched.
//! * **Nothing to do, nothing run.** Batch mode with no results starts no
//!   process, unlike plain `xargs`, which would run `rm` with no arguments.
//!
//! Known limit of `--exec` / `--exec-batch`: the classic `find -exec` race.
//! Paths are checked during the search and used later, so someone who can
//! write to a directory in the tree can swap a parent directory for a
//! symlink in between, and the command then acts on a file outside the
//! tree. This matters when a privileged user runs a command over a tree
//! that others can write to (e.g. root over `/tmp`). `--execdir` closes
//! that gap:
//!
//! * **`--execdir`** runs each command from the file's own directory and
//!   passes `./NAME`, like `find -execdir`. The directory is reopened
//!   component by component from `/` with `O_NOFOLLOW` (result paths come
//!   from a canonical root and never contain symlinks), so a directory
//!   swapped for a symlink makes arfind refuse that file instead of
//!   following the link; the command is then started inside that exact
//!   directory with `fchdir`. Because the working directory is
//!   attacker-writable in that scenario, the program is resolved to an
//!   absolute path before searching, and `PATH` entries that are relative
//!   (`.`, empty, `bin`) are refused, as `find -execdir` does.

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
        for (i, piece) in self.0.iter().enumerate() {
            match piece {
                Piece::Text(t) => out.push(t),
                Piece::Hole(ph) => {
                    let value = expand_placeholder(*ph, path);
                    // Only the start of an argument is read as an option. A
                    // file name there that starts with '-' gets "./" in
                    // front, so `{/}` of "--version" is "./--version".
                    if i == 0 && starts_with_dash(&value) {
                        out.push("./");
                    }
                    out.push(value);
                }
            }
        }
        out
    }
}

fn starts_with_dash(s: &OsStr) -> bool {
    s.as_encoded_bytes().first() == Some(&b'-')
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
    /// `--execdir`: run from each file's directory with `./NAME`.
    in_dir: bool,
    /// `--execdir`: the program resolved to an absolute path up front, so
    /// it can't be picked up from the (untrusted) working directory.
    resolved_program: Option<PathBuf>,
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
            in_dir: false,
            resolved_program: None,
        })
    }

    /// Parses the words after `--execdir`: like `--exec`, but each command
    /// runs from the file's own directory and gets `./NAME`. `path_env` is
    /// the `PATH` used to resolve a bare program name.
    pub fn parse_execdir(words: &[String], path_env: Option<&OsStr>) -> Result<Self, String> {
        let mut t = Self::parse(words, false).map_err(|e| e.replace("--exec", "--execdir"))?;
        t.resolved_program = Some(resolve_program(&t.program, path_env)?);
        t.in_dir = true;
        Ok(t)
    }

    pub fn is_execdir(&self) -> bool {
        self.in_dir
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
        // --execdir: placeholders see "./NAME", so {} is ./NAME, {/} NAME,
        // {//} ".", and so on; nothing ever names the directory again.
        let rel;
        let path = if self.in_dir {
            rel = split_for_execdir(path).1;
            rel.as_path()
        } else {
            path
        };
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

/// Whether the command after the first `--exec` / `--exec-batch` / `-X` /
/// `--execdir` in the raw command line was ended with a `;` argument.
pub fn command_terminated(raw_args: &[String]) -> bool {
    raw_args
        .iter()
        .skip_while(|a| !matches!(a.as_str(), "--exec" | "--exec-batch" | "-X" | "--execdir"))
        .skip(1)
        .any(|a| a == ";")
}

/// Quotes one word for display so a `--dry-run` line can be pasted into a
/// POSIX shell. Plain words stay as they are.
///
/// A word containing control characters (possible in file names) is
/// written in `$'...'` form with those characters as `\xHH` escapes, so
/// printing the line can never send raw escape sequences to the terminal,
/// and pasting it still reproduces the exact name (`$'...'` is in POSIX.1-2024
/// and supported by bash, zsh and ksh).
pub fn shell_quote(word: &OsStr) -> String {
    let s = word.to_string_lossy();
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:,@%".contains(c));
    if safe {
        return s.into_owned();
    }
    if s.chars()
        .any(|c| matches!(c as u32, 0x00..=0x1F | 0x7F..=0x9F))
    {
        let mut out = String::from("$'");
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\'' => out.push_str("\\'"),
                c if (c as u32) < 0x20 || c as u32 == 0x7F => {
                    out.push_str(&format!("\\x{:02x}", c as u32))
                }
                // C1: emit the UTF-8 bytes, which is what the name holds.
                c if matches!(c as u32, 0x80..=0x9F) => {
                    let mut buf = [0u8; 2];
                    for b in c.encode_utf8(&mut buf).bytes() {
                        out.push_str(&format!("\\x{b:02x}"));
                    }
                }
                c => out.push(c),
            }
        }
        out.push('\'');
        return out;
    }
    format!("'{}'", s.replace('\'', r"'\''"))
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

/// `--execdir`: the directory to run in and the `./NAME` to pass for a
/// result. A result without a file name (only `/` itself) runs in that
/// directory with `.`.
pub fn split_for_execdir(path: &Path) -> (PathBuf, PathBuf) {
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) if !dir.as_os_str().is_empty() => {
            (dir.to_path_buf(), Path::new(".").join(name))
        }
        _ => (path.to_path_buf(), PathBuf::from(".")),
    }
}

/// Resolves the `--execdir` program to an absolute path before anything
/// runs. A name with a `/` must already be absolute (a relative one would
/// be looked up in each file's directory). A bare name is searched in
/// `PATH`, and a `PATH` with a relative or empty entry is refused, since
/// the lookup would then reach into the directories being processed.
pub fn resolve_program(program: &str, path_env: Option<&OsStr>) -> Result<PathBuf, String> {
    let is_executable_file = |p: &Path| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            p.is_file()
        }
    };
    if program.contains('/') {
        let p = Path::new(program);
        if !p.is_absolute() {
            return Err(format!(
                "--execdir: '{program}' is a relative path, which would be looked up \
                 in each file's directory; give an absolute path or a command name"
            ));
        }
        return if is_executable_file(p) {
            Ok(p.to_path_buf())
        } else {
            Err(format!("--execdir: '{program}' is not an executable file"))
        };
    }
    let path_env = path_env.unwrap_or_default();
    let entries: Vec<PathBuf> = std::env::split_paths(path_env).collect();
    if let Some(bad) = entries.iter().find(|d| !d.is_absolute()) {
        let shown = if bad.as_os_str().is_empty() {
            "an empty entry".to_string()
        } else {
            format!("'{}'", bad.display())
        };
        return Err(format!(
            "--execdir: PATH contains {shown}, a relative directory; with --execdir \
             the command would be looked up in each file's directory. Remove it from \
             PATH or give the command as an absolute path"
        ));
    }
    entries
        .iter()
        .map(|d| d.join(program))
        .find(|c| is_executable_file(c))
        .ok_or_else(|| format!("--execdir: command '{program}' not found in PATH"))
}

/// Opens `dir` (absolute) one component at a time from `/`, refusing to
/// follow a symlink anywhere along the way. arfind's result paths are built
/// from a canonical root without following links, so a symlink here means
/// something was swapped after the search.
#[cfg(unix)]
pub fn open_dir_nofollow(dir: &Path) -> io::Result<std::os::fd::OwnedFd> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    let open = |at: Option<&OwnedFd>, name: &OsStr, nofollow: bool| -> io::Result<OwnedFd> {
        let c = CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
        let mut flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
        if nofollow {
            flags |= libc::O_NOFOLLOW;
        }
        // SAFETY: `c` is a valid NUL-terminated string and the directory fd,
        // when given, is open for the duration of the call.
        let fd = unsafe {
            match at {
                Some(d) => libc::openat(d.as_raw_fd(), c.as_ptr(), flags),
                None => libc::open(c.as_ptr(), flags),
            }
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: `fd` was just returned by open/openat and is owned here.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    };

    if !dir.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not an absolute path",
        ));
    }
    let mut cur = open(None, OsStr::new("/"), false)?;
    for comp in dir.components() {
        match comp {
            Component::RootDir => {}
            Component::Normal(name) => cur = open(Some(&cur), name, true)?,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path has '.' or '..' components",
                ));
            }
        }
    }
    Ok(cur)
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
    if template.in_dir {
        return run_in_dirs(template, paths, dry_run, out);
    }
    let invocations: Vec<Vec<OsString>> = if template.batch {
        template.batches_for(paths, BATCH_MAX_BYTES)
    } else {
        paths.iter().map(|p| template.args_for(p)).collect()
    };

    let mut summary = ExecSummary::default();
    for args in invocations {
        summary.runs += 1;
        if dry_run {
            arcommon::out::check(writeln!(
                out,
                "{}",
                display_command(&template.program, &args)
            ));
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

/// `--execdir`: one command per result, each started inside the result's
/// directory (opened without following symlinks) and given `./NAME`.
fn run_in_dirs(
    template: &ExecTemplate,
    paths: &[PathBuf],
    dry_run: bool,
    out: &mut dyn io::Write,
) -> Result<ExecSummary, ExecError> {
    let program = template
        .resolved_program
        .clone()
        .unwrap_or_else(|| PathBuf::from(&template.program));
    let mut summary = ExecSummary::default();
    for path in paths {
        let (dir, _) = split_for_execdir(path);
        let args = template.args_for(path);
        summary.runs += 1;
        if dry_run {
            arcommon::out::check(writeln!(
                out,
                "cd {} && {}",
                shell_quote(dir.as_os_str()),
                display_command(&template.program, &args)
            ));
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::process::CommandExt;
            let dir_fd = match open_dir_nofollow(&dir) {
                Ok(fd) => fd,
                Err(e) => {
                    let why = match e.raw_os_error() {
                        Some(libc::ELOOP) | Some(libc::ENOTDIR) => {
                            "a directory in its path is now a symlink or no longer a \
                             directory (changed since the search)"
                                .to_string()
                        }
                        _ => e.to_string(),
                    };
                    eprintln!(
                        "arfind: {}: not running the command: {why}",
                        arcommon::display_path(path)
                    );
                    summary.failed += 1;
                    continue;
                }
            };
            let raw = dir_fd.as_raw_fd();
            let mut cmd = Command::new(&program);
            cmd.args(&args);
            // SAFETY: fchdir is async-signal-safe, and `dir_fd` stays open
            // until the child has been started and waited for.
            unsafe {
                cmd.pre_exec(move || {
                    if libc::fchdir(raw) == 0 {
                        Ok(())
                    } else {
                        Err(io::Error::last_os_error())
                    }
                });
            }
            let status = cmd.status();
            drop(dir_fd);
            match status {
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
        #[cfg(not(unix))]
        {
            let _ = (&program, &args, &dir);
            return Err(ExecError::Spawn {
                program: template.program.clone(),
                error: io::Error::new(io::ErrorKind::Unsupported, "--execdir needs a Unix system"),
            });
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
    fn shell_quote_escapes_control_characters() {
        let q = shell_quote(OsStr::new("x\x1b]0;PWNED\x07\x1b[2J it's\\.txt"));
        assert_eq!(q, r"$'x\x1b]0;PWNED\x07\x1b[2J it\'s\\.txt'");
        assert!(!q.chars().any(|c| c.is_control()));
        assert_eq!(shell_quote(OsStr::new("a\u{9b}b")), r"$'a\xc2\x9bb'");
        // Ordinary words keep the old quoting.
        assert_eq!(shell_quote(OsStr::new("it's")), r"'it'\''s'");
        assert_eq!(shell_quote(OsStr::new("plain.txt")), "plain.txt");
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
    fn basename_placeholders_cannot_become_options() {
        let t = ExecTemplate::parse(&words(&["ls", "{/}", "{/.}", "x{/}", "--file={/}"]), false)
            .unwrap();
        assert_eq!(
            strs(&t.args_for(Path::new("/d/--version"))),
            [
                "./--version",
                "./--version",
                "x--version",
                "--file=--version"
            ]
        );
        assert_eq!(
            strs(&t.args_for(Path::new("/d/-rf.txt"))),
            ["./-rf.txt", "./-rf", "x-rf.txt", "--file=-rf.txt"]
        );
        assert_eq!(
            strs(&t.args_for(Path::new("/d/-"))),
            ["./-", "./-", "x-", "--file=-"]
        );
        // Names that don't start with '-' are left alone.
        assert_eq!(
            strs(&t.args_for(Path::new("/d/a-b.rs"))),
            ["a-b.rs", "a-b", "xa-b.rs", "--file=a-b.rs"]
        );
    }

    #[test]
    fn no_expanded_argument_starts_with_a_dash_unless_the_user_wrote_it() {
        let all = ["{}", "{/}", "{//}", "{.}", "{/.}", "{/}.bak", "{/.}{/}"];
        let t = ExecTemplate::parse(&words(&[&["echo"][..], &all].concat()), false).unwrap();
        for name in ["/d/-rf", "/d/--help", "/-x/-y.z", "/d/-.", "/d/--"] {
            for arg in t.args_for(Path::new(name)) {
                assert!(!starts_with_dash(&arg), "{name}: {arg:?}");
            }
        }
    }

    #[test]
    fn execdir_placeholders_name_only_the_file() {
        let mut t =
            ExecTemplate::parse(&words(&["echo", "{}", "{/}", "{//}", "{.}", "{/.}"]), false)
                .unwrap();
        t.in_dir = true;
        assert_eq!(
            strs(&t.args_for(Path::new("/src/dir/file.tar.gz"))),
            [
                "./file.tar.gz",
                "file.tar.gz",
                ".",
                "./file.tar",
                "file.tar"
            ]
        );
        // F4's rule still applies to {/} and {/.}.
        assert_eq!(
            strs(&t.args_for(Path::new("/d/-rf.txt"))),
            ["./-rf.txt", "./-rf.txt", ".", "./-rf", "./-rf"]
        );
        // Without placeholders, ./NAME is appended.
        let mut t = ExecTemplate::parse(&words(&["rm"]), false).unwrap();
        t.in_dir = true;
        assert_eq!(strs(&t.args_for(Path::new("/d/x"))), ["./x"]);
    }

    #[test]
    fn execdir_splits_into_directory_and_dot_slash_name() {
        assert_eq!(
            split_for_execdir(Path::new("/a/b/c.txt")),
            (PathBuf::from("/a/b"), PathBuf::from("./c.txt"))
        );
        assert_eq!(
            split_for_execdir(Path::new("/top")),
            (PathBuf::from("/"), PathBuf::from("./top"))
        );
        assert_eq!(
            split_for_execdir(Path::new("/")),
            (PathBuf::from("/"), PathBuf::from("."))
        );
    }

    #[cfg(unix)]
    #[test]
    fn execdir_program_resolution_refuses_relative_lookups() {
        let path = |s: &str| Some(OsString::from(s));
        // Bare names come from absolute PATH entries only.
        assert_eq!(
            resolve_program("sh", path("/usr/bin:/bin").as_deref()).unwrap(),
            if Path::new("/usr/bin/sh").exists() {
                PathBuf::from("/usr/bin/sh")
            } else {
                PathBuf::from("/bin/sh")
            }
        );
        for bad in [".:/bin", "/bin::/usr/bin", "bin:/bin", ""] {
            let err = resolve_program("sh", path(bad).as_deref()).unwrap_err();
            assert!(err.contains("relative"), "{bad:?}: {err}");
        }
        assert!(resolve_program("./tool", path("/bin").as_deref()).is_err());
        assert!(resolve_program("bin/tool", path("/bin").as_deref()).is_err());
        assert!(resolve_program("/bin/sh", None).is_ok());
        assert!(resolve_program("/definitely/missing", None).is_err());
        assert!(resolve_program("arfind-no-such-program-xyz", path("/bin").as_deref()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn open_dir_nofollow_refuses_symlinks_anywhere_in_the_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("real/sub")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        std::fs::write(root.join("file"), b"x").unwrap();

        assert!(open_dir_nofollow(&root.join("real/sub")).is_ok());
        for bad in ["link", "link/sub", "file"] {
            let err = open_dir_nofollow(&root.join(bad)).unwrap_err();
            assert!(
                matches!(err.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)),
                "{bad}: {err}"
            );
        }
        assert!(open_dir_nofollow(Path::new("relative/dir")).is_err());
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
