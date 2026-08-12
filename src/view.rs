//! The board view — the same embedded page, delivered two ways:
//! `bl serve` (live, polling localhost) and `bl export` (a self-contained
//! snapshot file with the cards baked in).
//!
//! Deliberately dependency-free: a small blocking HTTP/1.1 server on std::net.

use crate::Card;
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

const DASHBOARD: &str = include_str!("dashboard.html");

/// The placeholder the page ships with; export swaps it for real card data.
const BOOTSTRAP_SLOT: &str = r#"<script id="bldata" type="application/json">null</script>"#;

/// A database the board is allowed to read. Paths are fixed at launch, so a
/// query parameter can only pick one of them — never an arbitrary file.
pub struct Source {
    pub label: String,
    pub path: PathBuf,
}

#[derive(serde::Serialize)]
struct DbRef {
    index: usize,
    label: String,
    path: String,
}

#[derive(serde::Serialize)]
struct Feed {
    db: DbRef,
    databases: Vec<DbRef>,
    generated_at: String,
    cards: Vec<Card>,
}

// ---------------------------------------------------------------- serve

pub fn serve(sources: Vec<Source>, port: u16, open: bool) -> Result<()> {
    check_sources(&sources)?;

    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("failed to bind 127.0.0.1:{port}"))?;
    let url = format!("http://127.0.0.1:{port}");

    println!("bl board on {url}");
    for (i, s) in sources.iter().enumerate() {
        println!("  [{i}] {}  {}", s.label, s.path.display());
    }
    println!("ctrl-c to stop");

    if open {
        open_in_browser(&url);
    }

    // One thread per connection: browsers pre-open sockets they may never use,
    // and a blocking read on one of those would stall the whole board.
    let sources = std::sync::Arc::new(sources);
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let sources = std::sync::Arc::clone(&sources);
                std::thread::spawn(move || {
                    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(15)));
                    if let Err(e) = handle(s, &sources) {
                        eprintln!("request failed: {e}");
                    }
                });
            }
            Err(e) => eprintln!("connection failed: {e}"),
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, sources: &[Source]) -> Result<()> {
    let target = match read_request_target(&mut stream)? {
        Some(t) => t,
        None => return Ok(()),
    };
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target.as_str(), ""),
    };

    match path {
        "/" | "/index.html" => respond(&mut stream, 200, "text/html; charset=utf-8", DASHBOARD),
        "/api/cards" => {
            let idx = query_param(query, "db")
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|i| *i < sources.len())
                .unwrap_or(0);
            match feed_json(sources, idx) {
                Ok(body) => respond(&mut stream, 200, "application/json", &body),
                Err(e) => {
                    let body = serde_json::json!({ "error": e.to_string() }).to_string();
                    respond(&mut stream, 500, "application/json", &body)
                }
            }
        }
        _ => respond(&mut stream, 404, "text/plain; charset=utf-8", "not found"),
    }
}

/// Reads the request line, then drains headers so the client doesn't see a reset.
fn read_request_target(stream: &mut TcpStream) -> Result<Option<String>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/").to_string();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    if method != "GET" {
        return Ok(Some("/__method_not_allowed".into()));
    }
    Ok(Some(target))
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) -> Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()?;
    // Drain anything still in flight so the browser reads the full response.
    let _ = stream.set_nonblocking(true);
    let mut sink = [0u8; 256];
    let _ = stream.read(&mut sink);
    Ok(())
}

// ---------------------------------------------------------------- export

/// Writes a standalone snapshot: one HTML file with the cards baked in, no
/// server and no network needed to open it.
pub fn export(sources: Vec<Source>, out: &Path) -> Result<()> {
    check_sources(&sources)?;
    let total = write_snapshot(&sources[0], out)?;

    println!(
        "wrote {} ({} card{}, {:.0} KB)",
        out.display(),
        total,
        if total == 1 { "" } else { "s" },
        std::fs::metadata(out).map(|m| m.len()).unwrap_or(0) as f64 / 1024.0
    );
    if sources.len() > 1 {
        eprintln!("note: a snapshot holds one database; ignored the extra --also path(s)");
    }
    Ok(())
}

