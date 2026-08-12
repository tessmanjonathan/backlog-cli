# bl — lightweight Kanban backlog for Claude Code

Single-binary SQLite backlog. Designed for autonomous agent loops.

## Build

```bash
# re-extract / rebuild
tar -xzf bl-cli.tar.gz
cd backlog-cli
cargo build --release
cp target/release/bl ~/bin/bl   # or /usr/local/bin/bl

# in a project
bl init   # safe to re-run; migrates existing backlog.db
```

## Quick start

```bash
bl init
bl create "Diagnose flaky auth" --label auth --priority 8200
bl create "Add dark mode" --label ui --priority 4500
bl list
bl next
bl status 1 ready
bl note 1 "Found race on token refresh"
bl status 1 done --outcome "Fixed with mutex"
bl decay --amount 25   # run daily / on schedule
```

## Commands

| Command | Purpose |
|---------|---------|
| `bl init` | Create `backlog.db` + schema |
| `bl create "title" [-l label] [-p 0-10000] [-n notes]` | New card (status=new) |
| `bl set-priority <id> <0-10000>` | Set priority score |
| `bl status <id> <new\|ready\|done> [--outcome "..."]` | Move status |
| `bl list [-l label] [-s new,ready] [-n 30] [--json]` | List ordered by priority |
| `bl next [-l label] [--ready-only] [--json]` | Highest priority actionable card |
| `bl show <id> [--json]` | One card |
| `bl note <id> "text"` | Append to notes |
| `bl decay [-a 25]` | Subtract priority from all non-done cards |
| `bl board [-l label] [-d 8] [--watch]` | Draw the board in the terminal |
| `bl export [-o view/index.html] [--open]` | Standalone HTML snapshot, no server |
| `bl serve [-p 7788] [--also other.db] [--open]` | Live board view on localhost |

Env / flag: `BL_DB` or `--db path` overrides the database location (default `./backlog.db`).

## Board view

The same board — summary tiles, open cards by label, priority distribution, and the
`new / ready / in_progress / done` columns — in three deliveries.

**Terminal.** No browser, no server:

```bash
bl board                    # draw it once
bl board --watch            # redraw every 5s (--watch 2 for faster)
bl board -l worldgen -d 20  # one label, more done cards
```

Sizes itself to the terminal; honors `NO_COLOR` and pipes cleanly to a file.

**Static snapshot.** One self-contained HTML file with the cards baked in — open it with
`file://`, commit it, or mail it. Nothing is fetched at view time:

```bash
bl export                        # writes view/index.html
bl export -o docs/backlog.html --open
```

**Live server.** Polls every 5s, so an agent loop's progress shows up as it happens:

```bash
bl serve --open                                   # board for ./backlog.db on :7788
bl --db ~/git/foo/backlog.db serve \
   --also ~/git/bar/backlog.db --port 9000        # switch between projects in the UI
```

In both HTML views, click a card for notes, outcome, claim and timestamps; `#card-12` in
the URL deep-links to one card.

Read-only by design: databases are opened `SQLITE_OPEN_READ_ONLY`, only the paths given on
the command line are reachable, and the server binds `127.0.0.1` only. Use the CLI to make
changes.

## Suggested agent flow

```
new  →  diagnose sub-agent  →  ready  →  implement/repair sub-agent  →  done
```

Planner always starts with `bl next` (or `bl list --status ready`).
After a cycle, optionally `bl decay`.
Put the binary on PATH and teach Claude the commands via a small skill or CLAUDE.md.


# Agent Instructions (Add to Claude.md / Agent.md)
```
## Backlog (`bl`)

Local SQLite Kanban. DB path: `$BL_DB` or `--db` (default `./backlog.db`).
Shared across worktrees via absolute `BL_DB`.

### Commands
- `bl next --claim --by <agent-id>` — take highest-priority free card
- `bl claim <id> --by <agent-id>` — lock a specific card
- `bl show <id>` — read card details
- `bl status <id> done --outcome "..."` — finish (clears claim)
- `bl release <id> --by <agent-id>` — abandon, return to ready
- `bl create "title" --label <area> --priority <0-10000> [--notes "..."]`
- `bl note <id> "text"` — append notes
- `bl list [--status new,ready,in_progress] [--label X]`
- `bl set-priority <id> <N>`
- `bl decay --amount 25` — lower non-done priorities

### Rules
1. Always claim before working (`--claim --by` or `bl claim`).
2. Work only your claimed card. Never touch another agent's claim.
3. On finish: `bl status <id> done --outcome "one-line result"`.
4. On bail: `bl release <id> --by <agent-id>`.
5. Create follow-ups with `bl create`, don't overload the current card.
6. Status flow: `new → ready → in_progress → done`.

### Fan-out
- Orchestrator assigns IDs → each agent `bl claim <id> --by <name>`.
- Or agents self-pull → `bl next --claim --by <name>`.
```