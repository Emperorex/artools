//! What a walk skips: the default ignored directories, `--exclude-dir`
//! entries, nested `.gitignore`/`.ignore` files, and `--exclude` patterns.

use std::{collections::HashSet, path::Path, sync::Arc};

use glob::Pattern;
use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};

/// Directories every tool skips unless `--no-ignore` is given. A tool may
/// skip more (argrep also skips `target`).
pub const DEFAULT_IGNORED_DIRS: &[&str] = &[".git", "node_modules", "__pycache__"];

/// The ignore files read in each directory, in increasing priority.
pub const IGNORE_FILE_NAMES: [&str; 2] = [".gitignore", ".ignore"];

/// The `.gitignore`/`.ignore` matchers from the root down to a directory,
/// shallowest first. Cheap to clone and share between worker threads.
pub type IgnoreStack = Arc<Vec<Arc<Gitignore>>>;

/// Splits directory-skip entries into exact names and glob patterns.
///
/// Starts from `defaults` unless `no_ignore` is set; `extra` (the user's
/// `--exclude-dir`/`--ignore` entries) is always honored. With
/// `allow_globs`, entries containing `*`, `?` or `[` are compiled as globs
/// so the common plain-name case stays a fast set lookup; without it every
/// entry is a plain name. Returns the offending entry and the error for an
/// invalid glob, so the caller can report it in its own words.
pub fn split_dir_excludes(
    defaults: &[&str],
    no_ignore: bool,
    extra: impl IntoIterator<Item = String>,
    allow_globs: bool,
) -> Result<(HashSet<String>, Vec<Pattern>), (String, glob::PatternError)> {
    let mut names: HashSet<String> = if no_ignore {
        HashSet::new()
    } else {
        defaults.iter().map(|s| s.to_string()).collect()
    };
    let mut patterns = Vec::new();
    for entry in extra {
        if allow_globs && entry.contains(['*', '?', '[']) {
            match Pattern::new(&entry) {
                Ok(p) => patterns.push(p),
                Err(e) => return Err((entry, e)),
            }
        } else {
            names.insert(entry);
        }
    }
    Ok((names, patterns))
}

/// Builds the matcher for one directory's own ignore files.
///
/// `present` lists which of [`IGNORE_FILE_NAMES`] the directory actually
/// contains (taken from its listing, which the caller already has, so no
/// file is opened speculatively). Returns `None` when there is nothing to
/// apply. Problems in the files (bad patterns, unreadable files) are
/// returned rather than printed, for the caller to report under `--debug`;
/// valid lines still apply.
pub fn dir_gitignore(dir: &Path, present: &[&str]) -> (Option<Gitignore>, Vec<ignore::Error>) {
    let mut errors = Vec::new();
    if present.is_empty() {
        return (None, errors);
    }
    let mut builder = GitignoreBuilder::new(dir);
    for name in present {
        if let Some(err) = builder.add(dir.join(name)) {
            errors.push(err);
        }
    }
    let gi = match builder.build() {
        Ok(gi) if gi.num_ignores() > 0 || gi.num_whitelists() > 0 => Some(gi),
        Ok(_) => None,
        Err(err) => {
            errors.push(err);
            None
        }
    };
    (gi, errors)
}

/// The stack for a child directory: `parent` plus the directory's own
/// matcher, if it has one (otherwise the parent stack is shared as is).
pub fn push_gitignore(parent: &IgnoreStack, own: Option<Gitignore>) -> IgnoreStack {
    match own {
        Some(gi) => {
            let mut stack = (**parent).clone();
            stack.push(Arc::new(gi));
            Arc::new(stack)
        }
        None => Arc::clone(parent),
    }
}

/// The stack for the entries of `dir`: `parent` extended with `dir`'s own
/// ignore files. `has_file(name)` reports whether the directory listing
/// (which the caller already has) contains `name`, so only files that exist
/// are read. Problems in the files are returned for the caller to report.
pub fn stack_for_dir(
    parent: &IgnoreStack,
    dir: &Path,
    has_file: impl Fn(&str) -> bool,
) -> (IgnoreStack, Vec<ignore::Error>) {
    let present: Vec<&str> = IGNORE_FILE_NAMES
        .into_iter()
        .filter(|name| has_file(name))
        .collect();
    let (own, errors) = dir_gitignore(dir, &present);
    (push_gitignore(parent, own), errors)
}

