# argrep

Fast parallel text search utility — a drop-in alternative to `grep`.

## Installation

```bash
curl -fsSL https://artools.io/install.sh | bash -s -- argrep
```

Or build from source:

```bash
cargo build --release --bin argrep
```

## Usage

```
argrep [OPTIONS] QUERY [PATH]
```

`QUERY` is required. `PATH` defaults to `.` (current directory) if not specified.

`argrep` also reads from **stdin** when used in a pipeline — no path argument needed.

## Options

| Flag                   | Short | Default | Description                                                     |
|------------------------|-------|---------|-----------------------------------------------------------------|
| `--ignore-case`        | `-i`  | —       | Case-insensitive matching                                       |
| `--line-number`        | `-n`  | —       | Show line numbers in output                                     |
| `--before-context NUM` | `-B`  | —       | Show NUM lines of leading context before matches                |
| `--after-context NUM`  | `-A`  | —       | Show NUM lines of trailing context after matches                |
| `--context NUM`        | `-C`  | —       | Show NUM lines of leading and trailing context around matches   |
| `--invert`             | `-v`  | —       | Print lines that do NOT match the query                         |
| `--files-with-matches` | `-l`  | —       | Print only filenames of files containing a match                |
| `--count`              | `-c`  | —       | Print count of matching lines per file                          |
| `--include PATTERN`    | —     | —       | Only search files matching this glob (e.g. `"*.rs"`, `"*.log"`) |
| `--jobs N`             | `-j`  | `4`     | Number of parallel worker threads (must be ≥ 1)                 |
| `--debug`              | `-d`  | —       | Print scan statistics and errors to stderr                      |

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

```bash
# Search for a term in current directory
argrep "TODO" .

# Search with line numbers
argrep "error" /var/log -n

# Case-insensitive search
argrep "todo" ./src -i -n
```

### Filtering

```bash
# Only search Rust files
argrep "unwrap" . --include "*.rs"

# Only search log files
argrep "error" /var/log --include "*.log"

# Search multiple levels with specific extension
argrep "panic" . --include "*.rs" -n -i
```

### Output modes

```bash
# List only filenames that contain a match
argrep "TODO" . -l

# Count matching lines per file
argrep "error" /var/log --include "*.log" -c

# Invert — show lines that don't contain the query
argrep "ok" ./results.txt -v
```

### Stdin / pipeline mode

When piped from another command, `argrep` reads from stdin automatically:

```bash
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

```bash
# Find files containing TODOs, case-insensitive, Rust files only
argrep "todo" . -i -l --include "*.rs"

# Show line numbers for errors in logs, count per file
argrep "error" /var/log -c --include "*.log"

# Search with 8 workers on a large codebase
argrep "deprecated" /large/project -j 8 --include "*.py" -n
```

## Comparison with `grep`

| Task              | `grep`                               | `argrep`                            |
|-------------------|--------------------------------------|-------------------------------------|
| Recursive search  | `grep -r "query" .`                  | `argrep "query" .`                  |
| Case-insensitive  | `grep -ri "query" .`                 | `argrep "query" . -i`               |
| Show line numbers | `grep -rn "query" .`                 | `argrep "query" . -n`               |
| Context lines     | `grep -C 2 "query" .`                | `argrep "query" . -C 2`             |
| Files only        | `grep -rl "query" .`                 | `argrep "query" . -l`               |
| Count per file    | `grep -rc "query" .`                 | `argrep "query" . -c`               |
| Invert match      | `grep -rv "query" .`                 | `argrep "query" . -v`               |
| File type filter  | `grep -r --include="*.rs"`           | `argrep "query" . --include "*.rs"` |
| Skip binary files | `grep -rI "query" .`                 | automatic                           |
| Skip node_modules | `grep -r --exclude-dir=node_modules` | automatic                           |
| Pipe from stdin   | `cmd \| grep "query"`                | `cmd \| argrep "query"`             |

## Key advantages over `grep`

- **Parallel traversal** — scales with CPU cores, significantly faster on large codebases
- **Binary skipping** — no `-I` flag needed, binaries are automatically detected and skipped
- **Smart ignores** — `target/`, `node_modules/`, `.git/` skipped automatically
- **stdin support** — works as a drop-in in pipes without any special flags
- **Colored output** — matched filenames in magenta, line numbers in green, query highlighted in red

## Exit codes

| Code | Meaning                                                                                                                       |
|------|-------------------------------------------------------------------------------------------------------------------------------|
| `0`  | Success — every file was read, matches or not                                                                                 |
| `1`  | A file or directory could not be read (permission denied, I/O error), or another config error (e.g. invalid `--include` glob) |
| `2`  | Invalid CLI usage — bad or missing flag (e.g. `-j 0`, missing `QUERY`)                                                        |

A nonzero exit from an unreadable file doesn't mean the search stopped: every file that *could* be read is still searched and its matches printed. Run with `--debug` to see which paths failed and why; without it you still get a one-line summary and the nonzero exit code, so it can't be mistaken for "no matches found".