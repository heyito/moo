#!/bin/bash
# Demo 2: the five-tool stack replaced. N machines run the
# same service on the same guest port with their own state — no port-offset
# scripts, no .env symlinks, no DB-per-branch tool, no compose hacks.
set -euo pipefail

MOO="${MOO:-moo}"
DEMO_DIR="${DEMO_DIR:-/tmp/moo-parallel-demo}"
AGENTS=(agent-a agent-b agent-c)

say() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

say "Setup: one repo, one moo.toml, port 8080 declared"
for m in "${AGENTS[@]}"; do "$MOO" drop "$m" --snapshots >/dev/null 2>&1 || true; done
rm -rf "$DEMO_DIR"
mkdir -p "$DEMO_DIR"
cd "$DEMO_DIR"
git init -q -b main
printf '[network]\nports = [8080]\n' > moo.toml
git add -A && git commit -q -m "project config"

say "Three parallel machines, one command each"
for m in "${AGENTS[@]}"; do
    "$MOO" new "$m" from HEAD
done

say "Each machine: own state, same service, same guest port"
for m in "${AGENTS[@]}"; do
    "$MOO" run "$m" -- "echo $m > /identity && echo 'CREATE TABLE work(id INTEGER);' > /migration-by-$m.sql"
    "$MOO" run "$m" -- 'nohup perl -MIO::Socket::INET -e "open(F,q{</identity}); \$id=<F>; chomp \$id; \$s=IO::Socket::INET->new(LocalPort=>8080,Listen=>5,ReuseAddr=>1) or die; while(1){\$c=\$s->accept() or next; \$c->autoflush(1); while(<\$c>){last if /^\r?\$/} \$b=\$id.qq{\n}; print \$c qq{HTTP/1.1 200 OK\r\nContent-Length: }.length(\$b).qq{\r\nConnection: close\r\n\r\n}.\$b; close \$c}" >/dev/null 2>&1 & sleep 0.2; echo "server up"'
done

say "The port map (no EADDRINUSE, no offset script)"
"$MOO" ls

say "Every agent answers on its own stable host port"
sleep 1
pass=true
for m in "${AGENTS[@]}"; do
    # `moo open` resolves this repo's machine to its host URL; with stdout
    # captured it prints without launching a browser.
    url=$("$MOO" open "$m" 8080)
    port=${url%/}; port=${port##*:}
    body=$(curl -s --max-time 5 "http://127.0.0.1:${port}/")
    echo "  $m  localhost:$port  ->  \"$body\""
    [ "$body" = "$m" ] || pass=false
done

say "And their filesystems never touched each other"
for m in "${AGENTS[@]}"; do
    count=$("$MOO" run "$m" -- 'ls /migration-by-* | wc -l')
    echo "  $m sees $(echo "$count" | tr -d ' ') migration file(s)"
    [ "$(echo "$count" | tr -d ' ')" = "1" ] || pass=false
done

say "Keep the winner, drop the losers"
"$MOO" save agent-b
"$MOO" drop agent-a
"$MOO" drop agent-c

if $pass; then
    echo
    echo "DEMO: PASS — three isolated runtimes, one tool, zero collisions."
else
    echo
    echo "DEMO: FAIL"
    exit 1
fi

"$MOO" drop agent-b --snapshots >/dev/null
rm -rf "$DEMO_DIR"
