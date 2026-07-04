#!/bin/bash
# Install got from source on a clean Apple Silicon Mac.
# Usage: scripts/install.sh [prefix]   (default prefix: /opt/homebrew/bin)
set -euo pipefail

PREFIX="${1:-/opt/homebrew/bin}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

say()  { printf '\033[1m==> %s\033[0m\n' "$*"; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || die "got currently supports macOS only"
[ "$(uname -m)" = "arm64" ]  || die "got currently supports Apple Silicon only"
command -v brew  >/dev/null || die "Homebrew is required: https://brew.sh"
command -v cargo >/dev/null || die "Rust is required: https://rustup.rs"

say "Installing runtime dependencies"
# `brew trust` exists only on newer Homebrew; ignore where unsupported.
brew tap slp/krun 2>/dev/null || true
brew trust slp/krun 2>/dev/null || true
brew list libkrun   >/dev/null 2>&1 || brew install slp/krun/libkrun
brew list libkrunfw >/dev/null 2>&1 || brew install slp/krun/libkrunfw
brew list gvproxy   >/dev/null 2>&1 || brew install slp/krun/gvproxy
brew list e2fsprogs >/dev/null 2>&1 || brew install e2fsprogs

say "Adding the guest build target"
rustup target add aarch64-unknown-linux-musl

say "Building got"
cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"

say "Signing for machine isolation"
codesign --force --sign - \
    --entitlements "$REPO_ROOT/crates/got-cli/entitlements.plist" \
    "$REPO_ROOT/target/release/got"

say "Installing to $PREFIX/got"
install -m 755 "$REPO_ROOT/target/release/got" "$PREFIX/got"
# codesign identity survives the copy; re-sign in place to be safe.
codesign --force --sign - \
    --entitlements "$REPO_ROOT/crates/got-cli/entitlements.plist" \
    "$PREFIX/got"

say "Checking the host"
"$PREFIX/got" doctor

say "Done. Try: got new my-machine && got run my-machine -- uname -a"
