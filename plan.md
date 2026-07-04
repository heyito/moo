# moo — Technical Plan

`moo` is a single primitive: a `machine`. A hardware-isolated Linux runtime
with a copy-on-write disk, descended from a content-addressed golden image,
identified by a stable user-chosen handle. Four verbs: `new`, `run`, `save`,
`drop`. Read [vision.md](vision.md) first for the framing.

This document is the *how*: the microVM technology decision, the storage and
lifecycle model, the CLI, and the phased roadmap for shipping the primitive
without shipping anything else.

---

## 1. Problem statement (engineering terms)

We need a single-binary CLI that:

1. Creates hardware-isolated Linux microVMs on macOS Apple Silicon and Linux
   in under a second, with no root and no daemon.
2. Gives each machine its own copy-on-write disk descended from a
   content-addressed golden image (base ref + recipe hash).
3. Runs multiple machines in parallel without collisions on ports, files, or
   OS resources.
4. Records provenance — `(base_commit, recipe_hash, parent_machine)` — at
   creation, so a machine is reproducible and inspectable.
5. Snapshots machine state on demand and associates each snapshot with the
   current git HEAD SHA of the ref the handle shadows, so
   `git checkout <sha>` + `moo new <name>` restores the exact runtime that
   existed at that commit.
6. Exposes exactly four verbs to the outside world: `new`, `run`, `save`,
   `drop`. Everything an agent or a developer might want to do composes from
   those four plus the git verbs they already have.

The hard, differentiating requirements are (1) native macOS microVMs with no
root, (2) content-addressed provenance, (5) commit-tied snapshots, and (6)
the discipline to expose nothing else.

## 2. Goals and non-goals

**Goals**
- Single binary CLI. Four verbs. One noun.
- Native macOS Apple Silicon (primary) and Linux (secondary) via the same
  hypervisor library, no per-platform code paths for the core operations.
- Sub-second CoW machine forks (APFS `clonefile`, reflink, ZFS).
- Content-addressed golden image; hash of (base ref + `moo.toml` + lockfiles).
- Machine lifecycle independent of git ref/worktree lifecycle; handles are
  user-chosen labels that may or may not shadow git refs.
- Idempotent `moo new` — existing handle returns existing machine or the
  snapshot matching the current git HEAD if one is saved.
- **Commit-associated snapshots.** `moo save` quiesces the machine,
  CoW-clones the overlay to `~/.moo/snapshots/<content_hash>`, and records
  `(handle, head_sha, snapshot_hash)` so `moo new <name>` can restore the
  exact runtime that matched a given commit.
- **Content-addressed snapshot storage** — byte-identical overlays share the
  same file on disk.
- Backend-neutral public surface — hypervisor names never appear in any
  command, config, or error a user sees.

**Non-goals (initially)**
- General container runtime. `moo` is not Docker.
- Hosted platform. Local-first; remote-compatible primitives added later.
- GUI. CLI only.
- Windows guest support. Linux guests only.
- Live memory-state migration between hosts.
- Snapshot push/pull as a first-class network protocol — v-future.
- `moo.toml` as a service graph. No `[[services]]`, `depends_on`, `health`.
  `moo.toml` records the base image reference and build recipe inputs; no
  runtime declarations.
- MCP server as the primary surface. A thin `new`/`run`/`save`/`drop` MCP
  adapter can ship in M2; the primary and only stable surface in v1 is the
  CLI.
- Reverse proxy or `*.moo.test` DNS. Machines expose ports on localhost;
  routing is the caller's responsibility.
- Wrapping any git verb. `moo` never runs `git worktree add`, `git switch`,
  `git commit`, or `git branch`. Git and moo are orthogonal by default.
  Users who want automatic coupling opt in via `moo hook install` (§8.1),
  which places fail-silent hook scripts in `.git/hooks/` that invoke `moo`
  on git checkpoints — reversible with `moo hook uninstall`. `moo` never
  installs hooks itself.
- Attempt ledgers, egress policies, and secret injection as v1 product
  surfaces. Isolation is enforced by the microVM boundary; policy is a v2+
  concern.
- Agent-specific verbs (`--agent claude`, `moo try spawn`, etc.). Agents
  compose the four verbs and their own git verbs.

## 3. MicroVM technology decision

### 3.1 Hard constraint: the host is macOS Apple Silicon

Firecracker and Cloud Hypervisor require KVM and only run on Linux. Apple
Silicon has no KVM; the only hypervisor interface is Apple's
**Hypervisor.framework (HVF)**. Nested virtualization on M3/M4 exists but is
fragile.

Native macOS options that speak HVF:
- **libkrun** — a library VMM (Red Hat) that uses **KVM on Linux and
  Hypervisor.framework on macOS**. Same code path, both platforms.
