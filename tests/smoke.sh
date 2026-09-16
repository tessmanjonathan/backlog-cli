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
INSERT INTO cards (title, label, priority) VALUES ('space labelled', 'art enemies c676', 6000);
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
echo "$out" | grep -q "3 card(s) imported" || fail "import count: $out"
new=$(echo "$out" | grep -o '#1 → #[0-9]*' | grep -o '[0-9]*$')
$BL show "$new" | grep -q "imported: was #1" || fail "legacy id not shown"
$BL notes "$new" | grep -q "one line of notes" || fail "legacy notes not imported"
$BL project current | grep -q " legacy " || fail "legacy dir not registered by import"
$BL import "$T/legacy/backlog.db" | grep -q "0 card(s) imported, 0 note(s), 3 already present" || fail "import not idempotent"
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

# 17. delete: notes travel into the deleted event; a claimed card needs --force
cd "$T/alpha"
did=$($BL create "filed by mistake" -l oops | grep -o '#[0-9]*' | tr -d '#')
$BL note "$did" "this note must survive in history" -k finding --by noter >/dev/null
$BL claim "$did" --by holder >/dev/null
if $BL delete "$did" 2>/dev/null; then fail "delete of a claimed card without --force"; fi
$BL show "$did" >/dev/null || fail "refused delete still removed the card"
$BL delete "$did" --force --why "duplicate of #1" --by janitor | grep -q "deleted: filed by mistake" || fail "delete output"
if $BL show "$did" 2>/dev/null; then fail "card still there after delete"; fi
h=$($BL history "$did")
echo "$h" | grep -q "deleted .*title: filed by mistake" || fail "deleted event lacks title: $h"
echo "$h" | grep -q "this note must survive in history" || fail "deleted event lacks notes: $h"
echo "$h" | grep -q "→ duplicate of #1  by janitor" || fail "deleted event lacks why/by: $h"
[ "$($BL history "$did" --json | grep -c '"kind": "deleted"')" = 1 ] || fail "deleted event count"
ok "delete keeps history"

# 18. bulk edit: --ids, --where/--set, --dry-run writes nothing, each card gets an event
cd "$T/alpha"
b1=$($BL create "bulk a" -l art -p 1000 | grep -o '#[0-9]*' | tr -d '#')
b2=$($BL create "bulk b" -l art -p 1100 | grep -o '#[0-9]*' | tr -d '#')
b3=$($BL create "bulk c" -l code -p 1200 | grep -o '#[0-9]*' | tr -d '#')
$BL edit --ids "$b1,$b2" --priority 100 --by mover | grep -q "edited 2 card(s): priority" || fail "edit --ids"
$BL show "$b2" | grep -q "\[  100\]" || fail "--ids priority not applied"
$BL edit --where label=art --where status=new --set priority=200 --set label=visual --dry-run | grep -q "would edit 2 card(s); nothing written" || fail "dry-run count"
$BL show "$b1" | grep -q "\[art\]" || fail "dry-run wrote"
$BL edit --where label=art --where status=new --set priority=200 --set label=visual | grep -q "edited 2 card(s)" || fail "where/set"
$BL show "$b1" | grep -q "\[  200\]  new           alpha: \[visual\]" || fail "where/set not applied: $($BL show "$b1" | head -1)"
$BL show "$b3" | grep -q "\[code\]" || fail "where touched a non-matching card"
$BL edit --where "priority<150" --set priority=300 2>/dev/null && fail "priority<150 should match nothing now"
[ "$($BL edit --where label=nothing-here --set priority=1 >/dev/null 2>&1; echo $?)" = 2 ] || fail "no match should exit 2"
$BL history "$b1" | grep -q "label .*art → visual" || fail "bulk edit event missing: $($BL history "$b1")"
if $BL edit --set priority=1 2>/dev/null; then fail "edit with no target should refuse"; fi
ok "bulk edit"

