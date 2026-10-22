//! Small platform helpers.

use std::{borrow::Cow, fs::Metadata, path::Path};

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