- **QEMU** — cross-platform, HVF-accelerated, mature snapshots, heavy boot.
- **Apple VZ** — macOS-only, native, best memory-snapshot story on macOS.

### 3.2 Comparison

| VMM              | macOS (Apple Silicon) | Linux     | Boot    | Per-VM kernel | Footprint |
|------------------|-----------------------|-----------|---------|---------------|-----------|
| **libkrun**      | yes (HVF)             | yes (KVM) | <200 ms | yes           | tiny      |
| Firecracker      | no                    | yes (KVM) | ~125 ms | yes           | tiny      |
| Cloud Hypervisor | no                    | yes (KVM) | fast    | yes           | small     |
| QEMU (microvm)   | yes (HVF)             | yes (KVM) | ~1 s    | yes           | medium    |
| Apple VZ         | yes                   | no        | fast    | yes           | medium    |

### 3.3 Decision

**Adopt libkrun as the default hypervisor on both macOS and Linux.**

It is the only mature open-source library VMM with the same code path on
macOS Apple Silicon (HVF) and Linux (KVM). It boots in ~100–200 ms per-VM
with real hardware isolation, requires no root and no daemon, and is proven
in this problem space by Microsandbox, SmolVM, podman `krunkit`, and NVIDIA
OpenShell.

**Bind libkrun directly via its upstream C ABI, pinned to `stable-1.19.x`,
dynamically linked (LGPL-2.1).** We do not build on Microsandbox and do not
adopt its `msb_krun` Rust fork — both would stack our abstraction on
another team's abstraction, and the storage/lifecycle primitives that make
`moo` unique need direct control of the C ABI (block-device attach, sync
mode, CoW clone timing). Microsandbox is a reference implementation we
learn from — specifically its **OCI → EROFS + VMDK + ext4-overlay rootfs
conversion** — but we do not depend on it at runtime.

**We build the features Microsandbox already ships (MCP, snapshots, SDKs)
only if and when the primitive itself demands them.** Save via CoW clone
demands them now (§5.3, §6.3). Backend swap to Firecracker/CH on Linux or
Apple VZ on macOS is a v2+ concern for memory snapshots, gated behind an
internal driver seam that is designed early and extracted late (§4.3).

**The hypervisor never leaks into the public surface.** Users see the noun
`machine` and four verbs. `libkrun`, `krunfw`, `HVF`, and any driver name
are internal.

### 3.4 Validated technical assumptions (libkrun `stable-1.19.x`)

Verified against the public C header and the krunkit/podman-machine
consumers:

- **virtio-blk on macOS/HVF — confirmed.** `krun_add_disk2(ctx, block_id,
  path, format, read_only)` attaches raw + qcow2 data disks. `krun_add_disk3`
  adds `direct_io` + sync mode. `krun_set_root_disk`/`krun_set_data_disk`
  are deprecated in 1.x and removed in 2.0; target `krun_add_disk2/3`.
  Verify build flags at runtime with `krun_has_feature(KRUN_FEATURE_BLK)`.
- **virtio-fs on macOS — confirmed** via `krun_add_virtiofs(ctx, tag, path)`.
  Root uses the reserved tag `/dev/root`. Not used in v1 (Model B default).
- **Host↔guest exec channel — nuanced.** vsock works via
  `krun_add_vsock_port(...)`, but on macOS is bridged to a host UNIX socket,
  not real `AF_VSOCK`. Microsandbox chose virtio-serial for its guest agent
  on macOS. Bake-off in M0.
- **Root-free port forwarding — confirmed.** libkrun's built-in **TSI**
  (Transparent Socket Impersonation) gives outbound connectivity and
  host-reachable guest ports via `krun_set_port_map(["host:guest", …])` with
  no helper process and no root. TSI is TCP/UDP AF_INET/INET6 only (no
  raw/ICMP), no inbound UDP-listen from the guest, and requires the custom
  `libkrunfw` kernel.
- **Guest-triggered quiesce (for save) — confirmed.** The guest exec agent
  can run `sync`, DB-specific checkpoint commands (e.g. Postgres `CHECKPOINT`,
  SQLite `PRAGMA wal_checkpoint(TRUNCATE)`), and signal completion; the host
  then runs `F_FULLFSYNC` on the overlay file before `clonefile`. This is
  the same ordering the CoW-fork path uses (§5.1).
- **Memory snapshot/restore — does not exist** in shipping libkrun (neither
  `stable-1.19.x` nor 2.0 `main`). Unmerged, design-contested prototype.
  Not roadmapped. Save is a **disk-state snapshot**, not a memory snapshot;
  the machine is quiesced before save, not paused-in-flight.
