# got — Technical Plan

This document covers the microVM technology decision and the architecture/roadmap for
building `got`: a tool that pairs every git branch/worktree with a persistent,
snapshottable microVM so that application runtime state travels with the ref.

Read `vision.md` first for the product framing. This document is the *how*.

---

## 1. Problem statement (engineering terms)

We need a system where:

1. Each git ref (branch) and/or worktree maps to an isolated Linux runtime with its
   own kernel, filesystem, processes, and network namespace.
2. That runtime's **disk and process state persists** across branch switches and is
   **restored** when the ref is revisited.
3. Creating a branch **copy-on-write clones** the parent runtime (instant, cheap).
4. Multiple runtimes can be **active simultaneously** without port, database, or
   filesystem collisions (parallel agents / worktrees).
5. It runs **natively on macOS Apple Silicon** (primary dev target here) and on
   **Linux** (CI, remote, Linux devs).
6. A clean environment is always reproducible from a declared golden image.

The hard, differentiating requirements are (2) state persistence + (5) native macOS.

## 2. Goals and non-goals

**Goals**
- Local-first, single-binary CLI with git-like verbs.
- Per-branch microVM lifecycle: create, start, pause, snapshot, resume, fork, destroy.
- Copy-on-write disk model for instant branch forking and cheap storage.
- Declarative environment definition (`got.toml`) → reproducible golden image.
- Automatic port/hostname allocation so parallel VMs don't collide.
- Pluggable VMM driver so we're not locked to one hypervisor.

**Non-goals (initially)**
- Being a general container runtime or a Kubernetes replacement.
- A hosted/managed cloud service (the primitives allow it later; not v1).
- GUI. CLI + optional TUI first.
- Windows guest support (Linux guests only).
- Live migration between hosts (v-future).

## 3. MicroVM technology decision

### 3.1 Hard constraint: the host is macOS Apple Silicon

The single most important fact: **Firecracker and Cloud Hypervisor require KVM and only
run on Linux.** They cannot run natively on macOS. Apple Silicon has no KVM; the only
hypervisor interface is Apple's **Hypervisor.framework (HVF)**. Nested virtualization
exists on M3/M4 but is fragile and not a foundation to bet the core UX on.

So the *native macOS* open-source options narrow to VMMs that speak HVF:

- **libkrun** — a library VMM (Red Hat) that uses **KVM on Linux and
  Hypervisor.framework on macOS**. Same code path, both platforms. This is the key
  differentiator called out repeatedly in the sandbox landscape.
- **QEMU** — cross-platform, HVF-accelerated on macOS, mature `savevm`/`loadvm`
  snapshots, but heavier and slower to boot; a `microvm` machine type exists.
- **Apple Virtualization.framework (VZ)** — macOS-only, fast, and uniquely supports
  **full memory save/restore** (`saveMachineStateTo`). Great snapshotting, but not
  portable to Linux.

### 3.2 Comparison

| VMM | macOS (Apple Silicon) | Linux | Boot | Per-VM kernel | Mem snapshot/restore | Footprint | Notes |
|---|---|---|---|---|---|---|---|
| **libkrun** | ✅ (HVF) | ✅ (KVM) | <100–200ms | ✅ | ❌ none today (unmerged prototype, see §3.4) | tiny | Only mature cross-platform library VMM; powers Microsandbox, SmolVM, podman `krunkit`, OpenShell |
| **Firecracker** | ❌ | ✅ (KVM) | ~125ms | ✅ | ✅ excellent | tiny | Best-in-class snapshots, but Linux-only; great as a *Linux/remote* driver |
| **Cloud Hypervisor** | ❌ | ✅ (KVM) | fast | ✅ | ✅ | small | More device support than Firecracker; Linux-only |
| **QEMU (microvm)** | ✅ (HVF) | ✅ (KVM) | ~1s | ✅ | ✅ mature | medium | Portable fallback; heavier, slower boot |
| **Apple VZ** | ✅ | ❌ | fast | ✅ | ✅ (state save) | medium | macOS-only; best memory-snapshot story *on macOS* |

### 3.3 Decision

**Adopt `libkrun` as the default cross-platform microVM backend, behind a pluggable
`got` VMM driver interface.**

Rationale:
- It is the **only mature open-source VMM that runs the same way on macOS Apple Silicon
  and Linux**, which is exactly our host matrix. This removes the biggest risk (native
  macOS support) with a single dependency.
- It has a proven track record in precisely this problem space: **Microsandbox**
  (Apache-2.0, libkrun, <100ms boot, no daemon/no root), **SmolVM**, podman's
  `krunkit`, and NVIDIA's **OpenShell** all build on it.
- It boots in ~100–200ms with a per-VM kernel → real hardware isolation, cheap enough
  to run one per branch.
- Apache-2.0/LGPL licensing is compatible with an open-source product.

**Driver abstraction is warranted for two reasons** — but we *design* the seam early and
*abstract* it late (see §12): (1) libkrun's weak spot is **live memory snapshotting**
(pause a VM with running processes, persist RAM+CPU state, resume later), which is
**absent from every shipping libkrun today** (§3.4); and (2) libkrun's default macOS
networking (**TSI**, §3.4/§7) is fast and root-free but has real limits (no raw/ICMP
sockets, no inbound UDP-listen from the guest), so apps needing faithful networking
eventually need a `gvproxy`/`vmnet` NetDriver. The engines we can slot in per
platform/phase:

