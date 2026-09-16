# bl — lightweight Kanban backlog for Claude Code

Single-binary SQLite backlog. Designed for autonomous agent loops.

## Build

```bash
# re-extract / rebuild
tar -xzf bl-cli.tar.gz
cd backlog-cli
cargo build --release
cp target/release/bl ~/bin/bl   # or /usr/local/bin/bl

# once: create the central store under ~/.bl and register this repository
cd ~/git/myproject
bl init

# every other repository: register it
cd ~/git/other && bl project add
```

## One store, many projects

Every project's cards live in one database, `~/.bl/backlog.db` (the `db:` key in
`~/.bl/config.yml` moves it). A `projects` table maps each repository path to a project,
and a command run anywhere inside that repository, including any of its git worktrees, is
scoped to that project without a flag. Card ids are unique across the store.

```bash
bl project add [path] [--name N]   # register a repository (default: the one you are in)
bl project list                    # every project, active flag, open card count
bl project current                 # what this directory resolves to
bl project deactivate experiments  # skipped by bl next, hidden from the default views
bl project activate experiments
bl project remove old [--force]    # --force deletes its cards too
```

From a directory outside any project, `bl next`, `bl list`, `bl board` and `bl export` read
across every active project and name the project on each card. Inside one, `--all` does the
same. `--project <name>` (or `BL_PROJECT`) picks a project by hand from anywhere.

**Moving an existing repo-level board in:**

```bash
bl import ~/git/myproject/backlog.db     # source is read-only and untouched
bl migrate --scan ~/git                  # every <dir>/backlog.db under ~/git, plus each registered project's
bl migrate --dry-run
```

Imported cards get new ids in the store and keep the old one in `legacy_id`: `bl show`
prints it, and `bl import` prints the old-to-new map. A note that says `#123` still means
the old number, so keep the map (or the old file) if that matters. A second import of the
same file adds nothing. Once imported, delete or gitignore the repo copy so nothing writes
to it again.

`bl` never creates a database by accident: only `bl init` does. A directory with no store and
no `./backlog.db` gets an error, not an empty board. `--db <path>` (or `BL_DB`) still works
against one file, and `bl init --db path` creates one; such a file gets a single project row
named after its directory the first time this build opens it. A repository that still carries
its own `./backlog.db` and is not registered keeps using that file, with a hint on stderr.

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
| `bl init` | Create the central store and register this repository (`--db` for one file) |
| `bl project add\|list\|current\|activate\|deactivate\|remove` | Which repositories share the store |
| `bl import <backlog.db> [--project N] [--dry-run] [--json]` | Copy a repo-level board into the store |
| `bl import --stdin [--if-absent] [--by who] [--dry-run]` | File many cards at once from a JSON array or JSON lines (`{"title", "label"?, "priority"?, "notes"?, "project"?}`); prints the ids as JSON |
| `bl migrate [--scan DIR]... [--dry-run]` | Import every repo-level board it can find |
| `bl create "title" [-l tag,tag] [-p 0-10000] [-n notes] [--if-absent] [--by who]` | New card (status=new); a title over 120 characters is refused unless `--force` |
| `bl edit <id> [--title] [--label tag,tag] [--add-tag T] [--rm-tag T] [--priority] [--notes] [--outcome] [--status] [--move PROJECT] [--by who] [--json]` | Change any field, all flags in one transaction |
| `bl edit --ids 1,2,3 --priority 100` · `bl edit --where label=art --where status=new --set priority=100 [--dry-run]` | The same change on many cards; `--where` takes label, status, claimed_by, project, title with `=`/`!=` and priority, id with `< > <= >=`; every card gets its event rows |
| `bl retitle <id> "title"` | Short for `bl edit --title` |
| `bl delete <id> [--why "..."] [--force] [--by who]` | Remove a card; title and notes stay in `bl history <id>` (claimed cards need `--force`) |
| `bl set-priority <id> <0-10000>` | Set priority score |
| `bl status <id> <new\|ready\|done> [--outcome "..."] [--by who]` | Move status; an outcome over 300 characters is refused unless `--force` (put the detail in a note) |
| `bl history <id> [--json]` | Every change the card went through: status, claim, priority, title, ... (works after delete) |
| `bl link <id> [--blocks ID] [--child-of ID] [--related ID]` · `bl unlink` (same flags) · `bl block <id> --on ID` | Relate cards; a card with an open blocker is skipped by `bl next` and says `blocked by` wherever it is printed |
| `bl list [-l tag[,tag]] [-s new,ready] [-n 30] [--json]` | List ordered by priority; `-l` matches a card carrying any of the tags |
| `bl next [-l label] [--ready-only] [--json]` | Highest priority actionable card |
| `bl show <id> [--json]` | One card |
| `bl search <words...> [-l label] [--open] [-n 30] [--json]` | Find cards by any word in them |
| `bl note <id> "text" [-k kind] [--by who] [--commit [REV]] [--unique]` · `bl note <id> --stdin` · `bl note <id> -f FILE` | Add a note; `--stdin` / `-f` take the body without shell quoting |
| `bl notes <id> [-k kind] [--json]` | Read a card's notes, each with its note id |
| `bl note edit <note-id> ["text"] [-k kind]` · `bl note rm <note-id>` | Fix or remove one note by id; the card's notes mirror and the event log follow |
| `bl heartbeat <id> --by <agent-id>` | Keep a long claim alive |
| `bl reap [--older-than 30m] [--dry-run]` | Return claims from agents that died |
| `bl decay [-a 25]` | Subtract priority from all non-done cards |
| `bl board [-l label] [-d 8] [--watch]` | Draw the board in the terminal |
| `bl export [-o view/index.html] [--open] [--auto]` | Standalone HTML snapshot, no server |
| `bl auto on\|off\|status [-o view/index.html]` | Keep a snapshot in sync after every write |
| `bl prompt [-o FILE] [--append]` | Print agent instructions for *this* backlog |
| `bl serve [-p 7788] [--also other.db] [--open]` | Live board view on localhost |

**Tags.** A card's label is a comma-separated tag list (`-l art,enemies`; spaces work too).
`-l art` on `list`, `next`, `search` and `board` matches any card carrying that tag, and
`-l art,ui` any card carrying either. `bl edit --add-tag` / `--rm-tag` adjust one tag; `--label`
replaces the set. A database written before tags has its space-separated labels split once,
the first time this build opens it.

Global flags: `--project <name>` / `BL_PROJECT` scope to a project; `--all` reads across
active projects; `--db path` / `BL_DB` use one database file instead of the store.
`BL_HOME` moves the whole `~/.bl` directory.

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

The path is stored per project when run inside one, and store-wide when run from outside
(that page shows every active project). A write refreshes the touched project's page and the
store-wide page.
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