- **macOS packaging.** Any HVF caller must be codesigned with
  `com.apple.security.hypervisor` (ad-hoc OK, no paid cert) plus
  `com.apple.security.cs.disable-library-validation` to load Homebrew
  `libkrun.dylib`. No root/sudo needed to run VMs.
- **Guest kernel.** libkrun boots a firmware bundle, **`libkrunfw`** — a
  separate build artifact we vendor/ship. TSI networking depends on it.

## 4. Architecture

```
┌──────────────────────────────────────────────┐
│  moo CLI  (new / run / save / drop / doctor)  │
├──────────────────────────────────────────────┤
│  moo-core  (registry, provenance, lifecycle)  │
├──────────────────────────────────────────────┤
│  moo-vmm   (libkrun FFI, exec transport)      │
│  moo-store (SQLite registry, CoW clone,       │
│             content-addressed snapshots)      │
└──────────────────────────────────────────────┘
                │ virtio (blk, net, vsock)
┌───────────────▼──────────────────────────────┐
│  Guest microVM per machine                    │
│  • Content-addressed golden rootfs (RO)       │
│  • Per-machine CoW overlay (RW)               │
│  • Small static exec + quiesce agent          │
└──────────────────────────────────────────────┘
```

### 4.1 No daemon

`moo` is a single binary. No `mood`. No system service. Each invocation
opens the SQLite registry, does the requested operation, and exits.
Long-running guest VMs are owned by launchd / systemd user services or a
detached child process, not by a `moo` daemon.

### 4.2 Registry

`~/.moo/registry.db` (SQLite) records two tables.

**Machines** (live runtime handles):

```
machines(handle, base_commit, recipe_hash, parent_machine,
         base_image_path, overlay_path, lifecycle, created_at)
```

- `handle` — user-chosen label. Ref-shaped by default. Unique per project.
- `lifecycle` — `sealed` (quiesced, fork-safe O(1)) or `live` (mutating).
- `parent_machine` — the machine this was CoW-forked from, or null for root
  machines forked directly from a base image.

**Snapshots** (immutable saved states, indexed by handle + git SHA):

```
snapshots(snapshot_id, handle, head_sha, snapshot_path,
          content_hash, saved_at)
```

- `snapshot_id` — short unique ID (e.g. `s_a1f3`).
- `handle` — the machine handle at save time.
- `head_sha` — the git HEAD of the ref the handle shadowed at save time.
  Null if the handle was `--detached`.
- `snapshot_path` — points to `~/.moo/snapshots/<content_hash>`.
- `content_hash` — hash of overlay bytes. Two snapshots with identical
  content share the file (dedup).

There is no `Machine` type in the public surface. There is only the handle
and, optionally, saved snapshots keyed by SHA. `sealed`/`live` and
`named`/`--detached` are metadata bits, not separate nouns. A "branch
machine" is a machine with a named handle held against a git ref. An
"attempt machine" is a machine with `--detached` and a short lease. Same
noun.

### 4.3 Internal driver seam (not public)

Backend abstraction lives in the `moo-vmm` crate as a trait with exactly
one implementation in v1 (libkrun). The trait exists so that adding
Firecracker / Cloud Hypervisor / Apple VZ in v2+ (for memory snapshots) is
a compile-time swap, not a rewrite. The trait is internal — it does not
appear in the CLI, in `moo.toml`, or in any user-visible error message.

## 5. Storage & state

Three artifacts per project:

1. **Golden base image** — content-addressed and read-only. Content hash =
   `hash(base_image_ref + moo.toml + lockfiles)`. Built once per hash;
   cached under `~/.moo/images/<hash>`. If two projects share a hash, they
   share the base.
2. **Per-machine CoW overlay** — writable. Holds everything that mutates:
   DB data directories, caches, installed packages, uploads, artifacts.
3. **Immutable snapshots** — CoW clones of an overlay at save time, keyed
   by content hash under `~/.moo/snapshots/<content_hash>`, indexed in the
   registry by `(handle, head_sha)`.

Provenance is recorded in the registry at creation:
`(base_commit, recipe_hash, parent_machine)`. This is what makes a machine
reproducible and inspectable without a separate "ledger" product.

### 5.1 Copy-on-write per platform

- **macOS:** APFS `clonefile()` gives instant CoW clones of the overlay
  file. Forking a machine — or saving a snapshot — is one `clonefile` call.
  Zero-copy until write.
- **Linux:** `reflink` (btrfs, XFS), ZFS clones, or qcow2 backing files as
  a universal fallback.

