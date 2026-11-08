#!/usr/bin/env bash
# Rustup installs the checkout's pinned toolchain without changing its global default.
set -euo pipefail
# shellcheck source=scripts/lib/install.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/../lib/install.sh"

load_cargo_env
toolchain=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$SETUP_ROOT/rust-toolchain.toml")
current=$(rustc --version 2>/dev/null | awk '{print $2}' || true)
if command -v rustup >/dev/null && [[ $current == "$toolchain" ]] && cargo clippy --version >/dev/null 2>&1 && cargo fmt --version >/dev/null 2>&1; then
    present rust
else
    if ! command -v rustup >/dev/null; then
        curl -fsSL --proto '=https' --tlsv1.2 https://sh.rustup.rs -o "$SETUP_ROOT/.local/rustup-init.sh" >>"$SETUP_LOG" 2>&1
        log sh "$SETUP_ROOT/.local/rustup-init.sh" -y --profile minimal --default-toolchain none
        rm -f "$SETUP_ROOT/.local/rustup-init.sh"
        load_cargo_env
    fi
    log rustup toolchain install "$toolchain" --profile minimal --component clippy --component rustfmt
    current=$(rustc --version | awk '{print $2}')
    [[ $current == "$toolchain" ]] ||
        fail rust "Rust $toolchain is required, found $current" 'remove any RUSTUP_TOOLCHAIN or rustup directory override, then rerun ./bilibi setup'
    installed rust
fi