- **macOS default:** `libkrun` (footprint, boot, cross-platform parity).
- **Linux default:** **also `libkrun` (on KVM)** — it already works on Linux and gives
  us macOS/Linux code-path parity, our #1 differentiator. Do **not** default to CH/FC on
  Linux; they add jailer/tap/root ops burden for zero M1 benefit.
- **macOS memory-snapshot (Phase 3):** `Apple VZ` driver as an alternate for
  full-state save/restore of live processes (note the Model-B coupling in §12/M3).
- **Linux memory-snapshot / remote / CI (Phase 3+):** `Cloud Hypervisor` or
  `Firecracker` for best-in-class snapshot + jailer isolation.
- **Portable fallback:** `QEMU`.

**Build-vs-reuse — decided: bind `libkrun` directly.** We do **not** write a
hypervisor, and we do **not** build on Microsandbox. The primary reason is **layering**,
not "ephemerality": building on Microsandbox would stack `got`'s `VmmDriver` abstraction
on top of Microsandbox's own VM abstraction *and* its `msb_krun` Rust fork of libkrun — a
double indirection that breaks the moment we add the M3 memory-snapshot drivers (Cloud
Hypervisor/Firecracker on Linux, Apple VZ on macOS) that Microsandbox has no notion of.
(To be accurate: Microsandbox is **not** purely ephemeral — it ships persistent named
virtio-blk volumes, snapshots, and stop/start state. But its model is *single-writer per
block device* with fork-by-snapshot/copy, whereas `got` needs *live CoW branch forks*
via `clonefile`/reflink — a different storage thesis.) The features Microsandbox would
give us (MCP, agent SDKs, no-daemon) are mostly ones we don't need or want to own.

We therefore bind **libkrun directly via its upstream C ABI** (FFI from Rust), pinned to
the **`stable-1.19.x`** release branch (the version Homebrew/krunkit ship; `main` is
libkrun 2.0 and is explicitly ABI-unstable). This gives us low-level control over block
devices and snapshot semantics that is the actual product surface.

> **Correction / clarification:** Microsandbox does **not** call libkrun through its C
> ABI. It consumes a **native-Rust fork of libkrun** (the `msb_krun*` crates from
> `zerocore-ai/libkrun`) and drives it with `VmBuilder`/`Vm::enter()`. So Microsandbox
> proves the *fork-to-Rust* path, **not** the C-ABI-FFI path. We deliberately **do not**
> adopt `msb_krun`: depending on a third-party fork of libkrun is the same "double-stack
> another team's abstraction" trap we reject for Microsandbox itself, and it forfeits
> upstream fixes. The real proof the C ABI is FFI-friendly is its C consumers
> (krunvm/crun), plus §3.4's spike.

Microsandbox (Apache-2.0) is still kept as a **reference implementation** to mine for
two things: its **VM launch/lifecycle logic** (translated from `msb_krun` calls to the
equivalent C-ABI calls) and its **OCI-image → bootable-rootfs conversion** — which is a
concrete, non-trivial design we port/vendor: per-layer **EROFS** (read-only) stitched
into one read-only virtio-blk via a **VMDK flat descriptor**, plus a writable **ext4
"upper"** virtio-blk, assembled with **overlayfs inside the guest** (libkrun injects its
own init even for block roots). This is more than "learn from"; budget for it in M0.
Licensing: libkrun is LGPL-2.1, so we **dynamically link** it (`libkrun.dylib` on macOS)
to keep `got`'s own license unconstrained.