**Clone ordering (correctness).** Any CoW clone (both `moo new … from
<name>` and `moo save <name>`) must run **quiesce → host-flush
(`F_FULLFSYNC`) → `clonefile`**, in that order. Cloning from a `sealed`
machine skips the quiesce step (already quiesced). Cloning from `live`
requires an in-guest sync + host flush first, or fails with a clear error.

**Durability boundary.** libkrun's `KRUN_SYNC_RELAXED` means a guest
`fsync` reaches the host page cache but not the physical drive. Contract
is "survives switch," not "survives host power-loss." A per-machine
`sync_mode=full` opt-in exists for volumes that need it. `moo save` uses
full sync regardless of the machine's default mode — snapshots must
survive power loss even if the live overlay does not.

### 5.2 Model B is the v1 default

The working tree lives inside the machine's overlay. Code, deps, and DB
state — one CoW disk. No virtio-fs host-share. This is the only model
that:
- preserves working-tree CoW when you fan out six machines on a 2 GB
  monorepo (the failure mode of wrapping `git worktree add`);
- keeps machine-level isolation for autonomous agents;
- lets you fork or save a machine sub-second regardless of tree size.

Model A (shared host source via virtio-fs with per-ecosystem overlay
recipes for `node_modules`, `.venv`, build caches) is deferred to M2. It is
an opt-in adapter for humans who want editor visibility on the host, not
the default.

### 5.3 Snapshots (`moo save`)

A snapshot is an immutable CoW clone of a machine's overlay, saved to
`~/.moo/snapshots/<content_hash>` and recorded in the `snapshots` table
with `(handle, head_sha, snapshot_hash, saved_at)`.

- **Association.** `head_sha` is the current git HEAD of the ref the
  handle shadows at save time. Null if the handle is `--detached`.
- **Content addressing.** Two saves that produce byte-identical overlays
  share the same file on disk. Most commits move little runtime state, so
  dedup is effective in practice.
- **Idempotence.** If the latest snapshot for `(handle, head_sha)` already
  matches the current content hash, `moo save` is a no-op and returns the
  existing snapshot ID.
- **Snapshot-aware `moo new`.** On an existing handle, `moo new <name>`
  first looks up `snapshots WHERE handle = <name> AND head_sha =
  git_current_head(shadowed_ref)`. If found, restore from that snapshot
  (the snapshot is CoW-cloned into a new live overlay). If not found, boot
  the current live overlay.
- **Storage GC.** V1 keeps all named-handle snapshots. Content-addressing
  and CoW keep the marginal cost low. A retention policy (last N per
  handle, or expire-after-M-days for detached handles) is a v2 concern
  driven by real usage data.

## 6. Lifecycle

The four verbs. Nothing else.

### 6.1 `moo new <name> [from <src>] [--detached]`

```
new(name, source):
  if not name and not --detached: error
  if name in registry:                          # idempotent
      shadowed_ref = git_ref_for(name)
      snap = latest_snapshot(name, shadowed_ref.head_sha)
      if snap:
          overlay = cow_clone(snap.snapshot_path) # restore from saved state
      else:
          return handle(name)                    # existing live overlay
  base = resolve(source)                        # ref | commit | snapshot | machine
  provenance = (base_commit, recipe_hash, parent_machine)
  overlay = cow_clone(base_overlay)             # quiesce → flush → clonefile
  handle = new_handle(name or auto_id, provenance, overlay, lifecycle=live)
  registry.write(handle)
  vmm.start(handle)                             # libkrun boot, TSI ports assigned
  return handle
```

- `<src>` may be a ref (`feat/x`), a commit SHA, a snapshot ID (`s_a1f3`),
  an existing machine handle, or `HEAD`. Default is the current project's
  default base image plus `HEAD`.
- **Snapshot restore.** If `<src>` is a commit SHA and a snapshot exists
  for `(handle_being_created_or_shadowed_ref, commit_sha)`, restore from
  it. This is what makes `git checkout <old-sha>` + `moo new feat/x` work.
- **Naming.** `feat/x` **shadows** the ref `refs/heads/feat/x` — attaches
  the handle to whatever HEAD points at now, records `base_commit` at that
  SHA, but does not create or modify any git ref.
- `--detached` yields an auto-generated handle (`m_a1f3…`), no ref
  shadowing, short auto-GC lease.

### 6.2 `moo run <name> -- <cmd>`

Executes `<cmd>` inside the machine's guest. Captures stdout, stderr, and
exit code. Long-running services persist between invocations (`docker exec`
semantics). Interactive TTY via `moo run --tty <name> -- $SHELL` in M2.
This one verb subsumes `exec`, `ssh`, `logs`, and `doctor`. Idempotent
from the caller's point of view: each invocation is a fresh process
against the same machine.

### 6.3 `moo save [<name>]`

