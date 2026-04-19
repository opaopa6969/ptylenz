#!/usr/bin/env bash
# Build and install ptylenz from source into ~/.cargo/bin.
# Re-run after `git pull` to update.

set -euo pipefail

cd "$(dirname "$0")"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found. install Rust from https://rustup.rs and retry." >&2
  exit 1
fi

echo ">>> cargo install --path . --force"
cargo install --path . --force

bin="$(command -v ptylenz || true)"
if [ -z "$bin" ]; then
  cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
  echo ""
  echo "installed, but 'ptylenz' is not on PATH."
  echo "add this to your shell rc:"
  echo ""
  echo "  export PATH=\"$cargo_bin:\$PATH\""
  exit 0
fi

echo ""
echo "installed: $bin"
"$bin" --version 2>/dev/null || true
