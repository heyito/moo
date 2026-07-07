---
name: moo-code
description: Develop inside moo machines so runtime state (database, packages, services) is isolated per branch and versioned per commit. Use when starting work on an issue, branch, or feature in a repository that uses moo (has a moo.toml or the user mentions moo), and for any git workflow — checkout, commit, merge, bisect, parallel attempts.
---

# Coding with moo

One noun (machine), four verbs. A machine is a full Linux VM with
copy-on-write state; its snapshots are tagged with git commits and follow
`git checkout`.

```
moo new <name> [from <src>] [--detached]   create or restore (idempotent)
moo run <name> -- <cmd> [args...]          execute inside (docker-exec semantics)
moo save [<name>]                          snapshot, tagged with current commit
moo drop <name> [--force] [--snapshots]    destroy machine (snapshots survive)
moo ls                                     machines, host->guest port map, snapshots
moo open <name> [guest-port] [/path]       print + open the host URL for a
                                           forwarded guest port in the browser
```

Prerequisite — `moo` installed and healthy (installs it if missing):

```bash
moo doctor >/dev/null 2>&1 || curl -fsSL https://github.com/heyito/moo/releases/latest/download/install.sh | sh
```

## Starting an issue

1. Give the work its own files — **default to a worktree**. A machine
   isolates the runtime, not the working tree: the host tree syncs in
   and is authoritative, so two sessions editing one checkout overwrite
   each other no matter how many machines they have. A worktree costs
   nothing and makes the session parallel-safe from the start:

```bash
git worktree add ../$(basename $PWD)-feat-x -b feat/x
cd ../$(basename $PWD)-feat-x
```

   Branching in place (`git checkout -b feat/x`) is acceptable only when
   this session is certainly the sole user of the checkout — when in
   doubt, take the worktree.

2. Create its machine, named after the branch, forked from the provisioned
   baseline if one exists (`moo ls` shows a `base` handle):

```bash
moo new feat/x from base     # or: moo new feat/x
```

3. Run **everything runtime-related inside the machine**: migrations,
   seeds, servers, tests, package installs. Edit code on the host as
   normal — `moo new` and `moo run` automatically sync your working tree
   (tracked + untracked-unignored files, uncommitted changes included)
   into the machine at `/srv/app` (or `[project] workdir` from moo.toml)
   whenever it changed. Gitignored files are never pushed or deleted, so
   the machine's own `node_modules`, build output, and `.env` survive.

```bash
moo run feat/x -- 'cd /srv/app && npm run migrate'
```

The host tree is authoritative for synced files: host edits and deletions
propagate on the next `run`; guest edits to synced files last only until
the host tree next changes. Machines only sync when the command is run
from inside the repository they were created from.

## The one rule: save at commit boundaries

`moo save` is `git commit` for the runtime. After each meaningful commit:

```bash
git commit -am "add billing migration"
moo save feat/x
```

Saves are idempotent and deduplicated — saving often is free. **Save
before any `git checkout`**, because `moo new` on an existing handle
prefers the snapshot for the current commit over unsaved live state.

## Moving through history

The machine follows the code:

```bash
git checkout <anything>   # older commit, other branch, bisect probe
moo new feat/x            # boots the snapshot saved for that commit,
                          # or the live overlay if none was saved
```

`git reset --hard` and `git rebase` do not auto-restore — run
`moo new <name>` afterwards.

## Parallel attempts and promotion

Machines isolate runtime; **worktrees isolate files**. Every parallel
attempt needs both — one worktree + one machine per attempt:

```bash
git worktree add ../repo-attempt-1 -b attempt-1
cd ../repo-attempt-1
moo new attempt-1 from feat/x     # sub-second CoW fork, fully isolated
moo run attempt-1 -- <agent work>
moo save attempt-1
git merge <winning branch>        # promote via git (from the main checkout)
moo drop attempt-1                # losers vanish; snapshots survive
git worktree remove ../repo-attempt-1
```

Never run two agent sessions against the same checkout: they will
overwrite each other's edits and stash each other's work, and each
branch switch rewrites the files synced into the other session's
machine.

Machines never collide: each has its own filesystem, DB, processes, and
its own stable host port per declared guest port (see `moo ls`).

## Semantics that matter

- `moo run` passes the command to `sh -c` in the guest as root. Exit code
  and combined stdout/stderr round-trip to the caller.
- Services started in a machine keep running between `moo run` calls.
  Start them with `nohup … &` inside the command.
- Reach guest services from the host at `localhost:<host-port>` from
  `moo ls`, or let `moo open <name> <guest-port>` resolve the mapping and
  open the browser for you. A service must listen on `0.0.0.0` (not only
  loopback) to be reachable from the host — the same rule as containers.
  Plain request/response protocols (HTTP) work; TCP half-close is not
  proxied faithfully.
- The machine's loopback is private: `localhost` inside the machine is
  the machine's own, never the host's, and machines never see each other.
- A stopped machine reboots automatically on the next `moo run`.
- Live machine disk survives shutdown, not host power loss; snapshots
  survive power loss.

## Bisecting runtime-dependent bugs

```bash
git bisect start <bad> <good>
git bisect run bash -c 'moo new probe && moo run probe -- <test command>'
```

Each probe boots the runtime saved for that commit — migrations and seed
state match the code under test. Requires that saves were made at those
commits.
