# moo — Vision

> **Git versions files. `moo` versions the machine.**
> One primitive. Four verbs. A hardware-isolated Linux runtime forked from
> a commit, saved back to a commit, followed by `git checkout`, dropped
> when you're done.

---

## 1. The gap

Git versions files, and has always been very good at this. But files alone
are not enough to reproduce the state a running system was in — the
migrations applied, the packages installed, the services started, the seed
data loaded. That runtime state has never been a first-class artifact
anywhere. `git log` knows nothing about it.

With agents that install packages, run migrations, seed databases, and
start services on every task, this gap turns into daily collisions:
`git worktree` isolates files, but the database, the ports, the `.env`,
the installed packages, and the running services are shared. A migration
run by one agent corrupts the test assumptions of another, sibling
worktrees fight over ports with `EADDRINUSE`, and running a separate
Docker Compose per worktree turns a laptop into a data center.

Local microVM sandboxes have started to close the *isolation* half of
this gap. Docker Sandboxes, Microsandbox, and others give an agent a
safe place to run. What none of them do is treat the resulting runtime
state as a versioned artifact of the git repo. That specific gap is
what `moo` fills.

## 2. The primitive

`moo` gives you one thing: a **machine**.

A machine is a hardware-isolated Linux runtime with a copy-on-write disk,
descended from a content-addressed base image, identified by a stable
user-chosen handle. Every saved state of a machine is content-addressed
and associated with a git commit. Machines are a disk plus a recipe — the
hypervisor is an implementation detail.

Two properties of modern hosts make this cheap enough to be routine:
hardware-isolated Linux runtimes boot in well under a second on a
developer machine, and copy-on-write filesystems (APFS `clonefile`,
reflink, ZFS) make forking or snapshotting a 20 GB environment a
metadata operation, not a copy.

Four verbs, each an extension of a git verb you already use:

- **`moo new <name> [from <src>]`** — like `git checkout -b`, but for
  runtime. Creates a machine. `<src>` can be a git ref, a commit SHA, a
  saved snapshot, or another machine. Sub-second copy-on-write.
  Idempotent: if `<name>` exists, restore the snapshot matching current
  git HEAD if one exists, otherwise the current live overlay. Add
  `--detached` for ephemeral machines that auto-GC.
- **`moo run <name> -- <cmd>`** — execute inside a machine. Captures
  stdout, stderr, exit code. Long-running services persist between
  invocations (`docker exec` semantics, not `docker run --rm`). Subsumes
  exec, ssh, logs, and doctor.
- **`moo save [<name>]`** — like `git commit`, but for runtime. Quiesce
  the machine and snapshot its state, associating the snapshot with the
  current HEAD SHA of the ref the handle shadows. Idempotent: same HEAD
  + same content = same snapshot, deduped.
- **`moo drop <name>`** — destroy the live machine and its overlay. Saved
  snapshots survive.

The rest composes from these four plus `git`.

## 3. What it feels like

Branch, mutate, commit, snapshot — the full loop:

```
$ moo new feat/billing                       # runtime for this branch
$ moo run feat/billing -- npm run migrate    # migration applied
$ git commit -am "add billing migration"
$ moo save feat/billing                      # snapshot tagged with commit SHA
```

Time-travel — the runtime follows `git checkout`:

```
$ git checkout HEAD^                         # go back one commit
$ moo new feat/billing                       # boots HEAD^'s snapshot
                                             # migration is gone; state matches code

$ git checkout main
$ moo new feat/billing                       # jumps forward to the latest saved state
```

`git bisect` with real runtime — the demo that is uniquely `moo`:

```
$ git bisect start bad-sha good-sha
$ git bisect run bash -c 'moo new probe && moo run probe -- npm test'
# each commit boots its saved runtime; migrations, seeds, and installed
# packages match the code under test. bugs that only reproduce against a
# specific migration state become bisectable.
```

Fork one machine from another, promote the winner:

```
$ moo new attempt-1 from feat/billing        # CoW fork, sub-second
$ moo run attempt-1 -- claude "refactor"     # agent runs in the fork
$ moo save attempt-1                         # snapshot the result
$ git merge attempt-1                        # promote via git
$ moo new feat/billing                       # boots the merged state
$ moo drop attempt-1                         # cleanup
```

Rewind runtime and code together:

