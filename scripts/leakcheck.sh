#!/bin/bash
# Backend-leak gate (plan.md §13): no hypervisor names may appear in any
# user-facing output. Runs every CLI surface and greps stdout+stderr.
set -uo pipefail

GOT="${1:-target/release/got}"
PATTERN='libkrun|krunfw|krunkit|firecracker|cloud.hypervisor|apple.vz|hvf'

fail=0
check() {
    local label="$1"; shift
    local out
    out=$("$@" 2>&1)
    if echo "$out" | grep -qiE "$PATTERN"; then
        echo "LEAK in $label:"
        echo "$out" | grep -iE "$PATTERN"
        fail=1
    fi
}

check "usage"        "$GOT"
check "doctor"       "$GOT" doctor
check "ls"           "$GOT" ls
check "run-missing"  "$GOT" run no-such-machine -- true
check "drop-missing" "$GOT" drop no-such-machine
check "new-badsrc"   "$GOT" new leaktest from not-a-real-source

if [ "$fail" -eq 0 ]; then
    echo "leakcheck: PASS (no backend names in user-facing output)"
else
    echo "leakcheck: FAIL"
    exit 1
fi
