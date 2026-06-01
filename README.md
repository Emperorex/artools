# artools 🦀

A collection of ultra-fast, reliable, and production-ready CLI utilities written in Rust to enhance filesystem navigation, auditing, and system operations.

## Repository Structure

This repository is organized as a monorepo under the `tools/` directory:
```text
artools/
├── .github/workflows/   # Automated CI/CD pipelines (Linting & Manual Releases)
└── tools/
    ├── arfind/          # Fast parallel file and directory finder
    └── ardisk/          # Fast parallel disk usage analyzer
```

---

## 🔍 Featured Tools

### 1. `arfind` (Parallel File Finder)
A high-performance, multi-threaded alternative to the traditional `find` command. It uses low-latency, lock-free coordination to scan directories concurrently without CPU core thrashing.

*   **Key Features:** Object type filtering (`-t f`/`d`/`l` for symlinks), instant streaming to `stdout`, smart automatic ignoring (`.git`, `node_modules`), and hidden path constraints (`-H`).

### 2. `ardisk` (Parallel Disk Usage Analyzer)
An ultra-fast parallel alternative to the classic `du` utility. It scans targeted directories using multiple threads, extracts exact byte metrics, and aggregates sizes up the directory tree completely in-memory.

*   **Key Features:** Deep bottom-up size propagation, right-aligned human-readable table formatter (`KB`, `MB`, `GB`, `TB`), and scoped level visualization (`--max-depth`).

---

## 🛠️ Installation & Setup

Ensure you have [Rust and Cargo](https://rustup.rs) installed, then clone the repository and build the tools:

```bash
# Clone the repository
git clone https://github.com
cd artools/tools/ardisk # Or tools/arfind

# Build the optimized release binary
cargo build --release
```

### ⚠️ Apple Gatekeeper Note (macOS Users)
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
```

---

## 🚀 CI/CD & Automation

This repository enforces high code-quality standards and automates deployment gates via GitHub Actions:

1.  **Code Linting (`lint.yml`):** Automatically triggers on every push/PR, dynamically running strict format verification (`cargo fmt --check`) and static analysis (`cargo clippy -- -D warnings`) across all sub-projects.
2.  **On-Demand Releases (`release-tools.yaml`):** A manual, resource-optimized pipeline. Developers can trigger a production release via the GitHub UI using a secure drop-down selector to choose the target tool, generate historical changelogs, tag commits, and attach cross-compiled production assets.

## 📄 License

This project is licensed under the MIT License. See the `LICENSE` file for details.
