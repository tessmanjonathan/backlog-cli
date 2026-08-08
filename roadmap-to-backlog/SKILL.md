---
name: roadmap-to-backlog
description: Turn a goal, spec, or plan into a dependency-ordered roadmap and load it into the bl backlog as priority-ranked cards. Use for "make a roadmap", "plan the next milestone", "break this down into tasks", "seed the backlog", "what should we build next", or converting docs/spec + docs/STATUS.md open items into bl cards.
---

# Roadmap → backlog

Turns intent into `bl` cards that an autonomous orchestrator can pull without further planning.
The output is the backlog, not a document. A roadmap that lives only in markdown is not done.

Paths are repo-relative. `bl` is on PATH; the database is `./backlog.db` (gitignored).

## Read first

`docs/STATUS.md` (open spikes, risks), `docs/RULES.md` (hard constraints), `docs/spec/` on demand.
Do **not** scan `docs/research/` or `docs/spikes/`. Check what already exists before adding:

```bash
bl list --status new,ready,in_progress --limit 100
```

Never create a duplicate. If a card covers the work, adjust its priority instead.

## Card shape

A card is claimable by an agent with no other context. That means each one states its own
acceptance test.

```bash
bl create "Greedy mesher emits per-vertex AO corners" \
  --label core --priority 8200 \
  --notes "Done when: xUnit covers 4 corner values/vertex + zero alloc in the hot loop.
Depends on: #12. Gate: scripts/test.sh green."
```

- **Title** — imperative, one deliverable. "Fix lighting" is not a card; "Clamp practical light
  count to the 512 clustered budget" is.
- **`--label`** — one of `core`, `game`, `tests`, `spike`, `art`, `docs`. Matches the layout so
  `bl list --label core` is meaningful.
- **`--notes`** — must contain a **Done when:** line. Add **Depends on: #N** where real. This is
  the only place dependency order survives, because `bl` has no dependency field.
- **`--priority`** — see below.

## Priority bands

`bl` sorts by priority alone, so the number *is* the roadmap. Leave gaps to insert later.

| Band | Meaning |
|---|---|
| 9000–9999 | Broken gate, broken build, blocked work |
| 8000–8999 | Current milestone, on the critical path |
| 6000–7999 | Current milestone, parallel work |
| 4000–5999 | Next milestone (5000 is the `bl` default) |
| 2000–3999 | Known-needed, unscheduled |
| 1–1999 | Speculative |

A card that nothing depends on and nothing blocks does not belong above 6000. Dependencies must
outrank their dependents — an agent running `bl next` takes the highest number and will otherwise
start work it cannot finish.

## Procedure

1. **Restate the goal** in one sentence. If it needs two, it is two roadmaps.
2. **Decompose to deliverables**, not activities. "Research X" is only a card when its output is a
   written decision with a filename.
3. **Order by dependency**, then assign priorities so the order is monotonic.
4. **Split anything larger than a session.** A card no agent can finish in one run will be claimed,
   half-done, and released forever.
5. **Mark the visual ones.** Any card whose output is seen must say
   `Critic: harsh-critic on <res://scene>` in its notes. Those cards are not done until the critic
   writes a PASS.
6. **Create the cards**, lowest priority first so `bl list` reads top-down as you go.
7. **Verify**: `bl list --limit 100`. Every card has a Done-when line, priorities are monotonic
   against dependencies, no duplicates.
8. **Report** the created IDs and the critical path. Do not paste the whole backlog back.

## Seeding from STATUS.md

The 12 open spike questions in `docs/STATUS.md` are already card-shaped — question plus pass
condition. Convert them with `--label spike`, priority by whether they gate architecture:

```bash
bl create "Spike 1: chunk mesh throughput in C# on WorkerThreadPool" \
  --label spike --priority 8600 \
  --notes "Done when: sustained flight with no frame drop, number recorded in docs/STATUS.md.
Throwaway code in docs/spikes/. Blocks the mesher escape-hatch decision in RULES.md."
```

Spikes 1 and 7 outrank the rest: `RULES.md` makes the C++ mesher escape hatch conditional on them,
so every meshing card downstream is speculative until they land.

## Gotchas

- **`bl` has no delete.** A wrong card is permanent — you can only `bl set-priority <id> 1` and
  `bl status <id> done --outcome "created in error"`. Get the title right the first time.
- **`-l` binds to `--label`, not `--limit`,** in `bl list`. `bl list -l 5` silently filters by the
  label "5" and returns nothing. Always spell `--limit` out.
- **Priority is absolute, not relative.** Adding a 9000 card demotes nothing; it just outranks.
  Run `bl decay --amount 25` to let stale work sink rather than hand-editing a backlog.
- **Cards are not commits.** Creating them touches only `backlog.db`, which is gitignored, so none
  of this trips the governance gate.
