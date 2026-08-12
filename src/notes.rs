//! Notes as rows rather than one growing text blob.
//!
//! `cards.notes` is still maintained, as a rendered mirror of these rows: a
//! `bl` built before this table existed keeps working against the same
//! database, the terminal board keeps printing, and nothing has to be migrated
//! in lockstep with the binary. When such an older build appends to the blob,
//! [`reconcile`] adopts the extra lines back into the table.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

/// A single note. Kinds are free-form; `note`, `finding`, `decision`,
/// `blocker`, `attempt` and `reaped` are the ones the tool itself uses.
#[derive(Debug, serde::Serialize)]
pub(crate) struct Note {
    pub(crate) id: i64,
    pub(crate) kind: String,
    pub(crate) author: String,
    pub(crate) body: String,
    pub(crate) commit_sha: String,
    pub(crate) commit_subject: String,
    pub(crate) created_at: String,
}

impl Note {
    /// The blob form: `[kind] body [sha]`, with the pieces that carry no
    /// information left out.
    pub(crate) fn render(&self) -> String {
        let mut s = String::new();
        if self.kind != "note" && !self.kind.is_empty() {
            s.push_str(&format!("[{}] ", self.kind));
        }
        s.push_str(&self.body);
        if !self.commit_sha.is_empty() {
            s.push_str(&format!(" [{}]", self.commit_sha));
        }
        s
    }

    /// Inverse of `render`, used when adopting blob lines written by a build
    /// that predates this table.
    fn parse(line: &str) -> (String, String, String) {
        let mut kind = "note".to_string();
        let mut rest = line.trim().to_string();

        if let Some(close) = rest.find("] ") {
            if rest.starts_with('[') {
                let inner = &rest[1..close];
                if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                    kind = inner.to_string();
                    rest = rest[close + 2..].to_string();
                }
            }
        }

        let mut sha = String::new();
        if rest.ends_with(']') {
            if let Some(open) = rest.rfind('[') {
                let inner = &rest[open + 1..rest.len() - 1];
                if (7..=40).contains(&inner.len()) && inner.chars().all(|c| c.is_ascii_hexdigit()) {
                    sha = inner.to_string();
                    rest = rest[..open].trim_end().to_string();
                }
            }
        }
        (kind, rest, sha)
    }
}

pub(crate) fn list(conn: &Connection, card_id: i64) -> Result<Vec<Note>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, author, body, commit_sha, commit_subject, created_at
         FROM notes WHERE card_id = ? ORDER BY id ASC",
    )?;
    let rows = stmt.query_map(params![card_id], |r| {
        Ok(Note {
            id: r.get(0)?,
            kind: r.get(1)?,
            author: r.get(2)?,
            body: r.get(3)?,
            commit_sha: r.get(4)?,
            commit_subject: r.get(5)?,
            created_at: r.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Adopt any blob lines that have no matching row — either notes written
/// before this table existed, or by an older `bl` sharing the database.
pub(crate) fn reconcile(conn: &Connection, card_id: i64) -> Result<()> {
    let (blob, created): (String, String) = match conn
        .query_row(
            "SELECT notes, created_at FROM cards WHERE id = ?",
            params![card_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
    {
        Some(v) => v,
        None => return Ok(()),
    };

    let lines: Vec<&str> = blob.lines().filter(|l| !l.trim().is_empty()).collect();
    let have: i64 = conn.query_row(
        "SELECT COUNT(*) FROM notes WHERE card_id = ?",
        params![card_id],
        |r| r.get(0),
    )?;

    // The blob is written from the rows, so it can only ever run ahead.
    if lines.len() as i64 <= have {
        return Ok(());
    }
    for line in &lines[have as usize..] {
        let (kind, body, sha) = Note::parse(line);
        conn.execute(
            "INSERT INTO notes (card_id, kind, author, body, commit_sha, created_at)
             VALUES (?1, ?2, '', ?3, ?4, ?5)",
            params![card_id, kind, body, sha, created],
        )?;
    }
    Ok(())
}

/// Append a note and re-render the blob. Returns the new note's id, or None
/// when `unique` suppressed a duplicate.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add(
    conn: &Connection,
    card_id: i64,
    kind: &str,
    author: &str,
    body: &str,
    commit: Option<(String, String)>,
    unique: bool,
    now: &str,
) -> Result<Option<i64>> {
    reconcile(conn, card_id)?;

    if unique {
        let dup: Option<i64> = conn
            .query_row(
                "SELECT id FROM notes WHERE card_id = ?1 AND body = ?2 AND kind = ?3",
                params![card_id, body, kind],
                |r| r.get(0),
            )
            .optional()?;
        if dup.is_some() {
            return Ok(None);
        }
    }

    let (sha, subject) = commit.unwrap_or_default();
    conn.execute(
        "INSERT INTO notes (card_id, kind, author, body, commit_sha, commit_subject, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![card_id, kind, author, body, sha, subject, now],
    )?;
    let id = conn.last_insert_rowid();
    rebuild_blob(conn, card_id, now)?;
    Ok(Some(id))
}

/// Rewrite `cards.notes` from the rows, so both views of the notes agree.
pub(crate) fn rebuild_blob(conn: &Connection, card_id: i64, now: &str) -> Result<()> {
    let blob = list(conn, card_id)?
        .iter()
        .map(|n| n.render())
        .collect::<Vec<_>>()
        .join("\n");
    conn.execute(
        "UPDATE cards SET notes = ?1, updated_at = ?2 WHERE id = ?3",
        params![blob, now, card_id],
    )?;
    Ok(())
}

/// Whether this database has the notes table at all — a `--also` database
/// opened read-only may predate it.
pub(crate) fn table_exists(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='notes'",
        [],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
    .unwrap_or(false)
}
