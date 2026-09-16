mod board;
mod events;
mod import;
mod notes;
mod store;
mod view;

use events::Event;
use notes::Note;
use store::Project;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::env;
use std::path::{Path, PathBuf};

pub(crate) const DEFAULT_DB: &str = "backlog.db";

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
    updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
    project_id  INTEGER NOT NULL DEFAULT 0,
    legacy_id   INTEGER
);

CREATE INDEX IF NOT EXISTS idx_status_priority
    ON cards(status, priority DESC, created_at ASC);
CREATE INDEX IF NOT EXISTS idx_label ON cards(label);
CREATE INDEX IF NOT EXISTS idx_claimed_by ON cards(claimed_by);

-- One row per repository sharing this database. `path` is the main checkout;
-- worktrees resolve to it through git. Inactive projects are skipped by
-- `bl next` and hidden from the default views.
CREATE TABLE IF NOT EXISTS projects (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL UNIQUE,
    path        TEXT NOT NULL UNIQUE,
    active      INTEGER NOT NULL DEFAULT 1,
    autoexport  TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

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

-- The audit trail: one row per change to a card (status, claim, priority,
-- title...). Never updated or deleted, and not cascaded: a deleted card's
-- history stays readable. `before`/`after`/`by` are keywords, hence the names.
CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    card_id     INTEGER NOT NULL,
    project_id  INTEGER NOT NULL DEFAULT 0,
    kind        TEXT NOT NULL,
    old_value   TEXT NOT NULL DEFAULT '',
    new_value   TEXT NOT NULL DEFAULT '',
    actor       TEXT NOT NULL DEFAULT '',
    at          TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_events_card ON events(card_id, id);
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
    /// Use this database instead of the central store (also $BL_DB)
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    /// Scope to one project by name or #id (also $BL_PROJECT). Defaults to
    /// the project whose repository the current directory is in.
    #[arg(short = 'P', long, global = true, value_name = "NAME")]
    project: Option<String>,

    /// Read across every active project instead of the current one
    #[arg(long, global = true)]
    all: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Create the central store (or --db file) and register this repository
    Init,

    /// Register, list, activate, deactivate or remove projects
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },

    /// Copy a repo-level backlog.db into the store, or file many cards from stdin
    Import {
        /// The backlog.db to read (or use --stdin)
        #[arg(required_unless_present = "stdin", conflicts_with = "stdin")]
        source: Option<PathBuf>,
        /// Read cards from stdin: a JSON array or one JSON object per line
        /// ({"title", "label"?, "priority"?, "notes"?, "project"?}); prints the new ids as JSON
        #[arg(long)]
        stdin: bool,
        /// With --stdin: a card whose exact title already exists is reported, not filed again
        #[arg(long, requires = "stdin")]
        if_absent: bool,
        /// With --stdin: who is filing them, for notes and the event log
        #[arg(long, requires = "stdin")]
        by: Option<String>,
        /// Count what would happen without writing
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },

    /// Import every repo-level backlog.db beside a registered project or under --scan dirs
    Migrate {
        /// Directories whose immediate children may hold a backlog.db (e.g. ~/git)
        #[arg(long, value_name = "DIR")]
        scan: Vec<PathBuf>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },

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
        /// Who is creating it, for the event log
        #[arg(long)]
        by: Option<String>,
        /// Accept a title over 120 characters anyway
        #[arg(long)]
        force: bool,
    },

    /// Change any field of one card, or of many with --ids / --where
    Edit {
        /// The card; or use --ids / --where for several
        id: Option<i64>,
        /// Comma-separated card ids to change together
        #[arg(long, value_name = "1,2,3", conflicts_with = "id")]
        ids: Option<String>,
        /// Select cards by field: label=art, status=new, priority<2000, project=NAME, claimed_by=X (repeatable, all must hold)
        #[arg(long = "where", value_name = "FIELD=VALUE", conflicts_with_all = ["id", "ids"])]
        r#where: Vec<String>,
        /// Change a field by name: priority=100, label=visual, status=ready, outcome=..., project=NAME (repeatable)
        #[arg(long = "set", value_name = "FIELD=VALUE")]
        set: Vec<String>,
        /// List the cards that would change and write nothing
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        title: Option<String>,
        /// Replace the tags (comma-separated)
        #[arg(long)]
        label: Option<String>,
        /// Add a tag, keeping the others (repeatable)
        #[arg(long, value_name = "TAG")]
        add_tag: Vec<String>,
        /// Remove a tag, keeping the others (repeatable)
        #[arg(long, value_name = "TAG")]
        rm_tag: Vec<String>,
        #[arg(long)]
        priority: Option<i32>,
        /// Replace every note on the card with this text (one note per line)
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        outcome: Option<String>,
        /// Moves like `bl status`: leaving in_progress clears the claim
        #[arg(long)]
        status: Option<Status>,
        /// Move the card to another project (by name or #id)
        #[arg(long, value_name = "PROJECT")]
        r#move: Option<String>,
        /// Who is editing, for the event log
        #[arg(long)]
        by: Option<String>,
        /// Accept a title over 120 or an outcome over 300 characters anyway
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },

    /// Change a card's title (short for `bl edit <id> --title`)
    Retitle {
        id: i64,
        title: String,
        /// Accept a title over 120 characters anyway
        #[arg(long)]
        force: bool,
    },

    /// Remove a card created in error. Its title and notes stay in `bl history`.
    Delete {
        id: i64,
        /// Why it goes, kept on the deleted event
        #[arg(long)]
        why: Option<String>,
        /// Delete even if an agent holds the claim
        #[arg(long)]
        force: bool,
        /// Who is deleting, for the event log
        #[arg(long)]
        by: Option<String>,
    },

    /// Set priority score (0-10000)
    #[command(name = "set-priority")]
    SetPriority {
        id: i64,
        priority: i32,
        /// Who is re-ranking, for the event log
        #[arg(long)]
        by: Option<String>,
    },

    /// Update status (and optionally outcome). Clears claim when moving to ready/done/new.
    Status {
        id: i64,
        status: Status,
        #[arg(long, default_value = "")]
        outcome: String,
        /// Who is moving it, for the event log
        #[arg(long)]
        by: Option<String>,
        /// Accept an outcome over 300 characters anyway
        #[arg(long)]
        force: bool,
    },

    /// Replay every change a card went through (works for deleted cards too)
    History {
        id: i64,
        #[arg(long)]
        json: bool,
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

    /// Append a note to a card; `bl note edit|rm <note-id>` fixes one by its id
    #[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
    Note {
        #[command(subcommand)]
        action: Option<NoteAction>,
        #[arg(required = true)]
        id: Option<i64>,
        /// The note; or read it with --stdin / -f so quotes and globs never touch the shell
        #[arg(required_unless_present_any = ["stdin", "file"], conflicts_with_all = ["stdin", "file"])]
        text: Option<String>,
        /// Read the note body from standard input
        #[arg(long)]
        stdin: bool,
        /// Read the note body from a file
        #[arg(short = 'f', long = "file", value_name = "PATH", conflicts_with = "stdin")]
        file: Option<PathBuf>,
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

impl Commands {
    /// Commands about the store itself, which must never be redirected to a
    /// repo-level file by the unregistered-repository fallback.
    fn wants_store(&self) -> bool {
        matches!(
            self,
            Commands::Project { .. } | Commands::Import { stdin: false, .. } | Commands::Migrate { .. }
        )
    }
}

#[derive(Subcommand, Debug)]
enum ProjectAction {
    /// Register a repository (default: the one the current directory is in)
    Add {
        path: Option<PathBuf>,
        /// Project name (default: the directory name)
        #[arg(long)]
        name: Option<String>,
    },
    /// Every registered project with its open card count
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show the project the current directory resolves to
    Current {
        #[arg(long)]
        json: bool,
    },
    /// Let `bl next` and the default views pick this project up again
    Activate { name: String },
    /// Park a project: skipped by `bl next`, hidden from the default views
    Deactivate { name: String },
    /// Drop a project row. Refuses while it still has cards unless --force.
    Remove {
        name: String,
        /// Delete the project and every card and note under it
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum NoteAction {
    /// Rewrite one note's text and/or kind (`bl notes <card>` prints the ids)
    Edit {
        note_id: i64,
        text: Option<String>,
        #[arg(short, long)]
        kind: Option<String>,
        /// Who is editing, for the event log
        #[arg(long)]
        by: Option<String>,
    },
    /// Remove one note by its id
    Rm {
        note_id: i64,
        /// Who is removing it, for the event log
        #[arg(long)]
        by: Option<String>,
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

/// Everything a command needs to know about where it is working.
struct Ctx {
    conn: Connection,
    path: PathBuf,
    /// The shared store under `~/.bl`, as opposed to a `--db` file.
    central: bool,
    /// The project this command is scoped to, when one could be resolved.
    project: Option<Project>,
    /// `--all`: read across active projects even inside a repository.
    all: bool,
}

impl Ctx {
    /// The project a new card belongs to. Central mode outside any registered
    /// repository has nowhere to put one, and says so.
    fn require_project(&self) -> Result<&Project> {
        match &self.project {
            Some(p) => Ok(p),
            None if self.central => bail!(
                "this directory is not inside a registered project: pass --project <name>, \
                 or run `bl project add` from the repository"
            ),
            None => bail!("this database has no project row; run `bl init --db {}`", self.path.display()),
        }
    }

    /// The SQL that limits a read to the right cards: the current project, or
    /// every active project when none is scoped (or `--all` was given).
    /// Returns the clause (starting with ` AND `) and the id to bind, if any.
    fn scope(&self) -> (String, Option<i64>) {
        match (&self.project, self.all) {
            (Some(p), false) => (" AND cards.project_id = ?".to_string(), Some(p.id)),
            _ if self.central => (
                " AND cards.project_id IN (SELECT id FROM projects WHERE active = 1)".to_string(),
                None,
            ),
            _ => (String::new(), None),
        }
    }

    /// Where git runs for `--commit`: the project's checkout, not wherever the
    /// database file happens to sit.
    fn git_dir(&self) -> PathBuf {
        match &self.project {
            Some(p) if !p.path.is_empty() => PathBuf::from(&p.path),
            _ => absolute(&self.path)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from(".")),
        }
    }
}

/// Open the database a command should use and work out its project. Never
/// creates a file: an empty board minted in the wrong directory is how every
/// wrapper script and hook around this tool came to exist. `bl init` is the
/// only command that creates.
fn open_ctx(cli: &Cli) -> Result<Ctx> {
    let (path, central) = match store::target(cli.db.as_deref())? {
        store::Target::Explicit(p) => (p, false),
        store::Target::Central(p) => (p, true),
        store::Target::RepoLocal(p) => (p, false),
    };
    if !path.exists() {
        bail!(
            "database not found: {}\nRun `bl init --db {}` to create it.",
            path.display(),
            path.display()
        );
    }
    let conn = open_db(&path)?;
    ensure_schema(&conn, &path)?;
    let project = store::resolve_project(&conn, central, cli.project.as_deref())?;

    // A repository that still carries its own backlog.db and is not registered
    // in the store keeps working against that file, so nothing breaks between
    // creating the store and importing each project into it.
    if central && project.is_none() && cli.project.is_none() && !cli.command.wants_store() {
        let local = PathBuf::from(DEFAULT_DB);
        if local.exists() {
            eprintln!(
                "bl: using ./backlog.db (this repository is not registered in {}; \
                 `bl import` moves it there)",
                path.display()
            );
            drop(conn);
            let conn = open_db(&local)?;
            ensure_schema(&conn, &local)?;
            let project = store::resolve_project(&conn, false, None)?;
            return Ok(Ctx {
                conn,
                path: local,
                central: false,
                project,
                all: cli.all,
            });
        }
    }
    Ok(Ctx {
        conn,
        path,
        central,
        project,
        all: cli.all,
    })
}

fn open_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("failed to open database at {}", path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    Ok(conn)
}

/// The one place a database file comes into being.
fn create_db(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
    }
    let conn = Connection::open(path)
        .with_context(|| format!("failed to create database at {}", path.display()))?;
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

fn ensure_schema(conn: &Connection, path: &Path) -> Result<()> {
    conn.execute_batch(SCHEMA)?;

    // Cards learned which project they belong to when the central store
    // arrived. The index is created here, after the column is certain to
    // exist, rather than in the base schema.
    if !column_exists(conn, "cards", "project_id")? {
        conn.execute_batch("ALTER TABLE cards ADD COLUMN project_id INTEGER NOT NULL DEFAULT 0;")?;
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_project ON cards(project_id, status);")?;

    // The id a card had in the repo-level database it was imported from.
    if !column_exists(conn, "cards", "legacy_id")? {
        conn.execute_batch("ALTER TABLE cards ADD COLUMN legacy_id INTEGER;")?;
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_legacy ON cards(project_id, legacy_id);")?;

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
                updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
                project_id  INTEGER NOT NULL DEFAULT 0,
                legacy_id   INTEGER
            );
            INSERT INTO cards_new (id, title, notes, label, status, priority, outcome, claimed_by, claimed_at, commits, created_at, updated_at, project_id, legacy_id)
            SELECT id, title, notes, label, status, priority, outcome,
                   COALESCE(claimed_by, ''), COALESCE(claimed_at, ''),
                   COALESCE(commits, ''),
                   created_at, updated_at, COALESCE(project_id, 0), legacy_id
            FROM cards;
            DROP TABLE cards;
            ALTER TABLE cards_new RENAME TO cards;
            CREATE INDEX IF NOT EXISTS idx_status_priority ON cards(status, priority DESC, created_at ASC);
            CREATE INDEX IF NOT EXISTS idx_label ON cards(label);
            CREATE INDEX IF NOT EXISTS idx_claimed_by ON cards(claimed_by);
            CREATE INDEX IF NOT EXISTS idx_project ON cards(project_id, status);
            COMMIT;
            "#,
        )?;
    }

    // Labels became tag lists: `art enemies c676 build` is four tags, stored
    // as `art,enemies,c676,build`. Done once per database, marked in meta.
    if meta_get(conn, "labels_tagged")?.is_none() {
        let legacy: Vec<(i64, String)> = {
            let mut stmt = conn.prepare("SELECT id, label FROM cards WHERE label != ''")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        conn.execute_batch("BEGIN;")?;
        for (id, label) in legacy {
            let tagged = normalize_tags(&label);
            if tagged != label {
                conn.execute("UPDATE cards SET label = ?1 WHERE id = ?2", params![tagged, id])?;
            }
        }
        meta_set(conn, "labels_tagged", "1")?;
        conn.execute_batch("COMMIT;")?;
    }

    // A database from before projects existed holds one repository's cards.
    // Give it a project row named after the directory the file sits in, so
    // `bl import` and the views have something to attach those cards to.
    let unassigned: i64 = conn.query_row(
        "SELECT COUNT(*) FROM cards WHERE project_id = 0",
        [],
        |r| r.get(0),
    )?;
    if unassigned > 0 {
        let existing = store::all(conn)?;
        let target = match existing.first() {
            Some(p) if existing.len() == 1 => p.clone(),
            Some(_) => bail!(
                "{} unassigned card(s) in a database with several projects; \
                 `bl edit --where project_id=0 --project <name>` is not available yet, \
                 so assign them by hand before continuing",
                unassigned
            ),
            None => {
                let dir = absolute(path)
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."));
                store::add(conn, &dir, None, &now_str())?
            }
        };
        conn.execute(
            "UPDATE cards SET project_id = ?1 WHERE project_id = 0",
            params![target.id],
        )?;
    }

    Ok(())
}

/// The project a card belongs to, for refreshing the right snapshot.
fn card_project(conn: &Connection, id: i64) -> Option<i64> {
    conn.query_row(
        "SELECT project_id FROM cards WHERE id = ?",
        params![id],
        |r| r.get(0),
    )
    .ok()
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
    pub(crate) project_id: i64,
    /// The project's name, resolved at read time so a card always says
    /// which repository it is about.
    pub(crate) project: String,
    /// The id this card had in the repo-level database it was imported from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) legacy_id: Option<i64>,
    /// The notes as rows. Empty unless the caller asked for them, and always
    /// empty for a database old enough to lack the table.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) entries: Vec<Note>,
    /// The audit trail, oldest first. Filled with `entries`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) events: Vec<Event>,
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
        project_id: row.get::<_, i64>(12).unwrap_or(0),
        project: row.get::<_, String>(13).unwrap_or_default(),
        legacy_id: row.get::<_, Option<i64>>(14).unwrap_or(None),
        entries: Vec::new(),
        events: Vec::new(),
    })
}

/// Fill in each card's notes. Skipped silently where the table is absent, so a
/// legacy database still lists and serves.
pub(crate) fn load_entries(conn: &Connection, cards: &mut [Card]) {
    let has_notes = notes::table_exists(conn);
    let has_events = events::table_exists(conn);
    for c in cards.iter_mut() {
        if has_notes {
            c.entries = notes::list(conn, c.id).unwrap_or_default();
        }
        if has_events {
            c.events = events::list(conn, c.id).unwrap_or_default();
        }
    }
}

/// The plain columns of a card, in `row_to_card` order.
pub(crate) const CARD_COLS: &str =
    "id, title, notes, label, status, priority, outcome, claimed_by, claimed_at, commits, created_at, updated_at, project_id";

/// Columns that come after the project name in `row_to_card` order.
pub(crate) const TRAILING_COLS: &str = "legacy_id";

/// The project's name, looked up per row. No comma-space inside, so
/// `read_cards` can still split the list on `", "`.
pub(crate) const PROJECT_COL: &str =
    "IFNULL((SELECT name FROM projects WHERE projects.id=cards.project_id),'') AS project";

pub(crate) const SELECT_COLS: &str =
    "id, title, notes, label, status, priority, outcome, claimed_by, claimed_at, commits, created_at, updated_at, project_id, IFNULL((SELECT name FROM projects WHERE projects.id=cards.project_id),'') AS project, legacy_id";

/// Print a card. `show_project` puts the project name on the first line, for
/// listings that span more than one.
fn print_card(c: &Card, json: bool, show_project: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(c).unwrap());
    } else {
        let claim = if c.claimed_by.is_empty() {
            String::new()
        } else {
            format!("  claimed_by={}", c.claimed_by)
        };
        println!(
            "#{}  [{:>5}]  {:12}  {}{}{}{}",
            c.id,
            c.priority,
            c.status,
            if show_project && !c.project.is_empty() {
                format!("{}: ", c.project)
            } else {
                String::new()
            },
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
        if let Some(old) = c.legacy_id {
            println!("    imported: was #{} in {}'s own backlog.db", old, c.project);
        }
        for line in c.commits.lines().filter(|l| !l.trim().is_empty()) {
            let (sha, subject) = line.split_once('\t').unwrap_or((line, ""));
            println!("    commit: {} {}", sha, subject);
        }
        println!("    created: {}   updated: {}", c.created_at, c.updated_at);
    }
}

/// Databases the board may read: the primary one plus any `--also` paths.
/// The primary is scoped like every other read; an `--also` file is shown
/// whole, since nothing is known about its projects.
fn view_sources(ctx: &Ctx, also: Vec<PathBuf>) -> Result<Vec<view::Source>> {
    let (project, label) = match (&ctx.project, ctx.all) {
        (Some(p), false) => (Some(p.id), p.name.clone()),
        _ => (None, source_label(&ctx.path)),
    };
    let mut sources = vec![view::Source {
        label,
        path: ctx.path.clone(),
        project,
        active_only: ctx.central,
    }];
    for p in also {
        sources.push(view::Source {
            label: source_label(&p),
            path: p,
            project: None,
            active_only: false,
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

/// Where — if anywhere — the HTML snapshot for the current scope is kept in
/// sync: the project's own path inside a repository, else the store-wide one.
/// `BL_AUTOEXPORT` overrides the stored path; `BL_NO_AUTOEXPORT` turns it off.
fn autoexport_target(ctx: &Ctx) -> Option<PathBuf> {
    if env::var_os("BL_NO_AUTOEXPORT").is_some() {
        return None;
    }
    if let Ok(p) = env::var("BL_AUTOEXPORT") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Some(p) = &ctx.project {
        if !p.autoexport.is_empty() {
            return Some(PathBuf::from(&p.autoexport));
        }
    }
    meta_get(&ctx.conn, "autoexport")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Every snapshot a write may have made stale: the touched card's project (or
/// all of them when `card` is None) plus the store-wide page. Each pair is
/// (output path, project filter).
fn stale_snapshots(ctx: &Ctx, card: Option<i64>) -> Vec<(PathBuf, Option<i64>)> {
    if env::var_os("BL_NO_AUTOEXPORT").is_some() {
        return Vec::new();
    }
    if let Ok(p) = env::var("BL_AUTOEXPORT") {
        if !p.is_empty() {
            let scope = ctx.project.as_ref().filter(|_| !ctx.all).map(|p| p.id);
            return vec![(PathBuf::from(p), scope)];
        }
    }
    let mut out = Vec::new();
    let touched = card.and_then(|id| card_project(&ctx.conn, id));
    for p in store::all(&ctx.conn).unwrap_or_default() {
        if p.autoexport.is_empty() {
            continue;
        }
        if touched.map(|t| t == p.id).unwrap_or(true) {
            out.push((PathBuf::from(&p.autoexport), Some(p.id)));
        }
    }
    if let Some(global) = meta_get(&ctx.conn, "autoexport").ok().flatten().filter(|s| !s.is_empty()) {
        out.push((PathBuf::from(global), None));
    }
    out
}

/// Rewrite the snapshots after a write, so the view pages never serve stale
/// cards. `card` is the card just touched, so only its project's page is
/// redrawn; None redraws every project's. Best-effort: a failed refresh must
/// not fail the command that already committed.
fn refresh_views(ctx: &Ctx, card: Option<i64>) {
    for (out, project) in stale_snapshots(ctx, card) {
        if let Err(e) = view::refresh(&ctx.path, project, ctx.central, &out) {
            eprintln!("bl: auto-export to {} failed: {}", out.display(), e);
        }
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
fn agent_prompt(ctx: &Ctx) -> Result<String> {
    let conn = &ctx.conn;
    let db = &ctx.path;
    let labels: Vec<String> = {
        let (scope, bind) = ctx.scope();
        let mut stmt = conn.prepare(&format!(
            "SELECT label FROM cards WHERE label != '' AND status != 'done'{}",
            scope
        ))?;
        let binds: Vec<Box<dyn rusqlite::ToSql>> = match bind {
            Some(id) => vec![Box::new(id)],
            None => Vec::new(),
        };
        let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| r.get::<_, String>(0))?;
        // Counted per tag, since a card may carry several.
        let mut counts: std::collections::BTreeMap<String, i64> = Default::default();
        for label in rows.filter_map(|r| r.ok()) {
            for t in tags_of(&label) {
                *counts.entry(t).or_default() += 1;
            }
        }
        let mut pairs: Vec<(String, i64)> = counts.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        pairs.into_iter().map(|(t, n)| format!("{} ({})", t, n)).collect()
    };
    let labels = if labels.is_empty() {
        String::new()
    } else {
        format!(
            "\n## Tags in use\n\n{}\n\nReuse one of these rather than inventing a near-duplicate; a card may carry several (`-l art,enemies`).\n",
            labels.join(", ")
        )
    };

    let auto = match autoexport_target(ctx) {
        Some(t) => format!(
            "\nThe board at `{}` refreshes itself on every write — never run `bl export`.\n",
            t.display()
        ),
        None => String::new(),
    };

    let project = match &ctx.project {
        Some(p) => format!(
            "This repository is project **{}** (#{}, `{}`). Commands run from inside it,\n\
             or any of its worktrees, are scoped to it automatically; nothing has to be pinned.",
            p.name, p.id, p.path
        ),
        None if ctx.central => "No project is scoped: this directory is not a registered repository. \
             Pass `--project <name>` or run `bl project add`."
            .to_string(),
        None => String::new(),
    };

    Ok(AGENT_PROMPT
        .replace("{{DB}}", &absolute(db).display().to_string())
        .replace("{{PROJECT}}", &project)
        .replace("{{AUTO}}", &auto)
        .replace("{{LABELS}}", &labels))
}

// ---------------------------------------------------------------- git

/// Resolve a revision (default `HEAD`) to `(short sha, subject)` in the
/// project's repository.
fn resolve_commit(ctx: &Ctx, rev: &str) -> Result<(String, String)> {
    let rev = if rev.trim().is_empty() { "HEAD" } else { rev.trim() };
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(ctx.git_dir())
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

// ---------------------------------------------------------------- edit

#[derive(Default)]
struct EditFields {
    title: Option<String>,
    label: Option<String>,
    priority: Option<i32>,
    notes: Option<String>,
    outcome: Option<String>,
    status: Option<Status>,
    move_to: Option<String>,
    by: String,
    force: bool,
    add_tags: Vec<String>,
    rm_tags: Vec<String>,
}

/// Apply every given field in one transaction. Returns the names of the
/// fields that changed, for the confirmation line.
/// The fields of a card an edit or delete may need to compare against.
struct Snapshot {
    title: String,
    label: String,
    priority: i32,
    outcome: String,
    status: String,
    claimed_by: String,
    project_id: i64,
    notes: String,
}

fn snapshot(conn: &Connection, id: i64) -> Result<Option<Snapshot>> {
    Ok(conn
        .query_row(
            "SELECT title, label, priority, outcome, status, claimed_by, project_id, notes
             FROM cards WHERE id = ?",
            params![id],
            |r| {
                Ok(Snapshot {
                    title: r.get(0)?,
                    label: r.get(1)?,
                    priority: r.get(2)?,
                    outcome: r.get(3)?,
                    status: r.get(4)?,
                    claimed_by: r.get(5)?,
                    project_id: r.get(6)?,
                    notes: r.get(7)?,
                })
            },
        )
        .optional()?)
}

/// Run `f` inside one IMMEDIATE transaction, rolling back on error.
fn transaction<T>(conn: &Connection, f: impl FnOnce() -> Result<T>) -> Result<T> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    match f() {
        Ok(v) => {
            conn.execute_batch("COMMIT;")?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

fn edit_card(ctx: &Ctx, id: i64, f: EditFields) -> Result<Vec<String>> {
    let conn = &ctx.conn;
    let now = now_str();
    let (changed, moved) = transaction(conn, || apply_edit(conn, id, &f, &now))?;
    // A move leaves the old project's page stale too, so redraw every page.
    refresh_views(ctx, if moved { None } else { Some(id) });
    Ok(changed)
}

/// `--set field=value` in the vocabulary of `EditFields`.
fn parse_set(f: &mut EditFields, pair: &str) -> Result<()> {
    let (k, v) = pair
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("--set wants FIELD=VALUE, got '{}'", pair))?;
    let v = v.to_string();
    match k.trim() {
        "title" => f.title = Some(v),
        "label" => f.label = Some(v),
        "priority" => f.priority = Some(v.trim().parse().with_context(|| format!("priority '{}' is not a number", v))?),
        "outcome" => f.outcome = Some(v),
        "notes" => f.notes = Some(v),
        "status" => {
            f.status = Some(
                <Status as ValueEnum>::from_str(v.trim(), true)
                    .map_err(|_| anyhow::anyhow!("status '{}' is not one of new, ready, in_progress, done", v))?,
            )
        }
        "project" | "move" => f.move_to = Some(v),
        other => bail!("--set does not know the field '{}' (title, label, priority, outcome, notes, status, project)", other),
    }
    Ok(())
}

/// `--where field=value` (and `priority<N`, `priority>N`, `<=`, `>=`) as an
/// SQL clause starting with ` AND `, plus the value to bind.
fn parse_where(conn: &Connection, expr: &str) -> Result<(String, Box<dyn rusqlite::ToSql>)> {
    let ops = ["<=", ">=", "!=", "=", "<", ">"];
    let (k, op, v) = ops
        .iter()
        .find_map(|op| expr.split_once(op).map(|(k, v)| (k.trim(), *op, v.trim())))
        .ok_or_else(|| anyhow::anyhow!("--where wants FIELD=VALUE, got '{}'", expr))?;
    let text_ops = matches!(op, "=" | "!=");
    match k {
        "priority" | "id" => {
            let n: i64 = v.parse().with_context(|| format!("{} '{}' is not a number", k, v))?;
            Ok((format!(" AND {} {} ?", k, op), Box::new(n)))
        }
        "label" | "tag" if text_ops => Ok((
            format!(
                " AND (',' || label || ',') {} ?",
                if op == "=" { "LIKE" } else { "NOT LIKE" }
            ),
            Box::new(format!("%,{},%", v)),
        )),
        "status" | "claimed_by" | "title" | "outcome" if text_ops => {
            Ok((format!(" AND {} {} ?", k, op), Box::new(v.to_string())))
        }
        "project" if text_ops => {
            let p = store::lookup(conn, v)?.ok_or_else(|| anyhow::anyhow!("no project named '{}'", v))?;
            Ok((format!(" AND project_id {} ?", op), Box::new(p.id)))
        }
        _ => bail!(
            "--where does not understand '{}' (label, status, claimed_by, title, outcome, project with = or !=; priority, id with = != < > <= >=)",
            expr
        ),
    }
}

/// The cards `--where` selects, inside the current scope, lowest id first.
fn select_where(ctx: &Ctx, wheres: &[String]) -> Result<Vec<(i64, String)>> {
    let conn = &ctx.conn;
    let mut sql = String::from("SELECT id, title FROM cards WHERE 1=1");
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let (scope, pid) = ctx.scope();
    sql.push_str(&scope);
    if let Some(pid) = pid {
        binds.push(Box::new(pid));
    }
    for w in wheres {
        let (clause, bind) = parse_where(conn, w)?;
        sql.push_str(&clause);
        binds.push(bind);
    }
    sql.push_str(" ORDER BY id ASC");
    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Apply the same change to several cards in one transaction; every card
/// gets its own event rows. Returns the fields changed on the first card.
fn bulk_edit(ctx: &Ctx, ids: &[i64], f: &EditFields) -> Result<Vec<String>> {
    let conn = &ctx.conn;
    let now = now_str();
    let mut changed = Vec::new();
    transaction(conn, || {
        for id in ids {
            let (c, _) = apply_edit(conn, *id, f, &now)?;
            if changed.is_empty() {
                changed = c;
            }
        }
        Ok(())
    })?;
    refresh_views(ctx, None);
    Ok(changed)
}

/// Every field of one edit, inside the caller's transaction. Returns the
/// names of the fields that changed and whether the card moved project.
fn apply_edit(conn: &Connection, id: i64, f: &EditFields, now: &str) -> Result<(Vec<String>, bool)> {
    let old = snapshot(conn, id)?.ok_or_else(|| anyhow::anyhow!("card #{} not found", id))?;
    let mut sets: Vec<String> = Vec::new();
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut changed: Vec<String> = Vec::new();

    // Tags: --label replaces, --add-tag / --rm-tag adjust what is there.
    let new_label: Option<String> = if f.label.is_some() || !f.add_tags.is_empty() || !f.rm_tags.is_empty() {
        let mut tags = tags_of(f.label.as_deref().unwrap_or(&old.label));
        for t in f.add_tags.iter().flat_map(|t| tags_of(t)) {
            if !tags.contains(&t) {
                tags.push(t);
            }
        }
        let drop: Vec<String> = f.rm_tags.iter().flat_map(|t| tags_of(t)).collect();
        tags.retain(|t| !drop.contains(t));
        Some(tags.join(","))
    } else {
        None
    };

    if let Some(t) = &f.title {
        check_title(t, f.force)?;
        sets.push("title = ?".into());
        binds.push(Box::new(t.trim().to_string()));
        changed.push("title".into());
    }
    if let Some(l) = &new_label {
        sets.push("label = ?".into());
        binds.push(Box::new(l.clone()));
        changed.push("label".into());
    }
    if let Some(p) = f.priority {
        if !(0..=10000).contains(&p) {
            bail!("priority must be 0..=10000");
        }
        sets.push("priority = ?".into());
        binds.push(Box::new(p));
        changed.push("priority".into());
    }
    if let Some(o) = &f.outcome {
        check_outcome(o, f.force)?;
        sets.push("outcome = ?".into());
        binds.push(Box::new(o.clone()));
        changed.push("outcome".into());
    }
    if let Some(st) = &f.status {
        sets.push("status = ?".into());
        binds.push(Box::new(st.as_str().to_string()));
        // The same rule as `bl status`: leaving in_progress drops the claim.
        if matches!(st, Status::New | Status::Ready | Status::Done) {
            sets.push("claimed_by = ''".into());
            sets.push("claimed_at = ''".into());
        }
        changed.push("status".into());
    }
    let mut moved: Option<Project> = None;
    if let Some(key) = &f.move_to {
        let p = store::lookup(conn, key)?
            .ok_or_else(|| anyhow::anyhow!("no project named '{}'", key))?;
        sets.push("project_id = ?".into());
        binds.push(Box::new(p.id));
        changed.push(format!("project → {}", p.name));
        moved = Some(p);
    }
    if f.notes.is_some() {
        changed.push("notes".into());
    }
    if changed.is_empty() {
        bail!("nothing to change: give at least one of --title --label --add-tag --rm-tag --priority --notes --outcome --status --move");
    }

    {
        if !sets.is_empty() {
            sets.push("updated_at = ?".into());
            binds.push(Box::new(now.to_string()));
            binds.push(Box::new(id));
            let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
            conn.execute(
                &format!("UPDATE cards SET {} WHERE id = ?", sets.join(", ")),
                refs.as_slice(),
            )?;
        }
        if let Some(text) = &f.notes {
            // Replace, not append: `bl note` appends. The rows go with the blob
            // so the two views of the notes stay one thing.
            let old_n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM notes WHERE card_id = ?",
                params![id],
                |r| r.get(0),
            )?;
            conn.execute("DELETE FROM notes WHERE card_id = ?", params![id])?;
            conn.execute(
                "UPDATE cards SET notes = ?1, updated_at = ?2 WHERE id = ?3",
                params![text, now, id],
            )?;
            notes::reconcile(conn, id)?;
            if old_n > 0 {
                eprintln!("bl: #{} had {} note(s); they are replaced, not kept", id, old_n);
            }
        }
        // One event per field that actually changed, so the history reads
        // like a diff rather than a list of commands.
        let ev = |kind: &str, before: &str, after: &str| -> Result<()> {
            if before != after {
                events::record(conn, id, Some(old.project_id), kind, before, after, &f.by, now)?;
            }
            Ok(())
        };
        if let Some(t) = &f.title {
            ev("title", &old.title, t.trim())?;
        }
        if let Some(l) = &new_label {
            ev("label", &old.label, l)?;
        }
        if let Some(p) = f.priority {
            ev("priority", &old.priority.to_string(), &p.to_string())?;
        }
        if let Some(o) = &f.outcome {
            ev("outcome", &old.outcome, o)?;
        }
        if let Some(st) = &f.status {
            ev("status", &old.status, st.as_str())?;
            if !old.claimed_by.is_empty() && matches!(st, Status::New | Status::Ready | Status::Done) {
                ev("release", &old.claimed_by, "")?;
            }
        }
        if let Some(p) = &moved {
            let from = store::by_id(conn, old.project_id)?
                .map(|p| p.name)
                .unwrap_or_else(|| old.project_id.to_string());
            ev("move", &from, &p.name)?;
        }
        if let Some(text) = &f.notes {
            ev("notes", &old.notes, text)?;
        }
    }
    Ok((changed, moved.is_some()))
}

// ---------------------------------------------------------------- tags

/// A card's label is a comma-separated list of tags. Input may use commas or
/// spaces; the stored form is `a,b,c` with no blanks and no repeats.
pub(crate) fn tags_of(label: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in label.split(|c: char| c == ',' || c.is_whitespace()) {
        let t = t.trim();
        if !t.is_empty() && !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    out
}

pub(crate) fn normalize_tags(label: &str) -> String {
    tags_of(label).join(",")
}

/// `-l art` or `-l art,ui`: a card matches when it carries any of the tags.
/// Returns the clause (starting with ` AND `) and its binds.
pub(crate) fn tag_clause(filter: &str) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let tags = tags_of(filter);
    if tags.is_empty() {
        return (" AND label = ''".to_string(), Vec::new());
    }
    let ors: Vec<&str> = tags.iter().map(|_| "(',' || label || ',') LIKE ?").collect();
    let binds: Vec<Box<dyn rusqlite::ToSql>> = tags
        .iter()
        .map(|t| Box::new(format!("%,{},%", t)) as Box<dyn rusqlite::ToSql>)
        .collect();
    (format!(" AND ({})", ors.join(" OR ")), binds)
}

// ---------------------------------------------------------------- guards

/// Titles and outcomes are headlines; the detail belongs in notes. Boards
/// where agents ignored that ended up with 500-character titles.
pub(crate) const TITLE_MAX: usize = 120;
pub(crate) const OUTCOME_MAX: usize = 300;

pub(crate) fn check_title(title: &str, force: bool) -> Result<()> {
    if title.trim().is_empty() {
        bail!("a title cannot be empty");
    }
    let n = title.trim().chars().count();
    if n > TITLE_MAX && !force {
        bail!(
            "title is {} characters (limit {}): keep the title to one line and put the detail in a note \
             (`bl note <id> \"...\"`), or pass --force",
            n, TITLE_MAX
        );
    }
    Ok(())
}

pub(crate) fn check_outcome(outcome: &str, force: bool) -> Result<()> {
    let n = outcome.chars().count();
    if n > OUTCOME_MAX && !force {
        bail!(
            "outcome is {} characters (limit {}): say the result in a line and put the detail in a note \
             (`bl note <id> \"...\" -k finding`), or pass --force",
            n, OUTCOME_MAX
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- delete

/// Remove one card and its notes inside the caller's transaction, leaving a
/// `deleted` event that carries everything the card said so `bl history`
/// can still show it. Refuses a claimed card unless `force`.
fn delete_card(conn: &Connection, id: i64, why: &str, force: bool, by: &str, now: &str) -> Result<Snapshot> {
    let old = snapshot(conn, id)?.ok_or_else(|| anyhow::anyhow!("card #{} not found", id))?;
    if !old.claimed_by.is_empty() && !force {
        bail!(
            "card #{} is claimed by '{}'; `bl release {}` first, or --force",
            id, old.claimed_by, id
        );
    }
    notes::reconcile(conn, id)?;
    let mut payload = format!(
        "title: {}\nstatus: {}\npriority: {}",
        old.title, old.status, old.priority
    );
    if !old.label.is_empty() {
        payload.push_str(&format!("\nlabel: {}", old.label));
    }
    if !old.outcome.is_empty() {
        payload.push_str(&format!("\noutcome: {}", old.outcome));
    }
    for n in notes::list(conn, id)? {
        payload.push_str(&format!(
            "\nnote {} [{}]{}: {}",
            n.created_at,
            n.kind,
            if n.author.is_empty() { String::new() } else { format!(" {}", n.author) },
            n.body
        ));
    }
    events::record(conn, id, Some(old.project_id), "deleted", &payload, why, by, now)?;
    conn.execute("DELETE FROM notes WHERE card_id = ?", params![id])?;
    conn.execute("DELETE FROM cards WHERE id = ?", params![id])?;
    Ok(old)
}

// ---------------------------------------------------------------- init

/// The only command that creates a database. Without `--db` it creates the
/// central store and registers the repository the shell is in; with `--db`
/// it creates (or migrates) that one file and gives it its project row.
fn init(cli: &Cli) -> Result<()> {
    let now = now_str();
    let explicit = cli.db.clone().or_else(|| {
        env::var("BL_DB")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    });

    match explicit {
        Some(path) => {
            let existed = path.exists();
            let conn = if existed { open_db(&path)? } else { create_db(&path)? };
            ensure_schema(&conn, &path)?;
            if store::all(&conn)?.is_empty() {
                let dir = absolute(&path)
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."));
                let p = store::add(&conn, &dir, cli.project.as_deref(), &now)?;
                println!("registered project '{}' (#{}) at {}", p.name, p.id, p.path);
            }
            println!(
                "{} {}",
                if existed { "migrated" } else { "initialized" },
                path.display()
            );
        }
        None => {
            let path = store::central_db_path();
            let existed = path.exists();
            let conn = if existed { open_db(&path)? } else { create_db(&path)? };
            ensure_schema(&conn, &path)?;
            store::write_default_config(&path)?;
            println!(
                "{} central store {}",
                if existed { "using" } else { "initialized" },
                path.display()
            );
            match store::git_main_root(None) {
                Some(root) => match store::by_path(&conn, &root)? {
                    Some(p) => println!("project '{}' (#{}) already registered at {}", p.name, p.id, p.path),
                    None => {
                        let p = store::add(&conn, &root, cli.project.as_deref(), &now)?;
                        println!("registered project '{}' (#{}) at {}", p.name, p.id, p.path);
                        if root.join(DEFAULT_DB).exists() {
                            println!(
                                "note: {} has its own backlog.db; until it is imported, commands run \
                                 there keep using it",
                                p.path
                            );
                        }
                    }
                },
                None => println!(
                    "not inside a git repository; `bl project add <path>` registers one"
                ),
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- projects

#[derive(serde::Serialize)]
struct ProjectRow {
    #[serde(flatten)]
    project: Project,
    open: i64,
    total: i64,
}

fn project_cmd(ctx: &Ctx, action: ProjectAction) -> Result<()> {
    let conn = &ctx.conn;
    match action {
        ProjectAction::Add { path, name } => {
            let dir = match path {
                Some(p) => store::canon(&p),
                None => store::git_main_root(None)
                    .or_else(|| env::current_dir().ok())
                    .context("cannot tell which directory to register")?,
            };
            let p = store::add(conn, &dir, name.as_deref(), &now_str())?;
            println!("registered project '{}' (#{}) at {}", p.name, p.id, p.path);
            if dir.join(DEFAULT_DB).exists() {
                println!(
                    "note: {} has its own backlog.db; import it into this store and delete the copy",
                    p.path
                );
            }
        }

        ProjectAction::List { json } => {
            let mut rows = Vec::new();
            for p in store::all(conn)? {
                let open = store::open_card_count(conn, p.id)?;
                let total = store::card_count(conn, p.id)?;
                rows.push(ProjectRow { project: p, open, total });
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if rows.is_empty() {
                println!("(no projects; `bl project add` registers one)");
            } else {
                for r in &rows {
                    let here = ctx
                        .project
                        .as_ref()
                        .map(|p| p.id == r.project.id)
                        .unwrap_or(false);
                    println!(
                        "#{:<3} {} {:<24} {:>4} open / {:<4} {}{}",
                        r.project.id,
                        if r.project.active { "active  " } else { "inactive" },
                        r.project.name,
                        r.open,
                        r.total,
                        r.project.path,
                        if here { "  (here)" } else { "" }
                    );
                }
            }
        }

        ProjectAction::Current { json } => match &ctx.project {
            Some(p) => {
                if json {
                    println!("{}", serde_json::to_string_pretty(p)?);
                } else {
                    println!(
                        "#{}  {}  {}  {}",
                        p.id,
                        p.name,
                        if p.active { "active" } else { "inactive" },
                        p.path
                    );
                }
            }
            None => {
                if json {
                    println!("null");
                } else {
                    println!("(no project for this directory)");
                }
                exit_with(EXIT_EMPTY);
            }
        },

        ProjectAction::Activate { name } => {
            let p = store::lookup(conn, &name)?
                .ok_or_else(|| anyhow::anyhow!("no project named '{}'", name))?;
            store::set_active(conn, p.id, true)?;
            println!("project '{}' active", p.name);
        }

        ProjectAction::Deactivate { name } => {
            let p = store::lookup(conn, &name)?
                .ok_or_else(|| anyhow::anyhow!("no project named '{}'", name))?;
            store::set_active(conn, p.id, false)?;
            println!("project '{}' inactive: skipped by bl next, hidden from the default views", p.name);
        }

        ProjectAction::Remove { name, force } => {
            let p = store::lookup(conn, &name)?
                .ok_or_else(|| anyhow::anyhow!("no project named '{}'", name))?;
            let total = store::card_count(conn, p.id)?;
            if total > 0 && !force {
                bail!(
                    "project '{}' still has {} card(s); deactivate it, or --force to delete them too",
                    p.name,
                    total
                );
            }
            let now = now_str();
            transaction(conn, || {
                let ids: Vec<i64> = {
                    let mut stmt = conn.prepare("SELECT id FROM cards WHERE project_id = ? ORDER BY id")?;
                    let rows = stmt.query_map(params![p.id], |r| r.get(0))?;
                    rows.filter_map(|r| r.ok()).collect()
                };
                for id in ids {
                    delete_card(conn, id, &format!("project '{}' removed", p.name), true, "", &now)?;
                }
                store::remove(conn, p.id)
            })?;
            println!("removed project '{}' ({} card(s) deleted)", p.name, total);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- main

fn main() -> Result<()> {
    // `bl list | head` must end quietly. Rust ignores SIGPIPE, so a closed
    // pipe otherwise turns every println! into a panic with a backtrace.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();

    // Init creates files; nothing else may, so it is handled before a
    // database is opened.
    if matches!(cli.command, Commands::Init) {
        return init(&cli);
    }

    let ctx = open_ctx(&cli)?;
    let conn = &ctx.conn;
    // Listings that span projects say which project each card is from.
    let multi = ctx.project.is_none() || ctx.all;

    match cli.command {
        Commands::Init => unreachable!("handled above"),

        Commands::Project { action } => project_cmd(&ctx, action)?,

        Commands::Import { source: None, by, if_absent, dry_run, .. } => {
            let out = import::from_stdin(&ctx, by.as_deref().unwrap_or(""), if_absent, dry_run)?;
            println!("{}", serde_json::to_string_pretty(&out)?);
            if dry_run {
                eprintln!("bl: dry run, {} card(s) would be created", out.len());
            } else {
                let made = out.iter().filter(|c| c.created).count();
                eprintln!("bl: {} card(s) created, {} already present", made, out.len() - made);
                if made > 0 {
                    refresh_views(&ctx, None);
                }
            }
        }

        Commands::Import { source: Some(source), dry_run, json, .. } => {
            // The global --project names (or creates) the target project.
            let out = import::run(&ctx, &source, cli.project.as_deref(), dry_run)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!("{}", import::summary(&out));
                if !dry_run {
                    for (old, new) in &out.imported {
                        println!("  #{} → #{}", old, new);
                    }
                    if !out.imported.is_empty() {
                        println!(
                            "notes still say #<old>; `bl show <new>` prints the old id, and the map above is the key.\n\
                             Delete or gitignore {} so nothing writes to it again.",
                            out.source
                        );
                    }
                }
            }
            if !dry_run && !out.imported.is_empty() {
                refresh_views(&ctx, None);
            }
        }

        Commands::Migrate { scan, dry_run, json } => {
            let found = import::candidates(&ctx, &scan)?;
            if found.is_empty() {
                println!("nothing to import (no backlog.db beside a registered project or under --scan)");
                exit_with(EXIT_EMPTY);
            }
            let mut outs = Vec::new();
            for db in found {
                match import::run(&ctx, &db, None, dry_run) {
                    Ok(o) => {
                        if !json {
                            println!("{}", import::summary(&o));
                        }
                        outs.push(o);
                    }
                    Err(e) => eprintln!("bl: {}: {}", db.display(), e),
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&outs)?);
            }
            if !dry_run && outs.iter().any(|o| !o.imported.is_empty()) {
                refresh_views(&ctx, None);
            }
        }

        Commands::Create {
            title,
            label,
            priority,
            notes: notes_text,
            if_absent,
            by,
            force,
        } => {
            if !(0..=10000).contains(&priority) {
                bail!("priority must be 0..=10000");
            }
            check_title(&title, force)?;
            let title = title.trim().to_string();
            let label = normalize_tags(&label);
            let project = ctx.require_project()?;
            let now = now_str();

            if if_absent {
                let existing: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM cards WHERE title = ?1 AND project_id = ?2 ORDER BY id ASC LIMIT 1",
                        params![title, project.id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(id) = existing {
                    println!("#{} already exists: {}", id, title);
                    return Ok(());
                }
            }
            conn.execute(
                "INSERT INTO cards (title, notes, label, priority, created_at, updated_at, project_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
                params![title, notes_text, label, priority, now, project.id],
            )?;
            let id = conn.last_insert_rowid();
            // Notes given at creation become a first row like any other.
            if !notes_text.is_empty() {
                notes::reconcile(conn, id)?;
            }
            events::record(conn, id, Some(project.id), "created", "", &title, by.as_deref().unwrap_or(""), &now)?;
            println!(
                "created #{}  priority={}  label={}  project={}",
                id, priority, label, project.name
            );
            refresh_views(&ctx, Some(id));
        }

        Commands::Edit {
            id,
            ids,
            r#where,
            set,
            dry_run,
            title,
            label,
            add_tag,
            rm_tag,
            priority,
            notes: notes_text,
            outcome,
            status,
            r#move,
            by,
            force,
            json,
        } => {
            let mut fields = EditFields {
                title,
                label,
                priority,
                notes: notes_text,
                outcome,
                status,
                move_to: r#move,
                by: by.unwrap_or_default(),
                force,
                add_tags: add_tag,
                rm_tags: rm_tag,
            };
            for pair in &set {
                parse_set(&mut fields, pair)?;
            }
            // One card: the original verb. Several: --ids or --where.
            let targets: Vec<(i64, String)> = if let Some(id) = id {
                vec![(id, String::new())]
            } else if let Some(list) = &ids {
                let mut out = Vec::new();
                for part in list.split(',').map(|x| x.trim()).filter(|x| !x.is_empty()) {
                    let n: i64 = part.trim_start_matches('#').parse()
                        .with_context(|| format!("'{}' is not a card id", part))?;
                    let title: String = conn
                        .query_row("SELECT title FROM cards WHERE id = ?", params![n], |r| r.get(0))
                        .optional()?
                        .ok_or_else(|| anyhow::anyhow!("card #{} not found", n))?;
                    out.push((n, title));
                }
                out
            } else if !r#where.is_empty() {
                select_where(&ctx, &r#where)?
            } else {
                bail!("which card? give an id, --ids 1,2,3 or --where FIELD=VALUE");
            };
            let bulk = id.is_none();
            if bulk && targets.is_empty() {
                println!("(no cards match)");
                exit_with(EXIT_EMPTY);
            }
            if dry_run {
                for (n, t) in &targets {
                    println!("#{}  {}", n, t);
                }
                println!("would edit {} card(s); nothing written", targets.len());
                return Ok(());
            }
            if bulk {
                let only: Vec<i64> = targets.iter().map(|(n, _)| *n).collect();
                let changed = bulk_edit(&ctx, &only, &fields)?;
                if json {
                    let list = only.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
                    let mut cards: Vec<Card> = {
                        let mut stmt = conn.prepare(&format!(
                            "SELECT {} FROM cards WHERE id IN ({}) ORDER BY id", SELECT_COLS, list
                        ))?;
                        let rows = stmt.query_map([], row_to_card)?;
                        rows.filter_map(|r| r.ok()).collect()
                    };
                    load_entries(conn, &mut cards);
                    println!("{}", serde_json::to_string_pretty(&cards)?);
                } else {
                    println!("edited {} card(s): {}", only.len(), changed.join(", "));
                }
            } else {
                let id = targets[0].0;
                let changed = edit_card(&ctx, id, fields)?;
                if json {
                    let mut c: Card = conn.query_row(
                        &format!("SELECT {} FROM cards WHERE id = ?", SELECT_COLS),
                        params![id],
                        row_to_card,
                    )?;
                    load_entries(conn, std::slice::from_mut(&mut c));
                    print_card(&c, true, true);
                } else {
                    println!("#{} edited: {}", id, changed.join(", "));
                }
            }
        }

        Commands::Retitle { id, title, force } => {
            edit_card(
                &ctx,
                id,
                EditFields {
                    title: Some(title.clone()),
                    force,
                    ..Default::default()
                },
            )?;
            println!("#{} retitled: {}", id, title);
        }

        Commands::Delete { id, why, force, by } => {
            let now = now_str();
            let old = transaction(conn, || {
                delete_card(conn, id, why.as_deref().unwrap_or(""), force, by.as_deref().unwrap_or(""), &now)
            })?;
            println!("#{} deleted: {}  (bl history {} keeps its notes)", id, old.title, id);
            refresh_views(&ctx, None);
        }

        Commands::SetPriority { id, priority, by } => {
            if !(0..=10000).contains(&priority) {
                bail!("priority must be 0..=10000");
            }
            let now = now_str();
            transaction(conn, || {
                let old = snapshot(conn, id)?.ok_or_else(|| anyhow::anyhow!("card #{} not found", id))?;
                conn.execute(
                    "UPDATE cards SET priority = ?1, updated_at = ?2 WHERE id = ?3",
                    params![priority, now, id],
                )?;
                if old.priority != priority {
                    events::record(conn, id, Some(old.project_id), "priority", &old.priority.to_string(), &priority.to_string(), by.as_deref().unwrap_or(""), &now)?;
                }
                Ok(())
            })?;
            println!("#{} priority → {}", id, priority);
            refresh_views(&ctx, Some(id));
        }

        Commands::Status { id, status, outcome, by, force } => {
            check_outcome(&outcome, force)?;
            let now = now_str();
            let who = by.unwrap_or_default();
            // Clear claim when leaving in_progress (or explicitly setting ready/new/done)
            let clear_claim = matches!(status, Status::New | Status::Ready | Status::Done);
            transaction(conn, || {
                let old = snapshot(conn, id)?.ok_or_else(|| anyhow::anyhow!("card #{} not found", id))?;
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
                if old.status != status.as_str() {
                    events::record(conn, id, Some(old.project_id), "status", &old.status, status.as_str(), &who, &now)?;
                }
                if clear_claim && !old.claimed_by.is_empty() {
                    events::record(conn, id, Some(old.project_id), "release", &old.claimed_by, "", &who, &now)?;
                }
                if !outcome.is_empty() && old.outcome != outcome {
                    events::record(conn, id, Some(old.project_id), "outcome", &old.outcome, &outcome, &who, &now)?;
                }
                Ok(())
            })?;
            println!("#{} status → {}", id, status);
            refresh_views(&ctx, Some(id));
        }

        Commands::Claim { id, by } => {
            if by.trim().is_empty() {
                bail!("--by must be a non-empty agent identity");
            }
            let now = now_str();
            // Atomic claim: only if currently ready (or new) and unclaimed
            conn.execute_batch("BEGIN IMMEDIATE;")?;
            let old = snapshot(conn, id)?;
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
            if n == 1 {
                let old = old.expect("row just updated");
                events::record(conn, id, Some(old.project_id), "status", &old.status, "in_progress", &by, &now)?;
                events::record(conn, id, Some(old.project_id), "claim", "", &by, &by, &now)?;
                conn.execute_batch("COMMIT;")?;
            } else {
                conn.execute_batch("ROLLBACK;")?;
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
                        // Contention, not a failure: the loop should move on.
                        eprintln!(
                            "bl: card #{} already claimed by '{}'",
                            id,
                            if cb.is_empty() { "?" } else { &cb }
                        );
                        exit_with(EXIT_CONTENDED);
                    }
                    Some((st, _)) => {
                        bail!("card #{} is status '{}' (must be new or ready to claim)", id, st)
                    }
                }
            }
            println!("#{} claimed by {}", id, by);
            refresh_views(&ctx, Some(id));
        }

        Commands::Release { id, by } => {
            let now = now_str();
            conn.execute_batch("BEGIN IMMEDIATE;")?;
            let old = snapshot(conn, id)?;
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
            if n == 1 {
                let old = old.expect("row just updated");
                let who = by.clone().unwrap_or_default();
                events::record(conn, id, Some(old.project_id), "status", &old.status, "ready", &who, &now)?;
                events::record(conn, id, Some(old.project_id), "release", &old.claimed_by, "", &who, &now)?;
                conn.execute_batch("COMMIT;")?;
            } else {
                conn.execute_batch("ROLLBACK;")?;
            }
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
            refresh_views(&ctx, Some(id));
        }

        Commands::List {
            label,
            status,
            limit,
            json,
        } => {
            let mut sql = format!("SELECT {} FROM cards WHERE 1=1", SELECT_COLS);
            let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

            let (scope, pid) = ctx.scope();
            sql.push_str(&scope);
            if let Some(pid) = pid {
                binds.push(Box::new(pid));
            }

            if let Some(l) = &label {
                let (clause, tag_binds) = tag_clause(l);
                sql.push_str(&clause);
                binds.extend(tag_binds);
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
                    print_card(c, false, multi);
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
            let (scope, pid) = ctx.scope();

            if claim {
                let agent = match &by {
                    Some(b) if !b.trim().is_empty() => b.clone(),
                    _ => bail!("--claim requires --by <agent-id>"),
                };
                let now = now_str();

                // Pick highest-priority new/ready unclaimed card, then claim in one transaction
                conn.execute_batch("BEGIN IMMEDIATE;")?;

                let mut sql = String::from("SELECT id, status FROM cards WHERE status IN ('ready'");
                if !ready_only {
                    sql.push_str(", 'new'");
                }
                sql.push_str(") AND (claimed_by = '' OR claimed_by IS NULL)");
                sql.push_str(&scope);

                let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
                if let Some(pid) = pid {
                    binds.push(Box::new(pid));
                }
                if let Some(l) = &label {
                    let (clause, tag_binds) = tag_clause(l);
                    sql.push_str(&clause);
                    binds.extend(tag_binds);
                }
                sql.push_str(" ORDER BY priority DESC, created_at ASC LIMIT 1");

                let picked: Option<(i64, String)> = {
                    let mut stmt = conn.prepare(&sql)?;
                    let params_ref: Vec<&dyn rusqlite::ToSql> =
                        binds.iter().map(|b| b.as_ref()).collect();
                    stmt.query_row(params_ref.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?
                };

                let Some((id, prev_status)) = picked else {
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
                events::record(conn, id, None, "status", &prev_status, "in_progress", &agent, &now)?;
                events::record(conn, id, None, "claim", "", &agent, &agent, &now)?;

                conn.execute_batch("COMMIT;")?;
                refresh_views(&ctx, Some(id));

                let card: Card = conn.query_row(
                    &format!("SELECT {} FROM cards WHERE id = ?", SELECT_COLS),
                    params![id],
                    row_to_card,
                )?;
                print_card(&card, json, multi);
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
                sql.push_str(&scope);

                let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
                if let Some(pid) = pid {
                    binds.push(Box::new(pid));
                }
                if let Some(l) = &label {
                    let (clause, tag_binds) = tag_clause(l);
                    sql.push_str(&clause);
                    binds.extend(tag_binds);
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
                            load_entries(conn, std::slice::from_mut(&mut c));
                        }
                        print_card(&c, json, multi);
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

        Commands::History { id, json } => {
            let evs = events::list(conn, id)?;
            if evs.is_empty() {
                let exists: Option<i64> = conn
                    .query_row("SELECT id FROM cards WHERE id = ?", params![id], |r| r.get(0))
                    .optional()?;
                if exists.is_none() {
                    bail!("card #{} not found and it left no history", id);
                }
                if json {
                    println!("[]");
                } else {
                    println!("(no events; #{} predates the event log)", id);
                }
                exit_with(EXIT_EMPTY);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&evs)?);
            } else {
                for e in &evs {
                    println!("{}", events::render_line(e));
                }
            }
        }

        Commands::Show { id, json } => {
            let card: Option<Card> = conn
                .query_row(
                    &format!("SELECT {} FROM cards WHERE id = ?", SELECT_COLS),
                    params![id],
                    row_to_card,
                )
                .optional()?;
            match card {
                Some(mut c) => {
                    notes::reconcile(conn, id)?;
                    if json {
                        load_entries(conn, std::slice::from_mut(&mut c));
                    }
                    print_card(&c, json, true);
                }
                None => bail!("card #{} not found", id),
            }
        }

        Commands::Note {
            action: Some(NoteAction::Edit { note_id, text, kind, by }),
            ..
        } => {
            if text.is_none() && kind.is_none() {
                bail!("nothing to change: give new text and/or --kind");
            }
            let now = now_str();
            let (card_id, old) = transaction(conn, || {
                let (card_id, old) = notes::edit(conn, note_id, text.as_deref(), kind.as_deref(), &now)?;
                let who = by.as_deref().unwrap_or("");
                if let Some(t) = &text {
                    if *t != old.body {
                        events::record(conn, card_id, None, "note_edit", &old.body, t, who, &now)?;
                    }
                }
                if let Some(k) = &kind {
                    if *k != old.kind {
                        events::record(conn, card_id, None, "note_kind", &old.kind, k, who, &now)?;
                    }
                }
                Ok((card_id, old))
            })?;
            println!("note {} on #{} edited (was: {})", note_id, card_id, old.render().replace('\n', " | "));
            refresh_views(&ctx, Some(card_id));
        }

        Commands::Note {
            action: Some(NoteAction::Rm { note_id, by }),
            ..
        } => {
            let now = now_str();
            let (card_id, old) = transaction(conn, || {
                let (card_id, old) = notes::remove(conn, note_id, &now)?;
                events::record(conn, card_id, None, "note_rm", &old.render(), "", by.as_deref().unwrap_or(""), &now)?;
                Ok((card_id, old))
            })?;
            println!("note {} removed from #{}: {}", note_id, card_id, old.render().replace('\n', " | "));
            refresh_views(&ctx, Some(card_id));
        }

        Commands::Note {
            action: None,
            id,
            text,
            stdin,
            file,
            kind,
            by,
            commit,
            unique,
        } => {
            let id = id.ok_or_else(|| anyhow::anyhow!("usage: bl note <card-id> \"text\" (or bl note edit|rm <note-id>)"))?;
            let text = if stdin {
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).context("failed to read stdin")?;
                buf
            } else if let Some(path) = &file {
                std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?
            } else {
                text.unwrap_or_default()
            };
            let text = text.trim_end().to_string();
            if text.trim().is_empty() {
                bail!("the note is empty");
            }
            let now = now_str();
            let old_commits: String = conn
                .query_row("SELECT commits FROM cards WHERE id = ?", params![id], |r| {
                    r.get(0)
                })
                .optional()?
                .ok_or_else(|| anyhow::anyhow!("card #{} not found", id))?;

            // A backlog outside a repository still deserves its note: warn and
            // keep going rather than losing what the agent wanted to record.
            let linked = match commit.as_deref().map(|rev| resolve_commit(&ctx, rev)) {
                Some(Ok(pair)) => Some(pair),
                Some(Err(e)) => {
                    eprintln!("bl: no commit linked ({})", e);
                    None
                }
                None => None,
            };

            let added = notes::add(
                conn,
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
            refresh_views(&ctx, Some(id));
        }

        Commands::Notes { id, kind, json } => {
            let exists: Option<i64> = conn
                .query_row("SELECT id FROM cards WHERE id = ?", params![id], |r| r.get(0))
                .optional()?;
            if exists.is_none() {
                bail!("card #{} not found", id);
            }
            notes::reconcile(conn, id)?;
            let all = notes::list(conn, id)?;
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
                    println!("[{}] {}{}  (note {})", n.kind, n.created_at, who, n.id);
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
            // Every word must appear somewhere on the card, so "hero art"
            // finds a card whose title says Hero and whose notes say art.
            let mut sql = format!("SELECT {} FROM cards WHERE 1=1", SELECT_COLS);
            let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            let (scope, pid) = ctx.scope();
            sql.push_str(&scope);
            if let Some(pid) = pid {
                binds.push(Box::new(pid));
            }
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
                let (clause, tag_binds) = tag_clause(l);
                sql.push_str(&clause);
                binds.extend(tag_binds);
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
                load_entries(conn, &mut cards);
                println!("{}", serde_json::to_string_pretty(&cards)?);
                if cards.is_empty() {
                    exit_with(EXIT_EMPTY);
                }
            } else if cards.is_empty() {
                println!("(no matches for {})", query.join(" "));
                exit_with(EXIT_EMPTY);
            } else {
                for c in &cards {
                    print_card(c, false, multi);
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
            let cutoff = format!("-{} seconds", secs);
            let (scope, pid) = ctx.scope();

            let stale: Vec<(i64, String, String)> = {
                let mut stmt = conn.prepare(&format!(
                    "SELECT id, claimed_by, claimed_at FROM cards
                     WHERE status = 'in_progress'
                       AND claimed_at != ''
                       AND claimed_at <= datetime('now', ?){}
                     ORDER BY id ASC",
                    scope
                ))?;
                let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(cutoff)];
                if let Some(pid) = pid {
                    binds.push(Box::new(pid));
                }
                let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
                let rows = stmt.query_map(refs.as_slice(), |r| {
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
                let n = conn.execute(
                    "UPDATE cards
                     SET status = 'ready', claimed_by = '', claimed_at = '', updated_at = ?1
                     WHERE id = ?2 AND status = 'in_progress' AND claimed_at = ?3",
                    params![now, id, since],
                )?;
                if n == 1 {
                    events::record(conn, *id, None, "status", "in_progress", "ready", "reap", &now)?;
                    events::record(conn, *id, None, "reap", who, "", "reap", &now)?;
                }
                notes::add(
                    conn,
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
                refresh_views(&ctx, None);
            }
        }

        Commands::Heartbeat { id, by } => {
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
            let text = agent_prompt(&ctx)?;
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
            // Inside a project the setting is the project's; from outside
            // (or with --all) it is the store-wide, every-project page.
            let scoped = ctx.project.as_ref().filter(|_| !ctx.all);
            let set = |target: &str| -> Result<()> {
                match scoped {
                    Some(p) => store::set_autoexport(conn, p.id, target),
                    None => meta_set(conn, "autoexport", target),
                }
            };
            match action {
                AutoAction::On { out } => {
                    let target = absolute(&out);
                    set(&target.display().to_string())?;
                    view::refresh(&ctx.path, scoped.map(|p| p.id), ctx.central, &target)?;
                    println!(
                        "auto-export on → {}{}",
                        target.display(),
                        match scoped {
                            Some(p) => format!("  (project {})", p.name),
                            None => String::new(),
                        }
                    );
                }
                AutoAction::Off => {
                    set("")?;
                    println!("auto-export off");
                }
                AutoAction::Status => match autoexport_target(&ctx) {
                    Some(t) => println!("auto-export on → {}", t.display()),
                    None => println!("auto-export off"),
                },
            }
        }

        Commands::Serve { port, also, open } => {
            let sources = view_sources(&ctx, also)?;
            view::serve(sources, port, open)?;
        }

        Commands::Export { out, open, auto } => {
            let sources = view_sources(&ctx, Vec::new())?;
            view::export(sources, &out)?;
            if auto {
                let target = absolute(&out);
                match ctx.project.as_ref().filter(|_| !ctx.all) {
                    Some(p) => store::set_autoexport(conn, p.id, &target.display().to_string())?,
                    None => meta_set(conn, "autoexport", &target.display().to_string())?,
                }
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
            let (project, title) = match (&ctx.project, ctx.all) {
                (Some(p), false) => (Some(p.id), p.name.clone()),
                _ => (None, ctx.path.display().to_string()),
            };
            board::run(
                &ctx.path,
                project,
                ctx.central,
                &title,
                &board::Opts {
                    label,
                    done,
                    width,
                    watch,
                    color: !no_color,
                    show_project: multi,
                },
            )?;
        }

        Commands::Decay { amount } => {
            if amount < 0 {
                bail!("amount must be >= 0");
            }
            let now = now_str();
            let (scope, pid) = ctx.scope();
            let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(amount), Box::new(now)];
            if let Some(pid) = pid {
                binds.push(Box::new(pid));
            }
            let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
            let n = conn.execute(
                &format!(
                    "UPDATE cards
                     SET priority = MAX(0, priority - ?1),
                         updated_at = ?2
                     WHERE status != 'done'{}",
                    scope
                ),
                refs.as_slice(),
            )?;
            println!("decayed {} non-done card(s) by {}", n, amount);
            refresh_views(&ctx, None);
        }
    }

    Ok(())
}
