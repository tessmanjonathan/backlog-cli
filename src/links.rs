//! Relations between cards: `blocks`, `child_of` and `related`.
//!
//! A card with an open blocker is skipped by `bl next`, so a fan-out can be
//! ordered on the board instead of by an orchestrator. `child_of` groups work
//! under an epic; `related` is a plain cross-reference. Rows are directional
//! (`from` → `to`) and `related` is stored once, lowest id first.

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};

pub(crate) const KINDS: [&str; 3] = ["blocks", "child_of", "related"];

/// One relation as seen from a card: `rel` is `blocked_by`, `blocks`,
/// `child_of`, `parent_of` or `related`, and the rest describes the other card.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Rel {
    pub(crate) rel: String,
    pub(crate) id: i64,
    pub(crate) title: String,
    pub(crate) status: String,
}

fn check_kind(kind: &str) -> Result<()> {
    if !KINDS.contains(&kind) {
        bail!("unknown link kind '{}' (blocks, child_of, related)", kind);
    }
    Ok(())
}

/// Store `from` --kind--> `to`. Returns false when the link already exists.
pub(crate) fn add(conn: &Connection, from: i64, kind: &str, to: i64, by: &str, now: &str) -> Result<bool> {
    check_kind(kind)?;
    if from == to {
        bail!("a card cannot be linked to itself");
    }
    let (from, to) = if kind == "related" && from > to { (to, from) } else { (from, to) };
    for id in [from, to] {
        let exists: Option<i64> = conn
            .query_row("SELECT id FROM cards WHERE id = ?", params![id], |r| r.get(0))
            .optional()?;
        if exists.is_none() {
            bail!("card #{} not found", id);
        }
    }
    if kind == "blocks" && would_cycle(conn, from, to)? {
        bail!("#{} already waits on #{} (directly or through other cards); that would be a cycle", from, to);
    }
    let n = conn.execute(
        "INSERT OR IGNORE INTO links (from_id, kind, to_id, actor, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![from, kind, to, by, now],
    )?;
    Ok(n == 1)
}

/// Remove one link. Returns false when there was none.
pub(crate) fn remove(conn: &Connection, from: i64, kind: &str, to: i64) -> Result<bool> {
    check_kind(kind)?;
    let (from, to) = if kind == "related" && from > to { (to, from) } else { (from, to) };
    let n = conn.execute(
        "DELETE FROM links WHERE from_id = ?1 AND kind = ?2 AND to_id = ?3",
        params![from, kind, to],
    )?;
    Ok(n == 1)
}

/// Would `from blocks to` close a loop? True if `to` already (transitively)
/// blocks `from`.
fn would_cycle(conn: &Connection, from: i64, to: i64) -> Result<bool> {
    let hit: Option<i64> = conn
        .query_row(
            "WITH RECURSIVE up(id) AS (
                 SELECT ?1
                 UNION
                 SELECT l.to_id FROM links l JOIN up ON l.from_id = up.id WHERE l.kind = 'blocks'
             ) SELECT 1 FROM up WHERE id = ?2 LIMIT 1",
            params![to, from],
            |r| r.get(0),
        )
        .optional()?;
    Ok(hit.is_some())
}

/// Everything linked to one card, in reading order: what it waits on, what
/// waits on it, its parent, its children, then related cards.
pub(crate) fn of(conn: &Connection, card_id: i64) -> Result<Vec<Rel>> {
    let mut stmt = conn.prepare(
        "SELECT rel, c.id, c.title, c.status FROM (
             SELECT 'blocked_by' AS rel, from_id AS other, 0 AS o FROM links WHERE kind = 'blocks' AND to_id = ?1
             UNION ALL SELECT 'blocks', to_id, 1 FROM links WHERE kind = 'blocks' AND from_id = ?1
             UNION ALL SELECT 'child_of', to_id, 2 FROM links WHERE kind = 'child_of' AND from_id = ?1
             UNION ALL SELECT 'parent_of', from_id, 3 FROM links WHERE kind = 'child_of' AND to_id = ?1
             UNION ALL SELECT 'related', to_id, 4 FROM links WHERE kind = 'related' AND from_id = ?1
             UNION ALL SELECT 'related', from_id, 4 FROM links WHERE kind = 'related' AND to_id = ?1
         ) r JOIN cards c ON c.id = r.other ORDER BY o, c.id",
    )?;
    let rows = stmt.query_map(params![card_id], |r| {
        Ok(Rel { rel: r.get(0)?, id: r.get(1)?, title: r.get(2)?, status: r.get(3)? })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// The SQL that keeps a card out of `bl next` while something it waits on
/// is not done. Starts with ` AND `.
pub(crate) const NOT_BLOCKED: &str = " AND NOT EXISTS (SELECT 1 FROM links l JOIN cards b ON b.id = l.from_id \
     WHERE l.kind = 'blocks' AND l.to_id = cards.id AND b.status != 'done')";

/// Drop every link touching a card (used by delete).
pub(crate) fn drop_all(conn: &Connection, card_id: i64) -> Result<()> {
    conn.execute("DELETE FROM links WHERE from_id = ?1 OR to_id = ?1", params![card_id])?;
    Ok(())
}

pub(crate) fn table_exists(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='links'",
        [],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
    .unwrap_or(false)
}

/// The one-line label a rel gets in text output.
pub(crate) fn label(rel: &str) -> &'static str {
    match rel {
        "blocked_by" => "blocked by",
        "blocks" => "blocks",
        "child_of" => "child of",
        "parent_of" => "children",
        _ => "related",
    }
}
