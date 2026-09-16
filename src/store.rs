//! Where the backlog lives and which project a command is about.
//!
//! One central database under `~/.bl` holds every project's cards, and a
//! `projects` table maps a repository path to a project. A command run inside
//! a repository (or any of its worktrees) is scoped to that project without a
//! flag, an env var, or a wrapper script. `--db` / `BL_DB` keep the old
//! one-database-per-repo mode for anyone who wants it.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::env;
use std::path::{Path, PathBuf};

pub(crate) const CONFIG_FILE: &str = "config.yml";
pub(crate) const CENTRAL_DB: &str = "backlog.db";

/// `~/.bl`, or `$BL_HOME` (tests and unusual setups).
pub(crate) fn home() -> PathBuf {
    if let Some(h) = env::var_os("BL_HOME") {
        return PathBuf::from(h);
    }
    let base = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".bl")
}

pub(crate) fn config_path() -> PathBuf {
    home().join(CONFIG_FILE)
}

/// The central database: the `db:` key of `~/.bl/config.yml`, else
/// `~/.bl/backlog.db`. The file may not exist yet.
pub(crate) fn central_db_path() -> PathBuf {
    if let Ok(text) = std::fs::read_to_string(config_path()) {
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                if k.trim() == "db" {
                    let v = v.trim().trim_matches('"').trim_matches('\'');
                    if !v.is_empty() {
                        return expand_home(v);
                    }
                }
            }
        }
    }
    home().join(CENTRAL_DB)
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = env::var_os("HOME") {
            return PathBuf::from(h).join(rest);
        }
    }
    PathBuf::from(p)
}

/// Write the config the first time the central store is created. Existing
/// files are left alone: someone may have pointed `db:` elsewhere on purpose.
pub(crate) fn write_default_config(db: &Path) -> Result<()> {
    let cfg = config_path();
    if cfg.exists() {
        return Ok(());
    }
    if let Some(dir) = cfg.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    std::fs::write(
        &cfg,
        format!(
            "# bl central store. Every project's cards live in this one database;\n\
             # `bl project list` shows which repositories map to it.\n\
             db: {}\n",
            db.display()
        ),
    )
    .with_context(|| format!("failed to write {}", cfg.display()))?;
    Ok(())
}

// ---------------------------------------------------------------- projects

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Project {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) active: bool,
    pub(crate) autoexport: String,
    pub(crate) created_at: String,
}

const PROJECT_COLS: &str = "id, name, path, active, autoexport, created_at";

fn row_to_project(r: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get(0)?,
        name: r.get(1)?,
        path: r.get(2)?,
        active: r.get::<_, i64>(3)? != 0,
        autoexport: r.get(4)?,
        created_at: r.get(5)?,
    })
}

pub(crate) fn all(conn: &Connection) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM projects ORDER BY active DESC, name ASC",
        PROJECT_COLS
    ))?;
    let rows = stmt.query_map([], row_to_project)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub(crate) fn by_id(conn: &Connection, id: i64) -> Result<Option<Project>> {
    Ok(conn
        .query_row(
            &format!("SELECT {} FROM projects WHERE id = ?", PROJECT_COLS),
            params![id],
            row_to_project,
        )
        .optional()?)
}

pub(crate) fn by_name(conn: &Connection, name: &str) -> Result<Option<Project>> {
    Ok(conn
        .query_row(
            &format!("SELECT {} FROM projects WHERE name = ?", PROJECT_COLS),
            params![name],
            row_to_project,
        )
        .optional()?)
}

pub(crate) fn by_path(conn: &Connection, path: &Path) -> Result<Option<Project>> {
    let key = canon(path).display().to_string();
    Ok(conn
        .query_row(
            &format!("SELECT {} FROM projects WHERE path = ?", PROJECT_COLS),
            params![key],
            row_to_project,
        )
        .optional()?)
}

/// `name` and `#id` both resolve, so `--project 3` and `--project tinydungeons`
/// mean the same thing.
pub(crate) fn lookup(conn: &Connection, key: &str) -> Result<Option<Project>> {
    let key = key.trim();
    if let Some(p) = by_name(conn, key)? {
        return Ok(Some(p));
    }
    if let Ok(id) = key.trim_start_matches('#').parse::<i64>() {
        return by_id(conn, id);
    }
    Ok(None)
}

/// Register a directory as a project. The name defaults to the directory's
/// own name; a clash gets a clear error rather than a silent rename.
pub(crate) fn add(conn: &Connection, path: &Path, name: Option<&str>, now: &str) -> Result<Project> {
    let abs = canon(path);
    if !abs.is_dir() {
        bail!("{} is not a directory", abs.display());
    }
    let name = match name {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => abs
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string()),
    };
    if let Some(p) = by_path(conn, &abs)? {
        bail!("{} is already project '{}' (#{})", abs.display(), p.name, p.id);
    }
    if let Some(p) = by_name(conn, &name)? {
        bail!("project '{}' already exists at {} (#{}); pass --name", name, p.path, p.id);
    }
    conn.execute(
        "INSERT INTO projects (name, path, active, autoexport, created_at)
         VALUES (?1, ?2, 1, '', ?3)",
        params![name, abs.display().to_string(), now],
    )?;
    let id = conn.last_insert_rowid();
    Ok(by_id(conn, id)?.expect("project just inserted"))
}

