mod board;
mod view;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use rusqlite::{params, Connection, OptionalExtension};
use std::env;
use std::path::{Path, PathBuf};

const DEFAULT_DB: &str = "backlog.db";

// Base schema for fresh databases
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS cards (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    title       TEXT NOT NULL,
    notes       TEXT NOT NULL DEFAULT '',
    label       TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'new'
                CHECK(status IN ('new', 'ready', 'in_progress', 'done')),
    priority    INTEGER NOT NULL DEFAULT 5000
                CHECK(priority BETWEEN 0 AND 10000),
    outcome     TEXT NOT NULL DEFAULT '',
    claimed_by  TEXT NOT NULL DEFAULT '',
    claimed_at  TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_status_priority
    ON cards(status, priority DESC, created_at ASC);
CREATE INDEX IF NOT EXISTS idx_label ON cards(label);
CREATE INDEX IF NOT EXISTS idx_claimed_by ON cards(claimed_by);
"#;

#[derive(Debug, Clone, ValueEnum)]
enum Status {
    New,
    Ready,
    #[value(name = "in_progress")]
    InProgress,
    Done,
}

impl Status {
    fn as_str(&self) -> &'static str {
        match self {
            Status::New => "new",
            Status::Ready => "ready",
            Status::InProgress => "in_progress",
            Status::Done => "done",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Parser, Debug)]
#[command(name = "bl", about = "Lightweight Kanban backlog for Claude Code agents")]
struct Cli {
    /// Path to the SQLite database (default: ./backlog.db or $BL_DB)
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Create the database and schema (safe to re-run; migrates old DBs)
    Init,

    /// Create a new card
    Create {
        title: String,
        #[arg(short, long, default_value = "")]
        label: String,
        #[arg(short, long, default_value_t = 5000)]
        priority: i32,
        #[arg(short, long, default_value = "")]
        notes: String,
    },

    /// Set priority score (0-10000)
    #[command(name = "set-priority")]
    SetPriority {
        id: i64,
        priority: i32,
    },

    /// Update status (and optionally outcome). Clears claim when moving to ready/done/new.
    Status {
        id: i64,
        status: Status,
        #[arg(long, default_value = "")]
        outcome: String,
    },

    /// Atomically claim a card (ready → in_progress). Fails if already claimed.
    Claim {
        id: i64,
        /// Who is claiming (agent name, session id, etc.)
        #[arg(long)]
        by: String,
    },

    /// Release a claim (in_progress → ready). Optionally require matching claimed_by.
    Release {
        id: i64,
        /// Only release if claimed by this agent
        #[arg(long)]
        by: Option<String>,
    },

    /// List cards (filter by label / status, ordered by priority)
    List {
        #[arg(short, long)]
        label: Option<String>,
        /// Comma-separated statuses (e.g. new,ready,in_progress). Default: all non-done
        #[arg(short, long)]
        status: Option<String>,
        #[arg(short, long, default_value_t = 30)]
        limit: i64,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Highest-priority actionable card. With --claim, atomically claims it.
    Next {
        #[arg(short, long)]
        label: Option<String>,
        /// Prefer only 'ready' cards (skip 'new')
        #[arg(long)]
        ready_only: bool,
        /// Atomically claim the card (requires --by)
        #[arg(long)]
        claim: bool,
        /// Agent identity used with --claim
        #[arg(long)]
        by: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Show one card
    Show {
        id: i64,
        #[arg(long)]
        json: bool,
    },

    /// Append a note to a card
    Note {
        id: i64,
        text: String,
    },

    /// Serve a read-only board view of the backlog on localhost
    Serve {
        #[arg(short, long, default_value_t = 7788)]
        port: u16,
        /// Additional database(s) to make selectable in the board
        #[arg(long = "also", value_name = "PATH")]
        also: Vec<PathBuf>,
        /// Open the board in the default browser
        #[arg(long)]
        open: bool,
    },

    /// Write a standalone HTML snapshot of the board (no server needed)
    Export {
        /// Output file (default: view/index.html)
        #[arg(short, long, default_value = "view/index.html")]
        out: PathBuf,
        /// Open the snapshot in the default browser
        #[arg(long)]
        open: bool,
    },

    /// Draw the board in the terminal
    Board {
        #[arg(short, long)]
        label: Option<String>,
        /// How many done cards to show
        #[arg(short, long, default_value_t = 8)]
        done: usize,
        /// Force a column width instead of detecting the terminal's
        #[arg(long)]
        width: Option<usize>,
        /// Redraw every N seconds until interrupted
        #[arg(long, value_name = "SECS", num_args = 0..=1, default_missing_value = "5")]
        watch: Option<u64>,
        /// Disable color
        #[arg(long)]
        no_color: bool,
    },

    /// Decrement priority of all non-done cards (decay)
    Decay {
        #[arg(short, long, default_value_t = 25)]
        amount: i32,
    },
}

fn db_path(cli: &Cli) -> PathBuf {
    if let Some(p) = &cli.db {
        return p.clone();
    }
    if let Ok(p) = env::var("BL_DB") {
        return PathBuf::from(p);
    }
    PathBuf::from(DEFAULT_DB)
}

fn open_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open database at {}", path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    Ok(conn)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(names.iter().any(|n| n == column))
}

fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;

    // Migrate older DBs that lack claim columns / in_progress status
    if !column_exists(conn, "cards", "claimed_by")? {
        conn.execute_batch(
            "ALTER TABLE cards ADD COLUMN claimed_by TEXT NOT NULL DEFAULT '';
             ALTER TABLE cards ADD COLUMN claimed_at TEXT NOT NULL DEFAULT '';",
        )?;
    }

    // Detect old CHECK constraint (no in_progress) via sqlite_master, then rebuild table.
    let table_sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='cards'",
            [],
            |r| r.get(0),
        )
        .unwrap_or_default();
    let needs_status_migrate =
        !table_sql.is_empty() && !table_sql.contains("in_progress");

    if needs_status_migrate {
        conn.execute_batch(
            r#"
            BEGIN;
            CREATE TABLE cards_new (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                title       TEXT NOT NULL,
                notes       TEXT NOT NULL DEFAULT '',
                label       TEXT NOT NULL DEFAULT '',
                status      TEXT NOT NULL DEFAULT 'new'
                            CHECK(status IN ('new', 'ready', 'in_progress', 'done')),
                priority    INTEGER NOT NULL DEFAULT 5000
                            CHECK(priority BETWEEN 0 AND 10000),
                outcome     TEXT NOT NULL DEFAULT '',
                claimed_by  TEXT NOT NULL DEFAULT '',
                claimed_at  TEXT NOT NULL DEFAULT '',
                created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );
            INSERT INTO cards_new (id, title, notes, label, status, priority, outcome, claimed_by, claimed_at, created_at, updated_at)
            SELECT id, title, notes, label, status, priority, outcome,
                   COALESCE(claimed_by, ''), COALESCE(claimed_at, ''),
                   created_at, updated_at
            FROM cards;
            DROP TABLE cards;
            ALTER TABLE cards_new RENAME TO cards;
            CREATE INDEX IF NOT EXISTS idx_status_priority ON cards(status, priority DESC, created_at ASC);
            CREATE INDEX IF NOT EXISTS idx_label ON cards(label);
            CREATE INDEX IF NOT EXISTS idx_claimed_by ON cards(claimed_by);
            COMMIT;
            "#,
        )?;
    }

    Ok(())
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct Card {
    pub(crate) id: i64,
    pub(crate) title: String,
    pub(crate) notes: String,
    pub(crate) label: String,
    pub(crate) status: String,
    pub(crate) priority: i32,
    pub(crate) outcome: String,
    pub(crate) claimed_by: String,
    pub(crate) claimed_at: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

pub(crate) fn row_to_card(row: &rusqlite::Row<'_>) -> rusqlite::Result<Card> {
    Ok(Card {
        id: row.get(0)?,
        title: row.get(1)?,
        notes: row.get(2)?,
        label: row.get(3)?,
        status: row.get(4)?,
        priority: row.get(5)?,
        outcome: row.get(6)?,
        claimed_by: row.get(7)?,
        claimed_at: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

pub(crate) const SELECT_COLS: &str =
    "id, title, notes, label, status, priority, outcome, claimed_by, claimed_at, created_at, updated_at";

fn print_card(c: &Card, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(c).unwrap());
    } else {
        let claim = if c.claimed_by.is_empty() {
            String::new()
        } else {
            format!("  claimed_by={}", c.claimed_by)
        };
        println!(
            "#{}  [{:>5}]  {:12}  {}{}{}",
            c.id,
            c.priority,
            c.status,
            if c.label.is_empty() {
                String::new()
            } else {
                format!("[{}] ", c.label)
            },
            c.title,
            claim
        );
        if !c.notes.is_empty() {
            println!("    notes: {}", c.notes.replace('\n', " | "));
        }
        if !c.outcome.is_empty() {
            println!("    outcome: {}", c.outcome.replace('\n', " | "));
        }
        if !c.claimed_at.is_empty() {
            println!("    claimed_at: {}", c.claimed_at);
        }
        println!("    created: {}   updated: {}", c.created_at, c.updated_at);
    }
}

/// Databases the board may read: the primary one plus any `--also` paths.
/// Creates/migrates the primary so a fresh project still opens to a board.
fn view_sources(path: &Path, also: Vec<PathBuf>) -> Result<Vec<view::Source>> {
    let conn = open_db(path)?;
    ensure_schema(&conn)?;
    drop(conn);

    let mut sources = vec![view::Source {
        label: source_label(path),
        path: path.to_path_buf(),
    }];
    for p in also {
        sources.push(view::Source {
            label: source_label(&p),
            path: p,
        });
    }
    Ok(sources)
}

/// Short human label for a database: `<parent dir>/<file>` when we can get it.
fn source_label(path: &Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let file = abs
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| abs.display().to_string());
    match abs.parent().and_then(|p| p.file_name()) {
        Some(dir) => format!("{}/{}", dir.to_string_lossy(), file),
        None => file,
    }
}

fn now_str() -> String {
    Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = db_path(&cli);

    match cli.command {
        Commands::Init => {
            if path.exists() {
                println!("database already exists: {}", path.display());
            }
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            println!("initialized {}", path.display());
        }

        Commands::Create {
            title,
            label,
            priority,
            notes,
        } => {
            if !(0..=10000).contains(&priority) {
                bail!("priority must be 0..=10000");
            }
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            let now = now_str();
            conn.execute(
                "INSERT INTO cards (title, notes, label, priority, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![title, notes, label, priority, now],
            )?;
            let id = conn.last_insert_rowid();
            println!("created #{}  priority={}  label={}", id, priority, label);
        }

        Commands::SetPriority { id, priority } => {
            if !(0..=10000).contains(&priority) {
                bail!("priority must be 0..=10000");
            }
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            let now = now_str();
            let n = conn.execute(
                "UPDATE cards SET priority = ?1, updated_at = ?2 WHERE id = ?3",
                params![priority, now, id],
            )?;
            if n == 0 {
                bail!("card #{} not found", id);
            }
            println!("#{} priority → {}", id, priority);
        }

        Commands::Status { id, status, outcome } => {
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            let now = now_str();
            // Clear claim when leaving in_progress (or explicitly setting ready/new/done)
            let clear_claim = matches!(status, Status::New | Status::Ready | Status::Done);
            let n = if outcome.is_empty() {
                if clear_claim {
                    conn.execute(
                        "UPDATE cards SET status = ?1, claimed_by = '', claimed_at = '', updated_at = ?2 WHERE id = ?3",
                        params![status.as_str(), now, id],
                    )?
                } else {
                    conn.execute(
                        "UPDATE cards SET status = ?1, updated_at = ?2 WHERE id = ?3",
                        params![status.as_str(), now, id],
                    )?
                }
            } else if clear_claim {
                conn.execute(
                    "UPDATE cards SET status = ?1, outcome = ?2, claimed_by = '', claimed_at = '', updated_at = ?3 WHERE id = ?4",
                    params![status.as_str(), outcome, now, id],
                )?
            } else {
                conn.execute(
                    "UPDATE cards SET status = ?1, outcome = ?2, updated_at = ?3 WHERE id = ?4",
                    params![status.as_str(), outcome, now, id],
                )?
            };
            if n == 0 {
                bail!("card #{} not found", id);
            }
            println!("#{} status → {}", id, status);
        }

        Commands::Claim { id, by } => {
            if by.trim().is_empty() {
                bail!("--by must be a non-empty agent identity");
            }
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            let now = now_str();
            // Atomic claim: only if currently ready (or new) and unclaimed
            let n = conn.execute(
                "UPDATE cards
                 SET status = 'in_progress',
                     claimed_by = ?1,
                     claimed_at = ?2,
                     updated_at = ?2
                 WHERE id = ?3
                   AND status IN ('new', 'ready')
                   AND (claimed_by = '' OR claimed_by IS NULL)",
                params![by, now, id],
            )?;
            if n == 0 {
                // Diagnose why
                let row: Option<(String, String)> = conn
                    .query_row(
                        "SELECT status, claimed_by FROM cards WHERE id = ?",
                        params![id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                match row {
                    None => bail!("card #{} not found", id),
                    Some((st, cb)) if st == "in_progress" || !cb.is_empty() => {
                        bail!("card #{} already claimed by '{}'", id, if cb.is_empty() { "?" } else { &cb })
                    }
                    Some((st, _)) => {
                        bail!("card #{} is status '{}' (must be new or ready to claim)", id, st)
                    }
                }
            }
            println!("#{} claimed by {}", id, by);
        }

        Commands::Release { id, by } => {
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            let now = now_str();
            let n = if let Some(ref agent) = by {
                conn.execute(
                    "UPDATE cards
                     SET status = 'ready',
                         claimed_by = '',
                         claimed_at = '',
                         updated_at = ?1
                     WHERE id = ?2
                       AND status = 'in_progress'
                       AND claimed_by = ?3",
                    params![now, id, agent],
                )?
            } else {
                conn.execute(
                    "UPDATE cards
                     SET status = 'ready',
                         claimed_by = '',
                         claimed_at = '',
                         updated_at = ?1
                     WHERE id = ?2
                       AND status = 'in_progress'",
                    params![now, id],
                )?
            };
            if n == 0 {
                let row: Option<(String, String)> = conn
                    .query_row(
                        "SELECT status, claimed_by FROM cards WHERE id = ?",
                        params![id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                match row {
                    None => bail!("card #{} not found", id),
                    Some((st, cb)) => {
                        if let Some(agent) = by {
                            bail!(
                                "card #{} not released (status={}, claimed_by='{}', required by='{}')",
                                id, st, cb, agent
                            );
                        }
                        bail!("card #{} is not in_progress (status={})", id, st);
                    }
                }
            }
            println!("#{} released → ready", id);
        }

        Commands::List {
            label,
            status,
            limit,
            json,
        } => {
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;

            let mut sql = format!(
                "SELECT {} FROM cards WHERE 1=1",
                SELECT_COLS
            );
            let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

            if let Some(l) = &label {
                sql.push_str(" AND label = ?");
                binds.push(Box::new(l.clone()));
            }

            if let Some(s) = &status {
                let statuses: Vec<&str> = s
                    .split(',')
                    .map(|x| x.trim())
                    .filter(|x| !x.is_empty())
                    .collect();
                if !statuses.is_empty() {
                    let placeholders: Vec<String> =
                        statuses.iter().map(|_| "?".to_string()).collect();
                    sql.push_str(&format!(" AND status IN ({})", placeholders.join(",")));
                    for st in statuses {
                        binds.push(Box::new(st.to_string()));
                    }
                }
            } else {
                sql.push_str(" AND status != 'done'");
            }

            sql.push_str(" ORDER BY priority DESC, created_at ASC LIMIT ?");
            binds.push(Box::new(limit));

            let mut stmt = conn.prepare(&sql)?;
            let params_ref: Vec<&dyn rusqlite::ToSql> =
                binds.iter().map(|b| b.as_ref()).collect();
            let cards: Vec<Card> = stmt
                .query_map(params_ref.as_slice(), row_to_card)?
                .filter_map(|r| r.ok())
                .collect();

            if json {
                println!("{}", serde_json::to_string_pretty(&cards)?);
            } else if cards.is_empty() {
                println!("(no cards)");
            } else {
                for c in &cards {
                    print_card(c, false);
                    println!();
                }
                println!("{} card(s)", cards.len());
            }
        }

        Commands::Next {
            label,
            ready_only,
            claim,
            by,
            json,
        } => {
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;

            if claim {
                let agent = match &by {
                    Some(b) if !b.trim().is_empty() => b.clone(),
                    _ => bail!("--claim requires --by <agent-id>"),
                };
                let now = now_str();

                // Pick highest-priority new/ready unclaimed card, then claim in one transaction
                conn.execute_batch("BEGIN IMMEDIATE;")?;

                let mut sql = String::from(
                    "SELECT id FROM cards WHERE status IN ('ready'",
                );
                if !ready_only {
                    sql.push_str(", 'new'");
                }
                sql.push_str(") AND (claimed_by = '' OR claimed_by IS NULL)");

                let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
                if let Some(l) = &label {
                    sql.push_str(" AND label = ?");
                    binds.push(Box::new(l.clone()));
                }
                sql.push_str(" ORDER BY priority DESC, created_at ASC LIMIT 1");

                let id: Option<i64> = {
                    let mut stmt = conn.prepare(&sql)?;
                    let params_ref: Vec<&dyn rusqlite::ToSql> =
                        binds.iter().map(|b| b.as_ref()).collect();
                    stmt.query_row(params_ref.as_slice(), |r| r.get(0))
                        .optional()?
                };

                let Some(id) = id else {
                    conn.execute_batch("ROLLBACK;")?;
                    if json {
                        println!("null");
                    } else {
                        println!("(no matching card)");
                    }
                    return Ok(());
                };

                let n = conn.execute(
                    "UPDATE cards
                     SET status = 'in_progress',
                         claimed_by = ?1,
                         claimed_at = ?2,
                         updated_at = ?2
                     WHERE id = ?3
                       AND status IN ('new', 'ready')
                       AND (claimed_by = '' OR claimed_by IS NULL)",
                    params![agent, now, id],
                )?;

                if n == 0 {
                    conn.execute_batch("ROLLBACK;")?;
                    bail!("failed to claim #{} (race?)", id);
                }

                conn.execute_batch("COMMIT;")?;

                let card: Card = conn.query_row(
                    &format!("SELECT {} FROM cards WHERE id = ?", SELECT_COLS),
                    params![id],
                    row_to_card,
                )?;
                print_card(&card, json);
            } else {
                // Read-only next (no claim)
                let mut sql = format!(
                    "SELECT {} FROM cards WHERE status IN ('ready'",
                    SELECT_COLS
                );
                if !ready_only {
                    sql.push_str(", 'new'");
                }
                sql.push(')');

                let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
                if let Some(l) = &label {
                    sql.push_str(" AND label = ?");
                    binds.push(Box::new(l.clone()));
                }
                sql.push_str(" ORDER BY priority DESC, created_at ASC LIMIT 1");

                let mut stmt = conn.prepare(&sql)?;
                let params_ref: Vec<&dyn rusqlite::ToSql> =
                    binds.iter().map(|b| b.as_ref()).collect();
                let card: Option<Card> = stmt
                    .query_row(params_ref.as_slice(), row_to_card)
                    .optional()?;

                match card {
                    Some(c) => print_card(&c, json),
                    None => {
                        if json {
                            println!("null");
                        } else {
                            println!("(no matching card)");
                        }
                    }
                }
            }
        }

        Commands::Show { id, json } => {
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            let card: Option<Card> = conn
                .query_row(
                    &format!("SELECT {} FROM cards WHERE id = ?", SELECT_COLS),
                    params![id],
                    row_to_card,
                )
                .optional()?;
            match card {
                Some(c) => print_card(&c, json),
                None => bail!("card #{} not found", id),
            }
        }

        Commands::Note { id, text } => {
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            let now = now_str();
            let existing: Option<String> = conn
                .query_row(
                    "SELECT notes FROM cards WHERE id = ?",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(old) = existing else {
                bail!("card #{} not found", id);
            };
            let new_notes = if old.is_empty() {
                text
            } else {
                format!("{}\n{}", old, text)
            };
            conn.execute(
                "UPDATE cards SET notes = ?1, updated_at = ?2 WHERE id = ?3",
                params![new_notes, now, id],
            )?;
            println!("#{} note appended", id);
        }

        Commands::Serve { port, also, open } => {
            let sources = view_sources(&path, also)?;
            view::serve(sources, port, open)?;
        }

        Commands::Export { out, open } => {
            let sources = view_sources(&path, Vec::new())?;
            view::export(sources, &out)?;
            if open {
                let abs = out.canonicalize().unwrap_or(out);
                view::open_in_browser(&abs.display().to_string());
            }
        }

        Commands::Board {
            label,
            done,
            width,
            watch,
            no_color,
        } => {
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            drop(conn);
            board::run(
                &path,
                &board::Opts {
                    label,
                    done,
                    width,
                    watch,
                    color: !no_color,
                },
            )?;
        }

        Commands::Decay { amount } => {
            if amount < 0 {
                bail!("amount must be >= 0");
            }
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            let now = now_str();
            let n = conn.execute(
                "UPDATE cards
                 SET priority = MAX(0, priority - ?1),
                     updated_at = ?2
                 WHERE status != 'done'",
                params![amount, now],
            )?;
            println!("decayed {} non-done card(s) by {}", n, amount);
        }
    }

    Ok(())
}
