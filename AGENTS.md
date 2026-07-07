# moo — agent reference

`moo` gives every git branch, worktree, or agent attempt its own
hardware-isolated Linux machine whose state is snapshotted per git commit
and restored by `git checkout`.

## Install (non-interactive)

Preflight: macOS on Apple Silicon (`uname -s` = `Darwin`, `uname -m` =
`arm64`) and Homebrew on PATH. Then:

```bash
curl -fsSL https://github.com/heyito/moo/releases/latest/download/install.sh | sh
```

Success contract: the installer exits 0 and ends by running `moo doctor`
(four checks, exit 0). Smoke test — prints `ok`:

```bash
moo new smoke && moo run smoke -- echo ok && moo drop smoke --force
```

From source instead (additionally requires Rust): clone
`https://github.com/heyito/moo`, run `scripts/install.sh`.

## Commands

```
moo new <name> [from <src>] [--detached]   create or restore a machine (idempotent)
moo run <name> -- <cmd> [args...]          execute inside (docker-exec semantics)
moo save [<name>]                          snapshot, tagged with the current commit
moo drop <name> [--force] [--snapshots]    destroy machine (snapshots survive)

moo ls                                     machines, host->guest port map, snapshots
moo open <name> [guest-port] [/path]       print + open host URL for a forwarded port
moo doctor                                 host checks (exit 0 = ready)
```

`<src>` for `new` can be a git ref or SHA, a snapshot ID, or another
machine's name (sub-second copy-on-write fork).

## Rules

- Run everything runtime-related (migrations, seeds, servers, tests,
  package installs) inside the machine via `moo run`. Edit code on the
  host; the working tree auto-syncs into the machine at `/srv/app` (or
  `[project] workdir` from `moo.toml`) on every `new`/`run` from inside
  the repo.
- `moo save` after each meaningful commit, and **always before
  `git checkout`** — `moo new` on an existing handle prefers the snapshot
  for the current commit over unsaved live state. Saves are idempotent
  and deduplicated; saving often is free.
- After `git checkout`, run `moo new <name>` to boot the runtime saved
  for that commit. `git reset --hard` / `git rebase` do not auto-restore;
  run `moo new <name>` afterwards.
- Services started inside a machine keep running between `moo run` calls;
  start them with `nohup … &`. To be reachable from the host they must
  listen on `0.0.0.0`; find the host port with `moo ls` or `moo open`.
- Parallel attempts: machines isolate the runtime, **not the files** —
  the host working tree syncs in and is authoritative. Every parallel
  session needs its own `git worktree` (work from inside it) plus its
  own machine: `git worktree add ../repo-attempt-1 -b attempt-1`, then
  `moo new attempt-1 from <base>`. Promote the winner with `git merge`,
  then `moo drop` the losers. Never run two sessions against the same
  checkout.

Full workflow skill (installable in Claude Code / Cursor):
[skills/moo-code/SKILL.md](skills/moo-code/SKILL.md). Human-oriented
docs: [README.md](README.md).
