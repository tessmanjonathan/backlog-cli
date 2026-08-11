//! `bl serve` — read-only localhost dashboard over one or more backlog databases.
//!
//! Deliberately dependency-free: a small blocking HTTP/1.1 server on std::net,
//! serving an embedded single-page dashboard plus a JSON card feed.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

const DASHBOARD: &str = include_str!("dashboard.html");

/// A database the dashboard is allowed to read. Paths are fixed at launch, so
/// a query parameter can only pick one of them — never an arbitrary file.
pub struct Source {
    pub label: String,
    pub path: PathBuf,
}

pub fn run(sources: Vec<Source>, port: u16, open: bool) -> Result<()> {
    if sources.is_empty() {
        anyhow::bail!("no databases to serve");
    }
    for s in &sources {
        if !s.path.exists() {
            anyhow::bail!("database not found: {}", s.path.display());
        }
    }

    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("failed to bind 127.0.0.1:{port}"))?;
    let url = format!("http://127.0.0.1:{port}");

    println!("bl dashboard on {url}");
    for (i, s) in sources.iter().enumerate() {
        println!("  [{i}] {}  {}", s.label, s.path.display());
    }
    println!("ctrl-c to stop");

    if open {
        let _ = std::process::Command::new("open").arg(&url).status();
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
                    let body = format!("{{\"error\":{}}}", json_string(&e.to_string()));
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

fn feed_json(sources: &[Source], idx: usize) -> Result<String> {
    let src = &sources[idx];
    let cards = read_cards(&src.path)?;

    let dbs: Vec<String> = sources
        .iter()
        .enumerate()
        .map(|(i, s)| {
            format!(
                "{{\"index\":{i},\"label\":{},\"path\":{}}}",
                json_string(&s.label),
                json_string(&s.path.display().to_string())
            )
        })
        .collect();

    Ok(format!(
        "{{\"db\":{{\"index\":{idx},\"label\":{},\"path\":{}}},\"databases\":[{}],\"cards\":[{}]}}",
        json_string(&src.label),
        json_string(&src.path.display().to_string()),
        dbs.join(","),
        cards.join(",")
    ))
}

fn read_cards(path: &Path) -> Result<Vec<String>> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("failed to open {}", path.display()))?;

    let mut stmt = conn.prepare(
        "SELECT id, title, notes, label, status, priority, outcome,
                COALESCE(claimed_by, ''), COALESCE(claimed_at, ''), created_at, updated_at
         FROM cards
         ORDER BY priority DESC, created_at ASC",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(format!(
            "{{\"id\":{},\"title\":{},\"notes\":{},\"label\":{},\"status\":{},\"priority\":{},\"outcome\":{},\"claimed_by\":{},\"claimed_at\":{},\"created_at\":{},\"updated_at\":{}}}",
            row.get::<_, i64>(0)?,
            json_string(&row.get::<_, String>(1)?),
            json_string(&row.get::<_, String>(2)?),
            json_string(&row.get::<_, String>(3)?),
            json_string(&row.get::<_, String>(4)?),
            row.get::<_, i64>(5)?,
            json_string(&row.get::<_, String>(6)?),
            json_string(&row.get::<_, String>(7)?),
            json_string(&row.get::<_, String>(8)?),
            json_string(&row.get::<_, String>(9)?),
            json_string(&row.get::<_, String>(10)?),
        ))
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}
