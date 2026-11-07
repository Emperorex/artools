//! Terminal-safe output.
//!
//! File names and file contents can contain control characters (ESC, BEL,
//! CR, newline, ...). Printed raw, they are executed by the terminal: an
//! attacker who controls a file name can clear the screen, rewrite earlier
//! lines to hide entries, change the window title, or on some terminals
//! write to the clipboard (OSC 52). Every string that reaches a terminal and
//! did not come from the tools themselves goes through one of the functions
//! below.
//!
//! Machine-readable output (JSON, NUL-separated lists, arguments passed to
//! `--exec`) must keep the exact bytes and must NOT use these helpers.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// True for characters a terminal may interpret as a control code: the C0
/// range (U+0000-U+001F), DEL (U+007F) and the C1 range (U+0080-U+009F).
/// C1 matters because some terminals treat e.g. U+009B as a one-character
/// CSI, the same as `ESC [`.
fn is_control(c: char) -> bool {
    matches!(c as u32, 0x00..=0x1F | 0x7F..=0x9F)
}

/// Fast byte-level check: any C0/DEL byte, or a UTF-8 encoded C1 character
/// (always the two bytes 0xC2 0x80..=0x9F). Lets the common case return the
/// input borrowed, with no allocation and no char decoding.
fn may_need_escaping(s: &str, keep_tab: bool) -> bool {
    let b = s.as_bytes();
    b.iter().enumerate().any(|(i, &x)| match x {
        b'\t' => !keep_tab,
        0x00..=0x1F | 0x7F => true,
        0xC2 => matches!(b.get(i + 1), Some(0x80..=0x9F)),
        _ => false,
    })
}

fn escape(s: &str, keep_tab: bool) -> Cow<'_, str> {
    if !may_need_escaping(s, keep_tab) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c == '\t' && keep_tab {
            out.push(c);
        } else if is_control(c) {
            let code = c as u32;
            if code < 0x80 {
                let _ = write!(out, "\\x{code:02x}");
            } else {
                let _ = write!(out, "\\u{{{code:x}}}");
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Makes text (for example a matched line of a file) safe to print to a
/// terminal: C0 controls, DEL and C1 controls become visible escapes
/// (`\x1b`, `\x07`, `\u{9b}`). Tab is kept, since it is common in ordinary
/// text and harmless. Returns the input borrowed when nothing needs escaping.
pub fn sanitize_for_display(s: &str) -> Cow<'_, str> {
    escape(s, true)
}

/// Like [`sanitize_for_display`], for a single name or path given as a
/// string: tab is escaped as well, and so is newline, which in a file name
/// would let one entry pose as two lines of output.
pub fn sanitize_name(s: &str) -> Cow<'_, str> {
    escape(s, false)
}

/// Maps paths found under a search root back to the form the user typed.
///
/// The tools walk from the canonical root (symlinks and `.`/`..` resolved),
/// which `--exclude` anchoring, `--one-file-system` and `--execdir` rely
/// on. Printed as is, though, `argrep foo src` would report
/// `/home/me/project/src/main.rs`. Like grep and find, results are reported
/// under the root as given instead:
///
/// | Typed            | Reported              |
/// |------------------|-----------------------|
/// | `src`            | `src/main.rs`         |
/// | `.`              | `./src/main.rs`       |
/// | `../other`       | `../other/lib.rs`     |
/// | `/abs/path`      | `/abs/path/lib.rs`    |
/// | nothing (default)| `src/main.rs`         |
///
/// In the last row no path was given, so the implicit `./` prefix carries
/// no information and is left off. Paths outside
/// the canonical root are returned unchanged.
#[derive(Debug, Clone)]
pub struct DisplayRoot {
    given: PathBuf,
    canonical: PathBuf,
    implicit: bool,
}

impl DisplayRoot {
    /// `given` is the root as typed (`"."` when `implicit`, i.e. the user
    /// gave no path at all); `canonical` is the root the walk starts from.
    pub fn new(given: impl Into<PathBuf>, canonical: impl Into<PathBuf>, implicit: bool) -> Self {
        DisplayRoot {
            given: given.into(),
            canonical: canonical.into(),
            implicit,
        }
    }

    /// `path` as it should be reported to a person.
    pub fn map<'a>(&self, path: &'a Path) -> Cow<'a, Path> {
        self.map_with(path, self.implicit)
    }

    /// `path` for output another program may take as an argument (NUL
    /// lists for `xargs -0`): like [`map`](Self::map), but keeps the `./`
    /// even when no path was typed, so a file named `-rf`
    /// comes out as `./-rf` and can never be read as an option.
    pub fn map_arg<'a>(&self, path: &'a Path) -> Cow<'a, Path> {
        self.map_with(path, false)
    }

    fn map_with<'a>(&self, path: &'a Path, drop_dot: bool) -> Cow<'a, Path> {
        if !drop_dot && self.given == self.canonical {
            return Cow::Borrowed(path);
        }
        let Ok(rel) = path.strip_prefix(&self.canonical) else {
            return Cow::Borrowed(path);
        };
        if rel.as_os_str().is_empty() {
            Cow::Owned(self.given.clone())
        } else if drop_dot {
            Cow::Owned(rel.to_path_buf())
        } else {
            Cow::Owned(self.given.join(rel))
        }
    }
}