```
save(name):
  machine = registry.get(name)                   # or all live machines if none
  agent.quiesce(machine)                         # guest sync + DB checkpoint
  host.fullfsync(machine.overlay_path)           # F_FULLFSYNC
  content_hash = hash_file(machine.overlay_path)
  head_sha = git_current_head(machine.shadowed_ref) or null
  existing = registry.find_snapshot(name, head_sha)
  if existing and existing.content_hash == content_hash:
      return existing.snapshot_id                # no-op, dedup
  snapshot_path = cow_clone(machine.overlay_path,
                            f"~/.moo/snapshots/{content_hash}")
  snapshot_id = registry.write_snapshot(name, head_sha,
                                        snapshot_path, content_hash)
  return snapshot_id
```

- **Quiesce.** The guest exec agent runs `sync` plus any registered
  DB-checkpoint hooks (Postgres `CHECKPOINT`, SQLite
  `PRAGMA wal_checkpoint(TRUNCATE)`, Redis `BGSAVE + LASTSAVE`) before the
  host clones. Full-sync mode is used regardless of the machine's default
  `sync_mode` — snapshots must be power-loss-durable.
- **Association.** If `<name>` shadows a git ref (default), `head_sha` is
  the current HEAD of that ref. If `<name>` is `--detached`, `head_sha`
  is null; the snapshot is retrievable only by its ID.
- **Idempotence.** If the latest snapshot for `(handle, head_sha)` has a
  matching content hash, no-op and return the existing snapshot ID.
- **No `<name>`.** Saves every live machine. Useful in a `post-commit`
  shell alias the user chooses to write themselves. `moo` does not
  install git hooks.
- **Interaction with `moo new`.** After `moo save feat/x`, if the user
  runs `git checkout <old-sha>` and then `moo new feat/x`, the machine
  reboots from the snapshot saved for `(feat/x, old-sha)`. If no such
  snapshot exists, the current live overlay is used.

This is `git commit` for the runtime.

### 6.4 `moo drop <name>`

Quiesces the machine, stops the VMM, deletes the **live overlay**,
removes the handle from the `machines` table. **Saved snapshots survive
by default** — they remain in the `snapshots` table and can be restored
by a future `moo new <name>` if the handle name is reused against a
matching git SHA.

- `--force` — kill even if the machine is live and cannot be quiesced.
- `--snapshots` — also delete all saved snapshots for this handle.
- Idempotent.

### 6.5 `moo doctor`

Diagnostic-only. Checks HVF entitlements on the binary, `libkrunfw`
presence, APFS on the store path, `clonefile` support, base image cache,
snapshot cache integrity. Modifies nothing.

## 7. Networking

- **User-mode networking per machine via TSI + `krun_set_port_map`.** No
  root, no helper process, no bridge. Each machine's listening ports are
  exposed on host localhost at a stable, project-scoped port allocated
  from a managed range.
- **Deterministic per-handle port allocation.** `moo ls` shows the map.
  Handle collision within the allocated range triggers bind-failure retry,
  not a lease crash.
- **No reverse proxy. No `*.moo.test` DNS.** Machines expose
  `localhost:<port>`. Routing between them is the caller's job. This
  removes the `/etc/resolver/*` install friction and keeps `moo doctor`
  from having to manage privileged config.

## 8. Git integration

`moo` never runs git. Git never runs `moo`. They are orthogonal.

- **Handle shadowing.** `moo new feat/x` records the resolved commit at
  creation, but does not create, modify, or delete `refs/heads/feat/x`.
- **Save.** `moo save feat/x` reads `refs/heads/feat/x` HEAD but does not
  write to it. No `post-commit` hook is installed; users who want
  automatic save write their own alias (`gitcommit() { git commit "$@" &&
  moo save; }`).
- **Promotion.** `git merge <ref>` — the machine follows the promoted ref
  because the handle shadows it, and any snapshot saved for the merge
  commit is restorable.
- **Rollback.** `moo drop <name>` for runtime + `git reset` for code;
  snapshots for old SHAs remain and can be restored.
- **Fanout.** `for i in {1..6}; do moo new --detached from HEAD; done` —
  no refs polluted.

**No default hooks. No wrappers. No `moo switch`. No `moo worktree`.** If a
user runs `git worktree add`, they created a worktree; they can `moo new`
against it or not. `moo` does not know or care.

### 8.1 Opt-in hooks (`moo hook install`)

Users who want git-triggered auto-follow install hooks via a single admin
command. This is explicit user consent, not orthogonality violation — moo
itself never runs git and never installs hooks; git only invokes moo via
scripts the user chose to place.

**Admin commands:**

