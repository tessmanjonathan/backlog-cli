# Backlog (`bl`)

SQLite Kanban board for agent loops. The store: `{{DB}}`
{{PROJECT}}
{{AUTO}}
Every project shares the one store, so a card id is unique everywhere. Inside a registered
repository (or any of its worktrees) every command is scoped to that project; from outside,
`bl next` and `bl list` read across all active projects and name the project on each card.
`--project <name>` (or `BL_PROJECT`) scopes a command by hand; `--all` reads every active
project from inside one; `--db <path>` (or `BL_DB`) works against a different database file.

## Loop

```bash
bl next --claim --by <agent-id>     # take the highest-priority free card
bl show <id>                        # read it in full before working; bl notes <id> for the notes
bl note <id> "what you learned" --by <agent-id>     # as you go, not just at the end
bl status <id> done --outcome "one-line result" --by <agent-id>
```

- `bl next --claim` is atomic: two agents racing get different cards, or none.
- Nothing to do? `bl next` exits **2** — end the loop, don't invent work.
- Bailing out? `bl release <id> --by <agent-id>` so someone else can take it.
- Stuck on a person or another card? `bl status <id> blocked --on <who or #card>`. It leaves
  your hands, `bl next` and `bl reap` skip it, and `bl status <id> ready` brings it back.
- Long job? `bl heartbeat <id> --by <you>` every few minutes, or `bl reap` will decide you
  died and hand the card to someone else.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | worked |
| 1 | error — bad arguments, missing card, a title or outcome over the limit, unreadable database |
| 2 | nothing matched (`next`, `search`, `notes`, `history`, `reap`, `edit --where`) — normal, not a failure |
| 3 | someone else holds the claim (`claim`, `heartbeat`) — take a different card |

```bash
while bl next --claim --by "$AGENT" --json > card.json; do
  # ... work the card, then mark it done ...
done   # exits 2 when the board is drained
```

## Before creating a card, look

```bash
bl search hero art          # every word must appear somewhere on the card
bl search palette --open    # open cards only
bl create "Hero art pass" -l art,hero --if-absent
```

`bl search` covers titles, notes, outcomes and tags; it exits 2 when nothing matches.
`--if-absent` prints the existing card's id instead of filing a duplicate. Filing many at
once (a plan, a fan-out) is one call, not twenty:

```bash
bl import --stdin --by "$AGENT" <<'EOF'
[{"title": "Hero idle pose", "label": "art,hero", "priority": 6000, "notes": "from the plan"},
 {"title": "Hero hurt pose", "label": "art,hero", "notes": [{"kind": "finding", "body": "needs 3 frames"}]}]
EOF
```

The new ids come back as JSON. A title is at most **120 characters** and an outcome at most
**300**: they are headlines, the detail goes in notes, and `bl create` / `bl status done`
refuse longer ones.

## Notes are typed and editable

```bash
bl note 12 "auth races on token refresh" --kind finding --by "$AGENT"
bl note 12 "chose a mutex over a channel" --kind decision
bl note 12 "retrying the build" --unique      # skipped if already noted
bl note 12 --stdin <<'EOF'                    # quotes, $ and * without shell escaping
it's the "$PATH" glob (*) case
EOF
bl note 12 -f findings.md                     # or from a file
bl notes 12 [--kind finding] [--json]         # every line ends with its note id
bl note edit 87 "the corrected text" --kind decision
bl note rm 87
```

Kinds are free-form. `finding`, `decision`, `blocker`, `attempt` and plain `note` are the
useful ones; `reaped` is written by the tool. Prefer several small typed notes over one
long one — the next agent can filter them. A wrong note is fixed or removed by its id, not
buried under a correction.

## Tags, dependencies and history

A card carries any number of tags (`-l art,enemies`); `-l art` on `list`, `next`, `search`
and `board` matches every card carrying that tag, and `bl edit <id> --add-tag x` /
`--rm-tag x` adjust one without retyping the rest.

```bash
bl block 14 --on 12            # 14 waits for 12: bl next skips 14 until 12 is done
bl link 12 --child-of 3        # 12 is part of epic 3
bl link 12 --related 9         # plain cross-reference
bl history 12                  # every status, claim, priority, tag, title and link change, who and when
```

Nothing you do to a card is lost: status, claims, priority, edits, links, note edits and
even `bl delete` leave rows that `bl history <id>` replays. Pass `--by <you>` to `status`,
`edit`, `set-priority`, `link` and `delete` so the log says who.

## Commands

