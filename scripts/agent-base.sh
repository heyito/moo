#!/bin/bash
# Provision a machine with the common agent toolkit and snapshot it.
#
# Usage: scripts/agent-base.sh [machine-name]     (default name: base)
#
# The moo base image is deliberately a plain OCI image; anything richer is
# machine state you provision once and snapshot (see the moo-golden-image
# skill). This script is that "once": compilers, developer CLIs, database
# clients, Node.js, and a root-safe Chromium — the tools coding agents
# reach for. Fork the result per agent:
#
#     $ scripts/agent-base.sh base
#     $ moo new agent-1 from base        # <1s, toolkit included
#
# Composes with scripts/desktop.sh: run both against the same machine and
# the desktop gets a working browser.
set -euo pipefail

MOO="${MOO:-moo}"
NAME="${1:-base}"

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }

say "Machine: $NAME"
"$MOO" new "$NAME"

say "Core toolkit (one-time, a few minutes)"
"$MOO" run "$NAME" -- 'apt-get update -q >/dev/null && DEBIAN_FRONTEND=noninteractive \
    apt-get install -y -q --no-install-recommends \
    git git-lfs curl wget ca-certificates gnupg jq ripgrep tree file patch \
    rsync zip unzip xz-utils zstd \
    build-essential pkg-config python3 python3-pip python3-venv \
    sqlite3 postgresql-client redis-tools \
    procps psmisc lsof htop net-tools dnsutils iputils-ping \
    netcat-openbsd socat tmux bc openssh-client openssl \
    >/dev/null && echo "core toolkit installed"'

say "Node.js (current LTS via NodeSource)"
"$MOO" run "$NAME" -- 'test -f /etc/apt/sources.list.d/nodesource.list || { \
    curl -fsSL https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key \
        | gpg --dearmor -o /usr/share/keyrings/nodesource.gpg \
    && echo "deb [signed-by=/usr/share/keyrings/nodesource.gpg] https://deb.nodesource.com/node_24.x nodistro main" \
        > /etc/apt/sources.list.d/nodesource.list \
    && apt-get update -q >/dev/null; } \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y -q nodejs >/dev/null \
    && (corepack enable 2>/dev/null || true) \
    && echo "node $(node --version), npm $(npm --version)"'

say "Chromium (headless-capable, desktop-ready)"
# Everything in the guest runs as root, and Chromium refuses to start as
# root with its sandbox on. The machine itself is the sandbox here, so
# disable Chromium's via the wrapper config Debian provides.
"$MOO" run "$NAME" -- 'DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends \
    chromium chromium-driver fonts-noto-color-emoji fonts-dejavu-core \
    >/dev/null \
    && mkdir -p /etc/chromium.d \
    && printf "export CHROMIUM_FLAGS=\"\$CHROMIUM_FLAGS --no-sandbox --disable-dev-shm-usage --disable-gpu\"\n" \
        > /etc/chromium.d/moo-guest \
    && chromium --version'

say "Verifying headless Chromium"
"$MOO" run "$NAME" -- 'chromium --headless --disable-dev-shm-usage --dump-dom about:blank >/dev/null 2>&1 && echo "headless chromium works"'

say "Saving the baseline snapshot"
"$MOO" save "$NAME"

say "Done. Fork it per agent:"
printf '\n    moo new agent-1 from %s\n\n' "$NAME"
