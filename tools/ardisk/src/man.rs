//! What the manual page says beyond the options (see arcommon::man).

use arcommon::man::Page;

pub const PAGE: Page = Page {
    examples: &[
        (
            "The 10 heaviest directories below the current one",
            "ardisk . --top 10",
        ),
        (
            "Only directories over 1 GB",
            "ardisk / --threshold 1GB --top 5",
        ),
        ("The top level only", "ardisk . --max-depth 1"),
        ("The total size of a directory", "ardisk /var/log -s"),
        (
            "The total as du -sh reports it",
            "ardisk . --summarize --apparent-size",
        ),
        (
            "Space taken by videos, by directory",
            "ardisk ~/Movies --include '*.mp4'",
        ),
        ("The 20 largest files", "ardisk ~ --largest-files 20"),
        ("Which file types take the space", "ardisk ~ --by-type"),
        (
            "Duplicate files of at least 1 MB, the 10 most wasteful groups",
            "ardisk ~ --duplicates --min-size 1MB --top 10",
        ),
        (
            "Without log files and build output",
            "ardisk . --exclude '*.log' --exclude 'target/**'",
        ),
        (
            "Paths of the largest files, for a script",
            "ardisk . --largest-files 20 --json | jq -r '.entries[].path'",
        ),
    ],
    exit_status: &[
        ("0", "Success."),
        (
            "1",
            "PATH does not exist, is not a directory or cannot be read (no report is \
             printed); a directory or file below it could not be read (the report \
             covers the rest, and --debug names each one); an invalid --threshold, \
             --include or --exclude value; --diff snapshots that cannot be read or \
             compared.",
        ),
        ("2", "Invalid usage: a bad or missing flag."),
    ],
    see_also: &["arfind(1)", "argrep(1)", "du(1)"],
};