/// The roots of one search (`argrep foo src tests` has two), each
/// reported as typed.
#[derive(Debug, Clone, Default)]
pub struct DisplayRoots(Vec<DisplayRoot>);

impl DisplayRoots {
    pub fn new(roots: Vec<DisplayRoot>) -> Self {
        DisplayRoots(roots)
    }

    /// The root `path` is reported under: the one whose canonical path is
    /// the longest prefix of it. So with `src` and `src/lib` both given, a
    /// file below `src/lib` is shown as `src/lib/...` whichever walk found
    /// it, and two roots that are one directory (`. $PWD`) use the first.
    fn root_of(&self, path: &Path) -> Option<&DisplayRoot> {
        self.0
            .iter()
            .filter(|r| path.starts_with(&r.canonical))
            .max_by_key(|r| r.canonical.components().count())
    }

    /// [`DisplayRoot::map`] with the root `path` belongs to.
    pub fn map<'a>(&self, path: &'a Path) -> Cow<'a, Path> {
        match self.root_of(path) {
            Some(root) => root.map(path),
            None => Cow::Borrowed(path),
        }
    }

    /// [`DisplayRoot::map_arg`] with the root `path` belongs to.
    pub fn map_arg<'a>(&self, path: &'a Path) -> Cow<'a, Path> {
        match self.root_of(path) {
            Some(root) => root.map_arg(path),
            None => Cow::Borrowed(path),
        }
    }
}

static DISPLAY_ROOTS: OnceLock<DisplayRoots> = OnceLock::new();

/// Sets the roots that [`shown_path`] and [`display_path`] report paths
/// under, for the rest of the process. Called once by a tool's `main`
/// before the walk starts; later calls are ignored. Without a call, paths
/// are shown exactly as they are.
pub fn set_display_roots(roots: Vec<DisplayRoot>) {
    let _ = DISPLAY_ROOTS.set(DisplayRoots::new(roots));
}

/// [`set_display_roots`] for a search with a single root.
pub fn set_display_root(root: DisplayRoot) {
    set_display_roots(vec![root]);
}

/// `path` in the form it is reported to the user (see [`DisplayRoot`]),
/// with the exact bytes kept, e.g. for JSON. [`display_path`] applies it
/// too.
pub fn shown_path(path: &Path) -> Cow<'_, Path> {
    match DISPLAY_ROOTS.get() {
        Some(roots) => roots.map(path),
        None => Cow::Borrowed(path),
    }
}

/// Like [`shown_path`], for NUL-separated lists meant for `xargs -0`: see
/// [`DisplayRoot::map_arg`]. (`--exec` arguments are not mapped at all:
/// arfind passes absolute paths there, see its `exec` module.)
pub fn shown_arg_path(path: &Path) -> Cow<'_, Path> {
    match DISPLAY_ROOTS.get() {
        Some(roots) => roots.map_arg(path),
        None => Cow::Borrowed(path),
    }
}