# 19. note edit / rm by note id: mirror rebuilt, events written, plain bl note still works
cd "$T/alpha"
nc=$($BL create "note surgery" | grep -o '#[0-9]*' | tr -d '#')
$BL note "$nc" "first" >/dev/null; $BL note "$nc" "secnod, with typo" -k finding >/dev/null; $BL note "$nc" "third" >/dev/null
n2=$($BL notes "$nc" | grep -B1 "secnod" | grep -o '(note [0-9]*)' | grep -o '[0-9]*')
[ -n "$n2" ] || fail "bl notes prints no note id: $($BL notes "$nc")"
$BL note edit "$n2" "second, fixed" -k decision --by fixer | grep -q "note $n2 on #$nc edited" || fail "note edit"
$BL notes "$nc" | grep -q "^\[decision\].*(note $n2)" || fail "kind not changed"
$BL show "$nc" | grep -q "first | \[decision\] second, fixed | third" || fail "mirror not rebuilt after edit: $($BL show "$nc" | grep notes:)"
$BL note rm "$n2" --by fixer | grep -q "removed from #$nc" || fail "note rm"
$BL show "$nc" | grep -q "notes: first | third$" || fail "mirror not rebuilt after rm: $($BL show "$nc" | grep notes:)"
[ "$($BL notes "$nc" | grep -c '^\[')" = 2 ] || fail "note count after rm"
$BL history "$nc" | grep -q "note_edit .*secnod, with typo → second, fixed  by fixer" || fail "note_edit event"
$BL history "$nc" | grep -q "note_rm .*second, fixed →  by fixer" || fail "note_rm event: $($BL history "$nc")"
if $BL note rm 999999 2>/dev/null; then fail "rm of a missing note should fail"; fi
if $BL note "$nc" 2>/dev/null; then fail "bl note with no text should fail"; fi
ok "note edit / rm"

# 20. import --stdin: array or JSON lines, notes typed, --if-absent, atomic on a bad line
cd "$T/alpha"
out=$(printf '[{"title":"planned one","label":"plan","priority":6100,"notes":"from the planner"},{"title":"planned two","notes":[{"kind":"finding","body":"typed note"},"plain note"]}]' | $BL import --stdin --by planner 2>/dev/null)
[ "$(echo "$out" | grep -c '"created": true')" = 2 ] || fail "stdin array created count: $out"
p1=$(echo "$out" | grep -o '"id": [0-9]*' | head -1 | grep -o '[0-9]*')
$BL show "$p1" | grep -q "\[ 6100\]  new           alpha: \[plan\] planned one" || fail "stdin card fields: $($BL show "$p1" | head -1)"
$BL notes "$p1" | grep -q "planner" || fail "stdin note author"
p2=$(echo "$out" | grep -o '"id": [0-9]*' | tail -1 | grep -o '[0-9]*')
$BL notes "$p2" | grep -q "^\[finding\]" || fail "typed stdin note"
$BL history "$p1" | grep -q "created .*→ planned one  by planner" || fail "stdin created event"
out=$(printf '{"title":"planned one"}\n{"title":"planned three","project":"beta"}\n' | $BL import --stdin --if-absent 2>/dev/null)
echo "$out" | grep -q '"created": false' || fail "--if-absent did not report the existing card"
echo "$out" | grep -q '"project": "beta"' || fail "per-card project"
before=$($BL list --all --json | grep -c '"title"')
if printf '{"title":"ok"}\n{"title":""}\n' | $BL import --stdin >/dev/null 2>&1; then fail "empty title accepted"; fi
[ "$($BL list --all --json | grep -c '"title"')" = "$before" ] || fail "bad batch left cards behind"
printf '[{"title":"dry"}]' | $BL import --stdin --dry-run 2>/dev/null | grep -q '"created": false' || fail "dry run"
$BL search dry --open >/dev/null 2>&1 && fail "dry run wrote"
ok "import --stdin"