- **`moo hook install [--force] [--append]`** — writes hook scripts into
  `.git/hooks/`. Refuses to overwrite non-moo hooks unless `--force`; use
  `--append` to preserve existing script content and add moo-owned lines
  below a sentinel comment (removable by `moo hook uninstall`). Detects
  `husky` / `pre-commit` / `lefthook` hook-manager setups and prints
  integration instructions instead of installing directly.
- **`moo hook uninstall`** — removes moo-owned hook content. Preserves any
  non-moo content added via `--append` mode.
- **`moo hook status`** — prints which hooks are installed, which are
  missing, and which conflict with other tools.

**Hooks installed:**

- **`post-checkout`** — after `git checkout <branch>`, `git switch`, or
  `git checkout -b`, runs `moo save <outgoing-branch>` (if that machine
  exists), then `moo new <incoming-branch>` (which restores the snapshot
  matching the incoming HEAD or boots the current live overlay).
- **`post-commit`** — after `git commit`, runs `moo save <current-branch>`,
  tagging the snapshot with the new HEAD SHA. This is what makes
  `git checkout <old-sha>` + `moo new <name>` restore the exact runtime
  that matched that commit.
- **`post-merge`** — after `git merge` or a merge-based `git pull`, runs
  `moo save <current-branch>` followed by `moo new <current-branch>` to
  refresh runtime against the merged state.
- **`post-worktree`** (git 2.44+ only) — after `git worktree add`, runs
  `moo new <ref>` in the new worktree's directory.

**Semantics:**

- All hook actions are **fail-silent**: `moo` errors never break `git`.
- Every hook invocation appends to `~/.moo/hooks.log` for debugging.
- Detached HEAD, missing machines, and machines whose handle does not
  match any branch are all no-ops.
- The primitive is complete without hooks. Installation is an explicit
  user action; `moo init` and `moo new` do not touch git hooks.

**Known gaps (documented, not fixed):**

- `git reset --hard`, `git rebase`, and `git cherry-pick` move HEAD
  without firing `post-checkout`. Snapshot-per-commit time-travel still
  works for previously-saved SHAs, but the live overlay does not
  auto-refresh. Users who need this run `moo new <branch>` manually.
- Squash-merge and fast-forward pull edge cases in `post-merge` may
  double-save. Idempotent snapshot dedup makes this cheap in practice.

## 9. Configuration: `moo.toml`

Optional. Committed to the repo if used. Records the base image reference
and the recipe inputs whose hash becomes the golden-image identity. Nothing
else.

```toml
[project]
name = "acme"
base = "debian:12"          # OCI reference or Dockerfile path

[recipe]
lockfiles = ["package-lock.json", "poetry.lock"]

[resources]
cpus = 2
memory = "4GiB"

[quiesce]                   # optional: extra commands the guest agent runs
                            # during `moo save`, before host flush
commands = [
  "pg_ctl -D /var/lib/postgresql/16/main checkpoint",
  "redis-cli BGSAVE",
]
```

No `[[services]]`. No `depends_on`. No `health`. No `[[volumes]]`. No
`[snapshot]` config beyond `[quiesce]`. The recipe hash is
`hash(base + recipe.lockfiles content + resource block)`. If two developers
have the same `moo.toml` and the same lockfile contents, they get the same
golden image, byte-for-byte.

## 10. CLI surface

Five commands (four verbs plus `doctor`; `ls` is a read-only listing).

Primitive verbs (compose):

```
moo new <name> [from <src>] [--detached]      # create machine (restore snapshot if available)
moo run <name> -- <cmd>                       # exec inside machine
moo save [<name>]                             # snapshot state, tag with current HEAD SHA
moo drop <name> [--force] [--snapshots]       # destroy machine (snapshots survive by default)
```

Admin (do not compose; parallel to `docker system prune`, `git config`):

```
moo doctor                                    # diagnostic check
moo ls                                        # list handles and their snapshots
moo hook install [--force] [--append]         # opt-in: install git hooks (§8.1)
moo hook uninstall                            # remove moo-owned hook content
moo hook status                               # show hook installation state
```

`moo ls`, `moo doctor`, and `moo hook *` are administrative — they do not
compose with the primitive verbs and do not count against the four-verb
ceiling. Every user-facing operation on machine state goes through `new`,
`run`, `save`, `drop`, and git.

## 11. Technology stack

- **Language.** Rust. Cargo workspace: `crates/moo-cli`, `crates/moo-core`,
  `crates/moo-vmm`, `crates/moo-store`.
- **libkrun binding.** FFI to the upstream C ABI, pinned to `stable-1.19.x`.
  Disk via `krun_add_disk2/3`. Runtime feature check via
  `krun_has_feature`. No `msb_krun` fork.
