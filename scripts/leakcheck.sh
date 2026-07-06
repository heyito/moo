#!/bin/bash
# Backend-leak gate (plan.md §13): no hypervisor names may appear in any
# user-facing output. Runs every CLI surface and greps stdout+stderr.
set -uo pipefail

MOO="${1:-target/release/moo}"
PATTERN='libkrun|krunfw|krunkit|gvproxy|vfkit|firecracker|cloud.hypervisor|apple.vz|hvf'

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

check "usage"        "$MOO"
check "doctor"       "$MOO" doctor
check "ls"           "$MOO" ls
check "run-missing"  "$MOO" run no-such-machine -- true
check "drop-missing" "$MOO" drop no-such-machine
check "new-badsrc"   "$MOO" new leaktest from not-a-real-source
check "open-noargs"  "$MOO" open
check "open-missing" "$MOO" open no-such-machine

if [ "$fail" -eq 0 ]; then
    echo "leakcheck: PASS (no backend names in user-facing output)"
else
    echo "leakcheck: FAIL"
    exit 1
fi
