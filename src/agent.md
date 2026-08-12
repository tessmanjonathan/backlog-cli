# Backlog (`bl`)

Local SQLite Kanban board. This project's backlog: `{{DB}}`
{{AUTO}}
Pass `--db <path>` (or set `BL_DB`) to work against a different backlog. An absolute
`BL_DB` is shared across worktrees, so parallel agents see one board.

## Loop

```bash
bl next --claim --by <agent-id>     # take the highest-priority free card
bl show <id>                        # read it in full before working
bl note <id> "what you learned"     # as you go, not just at the end
bl status <id> done --outcome "one-line result"
```

- `bl next --claim` is atomic: two agents racing get different cards, or none.
- Nothing to do? `bl next` exits **2** — end the loop, don't invent work.
- Bailing out? `bl release <id> --by <agent-id>` so someone else can take it.
- Long job? `bl heartbeat <id> --by <you>` every few minutes, or `bl reap` will
  decide you died and hand the card to someone else.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | worked |
| 1 | error — bad arguments, missing card, unreadable database |
| 2 | nothing matched (`next`, `search`, `notes`, `reap`) — normal, not a failure |
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
bl create "Hero art pass" --if-absent
```

`bl search` covers titles, notes, outcomes and labels; it exits 2 when nothing matches.
`--if-absent` prints the existing card's id instead of filing a duplicate.

## Notes are typed

```bash
bl note 12 "auth races on token refresh" --kind finding --by "$AGENT"
bl note 12 "chose a mutex over a channel" --kind decision
bl note 12 "waiting on #14" --kind blocker
bl note 12 "retrying the build" --unique      # skipped if already noted
bl notes 12 [--kind blocker] [--json]
```

Kinds are free-form. `finding`, `decision`, `blocker`, `attempt` and plain `note` are the
useful ones; `reaped` is written by the tool. Prefer several small typed notes over one
long one — the next agent can filter them.

## Commands

| Command | Use |
|---|---|
| `bl next [--label X] [--ready-only] [--claim --by <id>] [--json]` | Highest-priority actionable card |
| `bl claim <id> --by <agent-id>` | Lock one specific card |
| `bl release <id> [--by <agent-id>]` | Give a claimed card back |
| `bl show <id> [--json]` | One card in full |
| `bl list [--label X] [--status new,ready,in_progress] [-n 30] [--json]` | Ordered by priority |
| `bl create "title" [--label X] [--priority 0-10000] [--notes "..."] [--if-absent]` | New card (status `new`) |
| `bl search <words...> [--label X] [--open] [--json]` | Find cards by any word in them |
| `bl note <id> "text" [--kind K] [--by <id>] [--commit [REV]] [--unique]` | Add a note |
| `bl notes <id> [--kind K] [--json]` | Read a card's notes |
| `bl heartbeat <id> --by <agent-id>` | Keep a long claim alive |
| `bl reap [--older-than 30m] [--dry-run]` | Return claims from agents that died |
| `bl status <id> <new\|ready\|in_progress\|done> [--outcome "..."]` | Move it |
| `bl set-priority <id> <0-10000>` | Re-rank |
| `bl decay [--amount 25]` | Age everything down (scheduled, not per-task) |
| `bl board`, `bl serve`, `bl export` | Human views — you rarely need these |

`--json` on `next`, `show` and `list` is the machine-readable form; prefer it when you
are parsing rather than reading.

## Rules

1. Claim before working (`bl next --claim --by <you>` or `bl claim <id> --by <you>`).
2. Work only your claimed card. Never touch another agent's claim.
3. Priority is 0–10000, higher first. Roughly: 8000+ urgent, 5000 default, under 2000 someday.
4. Record what you found in notes as you go — the next agent only sees what you wrote down.
   Type them (`--kind finding|decision|blocker`) and sign them (`--by <you>`).
5. After committing code for a card: `bl note <id> "what changed" --commit` links the sha.
6. Finish with `bl status <id> done --outcome "..."`. The outcome is what a human reads first.
7. Discovered extra work? `bl create` a follow-up. Don't quietly widen the card you hold.
8. Status flow: `new → ready → in_progress → done`.
9. Never run `bl export` in a loop — the view pages keep themselves current.

## Fan-out

Orchestrator assigns cards (`bl claim <id> --by <name>` per agent), or agents self-pull
(`bl next --claim --by <name>`) until `bl next` comes up empty.
{{LABELS}}
