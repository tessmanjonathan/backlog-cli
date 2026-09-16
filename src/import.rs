//! `bl import`: move a repo-level backlog.db into the central store.
//!
//! The source is opened read-only and never changed; every card gets a new id
//! in the store and keeps its old one in `legacy_id`, so a `#123` written in a
//! note can still be looked up, and a second import of the same file adds
//! nothing.

use crate::store::{self, Project};
use crate::{notes, view, Ctx};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, serde::Serialize)]
pub(crate) struct Outcome {
    pub(crate) source: String,
    pub(crate) project: String,
    pub(crate) project_id: i64,
    pub(crate) project_created: bool,
    /// (legacy id, new id) for every card imported by this run.
    pub(crate) imported: Vec<(i64, i64)>,
    /// Cards already present from an earlier import, left alone.
    pub(crate) skipped: usize,
    pub(crate) notes: usize,
    pub(crate) autoexport: Option<String>,
    pub(crate) dry_run: bool,
}

/// What the source database says about itself: its project row if this
/// build's schema ever touched it, else the directory it sits in.
struct SourceInfo {
    name: String,
    dir: PathBuf,
    autoexport: String,
}

fn source_info(path: &Path) -> Result<SourceInfo> {
    let abs = store::canon(path);
    let dir = abs
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let conn = Connection::open_with_flags(&abs, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open {}", abs.display()))?;

    let has = |table: &str| -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
            params![table],
            |_| Ok(()),
        )
        .optional()
        .map(|r| r.is_some())
        .unwrap_or(false)
    };

    let mut name = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    let mut autoexport = String::new();
    if has("projects") {
        if let Some((n, a)) = conn
            .query_row(
                "SELECT name, autoexport FROM projects ORDER BY id LIMIT 1",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        {
            name = n;
            autoexport = a;
        }
    }
    if autoexport.is_empty() && has("meta") {
        autoexport = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'autoexport'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_default();
    }
    Ok(SourceInfo { name, dir, autoexport })
}

