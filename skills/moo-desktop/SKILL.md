---
name: moo-desktop
description: Give a moo machine a clickable Linux desktop served in the browser, or provision it with the common agent toolkit (dev tools, Node.js, Chromium). Use when the user wants to see or click around an app running in a moo machine, asks for a desktop, GUI, VNC, or browser view of a machine, or wants a machine equipped for browser-based testing.
---

# Desktop and toolkit provisioning for moo machines

Machines are headless Linux systems; a desktop is just packages plus a
port. Both provisioners compose the four moo verbs — no special moo
surface — and end with `moo save`, so the result is part of the
machine's versioned state: it survives `moo drop` + `moo new`, and
forks of the machine carry their own copy.

## Prerequisite: forward the desktop port

The project's `moo.toml` must forward guest port 6901:

```toml
[network]
ports = [6901]
```

If the machine already exists without the port, recreate it:
`moo drop <name> && moo new <name>`.

## Set up the desktop

Run from inside the project, with the target machine name (defaults to
`desktop`). From a source checkout:

```bash
scripts/desktop.sh my-machine
```

Binary-only install (script is attached to every release):

```bash
curl -fsSL https://github.com/heyito/moo/releases/latest/download/desktop.sh | bash -s -- my-machine
```

One-time setup installs XFCE + VNC + noVNC inside the machine (a few
minutes), wires it to start on every boot, waits until it answers, and
saves a snapshot. It prints a `http://localhost:<host-port>/vnc.html`
URL when ready. Desktop terminals open in the synced working tree.

## Reopen later

The desktop starts on every boot. To get the URL again (read-only,
touches nothing):

```bash
moo open my-machine 6901 '/vnc.html?autoconnect=true&resize=scale'
```

## Agent toolkit (compilers, Node.js, headless Chromium)

To equip a machine for development and browser testing — and snapshot
it as a baseline other machines fork from in under a second:

```bash
scripts/agent-base.sh base        # or the release asset agent-base.sh
moo new attempt-1 from base
```

Run both provisioners against the same machine and the desktop gets a
working browser.

## Troubleshooting

- URL never answers: `moo run <name> -- 'cat /var/log/desktop-*.log'`.
- "does not forward guest port 6901": add the port to `moo.toml` and
  recreate the machine (see prerequisite above).
- Services must listen on `0.0.0.0` to be reachable from the host; the
  provisioner handles this for the desktop itself.
