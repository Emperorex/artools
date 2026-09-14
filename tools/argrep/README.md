# argrep

Fast parallel text search utility — a drop-in alternative to `grep`.

## Installation

```
curl -fsSL https://artools.io/install.sh | bash -s -- argrep
```

Or build from source:

```
cargo build --release --bin argrep
```

## Usage

```
argrep [OPTIONS] QUERY [PATH]
```

`QUERY` is required and is matched as a **regular expression** by default (Rust's [`regex`](https://docs.rs/regex) crate — a similar dialect to `grep -E`/PCRE, without backreferences or lookaround). Use `-F`/`--fixed-strings` to search for `QUERY` literally instead, e.g. when it contains characters like `.`, `*`, `(` that you don't want interpreted as regex syntax:

```
argrep -F 'foo.bar' .          # matches the literal text "foo.bar"
argrep -F 'connection refused' /var/log
```

Use `-w`/`--word-regexp` to only match whole words (the pattern is wrapped in `\b(?:...)\b`), and `-x`/`--line-regexp` to only match whole lines (wrapped in `^(?:...)$`) — same semantics as `grep -w`/`grep -x`:

```
argrep -w cat .                # matches "cat" and "the cat sat", not "category"
argrep -x ERROR .              # matches a line that is exactly "ERROR", not "ERROR: disk full"
```

Use `-q`/`--quiet` to suppress all output and rely on the exit code alone — the search stops as soon as one match is found instead of scanning the rest of the tree, and (unlike the tool's default exit-code contract below) follows grep's own convention: `0` = at least one match, `1` = no match, `2` = a CLI/config error occurred:

```
if argrep -q 'TODO' src/; then
  echo "TODO found"
fi
```

Use `-o`/`--only-matching` to print only the matched text instead of the whole line — same as `grep -o`. Each occurrence gets its own output line, so a line with multiple matches prints multiple lines:

```
echo 'foo bar foo' | argrep -o 'foo'
# foo
# foo
```

How `-o` interacts with other flags:

- **`-n`**: still works — the same line number is repeated once per occurrence on that line.
- **`-c`**: unaffected — `-c` counts matching *lines*, not individual occurrences, same as real grep.
- **`-l`**: unaffected — `-l` stops at the filename, before `-o` would ever apply.
- **`-v`**: rejected as a CLI error (`-o` and `-v` can't be combined) — `-v` selects whole lines that *don't* contain a match, so there'd be nothing for `-o` to extract.
- **`-A`/`-B`/`-C`**: have no effect and print a warning, same as GNU grep — context doesn't make sense when only the matched fragment (not the surrounding line) is being printed.

Use `-L`/`--files-without-match` to print only the files that contain **no** match at all — the opposite of `-l`. Useful for finding files missing a required header, checking a migration is complete, CI checks, or spotting files that don't yet conform to a rule:

```
argrep -L 'TODO' src/
```

How `-L` interacts with other flags:

- **`-l`**: rejected as a CLI error — a file can't be reported as both "has a match" and "has no match", so the two are mutually exclusive.
- **`-c`**: rejected as a CLI error, same reasoning as `-l`/`-c` above — `-L` stops reading a file the moment it finds *any* match, so it never finishes counting matches in files that do have one, and a per-line count is meaningless for files that have none.
- **`-q`**: prints nothing either way, exit code only — same as every other output mode under `-q`. No exit-code special case is needed: `-q`'s exit status already reflects "was any line matched anywhere," which is independent of `-l`/`-L`.
- **Unreadable files**: never reported by `-L`, in either direction. A file that fails to open isn't counted as "no match" (it was never actually searched), and neither is a file that fails partway through a read — only files that are opened successfully and read to EOF with zero matches are reported.
- **`-v`**: not rejected — combining them means "files where every selected line matched", i.e. files with zero non-matching lines. A narrow but coherent combination, so it's allowed rather than forbidden.

Use `-m`/`--max-count` to stop searching a file after `NUM` matching lines — useful for large logs and CI, where you often just need to know *whether* something matched, not every occurrence:

```
argrep -m 1 'panic!' src/
```

How `-m` interacts with other flags (matching GNU grep's own documented behavior):

- **`-v`**: counts non-matching (selected) lines instead of matching ones — the limit always applies to whatever lines are actually being *selected* for output.
- **`-c`**: the printed count is capped at `NUM`, even if the file actually has more matches.
- **`-o`**: the limit is on matching *lines*, not individual occurrences — a line with several matches still only counts once, but every occurrence on that line is still printed.
- **`-A`/`-B`/`-C`**: any pending trailing context is still printed after the limit is reached, before the file search actually stops.
- `NUM` must be `>= 1`; `-m 0` is rejected as invalid rather than replicating GNU grep's own corner-case behavior for it.

Use `--exclude PATTERN` to skip files by name — the opposite of `--include`, same glob syntax, repeatable:

```
argrep 'TODO' . --exclude '*.min.js'
argrep 'password' . --exclude '*.lock'
```

**`--include`/`--exclude` glob syntax is not the same thing as the `QUERY` regex.** `*`, `?`, and `[...]` in `--include`/`--exclude`/`--exclude-dir` are shell-style glob wildcards matched against a filename or directory name — nothing to do with the regex engine used for `QUERY` (see the `-F` section above). `argrep '*.rs' .` searches file contents for the literal regex `*.rs` (almost certainly not what you want — `*` is invalid at the start of a regex); `argrep 'fn main' . --include '*.rs'` searches `.rs` files for `fn main`.

If a file matches both `--include` and `--exclude`, **`--exclude` wins** — this is a deliberate simplification of GNU grep's actual precedence rule, which is order-dependent ("the last matching one wins", tracking the position of each `--include`/`--exclude` flag on the command line). Replicating that exactly would need argrep to track flag order across two different options, which isn't worth the complexity for what's usually a non-overlapping pair of filters in practice; "exclude always wins" is simpler to reason about and matches what most other tools with include/exclude filters do.

Use `--exclude-dir PATTERN` to skip whole directories during traversal — matched against the directory's **basename only** (not the full path), same as GNU grep's `--exclude-dir`, and applied *before* a matching directory is ever handed to a worker thread, so excluded subtrees cost no traversal time:

```
argrep 'TODO' . --exclude-dir node_modules
argrep 'TODO' . --exclude-dir target
argrep 'TODO' . --exclude-dir 'build*'
```

Repeatable, and accepts both plain names (`node_modules`) and glob patterns (`build*`) — plain names are matched directly, patterns are compiled as globs, and a directory is skipped if it matches *any* given `--exclude-dir`. `--exclude-dir` is the same mechanism as the older `--ignore` flag (kept as an alias for compatibility) merged with the built-in defaults (`.git`, `node_modules`, `__pycache__`, `target`, disabled via `--no-ignore`).

`PATH` defaults to `.` (current directory) if not specified.

`argrep` also reads from **stdin** when used in a pipeline — no path argument needed.

## Options

| Flag                    | Short | Default           | Description                                                                                                                                     |
|-------------------------|-------|-------------------|-------------------------------------------------------------------------------------------------------------------------------------------------|
| `--ignore-case`         | `-i`  | —                 | Case-insensitive matching                                                                                                                       |
| `--fixed-strings`       | `-F`  | —                 | Treat QUERY as a literal string instead of a regex                                                                                              |
| `--word-regexp`         | `-w`  | —                 | Match only whole words (wraps QUERY in `\b(?:...)\b`)                                                                                           |
| `--line-regexp`         | `-x`  | —                 | Match only whole lines (wraps QUERY in `^(?:...)$`)                                                                                             |
| `--line-number`         | `-n`  | —                 | Show line numbers in output                                                                                                                     |
| `--before-context NUM`  | `-B`  | —                 | Show NUM lines of leading context before matches (max 100,000)                                                                                  |
| `--after-context NUM`   | `-A`  | —                 | Show NUM lines of trailing context after matches (max 100,000)                                                                                  |
| `--context NUM`         | `-C`  | —                 | Show NUM lines of leading and trailing context around matches (max 100,000)                                                                     |
| `--invert`              | `-v`  | —                 | Print lines that do NOT match the query (conflicts with `-o`)                                                                                   |
| `--only-matching`       | `-o`  | —                 | Print only the matched text, one occurrence per line (conflicts with `-v`; no effect with `-A`/`-B`/`-C`, warns)                                |
| `--max-count NUM`       | `-m`  | unlimited         | Stop searching a file after NUM matching lines (must be >= 1)                                                                                   |
| `--files-with-matches`  | `-l`  | —                 | Print only filenames of files containing a match (conflicts with `-c`, `-L`)                                                                    |
| `--files-without-match` | `-L`  | —                 | Print only filenames of files containing NO match (conflicts with `-l`, `-c`)                                                                   |
| `--count`               | `-c`  | —                 | Print count of matching lines per file (conflicts with `-l`, `-L`)                                                                              |
| `--include PATTERN`     | —     | —                 | Only search files matching this glob (e.g. `"*.rs"`, `"*.log"`)                                                                                 |
| `--exclude PATTERN`     | —     | —                 | Skip files matching this glob (e.g. `"*.min.js"`, `"*.lock"`); repeatable; wins over `--include` on overlap                                     |
| `--exclude-dir PATTERN` | —     | built-in defaults | Skip directories matching this name or glob (e.g. `"node_modules"`, `"build*"`), matched by basename; repeatable; alias: `--ignore`             |
| `--no-ignore`           | —     | —                 | Disable the built-in directory defaults (`.git`, `node_modules`, `__pycache__`, `target`) — explicit `--exclude-dir`/`--ignore` still applies   |
| `--jobs N`              | `-j`  | CPU-aware         | Number of parallel worker threads (1–128; default is half the available cores, clamped to 1–16)                                                 |
| `--debug`               | `-d`  | —                 | Print scan statistics and errors to stderr                                                                                                      |
| `--quiet`               | `-q`  | —                 | No output; exit code alone reports match/no-match/error (see Exit codes below). Overrides -l/-c/-n if also set — nothing is printed either way. |

`-l`, `-L`, and `-c` cannot be combined with each other — they imply mutually incompatible output contracts (`filename` matched vs `filename` unmatched vs `filename: count`), so combining any two of them (`argrep foo . -c -l`, `argrep foo . -l -L`, etc.) is a CLI error rather than one silently overriding the other.

## Default ignores

The following directories are always skipped:

- `.git`
- `node_modules`
- `__pycache__`
- `target`

Hidden files and directories (names starting with `.`) are also skipped by default.

## Binary file handling

`argrep` automatically skips binary files by checking the first 1024 bytes for null bytes. No flag needed — compiled binaries, images, and media files are silently ignored.

Text files with invalid UTF-8 (a stray byte from a legacy encoding, a corrupted line, etc.) are still searched in full: an invalid sequence becomes `U+FFFD` in that one line rather than ending the scan partway through the file. This matches how tools like `ripgrep` treat non-UTF-8 text by default.

## Examples

### Basic search

```
# Search for a term in current directory
argrep "TODO" .

# Search with line numbers
argrep "error" /var/log -n

# Case-insensitive search
argrep "todo" ./src -i -n
```

### Filtering

```
# Only search Rust files
argrep "unwrap" . --include "*.rs"

# Only search log files
argrep "error" /var/log --include "*.log"

# Search multiple levels with specific extension
argrep "panic" . --include "*.rs" -n -i
```

### Output modes

```
# List only filenames that contain a match
argrep "TODO" . -l

# Count matching lines per file
argrep "error" /var/log --include "*.log" -c

# Invert — show lines that don't contain the query
argrep "ok" ./results.txt -v

# List files missing a required header/marker
argrep -L '^// SPDX-License-Identifier' . --include "*.rs"
```

### Stdin / pipeline mode

When piped from another command, `argrep` reads from stdin automatically:

```
# Filter process list
ps aux | argrep "rust"

# Filter log output
tail -f /var/log/system.log | argrep "error"

# Chain with other tools
cat access.log | argrep "404" | argrep -v "bot"

# Count matches from stdin
cat app.log | argrep "panic" -c
```

### Combined flags

```
# Find files containing TODOs, case-insensitive, Rust files only
argrep "todo" . -i -l --include "*.rs"

# Show line numbers for errors in logs, count per file
argrep "error" /var/log -c --include "*.log"

# Search with 8 workers on a large codebase
argrep "deprecated" /large/project -j 8 --include "*.py" -n
```

## Comparison with `grep`

| Task                   | `grep`                                         | `argrep`                                      |
|------------------------|------------------------------------------------|-----------------------------------------------|
| Recursive search       | `grep -r "query" .`                            | `argrep "query" .`                            |
| Case-insensitive       | `grep -ri "query" .`                           | `argrep -i "query" .`                         |
| Fixed string           | `grep -rF "query" .`                           | `argrep -F "query" .`                         |
| Whole word             | `grep -rw "query" .`                           | `argrep -w "query" .`                         |
| Whole line             | `grep -rx "query" .`                           | `argrep -x "query" .`                         |
| Quiet (exit code only) | `grep -rq "query" .`                           | `argrep -q "query" .`                         |
| Only matched text      | `grep -rho "query" .`                          | `argrep -o "query" .`                         |
| Limit matches          | `grep -rm 1 "query" .`                         | `argrep -m 1 "query" .`                       |
| Exclude files          | `grep -r --exclude='*.min.js' "query" .`       | `argrep --exclude '*.min.js' "query" .`       |
| Exclude directories    | `grep -r --exclude-dir=node_modules "query" .` | `argrep --exclude-dir node_modules "query" .` |
| Show line numbers      | `grep -rn "query" .`                           | `argrep -n "query" .`                         |
| Context lines          | `grep -C 2 "query" .`                          | `argrep -C 2 "query" .`                       |
| Files only             | `grep -rl "query" .`                           | `argrep -l "query" .`                         |
| Files without match    | `grep -rL "query" .`                           | `argrep -L "query" .`                         |
| Count per file         | `grep -rc "query" .`                           | `argrep -c "query" .`                         |
| Invert match           | `grep -rv "query" .`                           | `argrep -v "query" .`                         |
| File type filter       | `grep -r --include="*.rs" "query" .`           | `argrep --include "*.rs" "query" .`           |
| Skip binary files      | `grep -rI "query" .`                           | automatic                                     |
| Skip node_modules      | `grep -r --exclude-dir=node_modules`           | automatic                                     |
| Pipe from stdin        | `cmd \| grep "query"`                          | `cmd \| argrep "query"`                       |

## Key advantages over `grep`

- **Parallel traversal** — scales with CPU cores, significantly faster on large codebases
- **Binary skipping** — no `-I` flag needed, binaries are automatically detected and skipped
- **Smart ignores** — `target/`, `node_modules/`, `.git/` skipped automatically
- **stdin support** — works as a drop-in in pipes without any special flags
- **Colored output** — matched filenames in magenta, line numbers in green, query highlighted in red

## Exit codes

| Code | Meaning                                                                                                                                                 |
| ---- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `0`  | Success — every file was read, matches or not                                                                                                           |
| `1`  | A file or directory could not be read (permission denied, I/O error), an invalid regex `QUERY`, or another config error (e.g. invalid `--include` glob) |
| `2`  | Invalid CLI usage — bad or missing flag (e.g. `-j 0`, missing `QUERY`)                                                                                  |

A nonzero exit from an unreadable file doesn't mean the search stopped: every file that *could* be read is still searched and its matches printed. Run with `--debug` to see which paths failed and why; without it you still get a one-line summary and the nonzero exit code, so it can't be mistaken for "no matches found".

**With `-q`/`--quiet`, the exit-code meaning changes** to match grep's own convention instead of the table above:

| Code | Meaning (only when `-q` is set)                                                                    |
|------|----------------------------------------------------------------------------------------------------|
| `0`  | At least one match was found                                                                       |
| `1`  | No matches were found (no error)                                                                   |
| `2`  | An error occurred — invalid regex, invalid `--include` glob, or a file/directory could not be read |