/// Import one database. `name` picks or creates the target project; without
/// it the source's own project row (or directory) decides.
pub(crate) fn run(ctx: &Ctx, source: &Path, name: Option<&str>, dry_run: bool) -> Result<Outcome> {
    if !ctx.central {
        bail!("import needs the central store; run `bl init` first, then import from anywhere");
    }
    let src = store::canon(source);
    if !src.is_file() {
        bail!("{} is not a file", src.display());
    }
    if src == store::canon(&ctx.path) {
        bail!("{} is the central store itself", src.display());
    }

    let info = source_info(&src)?;
    let conn = &ctx.conn;
    let now = crate::now_str();

    // Target project: by name, by the source's directory, else a new row.
    let mut created = false;
    let project: Project = match name {
        Some(n) => match store::lookup(conn, n)? {
            Some(p) => p,
            None => {
                if dry_run {
                    Project {
                        id: 0,
                        name: n.to_string(),
                        path: info.dir.display().to_string(),
                        active: true,
                        autoexport: String::new(),
                        created_at: now.clone(),
                    }
                } else {
                    created = true;
                    store::add(conn, &info.dir, Some(n), &now)?
                }
            }
        },
        None => match store::by_path(conn, &info.dir)? {
            Some(p) => p,
            None => {
                if dry_run {
                    Project {
                        id: 0,
                        name: info.name.clone(),
                        path: info.dir.display().to_string(),
                        active: true,
                        autoexport: String::new(),
                        created_at: now.clone(),
                    }
                } else {
                    created = true;
                    // The source's project name may already be taken by an
                    // unrelated repository; the directory name is the fallback.
                    match store::add(conn, &info.dir, Some(&info.name), &now) {
                        Ok(p) => p,
                        Err(_) => store::add(conn, &info.dir, None, &now)?,
                    }
                }
            }
        },
    };

    // In old-id order, so an empty store gives the same numbers back and a
    // non-empty one at least keeps the cards' relative order.
    let mut cards = view::read_cards(&src, None, false)?;
    cards.sort_by_key(|c| c.id);

    let existing: BTreeMap<i64, i64> = {
        let mut stmt = conn.prepare(
            "SELECT legacy_id, id FROM cards WHERE project_id = ? AND legacy_id IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![project.id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let mut out = Outcome {
        source: src.display().to_string(),
        project: project.name.clone(),
        project_id: project.id,
        project_created: created,
        imported: Vec::new(),
        skipped: 0,
        notes: 0,
        autoexport: None,
        dry_run,
    };

    if dry_run {
        for c in &cards {
            if existing.contains_key(&c.id) {
                out.skipped += 1;
            } else {
                out.imported.push((c.id, 0));
                out.notes += c.entries.len().max(if c.notes.is_empty() { 0 } else { 1 });
            }
        }
        if project.autoexport.is_empty() && !info.autoexport.is_empty() {
            out.autoexport = Some(info.autoexport.clone());
        }
        return Ok(out);
    }

    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| -> Result<()> {
        for c in &cards {
            if existing.contains_key(&c.id) {
                out.skipped += 1;
                continue;
            }
            conn.execute(
                "INSERT INTO cards (title, notes, label, status, priority, outcome, claimed_by,
                                    claimed_at, commits, created_at, updated_at, project_id, legacy_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    c.title,
                    c.notes,
                    c.label,
                    c.status,
                    c.priority,
                    c.outcome,
                    c.claimed_by,
                    c.claimed_at,
                    c.commits,
                    c.created_at,
                    c.updated_at,
                    project.id,
                    c.id
                ],
            )?;
            let new_id = conn.last_insert_rowid();
            out.imported.push((c.id, new_id));
            crate::events::record(
                conn, new_id, Some(project.id), "imported",
                &format!("#{} in {}", c.id, out.source), &c.title, "", &now,
            )?;

            if c.entries.is_empty() {
                // A source from before typed notes: the blob is all there is,
                // and reconcile turns its lines into rows as it always has.
                if !c.notes.is_empty() {
                    notes::reconcile(conn, new_id)?;
                    out.notes += c.notes.lines().filter(|l| !l.trim().is_empty()).count();
                }
            } else {
                for n in &c.entries {
                    conn.execute(
                        "INSERT INTO notes (card_id, kind, author, body, commit_sha, commit_subject, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            new_id,
                            n.kind,
                            n.author,
                            n.body,
                            n.commit_sha,
                            n.commit_subject,
                            n.created_at
                        ],
                    )?;
                    out.notes += 1;
                }
            }
        }
        if project.autoexport.is_empty() && !info.autoexport.is_empty() {
            store::set_autoexport(conn, project.id, &info.autoexport)?;
            out.autoexport = Some(info.autoexport.clone());
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT;")?;
            Ok(out)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

/// Every repo-level database worth importing: one beside each registered
/// project, plus `<dir>/*/backlog.db` for each `--scan` directory.
pub(crate) fn candidates(ctx: &Ctx, scan: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let central = store::canon(&ctx.path);
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        let p = store::canon(&p);
        if p != central && p.is_file() && !out.contains(&p) {
            out.push(p);
        }
    };
    for p in store::all(&ctx.conn)? {
        push(PathBuf::from(&p.path).join(crate::DEFAULT_DB));
    }
    for dir in scan {
        let entries = std::fs::read_dir(dir)
            .with_context(|| format!("cannot scan {}", dir.display()))?;
        let mut found: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path().join(crate::DEFAULT_DB))
            .collect();
        found.sort();
        for f in found {
            push(f);
        }
    }
    Ok(out)
}

/// One line per import, for humans.
pub(crate) fn summary(o: &Outcome) -> String {
    let mut s = format!(
        "{}{} → project '{}'{}: {} card(s) imported, {} note(s), {} already present",
        if o.dry_run { "would import " } else { "imported " },
        o.source,
        o.project,
        if o.project_created { " (new)" } else { "" },
        o.imported.len(),
        o.notes,
        o.skipped
    );
    if let Some(a) = &o.autoexport {
        s.push_str(&format!("; auto-export → {}", a));
    }
    s
}

// ---------------------------------------------------------------- stdin

/// One card as a planning agent files it: `{"title": "...", "label": "art",
/// "priority": 7000, "notes": "..." | ["...", {"kind": "finding", "body": "..."}]}`.
#[derive(Debug, serde::Deserialize)]
struct Incoming {
    title: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    notes: Notes,
    /// Another registered project than the scoped one, by name or #id.
    #[serde(default)]
    project: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(untagged)]
