#!/usr/bin/env bash
set -euo pipefail

# clauth — xingfanxia macOS fork installer.
#
# This fork adds real macOS Keychain account switching, a browser-OAuth login,
# and a headless daemon + status.json feed. It ships NO prebuilt release binaries
# and is NOT published to crates.io (that name is upstream's), so the only honest
# install is a source build from this checkout. `cargo install clauth` (crates.io)
# would fetch UPSTREAM without any of the fork's features — do not use it here.

if ! command -v cargo >/dev/null 2>&1; then
    echo "Error: cargo (the Rust toolchain) is required to build the clauth fork." >&2
    echo "Install Rust from https://rustup.rs, then re-run this script." >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "Building and installing clauth from source (${SCRIPT_DIR})…"
cargo install --path "${SCRIPT_DIR}" --locked

echo ""
echo "Installed to ~/.cargo/bin/clauth — ensure ~/.cargo/bin is on your PATH."
echo "macOS menu-bar daemon (LaunchAgent): dist/macos/daemon-install.sh"
echo "To uninstall: cargo uninstall clauth"
