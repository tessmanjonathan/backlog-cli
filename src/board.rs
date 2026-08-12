//! `bl board` — the same board view, drawn in the terminal.
//!
//! Same information design as the HTML board: summary line, label bars,
//! priority histogram, then the four status columns side by side.

use crate::Card;
use anyhow::Result;
use std::io::{IsTerminal, Write};
use std::path::Path;

const COLUMNS: [(&str, &str); 4] = [
    ("new", "NEW"),
    ("ready", "READY"),
    ("in_progress", "IN PROGRESS"),
    ("done", "DONE"),
];

pub struct Opts {
    pub label: Option<String>,
    pub done: usize,
    pub width: Option<usize>,
    pub watch: Option<u64>,
    pub color: bool,
}

pub fn run(path: &Path, opts: &Opts) -> Result<()> {
    match opts.watch {
        None => {
            let cards = crate::view::read_cards(path)?;
            print!("{}", render(path, &cards, opts));
            std::io::stdout().flush()?;
        }
        Some(secs) => loop {
            let cards = crate::view::read_cards(path)?;
            // Home the cursor and clear, so the board redraws in place.
            print!("\x1b[H\x1b[2J{}", render(path, &cards, opts));
            std::io::stdout().flush()?;
            std::thread::sleep(std::time::Duration::from_secs(secs.max(1)));
        },
    }
    Ok(())
}

// ---------------------------------------------------------------- color

struct Paint {
    on: bool,
}

