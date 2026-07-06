# Contributing to moo

Thanks for looking under the hood. `moo` is small on purpose — one noun,
four verbs — and contributions are judged first on whether they preserve
that shape (see [vision.md](vision.md), especially the design principles).

## Build

macOS Apple Silicon, [Homebrew](https://brew.sh), [Rust](https://rustup.rs):

```bash
git clone https://github.com/heyito/moo && cd moo
scripts/install.sh          # deps + release build + sign + moo doctor
```

For iteration, `cargo build --release` rebuilds the CLI (with the guest
agent embedded). The binary must be codesigned with the hypervisor
entitlement to boot machines; `scripts/install.sh` does this, or run
`codesign --force --sign - --entitlements crates/moo-cli/entitlements.plist
target/release/moo` after a rebuild.

## The acceptance suite: three demos

There is no unit-test suite yet; the demos are the acceptance tests. Each
is self-contained, self-verifying, and cleans up after itself. **PRs must
keep all three green**, run locally before submitting (they boot real
machines, so they cannot run on GitHub-hosted CI runners):

```bash
scripts/demo-timetravel.sh   # runtime follows git checkout, survives drop
scripts/demo-parallel.sh     # 3 machines, same guest port, zero collisions
scripts/demo-bisect.sh       # git bisect over runtime state, unattended
```

## The leakcheck gate

The isolation backend is an implementation detail and must never appear
in user-facing output — not in errors, not in help text, not in hints.
CI enforces this:

```bash
scripts/leakcheck.sh
```

If you add a command or error path, add a case to `leakcheck.sh`.

## CI

Every PR runs `cargo build`, `cargo clippy -- -D warnings`,
`cargo fmt --check`, and `scripts/leakcheck.sh` on a macOS arm64 runner.

## Ground rules

- **Verb discipline.** New primitive verbs are effectively frozen: if a
  workflow can be composed from `new`/`run`/`save`/`drop` plus git and a
  shell loop, it should not become a verb. Admin conveniences (`ls`,
  `open`, `doctor`) are read-only.
- **The backend never leaks.** Hypervisor and firmware names stay out of
  commands, config, and all output.
- Keep dependencies minimal — the CLI parses arguments by hand for a
  reason.