/// A path ready to print to a terminal: reported under the root the user
/// typed (see [`shown_path`]), lossily decoded (invalid UTF-8 becomes
/// U+FFFD, exactly as `Path::display` does) and then escaped with
/// [`sanitize_name`]. Use this instead of `path.display()` for anything
/// printed for a human.
pub fn display_path(path: &Path) -> String {
    let shown = shown_path(path);
    let lossy = shown.to_string_lossy();
    match sanitize_name(&lossy) {
        Cow::Borrowed(s) => s.to_owned(),
        Cow::Owned(s) => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn plain_text_is_borrowed_unchanged() {
        assert!(matches!(
            sanitize_for_display("hello"),
            Cow::Borrowed("hello")
        ));
        assert!(matches!(sanitize_name("héllo 日本"), Cow::Borrowed(_)));
    }

    #[test]
    fn c0_and_del_are_escaped() {
        assert_eq!(
            sanitize_for_display("a\x1b[2Jb\x07\x7f"),
            "a\\x1b[2Jb\\x07\\x7f"
        );
        assert_eq!(sanitize_for_display("cr\rlf"), "cr\\x0dlf");
    }

    #[test]
    fn c1_controls_are_escaped() {
        // U+009B is a one-character CSI on some terminals.
        assert_eq!(sanitize_for_display("a\u{9b}2Jb"), "a\\u{9b}2Jb");
        assert_eq!(sanitize_name("\u{80}\u{9f}"), "\\u{80}\\u{9f}");
        // Neighbours of the C1 range are ordinary characters.
        assert!(matches!(sanitize_name("\u{a0}\u{7e}é"), Cow::Borrowed(_)));
    }

    #[test]
    fn tab_kept_in_text_but_escaped_in_names() {
        assert_eq!(sanitize_for_display("a\tb"), "a\tb");
        assert_eq!(sanitize_name("a\tb"), "a\\x09b");
        assert_eq!(sanitize_name("one\ntwo"), "one\\x0atwo");
    }

    #[test]
    fn display_path_escapes_the_report_payload() {
        let p = Path::new("dir/x\x1b]0;PWNED\x07\x1b[2J.txt");
        assert_eq!(display_path(p), "dir/x\\x1b]0;PWNED\\x07\\x1b[2J.txt");
        assert_eq!(display_path(Path::new("dir/ok.txt")), "dir/ok.txt");
    }

    fn mapped(root: &DisplayRoot, p: &str) -> String {
        root.map(Path::new(p)).to_string_lossy().into_owned()
    }

    #[test]
    fn display_root_reports_paths_as_typed() {
        let rel = DisplayRoot::new("src", "/home/me/proj/src", false);
        assert_eq!(mapped(&rel, "/home/me/proj/src"), "src");
        assert_eq!(mapped(&rel, "/home/me/proj/src/a/b.rs"), "src/a/b.rs");

        let dot = DisplayRoot::new(".", "/home/me/proj", false);
        assert_eq!(mapped(&dot, "/home/me/proj"), ".");
        assert_eq!(mapped(&dot, "/home/me/proj/a.rs"), "./a.rs");

        let up = DisplayRoot::new("../other", "/home/me/other", false);
        assert_eq!(mapped(&up, "/home/me/other/x"), "../other/x");

        // A trailing slash is kept on the root and not doubled below it.
        let slash = DisplayRoot::new("src/", "/p/src", false);
        assert_eq!(mapped(&slash, "/p/src"), "src/");
        assert_eq!(mapped(&slash, "/p/src/x"), "src/x");

        // Through a symlinked root: reported under the link, as typed.
        let link = DisplayRoot::new("link", "/real/dir", false);
        assert_eq!(mapped(&link, "/real/dir/f"), "link/f");
    }

    #[test]
    fn display_root_without_a_typed_path_drops_the_dot() {
        let imp = DisplayRoot::new(".", "/home/me/proj", true);
        assert_eq!(mapped(&imp, "/home/me/proj"), ".");
        assert_eq!(mapped(&imp, "/home/me/proj/src/a.rs"), "src/a.rs");
        // ...except where another program takes it as an argument.
        let arg = |p: &str| imp.map_arg(Path::new(p)).to_string_lossy().into_owned();
        assert_eq!(arg("/home/me/proj/-rf"), "./-rf");
        assert_eq!(arg("/home/me/proj"), ".");
        // With a typed path, both forms are the same.
        let typed = DisplayRoot::new("src", "/p/src", false);
        assert_eq!(typed.map_arg(Path::new("/p/src/-rf")), Path::new("src/-rf"));
    }

    #[test]
    fn several_roots_each_keep_their_typed_form() {
        let roots = DisplayRoots::new(vec![
            DisplayRoot::new("src", "/p/src", false),
            DisplayRoot::new("../other", "/home/other", false),
            DisplayRoot::new("/abs", "/abs", false),
        ]);
        let m = |p: &str| roots.map(Path::new(p)).to_string_lossy().into_owned();
        assert_eq!(m("/p/src/a.rs"), "src/a.rs");
        assert_eq!(m("/home/other/b"), "../other/b");
        assert_eq!(m("/abs/c"), "/abs/c");
        assert_eq!(m("/elsewhere/d"), "/elsewhere/d");
    }

    #[test]
    fn nested_roots_use_the_most_specific_one() {
        // Given in either order, a file under both is shown under the
        // deeper root.
        for roots in [
            vec![
                DisplayRoot::new("src", "/p/src", false),
                DisplayRoot::new("src/lib", "/p/src/lib", false),
            ],
            vec![
                DisplayRoot::new("src/lib", "/p/src/lib", false),
                DisplayRoot::new("src", "/p/src", false),
            ],
        ] {
            let roots = DisplayRoots::new(roots);
            let m = |p: &str| roots.map(Path::new(p)).to_string_lossy().into_owned();
            assert_eq!(m("/p/src/lib/x.rs"), "src/lib/x.rs");
            assert_eq!(m("/p/src/main.rs"), "src/main.rs");
        }
        // A sibling that only shares a string prefix is not "under" it.
        let roots = DisplayRoots::new(vec![DisplayRoot::new("src", "/p/src", false)]);
        assert_eq!(
            roots.map(Path::new("/p/srcfoo/x")),
            Path::new("/p/srcfoo/x")
        );
    }

    #[test]
    fn display_root_leaves_other_paths_alone() {
        let abs = DisplayRoot::new("/p", "/p", false);
        assert!(matches!(abs.map(Path::new("/p/x")), Cow::Borrowed(_)));
        let rel = DisplayRoot::new("src", "/p/src", false);
        // Outside the root, or a sibling sharing a string prefix.
        assert_eq!(mapped(&rel, "/elsewhere/x"), "/elsewhere/x");
        assert_eq!(mapped(&rel, "/p/srcfoo/x"), "/p/srcfoo/x");
    }

    #[cfg(unix)]
    #[test]
    fn display_path_handles_invalid_utf8() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let p = Path::new(OsStr::from_bytes(b"bad\xff\x1b.txt"));
        assert_eq!(display_path(p), "bad\u{fffd}\\x1b.txt");
    }
}