impl Paint {
    fn rgb(&self, s: &str, (r, g, b): (u8, u8, u8)) -> String {
        if self.on {
            format!("\x1b[38;2;{r};{g};{b}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn dim(&self, s: &str) -> String {
        if self.on {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn bold(&self, s: &str) -> String {
        if self.on {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

/// Priority heat, cold → hot across the 0–10000 range, matching the web board.
fn heat(priority: i32) -> (u8, u8, u8) {
    let t = (priority as f32 / 10000.0).clamp(0.0, 1.0);
    let cold = (124.0, 147.0, 160.0);
    let hot = (184.0, 69.0, 47.0);
    (
        (cold.0 + (hot.0 - cold.0) * t) as u8,
        (cold.1 + (hot.1 - cold.1) * t) as u8,
        (cold.2 + (hot.2 - cold.2) * t) as u8,
    )
}

fn status_color(status: &str) -> (u8, u8, u8) {
    match status {
        "new" => (147, 164, 181),
        "ready" => (79, 191, 169),
        "in_progress" => (220, 165, 63),
        _ => (108, 123, 116),
    }
}

// ---------------------------------------------------------------- layout

/// Visible width, counting a char as one column. Good enough for card titles;
/// wide CJK glyphs will read slightly narrow.
fn width_of(s: &str) -> usize {
    s.chars().count()
}

fn truncate(s: &str, max: usize) -> String {
    if width_of(s) <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}

fn wrap(s: &str, max: usize, lines: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let candidate = if cur.is_empty() {
            word.to_string()
        } else {
            format!("{cur} {word}")
        };
        if width_of(&candidate) <= max {
            cur = candidate;
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur = truncate(word, max);
        }
        if out.len() == lines {
            break;
        }
    }
    if out.len() < lines && !cur.is_empty() {
        out.push(cur);
    }
    // Mark a title that kept going past the last line we have room for.
    let consumed: usize = out.iter().map(|l| l.split_whitespace().count()).sum();
    if out.len() == lines && consumed < s.split_whitespace().count() {
        if let Some(last) = out.last_mut() {
            *last = truncate(last, max.saturating_sub(1)) + "…";
        }
    }
    out
}

fn pad(s: &str, w: usize) -> String {
    let len = width_of(s);
    if len >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - len))
    }
}

fn term_width() -> usize {
    if let Ok(cols) = std::env::var("COLUMNS") {
        if let Ok(n) = cols.parse::<usize>() {
            return n;
        }
    }
    // `stty size` is the dependency-free way to ask the tty how wide it is.
    if let Ok(out) = std::process::Command::new("stty")
        .arg("size")
        .stdin(std::process::Stdio::inherit())
        .output()
    {
        if let Some(cols) = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse::<usize>().ok())
        {
            if cols > 0 {
                return cols;
            }
        }
    }
    100
}

// ---------------------------------------------------------------- render

pub fn render(path: &Path, all: &[Card], opts: &Opts) -> String {
    let p = Paint {
        on: opts.color && std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    };
    let total_w = opts.width.unwrap_or_else(term_width).clamp(40, 240);

    let cards: Vec<&Card> = all
        .iter()
        .filter(|c| match &opts.label {
            Some(l) => &c.label == l,
            None => true,
        })
        .collect();

    let mut s = String::new();
    s.push_str(&header(path, &cards, &p, total_w, opts));
    s.push('\n');
    s.push_str(&panels(&cards, &p, total_w));
    s.push('\n');
    s.push_str(&board(&cards, &p, total_w, opts));
    s
}

fn header(path: &Path, cards: &[&Card], p: &Paint, w: usize, opts: &Opts) -> String {
    let open: Vec<&&Card> = cards.iter().filter(|c| c.status != "done").collect();
    let prog = cards.iter().filter(|c| c.status == "in_progress").count();
    let done = cards.iter().filter(|c| c.status == "done").count();
    let agents: std::collections::BTreeSet<&str> = cards
        .iter()
        .filter(|c| !c.claimed_by.is_empty())
        .map(|c| c.claimed_by.as_str())
        .collect();
    let avg = if open.is_empty() {
        0
    } else {
        open.iter().map(|c| c.priority as i64).sum::<i64>() / open.len() as i64
    };

    let title = p.bold("bl/board");
    let mut s = format!(
        "{}  {}\n",
        title,
        p.dim(&truncate(&path.display().to_string(), w.saturating_sub(12)))
    );

    let stats = format!(
        "{} open  ·  {} in progress{}  ·  avg priority {}  ·  {} done{}",
        open.len(),
        prog,
        if agents.is_empty() {
            String::new()
        } else {
            format!(" ({})", agents.into_iter().collect::<Vec<_>>().join(", "))
        },
        avg,
        done,
        match &opts.label {
            Some(l) => format!("  ·  label={l}"),
            None => String::new(),
        }
    );
    s.push_str(&stats);
    s.push('\n');
    s.push_str(&p.dim(&"─".repeat(w)));
    s.push('\n');
    s
}

fn panels(cards: &[&Card], p: &Paint, w: usize) -> String {
    let open: Vec<&&Card> = cards.iter().filter(|c| c.status != "done").collect();

    // Open cards by label
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for c in &open {
        *counts
            .entry(if c.label.is_empty() { "(none)" } else { &c.label })
            .or_default() += 1;
    }
    let mut rows: Vec<(&str, usize)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    rows.truncate(6);
    let max = rows.first().map(|r| r.1).unwrap_or(1).max(1);

    let name_w = 12usize;
    let bar_w = w.saturating_sub(name_w + 8).min(40);

    let mut s = String::new();
    s.push_str(&p.dim("OPEN BY LABEL"));
    s.push('\n');
    if rows.is_empty() {
        s.push_str(&p.dim("  nothing open\n"));
    }
    for (name, n) in &rows {
        let filled = (n * bar_w).div_ceil(max);
        s.push_str(&format!(
            "  {} {}{} {}\n",
            pad(&truncate(name, name_w), name_w),
            p.rgb(&"█".repeat(filled), (36, 140, 124)),
            p.dim(&"·".repeat(bar_w - filled)),
            n
        ));
    }

    // Priority distribution, one column per 500 points
    let mut buckets = [0usize; 20];
    for c in &open {
        buckets[((c.priority / 500) as usize).min(19)] += 1;
    }
    let bmax = buckets.iter().copied().max().unwrap_or(1).max(1);
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let spark: String = buckets
        .iter()
        .enumerate()
        .map(|(i, n)| {
            if *n == 0 {
                p.dim("·")
            } else {
                let level = ((n * 7).div_ceil(bmax)).min(7);
                p.rgb(&BLOCKS[level].to_string(), heat(i as i32 * 500 + 250))
            }
        })
        .collect();

    s.push('\n');
    s.push_str(&p.dim("OPEN PRIORITY  "));
    s.push_str(&spark);
    s.push('\n');
    s.push_str(&p.dim("               0            5000          10000\n"));
    s
}

fn board(cards: &[&Card], p: &Paint, w: usize, opts: &Opts) -> String {
    let gutter = 2usize;
    let col_w = (w.saturating_sub(gutter * 3)) / 4;
    let col_w = col_w.max(16);

    // Build each column's lines, then print them side by side.
    let mut columns: Vec<Vec<String>> = Vec::new();
    for (key, name) in COLUMNS {
        let mut items: Vec<&&Card> = cards.iter().filter(|c| c.status == key).collect();
        let total = items.len();
        if key == "done" {
            items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            items.truncate(opts.done);
        }
        let hidden = total - items.len();

        let mut lines = vec![
            format!(
                "{}{}",
                p.rgb(&pad(name, col_w.saturating_sub(4)), status_color(key)),
                p.dim(&format!("{:>4}", total))
            ),
            p.rgb(&"─".repeat(col_w), status_color(key)),
        ];

        if items.is_empty() {
            lines.push(p.dim(&pad("—", col_w)));
        }
        for c in items {
            let id = p.dim(&format!("#{}", c.id));
            let pri = p.rgb(&format!("{:>5}", c.priority), heat(c.priority));
            let id_plain = format!("#{}", c.id);
            let label_room = col_w.saturating_sub(width_of(&id_plain) + 6 + 2);
            let label = if c.label.is_empty() {
                " ".repeat(label_room)
            } else {
                p.dim(&pad(&truncate(&c.label, label_room), label_room))
            };
            lines.push(format!("{id} {label} {pri}"));

            for line in wrap(&c.title, col_w.saturating_sub(1), 2) {
                lines.push(pad(&line, col_w));
            }
            if !c.claimed_by.is_empty() {
                lines.push(p.rgb(
                    &pad(&truncate(&format!("@{}", c.claimed_by), col_w), col_w),
                    status_color("in_progress"),
                ));
            }
            lines.push(String::new());
        }
        if hidden > 0 {
            lines.push(p.dim(&format!("{hidden} older hidden")));
        }
        columns.push(lines);
    }

    let height = columns.iter().map(|c| c.len()).max().unwrap_or(0);
    let mut s = String::new();
    for row in 0..height {
        let mut line = String::new();
        for (i, col) in columns.iter().enumerate() {
            let cell = col.get(row).cloned().unwrap_or_default();
            // Pad by visible width — escape codes must not count toward it.
            let visible = strip_ansi_len(&cell);
            line.push_str(&cell);
            if i < columns.len() - 1 {
                line.push_str(&" ".repeat(col_w.saturating_sub(visible) + gutter));
            }
        }
        s.push_str(line.trim_end());
        s.push('\n');
    }
    s
}

/// Visible length of a string that may contain SGR escape sequences.
fn strip_ansi_len(s: &str) -> usize {
    let mut len = 0usize;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c2 in chars.by_ref() {
                if c2 == 'm' {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}