pub(crate) fn set_active(conn: &Connection, id: i64, active: bool) -> Result<()> {
    conn.execute(
        "UPDATE projects SET active = ?1 WHERE id = ?2",
        params![active as i64, id],
    )?;
    Ok(())
}

pub(crate) fn set_autoexport(conn: &Connection, id: i64, target: &str) -> Result<()> {
    conn.execute(
        "UPDATE projects SET autoexport = ?1 WHERE id = ?2",
        params![target, id],
    )?;
    Ok(())
}

pub(crate) fn remove(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM projects WHERE id = ?", params![id])?;
    Ok(())
}

pub(crate) fn open_card_count(conn: &Connection, id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM cards WHERE project_id = ? AND status != 'done'",
        params![id],
        |r| r.get(0),
    )?)
}

pub(crate) fn card_count(conn: &Connection, id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM cards WHERE project_id = ?",
        params![id],
        |r| r.get(0),
    )?)
}

// ---------------------------------------------------------------- resolution

/// Where a command should look, before the database is opened. The project
/// itself is resolved after opening, by [`resolve_project`].
pub(crate) enum Target {
    Explicit(PathBuf),
    Central(PathBuf),
    RepoLocal(PathBuf),
}

pub(crate) fn target(explicit: Option<&Path>) -> Result<Target> {
    if let Some(p) = explicit {
        return Ok(Target::Explicit(p.to_path_buf()));
    }
    if let Ok(p) = env::var("BL_DB") {
        if !p.is_empty() {
            return Ok(Target::Explicit(PathBuf::from(p)));
        }
    }
    let central = central_db_path();
    let local = PathBuf::from(crate::DEFAULT_DB);
    if central.exists() {
        return Ok(Target::Central(central));
    }
    if local.exists() {
        return Ok(Target::RepoLocal(local));
    }
    bail!(
        "no backlog found: {} does not exist and there is no ./backlog.db here.\n\
         Run `bl init` to create the central store and register this repository,\n\
         or pass --db <path> to use a specific database.",
        central.display()
    )
}

/// Pick the project for this command: `--project`, then `BL_PROJECT`, then the
/// repository the working directory is in. In central mode a directory that
/// matches no project leaves `project` empty; commands that need one say so.
pub(crate) fn resolve_project(conn: &Connection, central: bool, flag: Option<&str>) -> Result<Option<Project>> {
    let key = match flag {
        Some(k) => Some(k.to_string()),
        None => env::var("BL_PROJECT").ok().filter(|s| !s.is_empty()),
    };
    if let Some(k) = key {
        return match lookup(conn, &k)? {
            Some(p) => Ok(Some(p)),
            None => bail!(
                "no project named '{}'; `bl project list` shows the registered ones",
                k
            ),
        };
    }
    if !central {
        // A single-repo database has exactly one project after migration;
        // everything in it belongs to that project.
        let ps = all(conn)?;
        return Ok(ps.into_iter().next());
    }
    for dir in candidate_dirs() {
        if let Some(p) = by_path(conn, &dir)? {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

/// Directories the working directory could belong to, most specific first:
/// the main repository behind any worktree, then the cwd and its parents.
fn candidate_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(root) = git_main_root(None) {
        out.push(root);
    }
    if let Ok(cwd) = env::current_dir() {
        let mut d: Option<&Path> = Some(cwd.as_path());
        while let Some(p) = d {
            let c = canon(p);
            if !out.contains(&c) {
                out.push(c);
            }
            d = p.parent();
        }
    }
    out
}

/// The root of the main repository for `dir` (default: cwd), seen through a
/// worktree: `git rev-parse --git-common-dir` points at the main `.git`, so a
/// lane in `.claude/worktrees/x` maps to the same project as the checkout it
/// was cut from.
pub(crate) fn git_main_root(dir: Option<&Path>) -> Option<PathBuf> {
    let mut cmd = std::process::Command::new("git");
    if let Some(d) = dir {
        cmd.arg("-C").arg(d);
    }
    let out = cmd
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if common.is_empty() {
        return None;
    }
    let common = PathBuf::from(common);
    // `<root>/.git` for a normal repository; a bare repo has no checkout root
    // worth registering, so its common dir is returned as-is.
    let root = if common.file_name().map(|f| f == ".git").unwrap_or(false) {
        common.parent().map(|p| p.to_path_buf())?
    } else {
        common
    };
    Some(canon(&root))
}

/// Absolute and symlink-free where possible, so the same directory always
/// produces the same key whatever spelling the shell handed us.
pub(crate) fn canon(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    abs.canonicalize().unwrap_or(abs)
}