| Command | Use |
|---|---|
| `bl next [-l tag] [--ready-only] [--claim --by <id>] [--json]` | Highest-priority actionable card (not blocked, not waiting on another card) |
| `bl claim <id> --by <agent-id>` | Lock one specific card |
| `bl release <id> [--by <agent-id>]` | Give a claimed card back |
| `bl show <id> [--json]` | One card in full, with what it waits on and blocks |
| `bl list [-l tag[,tag]] [-s new,ready,in_progress,blocked] [-n N] [--json]` | Ordered by priority; `-l` is a tag filter, `-n`/`--limit` is the count (default 30) |
| `bl create "title" [-l tag,tag] [-p 0-10000] [-n "notes"] [--if-absent] [--by <id>]` | New card (status `new`); title at most 120 characters |
| `bl import --stdin [--if-absent] [--by <id>] [--dry-run]` | File many cards from a JSON array or JSON lines; ids come back as JSON |
| `bl search <words...> [-l tag] [--open] [--json]` | Find cards by any word in them |
| `bl note <id> "text" [-k kind] [--by <id>] [--commit [REV]] [--unique]` | Add a note; `--stdin` or `-f FILE` instead of the text |
| `bl notes <id> [-k kind] [--json]` | Read a card's notes, each with its note id |
| `bl note edit <note-id> ["text"] [-k kind]` · `bl note rm <note-id>` | Fix or drop one note by id |
| `bl status <id> <new\|ready\|in_progress\|blocked\|done> [--outcome "..."] [--by <id>]` | Move it; outcome at most 300 characters |
| `bl status <id> blocked --on <who or #card>` | Park it with the reason; `--on #12` also makes #12 a blocker |
| `bl block <id> --on <other>` · `bl link <id> --blocks\|--child-of\|--related <other>` · `bl unlink ...` | Dependencies, epics and cross-references |
| `bl history <id> [--json]` | Every change the card went through (works after `bl delete` too) |
| `bl edit <id> [--title] [--label tag,tag] [--add-tag T] [--rm-tag T] [--priority] [--outcome] [--status] [--move PROJECT] [--by <id>]` | Change a card; `bl retitle <id> "..."` for the title alone |
| `bl edit --ids 1,2,3 --priority 100` · `bl edit --where label=art --where status=new --set priority=100 [--dry-run]` | Same change on many cards; `--dry-run` lists them first |
| `bl set-priority <id> <0-10000> [--by <id>]` | Re-rank |
| `bl delete <id> --why "..." [--force] [--by <id>]` | Remove a card filed in error; its title and notes stay in `bl history` |
| `bl heartbeat <id> --by <agent-id>` | Keep a long claim alive |
| `bl reap [--older-than 30m] [--dry-run]` | Return claims from agents that died |
| `bl decay [--amount 25]` | Age everything down (scheduled, not per-task) |
| `bl board`, `bl serve`, `bl export` | Human views — you rarely need these |

`--json` on `next`, `show`, `list`, `search`, `notes` and `history` is the machine-readable
form; prefer it when you are parsing rather than reading.

## Rules

1. Claim before working (`bl next --claim --by <you>` or `bl claim <id> --by <you>`).
2. Work only your claimed card. Never touch another agent's claim.
3. Priority is 0–10000, higher first. Roughly: 8000+ urgent, 5000 default, under 2000 someday.
4. Record what you found in notes as you go — the next agent only sees what you wrote down.
   Type them (`--kind finding|decision|blocker`) and sign them (`--by <you>`).
5. After committing code for a card: `bl note <id> "what changed" --commit` links the sha.
6. Finish with `bl status <id> done --outcome "..."`. The outcome is what a human reads first:
   one line, under 300 characters, the detail in notes.
7. Discovered extra work? `bl create` a follow-up (or `bl import --stdin` for several). Don't
   quietly widen the card you hold. If it must land first, `bl block <yours> --on <new>`; if
   it belongs to an epic, `bl link <new> --child-of <epic>`.
8. Status flow: `new → ready → in_progress → done`. A card that cannot move without someone
   (a decision, another card) is `bl status <id> blocked --on <who or #card>`, never left
   in_progress with the reason written into `claimed_by`.
9. Never run `bl export` in a loop — the view pages keep themselves current.
10. A wrong title, tag or priority is fixed with `bl edit`; a wrong note with `bl note edit`
    or `bl note rm`; a card filed in error with `bl delete --why`. Never open the database
    with sqlite3: `bl` mirrors notes into a legacy column, re-adopts rows and writes the
    event log, and a direct write skips all three.

## Fan-out

An orchestrator files the cards in one `bl import --stdin`, orders them with `bl block`
and `bl link --child-of`, then either assigns them (`bl claim <id> --by <name>` per agent)
or lets agents self-pull (`bl next --claim --by <name>`) until `bl next` comes up empty.
Blocked cards resolve themselves: when a blocker is marked done, what waited on it becomes
pickable on the next `bl next`.
{{LABELS}}
