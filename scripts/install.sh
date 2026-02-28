#!/usr/bin/env bash
# AdaClaw installer for Linux and macOS
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/worldflat21-lang/AdaClaw/main/scripts/install.sh | bash
#
# Or with a specific version:
#   curl -fsSL .../install.sh | bash -s -- --version v0.1.0
#
# Environment variables:
#   INSTALL_DIR  - installation directory (default: ~/.cargo/bin)
#   VERSION      - version to install (default: latest)

set -euo pipefail

# ── Config ────────────────────────────────────────────────────────────────────

REPO="worldflat21-lang/AdaClaw"
BINARY="adaclaw"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.cargo/bin}"
VERSION="${VERSION:-}"

# ── Colors ────────────────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

info()    { echo -e "${BLUE}[info]${NC} $*"; }
ok()      { echo -e "${GREEN}[ok]${NC} $*"; }
warn()    { echo -e "${YELLOW}[warn]${NC} $*"; }
error()   { echo -e "${RED}[error]${NC} $*" >&2; exit 1; }

# ── Detect platform ───────────────────────────────────────────────────────────

detect_platform() {
    local OS ARCH

    OS=$(uname -s)
    ARCH=$(uname -m)

    case "$OS" in
        Linux)
            case "$ARCH" in
                x86_64)  echo "linux-x86_64" ;;
                aarch64) echo "linux-aarch64" ;;
                arm64)   echo "linux-aarch64" ;;
                *) error "Unsupported Linux architecture: $ARCH" ;;
            esac
            ;;
        Darwin)
            case "$ARCH" in
                x86_64) echo "macos-x86_64" ;;
                arm64)  echo "macos-aarch64" ;;
                *) error "Unsupported macOS architecture: $ARCH" ;;
            esac
            ;;
        *) error "Unsupported OS: $OS (try building from source)" ;;
    esac
}

# ── Get latest version ────────────────────────────────────────────────────────

get_latest_version() {
    local version
    version=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
              | grep '"tag_name"' \
              | sed 's/.*"tag_name": *"\(.*\)".*/\1/')
    if [ -z "$version" ]; then
        error "Could not determine latest version. Set VERSION= to specify one."
    fi
    echo "$version"
}

# ── Main ──────────────────────────────────────────────────────────────────────

main() {
    echo ""
    echo "  ╔══════════════════════════════════════════╗"
    echo "  ║  ⚡ AdaClaw Installer                    ║"
    echo "  ║  Lightweight Rust AI Agent Runtime       ║"
    echo "  ╚══════════════════════════════════════════╝"
    echo ""

    # Parse args
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --version|-v) VERSION="$2"; shift 2 ;;
            --install-dir|-d) INSTALL_DIR="$2"; shift 2 ;;
            *) warn "Unknown argument: $1"; shift ;;
        esac
    done

    # Resolve version
    if [ -z "$VERSION" ]; then
        info "Fetching latest release..."
        VERSION=$(get_latest_version)
    fi
    info "Installing AdaClaw ${VERSION}"

    # Detect platform
    PLATFORM=$(detect_platform)
    info "Detected platform: ${PLATFORM}"

    # Build download URL
    ARTIFACT="${BINARY}-${PLATFORM}"
    URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARTIFACT}"

    # Create install dir
    mkdir -p "$INSTALL_DIR"

    # Download
    info "Downloading from: ${URL}"
    TMPFILE=$(mktemp)
    if ! curl -fsSL --progress-bar -o "$TMPFILE" "$URL"; then
        rm -f "$TMPFILE"
        error "Download failed. Check if ${VERSION} exists: https://github.com/${REPO}/releases"
    fi

    # Verify checksum (if available)
    CHECKSUM_URL="${URL}.sha256"
    if curl -fsSL "$CHECKSUM_URL" -o "${TMPFILE}.sha256" 2>/dev/null; then
        info "Verifying checksum..."
        EXPECTED=$(awk '{print $1}' "${TMPFILE}.sha256")
        if command -v sha256sum &>/dev/null; then
            ACTUAL=$(sha256sum "$TMPFILE" | awk '{print $1}')
        else
            ACTUAL=$(shasum -a 256 "$TMPFILE" | awk '{print $1}')
        fi
        if [ "$EXPECTED" != "$ACTUAL" ]; then
            rm -f "$TMPFILE" "${TMPFILE}.sha256"
            error "Checksum mismatch! Expected: $EXPECTED, Got: $ACTUAL"
        fi
        ok "Checksum verified"
        rm -f "${TMPFILE}.sha256"
    fi

    # Install
    chmod +x "$TMPFILE"
    mv "$TMPFILE" "${INSTALL_DIR}/${BINARY}"

    ok "Installed to: ${INSTALL_DIR}/${BINARY}"

    # Verify
    if "${INSTALL_DIR}/${BINARY}" --version &>/dev/null; then
        VERSION_INSTALLED=$("${INSTALL_DIR}/${BINARY}" --version)
        ok "Installation verified: ${VERSION_INSTALLED}"
    fi

    # Check PATH
    if ! echo ":$PATH:" | grep -q ":${INSTALL_DIR}:"; then
        echo ""
        warn "${INSTALL_DIR} is not in your PATH."
        warn "Add this to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
        warn "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    fi

    echo ""
    echo "  ══════════════════════════════════════════"
    echo "  ✅  AdaClaw is ready!"
    echo "  ══════════════════════════════════════════"
    echo ""
    echo "  Get started:"
    echo "    adaclaw onboard    # interactive setup wizard"
    echo "    adaclaw chat       # start chatting"
    echo "    adaclaw doctor     # check configuration"
    echo ""
}

main "$@"
