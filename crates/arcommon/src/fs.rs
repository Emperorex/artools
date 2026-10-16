//! Small platform helpers.

use std::{fs::Metadata, path::Path};

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

    #[cfg(unix)]
    #[test]
    fn device_is_shared_within_a_directory_and_missing_paths_have_none() {
        let dir = std::env::temp_dir();
        assert_eq!(device_of(&dir), device_of(&dir.join(".")));
        assert!(device_of(&dir).is_some());
        assert!(device_of(Path::new("/definitely/missing/path")).is_none());
    }
}
