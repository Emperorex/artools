# artools 🦀

A collection of fast, reliable, and production-ready CLI tools written in Rust to enhance system operations and filesystem navigation.

## Repository Structure

This repository is organized as a monorepo under the `tools/` directory:
```text
artools/
├── .github/workflows/   # Automated CI gates (Linting & Formatting)
└── tools/
    └── arfind/          # Fast parallel file and directory finder
```

---

## 🔍 Featured Tool: `arfind`

`arfind` is a high-performance alternative to the traditional `find` utility. It utilizes a multi-threaded, lock-free architecture to maximize I/O throughput when scanning deeply nested filesystems.

### 🚀 Key Features

*   **Lock-Free Architecture:** Powered by `crossbeam-channel` (Multi-Producer Multi-Consumer) to eliminate thread contention and optimize parallel CPU utilization.
*   **Smart Ignoring:** Automatically skips heavy metadata and dependency directories (`.git`, `node_modules`, `__pycache__`) by default.
*   **Object Type Filtering:** Filter your search results on the fly to match only files (`-t f`) or directories (`-t d`).
*   **Streamed & Colorized Output:** Matches are color-coded (Directories in **bold blue**, Files in standard text) and flushed to `stdout` instantly as they are discovered.
*   **Graceful Error Handling:** Seamlessly bypasses restricted system areas (`Permission Denied`) without stalling, logging skipped paths cleanly to `stderr` when debug mode is enabled.

### 🛠️ Installation & Setup

Ensure you have [Rust and Cargo](https://rustup.rs) installed, then clone the repository and build the project in release mode:

```bash
# Clone the repository
git clone https://github.com
cd artools/tools/arfind

# Build the optimized release binary
cargo build --release

# (Optional) Move the binary to your local PATH
cp target/release/arfind /usr/local/bin/
```

### ⚙️ CLI Arguments Reference

```text
Usage: arfind [PATH] [OPTIONS]

Arguments:
  [PATH]  Root directory to start the search [default: .]

Options:
  -n, --name <NAME>       Filename glob pattern [default: *]
  -j, --jobs <JOBS>       Number of worker threads [default: 4]
      --max-depth <DEPTH> Maximum recursion depth
      --ignore <IGNORE>   Additional directories to ignore
  -t, --file-type <TYPE>  Filter by type: f (file) or d (directory)
  -d, --debug             Show real-time errors and search statistics
  -V, --version           Print version information
  -h, --help              Print help information
```

### 🧪 Usage Examples

```bash
# Find all Rust files in your home directory (highly parallel)
arfind ~ -n "*.rs" -j 8

# Find only directories matching "target" inside your current folder
arfind . -n "target*" -t d

# Search system bins, check exact operation metrics and restricted path locks
arfind /usr -n "bash*" --debug
```

---

## 🛠️ Development & CI/CD

This repository enforces strict code quality gates via GitHub Actions on every `push` and `pull_request`.

The pipeline dynamically discovers all Rust sub-projects inside the `tools/` directory and subjects them to formatting and linting workflows.

### Local Quality Assurance

Before opening a pull request, always format and lint your code locally within the specific tool's directory:

```bash
# 1. Format your code according to Rust standards
cargo fmt

# 2. Run the static analysis tool (Linter)
cargo clippy -- -D warnings
```

## 📄 License

This project is licensed under the MIT License. See the `LICENSE` file for details.
