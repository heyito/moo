# moo

> **Git versions files. `moo` versions the machine.**

`moo` gives every git branch, worktree, or agent attempt its own
hardware-isolated Linux machine — database, ports, packages, services and
all — with the machine's state **saved per commit and restored by
`git checkout`**.

```
$ moo new feat/billing                       # a machine for this branch
$ moo run feat/billing -- npm run migrate    # migration applied inside it
$ git commit -am "add billing migration"
$ moo save feat/billing                      # runtime snapshot, tagged with the commit

$ git checkout HEAD^                         # rewind the code…
$ moo new feat/billing                       # …and the machine follows.
                                             # the migration is gone; state matches code
```

One noun (the **machine**), four verbs (`new`, `run`, `save`, `drop`).
Everything else composes from those four plus the git you already know.

## Why

Running parallel coding agents against one repo hits the same wall every
time: `git worktree` isolates *files*, but the database, the ports, the
`.env`, the installed packages, and the running services all collide. The
common workaround is a five-tool stack — worktrees + a port-offset script +
`.env` symlinks + a DB-per-branch tool + `docker compose -p` hacks.

`moo` replaces the stack with one motion:

```
$ git worktree add ../app-agent-b -b agent/b
$ moo new agent-b
```

Each machine is a full Linux microVM with copy-on-write state. Six machines
on a 20 GB base cost megabytes and fork in under a second. And unlike any
sandbox, the machine's state is a **versioned artifact of the repo**:
`git bisect` can boot the exact runtime — migrations, seeds, packages —
that existed at every commit it probes.

## Install

Requirements: Apple Silicon Mac, [Homebrew](https://brew.sh),
[Rust](https://rustup.rs). No root, no daemon, nothing runs in the
background except your machines.

```
$ git clone <this repo> && cd moo
$ scripts/install.sh
```

The script installs the isolation runtime and filesystem tools via
Homebrew, builds and signs the binary, and finishes with `moo doctor` —
four green checks and you're ready.

## The four verbs

```
moo new <name> [from <src>] [--detached]   create or restore a machine
moo run <name> -- <cmd> [args...]          execute inside the machine
moo save [<name>]                          snapshot state, tagged with the current commit
moo drop <name> [--force] [--snapshots]    destroy the machine (snapshots survive)
```

- **`new`** is idempotent, like `git checkout`. If a snapshot exists for the
  commit the handle shadows, it boots that snapshot; otherwise it boots the
  current live state; otherwise it creates a fresh machine from the base
  image. `<src>` can be a git ref or SHA, a snapshot ID, or another
  machine's name (sub-second copy-on-write fork).
- **`run`** has `docker exec` semantics: services you start keep running
  between invocations. Exit codes and output round-trip faithfully.
- **The working tree follows you automatically.** `new` and `run`, invoked
  from inside the machine's repository, sync your working tree — tracked
  files plus untracked-unignored files, exactly what `git status` calls
  your work — into the machine at `/srv/app` (configurable via
  `[project] workdir`). Unchanged trees are skipped in milliseconds.
  Gitignored files are never pushed and never deleted, so `node_modules`,
  build output, and the machine's own `.env` survive every sync. The host
  tree is authoritative: files you delete or switch away from on the host
  disappear in the machine too.
- **`save`** is `git commit` for the runtime. Idempotent — same commit,
  same content, same snapshot. Byte-identical states share storage.
- **`drop`** destroys the live machine. Saved snapshots survive unless you
  pass `--snapshots`.

Admin, read-only: `moo ls` (machines, ports, snapshots), `moo doctor`
(host checks).

## A clickable desktop (optional)

Machines are headless, but they are full Linux systems — a desktop is
just packages plus a port. With `ports = [6901]` in `moo.toml`:

```
$ scripts/desktop.sh my-machine
```

installs XFCE + VNC + a browser client inside the machine, starts it on
every boot, saves a snapshot, and prints a `localhost` URL you can click
around in. The desktop is part of the machine's state: forks of the
machine get their own desktop on their own port, and `moo new` after a
`git checkout` boots the desktop exactly as it was at that commit.

## Restore semantics — read this once

`moo new <name>` on an existing handle **prefers the snapshot saved for the
current commit** over the live overlay. That is the whole point — the
runtime follows the code — but it means unsaved runtime work is replaced
when you switch commits. The rule of thumb is the same as git's:
**`moo save` before you `git checkout`**, the way you `git commit` before
you switch branches. A shell alias makes it automatic:

```sh
gitcommit() { git commit "$@" && moo save; }
```

## Configuration (`moo.toml`, optional)

Committed to the repo if used. This is the whole schema — there are no
service graphs, health checks, or volumes:

```toml
[project]
base = "debian:bookworm"     # any OCI image reference
workdir = "/srv/app"         # where the working tree is synced in the guest

[recipe]
lockfiles = ["package-lock.json"]   # participate in the base image identity

[resources]
cpus = 2
memory = "4GiB"

[network]
ports = [5432, 3000]         # guest ports; each gets a stable host port (moo ls shows the map)

[quiesce]
commands = [                 # run inside the guest before every save
  "su postgres -c 'psql -c CHECKPOINT'",
]
```

The base image is built automatically on first use — layers are fetched
straight from the registry (no Docker daemon needed) and assembled into a
bootable disk, unprivileged. Machines with the same base + lockfiles share
one image.

## What it's for

- **Time travel.** `git checkout <old-sha>` + `moo new <name>` boots the
  exact runtime that existed at that commit.
- **Runtime-dependent bisects.** Bugs that only reproduce against a
  specific migration state become bisectable:

```
$ git bisect start bad-sha good-sha
$ git bisect run bash -c 'moo new probe && moo run probe -- npm test'
```

- **Parallel agents, zero collisions.** Every attempt gets its own DB,
  ports, packages, and services:

```
$ for name in a b c; do moo new agent-$name from HEAD; done
```

- **Fork-and-promote.** Fork a machine, let an agent work, `git merge` the
  winner, `moo drop` the losers.

## Contracts and limitations

- **Durability.** A live machine's disk survives machine shutdown, not
  host power loss. Snapshots are flushed to physical disk and survive
  power loss.
- **Network isolation.** Every machine has a fully private network stack.
  `localhost` inside the machine is the machine's own loopback — never the
  host's. Host services are not reachable from the guest's loopback, and
  two machines never share network state.
- **Ports.** Guest TCP services listed in `[network] ports` are reachable
  on host localhost at the port shown by `moo ls`. Like containers, a
  service must listen on a non-loopback address (`0.0.0.0`) to be
  reachable from the host. TCP half-close is not proxied faithfully —
  plain request/response protocols (HTTP etc.) are unaffected.
- **Platform.** macOS Apple Silicon hosts, Linux guests (arm64). Linux
  host support is planned.
- `git reset --hard` and `git rebase` move HEAD without any hook — the
  machine doesn't auto-follow; run `moo new <name>` afterwards.

## See it work

Three self-contained, self-verifying demos (each cleans up after itself):

```
$ scripts/demo-timetravel.sh   # runtime follows git checkout, survives drop
$ scripts/demo-parallel.sh     # 3 machines, same guest port, zero collisions
$ scripts/demo-bisect.sh       # git bisect finds a bug that only exists in a
                               # specific migration state — unattended
```

## Development

```
$ cargo build --release             # builds the CLI + embedded guest agent
$ scripts/leakcheck.sh              # gate: no backend names in user output
```

The isolation backend is an implementation detail and never appears in
user-facing output; `scripts/leakcheck.sh` enforces this in CI.
