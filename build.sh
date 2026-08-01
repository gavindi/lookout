#!/usr/bin/env bash
# Builds everything in the Lookout repo:
#   1. The Rust Cargo workspace (crates live under source/).
#   2. The webmail frontend (Next.js app under webmail/).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$SCRIPT_DIR/source"
WEBMAIL_DIR="$SCRIPT_DIR/webmail"

RELEASE=false
for arg in "$@"; do
    case "$arg" in
        --release) RELEASE=true ;;
        -h|--help)
            echo "Usage: ./build.sh [--release]"
            echo "  --release   Build with optimizations (cargo build --release)"
            exit 0
            ;;
        *)
            echo "Unknown argument: $arg" >&2
            exit 1
            ;;
    esac
done

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found. Install Rust via https://rustup.rs and re-run." >&2
    exit 1
fi

missing_libs=()
for lib in gtk4 libadwaita-1 webkitgtk-6.0; do
    if ! pkg-config --exists "$lib" 2>/dev/null; then
        missing_libs+=("$lib")
    fi
done
if [ "${#missing_libs[@]}" -gt 0 ]; then
    echo "error: missing development libraries: ${missing_libs[*]}" >&2
    echo "On Debian/Ubuntu: sudo apt-get install libgtk-4-dev libadwaita-1-dev libwebkitgtk-6.0-dev" >&2
    exit 1
fi

cd "$WORKSPACE_DIR"

if [ "$RELEASE" = true ]; then
    cargo build --workspace --release
    BINARY_PATH="$WORKSPACE_DIR/target/release/lookout"
else
    cargo build --workspace
    BINARY_PATH="$WORKSPACE_DIR/target/debug/lookout"
fi

# --- Webmail (Next.js) frontend ---
if [ -f "$WEBMAIL_DIR/package.json" ]; then
    if ! command -v npm >/dev/null 2>&1; then
        echo "error: npm not found. Install Node.js and re-run." >&2
        exit 1
    fi
    cd "$WEBMAIL_DIR"
    echo "Building webmail frontend..."
    npm install
    npm run build
fi

echo
echo "Built binary: $BINARY_PATH"
