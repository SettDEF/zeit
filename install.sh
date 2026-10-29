#!/usr/bin/env bash
# Builds zeit and installs it to ~/.local/bin.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release ${JOBS:+-j "$JOBS"}
target="${CARGO_TARGET_DIR:-target}/release/zeit"
install -Dm755 "$target" "$HOME/.local/bin/zeit"
echo ":: installed: run 'zeit' (the session of this folder), 'zeit --help'"
