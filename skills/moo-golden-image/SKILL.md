---
name: moo-golden-image
description: Set up moo for a project by creating its golden base image and a provisioned baseline machine. Use when starting to use moo in a repository, when the user asks to create a golden image, set up moo, provision a base environment, or start from a blank VM.
---

# Create a moo golden image

`moo` versions runtime state (database, packages, services) against git
commits. This skill is the one-time setup for a project: define the base
image recipe, provision it with the project's runtime, and save a baseline
everyone forks from.

## Prerequisites

Run `moo doctor`. All checks must pass. If `moo` is not installed, run
`scripts/install.sh` from the moo repository.

## Step 1: Write `moo.toml` at the repo root

Only if it doesn't exist. Minimal schema — no services, no volumes:

```toml
[project]
base = "debian:bookworm"          # any OCI image reference
workdir = "/srv/app"              # guest path the working tree syncs into

[recipe]
lockfiles = ["package-lock.json"] # files whose content identifies the image

[resources]
cpus = 2
memory = "4GiB"

[network]
ports = [5432, 3000]              # guest ports the app listens on

[quiesce]
commands = []                     # e.g. a DB checkpoint, run before every save
```

Pick `ports` by inspecting the project (dev server port, DB port). Pick
`base` to match the project's ecosystem; `debian:bookworm` is the safe
default. Commit `moo.toml`.

## Step 2: Create the baseline machine

```bash
moo new base
```

The first `new` for a recipe builds the golden image automatically
(fetches the OCI layers, no Docker daemon needed). Takes seconds to a few
minutes depending on image size.

## Step 3: Provision inside the machine

For the common case — compilers, developer CLIs, database clients,
Node.js, headless Chromium — run `scripts/agent-base.sh base` from the
moo repository and skip to the project-specific parts below.

`moo new base` already synced the working tree to the guest workdir.
Install the project's runtime **inside** the machine, not on the host.
Everything runs as root in the guest:

```bash
moo run base -- 'apt-get update -q && apt-get install -y -q git curl <runtime> <db>'
moo run base -- '<start services, create db users, etc.>'
moo run base -- 'cd /srv/app && <install dependencies: npm ci / bun install / pip install ...>'
```

Write service starts to `/etc/rc.local` (make it executable) — the machine
runs it on every boot, so services come back after restores and reboots:

```bash
moo run base -- 'printf "#!/bin/sh\nservice postgresql start\nservice redis-server start\n" > /etc/rc.local && chmod +x /etc/rc.local && sh /etc/rc.local'
```

Services that should be reachable from the host (via the `moo ls` port map)
must listen on `0.0.0.0`, not only on loopback — the same rule as
containers. The machine's own loopback is private: `localhost` inside the
machine never reaches host services, and machines never see each other.

Gitignored files (like `.env`) are not synced; seed required secrets or
local config into the machine explicitly:

```bash
moo run base -- "printf '%s\n' 'DATABASE_URL=...' > /srv/app/.env"
```

Best effort: if a dependency or credential isn't available, provision what
is possible and tell the user what was skipped.

## Step 4: Save the baseline

```bash
moo save base
```

This snapshots the provisioned state, tagged with the current commit.
Verify with `moo ls` — the `base` handle should show one snapshot.

## Result

- New machines fork the provisioned baseline in under a second:
  `moo new feat/x from base`
- The `base` machine can stay stopped; its snapshot survives even
  `moo drop base`.
- Rebuilding from blank: `moo drop base --snapshots`, then repeat from
  Step 2.

## Notes

- Two developers with the same `moo.toml` and lockfile contents share the
  same golden image identity.
- Changing `base`, `lockfiles` content, or `[resources]` creates a new
  image on next use; old machines are unaffected.
