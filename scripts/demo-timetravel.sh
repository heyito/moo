#!/bin/bash
# Demo 1 (mvp-plan.md §2): the differentiator. Runtime state saved per
# commit, restored by git checkout — including across a full drop.
set -euo pipefail

GOT="${GOT:-got}"
DEMO_DIR="${DEMO_DIR:-/tmp/got-timetravel-demo}"
MACHINE="feat/billing"

say() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }
state() { "$GOT" run "$MACHINE" -- cat /db.txt; }

say "Setup"
"$GOT" drop "$MACHINE" --snapshots >/dev/null 2>&1 || true
rm -rf "$DEMO_DIR"
mkdir -p "$DEMO_DIR"
cd "$DEMO_DIR"
git init -q -b main
git commit -q --allow-empty -m "base"

say "Two commits, two saved runtimes"
"$GOT" new "$MACHINE"
"$GOT" run "$MACHINE" -- 'echo migration-1 >> /db.txt && sync'
git commit -q --allow-empty -m "add migration 1"
"$GOT" save "$MACHINE"

"$GOT" run "$MACHINE" -- 'echo migration-2 >> /db.txt && sync'
git commit -q --allow-empty -m "add migration 2"
"$GOT" save "$MACHINE"

say "Rewind: git checkout HEAD^ — the machine follows"
git checkout -q HEAD^
"$GOT" new "$MACHINE" 2>/dev/null
echo "state at HEAD^:"
state
[ "$(state)" = "migration-1" ] || { echo "DEMO: FAIL (migration-2 still present)"; exit 1; }

say "Forward: git checkout main — the machine follows back"
git checkout -q main
"$GOT" new "$MACHINE" 2>/dev/null
echo "state at main:"
state
[ "$(state)" = "migration-1
migration-2" ] || { echo "DEMO: FAIL (migration-2 missing)"; exit 1; }

say "Drop the machine entirely — snapshots survive"
"$GOT" drop "$MACHINE"
"$GOT" new "$MACHINE"
echo "state after drop + new:"
state
[ "$(state)" = "migration-1
migration-2" ] || { echo "DEMO: FAIL (state lost across drop)"; exit 1; }

"$GOT" drop "$MACHINE" --snapshots >/dev/null
rm -rf "$DEMO_DIR"
echo
echo "DEMO: PASS — the runtime followed git checkout in both directions,"
echo "and survived a full machine drop."