> **Key insight that de-risks v1:** the headline use case ("my migration stays with the
> branch") is a **disk-state** problem, not a memory-state problem. Database data,
> migrations, installed deps, and artifacts all live on a **persistent block device**.
> We can deliver the core value with libkrun + copy-on-write disks *without* memory
> snapshots. Live-process resume (memory snapshots) is a Phase-3 enhancement, not a
> prerequisite.

### 3.4 Validated technical assumptions (libkrun `stable-1.19.x`, verified against the C API + krunkit)

The v1 thesis depends on libkrun capabilities that we confirmed against the public C
header and the krunkit/podman-machine consumers, not just assumed:

- **virtio-blk on macOS/HVF — ✅ confirmed.** `krun_add_disk2(ctx, block_id, path,
  format, read_only)` (raw + qcow2) and `krun_add_disk3(…, direct_io, sync_mode)` attach
  one or more raw data disks (`/dev/vda…vdz`); macOS-specific sync semantics are baked
  into the API. This is the single fact the whole v1 rests on, and it holds. **Note:**
  `krun_set_root_disk`/`krun_set_data_disk` (an obvious API to reach for) are
  **deprecated in 1.x and removed in 2.0** — target `krun_add_disk2/3`. virtio-blk is a
  **build-time feature** (`BLK=1`); verify via `krun_has_feature(KRUN_FEATURE_BLK)` on
  the actual dylib.
- **virtio-fs on macOS — ✅ confirmed** via `krun_add_virtiofs(ctx, tag, path)` (root
  uses the reserved tag `/dev/root`). No host-side directory isolation is provided; that
  is the embedder's job.
- **Host↔guest channel — ✅ but nuanced.** vsock works via `krun_add_vsock_port(ctx,
  port, unix_socket_path)`, but on macOS it is **not real `AF_VSOCK`** — the VMM bridges
  it to a host **UNIX socket**. Microsandbox deliberately chose **virtio-serial** over
  vsock for its guest agent on macOS, which is a signal to spike both (see §12/M0).
- **Root-free port forwarding — ✅ confirmed, and simpler than this plan assumed.**
  libkrun's built-in **TSI** (Transparent Socket Impersonation) gives outbound
  connectivity + host-reachable guest ports via `krun_set_port_map(["host:guest", …])`
  with **no helper process and no root**. TSI requires the custom **libkrunfw** kernel
  and does not support raw/ICMP sockets or inbound UDP-listen from the guest.
- **Memory snapshot/restore — ❌ does not exist** in any shipping libkrun (neither
  `stable-1.19.x` nor 2.0 `main`). It is an **unmerged, design-contested prototype**
  (RFC #748, PRs #762/#767) that maintainers have not agreed to accept, with lazy-CoW,
  per-device state for virtio-blk/vsock, virtio-fs/TSI snapshot, and clone-reseed all
  still unsolved. Treat it as **not roadmapped**; memory snapshots come from alternate
  drivers in Phase 3, not from libkrun.
- **macOS packaging is not "just a static binary."** Any HVF caller — including the
  `got` binary itself — must be **codesigned with `com.apple.security.hypervisor`**
  (ad-hoc signing is fine, no paid cert) **plus `com.apple.security.cs.disable-library-
  validation`** to load the Homebrew `libkrun.dylib`. No root/sudo is needed to run VMs.
- **We ship a guest kernel.** libkrun boots a firmware bundle, **`libkrunfw`** — a
  separate build artifact we must vendor/ship; TSI networking depends on it.

## 4. Architecture overview

```
┌──────────────────────────────────────────────────────────────────┐
│  got CLI  (git-like verbs: switch, worktree, branch, up, ls …)     │
└───────────────┬──────────────────────────────────────────────────┘
                │ local IPC (unix socket / gRPC)
┌───────────────▼──────────────────────────────────────────────────┐
│  gotd — host manager (daemon or on-demand)                         │
│  • Ref↔Machine registry        • Port/hostname allocator           │
│  • Lifecycle orchestrator      • Snapshot/quiesce coordinator      │
│  • Git integration (hooks/wrap)• Reverse proxy (*.got.test)        │
├───────────────┬───────────────────────────┬───────────────────────┤
│ VmmDriver     │ StorageDriver             │ NetDriver              │
│ (libkrun /    │ (APFS clonefile /         │ (gvproxy / vmnet /     │
│  CH / FC /    │  reflink / ZFS / qcow2     │  user-net + port fwd)  │
│  QEMU / VZ)   │  CoW overlays + volumes)   │                        │
└───────────────┴───────────────────────────┴───────────────────────┘
                │ virtio (blk, fs, net, vsock)
┌───────────────▼──────────────────────────────────────────────────┐
│  Guest microVM (per branch/worktree)                               │
│  • gotd-agent (vsock/virtio-serial): start/stop svcs, health, exec │
│  • Declared services from got.toml (postgres, redis, app, …)       │
│  • Persistent data volume  +  code (virtio-fs share or in-disk)    │
└──────────────────────────────────────────────────────────────────┘
```

### 4.1 The core mapping

`gotd` maintains a registry mapping each git ref (and worktree) to a **Machine**:

```
Machine {
  id, project_id, ref (branch), worktree_path,
  base_image_ref,            // golden image this descends from
  data_disk,                 // CoW overlay: durable branch state
  snapshot,                  // optional saved memory+cpu state (Phase 3)
  state: Cold|Running|Paused|Saved,
  ports: { app: 51001, db: 51002, ... },
  vmm_handle,                // opaque driver handle when live
}
```

Registry lives at `~/.got/registry.db` (SQLite) plus per-project metadata in
`<repo>/.got/`.

### 4.2 Components

- **`got` CLI** — thin client; git-familiar verbs; talks to `gotd`.
- **`gotd` (host manager)** — orchestrates lifecycle, storage, networking, git
  integration. Can run as a launchd/systemd service or be spawned on demand.
- **`VmmDriver`** — trait: `create/start/pause/resume/snapshot/restore/destroy`.
  Impls: `libkrun` (default, **both** macOS *and* Linux), `cloud-hypervisor`,
  `firecracker`, `qemu`, `apple-vz`. **Caveat — impedance mismatch:** libkrun is an
  *in-process library*; CH/FC/QEMU are *child processes with REST/QMP APIs*; Apple VZ is
  a *framework* with a Swift/ObjC surface. A single trait will leak around disk-attach,
  net-config, and exec. We therefore **design the seam but build M1 against exactly one
  concrete combo** (`libkrun` + `apfs clonefile` + `TSI`) and only extract the trait when
  a second real driver forces its shape (§12).
- **`StorageDriver`** — trait for CoW disks + volumes. Impls: `apfs` (clonefile),
  `reflink` (btrfs/XFS), `zfs`, `qcow2` (portable backing-file fallback).
- **`NetDriver`** — user-mode networking + port forwarding; local DNS/reverse proxy
  for `*.got.test`. Impls: **libkrun TSI + `krun_set_port_map` (M1 default, no root/no
  helper)**, then `gvproxy`/`vmnet` (macOS), `tap`+`slirp`/`passt` (Linux) for faithful
  networking. (Domain is `.got.test`, an RFC 6761 dev TLD — **not** `.got.local`, because
  macOS routes all `.local` names to mDNS/Bonjour and they never reach our resolver.)
- **`gotd-agent`** — tiny static guest daemon over a host↔guest control channel:
  starts/stops declared services, runs health checks, performs **quiesce**
  (fsync/checkpoint DBs) before a snapshot, and provides an `exec` channel. **Transport
  is an open M0 decision:** `vsock` (via `krun_add_vsock_port`, which on macOS is a
  UNIX-socket proxy, not real `AF_VSOCK`) vs **virtio-serial** (what Microsandbox chose
  on macOS). Spike both for reliability before committing.

## 5. Storage & state model

State persistence is the heart of `got`. Three layered artifacts per project:

1. **Golden base image** — read-only. Built once from `got.toml` (OS + toolchain +
   project deps + service binaries). Rebuilt when the recipe changes. Content-addressed.

2. **Per-branch data disk (CoW overlay)** — writable copy-on-write layer on top of the
   base. Holds everything that mutates: DB data directories, caches, uploads, build
   artifacts, installed packages that aren't baked into the base. **This is what makes
   "the migration stays with the branch" work.**

3. **Optional memory snapshot** (Phase 3) — saved RAM+vCPU state for instant resume of
   *running processes*.

### 5.1 Copy-on-write strategy per platform

- **macOS:** APFS `clonefile()` gives instant CoW clones of a disk image file. Forking
  a branch's disk = one `clonefile` call. Zero-copy until write.
- **Linux:** `reflink` (btrfs, XFS), **ZFS** clones, or **qcow2 backing files** as a
  universal fallback that works on any filesystem.

This makes `got branch` a near-instant metadata operation regardless of disk size,
mirroring the "fork a machine as fast as it forks 1GB or 64GB" property the best cloud
sandboxes advertise.

**Clone ordering is not optional (correctness):** `clonefile()` clones the image file's
*on-disk extents*, so a live, dirty guest disk would be cloned torn/stale. Any CoW clone
(`got branch`/`fork`) must run: **(1)** guest quiesce (fsync + DB checkpoint via the
agent) → **(2)** host-side flush of the image file (`F_FULLFSYNC`) → **(3)** `clonefile`.
Prefer cloning from a `Saved`/stopped source.

**Durability boundary (macOS relaxed fsync):** on macOS libkrun's `KRUN_SYNC_RELAXED`
means a *guest* `fsync` flushes host OS buffers but does **not** force the physical drive
to flush. So committed DB writes survive a `got switch` (host page cache persists) but
are **not** guaranteed to survive host power-loss unless we select full-sync for that
volume. `got`'s durability contract is "survives switch," not "survives power-loss,"
unless a volume opts into `sync_mode=full` (see open questions).

### 5.2 Code: two mount models (configurable)

- **Model A — shared source (default):** the working tree lives on the host and is
  mounted into the VM via **virtio-fs**. The host editor/IDE sees files directly; git
  runs on the host. Only *runtime* state (DB/deps/artifacts) lives on the VM data disk.
  Simplest editor story; some virtio-fs perf caveats for huge trees.
  - **Unresolved composition problem:** "installed deps persist per branch" (the headline
    promise) conflicts with sharing the working tree from the host. `node_modules`,
    `.venv`, and build caches normally live *inside* the tree — if that tree is shared
    read/write via virtio-fs, those deps are shared across branches, not per-branch. Model
    A therefore needs an explicit rule for **which subtrees are runtime state
    (bind/overlaid from the per-branch data disk) vs source (virtio-fs)**. This must be
    specified before `got up` (see §12/M1 and open questions).
  - **Parallelism caveat:** a single repo checkout is one working tree, so Model-A branch
    *switching* is inherently serial (one tree ⇒ one active VM). True simultaneous VMs
    (goal #4, the "three agents at once" story) come from **worktrees**, not single-dir
    switching.
- **Model B — enclosed (max isolation):** the repo lives entirely inside the VM disk.
  Best for untrusted/parallel agents. Host access via SSH/vsock + remote-dev or a
  file-sync bridge. Chosen automatically for `got worktree add --isolated`. **Also the
  only model compatible with Phase-3 memory snapshots** (virtio-fs cannot be
  memory-snapshotted — see §12/M3).

### 5.3 Volumes

`got.toml` can declare named **volumes** (e.g. the Postgres data dir) that are pinned
to the branch data disk and explicitly quiesced before snapshot. Volumes can be marked
`shared` (persist across branches — e.g. a package download cache) or `per-branch`
(default — isolated).

**Two constraints inherited from the block-device model:** (1) a `shared` writable
volume must be **single-writer** — mounting one writable block device into two live
guests at once risks filesystem corruption (this is how Microsandbox's volumes behave
too); `shared` therefore means "reattached serially / read-mostly," not "concurrently
read-write." (2) Cloning a raw ext4 image duplicates its **filesystem UUID**; this is
harmless while each clone lives in its own guest kernel, but breaks if we ever mount a
clone **host-side** (for inspection) or into a second guest alongside the original —
generate a fresh UUID in those cases.

## 6. Lifecycle & the switch operation

The defining operation is `got switch <ref>`. Sequence:

```
switch(target):
  cur = active_machine_for_current_ref()

  # Pre-flight the git op BEFORE touching the current VM, so a dirty/blocked
  # tree fails fast and leaves the running machine untouched.
  if not git_switch_would_succeed(target):     # uncommitted/conflicting changes?
      abort_or_prompt_stash(target)            # mirror `git switch` semantics; no VM change

  if cur:
    agent.quiesce(cur)            # fsync, checkpoint DBs, flush caches
    if driver.supports_mem_snapshot:
        driver.pause(cur); driver.snapshot(cur)     # Phase 3: keep processes
    else:
        agent.stop_services(cur); driver.stop(cur)  # Phase 1: disk persists
    mark(cur, Saved|Cold)

  try:
      git_switch(target)         # or worktree hop; update working tree
  except GitError:
      resume(cur)                # ROLLBACK: restart the machine we just stopped
      raise

  tgt = registry.get(target) or provision(target)   # CoW-clone base/parent (quiesce+flush+clonefile, §5.1)
  ports = allocator.assign(tgt)                      # stable per-branch ports
  if tgt.snapshot and driver.supports_restore:
      driver.restore(tgt.snapshot)                   # instant, live processes
  else:
      driver.start(tgt); agent.start_services(tgt)   # boot + start declared svcs
  proxy.route("<ref>.got.test" -> tgt.ports.app)
  mark(tgt, Running)
```

Guarantees:
- **Safety:** always quiesce before saving; never lose committed DB writes. Any CoW
  clone follows the quiesce → host-flush → `clonefile` ordering from §5.1.
- **Atomicity:** the git op is pre-flighted and, if it fails after we've stopped the
  source VM, we **roll back** by resuming the source — never leave the user with no
  running machine and a half-switched tree.
- **Dirty-tree handling:** `got switch` mirrors `git switch` (refuse on conflict, or
  wrap `stash`); this is *why* the wrapper is primary over hooks (§8).
- **Determinism:** ports and hostnames are stable per ref across switches.
- **Speed target:** Phase 1 ≤ a few seconds (boot + service start). Phase 3 sub-second
  (memory restore).

Other lifecycle verbs: `up` (provision+start current ref), `down`/`sleep`
(quiesce+stop, keep disk), `reset --hard` (discard data disk, re-clone base),
`branch`/`fork` (**quiesce source → host-flush → CoW clone** current machine to a new
ref, per §5.1), `rm` (destroy machine + disk), `gc` (prune orphaned/dead-branch
machines).

### 6.1 Idle & resource management

- Auto-sleep VMs after configurable idle (default e.g. 5 min): quiesce + stop, freeing
  CPU/RAM; disk state stays. Resume on next access.
- Global caps: max concurrent running VMs, per-VM vCPU/RAM defaults (e.g. 2 vCPU /
  2–4 GiB), overridable in `got.toml`. Prevents a laptop meltdown when many branches
  exist — most are `Saved`, only the working set is `Running`.

## 7. Networking & port model

Problem: parallel VMs must not collide on ports, and users must reach services easily.

- **User-mode networking** per VM (no root, no bridge headaches). **M1 uses libkrun's
  built-in TSI** (Transparent Socket Impersonation) + `krun_set_port_map` — it exposes
  guest listening ports on host localhost with **no helper process and no root**, which
  is exactly what the reverse proxy needs. `gvproxy`/`vmnet` (macOS) and `passt`/`slirp`
  (Linux) are **later** NetDriver options for apps that need faithful networking, because
  **TSI has limits**: TCP/UDP over AF_INET/INET6 only (no raw/ICMP sockets), no inbound
  UDP-listen from the guest, and it requires the custom libkrunfw kernel. (Note: with
  `passt`, `krun_set_port_map` returns `-ENOTSUP` — port mapping moves to passt's CLI.)
- **Deterministic port allocation:** each (ref, service) gets a stable host port from a
  managed range (e.g. 51000+). `got ls` shows the map. **Caveat:** 51000+ overlaps the OS
  ephemeral range (macOS 49152–65535), so leases can race OS-assigned ports; allocate
  with bind-failure retry and record leases rather than assuming the range is free.
- **Local reverse proxy + DNS:** `gotd` runs a proxy resolving `<ref>.got.test` (and
  `<service>.<ref>.got.test`) to the right VM/port. So `billing.got.test:3000`
  always hits the billing branch's app, regardless of which VMs are up. We use
  **`.got.test`** (RFC 6761 reserved for testing), resolved via an `/etc/resolver/got.test`
  file pointing at `gotd`'s local DNS. **We must not use `.got.local`:** macOS sends every
  `.local` name to mDNSResponder/Bonjour and `/etc/resolver` overrides are ignored for it,
  so those names would never reach our proxy.
- **Egress policy (later):** optional per-VM allow/deny lists for network egress
  (valuable for untrusted agents), following the policy-proxy pattern common in the
  sandbox ecosystem.

## 8. Git integration

Two complementary mechanisms:

1. **Wrapper verbs (primary):** `got switch/checkout/worktree/branch/stash` wrap the
   corresponding git commands and attach machine lifecycle. This is the intended daily
   driver and gives us full control over ordering (quiesce before checkout, etc.).
2. **Hooks (assistive):** installed `post-checkout`, `post-merge`, `post-worktree`
   hooks let plain `git` usage still trigger `gotd` reconciliation, so `got` degrades
   gracefully when users forget to use the wrapper. **Hard limit:** `post-checkout` fires
   *after* the working tree has already changed, so hooks **cannot quiesce the old
   branch's DB before the switch** — they can guarantee reconciliation but **not** the
   safety invariant. Under bare `git`, treat the prior machine's state as
   crash-consistent, not cleanly quiesced. This is the core reason the wrapper is primary.

Worktree strategy: `got worktree add <ref>` creates a git worktree *and* a dedicated
machine (defaulting to `--isolated`/Model B for agent safety). Each worktree ↔ machine
is fully independent, solving parallel-agent interference.

Mapping storage: `<repo>/.got/config.toml` (project settings, image recipe ref) and
`~/.got/registry.db` (global ref→machine index, port leases, snapshots).

## 9. Configuration: `got.toml`

Committed to the repo; defines the reproducible environment (golden image + services).

```toml
[project]
name = "acme"
base = "debian:12"            # or a Dockerfile/OCI image; got builds the golden image
mount = "shared"             # shared (virtio-fs) | enclosed (in-disk)

[resources]
cpus = 2
memory = "4GiB"

[build]                       # baked into the read-only golden image
run = [
  "apt-get update && apt-get install -y postgresql redis-server nodejs",
  "npm ci",
]

[[services]]                  # started by gotd-agent on VM start
name = "postgres"
command = "pg_ctlcluster 16 main start"
volume = "pgdata"            # persisted on the per-branch data disk
health = "pg_isready"
port = 5432

[[services]]
name = "app"
command = "npm run dev"
port = 3000
depends_on = ["postgres"]

[[volumes]]
name = "pgdata"
scope = "per-branch"         # per-branch (default) | shared

[snapshot]
mode = "disk"               # disk (Phase 1) | memory (Phase 3, driver-dependent)
quiesce = ["postgres"]      # services to checkpoint/flush before saving
```

## 10. CLI surface (initial)

```
got init                     # scaffold got.toml, build golden image
got up                       # provision + start machine for current ref
got switch <ref>             # the core op: save current, restore/boot target
got checkout <ref>           # alias mirroring git
got branch <new> [from]      # git branch + CoW-clone the machine
got worktree add <ref>       # new worktree + isolated machine
got ls                       # list refs, machine state, ports, uptime
got status                   # current ref's machine + services health
got sleep | down             # quiesce + stop (keep disk)
got reset --hard             # discard data disk; re-clone from golden image
got rm <ref>                 # destroy machine + disk
got exec <ref> -- <cmd>      # run a command inside a machine (agent exec channel)
got ssh <ref>                # shell into a machine
got logs <ref> [service]     # stream service logs
got gc                       # prune orphaned machines/snapshots
got doctor                   # check host prerequisites (HVF/KVM, FS CoW, tools)
```

## 11. Technology stack

- **Language:** **Rust** for CLI, `gotd`, and drivers. strong FS, async, and packaging
  story. FC/CH are Rust; libkrun is a C library.
- **libkrun binding:** FFI to the **upstream C ABI**, pinned to **`stable-1.19.x`** (not
  `main`/2.0, which is ABI-unstable). Disk attach via `krun_add_disk2/3` (**not** the
  deprecated `krun_set_root_disk`/`krun_set_data_disk`); confirm build flags at runtime
  with `krun_has_feature(BLK/NET)`. We do **not** use Microsandbox's `msb_krun` Rust fork
  (see §3.3).
- **Guest kernel:** vendor/ship **`libkrunfw`** (the firmware bundle libkrun boots); TSI
  networking depends on it.
- **macOS packaging:** the `got` binary is **codesigned with `com.apple.security.hypervisor`
  + `com.apple.security.cs.disable-library-validation`** and **dynamically links**
  `libkrun.dylib` (LGPL-2.1). So "single binary" but **not** fully static on macOS. No
  root to run VMs.
- **Guest agent:** Rust, static (musl) → one small binary baked into the golden image.
- **Host IPC:** unix socket + gRPC or JSON-RPC (`tonic`/`prost` or a lightweight
  framing). Guest↔host over **vsock or virtio-serial** (decide in M0, §4.2).
- **Datastore:** SQLite (`rusqlite`) for the registry/port leases.
- **Image build:** accept a Dockerfile/OCI image and convert to a bootable rootfs for
  libkrun, porting Microsandbox's concrete design (EROFS layers + VMDK flat descriptor as
  a read-only virtio-blk root + writable ext4 upper + in-guest overlayfs, §3.3). **Note
  the bootstrap:** `[build] run = [...]` executes *Linux* commands, so `got init` must
  boot a build microVM to run them (docker-build semantics on our own VMM) or shell out
  to an external OCI builder — decide in M0 (open question).
- **Networking:** libkrun **TSI + `krun_set_port_map`** (M1); `gvproxy`/`vmnet` (macOS),
  `passt` (Linux) later for faithful networking.

## 12. Phased roadmap

**M0 — Spikes / de-risking (1–2 wks; note this scope is on the ambitious side)**
- Prototype **direct libkrun** boot (FFI to the **`stable-1.19.x` C ABI**) on macOS
  Apple Silicon (HVF) and Linux (KVM): boot a Debian rootfs, attach a **`krun_add_disk2`**
  virtio-blk data disk, virtio-fs a host dir. **Codesign the spike binary**
  (`com.apple.security.hypervisor` + disable-library-validation) and confirm it runs
  unprivileged. (Backend decided in §3.3; this validates the path against the pinned ABI.)
- **Agent transport bake-off:** exec over **vsock vs virtio-serial**; pick the winner.
- **Rootfs decision:** port Microsandbox's **OCI → EROFS+VMDK+ext4-overlay** conversion
  vs a simpler single-ext4 root; decide here, don't defer.
- **Networking:** confirm **TSI + `krun_set_port_map`** exposes a guest TCP port on host
  localhost with no root/helper.
- Validate the **quiesce → host-flush (`F_FULLFSYNC`) → `clonefile`** ordering produces
  an independently-writable branch disk both VMs can boot (and Linux reflink/qcow2).
- **Golden-image build bootstrap:** decide external OCI builder vs a `got`-owned build
  microVM for `[build] run` steps.
- Output: a throwaway demo that boots two isolated VMs on distinct ports.

**M1 — Core lifecycle, disk-state persistence (MVP)**
- **One concrete stack, no premature traits:** `libkrun` + `apfs clonefile` + `TSI`.
  Keep the driver seams as thin internal boundaries; extract `VmmDriver`/`StorageDriver`/
  `NetDriver` traits only when a second driver forces their shape (§4.2).
- `gotd` registry, port allocator (with ephemeral-range collision handling), `got
  init/up/switch/ls/down/rm`.
- `gotd-agent`: start/stop declared services, health, quiesce (fsync/checkpoint).
- `got.toml` parsing + golden-image build (from Dockerfile/base).
- **Resolve the Model-A runtime-vs-source composition rule** (which subtrees —
  `node_modules`, `.venv`, caches — are overlaid from the data disk) *before* `got up`,
  or default new projects to Model B until it's solved.
- **Delivers the headline value:** migrations/DB/deps persist per branch via CoW disk;
  parallel worktrees on isolated ports. *(No memory snapshots yet.)*

**M2 — Fork, reset, DX polish**
- `got branch/worktree add` with CoW machine cloning (quiesce→flush→clone); `reset
  --hard`; `gc`.
- Reverse proxy + `*.got.test` DNS (`/etc/resolver/got.test`); `got exec/ssh/logs`;
  `got doctor` (checks HVF/KVM, codesign entitlements, FS CoW, resolver, tools).
- Auto-sleep/idle management; resource caps; graceful multi-VM handling.

**M3 — Memory snapshots (instant live resume)**
- Add snapshot/restore to the driver interface; implement via **Cloud Hypervisor/
  Firecracker on Linux** and **Apple VZ on macOS** (alternate drivers). **Not libkrun** —
  its snapshot support is an unmerged, design-contested prototype with no committed
  timeline (§3.4); don't plan around it.
- **Coupling to surface, not fight:** memory snapshots require **Model B (in-disk)** — a
  virtio-fs-shared source cannot be memory-snapshotted, and FC/CH don't do virtio-fs at
  all. M3 effectively targets enclosed machines only.
- Coordinated quiesce for consistent memory+disk snapshots (memory snapshot + CoW disk
  clone taken at the same quiesced instant).

**M4 — Sharing & remote (v-future)**
- Export/import a machine alongside a git push/pull. **Portability limit:** the **disk**
  (raw/qcow2) is portable; a **memory+vCPU snapshot is driver- and host-CPU-specific**
  and won't restore on a different VMM/microarchitecture. "Share a running system" is
  realistically disk-state + re-boot, not cross-host live restore — scope the vision's
  time-travel/north-star claim accordingly.
- Remote host backend (run branch-machines on a Linux server via Firecracker/CH).
- Egress policy proxy; agent-fleet ergonomics; optional MCP server so agents manage
  their own branch-machines.

## 13. Key risks & mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| libkrun has **no** memory snapshot (unmerged, contested prototype) | No instant live-resume in v1 | Ship disk-state persistence first (covers the core use case); add CH/FC/VZ drivers for memory snapshots in M3; never block on libkrun snapshot |
| macOS relaxed `fsync` (`KRUN_SYNC_RELAXED`) | Committed writes lost on host power-loss | Contract = "survives switch," not "survives power-loss"; offer per-volume `sync_mode=full`; always guest-quiesce **and host-flush** before any clone |
| Wrong dev TLD (`.local` → mDNS) | `*.got.*` never resolves on macOS | Use `.got.test` (RFC 6761) + `/etc/resolver/got.test`; `got doctor` verifies |
| macOS codesigning/entitlements required | App won't launch / can't load dylib | Codesign with `com.apple.security.hypervisor` + disable-library-validation (ad-hoc OK); `got doctor` checks; bundle `libkrunfw` |
| Golden-image build bootstrap (must run Linux `[build]` steps) | `got init` can't build images on a bare macOS host | Boot a `got`-owned build microVM, or shell out to an external OCI builder; decide in M0 |
| Model-A deps composition ("per-branch deps" vs shared source) | Headline promise silently broken | Explicit runtime-vs-source overlay rule; default to Model B until solved |
| virtio-fs performance on large repos (macOS) | Slow file ops in Model A | Offer Model B (in-disk) for heavy repos; cache; benchmark early in M0 |
| Driver-trait impedance (in-proc lib vs child-proc VMMs vs VZ framework) | Leaky abstraction, schedule blowout | Build M1 against ONE concrete combo; extract traits only when a 2nd driver forces the shape |
| Storage bloat from many CoW disks | Disk fills up | CoW keeps clones cheap; `got gc`, quotas, shared caches, auto-prune dead branches |
| Snapshot/DB inconsistency | Data corruption | Mandatory quiesce (fsync/checkpoint) → host-flush → clone; DB-aware hooks in `got.toml` |
| Scope creep toward "a cloud platform" | Never ships | Local-first, git-verb-scoped MVP; remote/cloud strictly post-M3 |
| Nested virt / driver portability | Fragmented behavior | libkrun default on *both* OSes for parity; `got doctor` capability detection; add other drivers only where they pay off |

## 14. Open questions

- **libkrun binding path** — upstream C ABI (chosen, §3.3) vs the `msb_krun` Rust fork.
  Confirm the C ABI covers every M1 need against `stable-1.19.x` in the M0 spike.
- **Golden-image build engine** — require an external OCI builder for v1, or build a
  `got`-owned build microVM to run `[build] run` Linux steps? (Gates `got init`.)
- **Model-A composition rule** — exact list of runtime dirs (`node_modules`, `.venv`,
  build caches) overlaid from the per-branch data disk vs served from the shared source.
  (Gates the headline "deps persist per branch" promise.)
- **Agent transport** — vsock-UNIX-proxy vs virtio-serial on macOS (decide in M0).
- **Durability contract** — is the guarantee "survives switch" (host page cache) or
  "survives power-loss" (full-sync)? Per-volume `sync_mode`? Different perf profile.
- Default mount model per project type — can we auto-detect repo size/agent usage and
  choose Model A vs B?
- Golden-image rebuild triggers — hash `got.toml` + lockfiles; how to make partial
  rebuilds fast?
- ~~How much of Microsandbox to reuse vs. build directly on libkrun.~~ **Resolved:**
  bind libkrun directly via the upstream C ABI (not `msb_krun`); use Microsandbox only as
  a reference for VM-launch logic + OCI→rootfs (see §3.3).
- Secret handling — inject at host/proxy layer (placeholdering) vs. in-guest; default
  to never baking secrets into images.
- Multi-user / team registry format for shareable machine states (M4).

## 15. Summary of decisions

1. **MicroVM:** `libkrun` as the default cross-platform backend **on both macOS (HVF) and
   Linux (KVM)** for code-path parity. `VmmDriver` is a **seam designed early but
   abstracted late** — Firecracker/Cloud Hypervisor/QEMU/Apple VZ get added only for
   memory snapshots (M3) and remote hosts (M4), not in M1.
2. **Backend integration:** bind **libkrun directly** via its **upstream C ABI**, pinned
   to `stable-1.19.x`, dynamically linked, LGPL-2.1 (macOS binary codesigned with the
   hypervisor entitlement + `libkrunfw` bundled). Do **not** build on Microsandbox and do
   **not** adopt its `msb_krun` Rust fork — both would double-stack another team's
   abstraction. Microsandbox (Apache-2.0) is a **reference only**, for VM-launch logic and
   its OCI→(EROFS+VMDK+ext4-overlay) rootfs conversion. *(Correction: Microsandbox uses a
   Rust fork of libkrun, not the C ABI — it is not evidence for the C-ABI path.)*
3. **State model:** golden base image + **per-branch copy-on-write data disk** (APFS
   clonefile / reflink / qcow2). Disk-state persistence delivers the core value in v1;
   memory snapshots are an additive Phase-3 enhancement (and require Model B). Every clone
   follows quiesce → host-flush → `clonefile`; durability contract is "survives switch."
4. **Interface:** git-like CLI + `gotd` host manager + `gotd-agent` guest daemon
   (transport: vsock or virtio-serial, decided in M0). `got switch` pre-flights the git op
   and rolls back on failure.
5. **Isolation & networking:** one kernel per branch/worktree; deterministic per-ref
   ports (M1 via libkrun **TSI + `krun_set_port_map`**, no root/helper) + `*.got.test`
   (RFC 6761, **not** `.local`) reverse proxy so parallel agents never collide. True
   simultaneity comes from **worktrees**, not single-dir branch switching.
6. **Language:** Rust throughout; Linux guests only; local-first, cloud-optional.
