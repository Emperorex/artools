[![Lint](https://github.com/Emperorex/artools/actions/workflows/lint.yml/badge.svg)](https://github.com/Emperorex/artools/actions/workflows/lint.yml)
[![Test](https://github.com/Emperorex/artools/actions/workflows/test.yml/badge.svg)](https://github.com/Emperorex/artools/actions/workflows/test.yml)
[![Release](https://github.com/Emperorex/artools/actions/workflows/release_tool.yaml/badge.svg)](https://github.com/Emperorex/artools/actions/workflows/release_tool.yaml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://github.com/Emperorex/artools/blob/main/LICENSE.md)

# artools 🦀

A collection of fast, parallel CLI utilities written in Rust for filesystem navigation, disk analysis, and text search.

```bash
curl -fsSL https://artools.io/install.sh | bash
```

---

## Tools

| Tool     | Replaces | Description                                                                    | Docs                                      |
|----------|----------|--------------------------------------------------------------------------------|-------------------------------------------|
| `arfind` | `find`   | Parallel file and directory finder with glob patterns, type and size filtering | [→ arfind README](tools/arfind/README.md) |
| `ardisk` | `du`     | Parallel disk usage analyzer with bottom-up rollup and threshold filtering     | [→ ardisk README](tools/ardisk/README.md) |
| `argrep` | `grep`   | Parallel text search with stdin support, invert, count, and file-list modes    | [→ argrep README](tools/argrep/README.md) |

---

## Repository Structure

```text
artools/
├── .github/workflows/   # CI/CD pipelines (Lint, Test, Release)
├── docs/                # GitHub Pages site (artools.io)
└── tools/
    ├── arfind/          # Fast parallel file finder
    ├── ardisk/          # Fast parallel disk usage analyzer
    └── argrep/          # Fast parallel text searcher
```

---

## Installation

### One-line install (all tools)

```bash
curl -fsSL https://artools.io/install.sh | bash
```

### Install a specific tool

```bash
curl -fsSL https://artools.io/install.sh | bash -s -- arfind
```

### Custom install directory

```bash
INSTALL_DIR=~/.local/bin curl -fsSL https://artools.io/install.sh | bash
```

**Supported platforms:** macOS arm64, macOS x86_64, Linux x86_64 (musl static binary)

---

## Building from Source

**Requirements:** Rust stable v1.88+

```bash
git clone https://github.com/Emperorex/artools.git
cd artools

# Build all tools
cargo build --release

# Build a specific tool
cargo build --release --bin arfind
```

Binaries are placed in `target/release/`.

---

## macOS — First Run Note

> **Not applicable if you used `install.sh`** — the installer handles this automatically.

macOS applies a quarantine attribute to binaries downloaded via a browser. This causes Gatekeeper to block execution on the first run. To clear it, run the following command in the directory where you saved the binary:

```bash
# Apple Silicon (M1/M2/M3/M4)
xattr -d com.apple.quarantine ./arfind-macos-arm64
xattr -d com.apple.quarantine ./ardisk-macos-arm64
xattr -d com.apple.quarantine ./argrep-macos-arm64

# Intel
xattr -d com.apple.quarantine ./arfind-macos-x86_64
xattr -d com.apple.quarantine ./ardisk-macos-x86_64
xattr -d com.apple.quarantine ./argrep-macos-x86_64
```

After that, the binary runs normally with no further prompts.

---

## Development

```bash
# Format
cargo fmt --all

# Lint
cargo clippy --workspace -- -D warnings

# Test
cargo test --workspace
```

---

## CI/CD

- **Lint** — runs `cargo fmt --check` and `cargo clippy -D warnings` on every push and PR
- **Test** — runs integration test suites for all tools in parallel on every push and PR
- **Release** — manual `workflow_dispatch` trigger; cross-compiles binaries for all platforms and publishes a GitHub Release with per-tool scoped release notes

---

## License

MIT — see [LICENSE.md](LICENSE.md)
