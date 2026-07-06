#!/bin/sh
# Install moo on an Apple Silicon Mac. Non-interactive.
#
# Dual-mode — this same file is scripts/install.sh in the repo and the
# install.sh asset attached to every GitHub release:
#
#   from a source checkout   scripts/install.sh
#   from a release           curl -fsSL https://github.com/heyito/moo/releases/latest/download/install.sh | sh
#
# Optional: MOO_VERSION=v0.1.0 pins a specific release in download mode.
# Usage: install.sh [prefix]   (default prefix: /opt/homebrew/bin)
set -eu

# Keep brew fast and deterministic: no implicit self-update, no prompts.
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_UPGRADE=1 NONINTERACTIVE=1

REPO="heyito/moo"
ASSET="moo-aarch64-apple-darwin.tar.gz"
PREFIX="${1:-/opt/homebrew/bin}"

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || die "moo currently supports macOS only"
[ "$(uname -m)" = "arm64" ]  || die "moo currently supports Apple Silicon only"
command -v brew >/dev/null || die "Homebrew is required: https://brew.sh"

# A source checkout next to this script means build mode; a bare pipe from
# curl means download mode.
SCRIPT_DIR="$(cd "$(dirname "$0")" 2>/dev/null && pwd)" || SCRIPT_DIR=""
REPO_ROOT=""
if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/../crates/moo-cli/entitlements.plist" ]; then
    REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
fi

say "Installing runtime dependencies"
# `brew trust` exists only on newer Homebrew; ignore where unsupported.
brew tap slp/krun 2>/dev/null || true
brew trust slp/krun 2>/dev/null || true
brew list libkrun   >/dev/null 2>&1 || brew install slp/krun/libkrun
brew list libkrunfw >/dev/null 2>&1 || brew install slp/krun/libkrunfw
brew list gvproxy   >/dev/null 2>&1 || brew install slp/krun/gvproxy
brew list e2fsprogs >/dev/null 2>&1 || brew install e2fsprogs

if [ -n "$REPO_ROOT" ]; then
    command -v cargo >/dev/null || die "Rust is required to build from source: https://rustup.rs"

    say "Adding the guest build target"
    rustup target add aarch64-unknown-linux-musl

    say "Building moo"
    cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"
    BIN="$REPO_ROOT/target/release/moo"
    ENTITLEMENTS="$REPO_ROOT/crates/moo-cli/entitlements.plist"
else
    URL="https://github.com/$REPO/releases/latest/download/$ASSET"
    [ -n "${MOO_VERSION:-}" ] && URL="https://github.com/$REPO/releases/download/$MOO_VERSION/$ASSET"

    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT

    say "Downloading $URL"
    curl -fsSL "$URL" -o "$TMP/$ASSET"
    tar -xzf "$TMP/$ASSET" -C "$TMP"
    BIN="$TMP/moo"
    ENTITLEMENTS="$TMP/entitlements.plist"
    [ -f "$BIN" ] && [ -f "$ENTITLEMENTS" ] || die "unexpected release archive layout"
fi

say "Signing for machine isolation"
codesign --force --sign - --entitlements "$ENTITLEMENTS" "$BIN"

say "Installing to $PREFIX/moo"
install -m 755 "$BIN" "$PREFIX/moo"
# codesign identity survives the copy; re-sign in place to be safe.
codesign --force --sign - --entitlements "$ENTITLEMENTS" "$PREFIX/moo"

say "Checking the host"
"$PREFIX/moo" doctor

say "Done. Try: moo new my-machine && moo run my-machine -- uname -a"