/// Whether `path` is ignored by the stack. Deeper files win, and within a
/// file the last matching line wins, so a `!pattern` re-includes what a
/// parent ignored — git's own precedence.
pub fn is_ignored(stack: &[Arc<Gitignore>], path: &Path, is_dir: bool) -> bool {
    let mut ignored = false;
    for matcher in stack {
        match matcher.matched(path, is_dir) {
            Match::Ignore(_) => ignored = true,
            Match::Whitelist(_) => ignored = false,
            Match::None => {}
        }
    }
    ignored
}

/// Builds the `--exclude` matcher: gitignore-style patterns anchored at
/// `root`. `None` when there are no patterns.
pub fn exclude_matcher(root: &Path, patterns: &[String]) -> Result<Option<Gitignore>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        if pattern.trim().is_empty() {
            return Err("--exclude pattern must not be empty".to_string());
        }
        builder
            .add_line(None, pattern)
            .map_err(|e| format!("Invalid --exclude pattern '{}': {}", pattern, e))?;
    }
    builder
        .build()
        .map(Some)
        .map_err(|e| format!("Invalid --exclude patterns: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn dir_excludes_defaults_names_and_globs() {
        let (names, globs) = split_dir_excludes(
            DEFAULT_IGNORED_DIRS,
            false,
            ["build*".into(), "out".into()],
            true,
        )
        .unwrap();
        assert!(names.contains(".git") && names.contains("out"));
        assert_eq!(globs.len(), 1);
        assert!(globs[0].matches("build-x"));

        // --no-ignore drops the defaults but keeps explicit entries.
        let (names, _) =
            split_dir_excludes(DEFAULT_IGNORED_DIRS, true, ["out".into()], true).unwrap();
        assert_eq!(names, HashSet::from(["out".to_string()]));

        // Without glob support every entry is a plain name.
        let (names, globs) = split_dir_excludes(&[], false, ["b*".into()], false).unwrap();
        assert!(names.contains("b*") && globs.is_empty());

        let (entry, _) = split_dir_excludes(&[], false, ["[".into()], true).unwrap_err();
        assert_eq!(entry, "[");
    }

    #[test]
    fn dir_gitignore_reads_only_present_files() {
        let dir = tempfile::TempDir::new().unwrap();
        // Nothing listed: nothing read, even if a file exists.
        fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        assert!(dir_gitignore(dir.path(), &[]).0.is_none());

        let (gi, errors) = dir_gitignore(dir.path(), &[".gitignore"]);
        assert!(errors.is_empty());
        let gi = gi.expect("matcher");
        assert!(gi.matched(dir.path().join("a.log"), false).is_ignore());

        // A file with only comments yields no matcher.
        fs::write(dir.path().join(".ignore"), "# nothing\n").unwrap();
        assert!(dir_gitignore(dir.path(), &[".ignore"]).0.is_none());
    }

    #[test]
    fn deeper_whitelist_overrides_parent_ignore() {
        let dir = tempfile::TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        fs::write(sub.join(".gitignore"), "!keep.log\n").unwrap();

        let root: IgnoreStack = Arc::new(Vec::new());
        let top = push_gitignore(&root, dir_gitignore(dir.path(), &[".gitignore"]).0);
        let deep = push_gitignore(&top, dir_gitignore(&sub, &[".gitignore"]).0);
        assert!(is_ignored(&top, &sub.join("keep.log"), false));
        assert!(!is_ignored(&deep, &sub.join("keep.log"), false));
        assert!(is_ignored(&deep, &sub.join("other.log"), false));
        // No own file: the parent stack is shared, not copied.
        assert!(Arc::ptr_eq(&push_gitignore(&top, None), &top));
    }

    #[test]
    fn stack_for_dir_reads_only_listed_files() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        let root: IgnoreStack = Arc::new(Vec::new());
        let (stack, errors) = stack_for_dir(&root, dir.path(), |_| false);
        assert!(errors.is_empty() && stack.is_empty());
        let (stack, _) = stack_for_dir(&root, dir.path(), |n| n == ".gitignore");
        assert!(is_ignored(&stack, &dir.path().join("a.log"), false));
    }

    #[test]
    fn exclude_matcher_validates_patterns() {
        let root = Path::new("/r");
        assert!(exclude_matcher(root, &[]).unwrap().is_none());
        assert!(exclude_matcher(root, &[" ".into()]).is_err());
        let m = exclude_matcher(root, &["*.tmp".into()]).unwrap().unwrap();
        assert!(m.matched(root.join("x.tmp"), false).is_ignore());
    }
}
