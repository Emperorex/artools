//! Helpers shared by the artools binaries (arfind, ardisk, argrep).
//!
//! Right now this is terminal-safe output: file names and file contents can
//! contain control characters (ESC, BEL, CR, newline, ...). Printed raw, they
//! are executed by the terminal: an attacker who controls a file name can
//! clear the screen, rewrite earlier lines to hide entries, change the window
//! title, or on some terminals write to the clipboard (OSC 52). Every string
//! that reaches a terminal and did not come from the tools themselves goes
//! through one of the functions below.
//!
//! Machine-readable output (JSON, NUL-separated lists, arguments passed to
//! `--exec`) must keep the exact bytes and must NOT use these helpers.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::Path;

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

/// A path ready to print to a terminal: lossily decoded (invalid UTF-8
/// becomes U+FFFD, exactly as `Path::display` does) and then escaped with
/// [`sanitize_name`]. Use this instead of `path.display()` for anything
/// printed for a human.
pub fn display_path(path: &Path) -> String {
    let lossy = path.to_string_lossy();
    match sanitize_name(&lossy) {
        Cow::Borrowed(_) => lossy.into_owned(),
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

    #[cfg(unix)]
    #[test]
    fn display_path_handles_invalid_utf8() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let p = Path::new(OsStr::from_bytes(b"bad\xff\x1b.txt"));
        assert_eq!(display_path(p), "bad\u{fffd}\\x1b.txt");
    }
}
