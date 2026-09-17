//! The audit trail: one row per change to a card.
//!
//! Notes say what an agent chose to write down; events say what actually
//! happened to the card (status, claim, priority, title...), who did it and
//! when. Rows are never updated or deleted, and they outlive the card, so a
//! deleted card can still be read back through `bl history`.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

/// One change. `before` and `after` are free text: a status name, a priority,
/// a title, an agent id, or a longer payload for `deleted`.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Event {
    pub(crate) id: i64,
    pub(crate) card_id: i64,
    pub(crate) project_id: i64,
    pub(crate) kind: String,
    pub(crate) before: String,
    pub(crate) after: String,
    pub(crate) by: String,
    pub(crate) at: String,
}

/// `before`, `after` and `by` are SQL keywords, so the columns are named
/// `old_value`, `new_value` and `actor`; the JSON keeps the plain names.
const COLS: &str = "id, card_id, project_id, kind, old_value, new_value, actor, at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: r.get(0)?,
        card_id: r.get(1)?,
        project_id: r.get(2)?,
        kind: r.get(3)?,
        before: r.get(4)?,
        after: r.get(5)?,
        by: r.get(6)?,
        at: r.get(7)?,
    })
}

/// Append one event. The project is read from the card so callers do not
/// have to carry it; pass `project` when the card row is already gone.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record(
    conn: &Connection,
    card_id: i64,
    project: Option<i64>,
    kind: &str,
    before: &str,
    after: &str,
    by: &str,
    now: &str,
) -> Result<i64> {
    let project_id = match project {
        Some(p) => p,
        None => conn
            .query_row(
                "SELECT project_id FROM cards WHERE id = ?",
                params![card_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0),
    };
    conn.execute(
        "INSERT INTO events (card_id, project_id, kind, old_value, new_value, actor, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![card_id, project_id, kind, before, after, by, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Every event of one card, oldest first.
pub(crate) fn list(conn: &Connection, card_id: i64) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM events WHERE card_id = ? ORDER BY id ASC",
        COLS
    ))?;
    let rows = stmt.query_map(params![card_id], row)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Whether this database has the table at all: a board opened read-only
/// through `--also` may have been written by a build that predates it.
pub(crate) fn table_exists(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='events'",
        [],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
    .unwrap_or(false)
}

/// One line per event, the shape `bl history` prints.
pub(crate) fn render_line(e: &Event) -> String {
    let who = if e.by.is_empty() {
        String::new()
    } else {
        format!("  by {}", e.by)
    };
    let change = match (e.before.is_empty(), e.after.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("  → {}", one_line(&e.after)),
        (false, true) => format!("  {} →", one_line(&e.before)),
        (false, false) => format!("  {} → {}", one_line(&e.before), one_line(&e.after)),
    };
    format!("{}  {:<10}{}{}", e.at, e.kind, change, who)
}

fn one_line(s: &str) -> String {
    let flat = s.replace('\n', " | ");
    if flat.chars().count() > 160 {
        let cut: String = flat.chars().take(157).collect();
        format!("{}...", cut)
    } else {
        flat
    }
}
