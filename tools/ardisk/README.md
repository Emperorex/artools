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
| `--largest-files N` | —     | —         | List the `N` largest individual files instead of the directory report (see [Finding the largest files](#finding-the-largest-files)) |
| `--by-type`         | —     | —         | Show files and total size per file extension, with each type's share (see [Breakdown by file type](#breakdown-by-file-type)) |
| `--duplicates`      | —     | —         | Report groups of files with identical content; read-only (see [Finding duplicate files](#finding-duplicate-files)) |
| `--min-size SIZE`   | —     | `1B`      | With `--duplicates`: ignore files smaller than `SIZE` (e.g. `1MB`) |
| `--inodes`          | —     | —         | Count inodes instead of bytes, like `du --inodes` (see [Counting inodes](#counting-inodes)) |
| `--json`            | —     | —         | Print the result as one JSON document on stdout (see [JSON output](#json-output)) |
| `--include PATTERN` | —     | —         | Only count files matching this glob pattern (e.g. `"*.rs"`, `"*.mp4"`) |
| `--exclude GLOB`    | —     | —         | Exclude files/dirs matching a gitignore-style glob; repeatable (see [Excluding paths](#excluding-paths)) |
| `--no-hidden`       | —     | —         | Skip files and directories whose name starts with `.` (see [Hidden files](#hidden-files)) |
| `--apparent-size`   | —     | —         | Use logical file sizes instead of block allocation — matches `du -sh`  |
| `--jobs N`          | `-j`  | CPU-aware | Number of parallel worker threads (1–128; default is half the available cores, clamped to 1–16) |
| `--debug`           | `-d`  | —         | Print scan statistics and errors to stderr                             |

## Size units

Supported in `--threshold`: `B`, `KB`, `MB`, `GB`, `TB` (case-insensitive).

```bash
ardisk . --threshold 500MB
ardisk . --threshold 1.5GB
```

## Hidden files

Like `du`, ardisk counts hidden files and directories (names starting with `.`) by default, so totals match `du -sh`. Pass `--no-hidden` to leave them out:

```bash
ardisk ~ --no-hidden             # only non-dot entries
ardisk ~ --by-type               # includes ~/.cache, ~/.cargo, ...
ardisk ~/project --no-ignore     # also counts .git
```

- A hidden directory is **not entered**, so everything below it is skipped, whatever the names down there are.
- `--no-hidden` and `--no-ignore` are independent: `--no-ignore` re-enables `.git`, `node_modules` and `.gitignore`d paths, `--no-hidden` removes dot-entries. Use both for "everything that is not a dotfile".
- Works with every mode (`--by-type`, `--largest-files`, `--duplicates`, `--json`) and with `--ignore`, `--include` and `--exclude`; an entry is counted only if every filter lets it through.
- A hard link whose hidden name is skipped is still counted through its visible name.
- The scan root is never skipped: `ardisk ~/.cache --no-hidden` scans `~/.cache`, but not its dot-entries.
- Only the leading dot matters (`.env`, `.cache`); the Windows "hidden" attribute is not consulted.

## Default ignores

The following directories are always skipped:

- `.git`
- `node_modules`
- `__pycache__`

## Finding the largest files

`--largest-files N` lists the `N` biggest individual files, largest first, instead of the per-directory report:

```bash
ardisk ~ --largest-files 20
```

```
  2.31 GB  /Users/me/Movies/raw/take3.mov
  1.12 GB  /Users/me/Library/Caches/big.bin
 ...
```

What counts as a candidate is exactly what counts toward the directory totals:

- **Same filters as the directory report.** `--include`, `--exclude`, `--ignore`, the built-in ignores and `.gitignore`/`.ignore` rules all apply, so `ardisk . --largest-files 10 --include "*.mp4"` finds the biggest videos, and files you excluded can never show up.
- **Same sizes.** Physical block allocation by default, logical length with `--apparent-size`, so the numbers agree with the directory report.
- **Hard links are listed once.** A file with several names (hard links) appears as a single entry. Which name is shown depends on which one the parallel scan reached first and can differ between runs; the size and the set of other files are unaffected. A name hidden by `--exclude`/`--include` never "uses up" the file, so another name that passes the filters is still reported.
- **Symlinks are never listed** (and never followed), and directories are never listed.
- **Ties** (equal sizes) are ordered by path, so the output is deterministic.
- If the tree has fewer than `N` files, all of them are printed.

`--largest-files` replaces the directory report, so it cannot be combined with `--summarize`, `--top`, `--max-depth` or `--threshold`. Memory use is proportional to `N` times the number of worker threads, not to the number of files scanned.

## Breakdown by file type

`--by-type` answers "what kinds of data take the space?", where `--largest-files` answers "which files?":

```bash
ardisk ~ --by-type --top 6
```

```
Extension         Files       Size   Share
------------------------------------------
.mp4                124   18.40 GB   43.2%
.jpg               8321    7.20 GB   16.9%
.zip                 86    4.80 GB   11.3%
.rs               12400    1.10 GB    2.6%
.pdf                412  820.30 MB    1.9%
(no extension)     1932  410.20 MB    0.9%
(35 other types)   1725    9.89 GB   23.2%
------------------------------------------
Total             25000   42.59 GB  100.0%
```

Rows are ordered by size, biggest first; types of equal size are ordered by extension, and files without an extension come first among equals. `--top N` (default 20) limits the rows: the remaining types are summed into one `(N other types)` row, so the shares always add up to 100%. `--summarize` prints just one line with the totals.

**What counts as the extension.** The part of the file name after the **last** `.`, compared case-insensitively:

| Name           | Type            |
|----------------|-----------------|
| `archive.zip`  | `.zip`          |
| `foo.tar.gz`   | `.gz` (compound extensions are not recognized) |
| `PHOTO.JPG`    | `.jpg` (same type as `photo.jpg`) |
| `.config.json` | `.json`         |
| `README`       | no extension    |
| `.env`, `.gitignore` | no extension (a leading dot marks a hidden file) |
| `foo.`         | no extension    |

**What is counted.**

- The same files as the directory report: `--include`, `--exclude`, `--ignore`, the built-in ignores and `.gitignore` rules apply, symlinks are never followed, and a hard-linked file counts once.
- Sizes follow `--apparent-size`: physical block allocation by default, logical length with the flag. (`--include` patterns are case-sensitive, even though types are grouped case-insensitively.)
- The total is the sum of the **file** sizes. It is smaller than the root total of the directory report by the space taken by the directory entries themselves.
- Empty files count as files of their type (with 0 bytes).
- Memory use depends on the number of distinct extensions, not on the number of files.

`--by-type` replaces the directory report, so it cannot be combined with `--largest-files`, `--duplicates`, `--max-depth` or `--threshold`.

## Finding duplicate files

`--duplicates` reports files whose **content** is identical, whatever their names or locations. It only reports: nothing is deleted, moved or linked.

```bash
ardisk ~ --duplicates --min-size 1MB
```

```
3 identical files, 2.86 MB each, 5.72 MB reclaimable
  /Users/me/backup/photos/a-copy.raw
  /Users/me/docs/renamed.bin
  /Users/me/photos/a.raw

1 duplicate group, 2 redundant files, 5.72 MB reclaimable
```

Groups are ordered by reclaimable space (the space freed by keeping one copy of each), paths inside a group are sorted. `--top N` limits how many groups are printed (the summary line always covers all of them), and `--summarize` prints only the summary line.

**How it works.** Candidates are narrowed in three stages so that most files are never read:

1. **Length.** A file with a unique length cannot have a twin and is never opened.
2. **Prefix hash.** Files of equal length are compared by a BLAKE3 hash of their first 4 KiB. For files up to 4 KiB this is already the whole file.
3. **Full hash.** Only files that still collide are read completely.

Groups are formed from the 256-bit hash plus the length. Files are not additionally compared byte by byte. The result is a snapshot: a file modified while the search runs can be misreported, but one whose size changes while being read is detected and skipped.

**What counts.**

- Same filters as the directory report: `--include`, `--exclude`, `--ignore`, the built-in ignores and `.gitignore` rules apply, so `--include "*.jpg"` looks for duplicate photos only.
- Only regular files. Symlinks are never followed or listed.
- **Hard links are not duplicates.** Several names for one file occupy the space once, so they never form a group on their own. If a hard-linked file also has a separate copy, the group holds two entries (one of the hard-link names and the copy); which name is shown can vary between runs.
- **Empty files are never reported** (all empty files are trivially identical). `--min-size` can only raise this floor.
- Sizes in this report are **logical file lengths** (as with `--apparent-size`), because identical content means identical length; physical block allocation is not used.
- Files that cannot be read (permissions, vanished, changed) are skipped with a one-line warning on stderr; `--debug` lists them.

**Speed and memory on big trees.**

- The scan keeps one path per regular file of at least `--min-size` bytes until it has finished, so peak memory grows with the number of such files (very roughly 100-200 bytes each). Raising `--min-size` is the most effective way to cut both memory and run time; on a large tree `--min-size 1MB` usually leaves a small fraction of the files and most of the reclaimable space.
- Hashing uses `-j` threads. On spinning disks or network shares, fewer threads (`-j 1` or `-j 2`) can be faster than many.
- `--debug` prints how many files reached each stage and how many bytes were read.

`--duplicates` replaces the directory report, so it cannot be combined with `--largest-files`, `--max-depth` or `--threshold`.

## Counting inodes

When a filesystem fails with "No space left on device" while `df` still shows free space, it has usually run out of **inodes**, not bytes. `--inodes` counts them per directory, like `du --inodes`:

```bash
ardisk /var --inodes --top 5
ardisk /var --inodes --threshold 10000   # only directories with >= 10000 inodes
ardisk /var --inodes -s                  # just the total
```

```
    412873  /var
    398212  /var/lib
    391004  /var/lib/docker
      6120  /var/cache
      ...
```

- Every directory (the scanned one included), regular file, symlink and special file counts as **1**, whatever its size. Symlinks are counted but never followed.
- A hard-linked inode counts once per scan, as in `du`.
- Counts roll up: a directory's number includes everything below it. The list is sorted by count, biggest first; ties are ordered by path.
- `--threshold` is a plain whole number here (`10000`), not a size.
- Works with `--top`, `--max-depth`, `--summarize`, `--json`, `--include` (matched against file and symlink names; directories always cost their own inode), `--exclude`, `--ignore`, `--no-ignore` and `--no-hidden`. By default `.git`, `node_modules` and `.gitignore`d paths are skipped as usual, so use `--no-ignore` to match `du --inodes` exactly.
- Cannot be combined with `--by-type`, `--duplicates`, `--largest-files`, `--min-size` or `--apparent-size`.
- Directories that cannot be read are not counted (use `--debug` to see them).

In JSON the document has `"mode": "inodes"`, no `size_mode`, `params.threshold_inodes`, `summary.total_inodes` and entries of the form `{ "path": "var/lib", "inodes": 398212, "depth": 2, "kind": "directory" }`.

## JSON output

`--json` changes only the **output format**. It does not turn on `--largest-files`, `--duplicates` or `--by-type`; it serializes the result of whichever mode you picked:

```bash
ardisk . --json                          # directory report
ardisk . --largest-files 20 --json       # largest files
ardisk . --duplicates --json             # duplicate files
ardisk . --by-type --json                # files and bytes per extension
ardisk ~/Projects --exclude 'target/**' --json
```

Only the JSON document goes to **stdout**. Warnings and `--debug` diagnostics go to **stderr**, so the output can be piped to `jq` or saved to a file as is. If ardisk fails, nothing is written to stdout and the exit code is non-zero.

### Document layout (`schema_version` 1)

Every document is one object with the same header:

| Key              | Meaning |
|------------------|---------|
| `schema_version` | Integer. Changes only for incompatible changes. New keys may be added without a bump, so ignore keys you don't know. |
| `mode`           | `"directories"`, `"largest_files"`, `"duplicates"`, `"by_type"` or `"inodes"` |
| `root`           | Canonical absolute path that was scanned |
| `size_mode`      | Absent for `inodes`. `"disk"` (block allocation, the default) or `"apparent"` (logical length, `--apparent-size`). Always `"apparent"` for `duplicates`. |
| `filters`        | `include` (string or `null`), `exclude` (array), `ignore` (array, extra `--ignore` names only), `no_ignore` (bool), `no_hidden` (bool) |
| `params`         | Options that shape the result, mode-specific (below) |
| `summary`        | Totals for the **whole scan**, mode-specific (below) |
| `entries` / `groups` / `types` | The listed items |
| `truncated`      | `true` if more items existed than were listed |

Paths in `entries` and `groups` are **relative to `root`**, use `/` as separator, and the root itself is `"."`. Sizes are integers in bytes. A path that is not valid UTF-8 is written with the invalid bytes replaced by U+FFFD, and ardisk prints a warning to stderr.

The list obeys the same limits as the text report: `--top` (default 20) for directories, duplicate groups and file types, `--max-depth`, `--threshold`, and `--summarize`, which lists nothing and keeps only `summary`. Use `truncated` to tell whether a list was cut, and raise `--top` to get more. Ties in size are ordered by path, so output does not depend on thread count or timing.

**`directories`**

```json
{
  "schema_version": 1,
  "mode": "directories",
  "root": "/home/me/project",
  "size_mode": "disk",
  "filters": { "include": null, "exclude": ["target/**"], "ignore": [], "no_ignore": false, "no_hidden": false },
  "params": { "top": 20, "max_depth": null, "threshold_bytes": null },
  "summary": { "total_bytes": 1073741824, "files": 15230, "directories": 842 },
  "entries": [
    { "path": ".",    "bytes": 1073741824, "depth": 0, "kind": "directory" },
    { "path": "data", "bytes": 734003200,  "depth": 1, "kind": "directory" }
  ],
  "truncated": true
}
```

`summary.total_bytes` is the size of the root (what `--summarize` prints). `files` counts regular files that contributed to the totals (a hard-linked file counts once; symlinks and filtered-out files not at all). `directories` counts directories visited, including the root.

**`largest_files`**

```json
{
  "schema_version": 1,
  "mode": "largest_files",
  "params": { "limit": 20 },
  "summary": { "total_bytes": 1073741824, "files": 15230 },
  "entries": [
    { "path": "data/archive.tar", "bytes": 524288000, "kind": "file" }
  ],
  "truncated": true
}
```

(header keys omitted for brevity). Files are listed largest first. `truncated` is `true` when the scan contained more than `limit` files.

**`duplicates`**

```json
{
  "schema_version": 1,
  "mode": "duplicates",
  "params": { "top": 20, "min_size_bytes": 1 },
  "summary": {
    "groups": 1,
    "duplicate_files": 2,
    "redundant_files": 1,
    "potentially_reclaimable_bytes": 52428800,
    "files_considered": 9120,
    "unreadable_files": 0
  },
  "groups": [
    {
      "hash": "blake3:3fcfb222e40513a26f6990c23f423c0c14cf40955b2190a1010e7c6b1a003007",
      "bytes_per_file": 52428800,
      "files": ["backup/archive1.zip", "backup/archive2.zip"],
      "potentially_reclaimable_bytes": 52428800
    }
  ],
  "truncated": false
}
```

- `hash` is the BLAKE3 digest of the full content shared by the files in the group.
- `duplicate_files` counts every file that belongs to a group, including the copy you would keep; `redundant_files` is that number minus one per group.
- `files_considered` is the number of files examined (regular files of at least `min_size_bytes`), which tells "no duplicates" apart from "nothing was looked at". `unreadable_files` counts files that were skipped because they could not be read; if it is not zero the result may be incomplete.
- `summary` covers every group found; `groups` holds the `--top` groups with the most reclaimable space.

**Why "potentially" reclaimable.** `potentially_reclaimable_bytes` is `bytes_per_file × (files − 1)` per group, and the summary is the sum over all groups: the space freed if you keep **one** copy of each group and delete the rest. It is not the sum of all sizes in the group. The real saving can be smaller: the same data may also be hard-linked from places outside the scanned tree, shared by snapshots, clones or deduplicating filesystems, or kept alive by other references. Sizes here are logical lengths, not disk blocks.

**`by_type`**

```json
{
  "schema_version": 1,
  "mode": "by_type",
  "params": { "top": 20 },
  "summary": { "total_bytes": 45731340288, "files": 23275, "types": 41 },
  "types": [
    { "extension": ".mp4", "files": 124,  "bytes": 19756849521 },
    { "extension": ".jpg", "files": 8321, "bytes": 7730941132 },
    { "extension": null,   "files": 1932, "bytes": 430143283 }
  ],
  "truncated": true
}
```

(header keys omitted for brevity).

- `extension` is lower-cased and includes the dot; it is `null` for files without an extension. See [Breakdown by file type](#breakdown-by-file-type) for the rule.
- Types are ordered by `bytes` descending, then by `extension` ascending (`null` first).
- `summary.total_bytes` is the sum of `bytes` over **all** types, not just the listed ones, and `summary.types` is the number of types found; `truncated` is `true` if the list is shorter than that.
- Shares are not stored. Compute them from the bytes (`bytes / summary.total_bytes`), which avoids rounding differences between consumers.

### Examples with `jq`

```bash
# total size of a tree, in bytes
ardisk . --json | jq .summary.total_bytes

# paths of the 20 largest files
ardisk . --largest-files 20 --json | jq -r '.entries[].path'

# share of each file type, in percent
ardisk ~ --by-type --top 100 --json \
  | jq '.summary.total_bytes as $t | .types[] | {extension, share: (.bytes / $t * 100)}'

# how much a duplicate cleanup could free, in MiB
ardisk ~ --duplicates --min-size 1MB --summarize --json \
  | jq '.summary.potentially_reclaimable_bytes / 1048576'

# save a snapshot for later comparison
ardisk ~/Projects --exclude 'target/**' --json > projects.json
```

## Excluding paths

`--exclude GLOB` removes matching entries from the scan. It can be repeated, and patterns use gitignore syntax, matched against paths relative to `PATH`:

| Pattern        | Effect                                                                             |
|----------------|------------------------------------------------------------------------------------|
| `*.log`        | No `/` → matches the name at **any depth**                                         |
| `/build`       | Leading `/` → anchored to `PATH`                                                   |
| `target/**`    | Contains `/` → anchored to `PATH`; excludes everything **inside** `target`, not `target` itself |
| `**/target/**` | Same, at any depth                                                                 |
| `cache/`       | Trailing `/` → directories only                                                    |
| `src/**/*.rs`  | Files only; never stops `src` or its subdirectories from being traversed           |
| `!keep.log`    | Re-includes a path matched by an earlier pattern                                   |

Notes:

- A pattern only prunes a directory when it matches **that directory's own path**. A contents-only pattern such as `target/**` keeps `target` in the report (with just its own entry size) but drops everything inside it.
- Like gitignore, a pattern that matches a *directory's* name excludes that whole directory: `*.log` would also drop a directory called `x.log`. Add a trailing `/` to target directories only, or anchor the pattern (`/logs/*.log`) when that matters.
- As in git, `!pattern` cannot rescue a path below an excluded directory.
- `--exclude` always applies, including with `--no-ignore`, and wins over `--include`.
- Excluded entries count as 0 bytes in every total.
- Quote patterns so the shell doesn't expand them: `--exclude '*.log'`.

```bash
ardisk . --exclude '*.log' --exclude 'target/**'
```

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

# 20 largest files under the home directory
ardisk ~ --largest-files 20

# 10 largest videos, ignoring a scratch directory
ardisk ~/Movies --largest-files 10 --include "*.mp4" --exclude 'scratch/**'

# Machine-readable output for scripts
ardisk . --largest-files 20 --json | jq -r '.entries[].path'

# Which file types take the space?
ardisk ~ --by-type

# ...only counting a project, without build output
ardisk ~/Projects --by-type --exclude 'target/**' --top 10

# Duplicate files of at least 1 MB, show the 10 most wasteful groups
ardisk ~ --duplicates --min-size 1MB --top 10

# Ignore log files and the build directory
ardisk . --exclude '*.log' --exclude 'target/**'

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
| Space per file type | not supported                      | `ardisk . --by-type`                        |
| Biggest files       | `find . -type f -printf '%s %p\n' \| sort -rn \| head` | `ardisk . --largest-files 20`      |
| Skip node_modules   | `--exclude=node_modules`           | automatic                                   |
| Exclude by glob     | `--exclude='*.log'`                | `ardisk . --exclude '*.log'`                |

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
| `1`  | Invalid `--threshold`/`--include`/`--exclude` value or config error |
| `2`  | Invalid CLI usage — bad or missing flag (e.g. `-j 0`, `--largest-files 0`) |