# 21. guards: long titles and outcomes are refused with a hint, --force overrides
cd "$T/alpha"
long=$(printf 'x%.0s' $(seq 1 121)); longer=$(printf 'y%.0s' $(seq 1 301))
$BL create "$long" 2>&1 | grep -q "title is 121 characters (limit 120).*note" || fail "long title accepted or wrong hint"
gid=$($BL create "$long" --force | grep -o '#[0-9]*' | tr -d '#') || fail "--force did not create"
if $BL retitle "$gid" "$long" 2>/dev/null; then fail "retitle long title accepted"; fi
if $BL edit "$gid" --title "$long" 2>/dev/null; then fail "edit long title accepted"; fi
$BL edit "$gid" --title "$long" --force >/dev/null || fail "edit --force"
$BL status "$gid" done --outcome "$longer" 2>&1 | grep -q "outcome is 301 characters (limit 300).*note" || fail "long outcome accepted"
$BL show "$gid" | grep -q "  in_progress\|  new " || fail "refused status still moved the card"
$BL status "$gid" done --outcome "$longer" --force | grep -q "done" || fail "status --force"
if $BL edit "$gid" --outcome "$longer" 2>/dev/null; then fail "edit long outcome accepted"; fi
if printf '{"title":"%s"}' "$long" | $BL import --stdin >/dev/null 2>&1; then fail "stdin long title accepted"; fi
$BL prompt | grep -q "over 120 characters" || fail "prompt does not state the limit"
ok "title / outcome guards"

# 22. note body from stdin or a file: quotes, $ and * arrive untouched
cd "$T/alpha"
sid=$($BL create "stdin notes" | grep -o '#[0-9]*' | tr -d '#')
printf "it's the \"\$PATH\" glob (*) case\nsecond line" | $BL note "$sid" --stdin -k finding --by piper | grep -q "note added" || fail "note --stdin"
$BL notes "$sid" | grep -q 'it'"'"'s the "$PATH" glob (\*) case' || fail "stdin body mangled: $($BL notes "$sid")"
$BL notes "$sid" | grep -q "^    second line" || fail "stdin second line lost"
printf 'from a file\n' > "$T/note.txt"
$BL note "$sid" -f "$T/note.txt" | grep -q "note added" || fail "note -f"
$BL notes "$sid" | grep -q "from a file" || fail "file body missing"
if printf '' | $BL note "$sid" --stdin 2>/dev/null; then fail "empty stdin note accepted"; fi
if $BL note "$sid" "text" --stdin 2>/dev/null; then fail "text and --stdin together should be refused"; fi
ok "note --stdin / -f"

