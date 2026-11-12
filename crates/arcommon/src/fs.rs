//! Small platform helpers.

use std::{
    borrow::Cow,
    fs::{self, DirEntry, Metadata},
    io,
    path::Path,
};

use crate::color::ErrPaint;

/// The entries of directory `dir`, for a walk.
///
/// Opening is retried when a signal interrupts it (macOS sends these often,
/// from Spotlight, sandboxd and the like). An entry that fails while the
/// directory is being listed is passed to `on_error` rather than dropped,
/// so the caller can count it as unreadable like any other read failure.
pub fn read_dir_entries(
    dir: &Path,
    mut on_error: impl FnMut(io::Error),
) -> io::Result<Vec<DirEntry>> {
    let listing = loop {
        match fs::read_dir(dir) {
            Ok(listing) => break listing,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    };
    let mut entries = Vec::new();
    for entry in listing {
        match entry {
            Ok(entry) => entries.push(entry),
            Err(err) => on_error(err),
        }
    }
    Ok(entries)
}

/// Prints one read failure, as every tool does with `--debug`:
/// `TOOL: PATH: reason`.
pub fn print_read_error(tool: &str, path: &Path, err: &io::Error) {
    eprintln!(
        "{}: {}: {}",
        tool.err_red(),
        crate::display_path(path),
        error_text(err)
    );
}

/// The note every tool prints at the end of a run in which `count` files or
/// directories could not be read (the run then exits 1, or 2 for
/// `argrep -q`).
pub fn print_unreadable_summary(tool: &str, count: usize, debug: bool) {
    let hint = if debug {
        ""
    } else {
        " (rerun with --debug for details)"
    };
    eprintln!(
        "{}",
        format!("{tool}: {count} file(s)/director(ies) could not be read{hint}").err_red()
    );
}

/// An I/O error as a short message for the user, without the
/// " (os error N)" that `io::Error`'s `Display` appends: `No such file or
/// directory`, as `ls` and `grep` print it.
pub fn error_text(err: &io::Error) -> String {
    let text = err.to_string();
    match text.rfind(" (os error ") {
        Some(i) if text.ends_with(')') => text[..i].to_string(),
        _ => text,
    }
}

/// The exact bytes of a path, for machine-readable output such as
/// NUL-separated lists (`argrep -0`, `arfind -0`) that must round-trip any
/// file name. On non-Unix platforms paths are not plain bytes, so invalid
/// UTF-16 is replaced (U+FFFD), as `Path::to_string_lossy` does.
pub fn path_bytes(path: &Path) -> Cow<'_, [u8]> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Cow::Borrowed(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        Cow::Owned(path.to_string_lossy().into_owned().into_bytes())
    }
}

/// Device id of `path` (following symlinks), for `--one-file-system`.
/// `None` when it can't be read or on platforms without device ids.
pub fn device_of(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| device_of_metadata(&m))
}

/// Device id from metadata already at hand. `None` on platforms without
/// device ids.
pub fn device_of_metadata(metadata: &Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_dir_entries_lists_a_directory_and_reports_open_failures() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a"), "").unwrap();
        std::fs::create_dir(dir.path().join("b")).unwrap();
        let mut names: Vec<_> = read_dir_entries(dir.path(), |e| panic!("{e}"))
            .unwrap()
            .into_iter()
            .map(|e| e.file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["a", "b"]);
        let missing = read_dir_entries(&dir.path().join("nope"), |e| panic!("{e}"));
        assert_eq!(missing.unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn error_text_drops_the_os_error_number() {
        let missing = std::fs::metadata("/no/such/path/for/arcommon").unwrap_err();
        assert_eq!(error_text(&missing), "No such file or directory");
        let custom = io::Error::other("disk on fire");
        assert_eq!(error_text(&custom), "disk on fire");
    }

    #[test]
    fn path_bytes_are_the_exact_name() {
        assert_eq!(&*path_bytes(Path::new("a b/c\nd.txt")), b"a b/c\nd.txt");
    }

    #[cfg(unix)]
    #[test]
    fn path_bytes_keep_invalid_utf8_and_control_bytes() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let raw = b"bad\xff\x1b[2J.txt";
        assert_eq!(&*path_bytes(Path::new(OsStr::from_bytes(raw))), raw);
    }

    #[cfg(unix)]
    #[test]
    fn device_is_shared_within_a_directory_and_missing_paths_have_none() {
        let dir = std::env::temp_dir();
        assert_eq!(device_of(&dir), device_of(&dir.join(".")));
        assert!(device_of(&dir).is_some());
        assert!(device_of(Path::new("/definitely/missing/path")).is_none());
    }
}
