#!/usr/bin/env bash
# Builds the Lookout Cargo workspace (crates live under source/).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$SCRIPT_DIR/source"

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
    echo "Built: $WORKSPACE_DIR/target/release/lookout"
else
    cargo build --workspace
    echo "Built: $WORKSPACE_DIR/target/debug/lookout"
fi