# 23. tags: several per card, any-match filter everywhere, add/rm, legacy space labels split once
cd "$T/alpha"
t1=$($BL create "tagged one" -l "art,enemies" -p 2100 | grep -o '#[0-9]*' | tr -d '#')
t2=$($BL create "tagged two" -l "ui enemies" -p 2000 | grep -o '#[0-9]*' | tr -d '#')
$BL show "$t1" | grep -q "\[art,enemies\]" || fail "tags not normalized: $($BL show "$t1" | head -1)"
$BL show "$t2" | grep -q "\[ui,enemies\]" || fail "space input not normalized: $($BL show "$t2" | head -1)"
[ "$($BL list -l enemies --json | grep -c '"title": "tagged')" = 2 ] || fail "list -l tag any-match"
[ "$($BL list -l art --json | grep -c '"title": "tagged')" = 1 ] || fail "list -l art"
[ "$($BL list -l art,ui --json | grep -c '"title": "tagged')" = 2 ] || fail "list -l art,ui"
$BL next -l enemies | grep -q "tagged one" || fail "next -l tag"
$BL search tagged -l ui | grep -q "tagged two" || fail "search -l tag"
$BL board --no-color -l art | grep -q "tagged one" || fail "board -l tag"
$BL board --no-color -l art | grep -q "tagged two" && fail "board -l art matched a card without it"
$BL edit "$t1" --add-tag build --rm-tag art | grep -q "label" || fail "edit add/rm tag"
$BL show "$t1" | grep -q "\[enemies,build\]" || fail "add/rm result: $($BL show "$t1" | head -1)"
$BL edit --where label=enemies --set priority=2200 | grep -q "edited 2 card(s)" || fail "--where label= is tag match"
$BL history "$t1" | grep -q "label .*art,enemies → enemies,build" || fail "tag event"
$BL --db "$T/legacy/backlog.db" list -l enemies | grep -q "\[art,enemies,c676\] space labelled" || fail "legacy space label not split: $($BL --db "$T/legacy/backlog.db" list)"
$BL prompt | grep -q "Tags in use" || fail "prompt tag section"
ok "tags"

# 24. links: blocks keeps a card out of next until the blocker is done; child_of / related; unlink; cycles refused
cd "$T/alpha"
l1=$($BL create "dep first" -p 9800 | grep -o '#[0-9]*' | tr -d '#')
l2=$($BL create "dep second" -p 9900 | grep -o '#[0-9]*' | tr -d '#')
ep=$($BL create "dep epic" -p 100 | grep -o '#[0-9]*' | tr -d '#')
$BL next | grep -q "dep second" || fail "precondition: second is top"
$BL block "$l2" --on "$l1" --by planner | grep -q "linked: #$l1 blocks #$l2" || fail "block --on"
$BL next | grep -q "dep first" || fail "next did not skip the blocked card"
$BL show "$l2" | grep -q "blocked by: #$l1 (new) dep first" || fail "show lacks blocked by: $($BL show "$l2")"
$BL list | grep -q "blocks: #$l2 dep second" || fail "list lacks blocks line"
$BL show "$l2" --json | grep -q '"rel": "blocked_by"' || fail "json links"
if $BL link "$l2" --blocks "$l1" 2>/dev/null; then fail "cycle accepted"; fi
$BL link "$l1" --child-of "$ep" --related "$l2" >/dev/null || fail "child-of/related"
$BL show "$ep" | grep -q "children: #$l1" || fail "epic children"
$BL show "$l2" | grep -q "related: #$l1" || fail "related is symmetric"
$BL link "$l1" --child-of "$ep" | grep -q "already linked" || fail "duplicate link"
$BL history "$l2" | grep -q "link .*→ #$l1 blocks #$l2  by planner" || fail "link event: $($BL history "$l2")"
nid=$($BL next --claim --by dep | grep -o '#[0-9]*' | head -1 | tr -d '#'); [ "$nid" = "$l1" ] || fail "next --claim picked $nid not the blocker"
$BL status "$l1" done --outcome "done" >/dev/null
$BL next | grep -q "dep second" || fail "done blocker still blocks"
$BL export -o "$T/links.html" >/dev/null; grep -q '"links":\[' "$T/links.html" || fail "page carries no links"
$BL unlink "$l1" --related "$l2" | grep -q "unlinked" || fail "unlink"
$BL show "$l2" | grep -q "related:" && fail "related survived unlink"
$BL delete "$l1" --why test >/dev/null; $BL show "$l2" | grep -q "blocked by" && fail "link survived delete of the blocker"
ok "links / block"

# 25. blocked status: --on required, skipped by next/reap, reason shown, ready clears it; 0.4 db rebuilt with its notes intact
cd "$T/alpha"
bk=$($BL create "needs jonathan" -p 9950 | grep -o '#[0-9]*' | tr -d '#')
$BL next | grep -q "needs jonathan" || fail "precondition: blocked candidate is top"
if $BL status "$bk" blocked 2>/dev/null; then fail "blocked without --on accepted"; fi
$BL claim "$bk" --by parker >/dev/null
$BL status "$bk" blocked --on jonathan --by parker | grep -q "blocked" || fail "status blocked"
$BL next | grep -q "needs jonathan" && fail "next picked a blocked card"
$BL list | grep -q "blocked on: jonathan" || fail "list lacks the reason: $($BL list | head -3)"
$BL export -o "$T/blocked.html" >/dev/null; grep -q '"blocked_on":"jonathan"' "$T/blocked.html" || fail "page lacks the reason"
$BL show "$bk" | grep -q "  blocked       " || fail "status column"
$BL show "$bk" | grep -q "claimed_by=" && fail "claim survived blocking"
$BL reap --older-than 0s 2>&1 | grep -q "#$bk" && fail "reap touched a blocked card"
$BL board --no-color --width 120 | grep -q "BLOCKED" || fail "terminal board lacks the blocked column"
$BL history "$bk" | grep -q "blocked_on .*→ jonathan  by parker" || fail "blocked_on event"
$BL status "$bk" ready >/dev/null
$BL show "$bk" | grep -q "blocked on:" && fail "ready did not clear the reason"
$BL board --no-color --width 120 | grep -q "BLOCKED" && fail "blocked column shown with nothing blocked"
other=$($BL create "the decision card" -p 10 | grep -o '#[0-9]*' | tr -d '#')
$BL status "$bk" blocked --on "#$other" >/dev/null
$BL show "$bk" | grep -q "blocked by: #$other" || fail "--on #card did not link"
$BL status "$bk" ready >/dev/null; $BL delete "$other" --why t >/dev/null
# a database written by the 0.4 build: notes table with a cascade FK, status CHECK without blocked
mkdir -p "$T/v04"
sqlite3 "$T/v04/backlog.db" <<'SQL'
CREATE TABLE cards (
    id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, notes TEXT NOT NULL DEFAULT '',
    label TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'new'
        CHECK(status IN ('new','ready','in_progress','done')),
    priority INTEGER NOT NULL DEFAULT 5000 CHECK(priority BETWEEN 0 AND 10000),
    outcome TEXT NOT NULL DEFAULT '', claimed_by TEXT NOT NULL DEFAULT '', claimed_at TEXT NOT NULL DEFAULT '',
    commits TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')), project_id INTEGER NOT NULL DEFAULT 0, legacy_id INTEGER);
CREATE TABLE projects (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE, path TEXT NOT NULL UNIQUE,
    active INTEGER NOT NULL DEFAULT 1, autoexport TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL DEFAULT (datetime('now')));
CREATE TABLE notes (id INTEGER PRIMARY KEY AUTOINCREMENT, card_id INTEGER NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
    kind TEXT NOT NULL DEFAULT 'note', author TEXT NOT NULL DEFAULT '', body TEXT NOT NULL, commit_sha TEXT NOT NULL DEFAULT '',
    commit_subject TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL DEFAULT (datetime('now')));
INSERT INTO projects (name, path) VALUES ('v04', '/nonexistent/v04');
INSERT INTO cards (title, notes, priority, project_id) VALUES ('kept card', '[finding] must survive the rebuild', 7000, 1);
INSERT INTO notes (card_id, kind, body) VALUES (1, 'finding', 'must survive the rebuild');
SQL
$BL --db "$T/v04/backlog.db" list | grep -q "kept card" || fail "0.4 db unreadable"
[ "$($BL --db "$T/v04/backlog.db" notes 1 | grep -c 'must survive')" = 1 ] || fail "rebuild lost or duplicated the note row: $($BL --db "$T/v04/backlog.db" notes 1)"
$BL --db "$T/v04/backlog.db" status 1 blocked --on someone >/dev/null || fail "blocked refused on the rebuilt table"
$BL --db "$T/v04/backlog.db" notes 1 | grep -q "must survive" || fail "note gone after blocked"
ok "blocked status / 0.4 rebuild"

# 26. --version: package version plus the schema stamp of the database it would open, without migrating it
cd "$T/alpha"
$BL --version | grep -q "^bl [0-9][0-9.]*  (schema [0-9]*)" || fail "--version header: $($BL --version)"
$BL --version | grep -q "schema 5  (current)" || fail "store not stamped current: $($BL --version)"
mkdir -p "$T/v04b"; sqlite3 "$T/v04b/backlog.db" "CREATE TABLE cards (id INTEGER PRIMARY KEY, title TEXT NOT NULL, notes TEXT NOT NULL DEFAULT '', label TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'new' CHECK(status IN ('new','ready','in_progress','done')), priority INTEGER NOT NULL DEFAULT 5000, outcome TEXT NOT NULL DEFAULT '', claimed_by TEXT NOT NULL DEFAULT '', claimed_at TEXT NOT NULL DEFAULT '', commits TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL DEFAULT (datetime('now')), updated_at TEXT NOT NULL DEFAULT (datetime('now')));"
$BL --db "$T/v04b/backlog.db" --version | grep -q "schema ?  (unstamped" || fail "old db not reported unstamped: $($BL --db "$T/v04b/backlog.db" --version)"
sqlite3 "$T/v04b/backlog.db" "SELECT name FROM sqlite_master WHERE name='meta'" | grep -q meta && fail "--version migrated the database"
$BL --db "$T/v04b/backlog.db" list >/dev/null 2>&1
$BL --db "$T/v04b/backlog.db" --version | grep -q "schema 5  (current)" || fail "db not stamped after a command"
cd "$T/nowhere-$$" 2>/dev/null || mkdir -p "$T/nowhere" && cd "$T/nowhere"
BL_HOME="$T/empty-home" $BL --version | grep -q "database: none found" || fail "no-db case: $(BL_HOME="$T/empty-home" $BL --version)"
ok "--version / schema stamp"

echo "all $pass checks passed  ($T)"
rm -rf "$T"
