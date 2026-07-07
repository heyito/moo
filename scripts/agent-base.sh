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

say "Chromium via Playwright (headless-capable, desktop-ready)"
# Debian's apt `chromium` crashes under the guest kernel in real use
# (dbus/crashpad/seccomp) even when a trivial headless probe passes, so
# the toolkit ships Playwright's Chromium build instead. Everything in
# the guest runs as root and the machine itself is the sandbox, so the
# wrapper disables Chromium's own.
"$MOO" run "$NAME" -- 'DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends \
    fonts-noto-color-emoji fonts-dejavu-core fonts-liberation >/dev/null \
    && npx -y playwright@latest install --with-deps chromium >/dev/null 2>&1 \
    && echo "playwright chromium installed"'

"$MOO" run "$NAME" -- 'cat > /usr/local/bin/chromium <<"EOF"
#!/bin/sh
# Playwright-managed Chromium with root-safe flags. The newest installed
# build wins; projects that install their own playwright browsers land in
# the same place and are picked up automatically.
bin=$(ls -t /root/.cache/ms-playwright/chromium-*/chrome-linux*/chrome 2>/dev/null | head -1)
[ -n "$bin" ] || { echo "chromium: no playwright build found — run: npx playwright install chromium" >&2; exit 127; }
exec "$bin" --no-sandbox --disable-dev-shm-usage --disable-gpu "$@"
EOF
chmod +x /usr/local/bin/chromium
ln -sf /usr/local/bin/chromium /usr/local/bin/chromium-browser
mkdir -p /usr/share/applications
cat > /usr/share/applications/chromium.desktop <<"EOF"
[Desktop Entry]
Type=Application
Name=Chromium
Comment=Playwright-managed Chromium (root-safe flags)
Exec=/usr/local/bin/chromium %U
Terminal=false
Categories=Network;WebBrowser;
EOF
echo "chromium wrapper + desktop launcher installed"'

say "Verifying the browser actually renders"
# An about:blank probe passes even on broken builds; render a real page
# and check the DOM comes back.
"$MOO" run "$NAME" -- 'chromium --headless=new --dump-dom "data:text/html,<h1>moo-browser-ok</h1>" 2>/dev/null | grep -q moo-browser-ok && echo "browser renders (headless)"'

say "Saving the baseline snapshot"
"$MOO" save "$NAME"

say "Done. Fork it per agent:"
printf '\n    moo new agent-1 from %s\n\n' "$NAME"
