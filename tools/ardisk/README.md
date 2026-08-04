# ardisk

Fast parallel disk usage analyzer — a drop-in alternative to `du`.

## Installation

```bash
curl -fsSL https://artools.io/install.sh | bash -s -- ardisk
```

Or build from source:

```bash
cargo build --release --bin ardisk
```

## Usage

```
ardisk [OPTIONS] [PATH]
```

`PATH` defaults to `.` (current directory) if not specified.

## Options

| Flag                | Short | Default   | Description                                                            |
|---------------------|-------|-----------|------------------------------------------------------------------------|
| `--top N`           | `-n`  | `20`      | Number of top directories to display                                   |
| `--max-depth N`     | —     | unlimited | Maximum depth of directories to display in the report                  |
| `--threshold SIZE`  | —     | —         | Only show directories larger than this size (e.g. `100MB`, `1GB`)      |
| `--summarize`       | `-s`  | —         | Print only the grand total for the root directory                      |
| `--include PATTERN` | —     | —         | Only count files matching this glob pattern (e.g. `"*.rs"`, `"*.mp4"`) |
| `--jobs N`          | `-j`  | `4`       | Number of parallel worker threads                                      |
| `--debug`           | `-d`  | —         | Print scan statistics and errors to stderr                             |

## Size units

Supported in `--threshold`: `B`, `KB`, `MB`, `GB`, `TB` (case-insensitive).

```bash
ardisk . --threshold 500MB
ardisk . --threshold 1.5GB
```

## Default ignores

The following directories are always skipped:

- `.git`
- `node_modules`
- `__pycache__`

## How sizing works

On **macOS and Linux**, `ardisk` uses `blocks * 512` to report physical on-disk space (matching `du` behavior). On **Windows**, it uses the logical file length. Symlinks are never counted to prevent double-counting.

Sizes are rolled up bottom-up in a single pass — parent directories always include the full recursive size of all children.

## Examples

```bash
# Top 10 heaviest directories in current folder
ardisk . --top 10

# Analyze a specific path
ardisk /Users/user/Projects

# Only show directories over 500MB
ardisk . --threshold 500MB

# Only show directories over 1GB, top 5
ardisk / --threshold 1GB --top 5

# How much space do all .mp4 files use, by directory?
ardisk ~/Movies --include "*.mp4"

# Total size of all .rs files under src/
ardisk ./src --include "*.rs" --summarize

# Show only top-level breakdown (depth 1)
ardisk . --max-depth 1

# Quick total size of a directory
ardisk /var/log -s

# Use more threads on large filesystems
ardisk / -j 8 --top 20
```

## Comparison with `du`

| Task                | `du`                               | `ardisk`                     |
|---------------------|------------------------------------|------------------------------|
| Top heaviest dirs   | `du -sh * \| sort -rh \| head -10` | `ardisk . --top 10`          |
| Limit depth         | `du -d 1`                          | `ardisk . --max-depth 1`     |
| Total only          | `du -sh .`                         | `ardisk . --summarize`       |
| Filter by size      | not supported                      | `ardisk . --threshold 1GB`   |
| Filter by file type | not supported                      | `ardisk . --include "*.mp4"` |
| Skip node_modules   | `--exclude=node_modules`           | automatic                    |

## Key advantages over `du`

- `--top N` — show heaviest N directories directly, no need to pipe to `sort | head`
- `--max-depth` filters **display only** — the full tree is always scanned so parent sizes remain accurate (unlike `du -d N` which stops scanning at depth N)
- `--include PATTERN` — calculate space used by specific file types per directory
- Parallel scanning — significantly faster on large trees with NVMe storage

## Exit codes

| Code | Meaning                                  |
|------|------------------------------------------|
| `0`  | Success                                  |
| `1`  | Invalid arguments or configuration error |
