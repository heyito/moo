# moo — Product Vision

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

In 2020 this was a nuisance. In 2026 — with agents that install packages,
run migrations, seed databases, and start services on every task — it is
the most-blogged dev-infra pain of the year. Every serious write-up on
running parallel Claude Code / Codex / Cursor agents lands on the same
wall: `git worktree` isolates files, but the database, the ports, the
`.env`, the installed packages, and the running services collide.
Developers describe migrations from one agent corrupting the test
assumptions of another, `EADDRINUSE` port wars between sibling worktrees,
and "spin up a separate Docker Compose per worktree? your laptop is now
a data center."

The workaround market has already voted with its feet. In 2025–2026,
at least **seven independent projects** shipped some flavor of
"per-git-branch database" — three separately named `pgbranch`, plus
`wtdb`, `db-git`, `dbfork`, `branchdb`, plus a Rails gem, plus Neon's
own worktree-subagent guide. Several install the same `post-checkout`
hook `moo` describes. Every one of them is DB-only — because the DB is
~80% of the pain and `CREATE DATABASE ... TEMPLATE` is 100× easier than
a microVM. That is the shape of the gap: the pain is validated, the
design is validated, the whole-runtime version is missing.

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

## 6. Why now

- **The pain became loud in 2026.** Parallel coding agents against one
  repo made the runtime-collision problem daily, not theoretical. Seven
  independent per-git-branch workaround tools shipped in one year; every
  worktree-agent tutorial in 2026 ends on the same "you still need to
  solve the DB, the ports, and the `.env`." The market is not silent
  about wanting this.
- **State branching is a loved behavior, not demo-ware.** Neon reports
  ~500,000 database branches created per day; PlanetScale users call
  branching "the one feature we miss most when we touch other systems."
  When state branching has been shipped in a slice, it has been used at
  scale. `moo` generalizes proven behavior from the DB to the whole
  runtime.
- **Local microVMs are now a real category.** libkrun, Apple VZ, and
  Docker's own sandbox effort have proved that hardware-isolated Linux
  runtimes on a developer machine are cheap, fast, and safe enough for
  autonomous agent execution. That is a solved problem, not something
  `moo` needs to prove.
- **What isn't solved is versioning those runtimes against the repo.**
  Every existing sandbox treats the workspace as ephemeral and the
  microVM as disposable. None of them make runtime state a first-class
  artifact of the git commit that produced it.
- **Copy-on-write is universal.** APFS `clonefile`, reflink, and ZFS
  make forking a 20 GB dev environment a metadata operation, not a copy
  — and make snapshot-per-commit cheap enough to be routine.
- **Vagrant is the ghost that doesn't haunt you.** Vagrant died from
  multi-minute boots, gigabytes of RAM per VM, and provisioning pain —
  not from a lack of desire for versioned environments. HN commenters
  still say "I miss that pattern so much." `moo`'s premises (sub-200 ms
  boot, CoW metadata-op forks, snapshot-per-commit at the git layer)
  remove precisely the mechanisms that killed Vagrant. The desire
  survived the tool.

The gap is specific and small. The isolation category is filling in.
The git-native runtime versioning category isn't.

## 7. How it's different

`moo` is not competing for the local microVM category. That category
exists and has strong entrants. The real v1 competitor is not Docker
Sandboxes — it is the **five-tool stack** developers running parallel
agents already glue together:

1. `git worktree` for file isolation,
2. a port-offset shell script for `EADDRINUSE`,
3. a `.env` symlink for missing environment,
4. a `pgbranch`-style tool for per-branch database state,
5. a `docker-compose --project-name` hack for services.

Every workaround blog post from 2026 lands on some variant of this
stack. `moo` replaces all five with `moo new` + `moo run` + `moo save`,
inside the same `git worktree` motion the target user already uses. The
pitch is not "isolation" (Docker Sandboxes owns that) or "database
branching" (seven free tools own that). It is **one tool replacing the
five-tool stack, with the runtime versioned against the commit.**

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

Docker Sandboxes is the closest peer on isolation — real distribution,
integrated with major coding agents, no commit-tied snapshots. The
DB-per-branch tools are the closest peer on git-native versioning
behavior — one collision layer solved, four to go. `moo` sits between
the two categories with a claim neither can make: **the whole runtime,
versioned per commit, one tool.** It could plausibly run on top of
another project's microVM primitive — the git-native versioning layer
is the piece nobody else is building.

## 8. North star

`moo` becomes the standard way runtime state is versioned against a git
repo — the layer `git log` doesn't see today. It succeeds if:

- A developer can `git checkout <sha>` and reboot the exact runtime that
  existed at that commit.
- `git bisect` with a runtime-dependent test is a one-liner.
- The `git worktree add` + `moo new` combination replaces the five-tool
  stack for the parallel-coding-agent workflow already teaching itself
  in the wild.
- The snapshot format is open and portable enough that snapshots
  produced by `moo` can move between hosts, and eventually between
  providers.

The runtime isolation itself, and the mainstream agent-sandboxing use
case, may well be owned by others. `moo` doesn't need to own that to be
worth building. It rides the existing worktree-agent motion instead of
asking anyone to change how they work.

## 9. What year-one success looks like

- **Install and first useful machine in under five minutes** on a clean
  Apple Silicon Mac.
- **`moo new` returns in under a second** for a warm 20 GB base image.
- **`moo save` completes in under a second** for a typical commit's
  worth of state change.
- **`git checkout <old-sha>` + `moo new <name>`** restores the exact
  runtime that existed at that commit, deterministically.
- **`git bisect run` with `moo new` in the loop** works well enough to
  be the recommended way to hunt runtime-dependent regressions.
- **A documented, stable snapshot format** — versioned, portable across
  `moo` releases, and open enough to build on.
- **At least one project publicly using `moo`** for parallel agent
  attempts against real commits, with winners promoted via `git merge`.

## 10. Kill criteria

The decision to build `moo` carries three explicit off-ramps. Written
down now so they cannot be rationalized away later.

- **Before building** — if a weekend spike cannot achieve microVM boot
  plus APFS-CoW snapshot restore under ~2 s on a real Apple Silicon
  Mac, the core promise is at risk and the plan re-opens. (This gate
  passed; the spike lives in `crates/spike`.)
- **At three months** — if the worktree-agent developers who wrote the
  2026 collision-pain blog posts (the natural first 20 users) try `moo`
  and still prefer their five-tool script stack, the whole-runtime
  premise is wrong. Fall back to contributing to the DB-per-branch
  tools instead of building parallel to them.
- **Anytime** — if Docker Sandboxes or Morph ships commit-keyed runtime
  snapshots as a first-class feature, the git-native wedge is gone.
  Pivot to the snapshot-format / interop-layer play, or stop.

Everything beyond the year-one bar — snapshot registry, team sharing,
CI integration, platform build-out — is v-future and depends on whether
the primitive earns its keep first.
