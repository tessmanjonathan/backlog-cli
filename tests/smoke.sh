#!/usr/bin/env bash
# End-to-end smoke test for the central store. Runs against a throwaway
# BL_HOME and throwaway repositories; never touches ~/.bl.
set -eu
SRC=$(cd "$(dirname "$0")/.." && pwd)
BL=${BL:-$SRC/target/release/bl}
T=$(mktemp -d)
export BL_HOME="$T/home" BL_NO_AUTOEXPORT=1
unset BL_DB BL_PROJECT
fail() { echo "FAIL: $*" >&2; exit 1; }
pass=0; ok() { pass=$((pass+1)); echo "ok  $*"; }

mkrepo() { mkdir -p "$1"; git -C "$1" init -q; git -C "$1" -c user.email=t@t -c user.name=t commit -q --allow-empty -m "root"; }
mkrepo "$T/alpha"; mkrepo "$T/beta"

# 1. nothing exists: no command may create a database
cd "$T/alpha"
if $BL list 2>/dev/null; then fail "list minted a db"; fi
[ ! -e "$T/alpha/backlog.db" ] || fail "stray backlog.db minted"
[ ! -e "$BL_HOME/backlog.db" ] || fail "central db minted by list"
ok "no silent mint"

# 2. init creates the store and registers this repo
$BL init | grep -q "registered project 'alpha'" || fail "init did not register alpha"
[ -f "$BL_HOME/config.yml" ] || fail "no config written"
grep -q "^db: $BL_HOME/backlog.db" "$BL_HOME/config.yml" || fail "config db line"
ok "init + config"

# 3. create is scoped to alpha; from a worktree it resolves to alpha too
id=$($BL create "alpha one" -p 9000 | grep -o '#[0-9]*' | tr -d '#')
git -C "$T/alpha" worktree add -q "$T/alpha/.wt/lane" -b lane
cd "$T/alpha/.wt/lane"
$BL project current | grep -q " alpha " || fail "worktree did not resolve to alpha"
$BL create "alpha two from worktree" -p 8000 >/dev/null
[ ! -e backlog.db ] || fail "worktree minted a db"
ok "worktree resolves to main repo"

# 4. commit linking uses the project checkout
git -c user.email=t@t -c user.name=t commit -q --allow-empty -m "lane work"
$BL note "$id" "linked from lane" --commit | grep -q "commit" || fail "no commit linked"
ok "commit resolves from worktree"

# 5. second project; scoping keeps boards apart; outside dirs see all
cd "$T/beta"; $BL project add >/dev/null
$BL create "beta one" -p 9500 >/dev/null
[ "$($BL list --json | grep -c '"title"')" = 1 ] || fail "beta sees alpha cards"
cd "$T/alpha"
[ "$($BL list --json | grep -c '"title"')" = 2 ] || fail "alpha count"
[ "$($BL list --all --json | grep -c '"title"')" = 3 ] || fail "--all count"
cd "$T"
[ "$($BL list --json | grep -c '"title"')" = 3 ] || fail "outside sees all active"
$BL next | grep -q "beta: " || fail "next outside should pick beta 9500 with project prefix"
ok "scope: project / --all / outside"

# 6. deactivate hides beta from next and default views
$BL project deactivate beta >/dev/null
$BL next | grep -q "alpha one" || fail "deactivated beta still picked"
$BL --project beta list | grep -q "beta one" || fail "--project beta should still read it"
$BL project activate beta >/dev/null
ok "deactivate / activate"

# 7. create outside any project is refused; --project fixes it
cd "$T"
if $BL create "nowhere" 2>/dev/null; then fail "create without project"; fi
$BL --project alpha create "via flag" >/dev/null
ok "create needs a project"

