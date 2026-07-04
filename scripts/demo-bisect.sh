#!/bin/bash
# The headline demo (mvp-plan.md §2, Demo 3): a bug that only reproduces
# against a specific database migration state, found unattended by
# `git bisect run` booting each commit's saved runtime.
#
# What it proves: `moo save` at commit boundaries makes runtime state a
# versioned artifact of the repo. No fixtures, no re-seeding — bisect
# boots the exact database that existed at every commit it probes.
set -euo pipefail

MOO="${MOO:-moo}"
DEMO_DIR="${DEMO_DIR:-/tmp/moo-bisect-demo}"
MACHINE="billing"
EXPECTED_TOTAL=6000   # cents; the sum a healthy database always reports

say() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

sql() { "$MOO" run "$MACHINE" -- sqlite3 /srv/app.db "$1"; }

say "Setup: fresh repo and machine"
"$MOO" drop "$MACHINE" --snapshots >/dev/null 2>&1 || true
rm -rf "$DEMO_DIR"
mkdir -p "$DEMO_DIR"
cd "$DEMO_DIR"
git init -q -b main
git commit -q --allow-empty -m "init"

"$MOO" new "$MACHINE"
"$MOO" run "$MACHINE" -- 'apt-get update -q >/dev/null 2>&1 && apt-get install -y -q sqlite3 >/dev/null 2>&1 && mkdir -p /srv && echo tools ready'

say "Building history: 8 migrations, each committed and saved"
migrate() { # migrate <n> <description> <sql>
    local n="$1" desc="$2" stmt="$3"
    mkdir -p migrations
    echo "$stmt" > "migrations/$(printf '%03d' "$n").sql"
    sql "$stmt" >/dev/null
    "$MOO" run "$MACHINE" -- sync >/dev/null
    git add -A
    git commit -q -m "migration $n: $desc"
    "$MOO" save "$MACHINE" >/dev/null
    echo "  [$n] $desc  ($(git rev-parse --short HEAD))"
}

migrate 1 "create products table" \
    "CREATE TABLE products (id INTEGER PRIMARY KEY, name TEXT, price_cents INTEGER);"
migrate 2 "seed catalog" \
    "INSERT INTO products (name, price_cents) VALUES ('widget', 1000), ('gadget', 2000), ('doodad', 3000);"
migrate 3 "add sku column" \
    "ALTER TABLE products ADD COLUMN sku TEXT;"
migrate 4 "backfill skus" \
    "UPDATE products SET sku = 'SKU-' || id;"
migrate 5 "normalize prices" \
    "UPDATE products SET price_cents = price_cents / 100;"   # THE BUG: treats cents as dollars
migrate 6 "add stock column" \
    "ALTER TABLE products ADD COLUMN stock INTEGER DEFAULT 0;"
migrate 7 "receive inventory" \
    "UPDATE products SET stock = 50;"
migrate 8 "add index" \
    "CREATE INDEX idx_products_sku ON products(sku);"

FIRST_GOOD=$(git rev-parse HEAD~6)   # migration 2 (seeded, healthy)
echo
echo "The catalog should total ${EXPECTED_TOTAL} cents. It reports: $(sql 'SELECT SUM(price_cents) FROM products;')"

say "git bisect run, with the runtime restored at every probe"
cat > /tmp/moo-bisect-test.sh <<EOF
#!/bin/bash
# Boot this commit's saved runtime, then check the database invariant.
$MOO new $MACHINE >/dev/null 2>&1
actual=\$($MOO run $MACHINE -- sqlite3 /srv/app.db "SELECT SUM(price_cents) FROM products;")
echo "  probe \$(git rev-parse --short HEAD): total=\$actual"
[ "\$actual" = "$EXPECTED_TOTAL" ]
EOF
chmod +x /tmp/moo-bisect-test.sh

git bisect start HEAD "$FIRST_GOOD" >/dev/null
git bisect run /tmp/moo-bisect-test.sh > /tmp/moo-bisect-out.txt 2>&1 || true
grep '  probe' /tmp/moo-bisect-out.txt || true
CULPRIT=$(git rev-parse refs/bisect/bad)
git bisect reset >/dev/null

say "Verdict"
echo "bisect found:  $(git log -1 --format='%h %s' "$CULPRIT")"
if git log -1 --format='%s' "$CULPRIT" | grep -q 'migration 5'; then
    echo "DEMO: PASS — the culprit is the price-normalization migration."
    echo "Each probe booted that commit's saved database. No fixtures, no reseeding."
else
    echo "DEMO: FAIL — expected migration 5 to be identified."
    exit 1
fi

"$MOO" drop "$MACHINE" --snapshots >/dev/null
rm -rf "$DEMO_DIR" /tmp/moo-bisect-test.sh /tmp/moo-bisect-out.txt
