# arfind

Fast parallel file and directory finder — a drop-in alternative to `find`.

## Installation

```bash
curl -fsSL https://artools.io/install.sh | bash -s -- arfind
```

Or build from source:

```bash
cargo build --release --bin arfind
```

## Usage

```
arfind [OPTIONS] [PATH]
```

`PATH` defaults to `.` (current directory) if not specified.

## Options

| Flag                 | Short | Default   | Description                                                                     |
|----------------------|-------|-----------|---------------------------------------------------------------------------------|
| `--name PATTERN`     | `-n`  | `*`       | Glob pattern to match filenames (e.g. `"*.rs"`, `"Cargo.*"`)                    |
| `--case-insensitive` | `-i`  | —         | Match `--name` pattern case-insensitively                                       |
| `--type TYPE`        | `-t`  | —         | Filter by entry type: `f` (file), `d` (directory), `l` (symlink)                |
| `--size SIZE`        | —     | —         | Filter by file size: `100MB` or `+100MB` (larger than), `-1KB` (smaller than)   |
| `--empty`            | `-e`  | —         | Match only empty files (0 bytes) or empty directories (no children)             |
| `--count`            | `-c`  | —         | Print total match count instead of paths                                        |
| `--max-depth N`      | —     | unlimited | Maximum directory depth to recurse into                                         |
| `--exclude GLOB`     | —     | —         | Hide files/dirs matching a gitignore-style glob, e.g. `"*.log"`, `"target/**"`, `"build/"`; repeatable (see [Excluding paths](#excluding-paths)) |
| `--exclude-dir NAME` | —     | —         | Don't descend into directories with this name or glob (e.g. `vendor`, `"build*"`); repeatable; alias: `--ignore` |
| `--older-than AGE`   | —     | —         | Only match entries last modified at least `AGE` ago, e.g. `30d`, `6mo`, `1y` (see [Filtering by age](#filtering-by-age)) |
| `--newer-than AGE`   | —     | —         | Only match entries modified less than `AGE` ago, e.g. `7d`, `12h`               |
| `--one-file-system`  | `-x`  | —         | Don't descend into other filesystems or volumes, like `find -xdev` (see [Staying on one filesystem](#staying-on-one-filesystem)) |
| `--no-ignore`        | —     | —         | Search everything — disables `.gitignore`/`.ignore` respect and default ignores |
| `--hidden`           | `-H`  | —         | Include hidden files and directories (dotfiles)                                 |
| `--jobs N`           | `-j`  | CPU-aware | Number of parallel worker threads (1–128; default is half the available cores, clamped to 1–16) |
| `--debug`            | `-d`  | —         | Print scan statistics and errors to stderr                                      |

## Default ignores

The following directories are skipped by default:

- `.git`
- `node_modules`
- `__pycache__`

Use `--exclude-dir NAME` (alias `--ignore`) to skip additional directories by
name or glob (this is always honored, even with `--no-ignore`). Use `--no-ignore` to disable the defaults
above as well as `.gitignore`/`.ignore` respect — i.e. to search truly
everything.

## Gitignore support

By default, `arfind` respects `.gitignore` and `.ignore` files found within the
searched directory tree — matched files and directories (and everything under
an ignored directory) are excluded automatically, the same way `fd` behaves.
Nested `.gitignore` files are supported, including `!negation` patterns that
re-include something an ancestor `.gitignore` excluded.

Pass `--no-ignore` to search everything: this disables both `.gitignore`/`.ignore`
respect and the built-in default ignores (`.git`, `node_modules`, `__pycache__`):

```bash
arfind . --name "*.log" --no-ignore
```

**Note:** only `.gitignore`/`.ignore` files inside the path you're searching
are read. `arfind` does not walk up to a repository root for parent ignore
files, and does not read `.git/info/exclude` or a global git ignore file.

## Excluding paths

arfind has two exclusion flags, matching its sibling tools:

- **`--exclude GLOB`** works like ardisk's `--exclude`. It uses gitignore syntax, and matching entries are neither listed nor searched:

  | Pattern       | Meaning                                                                 |
  |---------------|-------------------------------------------------------------------------|
  | `*.log`       | No `/` → matches the name at any depth (files *and* directories)        |
  | `src/gen`     | Contains `/` → anchored to `PATH`                                       |
  | `build/`      | Trailing `/` → directories only                                         |
  | `target/**`   | Everything *inside* `target`; `target` itself is still listed           |
  | `!keep.log`   | Re-includes a path matched by an earlier pattern                        |

- **`--exclude-dir NAME|GLOB`** works like argrep's `--exclude-dir` and is the same mechanism as the default ignores. It matches a directory's **name** (plain name or glob like `build*`), and the directory is listed but its contents are not searched. `--ignore` is an alias.

Both are honored even with `--no-ignore`, and both are repeatable. The search root itself is never excluded. Quote patterns so the shell doesn't expand them.

```bash
arfind . --exclude '*.log' --exclude 'target/**'
arfind . -t f --exclude-dir vendor --exclude-dir 'build*'
arfind . --exclude 'node_modules/'               # hide node_modules entirely, not just its contents
```

## Filtering by age

`--older-than` and `--newer-than` test each entry's **modification time** (mtime), like `find -mtime`. Units are the same as ardisk's:

| Unit  | Meaning           |
|-------|-------------------|
| `s`   | seconds           |
| `min` | minutes           |
| `h`   | hours             |
| `d`   | days              |
| `w`   | weeks             |
| `mo`  | months (30 days)  |
| `y`   | years (365 days)  |

```bash
arfind ~/Downloads -t f --older-than 6mo        # stale downloads
arfind . -t f --newer-than 1d                   # changed today
arfind /var/log --name "*.log" --newer-than 1y --older-than 30d   # a window
```

- An age is a whole number with a unit. A bare `m` is rejected, since it could mean minutes or months.
- Ages count back from the moment arfind starts. `--older-than 30d` matches entries modified **at or before** that point, and `--newer-than 30d` those modified **after** it, so for the same age the two split all entries exactly in two.
- Together they select a window, which needs `--older-than` shorter than `--newer-than`; anything else is an error.
- The test applies to every listed entry: files, directories and symlinks (the link's own mtime, like `find` without `-L`). Combine with `-t f` for files only. Directories are always searched, whatever their own age.

## Staying on one filesystem

`-x` / `--one-file-system` keeps the search on the filesystem `PATH` is on, like `find -xdev`. A directory on another filesystem (a mount point, an external disk, a network share, another APFS volume) is still **listed**, but arfind does not descend into it.

```bash
arfind / -x --name "*.conf"
arfind /System/Volumes/Data -x -t f --size +1GB   # macOS: just the data volume
```

On macOS, `/` and your data live on separate APFS volumes joined by firmlinks (`/Users`, `/Applications`, ...), so `arfind / -x` stays on the small system volume. Search `/System/Volumes/Data` to cover your files. `-x` is Unix-only; elsewhere arfind prints a warning and searches normally. With `--debug`, each mount point it does not enter is reported on stderr.

## Examples

```bash
# Find all Rust source files
arfind . --name "*.rs"

# Find directories named "src" up to depth 2
arfind . -t d --name "src" --max-depth 2

# Find files larger than 100MB
arfind . -t f --size +100MB

# Find files smaller than 1KB
arfind . -t f --size -1KB

# Find empty files
arfind . -t f --empty

# Count how many .log files exist
arfind /var/log --name "*.log" --count

# Include hidden files in the search
arfind . --name "*.env" -H

# Case-insensitive name matching
arfind . --name "readme*" -i

# Search everything, including .gitignore'd files (e.g. build output)
arfind . --name "*.log" --no-ignore

# Search a specific directory, skip vendor
arfind ./src --name "*.go" --exclude-dir vendor

# Everything except logs and build output
arfind . -t f --exclude '*.log' --exclude 'target/**'

# Files not touched for a year
arfind ~ -t f --older-than 1y

# Stay on this volume
arfind / -x --name "*.conf"

# Use multiple workers for large trees
arfind /home -j 8 --name "*.txt"
```

## Comparison with `find`

| Task                 | `find`                                 | `arfind`                             |
|----------------------|----------------------------------------|--------------------------------------|
| Find by name         | `find . -name "*.rs"`                  | `arfind . --name "*.rs"`             |
| Find files only      | `find . -type f`                       | `arfind . -t f`                      |
| Limit depth          | `find . -maxdepth 2`                   | `arfind . --max-depth 2`             |
| Skip directory       | `find . -not -path '*/node_modules/*'` | automatic                            |
| Respect `.gitignore` | manual (`grep`/exclude flags)          | automatic (`--no-ignore` to disable) |
| Count results        | `find . \| wc -l`                      | `arfind . --count`                   |
| Find empty files     | `find . -empty -type f`                | `arfind . -t f --empty`              |
| Find large files     | `find . -size +100M`                   | `arfind . --size +100MB`             |
| Modified > 30 days   | `find . -mtime +30`                    | `arfind . --older-than 30d`          |
| Modified < 1 day     | `find . -mtime -1`                     | `arfind . --newer-than 1d`           |
| Stay on one fs       | `find . -xdev`                         | `arfind . -x`                        |
| Exclude by glob      | `find . -not -name '*.log'`            | `arfind . --exclude '*.log'`         |

## Performance

`arfind` uses parallel worker threads (default: 4) to traverse directory trees concurrently. On NVMe storage with large trees, this is significantly faster than single-threaded `find`. Tune with `-j` to match your hardware.

## Exit codes

| Code | Meaning                                                  |
|------|----------------------------------------------------------|
| `0`  | Success (even if no matches found)                       |
| `1`  | Invalid `--size`, `--exclude`, `--exclude-dir` or `--type` value, or an empty `--older-than`/`--newer-than` window |
| `2`  | Invalid CLI usage — bad or missing flag (e.g. `-j 0`)    |