# 8. legacy --db file: migration adds a project row named after the directory
mkdir -p "$T/legacy"
# A database from before projects existed: the pre-0.5 schema, made by hand so
# the test does not depend on whatever state the repo's own board is in.
sqlite3 "$T/legacy/backlog.db" <<'SQL'
CREATE TABLE cards (
    id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, notes TEXT NOT NULL DEFAULT '',
    label TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'new'
        CHECK(status IN ('new','ready','in_progress','done')),
    priority INTEGER NOT NULL DEFAULT 5000 CHECK(priority BETWEEN 0 AND 10000),
    outcome TEXT NOT NULL DEFAULT '', claimed_by TEXT NOT NULL DEFAULT '', claimed_at TEXT NOT NULL DEFAULT '',
    commits TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')));
INSERT INTO cards (title, notes, priority) VALUES ('old card', 'one line of notes', 7000);
SQL
$BL --db "$T/legacy/backlog.db" list | grep -q "old card" || fail "legacy cards unreadable after migration"
$BL --db "$T/legacy/backlog.db" project list | grep -q "legacy" || fail "legacy project row"
$BL --db "$T/legacy/backlog.db" create "legacy card" >/dev/null
$BL --db "$T/legacy/backlog.db" list | grep -q "legacy card" || fail "legacy create"
$BL --db "$T/legacy/backlog.db" notes 1 | grep -q "one line of notes" || fail "legacy notes adopted"
ok "legacy --db migration"

# 9. unregistered repo with its own backlog.db keeps using it, with a hint
mkrepo "$T/gamma"; cd "$T/gamma"
$BL init --db ./backlog.db >/dev/null
$BL create "gamma local" >/dev/null 2>&1
$BL list 2>&1 | grep -q "using ./backlog.db" || fail "no fallback hint"
$BL list 2>/dev/null | grep -q "gamma local" || fail "fallback did not read local db"
ok "unregistered repo falls back to ./backlog.db"

# 10. board and export render for a project and for the store
cd "$T/alpha"; $BL board --no-color | grep -q "alpha" || fail "board title"
$BL export -o "$T/alpha.html" >/dev/null; grep -q "alpha one" "$T/alpha.html" || fail "export content"
cd "$T"; $BL export -o "$T/all.html" >/dev/null; grep -q "beta one" "$T/all.html" || fail "store export"
ok "board / export"

# 11. per-project auto-export
cd "$T/alpha"; unset BL_NO_AUTOEXPORT
$BL auto on -o "$T/alpha/view/index.html" | grep -q "project alpha" || fail "auto on scope"
$BL create "refresh me" >/dev/null
grep -q "refresh me" "$T/alpha/view/index.html" || fail "auto export not refreshed"
export BL_NO_AUTOEXPORT=1
ok "per-project auto export"

# 12. import a repo-level db into the store: new ids, legacy ids kept, idempotent
cd "$T/legacy"
git init -q .
out=$($BL import "$T/legacy/backlog.db")
echo "$out" | grep -q "2 card(s) imported" || fail "import count: $out"
new=$(echo "$out" | grep -o '#1 → #[0-9]*' | grep -o '[0-9]*$')
$BL show "$new" | grep -q "imported: was #1" || fail "legacy id not shown"
$BL notes "$new" | grep -q "one line of notes" || fail "legacy notes not imported"
$BL project current | grep -q " legacy " || fail "legacy dir not registered by import"
$BL import "$T/legacy/backlog.db" | grep -q "0 card(s) imported, 0 note(s), 2 already present" || fail "import not idempotent"
ok "import with legacy ids, idempotent"

# 13. migrate scans a directory of repos; gamma's local db comes in
$BL migrate --scan "$T" --dry-run | grep -q "would import.*gamma" || fail "migrate dry-run"
$BL migrate --scan "$T" | grep -q "gamma.*1 card(s) imported" || fail "migrate gamma"
cd "$T/gamma"; rm backlog.db
$BL list | grep -q "gamma local" || fail "gamma not served from the store after migrate"
ok "migrate --scan"

# 14. the store-wide page carries the project list and marks the store; a scoped page names its project
cd "$T"; $BL export -o "$T/all.html" >/dev/null
grep -q '"projects":\[' "$T/all.html" || fail "store page has no projects list"
grep -q '"central":true' "$T/all.html" || fail "store page not marked central"
grep -q 'id="projectpick"' "$T/all.html" || fail "no project picker in page"
cd "$T/alpha"; $BL export -o "$T/alpha2.html" >/dev/null
grep -q '"project":{"id":[0-9]*,"name":"alpha"' "$T/alpha2.html" || fail "scoped page not scoped to alpha"
ok "page carries projects"

# 15. edit and retitle: several fields in one go, notes replaced, move between projects
cd "$T/alpha"
eid=$($BL create "typo in tilte" -l ui -p 4000 | grep -o '#[0-9]*' | tr -d '#')
$BL retitle "$eid" "typo in title, fixed" | grep -q "retitled" || fail "retitle"
$BL show "$eid" | grep -q "typo in title, fixed" || fail "retitle not applied"
$BL edit "$eid" --label polish --priority 6500 --status ready | grep -q "label, priority, status" || fail "edit fields"
$BL show "$eid" | grep -q "\[ 6500\]  ready         alpha: \[polish\]" || fail "edit not applied: $($BL show "$eid" | head -1)"
$BL note "$eid" "first note" >/dev/null
$BL edit "$eid" --notes "replaced line one
replaced line two" 2>/dev/null | grep -q "notes" || fail "edit --notes"
[ "$($BL notes "$eid" | grep -c 'replaced line')" = 2 ] || fail "notes not replaced as rows"
$BL notes "$eid" | grep -q "first note" && fail "old note survived the replace"
if $BL edit "$eid" 2>/dev/null; then fail "edit with no fields should refuse"; fi
$BL edit "$eid" --move beta | grep -q "project → beta" || fail "move"
cd "$T/beta"; $BL list | grep -q "typo in title, fixed" || fail "card not in beta after move"
cd "$T/alpha"; $BL list | grep -q "typo in title, fixed" && fail "card still in alpha after move"
$BL edit "$eid" --title "" 2>/dev/null && fail "empty title accepted"
ok "edit / retitle / move"

# 16. event log: every status/claim/priority/field change lands in bl history with who did it
cd "$T/alpha"
hid=$($BL create "audited card" -p 3000 --by minter | grep -o '#[0-9]*' | tr -d '#')
$BL claim "$hid" --by worker >/dev/null
$BL release "$hid" --by worker >/dev/null
$BL set-priority "$hid" 3500 --by ranker >/dev/null
$BL edit "$hid" --title "audited card, renamed" --label audit --by editor >/dev/null
$BL status "$hid" done --outcome "shipped" --by closer >/dev/null
h=$($BL history "$hid")
echo "$h" | grep -q "created .*→ audited card  by minter" || fail "created event: $h"
echo "$h" | grep -q "claim .*→ worker  by worker" || fail "claim event: $h"
echo "$h" | grep -q "status .*new → in_progress  by worker" || fail "claim status event: $h"
echo "$h" | grep -q "release .*worker →  by worker" || fail "release event: $h"
echo "$h" | grep -q "priority .*3000 → 3500  by ranker" || fail "priority event: $h"
echo "$h" | grep -q "title .*audited card → audited card, renamed  by editor" || fail "title event: $h"
echo "$h" | grep -q "label .*→ audit  by editor" || fail "label event: $h"
echo "$h" | grep -q "status .*ready → done  by closer" || fail "done event: $h"
echo "$h" | grep -q "outcome .*→ shipped  by closer" || fail "outcome event: $h"
[ "$($BL history "$hid" --json | grep -c '"kind"')" = 10 ] || fail "history --json count: $($BL history "$hid" --json | grep -c '"kind"')"
nid=$($BL next --claim --by looper | grep -o '#[0-9]*' | head -1 | tr -d '#')
$BL history "$nid" | grep -q "claim .*→ looper" || fail "next --claim not logged"
$BL release "$nid" >/dev/null
$BL export -o "$T/hist.html" >/dev/null; grep -q '"events":\[' "$T/hist.html" || fail "page carries no events"
if $BL history 99999 2>/dev/null; then fail "history of a missing card should fail"; fi
ok "event log / bl history"

echo "all $pass checks passed  ($T)"
rm -rf "$T"