- **Guest kernel.** Vendor and ship `libkrunfw`.
- **macOS packaging.** Binary codesigned with `com.apple.security.hypervisor`
  + `com.apple.security.cs.disable-library-validation`. Dynamically links
  `libkrun.dylib` (LGPL-2.1). No root.
- **Guest exec + quiesce agent.** Small static Rust binary baked into the
  golden image. Handles both command execution (for `moo run`) and quiesce
  orchestration (for `moo save`). Transport (vsock vs virtio-serial)
  decided in M0.
- **Registry.** SQLite via `rusqlite`. Two tables: `machines`, `snapshots`.
- **Snapshot store.** Content-addressed directory
  `~/.moo/snapshots/<content_hash>`. CoW clones via `clonefile`/reflink.
- **Image build.** Accept an OCI reference or a Dockerfile; convert to a
  bootable rootfs. Port Microsandbox's EROFS + VMDK + ext4-overlay design.
  Build orchestration decision (external OCI builder vs a `moo`-owned build
  microVM) is an M0 fork.

## 12. Phased roadmap

### M0 — Spike (4–6 wk)

Prove the primitive is buildable, not build it.

- Direct libkrun FFI boot of a Debian rootfs on macOS Apple Silicon and
  Linux. Codesign the spike binary. Confirm unprivileged run.
- CoW clone timing: `clonefile` on a 2 GB and a 20 GB overlay image.
  Target: sub-500 ms.
- Agent transport bake-off: vsock (UNIX-socket proxy) vs virtio-serial for
  the exec + quiesce channel.
- OCI → bootable rootfs conversion decision: port Microsandbox's design vs
  shell to an external builder.
- End-to-end `moo new HEAD --detached` on a codesigned macOS binary,
  unprivileged, in one command.
- **Snapshot spike:** validate quiesce → `F_FULLFSYNC` → `clonefile` +
  content-hash storage produces a byte-stable, restorable overlay.

**Definition of done:** a throwaway binary that creates a machine from a
real image, runs a command inside it, saves a snapshot associated with a
git SHA, restores that snapshot into a new machine, and drops both — with
provenance recorded and zero `libkrun`/`HVF`/`krunfw` strings in stdout,
stderr, or usage output.

### M1 — Ship the primitive (macOS Apple Silicon only)

`moo new` / `run` / `save` / `drop` / `doctor`. Model B only. libkrun only.

- SQLite registry schema per §4.2 (both tables).
- Content-addressed image cache.
- Provenance recording at `new`.
- Idempotent `new` with snapshot restore for current HEAD SHA.
- Handle shadowing of git refs (no ref modification).
- CoW clone with quiesce → flush → clonefile ordering.
- **`moo save` with content-addressed snapshot storage, dedup, and
  idempotence.** Guest quiesce agent supports `sync`, Postgres checkpoint,
  SQLite WAL checkpoint, custom `[quiesce] commands` from `moo.toml`.
- **`moo hook install/uninstall/status`** — opt-in `post-checkout`,
  `post-commit`, `post-merge`, and `post-worktree` (git 2.44+) hooks
  that auto-save the outgoing branch and auto-restore the incoming
  branch. Fail-silent; reversible; detects `husky`/`pre-commit`/`lefthook`
  and prints integration instructions instead of clobbering.
- TSI port allocation with bind-failure retry.
- `moo doctor` diagnostic-only.

**Not in M1:** MCP server, reverse proxy, `*.moo.test` DNS, `moo.toml`
service graph, attempt ledger, egress policy, secrets injection, `--tty`,
Model A, Linux, snapshot push/pull, snapshot GC policy.

**Delivers:** a developer or agent can create a machine, run migrations
and services inside it, `moo save` at commit boundaries, fork it
CoW-cheap, restore old runtimes via `git checkout` + `moo new`, and drop
it — using only four verbs and existing git.

### M2 — Adopters

- Thin MCP adapter exposing `new`/`run`/`save`/`drop` as MCP tools.
- Linux (libkrun + KVM).
- `moo run --tty` for interactive sessions.
- Model A: virtio-fs host-share with per-ecosystem overlay recipes for
  `node_modules`, `.venv`, build caches. Opt-in per project.
- Snapshot GC policy (last-N-per-handle, expire-after-M-days for
  detached).

### M3+ — Post-PMF possibilities (appendix)

- Snapshot push/pull as a first-class network protocol — share runtime
  state alongside `git push`. Requires content-addressed transport,
  chunked upload, and a receiving daemon on the remote.
- Memory snapshots via alternate drivers (Apple VZ on macOS, Cloud
  Hypervisor / Firecracker on Linux). Not cross-host portable.
