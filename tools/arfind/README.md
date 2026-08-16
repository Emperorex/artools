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
| `--ignore DIR`       | —     | —         | Additional directory names to skip (repeatable)                                 |
| `--no-ignore`        | —     | —         | Search everything — disables `.gitignore`/`.ignore` respect and default ignores |
| `--hidden`           | `-H`  | —         | Include hidden files and directories (dotfiles)                                 |
| `--jobs N`           | `-j`  | `4`       | Number of parallel worker threads                                               |
| `--debug`            | `-d`  | —         | Print scan statistics and errors to stderr                                      |

## Default ignores

The following directories are skipped by default:

- `.git`
- `node_modules`
- `__pycache__`

Use `--ignore DIR` to skip additional directories by name (this is always
honored, even with `--no-ignore`). Use `--no-ignore` to disable the defaults
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
arfind ./src --name "*.go" --ignore vendor

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

## Performance

`arfind` uses parallel worker threads (default: 4) to traverse directory trees concurrently. On NVMe storage with large trees, this is significantly faster than single-threaded `find`. Tune with `-j` to match your hardware.

## Exit codes

| Code | Meaning                                  |
|------|------------------------------------------|
| `0`  | Success (even if no matches found)       |
| `1`  | Invalid arguments or configuration error |
