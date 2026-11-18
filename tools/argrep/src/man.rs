//! What the manual page says beyond the options (see arcommon::man).

use arcommon::man::Page;

pub const PAGE: Page = Page {
    examples: &[
        ("Search the current directory", "argrep TODO ."),
        (
            "Several files and directories, with line numbers",
            "argrep -n error src tests README.md",
        ),
        ("Ignore case", "argrep -i todo src"),
        (
            "Any of several patterns",
            "argrep -e TODO -e FIXME -e HACK src",
        ),
        (
            "A literal string, not a regular expression",
            "argrep -F 'foo.bar' .",
        ),
        ("Rust files only", "argrep unwrap . --type rust"),
        (
            "Everything, including hidden and ignored files",
            "argrep TODO . --hidden --no-ignore",
        ),
        ("Only the names of files with a match", "argrep -l TODO ."),
        (
            "Files that lack a required header",
            "argrep -L '^// SPDX-License-Identifier' . --include '*.rs'",
        ),
        (
            "Hand matching files to another command, safe for any file name",
            "argrep -l0 deprecated_api src | xargs -0 git add",
        ),
        (
            "Filter the output of another command",
            "ps aux | argrep rust",
        ),
        (
            "Use the exit status alone",
            "argrep -q TODO src && echo 'TODO found'",
        ),
        (
            "Compressed logs, without unpacking them",
            "argrep -z OutOfMemoryError /var/log/app/*.log.gz",
        ),
    ],
    exit_status: &[
        (
            "0",
            "Success: every file was read, with or without matches.",
        ),
        (
            "1",
            "A PATH does not exist; a file or directory could not be read (the rest is \
             still searched, and --debug names each one); an invalid pattern or another \
             invalid value, such as an --include glob.",
        ),
        (
            "2",
            "Invalid usage: a bad or missing flag, or a missing QUERY.",
        ),
        (
            "With -q",
            "The statuses are those of grep: 0 if a match was found, 1 if none was, \
             2 if an error occurred.",
        ),
    ],
    see_also: &["arfind(1)", "ardisk(1)", "grep(1)", "xargs(1)"],
};
