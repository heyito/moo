#!/bin/bash
# Demo 1 (mvp-plan.md §2): the differentiator. Runtime state saved per
# commit, restored by git checkout — including across a full drop.
set -euo pipefail

MOO="${MOO:-moo}"
DEMO_DIR="${DEMO_DIR:-/tmp/moo-timetravel-demo}"
MACHINE="feat/billing"

say() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }
state() { "$MOO" run "$MACHINE" -- cat /db.txt; }

say "Setup"
"$MOO" drop "$MACHINE" --snapshots >/dev/null 2>&1 || true
rm -rf "$DEMO_DIR"
mkdir -p "$DEMO_DIR"
cd "$DEMO_DIR"
git init -q -b main
git commit -q --allow-empty -m "base"

say "Two commits, two saved runtimes"
"$MOO" new "$MACHINE"
"$MOO" run "$MACHINE" -- 'echo migration-1 >> /db.txt && sync'
git commit -q --allow-empty -m "add migration 1"
"$MOO" save "$MACHINE"

"$MOO" run "$MACHINE" -- 'echo migration-2 >> /db.txt && sync'
git commit -q --allow-empty -m "add migration 2"
"$MOO" save "$MACHINE"

say "Rewind: git checkout HEAD^ — the machine follows"
git checkout -q HEAD^
"$MOO" new "$MACHINE" 2>/dev/null
echo "state at HEAD^:"
state
[ "$(state)" = "migration-1" ] || { echo "DEMO: FAIL (migration-2 still present)"; exit 1; }

say "Forward: git checkout main — the machine follows back"
git checkout -q main
"$MOO" new "$MACHINE" 2>/dev/null
echo "state at main:"
state
[ "$(state)" = "migration-1
migration-2" ] || { echo "DEMO: FAIL (migration-2 missing)"; exit 1; }

say "Drop the machine entirely — snapshots survive"
"$MOO" drop "$MACHINE"
"$MOO" new "$MACHINE"
echo "state after drop + new:"
state
[ "$(state)" = "migration-1
migration-2" ] || { echo "DEMO: FAIL (state lost across drop)"; exit 1; }

"$MOO" drop "$MACHINE" --snapshots >/dev/null
rm -rf "$DEMO_DIR"
echo
echo "DEMO: PASS — the runtime followed git checkout in both directions,"
echo "and survived a full machine drop."
