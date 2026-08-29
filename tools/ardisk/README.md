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
| `--apparent-size`   | —     | —         | Use logical file sizes instead of block allocation — matches `du -sh`  |
| `--jobs N`          | `-j`  | `4`       | Number of parallel worker threads (must be ≥ 1)                        |
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

`ardisk` supports two sizing modes:

**Physical block allocation** (default) — uses `blocks * 512` on macOS/Linux, reporting actual on-disk space including filesystem overhead. This reflects what the filesystem has reserved for each file, which can be larger than the file's logical size due to block rounding.

**Logical file size** (`--apparent-size`) — uses `metadata.len()`, the logical byte count of each file. This matches `du -sh` output on macOS and Linux and is useful when you want to compare file sizes as reported by the OS rather than physical disk consumption.

On **Windows**, logical file size is always used regardless of the flag.

Sizes are rolled up bottom-up in a single pass — parent directories always include the full recursive size of all children. Symlinks are never counted to prevent double-counting.

**Hard links** are also deduplicated: if two directory entries share the same inode (`fs::hard_link`, or `ln` without `-s`), that file is counted once, not once per name — in both physical and `--apparent-size` mode. A file with 100MB of content and two hard-linked names contributes 100MB to the total, matching how `du` itself counts hard links. Which of the two paths gets "credit" for that size in a per-directory breakdown can vary between runs, since scanning is parallel; the grand total is unaffected either way.

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

# Match du -sh output exactly
ardisk . --summarize --apparent-size

# Top directories using logical sizes
ardisk . --top 10 --apparent-size

# Use more threads on large filesystems
ardisk / -j 8 --top 20
```

## Comparison with `du`

| Task                | `du`                               | `ardisk`                                    |
|---------------------|------------------------------------|---------------------------------------------|
| Top heaviest dirs   | `du -sh * \| sort -rh \| head -10` | `ardisk . --top 10`                         |
| Limit depth         | `du -d 1`                          | `ardisk . --max-depth 1`                    |
| Total only          | `du -sh .`                         | `ardisk . --summarize --apparent-size`      |
| Physical total      | `du -s .`                          | `ardisk . --summarize`                      |
| Filter by size      | not supported                      | `ardisk . --threshold 1GB`                  |
| Filter by file type | not supported                      | `ardisk . --include "*.mp4"`                |
| Skip node_modules   | `--exclude=node_modules`           | automatic                                   |

## Key advantages over `du`

- `--top N` — show heaviest N directories directly, no need to pipe to `sort | head`
- `--max-depth` filters **display only** — the full tree is always scanned so parent sizes remain accurate (unlike `du -d N` which stops scanning at depth N)
- `--include PATTERN` — calculate space used by specific file types per directory
- `--apparent-size` — switch between physical block allocation and logical file sizes to match `du -sh` exactly
- Parallel scanning — significantly faster on large trees with NVMe storage

## Exit codes

| Code | Meaning                                                 |
|------|---------------------------------------------------------|
| `0`  | Success                                                 |
| `1`  | Invalid `--threshold`/`--include` value or config error |
| `2`  | Invalid CLI usage — bad or missing flag (e.g. `-j 0`)   |