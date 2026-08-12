mod board;
mod notes;
mod view;

use notes::Note;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use rusqlite::{params, Connection, OptionalExtension};
use std::env;
use std::path::{Path, PathBuf};

const DEFAULT_DB: &str = "backlog.db";

/// Exit codes an agent loop can branch on without parsing output.
/// 0 success · 1 error · 2 nothing matched · 3 someone else holds the claim.
const EXIT_EMPTY: i32 = 2;
const EXIT_CONTENDED: i32 = 3;

/// Print anything buffered, then leave with a code the caller can test.
fn exit_with(code: i32) -> ! {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::process::exit(code)
}

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
    commits     TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_status_priority
    ON cards(status, priority DESC, created_at ASC);
CREATE INDEX IF NOT EXISTS idx_label ON cards(label);
CREATE INDEX IF NOT EXISTS idx_claimed_by ON cards(claimed_by);

-- Per-backlog settings, e.g. the snapshot path kept in sync on every write.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL DEFAULT ''
);

-- One row per note. cards.notes is kept as a rendered mirror of these rows so
-- an older `bl` (or anything reading the table directly) still sees the notes.
CREATE TABLE IF NOT EXISTS notes (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    card_id        INTEGER NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL DEFAULT 'note',
    author         TEXT NOT NULL DEFAULT '',
    body           TEXT NOT NULL,
    commit_sha     TEXT NOT NULL DEFAULT '',
    commit_subject TEXT NOT NULL DEFAULT '',
    created_at     TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_notes_card ON notes(card_id, id);
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
        /// If a card with this exact title exists, print its id and create nothing
        #[arg(long)]
        if_absent: bool,
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
        /// `-l` is taken by --label, so the limit is `-n`.
        #[arg(short = 'n', long, default_value_t = 30)]
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

    /// Append a note to a card, optionally typed and linked to a git commit
    Note {
        id: i64,
        text: String,
        /// What kind of note: note, finding, decision, blocker, attempt, …
        #[arg(short, long, default_value = "note")]
        kind: String,
        /// Who is writing (agent id)
        #[arg(long)]
        by: Option<String>,
        /// Link a commit to the card. Bare `--commit` uses HEAD; otherwise any
        /// revision git understands (sha, tag, HEAD~1). Ignored if not a repo.
        #[arg(long, num_args = 0..=1, default_missing_value = "HEAD", value_name = "REV")]
        commit: Option<String>,
        /// Skip if this card already has an identical note (retry-safe)
        #[arg(long)]
        unique: bool,
    },

    /// List a card's notes
    Notes {
        id: i64,
        /// Only this kind
        #[arg(short, long)]
        kind: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Search titles, notes, outcomes and labels
    Search {
        /// Words to look for; a card must match every one of them
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[arg(short, long)]
        label: Option<String>,
        /// Only open cards (default searches done cards too)
        #[arg(long)]
        open: bool,
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },

    /// Return claims that no one is working on any more
    Reap {
        /// How idle a claim must be: 90s, 30m, 2h, 1d (bare number = minutes)
        #[arg(long, default_value = "30m", value_name = "DURATION")]
        older_than: String,
        /// Report what would be released, change nothing
        #[arg(long)]
        dry_run: bool,
    },

    /// Keep a claim alive while a long job runs
    Heartbeat {
        id: i64,
        #[arg(long)]
        by: String,
    },

    /// Print instructions that teach an agent to use this backlog
    Prompt {
        /// Write to a file instead of stdout (e.g. CLAUDE.md, .claude/skills/bl/SKILL.md)
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Append to the file rather than replacing it
        #[arg(long)]
        append: bool,
    },

    /// Keep an HTML snapshot in sync automatically after every write
    Auto {
        #[command(subcommand)]
        action: AutoAction,
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
        /// Also keep this file in sync after every future write
        #[arg(long)]
        auto: bool,
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

#[derive(Subcommand, Debug)]
enum AutoAction {
    /// Refresh a snapshot on every write (writes it once, now)
    On {
        /// Snapshot to keep current (default: view/index.html)
        #[arg(short, long, default_value = "view/index.html")]
        out: PathBuf,
    },
    /// Stop refreshing
    Off,
    /// Show what is being kept in sync
    Status,
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

    // Linked git commits arrived after the first databases were created.
    if !column_exists(conn, "cards", "commits")? {
        conn.execute_batch("ALTER TABLE cards ADD COLUMN commits TEXT NOT NULL DEFAULT '';")?;
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
                commits     TEXT NOT NULL DEFAULT '',
                created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );
            INSERT INTO cards_new (id, title, notes, label, status, priority, outcome, claimed_by, claimed_at, commits, created_at, updated_at)
            SELECT id, title, notes, label, status, priority, outcome,
                   COALESCE(claimed_by, ''), COALESCE(claimed_at, ''),
                   COALESCE(commits, ''),
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
    /// One `sha\tsubject` per line, oldest first.
    pub(crate) commits: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    /// The notes as rows. Empty unless the caller asked for them, and always
    /// empty for a database old enough to lack the table.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) entries: Vec<Note>,
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
        commits: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        entries: Vec::new(),
    })
}

/// Fill in each card's notes. Skipped silently where the table is absent, so a
/// legacy database still lists and serves.
pub(crate) fn load_entries(conn: &Connection, cards: &mut [Card]) {
    if !notes::table_exists(conn) {
        return;
    }
    for c in cards.iter_mut() {
        c.entries = notes::list(conn, c.id).unwrap_or_default();
    }
}

pub(crate) const SELECT_COLS: &str =
    "id, title, notes, label, status, priority, outcome, claimed_by, claimed_at, commits, created_at, updated_at";

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
        for line in c.commits.lines().filter(|l| !l.trim().is_empty()) {
            let (sha, subject) = line.split_once('\t').unwrap_or((line, ""));
            println!("    commit: {} {}", sha, subject);
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

// ---------------------------------------------------------------- settings

fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?", params![key], |r| {
            r.get::<_, String>(0)
        })
        .optional()?)
}

fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Absolute, so the snapshot lands in the same place whatever directory an
/// agent happens to be standing in.
fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    env::current_dir()
        .map(|d| d.join(p))
        .unwrap_or_else(|_| p.to_path_buf())
}

/// Where — if anywhere — the HTML snapshot should be kept in sync.
/// `BL_AUTOEXPORT` overrides the stored path; `BL_NO_AUTOEXPORT` turns it off.
fn autoexport_target(conn: &Connection) -> Option<PathBuf> {
    if env::var_os("BL_NO_AUTOEXPORT").is_some() {
        return None;
    }
    if let Ok(p) = env::var("BL_AUTOEXPORT") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    meta_get(conn, "autoexport")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Rewrite the snapshot after a write, so the view pages never serve stale
/// cards. Best-effort: a failed refresh must not fail the command that
/// already committed.
fn refresh_views(conn: &Connection, db: &Path) {
    let Some(out) = autoexport_target(conn) else {
        return;
    };
    if let Err(e) = view::refresh(db, &out) {
        eprintln!("bl: auto-export to {} failed: {}", out.display(), e);
    }
}

// ---------------------------------------------------------------- search

/// The lines of a card that actually contain a search term, so a hit shows why
/// it matched instead of making the reader re-read the whole card.
fn matching_lines(c: &Card, terms: &[String]) -> Vec<String> {
    let hit = |s: &str| {
        let low = s.to_lowercase();
        terms.iter().any(|t| low.contains(&t.to_lowercase()))
    };
    let mut out = Vec::new();
    for line in c.notes.lines().chain(c.outcome.lines()) {
        let line = line.trim();
        if !line.is_empty() && hit(line) {
            out.push(if line.chars().count() > 140 {
                let cut: String = line.chars().take(137).collect();
                format!("{}...", cut)
            } else {
                line.to_string()
            });
        }
    }
    out.truncate(4);
    out
}

/// `90s`, `30m`, `2h`, `1d`; a bare number means minutes. Returns seconds.
fn parse_duration(s: &str) -> Result<i64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    let (num, mult) = match s.chars().last().unwrap() {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86400),
        _ => (s, 60),
    };
    let n: i64 = num
        .trim()
        .parse()
        .with_context(|| format!("bad duration '{}' (try 90s, 30m, 2h, 1d)", s))?;
    if n < 0 {
        bail!("duration must not be negative");
    }
    Ok(n * mult)
}

// ---------------------------------------------------------------- prompt

const AGENT_PROMPT: &str = include_str!("agent.md");