```
$ moo drop <name> && git reset --hard HEAD^
# runtime rewinds. code rewinds. no "moo reset."
```

Replace the five-tool stack in one motion:

```
# Before: worktree + port-offset script + .env symlink + pgbranch + compose-project-name
$ git worktree add ../app-agent-b -b agent/b
$ PORT_OFFSET=20 ./scripts/worktree-env.sh
$ pgbranch create agent-b
$ ln -s ../../app/.env.local .env.local
$ docker compose -p agent-b up -d

# After:
$ git worktree add ../app-agent-b -b agent/b
$ moo new agent-b
```

## 4. What git-native runtime enables

The value is anything git already does, extended to include runtime:

- **`git bisect` with runtime state.** Bugs that only reproduce with a
  specific migration or seed become bisectable — the sharpest single
  capability `moo` uniquely enables.
- **Time-travel to any historical commit's runtime.** Not just the code
  — the exact database, cache, and dependency state that existed there.
- **Branch-shaped runtimes.** A durable name that saves per commit and
  follows `git checkout`.
- **Fork-and-promote for agent attempts.** Fork a machine, let an agent
  work, `git merge` the winner, drop the losers.
- **CI parity by construction.** The machine that produced the commit
  *is* the machine that reproduces the commit. No separate CI env drift.
- **Auto-follow git-checkout (opt-in).** `moo hook install` writes
  fail-silent `post-checkout`, `post-commit`, `post-merge`, and
  `post-worktree` scripts so the runtime follows the code automatically.
  Reversible in one command. Off by default.
- **Portable snapshot format.** Content-addressed, dedup-friendly,
  git-adjacent. In principle shareable alongside commits (v-future).

These are the capabilities the git-native framing uniquely produces. The
runtime isolation itself — microVM sandboxes for agents — is already a
category with good implementations, and `moo` does not need to reinvent
it to be useful.

## 5. Design principles

Two:

1. **Compose with git first.** Anything that can be done by chaining
   `new`, `run`, `save`, and `drop` with existing git verbs and shell
   loops should not become a new verb in `moo`.
2. **The backend never leaks.** `libkrun`, `Firecracker`, `Apple VZ`,
   `HVF`, `krunfw` — none of these words appear in commands, config, or
   error messages. A machine is a disk and a recipe.

Four verbs is a starting bar, not a religion. Additions require a strong
case that the workflow cannot be expressed by composition — and a
stronger case that the fifth verb pays for the conceptual weight it adds.

## 6. How it compares

How `moo` relates to existing tools in the same space:

| Tool                                    | Runtime isolation      | Local & private   | Per-fork CoW state | **Commit-tied snapshots** |
|-----------------------------------------|------------------------|-------------------|--------------------|---------------------------|
| `git worktree`                          | shared runtime         | yes               | no                 | no                        |
| Docker Compose                          | partial (shared kernel)| yes               | no                 | no                        |
| Dev Containers / Codespaces             | partial (shared kernel)| Codespaces hosted | no                 | no                        |
| Gitpod / Coder                          | container / VM         | hosted            | no                 | no                        |
| E2B / Modal / Daytona / Vercel Sandbox  | microVM / container    | hosted, metered   | yes                | session-scoped            |
| Microsandbox / SmolVM                   | microVM                | yes               | yes                | manual                    |
| **Docker Sandboxes (`sbx`)**            | **microVM**            | **yes**           | not first-class    | **no**                    |
| pgbranch / wtdb / db-git / branchdb     | DB only                | yes               | DB only            | git-hook, DB only         |
| **moo**                                 | microVM                | yes               | first-class        | **yes, per commit**       |

## 7. North star

`moo` becomes the standard way runtime state is versioned against a git
repo — the layer `git log` doesn't see today. It succeeds if:

- A developer can `git checkout <sha>` and reboot the exact runtime that
  existed at that commit.
- `git bisect` with a runtime-dependent test is a one-liner.
- The `git worktree add` + `moo new` combination replaces the
  worktree + port-offset + `.env` + DB-branch + compose stack for
  parallel-agent work.
- The snapshot format is open and portable enough that snapshots
  produced by `moo` can move between hosts, and eventually between
  providers.

Everything beyond the core primitive — snapshot registry, team sharing,
CI integration, platform build-out — is v-future and depends on whether
the primitive earns its keep first.
