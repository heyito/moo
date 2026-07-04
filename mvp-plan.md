# moo — MVP Plan

> The smallest version of `moo` that proves the one thing nobody else does:
> **runtime state versioned against git commits.** Everything in this
> document serves two demos — save a machine at a commit and `git checkout`
> back into the exact runtime that existed there, and replace the
> five-tool parallel-agent stack with one `moo new`.

This plan is derived from [vision.md](vision.md) (the *why*) and
[plan.md](plan.md) (the *how*). It does not change any decision made there;
it selects the minimal subset and sequences it. Where plan.md defines M0
and M1, the MVP is: **M0 in full, plus the narrowest cut of M1 that makes
the core demos real for an outside user on a clean Apple Silicon Mac.**

---

## 1. The MVP thesis

The isolation half of the problem (local microVM sandboxes) is a solved
category. The MVP must not spend scope proving it again. The MVP exists to
prove the *versioning* half, and to land it inside the workflow where the
pain is already loudest — parallel coding agents against one repo
(vision.md §1, §7):

1. A machine's runtime state can be snapshotted and associated with a git
   HEAD SHA (`moo save`).
2. `git checkout <old-sha>` + `moo new <name>` deterministically restores
   the runtime that existed at that commit.
3. N machines run in parallel with no collisions on database, ports, env,
   packages, or services — the whole five-tool stack collapses into
   `moo new`.
4. The whole loop is fast enough to be routine — sub-second forks and
   saves on real project sizes.

If those hold, `git bisect` with runtime state falls out for free, the
worktree-agent audience has a reason to switch, and the primitive has
earned further investment. If they don't, no amount of polish elsewhere
matters — see the kill criteria (§6 here, vision.md §10).

## 2. The acceptance demos

The MVP is done when these scripts work, unmodified, on a clean Apple
Silicon Mac after a documented install, with zero hypervisor names in any
output.

**Demo 1 — time travel (the differentiator):**

```
$ moo new feat/billing                        # machine boots from base image
$ moo run feat/billing -- ./setup-db.sh       # migration + seed applied
$ git commit -am "add billing migration"
$ moo save feat/billing                       # snapshot tagged with HEAD SHA

$ moo run feat/billing -- ./mutate-more.sh    # further state change
$ git commit -am "second migration"
$ moo save feat/billing

$ git checkout HEAD^                          # rewind code one commit
$ moo new feat/billing                        # boots snapshot for HEAD^
$ moo run feat/billing -- ./verify-state.sh   # second migration is GONE

$ git checkout -                              # forward again
$ moo new feat/billing                        # boots the later snapshot
$ moo run feat/billing -- ./verify-state.sh   # second migration is BACK

$ moo drop feat/billing                       # live machine gone
$ moo new feat/billing                        # restored from snapshot; state intact
```

**Demo 2 — the five-tool stack replaced (the wedge audience):**

```
# Before (vision.md §7): worktree + port-offset script + .env symlink
#                        + pgbranch + compose-project-name
# After:
$ for name in agent-a agent-b agent-c; do moo new $name from HEAD; done
$ moo run agent-a -- npm run migrate          # each has its own DB,
$ moo run agent-b -- npm run dev &            # own ports, own packages,
$ moo run agent-c -- claude "refactor"        # own services — zero collisions
$ moo save agent-b                            # keep the winner's runtime
$ git merge agent-b                           # promote via git
$ moo drop agent-a agent-c                    # losers gone
```

**Demo 3 — the headline showcase, run on a seeded demo repo:**

```
$ git bisect start bad-sha good-sha
$ git bisect run bash -c 'moo new probe && moo run probe -- npm test'
# finds a regression that only reproduces against a specific migration state
```

## 3. Scope

### In the MVP

- **The four verbs**: `new`, `run`, `save`, `drop` — with the exact
  semantics in plan.md §6 (idempotent `new` with snapshot restore,
  `docker exec`-style `run`, content-addressed dedup'd `save`,
  snapshot-preserving `drop`).
- **`moo doctor` and `moo ls`** — read-only admin. `doctor` is required
  because macOS codesigning/entitlement friction is the #1 install-time
  failure mode; `ls` because users cannot trust an invisible registry
  (and Demo 2 needs the port map visible).
- **libkrun via direct C ABI FFI**, pinned `stable-1.19.x`, dynamically
  linked, per plan.md §3.3. macOS Apple Silicon **only**.
- **Model B storage** — working tree inside the machine overlay. One CoW
  disk per machine (plan.md §5.2).
- **SQLite registry** with both tables (`machines`, `snapshots`) per
  plan.md §4.2, including provenance
  `(base_commit, recipe_hash, parent_machine)` at `new`.
- **Content-addressed snapshots** under `~/.moo/snapshots/<content_hash>`
  with quiesce → `F_FULLFSYNC` → `clonefile` ordering and dedup
  (plan.md §5.3).
- **Guest exec + quiesce agent** — small static Rust binary in the golden
  image; runs commands for `moo run` and `sync` + registered DB
  checkpoints for `moo save`.
