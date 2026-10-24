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
| `--file-type KIND`   | `-t`  | —         | Filter by entry kind: `f` (file), `d` (directory), `l` (symlink)                |
| `--type NAME`        | —     | —         | Only files of a built-in type, e.g. `rust`, `python`; same names as argrep (see [File types](#file-types)); repeatable |
| `--type-not NAME`    | —     | —         | Skip files of a built-in type; repeatable; wins over `--type`/`--extension`     |
| `--type-list`        | —     | —         | Print the built-in types and their globs (`rust: *.rs`), one per line, and exit |
| `--extension EXT`    | —     | —         | Only files with this extension, e.g. `rs`, `.md`, `tar.gz` (case-insensitive); repeatable |
| `--size SIZE`        | —     | —         | Filter by file size: `100MB` or `+100MB` (larger than), `-1KB` (smaller than)   |
| `--empty`            | `-e`  | —         | Match only empty files (0 bytes) or empty directories (no children)             |
| `--count`            | `-c`  | —         | Print total match count instead of paths                                        |
| `--print0`           | `-0`  | —         | End each path with NUL instead of a newline, unescaped (for `xargs -0`; see [NUL-separated output](#nul-separated-output)) |
| `--exec CMD...`      | —     | —         | Run `CMD` once per result, without a shell (see [Running commands](#running-commands)) |
| `--exec-batch CMD...`| `-X`  | —         | Run `CMD` once with all results as arguments                                    |
| `--execdir CMD...`   | —     | —         | Like `--exec`, but run from each result's directory with `./NAME` (safer on trees others can write to) |
| `--dry-run`          | —     | —         | With `--exec`/`-X`/`--execdir`: print the commands instead of running them      |
| `--sort KEY`         | —     | —         | Sort results by `name`, `path`, `size` or `time`, ascending (see [Sorting results](#sorting-results)) |
| `--reverse`          | —     | —         | With `--sort`: largest, newest or Z-A first                                     |
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

## File types

`--type NAME` selects files by a built-in type. The names and globs are **the same as argrep's `--type`**, so `arfind . --type rust` lists exactly the files `argrep PATTERN . --type rust` would search. `arfind --type-list` prints them, one `name: globs` line per type, exactly like `argrep --type-list`:

```bash
$ arfind --type-list
rust: *.rs
python: *.py, *.pyi
...
$ arfind --type-list | grep script     # which type covers .mjs?
javascript: *.js, *.jsx, *.mjs, *.cjs
typescript: *.ts, *.tsx
```

`--type-list` takes no PATH and no other options. The built-in types are:

| Type         | Globs                             |
|--------------|-----------------------------------|
| `rust`       | `*.rs`                            |
| `python`     | `*.py`, `*.pyi`                   |
| `javascript` | `*.js`, `*.jsx`, `*.mjs`, `*.cjs` |
| `typescript` | `*.ts`, `*.tsx`                   |
| `json`       | `*.json`                          |
| `yaml`       | `*.yaml`, `*.yml`                 |
| `toml`       | `*.toml`                          |
| `markdown`   | `*.md`                            |
| `shell`      | `*.sh`, `*.bash`, `*.zsh`         |
| `go`         | `*.go`                            |
| `java`       | `*.java`                          |
| `c`          | `*.c`, `*.h`                      |
| `cpp`        | `*.cpp`, `*.cc`, `*.cxx`, `*.hpp` |
| `html`       | `*.html`, `*.htm`                 |
| `css`        | `*.css`                           |
| `sql`        | `*.sql`                           |
| `xml`        | `*.xml`                           |

For anything not in the table, use `--extension EXT` (`rs`, `.md` and `RS` all work, as do compound extensions like `tar.gz`; matching is case-insensitive, and a leading dot is a hidden file, not an extension, so `.env` has none).

```bash
arfind . --type rust                    # Rust sources
arfind . --type rust --type toml        # Rust sources and Cargo files
arfind . --extension log --older-than 30d
arfind . -t f --type-not markdown       # every file except Markdown
```

How they combine:

- Several `--type` and `--extension` values are **OR'ed**: a file of any selected type or extension matches.
- `--type-not` wins over `--type` and `--extension`.
- They are **AND'ed** with everything else: `--name`, `-t`, `--size`, `--older-than`, ...
- They select **files only**. While `--type` or `--extension` is active, directories are not listed (they are still searched); `--type-not` never hides a directory.
- `-t` is short for `--file-type` (entry kind), not for `--type`. `--type f` is an error that points you to `-t f`.
- An unknown type name is an error listing the known types and pointing to `--type-list`.

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

## NUL-separated output

`-0` / `--print0` ends each path with a NUL byte instead of a newline, like `find -print0` and `fd -0`. File names can contain spaces and even newlines, so a newline-separated list can't be split back into names reliably; NUL is the one byte a path can't contain:

```bash
arfind . --name '*.log' --older-than 30d -0 | xargs -0 rm
arfind . --type rust -0 | xargs -0 wc -l
arfind src -t f -0 | xargs -0 -n1 argrep -l0 TODO | xargs -0 git add   # with argrep -0 (one PATH per run)
```

With `-0`, paths are printed exactly as stored: no `\xHH` escaping and no color, since the reader is a program, not a terminal (see Terminal safety). `-0` can't be combined with `--count` or the exec options, which print something other than paths. For running a command on each result, `--exec`/`--execdir` avoid the pipe altogether.

## Sorting results

arfind searches with several threads, so by default results appear in the order the workers find them, which changes from run to run. `--sort` prints them in a fixed order instead:

| Key    | Order (ascending)                              | `--reverse`       |
|--------|------------------------------------------------|-------------------|
| `name` | file name only, A-Z; equal names by path       | Z-A               |
| `path` | full path, so each directory's contents stay together | Z-A        |
| `size` | smallest first                                 | largest first     |
| `time` | modification time, oldest first                | newest first      |

```bash
arfind . -t f --sort size --reverse | head -20     # the 20 largest files
arfind ~/Downloads -t f --sort time --reverse      # newest downloads first
arfind . --type rust --sort name                   # by file name, wherever they live
arfind src --type rust --sort path -0 | xargs -0 cat > all.rs   # concatenate in a stable order
```

- **Deterministic.** Names compare byte by byte (case-sensitive, like `LC_ALL=C sort`). Entries with the same key are ordered by path, ascending even with `--reverse`, so the same tree always gives the same output.
- **Output starts when the search ends.** Sorting needs every result, so nothing is printed until the whole tree has been searched, and the results are held in memory.
- **Size and time are the entry's own**, as with `--size` and `--older-than`: a symlink sorts by its own size and mtime, not its target's, and a directory by the size the filesystem reports for the directory itself, not the size of its contents (use `-t f` to sort files only, or `ardisk` for directory totals). An entry that vanishes during the search sorts last.
- **Works with `-0` and the exec options.** `--exec`/`--execdir` run, and `-X` passes paths, in the sorted order; without `--sort` they use path order. `--sort` can't be combined with `--count`.

## Running commands

`--exec` runs a command for every result, like `find -exec` or `fd -x`. `-X` / `--exec-batch` runs it once with all results, like `find -exec ... +` or `fd -X`:

```bash
arfind /var/log --name "*.log" --older-than 90d --dry-run --exec rm   # check first
arfind /var/log --name "*.log" --older-than 90d --exec rm             # then delete
arfind . --type rust --exec wc -l          # one wc per file
arfind . --type rust -X wc -l              # one wc for all files, with a total
arfind . --name "TODO*" -X "$EDITOR"       # open all matches in one editor
arfind . --extension png --exec convert {} {.}.webp
```

Everything after `--exec`/`-X` is the command, up to an argument that is exactly `;` (quote it as `';'` or `\;` in the shell) or the end of the line. **Put arfind's own options before `--exec`.**

**Placeholders** (same as `fd`), usable inside an argument (`{.}.bak`):

| Placeholder | Expands to                      | `/src/app/main.rs` |
|-------------|---------------------------------|--------------------|
| `{}`        | the path                        | `/src/app/main.rs` |
| `{/}`       | the file name                   | `main.rs`          |
| `{//}`      | the parent directory            | `/src/app`         |
| `{.}`       | the path without its extension  | `/src/app/main`    |
| `{/.}`      | the file name without extension | `main`             |

Without any placeholder, the path is appended as the last argument. With `-X` only a whole-argument `{}` is allowed, and it marks where the list of paths goes.

### Safety

Running commands on search results is powerful, so arfind adds a few guarantees:

- **No shell.** The command is started directly, with every path as one separate argument. Nothing in a file name is ever interpreted: spaces, quotes, `;`, `$(...)`, backticks and newlines are just characters. For pipes or `&&`, call a shell yourself and pass the path as an argument: `--exec sh -c 'gzip -c "$1" > "$1.gz"' sh {}`.
- **No option injection.** Results are absolute paths starting with `/`, so `{}`, `{//}` and `{.}` can never be mistaken for an option. `{/}` and `{/.}` are bare file names: when one starts an argument and the name starts with `-`, arfind passes `./-rf` instead of `-rf`, as `find -execdir` does. So `--exec ls {/}` on a file named `--version` lists `./--version` instead of running `ls --version`.
- **Search first, then run.** The whole search finishes before the first command starts, and commands run one at a time in path order (or in `--sort` order). A command that renames, moves or deletes files cannot affect which files are found, so nothing is processed twice or skipped.
- **The command can't be a result.** `--exec {}` (running the files you found) is rejected.
- **Options in the wrong place are refused.** In `arfind . --exec rm -f --dry-run`, `--dry-run` would be passed to `rm` and the files deleted for real. If an argument of the command is spelled like one of arfind's long options, arfind refuses to run anything unless the command is ended with `;`, which tells it those words really are for the command (`--exec rsync --dry-run {} host: ';'`).
- **`--dry-run`** prints each command, shell-quoted, instead of running it.
- **No results, no command.** `-X` never runs the command with an empty list (unlike plain `xargs`, which would run `rm` with no arguments). Very long lists are split over several runs to stay below the system's argument limit.
- **Errors are not hidden.** If the program cannot be started (e.g. not found), arfind stops at once. Otherwise every result is processed, failures are counted on stderr (`arfind: 2 of 40 commands failed`) and the exit status is `1`.

### Trees other people can write to: use `--execdir`

`--exec` and `-X` have the same race as `find -exec`. arfind collects the paths first and runs the commands afterwards, so in between, anyone who can write to a directory in the tree can replace it with a symlink. The command then gets an absolute path that leads through the symlink and acts on a file *outside* the tree. This matters when a privileged user runs a destructive command over a tree others can write to, for example root cleaning `/tmp`:

```bash
sudo arfind /tmp --older-than 7d -t f --exec rm      # racy: don't do this
sudo arfind /tmp --older-than 7d -t f --execdir rm   # use this instead
```

`--execdir` works like `find -execdir`. Each command runs from the result's own directory and gets `./NAME` instead of the full path (`{}` is `./NAME`, `{//}` is `.`).

- **The directory is checked when the command runs.** arfind opens it again from `/`, one directory at a time, without following symlinks, and starts the command inside exactly that directory. If a directory along the path has been swapped for a symlink since the search, that file is skipped with a message (`not running the command: a directory in its path is now a symlink…`), counted as a failure, and the others still run.
- **The program is fixed before searching.** The working directory is the untrusted one, so the command is resolved to an absolute path first. A `PATH` with a relative or empty entry (`.`, `::`) is refused, as are relative program paths like `./script`; use a command name or an absolute path.
- **Limits.** It runs one command per file (there is no batch form), and it protects the directories along the path, not the file itself. As with `find -execdir`, what the command does with `./NAME` (e.g. following a symlink at that name) is up to the command.

Commands inherit arfind's terminal, so interactive programs work. With `--exec` they run one after another (an editor opens once per file); `-X` opens them all at once. Paths are not printed while running commands, and `--count` cannot be combined with them.

## Terminal safety

File names can contain ESC and other control characters, which a terminal would otherwise execute (clearing the screen, rewriting earlier lines, changing the window title). arfind prints every such character as a visible escape: a file named `x<ESC>[2J.txt` is listed as `x\x1b[2J.txt`. This covers C0 controls (including tab and newline), DEL and C1 controls (U+0080–U+009F).

`-0`/`--print0` is the deliberate exception: it is for programs such as `xargs -0`, so it prints the exact bytes of each name (see [NUL-separated output](#nul-separated-output)).

`--dry-run` writes names that contain control characters in `$'...'` form (`$'x\x1b[2J.txt'`), so the line is safe to print and still reproduces the exact name when pasted into bash, zsh or ksh. Commands run by `--exec`/`--exec-batch` always receive the exact, unescaped name.

## Colors

On a terminal, arfind colors each result by its kind, with the same defaults as GNU `ls --color`:

| Kind                         | Color        |
|------------------------------|--------------|
| directory                    | bold blue    |
| symlink                      | bold cyan    |
| FIFO (named pipe)            | yellow       |
| socket                       | bold magenta |
| block or character device    | bold yellow  |
| regular file                 | uncolored    |

- **The entry's own kind.** A symlink is cyan even when it points to a directory, as in `ls`. The kind comes from the directory listing arfind reads anyway, so coloring adds no extra system calls.
- **Terminal only.** Piped or redirected output has no color codes, so scripts and `grep` see plain paths. `NO_COLOR` (any value) or `CLICOLOR=0` turns color off on a terminal too; `CLICOLOR_FORCE=1` keeps it in a pipe, e.g. `CLICOLOR_FORCE=1 arfind . | less -R`.
- **`-0` is never colored**, and neither are `--count` and `--dry-run` output.
- Colors are fixed for now; `LS_COLORS` is not read.

## Examples

```bash
# Find all Rust source files
arfind . --name "*.rs"
arfind . --type rust          # same, by type name (as in argrep)

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

# The 20 largest files, biggest first
arfind . -t f --sort size --reverse | head -20

# Use multiple workers for large trees
arfind /home -j 8 --name "*.txt"
```

## Comparison with `find`

| Task                 | `find`                                 | `arfind`                             |
|----------------------|----------------------------------------|--------------------------------------|
| Find by name         | `find . -name "*.rs"`                  | `arfind . --name "*.rs"`             |
| Find files only      | `find . -type f`                       | `arfind . -t f`                      |
| Find by extensions   | `find . -name '*.rs' -o -name '*.toml'` | `arfind . --extension rs --extension toml` |
| Limit depth          | `find . -maxdepth 2`                   | `arfind . --max-depth 2`             |
| Skip directory       | `find . -not -path '*/node_modules/*'` | automatic                            |
| Respect `.gitignore` | manual (`grep`/exclude flags)          | automatic (`--no-ignore` to disable) |
| Count results        | `find . \| wc -l`                      | `arfind . --count`                   |
| NUL-separated paths  | `find . -print0 \| xargs -0 …`          | `arfind . -0 \| xargs -0 …`          |
| Run per result       | `find . -name '*.rs' -exec wc -l {} \;` | `arfind . --type rust --exec wc -l`  |
| Run once for all     | `find . -name '*.rs' -exec wc -l {} +`  | `arfind . --type rust -X wc -l`      |
| Find empty files     | `find . -empty -type f`                | `arfind . -t f --empty`              |
| Find large files     | `find . -size +100M`                   | `arfind . --size +100MB`             |
| Modified > 30 days   | `find . -mtime +30`                    | `arfind . --older-than 30d`          |
| Modified < 1 day     | `find . -mtime -1`                     | `arfind . --newer-than 1d`           |
| Stay on one fs       | `find . -xdev`                         | `arfind . -x`                        |
| Largest files first  | `find . -type f -printf '%s %p\n' \| sort -rn` | `arfind . -t f --sort size --reverse` |
| Newest first         | `find . -printf '%T@ %p\n' \| sort -rn` | `arfind . --sort time --reverse`  |
| Exclude by glob      | `find . -not -name '*.log'`            | `arfind . --exclude '*.log'`         |

## Performance

`arfind` uses parallel worker threads (default: 4) to traverse directory trees concurrently. On NVMe storage with large trees, this is significantly faster than single-threaded `find`. Tune with `-j` to match your hardware.

## Exit codes

| Code | Meaning                                                  |
|------|----------------------------------------------------------|
| `0`  | Success (even if no matches found)                       |
| `1`  | Invalid `--size`, `--exclude`, `--exclude-dir` or `-t` value, an empty `--older-than`/`--newer-than` window, a rejected `--exec` command, or a command that failed or could not be started |
| `2`  | Invalid CLI usage — bad or missing flag (e.g. `-j 0`, an unknown `--type`, an empty `--extension`) |
