#!/usr/bin/env bash
set -euo pipefail

# ── Configuration ─────────────────────────────────────────────────────────────
REPO="Emperorex/artools"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
TOOLS=(arfind ardisk argrep)

# ── Colors ────────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info()    { echo -e "${CYAN}[artools]${NC} $*"; }
success() { echo -e "${GREEN}[artools]${NC} $*"; }
warn()    { echo -e "${YELLOW}[artools]${NC} $*"; }
error()   { echo -e "${RED}[artools]${NC} $*" >&2; exit 1; }

# ── OS / arch detection ───────────────────────────────────────────────────────
detect_platform() {
    local os arch

    case "$(uname -s)" in
        Linux)  os="linux"  ;;
        Darwin) os="macos"  ;;
        *)      error "Unsupported OS: $(uname -s). Only Linux and macOS are supported." ;;
    esac

    case "$(uname -m)" in
        x86_64)           arch="x86_64" ;;
        arm64 | aarch64)  arch="arm64"  ;;
        *)                error "Unsupported architecture: $(uname -m)." ;;
    esac

    echo "${os}-${arch}"
}

# ── Dependency checks ─────────────────────────────────────────────────────────
check_deps() {
    for cmd in curl jq chmod; do
        command -v "$cmd" >/dev/null 2>&1 || error "Required dependency not found: $cmd"
    done
}

# ── Resolve the latest release tag for a given tool ──────────────────────────
latest_tag() {
    local tool="$1"
    curl -fsSL "https://api.github.com/repos/${REPO}/releases" \
        | jq -r '.[].tag_name' \
        | grep "^${tool}-v" \
        | head -n1
}

# ── Download and install a single binary ─────────────────────────────────────
install_tool() {
    local tool="$1"
    local platform="$2"
    local tag
    tag=$(latest_tag "$tool")

    if [ -z "$tag" ]; then
        warn "No release found for '$tool'. Skipping."
        return
    fi

    local version="${tag#"${tool}-v"}"
    local binary_name="${tool}-${platform}"

    # Public repo: use browser_download_url for direct unauthenticated download
    local download_url
    download_url=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/tags/${tag}" \
        | jq -r --arg name "$binary_name" \
            '.assets[] | select(.name == $name) | .browser_download_url')

    if [ -z "$download_url" ]; then
        error "Asset '$binary_name' not found in release '$tag'."
    fi

    # Use /tmp explicitly so the path is accessible to both the current user
    # and sudo — mktemp's default dir on macOS is user-scoped and sudo cannot
    # access it, causing "No such file or directory" on sudo mv.
    local tmp_file="/tmp/artools_${tool}_$$"

    info "Installing $tool v${version} (${platform})..."

    curl -fsSL --progress-bar "$download_url" -o "$tmp_file"

    chmod +x "$tmp_file"

    if [ -w "$INSTALL_DIR" ]; then
        mkdir -p "$INSTALL_DIR"
        mv "$tmp_file" "${INSTALL_DIR}/${tool}"
    else
        info "Requesting sudo to write to ${INSTALL_DIR}..."
        sudo mkdir -p "$INSTALL_DIR"
        sudo cp "$tmp_file" "${INSTALL_DIR}/${tool}"
        sudo chmod +x "${INSTALL_DIR}/${tool}"
        rm -f "$tmp_file"
    fi

    success "$tool installed → ${INSTALL_DIR}/${tool}"
}

# ── macOS Gatekeeper quarantine removal ───────────────────────────────────────
remove_quarantine() {
    local tool="$1"
    if [[ "$(uname -s)" == "Darwin" ]]; then
        xattr -d com.apple.quarantine "${INSTALL_DIR}/${tool}" 2>/dev/null || true
    fi
}

# ── Argument parsing ──────────────────────────────────────────────────────────
parse_args() {
    if [ "$#" -eq 0 ]; then
        echo "${TOOLS[@]}"
    else
        for tool in "$@"; do
            local valid=false
            for known in "${TOOLS[@]}"; do
                [[ "$tool" == "$known" ]] && valid=true && break
            done
            if ! $valid; then
                error "Unknown tool: '$tool'. Valid options are: ${TOOLS[*]}"
            fi
        done
        echo "$@"
    fi
}

# ── Main ──────────────────────────────────────────────────────────────────────
main() {
    echo ""
    echo -e "${CYAN}╔══════════════════════════════╗${NC}"
    echo -e "${CYAN}║     artools  installer  🦀   ║${NC}"
    echo -e "${CYAN}╚══════════════════════════════╝${NC}"
    echo ""

    check_deps

    local platform
    platform=$(detect_platform)
    info "Platform detected: ${platform}"
    info "Install directory: ${INSTALL_DIR}"
    echo ""

    local selected_tools
    read -ra selected_tools <<< "$(parse_args "$@")"

    for tool in "${selected_tools[@]}"; do
        install_tool "$tool" "$platform"
        remove_quarantine "$tool"
    done

    echo ""
    success "Done! Make sure ${INSTALL_DIR} is in your PATH."
    echo ""
    echo "  Verify with:"
    for tool in "${selected_tools[@]}"; do
        echo "    ${tool} --version"
    done
    echo ""
}

main "$@"