- **TSI networking** with deterministic per-handle port allocation and
  bind-failure retry (plan.md §7). This is what makes Demo 2's
  "zero port collisions" claim true without a port-offset script.
- **Minimal `moo.toml`** — `[project] base`, `[recipe] lockfiles`,
  `[resources]`, `[quiesce] commands`. Exactly the plan.md §9 schema,
  nothing more.
- **Handle shadowing of git refs** — read-only; `moo` never runs git.
- **Install path**: Homebrew formula (or a documented script) that
  installs the binary + `libkrunfw`, runs the ad-hoc codesign with the
  hypervisor entitlement, and verifies with `moo doctor`.
- **A seeded demo repository** with a runtime-dependent bug for the
  bisect showcase, plus the five-tool-stack before/after comparison.
  This is a deliverable, not an afterthought — it is the pitch to the
  worktree-agent audience that vision.md §10 names as the first 20
  users.

### Deferred from M1 (in plan.md's M1, not in the MVP)

- **`moo hook install/uninstall/status`.** The primitive is complete
  without hooks (plan.md §8.1 says so explicitly). The MVP demos use
  manual `moo save` / `moo new`; a shell alias in the docs covers
  auto-save. Hooks ship in the first post-MVP release once restore
  semantics have survived contact with real users — notably the
  worktree-agent adopters, several of whose existing DB-per-branch
  tools already install the same `post-checkout` hook (vision.md §1),
  so the demand is proven and will still be there.
- **Full OCI/Dockerfile → rootfs build pipeline.** The MVP ships with a
  small set of prebuilt, content-addressed base images (a Debian base
  plus one batteries-included dev image) fetched on first use. The
  `moo.toml` `base` field accepts these names. Arbitrary OCI references
  and Dockerfiles land post-MVP; the M0 conversion spike decides the
  design either way. This is the single largest scope cut and it does
  not weaken the thesis — versioning is proven on any base image.

### Out (unchanged from plan.md non-goals)

Linux host, MCP adapter, `--tty`, Model A / virtio-fs, snapshot GC
policy, snapshot push/pull, memory snapshots, reverse proxy / DNS,
service graphs, agent-specific verbs, egress policy, secrets. See
plan.md §2 — none of these move for the MVP.

## 4. Work packages

Sequenced. Each has a hard definition of done. WP0 is plan.md's M0
verbatim, with the vision.md §10 kill gate bolted to the front; nothing
downstream starts until its exit gate passes.

### WP0 — Spike (plan.md M0, 4–6 wk)

Prove buildability; resolve every open question in plan.md §14.

- **Kill gate first (vision.md §10, "before building").** A weekend
  spike must achieve microVM boot plus APFS-CoW snapshot restore in
  under ~2 s on a real Apple Silicon Mac. If it cannot, stop and
  re-open the plan before any further WP0 work.
- Direct libkrun FFI boot of a Debian rootfs on macOS Apple Silicon.
  Codesigned, unprivileged.
- CoW clone timing on 2 GB and 20 GB overlays. **Gate: sub-500 ms.**
- Exec/quiesce transport bake-off: vsock UNIX-socket proxy vs
  virtio-serial. **Decide.**
- OCI → rootfs conversion approach (port Microsandbox's EROFS design vs
  external builder). **Decide** — even though the full pipeline is
  post-MVP, the prebuilt MVP images must be produced by the chosen
  design so they don't get thrown away.
- Snapshot content-hash algorithm: BLAKE3 vs SHA-256, measured on a
  20 GB overlay. **Decide.**
- Snapshot spike: quiesce → `F_FULLFSYNC` → `clonefile` + content-hash
  produces a byte-stable, restorable overlay. **Gate: restore is
  bit-exact and a reboot from it passes an app-level state check.**

**Exit gate (plan.md M0 DoD):** a throwaway binary that creates a
machine from a real image, runs a command in it, saves a snapshot
associated with a git SHA, restores that snapshot into a new machine,
and drops both — provenance recorded, zero backend strings in output.

### WP1 — Core engine

The real crates (`moo-cli`, `moo-core`, `moo-vmm`, `moo-store`), built
on WP0's validated decisions.

- SQLite registry, both tables, provenance at `new`.
- Golden image cache under `~/.moo/images/<hash>`; recipe hash =
  `hash(base + lockfile contents + resources)`.
- `new` / `drop` lifecycle: CoW fork, boot, TSI port map, sealed/live
  states, `--detached` handles, idempotent re-entry.
- `run`: exec agent transport (per WP0 decision), stdout/stderr/exit
  code capture, services persisting across invocations.

