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
| `bl create "title" [-l label] [-p 0-10000] [-n notes] [--if-absent]` | New card (status=new) |
| `bl set-priority <id> <0-10000>` | Set priority score |
| `bl status <id> <new\|ready\|done> [--outcome "..."]` | Move status |
| `bl list [-l label] [-s new,ready] [-n 30] [--json]` | List ordered by priority |
| `bl next [-l label] [--ready-only] [--json]` | Highest priority actionable card |
| `bl show <id> [--json]` | One card |
| `bl search <words...> [-l label] [--open] [-n 30] [--json]` | Find cards by any word in them |
| `bl note <id> "text" [-k kind] [--by who] [--commit [REV]] [--unique]` | Add a note |
| `bl notes <id> [-k kind] [--json]` | Read a card's notes |
| `bl heartbeat <id> --by <agent-id>` | Keep a long claim alive |
| `bl reap [--older-than 30m] [--dry-run]` | Return claims from agents that died |
| `bl decay [-a 25]` | Subtract priority from all non-done cards |
| `bl board [-l label] [-d 8] [--watch]` | Draw the board in the terminal |
| `bl export [-o view/index.html] [--open] [--auto]` | Standalone HTML snapshot, no server |
| `bl auto on\|off\|status [-o view/index.html]` | Keep a snapshot in sync after every write |
| `bl prompt [-o FILE] [--append]` | Print agent instructions for *this* backlog |
| `bl serve [-p 7788] [--also other.db] [--open]` | Live board view on localhost |

Env / flag: `BL_DB` or `--db path` overrides the database location (default `./backlog.db`).

## Keeping the snapshot live

A snapshot normally goes stale the moment an agent touches the backlog. Turn auto-export
on once and every command that writes a card rewrites it:

```bash
bl auto on                       # writes view/index.html and remembers it
bl auto on -o docs/backlog.html  # somewhere else
bl auto status
bl auto off
```

After that, `bl next --claim`, `bl status`, `bl note`, `bl create` and `bl decay` all refresh
the file as a side effect — no separate step in the agent loop, nothing to remember. The
write is atomic (temp file + rename), so a reload never catches a half-written page, and a
failed refresh warns on stderr without failing the command that already committed.

The path is stored in the database, so it follows the backlog rather than the shell.
`BL_AUTOEXPORT=path` overrides it for one command; `BL_NO_AUTOEXPORT=1` suppresses the
refresh (useful for bulk imports — run `bl export` once at the end).

Served over http, the snapshot re-reads itself every 5s, so it behaves like `bl serve`
without a server running. Opened as `file://` it stays a true point-in-time snapshot,
since browsers won't let a local page fetch.

## Notes

Notes are rows, not one growing paragraph. Each carries a kind, an author, a timestamp
and optionally a commit:

```bash
bl note 12 "auth races on token refresh" --kind finding --by claude
bl note 12 "chose a mutex over a channel" --kind decision
bl note 12 "waiting on #14" --kind blocker
bl note 12 "retrying the build" --unique     # no-op if that note is already there
bl notes 12 --kind blocker
```

Kinds are free-form. `cards.notes` is still maintained as a rendered mirror of the rows,
so a `bl` built before this table — a pinned copy on another machine, an agent that never
upgraded — keeps reading and writing the same database. When such a build appends
straight to the blob, the next command adopts those lines back into the table, kind and
commit sha included. Nothing has to be migrated in lockstep with the binary.

## Search

```bash
bl search hero art              # every word must appear somewhere on the card
bl search palette --open        # skip done cards
bl search token --json
```

Covers titles, notes, outcomes and labels, and prints the lines that matched.

## Claims that outlive their agent

An agent that crashes mid-card leaves it `in_progress` forever, invisible to `bl next`.

```bash
bl reap --older-than 30m --dry-run   # what would be released
bl reap --older-than 30m            # release it, with a [reaped] note saying why
bl heartbeat 12 --by claude         # from a long-running agent, so it isn't reaped
```

## Exit codes

| Code | Meaning |
|---|---|
| 0 | worked |
| 1 | error |
| 2 | nothing matched — `next`, `search`, `notes`, `reap` |
| 3 | contention — `claim`/`heartbeat` on a card someone else holds |

So an agent loop terminates on its own, without parsing output:

```bash
while bl next --claim --by "$AGENT" --json > card.json; do
  work "$(jq -r .id card.json)"
done
```

## Teaching an agent

`bl prompt` prints the instructions below, already filled in with this backlog's absolute
path, whether the board is self-refreshing, and the labels currently in use — so an agent
doesn't have to guess at any of it:

```bash
bl prompt                                   # read it
bl prompt -o .claude/skills/bl/SKILL.md     # install as a skill
bl prompt -o CLAUDE.md --append             # or paste it into the project's context
```

Re-run it when the label set changes; it is generated, not hand-maintained.

## Linking commits

When git is in play, a note can record the commit it describes:

```bash
bl note 12 "Fixed the token refresh race" --commit        # HEAD
bl note 12 "Reverted the first attempt" --commit a1b2c3d  # any revision git resolves
```

The short sha is appended to the note line, and `sha  subject` is collected on the card —
visible in `bl show`, in the terminal board, and in the Commits block of the card detail.
Revisions resolve against the repository holding the database, not the current directory,
so this works from any worktree. Outside a repository the note is still recorded, with a
warning that nothing was linked.

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
Put the binary on PATH and hand Claude the instructions with `bl prompt`.


# Agent instructions

They live in `src/agent.md` and ship inside the binary — run `bl prompt` to get the copy
filled in for a particular backlog, rather than pasting a stale block from here.