/// Rewrite an existing snapshot from the database, silently. This is what
/// makes the static file behave like a live view: every command that writes a
/// card calls it, so opening (or reloading) the HTML always shows the truth.
pub fn refresh(db: &Path, out: &Path) -> Result<()> {
    let source = Source {
        label: db.display().to_string(),
        path: db.to_path_buf(),
    };
    write_snapshot(&source, out)?;
    Ok(())
}

/// Bakes one database's cards into the dashboard template. Returns the card count.
fn write_snapshot(source: &Source, out: &Path) -> Result<usize> {
    let feed = feed(std::slice::from_ref(source), 0)?;
    let total = feed.cards.len();

    // `<` can't appear raw inside the data block or a card's text could close
    // the script tag; escaping it keeps the JSON valid either way.
    let safe = serde_json::to_string(&feed)?.replace('<', "\\u003c");
    let block = format!(r#"<script id="bldata" type="application/json">{safe}</script>"#);
    let page = DASHBOARD.replace(BOOTSTRAP_SLOT, &block);
    if page == DASHBOARD {
        anyhow::bail!("dashboard template is missing its data slot");
    }

    if let Some(dir) = out.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
    }
    // Write beside the target and rename, so a reader never sees a half-written
    // page when a refresh lands while the browser is loading it.
    let tmp = out.with_extension("html.tmp");
    std::fs::write(&tmp, page).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, out)
        .with_context(|| format!("failed to replace {}", out.display()))?;

    Ok(total)
}

pub fn open_in_browser(target: &str) {
    let _ = std::process::Command::new("open").arg(target).status();
}

// ---------------------------------------------------------------- data

fn check_sources(sources: &[Source]) -> Result<()> {
    if sources.is_empty() {
        anyhow::bail!("no databases to read");
    }
    for s in sources {
        if !s.path.exists() {
            anyhow::bail!("database not found: {}", s.path.display());
        }
    }
    Ok(())
}

fn feed_json(sources: &[Source], idx: usize) -> Result<String> {
    Ok(serde_json::to_string(&feed(sources, idx)?)?)
}

fn feed(sources: &[Source], idx: usize) -> Result<Feed> {
    let src = &sources[idx];
    // Absolute, so a snapshot mailed elsewhere still says which backlog it came from.
    let dbref = |i: usize, s: &Source| DbRef {
        index: i,
        label: s.label.clone(),
        path: s
            .path
            .canonicalize()
            .unwrap_or_else(|_| s.path.clone())
            .display()
            .to_string(),
    };
    let feed = Feed {
        db: dbref(idx, src),
        databases: sources.iter().enumerate().map(|(i, s)| dbref(i, s)).collect(),
        generated_at: chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        cards: read_cards(&src.path)?,
    };
    Ok(feed)
}

/// Reads every card, highest priority first. Opened read-only: the board and
/// the snapshot never write to a backlog.
pub fn read_cards(path: &Path) -> Result<Vec<Card>> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("failed to open {}", path.display()))?;

    // A database reached through --also is opened read-only and may predate a
    // column this build knows about; substitute empties rather than failing.
    let present: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(cards)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    let cols: Vec<String> = crate::SELECT_COLS
        .split(", ")
        .map(|c| {
            if present.iter().any(|p| p == c) {
                c.to_string()
            } else {
                format!("'' AS {c}")
            }
        })
        .collect();

    let sql = format!(
        "SELECT {} FROM cards ORDER BY priority DESC, created_at ASC",
        cols.join(", ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], crate::row_to_card)?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    // The typed notes ride along so the card detail can show a timeline.
    crate::load_entries(&conn, &mut out);
    Ok(out)
}