enum Notes {
    #[default]
    None,
    One(String),
    Many(Vec<NoteIn>),
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum NoteIn {
    Plain(String),
    Typed {
        body: String,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        by: Option<String>,
    },
}

/// What happened to each incoming card, in input order.
#[derive(Debug, serde::Serialize)]
pub(crate) struct Created {
    pub(crate) id: i64,
    pub(crate) title: String,
    pub(crate) project: String,
    /// False when `--if-absent` found the title already on the board.
    pub(crate) created: bool,
}

/// A JSON array, or one JSON object per line.
fn parse_incoming(text: &str) -> Result<Vec<Incoming>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("nothing on stdin: pipe a JSON array or one JSON object per line");
    }
    if trimmed.starts_with('[') {
        return serde_json::from_str(trimmed).context("stdin is not a JSON array of cards");
    }
    let mut out = Vec::new();
    for (i, line) in trimmed.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let card: Incoming = serde_json::from_str(line)
            .with_context(|| format!("line {} is not a JSON card object", i + 1))?;
        out.push(card);
    }
    Ok(out)
}

/// `bl import --stdin`: file many cards in one transaction. Works in any
/// scope `bl create` works in, including `--db` files.
pub(crate) fn from_stdin(ctx: &Ctx, by: &str, if_absent: bool, dry_run: bool) -> Result<Vec<Created>> {
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).context("failed to read stdin")?;
    let incoming = parse_incoming(&text)?;
    let conn = &ctx.conn;
    let now = crate::now_str();

    // Validate everything before writing anything, so a bad line 17 does not
    // leave 16 cards behind.
    let mut planned: Vec<(Incoming, Project)> = Vec::new();
    for (i, c) in incoming.into_iter().enumerate() {
        if c.title.trim().is_empty() {
            bail!("card {} has an empty title", i + 1);
        }
        crate::check_title(&c.title, false)?;
        if let Some(p) = c.priority {
            if !(0..=10000).contains(&p) {
                bail!("card {} ('{}'): priority must be 0..=10000", i + 1, c.title);
            }
        }
        let project = match &c.project {
            Some(key) => store::lookup(conn, key)?
                .ok_or_else(|| anyhow::anyhow!("card {} ('{}'): no project named '{}'", i + 1, c.title, key))?,
            None => ctx.require_project()?.clone(),
        };
        planned.push((c, project));
    }

    let mut out = Vec::new();
    if dry_run {
        for (c, p) in &planned {
            out.push(Created { id: 0, title: c.title.trim().to_string(), project: p.name.clone(), created: false });
        }
        return Ok(out);
    }

    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| -> Result<()> {
        for (c, project) in &planned {
            let title = c.title.trim().to_string();
            if if_absent {
                let existing: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM cards WHERE title = ?1 AND project_id = ?2 ORDER BY id ASC LIMIT 1",
                        params![title, project.id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(id) = existing {
                    out.push(Created { id, title, project: project.name.clone(), created: false });
                    continue;
                }
            }
            conn.execute(
                "INSERT INTO cards (title, notes, label, priority, created_at, updated_at, project_id)
                 VALUES (?1, '', ?2, ?3, ?4, ?4, ?5)",
                params![title, c.label.trim(), c.priority.unwrap_or(5000), now, project.id],
            )?;
            let id = conn.last_insert_rowid();
            let notes: Vec<(String, String, String)> = match &c.notes {
                Notes::None => Vec::new(),
                Notes::One(s) => s
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| ("note".to_string(), l.trim().to_string(), by.to_string()))
                    .collect(),
                Notes::Many(v) => v
                    .iter()
                    .map(|n| match n {
                        NoteIn::Plain(s) => ("note".to_string(), s.clone(), by.to_string()),
                        NoteIn::Typed { body, kind, by: who } => (
                            kind.clone().unwrap_or_else(|| "note".to_string()),
                            body.clone(),
                            who.clone().unwrap_or_else(|| by.to_string()),
                        ),
                    })
                    .collect(),
            };
            for (kind, body, who) in &notes {
                notes::add(conn, id, kind, who, body, None, false, &now)?;
            }
            crate::events::record(conn, id, Some(project.id), "created", "", &title, by, &now)?;
            out.push(Created { id, title, project: project.name.clone(), created: true });
        }
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT;")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(e);
        }
    }
    Ok(out)
}
