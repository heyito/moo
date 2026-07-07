#!/bin/bash
# Give a machine a clickable Linux desktop, served in your browser.
#
# Usage: scripts/desktop.sh [machine-name]     (default name: desktop)
#
# Requires the project's moo.toml to forward guest port 6901:
#
#     [network]
#     ports = [6901]
#
# Installs XFCE + a VNC server + a browser client (noVNC) inside the
# machine, wires it to start on every boot via /etc/rc.local (the boot
# hook the guest agent runs), saves a snapshot, and prints the URL.
# Everything is composed from the four verbs — no new moo surface.
set -euo pipefail

MOO="${MOO:-moo}"
NAME="${1:-desktop}"
GUEST_PORT=6901

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

say "Machine: $NAME"
"$MOO" new "$NAME"

# `moo open` resolves the host port for this repo's machine and prints the
# URL; with stdout captured it never launches a browser.
url=$("$MOO" open "$NAME" "$GUEST_PORT" 2>/dev/null) || die "machine '$NAME' does not forward guest port $GUEST_PORT.
Declare it in moo.toml and recreate the machine:

    [network]
    ports = [$GUEST_PORT]

    \$ moo drop $NAME && moo new $NAME"
url=${url%%$'\n'*}
port=${url%/}; port=${port##*:}
[ -n "$port" ] || die "could not resolve the host port for guest port $GUEST_PORT"

if ! "$MOO" run "$NAME" -- \
    'command -v websockify >/dev/null && command -v Xtigervnc >/dev/null && command -v startxfce4 >/dev/null' \
    >/dev/null 2>&1; then
    say "Installing the desktop inside the machine (one-time, a few minutes)"
    "$MOO" run "$NAME" -- 'apt-get update -q >/dev/null && DEBIAN_FRONTEND=noninteractive \
        apt-get install -y -q --no-install-recommends \
        xfce4 xfce4-terminal dbus-x11 procps tigervnc-standalone-server novnc websockify \
        >/dev/null && echo "desktop packages installed"'
fi

say "Wiring the desktop to start on every boot"
"$MOO" run "$NAME" -- 'cat > /usr/local/bin/desktop-up <<"EOF"
#!/bin/sh
# Started by /etc/rc.local on every boot. rc.local must return for the
# machine to finish booting, so every service backgrounds itself.
pgrep -x Xtigervnc >/dev/null 2>&1 && exit 0
# The guest /tmp lives on the versioned disk: a snapshot taken while the
# desktop ran carries stale X lock files that would block the next boot.
rm -f /tmp/.X1-lock
rm -rf /tmp/.X11-unix
setsid Xtigervnc :1 -geometry 1440x900 -depth 24 -SecurityTypes None \
    >/var/log/desktop-x.log 2>&1 &
sleep 1
DISPLAY=:1 setsid dbus-launch startxfce4 >/var/log/desktop-session.log 2>&1 &
# noVNC must listen on a non-loopback address to be reachable from the host.
setsid websockify --web /usr/share/novnc 0.0.0.0:6901 localhost:5901 \
    >/var/log/desktop-web.log 2>&1 &
exit 0
EOF
chmod +x /usr/local/bin/desktop-up
grep -qs desktop-up /etc/rc.local 2>/dev/null \
    || printf "#!/bin/sh\n/usr/local/bin/desktop-up\n" > /etc/rc.local
chmod +x /etc/rc.local
/usr/local/bin/desktop-up
echo "desktop services started"'

say "Opening desktop terminals in the synced working tree"
# The guest shells should land where the working tree syncs to: the
# project's [project] workdir, or moo's default when unset.
WORKDIR=$(sed -n 's/^[[:space:]]*workdir[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' moo.toml 2>/dev/null | head -1)
WORKDIR="${WORKDIR:-/srv/app}"
"$MOO" run "$NAME" -- "grep -qs 'cd $WORKDIR' /root/.bashrc || printf '\n# moo desktop: open shells in the synced working tree\n[ -d $WORKDIR ] && cd $WORKDIR\n' >> /root/.bashrc"

say "Waiting for the desktop to answer"
for _ in $(seq 1 60); do
    if curl -sf -o /dev/null "http://localhost:$port/vnc.html"; then ok=1; break; fi
    sleep 0.5
done
[ "${ok:-}" = 1 ] || die "desktop did not come up; check: moo run $NAME -- 'cat /var/log/desktop-*.log'"

say "Saving a snapshot (the desktop survives drop/restore and forks)"
"$MOO" save "$NAME"

say "Done. Click around at:"
printf '\n    http://localhost:%s/vnc.html?autoconnect=true&resize=scale\n\n' "$port"
printf 'Reopen any time with:\n\n    %s open %s %s '\''/vnc.html?autoconnect=true&resize=scale'\''\n\n' \
    "$MOO" "$NAME" "$GUEST_PORT"