- Remote hosts: same primitive, remote registry.
- Egress policy proxy for untrusted agents.

## 13. Risks & mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Feature creep back to a service graph / MCP-first / ledger | The primitive dies before it ships | The hyperplan MUST-NOT list is the design gate; every added feature must decompose into the four verbs or be rejected |
| libkrun ABI churn | Rebuild breakage | Pin `stable-1.19.x`; runtime `krun_has_feature`; do not depend on 2.0 until it stabilizes |
| macOS codesigning / entitlement friction | Install-time bounce | `moo doctor` catches missing entitlements; Homebrew formula runs the ad-hoc codesign post-install |
| Model A composition problem | Silently broken headline promise | Model B is v1 default; Model A blocked until per-ecosystem overlay recipes exist and are tested |
| Snapshot storage bloat | Disk fills | Content-addressed dedup keeps marginal cost low; v2 adds retention policy; `moo drop --snapshots` is the manual knob |
| Snapshot restore vs live overlay confusion | Users lose recent unsaved work when `moo new` restores an older snapshot | Restore semantics documented sharply: `moo new` on existing handle prefers snapshot-for-current-HEAD; users are told to `moo save` before switching commits |
| macOS relaxed fsync on live overlay | Committed writes lost on power-loss | Contract = "survives switch"; `moo save` uses full sync regardless; opt-in `sync_mode=full` per volume for the live overlay |
| Backend leak into public surface | Loses "backend-neutral" invariant | CI check: grep of CLI stdout/stderr/usage for `libkrun`/`HVF`/`krunfw` returns 0 |
| libkrun has no memory snapshots | No instant live-resume in v1 | Save is a **disk-state** snapshot with quiesce; memory-state snapshots come from alternate drivers in M3+ |

## 14. Open questions (M0 forks only)

- Agent exec + quiesce transport: vsock UNIX-socket proxy vs virtio-serial
  on macOS. Decide in M0 bake-off.
- OCI → bootable rootfs conversion: port Microsandbox's EROFS + VMDK +
  ext4-overlay design, or shell to an external OCI builder for v1.
- Snapshot content-hash algorithm: BLAKE3 for speed vs SHA-256 for
  ubiquity. Decide in M0 based on measured overhead on a 20 GB overlay.
- Base image cache eviction policy. Content-hash addressed but disk fills.
  Simple LRU vs explicit `moo image prune`.

## 15. Summary of decisions

1. **Primitive.** One noun (`machine`), four verbs (`new`, `run`, `save`,
   `drop`). Nothing else in the public surface. `doctor` and `ls` are
   diagnostic conveniences, not part of the primitive.
2. **Hypervisor.** libkrun on both macOS (HVF) and Linux (KVM) for code-path
   parity. Direct C ABI FFI, `stable-1.19.x`, dynamically linked. No
   `msb_krun` fork. No Microsandbox at runtime. Backend never leaks into
   the public surface.
3. **State model.** Content-addressed golden image + per-machine CoW
   overlay + content-addressed immutable snapshots keyed by
   `(handle, head_sha)`. Provenance
   `(base_commit, recipe_hash, parent_machine)` recorded at `new`. Quiesce
   → host-flush → clonefile ordering for both fork and save. Live-overlay
   durability contract is "survives switch"; snapshot durability contract
   is "survives power loss."
4. **Storage default.** Model B (code inside machine overlay) in v1. Model
   A (virtio-fs host-share) deferred to M2 with per-ecosystem overlay
   recipes.
5. **Git relationship.** Orthogonal by default. `moo` never runs git; git
   never runs `moo` unless the user opts into `moo hook install`, which
   places fail-silent hook scripts in `.git/hooks/` for auto-follow on
   checkout, commit, merge, and worktree events. Handles shadow refs
   without modifying them. Promotion is `git merge`.
6. **No daemon.** `moo` is a single binary. Registry is SQLite.
7. **Networking.** TSI + `krun_set_port_map`. No reverse proxy. No DNS.
   Ports on localhost.
8. **Configuration.** `moo.toml` records base image ref + recipe inputs +
   optional `[quiesce]` commands. No services, no health checks, no
   volumes, no snapshot config. Recipe hash is the golden-image identity.
9. **Verb-count discipline.** Four *primitive* verbs (`new`/`run`/`save`/
   `drop`) is the ceiling. The hyperplan bundle rejected orchestration
   verbs (`spawn`/`promote`/`discard`); save was added because it is a
   *state* verb parallel to `git commit`. `doctor`, `ls`, and `hook
   install/uninstall/status` are *administrative* — they do not compose
   with the primitive verbs and do not count against the ceiling. Any
   future feature request that requires a fifth primitive verb triggers a
   design review, not an implementation.
