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

Env / flag: `BL_DB` or `--db path` overrides the database location (default `./backlog.db`).

## Suggested agent flow

```
new  →  diagnose sub-agent  →  ready  →  implement/repair sub-agent  →  done
```

Planner always starts with `bl next` (or `bl list --status ready`).
After a cycle, optionally `bl decay`.
Put the binary on PATH and teach Claude the commands via a small skill or CLAUDE.md.