/// The agent instructions, filled in with this backlog's actual path, labels and
/// view setup — a generic prompt makes an agent guess at exactly those things.
fn agent_prompt(conn: &Connection, db: &Path) -> Result<String> {
    let labels: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT label, COUNT(*) FROM cards
             WHERE label != '' AND status != 'done'
             GROUP BY label ORDER BY COUNT(*) DESC, label ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(format!("{} ({})", r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let collected: Vec<String> = rows.filter_map(|r| r.ok()).collect();
        collected
    };
    let labels = if labels.is_empty() {
        String::new()
    } else {
        format!(
            "\n## Labels in use\n\n{}\n\nReuse one of these rather than inventing a near-duplicate.\n",
            labels.join(", ")
        )
    };

    let auto = match autoexport_target(conn) {
        Some(t) => format!(
            "\nThe board at `{}` refreshes itself on every write — never run `bl export`.\n",
            t.display()
        ),
        None => String::new(),
    };

    Ok(AGENT_PROMPT
        .replace("{{DB}}", &absolute(db).display().to_string())
        .replace("{{AUTO}}", &auto)
        .replace("{{LABELS}}", &labels))
}

// ---------------------------------------------------------------- git

/// Directory to run git in: the backlog's own directory, since that is the
/// repository the cards are about.
fn git_dir(db: &Path) -> PathBuf {
    absolute(db)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Resolve a revision (default `HEAD`) to `(short sha, subject)`.
fn resolve_commit(db: &Path, rev: &str) -> Result<(String, String)> {
    let rev = if rev.trim().is_empty() { "HEAD" } else { rev.trim() };
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(git_dir(db))
        .args(["--no-pager", "log", "-1", "--format=%h%x09%s", rev, "--"])
        .output()
        .context("failed to run git (is it installed and on PATH?)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "git could not resolve '{}': {}",
            rev,
            err.trim().lines().next().unwrap_or("not a git repository?")
        );
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (sha, subject) = line.split_once('\t').unwrap_or((line.as_str(), ""));
    if sha.is_empty() {
        bail!("git returned no commit for '{}'", rev);
    }
    Ok((sha.to_string(), subject.to_string()))
}

/// Append `sha\tsubject` unless that sha is already linked.
fn link_commit(existing: &str, sha: &str, subject: &str) -> String {
    if existing
        .lines()
        .any(|l| l.split('\t').next().unwrap_or("") == sha)
    {
        return existing.to_string();
    }
    let entry = format!("{}\t{}", sha, subject);
    if existing.trim().is_empty() {
        entry
    } else {
        format!("{}\n{}", existing.trim_end(), entry)
    }
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
            notes: notes_text,
            if_absent,
        } => {
            if !(0..=10000).contains(&priority) {
                bail!("priority must be 0..=10000");
            }
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            let now = now_str();

            if if_absent {
                let existing: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM cards WHERE title = ? ORDER BY id ASC LIMIT 1",
                        params![title],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(id) = existing {
                    println!("#{} already exists: {}", id, title);
                    return Ok(());
                }
            }
            conn.execute(
                "INSERT INTO cards (title, notes, label, priority, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![title, notes_text, label, priority, now],
            )?;
            let id = conn.last_insert_rowid();
            // Notes given at creation become a first row like any other.
            if !notes_text.is_empty() {
                notes::reconcile(&conn, id)?;
            }
            println!("created #{}  priority={}  label={}", id, priority, label);
            refresh_views(&conn, &path);
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
            refresh_views(&conn, &path);
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
            refresh_views(&conn, &path);
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
                    {
                        // Contention, not a failure: the loop should move on.
                        eprintln!(
                            "bl: card #{} already claimed by '{}'",
                            id,
                            if cb.is_empty() { "?" } else { &cb }
                        );
                        exit_with(EXIT_CONTENDED);
                    }
                    }
                    Some((st, _)) => {
                        bail!("card #{} is status '{}' (must be new or ready to claim)", id, st)
                    }
                }
            }
            println!("#{} claimed by {}", id, by);
            refresh_views(&conn, &path);
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
            refresh_views(&conn, &path);
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
                    // Nothing to claim: a distinct code so `while bl next
                    // --claim --by me; do …; done` ends on its own.
                    exit_with(EXIT_EMPTY);
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
                refresh_views(&conn, &path);

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
                    Some(mut c) => {
                        if json {
                            load_entries(&conn, std::slice::from_mut(&mut c));
                        }
                        print_card(&c, json);
                    }
                    None => {
                        if json {
                            println!("null");
                        } else {
                            println!("(no matching card)");
                        }
                        exit_with(EXIT_EMPTY);
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
                Some(mut c) => {
                    notes::reconcile(&conn, id)?;
                    if json {
                        load_entries(&conn, std::slice::from_mut(&mut c));
                    }
                    print_card(&c, json);
                }
                None => bail!("card #{} not found", id),
            }
        }

        Commands::Note {
            id,
            text,
            kind,
            by,
            commit,
            unique,
        } => {
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            let now = now_str();
            let old_commits: String = conn
                .query_row("SELECT commits FROM cards WHERE id = ?", params![id], |r| {
                    r.get(0)
                })
                .optional()?
                .ok_or_else(|| anyhow::anyhow!("card #{} not found", id))?;

            // A backlog outside a repository still deserves its note: warn and
            // keep going rather than losing what the agent wanted to record.
            let linked = match commit.as_deref().map(|rev| resolve_commit(&path, rev)) {
                Some(Ok(pair)) => Some(pair),
                Some(Err(e)) => {
                    eprintln!("bl: no commit linked ({})", e);
                    None
                }
                None => None,
            };

            let added = notes::add(
                &conn,
                id,
                &kind,
                by.as_deref().unwrap_or(""),
                &text,
                linked.clone(),
                unique,
                &now,
            )?;

            if added.is_none() {
                println!("#{} already has that note", id);
                return Ok(());
            }

            match &linked {
                Some((sha, subject)) => {
                    let commits = link_commit(&old_commits, sha, subject);
                    conn.execute(
                        "UPDATE cards SET commits = ?1 WHERE id = ?2",
                        params![commits, id],
                    )?;
                    println!("#{} [{}] note added (commit {})", id, kind, sha);
                }
                None => println!("#{} [{}] note added", id, kind),
            }
            refresh_views(&conn, &path);
        }

        Commands::Notes { id, kind, json } => {
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            let exists: Option<i64> = conn
                .query_row("SELECT id FROM cards WHERE id = ?", params![id], |r| r.get(0))
                .optional()?;
            if exists.is_none() {
                bail!("card #{} not found", id);
            }
            notes::reconcile(&conn, id)?;
            let all = notes::list(&conn, id)?;
            let shown: Vec<&Note> = all
                .iter()
                .filter(|n| kind.as_ref().map(|k| &n.kind == k).unwrap_or(true))
                .collect();

            if json {
                println!("{}", serde_json::to_string_pretty(&shown)?);
                if shown.is_empty() {
                    exit_with(EXIT_EMPTY);
                }
            } else if shown.is_empty() {
                println!("(no notes)");
                exit_with(EXIT_EMPTY);
            } else {
                for n in shown {
                    let who = if n.author.is_empty() {
                        String::new()
                    } else {
                        format!("  {}", n.author)
                    };
                    println!("[{}] {}{}", n.kind, n.created_at, who);
                    println!("    {}", n.body.replace('\n', "\n    "));
                    if !n.commit_sha.is_empty() {
                        println!("    commit: {} {}", n.commit_sha, n.commit_subject);
                    }
                }
            }
        }

        Commands::Search {
            query,
            label,
            open,
            limit,
            json,
        } => {
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;

            // Every word must appear somewhere on the card, so "hero art"
            // finds a card whose title says Hero and whose notes say art.
            let mut sql = format!("SELECT {} FROM cards WHERE 1=1", SELECT_COLS);
            let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            for term in &query {
                sql.push_str(
                    " AND (lower(title) LIKE ?
                           OR lower(notes) LIKE ?
                           OR lower(outcome) LIKE ?
                           OR lower(label) LIKE ?
                           OR EXISTS (SELECT 1 FROM notes n
                                      WHERE n.card_id = cards.id AND lower(n.body) LIKE ?))",
                );
                let like = format!("%{}%", term.to_lowercase());
                for _ in 0..5 {
                    binds.push(Box::new(like.clone()));
                }
            }
            if let Some(l) = &label {
                sql.push_str(" AND label = ?");
                binds.push(Box::new(l.clone()));
            }
            if open {
                sql.push_str(" AND status != 'done'");
            }
            sql.push_str(" ORDER BY priority DESC, created_at ASC LIMIT ?");
            binds.push(Box::new(limit));

            let mut cards: Vec<Card> = {
                let mut stmt = conn.prepare(&sql)?;
                let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
                let rows = stmt.query_map(refs.as_slice(), row_to_card)?;
                let collected: Vec<Card> = rows.filter_map(|r| r.ok()).collect();
                collected
            };

            if json {
                load_entries(&conn, &mut cards);
                println!("{}", serde_json::to_string_pretty(&cards)?);
                if cards.is_empty() {
                    exit_with(EXIT_EMPTY);
                }
            } else if cards.is_empty() {
                println!("(no matches for {})", query.join(" "));
                exit_with(EXIT_EMPTY);
            } else {
                for c in &cards {
                    print_card(c, false);
                    for line in matching_lines(c, &query) {
                        println!("    match: {}", line);
                    }
                    println!();
                }
                println!("{} match(es)", cards.len());
            }
        }

        Commands::Reap {
            older_than,
            dry_run,
        } => {
            let secs = parse_duration(&older_than)?;
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            let cutoff = format!("-{} seconds", secs);

            let stale: Vec<(i64, String, String)> = {
                let mut stmt = conn.prepare(
                    "SELECT id, claimed_by, claimed_at FROM cards
                     WHERE status = 'in_progress'
                       AND claimed_at != ''
                       AND claimed_at <= datetime('now', ?)
                     ORDER BY id ASC",
                )?;
                let rows = stmt.query_map(params![cutoff], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
                let collected: Vec<(i64, String, String)> = rows.filter_map(|r| r.ok()).collect();
                collected
            };

            if stale.is_empty() {
                println!("nothing stale (no claim idle over {})", older_than);
                exit_with(EXIT_EMPTY);
            }

            let now = now_str();
            for (id, who, since) in &stale {
                if dry_run {
                    println!("#{} would be released (claimed by {} at {})", id, who, since);
                    continue;
                }
                conn.execute(
                    "UPDATE cards
                     SET status = 'ready', claimed_by = '', claimed_at = '', updated_at = ?1
                     WHERE id = ?2 AND status = 'in_progress' AND claimed_at = ?3",
                    params![now, id, since],
                )?;
                notes::add(
                    &conn,
                    *id,
                    "reaped",
                    "",
                    &format!("claim by {} released: idle since {} UTC", who, since),
                    None,
                    false,
                    &now,
                )?;
                println!("#{} released → ready (was {})", id, who);
            }
            if !dry_run {
                refresh_views(&conn, &path);
            }
        }

        Commands::Heartbeat { id, by } => {
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            let now = now_str();
            let n = conn.execute(
                "UPDATE cards SET claimed_at = ?1 WHERE id = ?2
                   AND status = 'in_progress' AND claimed_by = ?3",
                params![now, id, by],
            )?;
            if n == 0 {
                eprintln!("bl: #{} is not claimed by {} any more", id, by);
                exit_with(EXIT_CONTENDED);
            }
            println!("#{} claim refreshed for {}", id, by);
        }

        Commands::Prompt { out, append } => {
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            let text = agent_prompt(&conn, &path)?;
            match out {
                None => print!("{}", text),
                Some(file) => {
                    if let Some(dir) = file.parent() {
                        if !dir.as_os_str().is_empty() {
                            std::fs::create_dir_all(dir)?;
                        }
                    }
                    if append {
                        use std::io::Write;
                        let mut f = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&file)
                            .with_context(|| format!("failed to open {}", file.display()))?;
                        write!(f, "\n{}", text)?;
                    } else {
                        std::fs::write(&file, &text)
                            .with_context(|| format!("failed to write {}", file.display()))?;
                    }
                    println!(
                        "{} {}",
                        if append { "appended to" } else { "wrote" },
                        file.display()
                    );
                }
            }
        }

        Commands::Auto { action } => {
            let conn = open_db(&path)?;
            ensure_schema(&conn)?;
            match action {
                AutoAction::On { out } => {
                    let target = absolute(&out);
                    meta_set(&conn, "autoexport", &target.display().to_string())?;
                    view::refresh(&path, &target)?;
                    println!("auto-export on → {}", target.display());
                }
                AutoAction::Off => {
                    meta_set(&conn, "autoexport", "")?;
                    println!("auto-export off");
                }
                AutoAction::Status => match autoexport_target(&conn) {
                    Some(t) => println!("auto-export on → {}", t.display()),
                    None => println!("auto-export off"),
                },
            }
        }

        Commands::Serve { port, also, open } => {
            let sources = view_sources(&path, also)?;
            view::serve(sources, port, open)?;
        }

        Commands::Export { out, open, auto } => {
            let sources = view_sources(&path, Vec::new())?;
            view::export(sources, &out)?;
            if auto {
                let conn = open_db(&path)?;
                ensure_schema(&conn)?;
                let target = absolute(&out);
                meta_set(&conn, "autoexport", &target.display().to_string())?;
                println!("auto-export on → {}", target.display());
            }
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
            refresh_views(&conn, &path);
        }
    }

    Ok(())
}
