# arcommon

Shared library for the artools binaries (`arfind`, `ardisk`, `argrep`). Not published; the tools depend on it by path.

| Module      | What it provides |
|-------------|------------------|
| `walk`      | The parallel work queue every directory walk runs on. Idle workers sleep until there is work instead of spinning, the walk ends when nothing is queued or running, `Queue::stop()` ends it early (`argrep -q`), and per-worker state can be merged at the end (`ardisk`). |
| `ignore`    | Default ignored directories, `--exclude-dir` name/glob splitting, nested `.gitignore`/`.ignore` stacks with git's precedence, and the `--exclude` matcher. |
| `jobs`      | The `-j/--jobs` default (half the CPUs, 1–16) and upper bound. |
| `age`       | `--older-than` / `--newer-than` parsing and the mtime filter. |
| `filetypes` | The built-in `--type` table, so `--type rust` means the same in every tool. |
| `fs`        | Device ids for `--one-file-system`. |
| `term`      | Escaping control characters in anything printed to a terminal (`display_path`, `sanitize_for_display`). |

## Conventions

- **No tool-specific behaviour.** Functions return values or errors; printing, colours, exit codes and the tool name in messages stay in each tool, so a tool's user-visible behaviour is its own.
- **Differences stay explicit.** Where tools differ on purpose (argrep also skips `target/`, ardisk's `--ignore` takes plain names only), the shared function takes that as a parameter instead of picking one behaviour.
- **Tested here once.** Shared logic has its unit tests in this crate; the tools keep only tests of their own CLI and behaviour.

```bash
cargo test -p arcommon
```
