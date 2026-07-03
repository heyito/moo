#!/bin/bash
# Demo 2 (mvp-plan.md §2): the five-tool stack replaced. N machines run the
# same service on the same guest port with their own state — no port-offset
# scripts, no .env symlinks, no DB-per-branch tool, no compose hacks.
set -euo pipefail

GOT="${GOT:-got}"
DEMO_DIR="${DEMO_DIR:-/tmp/got-parallel-demo}"
AGENTS=(agent-a agent-b agent-c)

say() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

say "Setup: one repo, one got.toml, port 8080 declared"
for m in "${AGENTS[@]}"; do "$GOT" drop "$m" --snapshots >/dev/null 2>&1 || true; done
rm -rf "$DEMO_DIR"
mkdir -p "$DEMO_DIR"
cd "$DEMO_DIR"
git init -q -b main
printf '[network]\nports = [8080]\n' > got.toml
git add -A && git commit -q -m "project config"

say "Three parallel machines, one command each"
for m in "${AGENTS[@]}"; do
    "$GOT" new "$m" from HEAD
done

say "Each machine: own state, same service, same guest port"
for m in "${AGENTS[@]}"; do
    "$GOT" run "$m" -- "echo $m > /identity && echo 'CREATE TABLE work(id INTEGER);' > /migration-by-$m.sql"
    "$GOT" run "$m" -- 'nohup perl -MIO::Socket::INET -e "open(F,q{</identity}); \$id=<F>; chomp \$id; \$s=IO::Socket::INET->new(LocalPort=>8080,Listen=>5,ReuseAddr=>1) or die; while(1){\$c=\$s->accept() or next; \$c->autoflush(1); while(<\$c>){last if /^\r?\$/} \$b=\$id.qq{\n}; print \$c qq{HTTP/1.1 200 OK\r\nContent-Length: }.length(\$b).qq{\r\nConnection: close\r\n\r\n}.\$b; close \$c}" >/dev/null 2>&1 & sleep 0.2; echo "server up"'
done

say "The port map (no EADDRINUSE, no offset script)"
"$GOT" ls

say "Every agent answers on its own stable host port"
sleep 1
pass=true
for m in "${AGENTS[@]}"; do
    port=$("$GOT" ls | awk -v m="$m" '$1 == m { split($4, p, "->"); print p[1] }')
    body=$(curl -s --max-time 5 "http://127.0.0.1:${port}/")
    echo "  $m  localhost:$port  ->  \"$body\""
    [ "$body" = "$m" ] || pass=false
done

say "And their filesystems never touched each other"
for m in "${AGENTS[@]}"; do
    count=$("$GOT" run "$m" -- 'ls /migration-by-* | wc -l')
    echo "  $m sees $(echo "$count" | tr -d ' ') migration file(s)"
    [ "$(echo "$count" | tr -d ' ')" = "1" ] || pass=false
done

say "Keep the winner, drop the losers"
"$GOT" save agent-b
"$GOT" drop agent-a
"$GOT" drop agent-c

if $pass; then
    echo
    echo "DEMO: PASS — three isolated runtimes, one tool, zero collisions."
else
    echo
    echo "DEMO: FAIL"
    exit 1
fi

"$GOT" drop agent-b --snapshots >/dev/null
rm -rf "$DEMO_DIR"
