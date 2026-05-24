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
  -t, --file-type <TYPE>  Filter by type: f (file), d (directory), or l (symlink)
  -H, --hidden            Search hidden files and directories (disabled by default)
  -d, --debug             Show real-time errors and search statistics
  -V, --version           Print version information
  -h, --help              Print help information
```


### 🧪 Usage Examples

```bash
# Find all Rust files in your home directory (highly parallel, skips hidden paths)
arfind ~ -n "*.rs" -j 8

# Find hidden environment files (.env) inside your current folder
arfind . -n "*.env" -H

# Find only symbolic links inside a system directory and display metrics
arfind /usr -t l --debug

# Find only directories matching "target" inside your current folder
arfind . -n "target*" -t d
```

## ⚙️ Analyzer Tool: `ardisk`

`ardisk` is a high-speed parallel alternative to the classic `du` (Disk Usage) command. It scans the targeted directory using multiple threads and aggregates the sizes of all files, rolling up weights from deepest child folders up to the root directory.

### 🚀 Key Features
*   **Parallel Multi-Threaded Scanning:** Processes thousands of directories concurrently using lock-free architecture.
*   **Bottom-Up Size Rollup:** Accurately propagates file weights from deep nested structures to parent folders completely in-memory.
*   **Human-Readable Table:** Auto-aligns output and renders bytes elegantly into `KB`, `MB`, or `GB` metrics with color codes.
*   **Scoped Top Reports:** Automatically isolates analysis within the target path, suppressing noisy global OS system roots (`/`, `/Users`).

### 🧪 Usage Examples
```bash
# Analyze the current folder and display top 20 heaviest locations
ardisk

# Analyze your whole user home directory using 8 parallel cores with execution metrics
ardisk ~ -j 8 --debug
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
