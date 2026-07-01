# got — Product Vision

> **Every agent attempt gets its own machine.**
> *Under the hood, every branch is a machine.*
>
> `got` gives each git ref, worktree, and agent attempt its own microVM — so the
> entire application runtime (databases, migrations, running services, caches,
> installed packages, background jobs, generated files) can be forked, rolled
> back, and promoted the way you already fork, roll back, and promote code.

---

## 1. The agent-era problem

Coding agents no longer just write code. They **execute** it — running
migrations, installing packages, starting services, mutating databases, hitting
the network, seeding data, and leaving residue. Every autonomous run is a real
state change against a real system.

Git isolates files. It has never isolated runtime. That gap was a nuisance in
2020. In 2026, with 4–8 agents open at once against the same repo, it is the
bottleneck.

**The unsafe-mutation problem.** An agent applies a migration to try a refactor.
The refactor fails. The migration stays. Now the next agent, the next branch,
and your dev server are all running against the wrong schema. Nobody knows.

**The collision problem.** Three agents in three worktrees all bind port 3000.
They all migrate "the" database. They all write to the same Redis. Worktrees
isolate *files*, not *worlds* — the agents corrupt each other silently, and
you cannot trust any of their results.

**The rollback gap.** `git reset --hard` rewinds the code. It does not rewind
the DB, the caches, the installed deps, the background jobs, or the generated
files. Autonomous agents live in the delta between those two things.

**The promotion problem.** An agent finds the right approach on attempt 7 of
12. Its DB has the right migration; its `node_modules` has the right lockfile;
its logs prove it. Without a primitive to promote that attempt back into git,
the work is trapped — the only way out is to redo it by hand.

The root cause is the same in every case: **agents mutate whole runtimes, and
git only versions files.**

## 2. The core idea

`got` makes runtime state a first-class, git-versioned citizen by pairing every
ref, worktree, and agent attempt with its own **microVM** — a real,
hardware-isolated Linux machine with its own kernel, disk, network, and
processes.

Two units of state, deliberately distinct:

- A **branch-machine** is durable state tied to a git ref. Switch to it and its
  database, migrations, services, and deps come with you.
- An **attempt-machine** is an ephemeral copy-on-write fork of a branch-machine.
  Agents run inside attempts. Winners get promoted to branches; losers are
  garbage-collected.

Every fork is instant — APFS `clonefile` on macOS, reflink/ZFS on Linux — so
forking a 20 GB dev environment is a metadata operation, not a copy. Every
rollback is total: code, DB, deps, services, caches, and generated files rewind
together. Every winner becomes real git history: `got try promote` turns an
attempt-machine into a named branch, ready to push and PR.

## 3. The five-minute demo

```
$ got init                            # scaffold got.toml, build golden image
$ got up                              # branch-machine for main is running

# fan out
$ got try spawn 6 --agent claude "refactor billing to Stripe subscriptions"
  attempt/a1f3  running  app:51001 db:51002  agent: claude
  attempt/b820  running  app:51011 db:51012  agent: claude
  attempt/c4d1  running  app:51021 db:51022  agent: claude
  attempt/d7ee  running  app:51031 db:51032  agent: claude
  attempt/e529  running  app:51041 db:51042  agent: claude
  attempt/f014  running  app:51051 db:51052  agent: claude

# six agents. six databases. six port sets. zero collisions.

$ got try inspect                     # diff, tests, logs, resource use per attempt
$ got try promote c4d1 --as feature/billing-stripe
$ got try discard --losers            # everything else is gone
```

That loop — **fork N attempts, isolate every runtime, promote one, discard the
rest** — is not a workflow that exists on your laptop today. It is what `got`
is for.

## 4. Who this is for

1. **Developers orchestrating coding agents** — Cursor, Claude Code, Codex,
   opencode, Aider, Factory, Amp — running multiple attempts against stateful
   apps and needing real isolation, real rollback, and real promotion.
2. **The agents themselves** — through a first-class MCP server and SDK, so
   agents create, fork, exec, log, and reset machines as primitives instead of
   scraping brittle shell commands.
3. **Solo developers on stateful apps** who are tired of environment drift
   across their own branches.
4. **Teams and CI** that need a reproducible, shareable definition of "a
   running instance of our app" that maps cleanly onto their branch workflow.

`got` is **local-first, remote-compatible**. It runs on your M-series laptop
with no account and no per-second billing. The same primitive extends to a
shared remote Linux host when you outgrow local — but the wedge is the machine
already under your desk.

## 5. The product surface

Agents are not humans with a keyboard. A must-use agent tool ships more than a
CLI on day one:

- **`got` CLI** — git-native verbs (`up`, `switch`, `worktree`, `branch`,
  `try`, `reset`, `promote`, `discard`) for humans.
- **`got mcp serve`** — first-class MCP server. Agents call structured tools:
  `create_attempt`, `fork`, `exec`, `logs`, `expose_url`, `reset`, `promote`,
  `set_policy`. No shell scraping.
- **`got` SDK** — TypeScript and Python bindings so orchestrators (LangGraph,
  crewAI, custom pipelines) can drive `got` programmatically.
- **`got.toml`** — the reproducible environment recipe (golden image, services,
  volumes, policy), committed to the repo.
- **Attempt ledger** — append-only audit log per attempt: agent, prompt,
  commands run, ports touched, files changed, network calls made, final diff,
  outcome. You can read what the agent did before you promote it.
- **Policy defaults** — egress deny-by-default, secret injection at the proxy
  layer (never baked into images), hard CPU/mem/disk caps per attempt,
  per-repo egress allowlists.