**DoD:** `moo new --detached from HEAD`, `moo run`, `moo drop` work
end-to-end on the prebuilt base image; **six parallel machines run
services and DBs simultaneously with zero port/file collisions**
(this is Demo 2's substance); fork of a warm 20 GB machine < 1 s.

### WP2 — Save & restore (the differentiator)

- `moo save`: quiesce (guest `sync` + Postgres/SQLite/Redis checkpoint
  hooks + `[quiesce] commands`) → host full fsync → `clonefile` to
  content-addressed store → registry row `(handle, head_sha,
  content_hash)`. Idempotent, dedup'd, full-sync always.
- Snapshot-aware `moo new`: existing handle looks up
  `(handle, current HEAD of shadowed ref)`; restore on hit, live
  overlay on miss.
- `moo drop` preserves snapshots; `--snapshots` deletes them;
  `--force` kills unquiesceable machines.
- Restore-semantics messaging: when `moo new` restores an older
  snapshot over a live overlay, say so loudly (this is the top
  user-confusion risk in plan.md §13).

**DoD:** the Demo 1 script passes end-to-end, including the
drop-then-restore step and byte-identical dedup on a no-change save.

### WP3 — Fit and finish for outsiders

- `moo doctor`: entitlements, `libkrunfw`, APFS + `clonefile` on the
  store path, image cache, snapshot integrity.
- `moo ls`: handles, lifecycle, port maps, snapshots per handle.
- Install: Homebrew formula (or script) with post-install ad-hoc
  codesign. **Gate: clean-Mac install to first machine < 5 min.**
- Error message audit + CI grep: no `libkrun`/`HVF`/`krunfw` in any
  stdout/stderr/usage string (plan.md §13 backend-leak check).
- Docs: README quickstart, the four verbs, restore semantics,
  the manual `git commit && moo save` alias, the five-tool-stack
  before/after (vision.md §3), and the known durability contract
  ("survives switch" vs snapshot "survives power loss").

**DoD:** a developer who has never seen the project completes Demos 1
and 2 from the README alone.

### WP4 — The launch showcase

- Build the seeded demo repo: an app with migrations where a bug only
  reproduces against a specific migration state, plus the parallel-
  agent scenario for Demo 2.
- Script and verify `git bisect run` with `moo new` + `moo run` in the
  loop finding the culprit commit.
- Record both demos (asciinema or equivalent): the bisect showcase and
  the five-tool-stack replacement. The recordings target the
  worktree-agent developers who wrote the 2026 collision-pain posts —
  vision.md §10 names them as the natural first 20 users and the
  three-month validation cohort, so the launch material must speak
  their language (worktrees, port wars, migration collisions).

**DoD:** bisect finds the planted regression unattended, and both
recordings exist.

## 5. Success criteria (measured, from vision.md §9)

| Criterion | Target |
|---|---|
| Clean-Mac install → first useful machine | < 5 min |
| `moo new` on a warm 20 GB base | < 1 s |
| `moo save` for a typical commit's state delta | < 1 s |
| `git checkout <old-sha>` + `moo new` restore | deterministic, bit-exact overlay |
| Parallel machines (6+) with services and DBs | zero port/DB/env collisions, no helper scripts |
| `git bisect run` with moo in the loop | works unattended on the demo repo |
| Backend leak check (CI grep of all user-facing strings) | 0 matches |

## 6. Kill criteria and top MVP risks

Vision.md §10 defines three off-ramps; the MVP operationalizes the
first two and watches for the third:

- **Before building:** the WP0 weekend-spike gate (boot + CoW snapshot
  restore < ~2 s). Failing it stops the project before the engine is
  built, not after.
- **At three months post-launch:** the worktree-agent developers who
  wrote the 2026 collision-pain posts try the MVP. If they still prefer
  their five-tool script stack, the whole-runtime premise is wrong —
  fall back to contributing to the DB-per-branch tools. WP4 exists to
  make this test fair: the launch material must reach that exact
  audience with demos in their language.
- **Anytime:** if Docker Sandboxes or Morph ships commit-keyed runtime
  snapshots as a first-class feature, the git-native wedge is gone.
  Pivot to the snapshot-format / interop-layer play, or stop. Monitor
  their releases throughout the build.

Remaining MVP-specific risks (subset of plan.md §13):

- **WP0 gate failure beyond the kill gate** (clone too slow, no viable
  exec transport, snapshot not byte-stable). *Response:* this is why
  WP0 exists — stop, re-plan, do not build the engine on an unproven
  substrate.
- **Restore-vs-live-overlay confusion** — users lose unsaved runtime
  work when `moo new` prefers a snapshot. *Response:* loud messaging in
  WP2, sharp docs in WP3. Hooks stay deferred until this is validated.
- **Codesigning/entitlement install friction.** *Response:* `doctor`
  in-scope from the start; the < 5 min install target is a hard gate,
  not a wish.
- **Scope creep back toward M1/M2** (hooks, OCI pipeline, MCP, TTY).
  *Response:* the §3 deferred list is the design gate. Anything not
  needed by the §2 demos waits.

## 7. After the MVP

First post-MVP release, in order of pull: `moo hook install` (auto-follow
on checkout/commit/merge — the behavior the DB-per-branch tools already
proved demand for), full OCI/Dockerfile image builds, Linux host support,
then the M2 items in plan.md §12 as adoption warrants. None of it starts
until the §2 demos have been run by hands that aren't ours — and the
three-month kill criterion (§6) is evaluated honestly against the
worktree-agent cohort before deeper platform investment.
