# got

> **Git versions files. `got` versions the machine.**

`got` gives every git branch, worktree, or agent attempt its own
hardware-isolated Linux machine — database, ports, packages, services and
all — with the machine's state **saved per commit and restored by
`git checkout`**.

```
$ got new feat/billing                       # a machine for this branch
$ got run feat/billing -- npm run migrate    # migration applied inside it
$ git commit -am "add billing migration"
$ got save feat/billing                      # runtime snapshot, tagged with the commit

$ git checkout HEAD^                         # rewind the code…
$ got new feat/billing                       # …and the machine follows.
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

`got` replaces the stack with one motion:

```
$ git worktree add ../app-agent-b -b agent/b
$ got new agent-b
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
$ git clone <this repo> && cd got
$ scripts/install.sh
```

The script installs the isolation runtime and filesystem tools via
Homebrew, builds and signs the binary, and finishes with `got doctor` —
four green checks and you're ready.

## The four verbs

```
got new <name> [from <src>] [--detached]   create or restore a machine
got run <name> -- <cmd> [args...]          execute inside the machine
got save [<name>]                          snapshot state, tagged with the current commit
got drop <name> [--force] [--snapshots]    destroy the machine (snapshots survive)
```

- **`new`** is idempotent, like `git checkout`. If a snapshot exists for the
  commit the handle shadows, it boots that snapshot; otherwise it boots the
  current live state; otherwise it creates a fresh machine from the base
  image. `<src>` can be a git ref or SHA, a snapshot ID, or another
  machine's name (sub-second copy-on-write fork).
- **`run`** has `docker exec` semantics: services you start keep running
  between invocations. Exit codes and output round-trip faithfully.
- **`save`** is `git commit` for the runtime. Idempotent — same commit,
  same content, same snapshot. Byte-identical states share storage.
- **`drop`** destroys the live machine. Saved snapshots survive unless you
  pass `--snapshots`.

Admin, read-only: `got ls` (machines, ports, snapshots), `got doctor`
(host checks).

## Restore semantics — read this once

`got new <name>` on an existing handle **prefers the snapshot saved for the
current commit** over the live overlay. That is the whole point — the
runtime follows the code — but it means unsaved runtime work is replaced
when you switch commits. The rule of thumb is the same as git's:
**`got save` before you `git checkout`**, the way you `git commit` before
you switch branches. A shell alias makes it automatic:

```sh
gitcommit() { git commit "$@" && got save; }
```

## Configuration (`got.toml`, optional)

Committed to the repo if used. This is the whole schema — there are no
service graphs, health checks, or volumes:

```toml
[project]
base = "debian:bookworm"     # any OCI image reference

[recipe]
lockfiles = ["package-lock.json"]   # participate in the base image identity

[resources]
cpus = 2
memory = "4GiB"

[network]
ports = [5432, 3000]         # guest ports; each gets a stable host port (got ls shows the map)

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

- **Time travel.** `git checkout <old-sha>` + `got new <name>` boots the
  exact runtime that existed at that commit.
- **Runtime-dependent bisects.** Bugs that only reproduce against a
  specific migration state become bisectable:

```
$ git bisect start bad-sha good-sha
$ git bisect run bash -c 'got new probe && got run probe -- npm test'
```

- **Parallel agents, zero collisions.** Every attempt gets its own DB,
  ports, packages, and services:

```
$ for name in a b c; do got new agent-$name from HEAD; done
```

- **Fork-and-promote.** Fork a machine, let an agent work, `git merge` the
  winner, `got drop` the losers.

## Contracts and limitations

- **Durability.** A live machine's disk survives machine shutdown, not
  host power loss. Snapshots are flushed to physical disk and survive
  power loss.
- **Ports.** Guest TCP services are reachable on host localhost at the
  port shown by `got ls`. TCP half-close is not proxied faithfully — plain
  request/response protocols (HTTP etc.) are unaffected.
- **Platform.** macOS Apple Silicon hosts, Linux guests (arm64). Linux
  host support is planned.
- `git reset --hard` and `git rebase` move HEAD without any hook — the
  machine doesn't auto-follow; run `got new <name>` afterwards.

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