## 6. Design principles

1. **Git is the source of truth for state, not just code.** If git knows a ref,
   `got` can give it a machine.
2. **Programmable, not just interactive.** Every human verb has an MCP tool and
   an SDK method. Agents are equal-class users.
3. **Safe by default for autonomous mutation.** Isolation is a hardware
   guarantee. Egress is denied by default. Resources are capped by default. An
   agent is *allowed* to migrate the DB and install packages because the blast
   radius is one machine.
4. **Rewind-friendly.** Any attempt can be reset. DB, deps, services, caches,
   and generated files all return to a clean fork point.
5. **Auditable.** Every attempt leaves a readable ledger before you promote.
6. **Fast fork or it does not count.** Sub-second CoW disk clones. Boot in
   seconds. Switching a branch feels like `git switch`.
7. **Reproducible by default, mutable by choice.** Every machine descends from
   a declared golden image. Clean state is one command away; accumulated state
   is preserved unless you ask to discard it.
8. **Feel like git.** The primary interface mirrors git verbs. No Kubernetes.
   No YAML sprawl. No daemon to babysit for the common case.
9. **Local-first, remote-compatible.** Works offline on one laptop. Same
   primitive runs on a shared remote host without changing the model.

## 7. Why now

- **Agents made safe execution the bottleneck.** Writing code is no longer the
  hard part. Running it against a real system, in parallel, without poisoning
  the world, is.
- **MCP became the substrate.** Anthropic's MCP and the OpenAI Agents SDK
  standardized how agents talk to tools — so a runtime that speaks MCP natively
  is instantly usable by every serious agent framework.
- **Local microVMs became cheap.** libkrun boots a Linux microVM on macOS
  Apple Silicon in under 200 ms with no root, no daemon, and no Docker Desktop.
  A per-attempt machine is no longer expensive.
- **Copy-on-write storage is universal.** APFS `clonefile` on macOS and
  reflink/ZFS/btrfs on Linux make instant, near-free disk forks a primitive we
  can build on. "Fork the machine" is now a metadata operation.
- **Cloud sandboxes proved the demand but left the gap.** E2B, Modal, Daytona,
  Cloudflare Sandboxes, Vercel Sandbox, Runloop — every serious agent stack
  needs a runtime. None of them is local, branch-attached, and private by
  default. That is the gap.

## 8. How it's different

| Tool                                 | Type                | Runtime isolation      | Branch-attached state       | Per-attempt fork | Local & private     |
|--------------------------------------|---------------------|------------------------|-----------------------------|------------------|---------------------|
| `git worktree`                       | VCS                 | ❌ shared runtime      | ❌                          | ❌               | ✅                  |
| Docker Compose                       | container           | partial (shared kernel)| ❌ (one shared set)         | ❌               | ✅                  |
| Dev Containers / Codespaces          | container           | partial (shared kernel)| per-container, not per-ref  | ❌               | Codespaces hosted   |
| Gitpod / Coder                       | cloud workspace     | container / VM         | per-workspace, not per-ref  | ❌               | hosted              |
| GitButler                            | VCS UX              | ❌ shared runtime      | ❌                          | ❌               | ✅                  |
| Neon / PlanetScale / Xata            | DB branching        | DB-scoped only         | DB per branch               | limited          | hosted              |
| E2B / Modal / Daytona / Runloop      | hosted agent sandbox| microVM / container    | per-session                 | ✅               | ❌ hosted, metered  |
| Cloudflare / Vercel Sandbox          | edge / hosted       | container / Firecracker| per-session                 | ✅               | ❌ hosted           |
| Microsandbox / SmolVM                | local microVM       | microVM                | ❌ (session, not ref)       | ✅               | ✅                  |
| **got**                              | **git-native runtime** | **microVM**         | **✅ per branch and per attempt** | **✅ CoW**  | **✅**              |

`got`'s unique wedge: **local, git-native, branch-attached, per-attempt
persistent compute.** Nobody else combines those four. Cloud sandboxes own
hosted programmable agent runtimes; local microVM engines own the primitive;
`got` owns the git-native lifecycle layer that ties them to how developers and
agents actually work — refs, forks, attempts, promotions, rollbacks.

## 9. North star

`got` becomes **the default execution substrate for coding agents**:

- **Agent tree search over real stateful apps.** Fan out dozens of attempts
  from a commit; keep the one that works.
- **Auditable autonomous runs.** Every attempt's ledger is readable before you
  promote it.
- **Policy-bounded execution.** Egress, secrets, resources, and time are
  enforced at the machine boundary — not on the trust of the model.
- **Promote to git.** Winning attempts become branches, PRs, and merges
  through the workflow developers already know.
- **Local ↔ remote continuity.** The same branch-machine lives on your laptop
  today and on a shared Linux host tomorrow — because it is a portable disk
  and a recipe, not a hosted account.

## 10. What year-one success looks like

- **Install and first useful demo in under five minutes** on a clean Apple
  Silicon Mac.
- **A developer runs 4–8 agents against one repo** without DB, port, or
  dependency collisions.
- **Every serious agent framework can drive `got` through MCP** — no bash
  scraping required.
- **`got try promote` turns a winning attempt into a branch and a PR**
  without redoing the work by hand.
- **Runtime rollback is one command** — code, DB, deps, and services all
  rewind together.
- **`got.toml` becomes the default template** for agent-heavy repos.
- **"But it works on my branch"** stops being a phrase, because the branch —
  and every attempt under it — *is* the machine.
