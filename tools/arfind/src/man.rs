//! What the manual page says beyond the options (see arcommon::man).

use arcommon::man::Page;

pub const PAGE: Page = Page {
    examples: &[
        ("Rust source files, by name", "arfind . --name '*.rs'"),
        ("The same, by type name", "arfind . --type rust"),
        (
            "Several directories at once",
            "arfind src tests --extension rs",
        ),
        (
            "Directories named src, at most two levels down",
            "arfind . -t d --name src --max-depth 2",
        ),
        ("Files larger than 100 MB", "arfind . -t f --size +100MB"),
        ("Empty files", "arfind . -t f --empty"),
        (
            "How many log files there are",
            "arfind /var/log --name '*.log' --count",
        ),
        (
            "Everything, including hidden and ignored files",
            "arfind . -H --no-ignore",
        ),
        (
            "Everything except logs and build output",
            "arfind . -t f --exclude '*.log' --exclude 'target/**'",
        ),
        (
            "Files not touched for a year",
            "arfind ~ -t f --older-than 1y",
        ),
        (
            "Configuration files on this filesystem only",
            "arfind / -x --name '*.conf'",
        ),
        (
            "The 20 largest files, biggest first",
            "arfind . -t f --sort size --reverse | head -20",
        ),
        (
            "Hand the results to another command, safe for any file name",
            "arfind . --name '*.orig' -0 | xargs -0 rm",
        ),
    ],
    exit_status: &[
        ("0", "Success, also when nothing matched."),
        (
            "1",
            "A PATH does not exist; a directory or entry could not be read (the rest is \
             still listed, and --debug names each one); an invalid --name, --size, \
             --exclude, --exclude-dir or -t value; an empty --older-than/--newer-than \
             window; a rejected --exec command, or a command that failed or could not \
             be started.",
        ),
        (
            "2",
            "Invalid usage: a bad or missing flag, an unknown --type, an unknown \
             --owner or --group name.",
        ),
    ],
    see_also: &["argrep(1)", "ardisk(1)", "find(1)", "xargs(1)"],
};
