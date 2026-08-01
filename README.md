# artools 🦀

[![Lint](https://github.com/Emperorex/artools/actions/workflows/lint.yml/badge.svg)](https://github.com/Emperorex/artools/actions/workflows/lint.yml)
[![Test](https://github.com/Emperorex/artools/actions/workflows/test.yml/badge.svg)](https://github.com/Emperorex/artools/actions/workflows/test.yml)
[![Release](https://github.com/Emperorex/artools/actions/workflows/release_tool.yaml/badge.svg)](https://github.com/Emperorex/artools/actions/workflows/release_tool.yaml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://github.com/Emperorex/artools/blob/main/LICENSE.md)

A collection of ultra-fast, reliable, and production-ready CLI utilities written in Rust to enhance filesystem navigation, auditing, and system operations.

## Repository Structure

This repository is organized as a monorepo under the `tools/` directory:
```text
artools/
├── .github/workflows/   # Automated CI/CD pipelines (Lint, Test & Manual Releases)
└── tools/
    ├── arfind/          # Fast parallel file and directory finder
    ├── ardisk/          # Fast parallel disk usage analyzer
    └── argrep/          # Fast parallel text searcher
```

---

## 🔍 Featured Tools

### 1. `arfind` (Parallel File Finder)
A fast alternative to the classic `find` utility. It traverses directory trees concurrently using worker threads and a decoupled dedicated printer thread to stream matched file pathways with zero terminal backpressure deadlocks.
*   **Key Features:** Conditional tree-pruning, filename pattern evaluation via `glob`, search filtering by filesystem object type (`-t f/d/l`), and strict POSIX symlink cycle protection.

### 2. `ardisk` (Parallel Disk Usage Analyzer)
A blazing-fast multi-threaded drop-in replacement for the system `du` tool. It analyzes storage distribution and identifies heavy folders immediately.
*   **Key Features:** High-speed single-pass bottom-up aggregation ($O(N \log N)$), explicit sorting by structural depth, strict physical block allocation auditing (`blocks * 512`) on macOS/Linux via `MetadataExt`, and interactive output formatting via `--top` and `--max-depth` parameters.

### 3. `argrep` (Parallel Text Searcher)
A highly optimized code-scanning tool built as a parallel alternative to `grep` or `ripgrep`. It pipes and streams file bytes in parallel, matching multi-core processors directly to file system inputs.
*   **Key Features:** Automated chunk buffering (`BufReader`), case-insensitive keyword targeting (`-i`), deterministic row index rendering (`-n`), dynamic heap memory recycling, and rapid raw null-byte signature checking to automatically skip binaries and media files.

---

## 📥 Installation & Setup Development

### Requirements
*   **Rust Stable** (v1.88+ required for stable `if let` chain syntax with `&&`)

### Cloning the Workspace Monorepo
```bash
git clone https://github.com/Emperorex/artools.git
cd artools
```

### Building the Tools
Since the project is structured as a **Cargo Workspace**, you can compile all binaries concurrently from the root directory:

```bash
# Build development profiles
cargo build

# Build optimized production binaries (Recommended for benchmarking)
cargo build --release
```

The compiled binaries will be located inside the global `target/release/` workspace directory:
*   `target/release/arfind`
*   `target/release/ardisk`
*   `target/release/argrep`

---

## 📥 Installation

### Requirements
- macOS or Linux (x86_64 or arm64)
- `curl` and `jq` available in PATH

### One-line install (all tools)
```bash
curl -fsSL https://artools.io/install.sh | bash
```

### One-line install (all tools) - using GitHub API
```bash
export GITHUB_TOKEN=github_pat_...
curl -fsSL \
  -H "Authorization: token $GITHUB_TOKEN" \
  -H "Accept: application/vnd.github.v3.raw" \
  "https://api.github.com/repos/Emperorex/artools/contents/install-private.sh" \
  | bash
```

### Install a specific tool
```bash
... | bash -s -- arfind
```

### Custom install directory (no sudo needed)
```bash
INSTALL_DIR=~/.local/bin ... | bash
```
---
### ⚠️ Apple Gatekeeper Note (macOS Users) - skip if installing via install.sh
When downloading pre-compiled binaries from GitHub Releases on Apple Silicon (M1/M2/M3), macOS may block execution with a malware warning. To bypass this, clear the browser quarantine attribute via your terminal:

```bash
# for arfind tool:
# ARM based Macs:
xattr -d com.apple.quarantine ./arfind-macos-arm64
# Intel based Macs:
xattr -d com.apple.quarantine ./arfind-macos-x86_64

# Or for ardisk tool:
# ARM based Macs:
xattr -d com.apple.quarantine ./ardisk-macos-arm64
# Intel based Macs:
xattr -d com.apple.quarantine ./ardisk-macos-x86_64

# Or for argrep tool:
# ARM based Macs:
xattr -d com.apple.quarantine ./argrep-macos-arm64
# Intel based Macs:
xattr -d com.apple.quarantine ./argrep-macos-x86_64
```

---

## 🚀 Quick Usage Examples

### Running `arfind`
```bash
# Find all Rust source files in the current project
cargo run --bin arfind -- . -n "*.rs"

# Find only directories matching a specific criteria down to depth level 2
./target/release/arfind /Users/user/Documents -n "pro*" -t d --max-depth 2
```

### Running `ardisk`
```bash
# Analyze the current folder, display top 15 heaviest paths up to depth 1
cargo run --bin ardisk -- . -n 15 --max-depth 1

# Audit an absolute path with operational debugging stats enabled
./target/release/ardisk /var/log --top 10 --debug
```

### Running `argrep`
```bash
# Search for the term "SearchStats" in the workspace with line numbers and statistics
cargo run --bin argrep -- "SearchStats" . -n -d

# Perform a case-insensitive text match in a specific directory
./target/release/argrep "todo!" ./src -i -n
```

---

## 🏗️ Monorepo Maintenance Commands

Because of the monolithic virtual workspace setup, quality assurance can be enforced globally with single-line controls:

```bash
# Format the entire codebase according to Rust guidelines
cargo fmt --all

# Run workspace-wide static code analysis and linting policies
cargo clippy --workspace -- -D warnings

# Execute all test suites across all workspace members
cargo test --workspace
```

---
## 🤖 CI/CD Governance & Automated Releases

The repository is integrated with robust automated DevOps infrastructure:

*   **Lint Pipeline (`lint.yml`):** Runs on every push and pull request to `main`. Enforces `cargo fmt --check` and `cargo clippy -D warnings` across all workspace tools, and validates any HTML files via `htmlhint`.
*   **Test Pipeline (`test.yml`):** Runs on every push and pull request to `main`. Executes the full integration test suite for each tool (`arfind`, `ardisk`, `argrep`) in parallel matrix jobs, so a failure in one tool doesn't mask results from the others.
*   **Dependabot Integration:** Configured via `.github/dependabot.yml`. Periodically inspects Cargo crate dependencies and GitHub Actions workflow tags on a weekly schedule.
*   **Manual On-Demand Release Pipeline (`release-tools.yaml`):** Implements a GitHub `workflow_dispatch` drop-down interface. Allows developers to manually trigger production releases for any specific workspace tool. It automatically initiates cross-compilation matrix builds, packaging fully static binaries for **Linux (musl)**, **Apple Silicon macOS (arm64)**, and **Legacy Intel macOS (x86_64)** simultaneously.

---

## 📄 License

This project is licensed under the MIT License. See the `LICENSE` file for details.
