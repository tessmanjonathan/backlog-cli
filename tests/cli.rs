//! End-to-end tests for the bl binary. Cargo builds `bl` before running these
//! and hands its path in as CARGO_BIN_EXE_bl; every test drives that binary as
//! a subprocess against its own throwaway BL_HOME and git repositories, so the
//! tests run in parallel and never touch ~/.bl.

use assert_cmd::Command;
use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Builds a `&[String]` from mixed literals, ids and paths (`.display()`).
macro_rules! a {
    ($($x:expr),* $(,)?) => { &[$(($x).to_string()),*][..] };
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    /// stdout then stderr, the `2>&1` view.
    fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// One test's world: a temp dir holding BL_HOME and the repos.
struct T {
    _tmp: TempDir,
    root: PathBuf,
}

impl T {
    fn new() -> T {
        let tmp = TempDir::new().unwrap();
        // Canonical, so paths bl derives from its cwd (/private/var on macOS)
        // compare equal to the ones the test builds.
        let root = tmp.path().canonicalize().unwrap();
        T { _tmp: tmp, root }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn home(&self) -> PathBuf {
        self.p("home")
    }

    /// A git repository with one empty commit.
    fn repo(&self, name: &str) -> PathBuf {
        let dir = self.p(name);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        commit(&dir, "root");
        dir
    }

    fn cmd(&self, dir: &Path, args: &[String]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_bl"));
        c.current_dir(dir)
            .args(args)
            .env("BL_HOME", self.home())
            .env("BL_NO_AUTOEXPORT", "1")
            .env_remove("BL_DB")
            .env_remove("BL_PROJECT")
            .env_remove("BL_AGENT");
        c
    }

    fn exec(mut c: Command) -> Out {
        let o = c.output().unwrap();
        Out {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    fn run(&self, dir: &Path, args: &[String]) -> Out {
        T::exec(self.cmd(dir, args))
    }

    fn run_stdin(&self, dir: &Path, args: &[String], input: &str) -> Out {
        let mut c = self.cmd(dir, args);
        c.write_stdin(input.to_string());
        T::exec(c)
    }

    /// Runs and requires exit 0; returns stdout.
    #[track_caller]
    fn ok(&self, dir: &Path, args: &[String]) -> String {
        let o = self.run(dir, args);
        assert_eq!(o.code, 0, "bl {args:?} failed:\n{}", o.all());
        o.stdout
    }

    #[track_caller]
    fn ok_stdin(&self, dir: &Path, args: &[String], input: &str) -> String {
        let o = self.run_stdin(dir, args, input);
        assert_eq!(o.code, 0, "bl {args:?} failed:\n{}", o.all());
        o.stdout
    }

    /// Runs and requires a non-zero exit.
    #[track_caller]
    fn fails(&self, dir: &Path, args: &[String]) -> Out {
        let o = self.run(dir, args);
        assert_ne!(o.code, 0, "bl {args:?} should have failed:\n{}", o.all());
        o
    }

    /// `bl create <args>`, returning the new card's id.
    #[track_caller]
    fn create(&self, dir: &Path, args: &[String]) -> i64 {
        let mut all = vec!["create".to_string()];
        all.extend_from_slice(args);
        id(&self.ok(dir, &all))
    }

    #[track_caller]
    fn json(&self, dir: &Path, args: &[String]) -> Value {
        let out = self.ok(dir, args);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("bl {args:?}: not JSON ({e}):\n{out}"))
    }
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} failed in {}", dir.display());
}

fn commit(dir: &Path, msg: &str) {
    git(dir, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", msg]);
}

/// The first `#N` in a command's output.
#[track_caller]
fn id(out: &str) -> i64 {
    let c = Regex::new(r"#(\d+)").unwrap().captures(out);
    c.unwrap_or_else(|| panic!("no #id in:\n{out}"))[1].parse().unwrap()
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

#[track_caller]
fn must(hay: &str, re: &str, what: &str) {
    assert!(Regex::new(re).unwrap().is_match(hay), "{what}: /{re}/ not found in:\n{hay}");
}

#[track_caller]
fn must_not(hay: &str, re: &str, what: &str) {
    assert!(!Regex::new(re).unwrap().is_match(hay), "{what}: /{re}/ found in:\n{hay}");
}

#[track_caller]
fn has(hay: &str, needle: &str, what: &str) {
    assert!(hay.contains(needle), "{what}: '{needle}' not found in:\n{hay}");
}

#[track_caller]
fn lacks(hay: &str, needle: &str, what: &str) {
    assert!(!hay.contains(needle), "{what}: '{needle}' found in:\n{hay}");
}

fn arr(v: &Value) -> &Vec<Value> {
    v.as_array().unwrap_or_else(|| panic!("not a JSON array: {v}"))
}

/// Cards in a `--json` listing whose title starts with `prefix`.
fn titled(v: &Value, prefix: &str) -> usize {
    arr(v).iter().filter(|c| c["title"].as_str().is_some_and(|t| t.starts_with(prefix))).count()
}

/// Writes a database by hand, the way an older build left it.
fn sqlite(path: &Path, sql: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    rusqlite::Connection::open(path).unwrap().execute_batch(sql).unwrap();
}

/// A database from before projects existed (pre-0.5 schema).
const PRE_PROJECTS: &str = r#"
CREATE TABLE cards (
    id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, notes TEXT NOT NULL DEFAULT '',
    label TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'new'
        CHECK(status IN ('new','ready','in_progress','done')),
    priority INTEGER NOT NULL DEFAULT 5000 CHECK(priority BETWEEN 0 AND 10000),
    outcome TEXT NOT NULL DEFAULT '', claimed_by TEXT NOT NULL DEFAULT '', claimed_at TEXT NOT NULL DEFAULT '',
    commits TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')));
INSERT INTO cards (title, notes, priority) VALUES ('old card', 'one line of notes', 7000);
INSERT INTO cards (title, label, priority) VALUES ('space labelled', 'art enemies c676', 6000);
"#;

/// A database written by the 0.4 build: notes table with a cascade FK, status
/// CHECK without blocked.
const V04: &str = r#"
CREATE TABLE cards (
    id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, notes TEXT NOT NULL DEFAULT '',
    label TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'new'
        CHECK(status IN ('new','ready','in_progress','done')),
    priority INTEGER NOT NULL DEFAULT 5000 CHECK(priority BETWEEN 0 AND 10000),
    outcome TEXT NOT NULL DEFAULT '', claimed_by TEXT NOT NULL DEFAULT '', claimed_at TEXT NOT NULL DEFAULT '',
    commits TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')), project_id INTEGER NOT NULL DEFAULT 0, legacy_id INTEGER);
CREATE TABLE projects (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE, path TEXT NOT NULL UNIQUE,
    active INTEGER NOT NULL DEFAULT 1, autoexport TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL DEFAULT (datetime('now')));
CREATE TABLE notes (id INTEGER PRIMARY KEY AUTOINCREMENT, card_id INTEGER NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
    kind TEXT NOT NULL DEFAULT 'note', author TEXT NOT NULL DEFAULT '', body TEXT NOT NULL, commit_sha TEXT NOT NULL DEFAULT '',
    commit_subject TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL DEFAULT (datetime('now')));
INSERT INTO projects (name, path) VALUES ('v04', '/nonexistent/v04');
INSERT INTO cards (title, notes, priority, project_id) VALUES ('kept card', '[finding] must survive the rebuild', 7000, 1);
INSERT INTO notes (card_id, kind, body) VALUES (1, 'finding', 'must survive the rebuild');
"#;

/// The store, with project alpha registered and holding "alpha one" (9000).
fn alpha_only() -> (T, PathBuf) {
    let t = T::new();
    let alpha = t.repo("alpha");
    t.ok(&alpha, a!["init"]);
    t.ok(&alpha, a!["create", "alpha one", "-p", 9000]);
    (t, alpha)
}

/// `alpha_only` plus project beta holding "beta one" (9500).
fn alpha_beta() -> (T, PathBuf, PathBuf) {
    let (t, alpha) = alpha_only();
    let beta = t.repo("beta");
    t.ok(&beta, a!["project", "add"]);
    t.ok(&beta, a!["create", "beta one", "-p", 9500]);
    (t, alpha, beta)
}

/// A worktree of `repo` on branch `lane`.
fn worktree(repo: &Path) -> PathBuf {
    git(repo, &["worktree", "add", "-q", ".wt/lane", "-b", "lane"]);
    repo.join(".wt/lane")
}

/// The legacy (pre-projects) database at <root>/legacy/backlog.db.
fn legacy_db(t: &T) -> String {
    let p = t.p("legacy/backlog.db");
    sqlite(&p, PRE_PROJECTS);
    p.display().to_string()
}

#[test]
fn no_command_mints_a_database() {
    let t = T::new();
    let alpha = t.repo("alpha");
    t.fails(&alpha, a!["list"]);
    assert!(!alpha.join("backlog.db").exists(), "stray backlog.db minted");
    assert!(!t.home().join("backlog.db").exists(), "central db minted by list");
}

#[test]
fn init_creates_the_store_and_registers_the_repo() {
    let t = T::new();
    let alpha = t.repo("alpha");
    has(&t.ok(&alpha, a!["init"]), "registered project 'alpha'", "init did not register alpha");
    let cfg = read(&t.home().join("config.yml"));
    let db = regex::escape(&t.home().join("backlog.db").display().to_string());
    must(&cfg, &format!("(?m)^db: {db}"), "config db line");
}

#[test]
fn a_worktree_resolves_to_its_main_repo() {
    let (t, alpha) = alpha_only();
    let lane = worktree(&alpha);
    has(&t.ok(&lane, a!["project", "current"]), " alpha ", "worktree did not resolve to alpha");
    t.ok(&lane, a!["create", "alpha two from worktree", "-p", 8000]);
    assert!(!lane.join("backlog.db").exists(), "worktree minted a db");
    assert_eq!(titled(&t.json(&alpha, a!["list", "--json"]), "alpha two from worktree"), 1);
}

#[test]
fn commit_linking_resolves_from_a_worktree() {
    let (t, alpha) = alpha_only();
    let card = t.create(&alpha, a!["linked card"]);
    let lane = worktree(&alpha);
    commit(&lane, "lane work");
    has(&t.ok(&lane, a!["note", card, "linked from lane", "--commit"]), "commit", "no commit linked");
}

#[test]
fn scope_is_the_project_or_all_or_everything_outside() {
    let (t, alpha, beta) = alpha_beta();
    t.ok(&alpha, a!["create", "alpha two", "-p", 8000]);
    assert_eq!(arr(&t.json(&beta, a!["list", "--json"])).len(), 1, "beta sees alpha cards");
    assert_eq!(arr(&t.json(&alpha, a!["list", "--json"])).len(), 2, "alpha count");
    assert_eq!(arr(&t.json(&alpha, a!["list", "--all", "--json"])).len(), 3, "--all count");
    assert_eq!(arr(&t.json(&t.root, a!["list", "--json"])).len(), 3, "outside sees all active");
    has(&t.ok(&t.root, a!["next"]), "beta: ", "next outside should pick beta 9500 with project prefix");
}

#[test]
fn deactivate_hides_a_project_from_next() {
    let (t, _, _) = alpha_beta();
    t.ok(&t.root, a!["project", "deactivate", "beta"]);
    has(&t.ok(&t.root, a!["next"]), "alpha one", "deactivated beta still picked");
    has(&t.ok(&t.root, a!["--project", "beta", "list"]), "beta one", "--project beta should still read it");
    t.ok(&t.root, a!["project", "activate", "beta"]);
    has(&t.ok(&t.root, a!["next"]), "beta one", "activate did not bring beta back");
}

#[test]
fn create_outside_a_project_needs_project_flag() {
    let (t, _) = alpha_only();
    t.fails(&t.root, a!["create", "nowhere"]);
    t.ok(&t.root, a!["--project", "alpha", "create", "via flag"]);
}

#[test]
fn a_pre_projects_database_migrates_on_open() {
    let t = T::new();
    let db = legacy_db(&t);
    has(&t.ok(&t.root, a!["--db", db, "list"]), "old card", "legacy cards unreadable after migration");
    has(&t.ok(&t.root, a!["--db", db, "project", "list"]), "legacy", "legacy project row");
    t.ok(&t.root, a!["--db", db, "create", "legacy card"]);
    has(&t.ok(&t.root, a!["--db", db, "list"]), "legacy card", "legacy create");
    has(&t.ok(&t.root, a!["--db", db, "notes", 1]), "one line of notes", "legacy notes adopted");
}

#[test]
fn an_unregistered_repo_falls_back_to_its_own_db() {
    let (t, _) = alpha_only();
    let gamma = t.repo("gamma");
    t.ok(&gamma, a!["init", "--db", "./backlog.db"]);
    t.run(&gamma, a!["create", "gamma local"]);
    let o = t.run(&gamma, a!["list"]);
    has(&o.all(), "using ./backlog.db", "no fallback hint");
    has(&o.stdout, "gamma local", "fallback did not read local db");
}

#[test]
fn board_and_export_render_for_a_project_and_the_store() {
    let (t, alpha, _) = alpha_beta();
    has(&t.ok(&alpha, a!["board", "--no-color"]), "alpha", "board title");
    let page = t.p("alpha.html");
    t.ok(&alpha, a!["export", "-o", page.display()]);
    has(&read(&page), "alpha one", "export content");
    let all = t.p("all.html");
    t.ok(&t.root, a!["export", "-o", all.display()]);
    has(&read(&all), "beta one", "store export");
}

#[test]
fn auto_export_refreshes_the_project_page() {
    let (t, alpha) = alpha_only();
    let page = alpha.join("view/index.html");
    let mut on = t.cmd(&alpha, a!["auto", "on", "-o", page.display()]);
    on.env_remove("BL_NO_AUTOEXPORT");
    has(&T::exec(on).stdout, "project alpha", "auto on scope");
    let mut create = t.cmd(&alpha, a!["create", "refresh me"]);
    create.env_remove("BL_NO_AUTOEXPORT");
    assert_eq!(T::exec(create).code, 0);
    has(&read(&page), "refresh me", "auto export not refreshed");
}

#[test]
fn import_keeps_legacy_ids_and_is_idempotent() {
    let (t, _) = alpha_only();
    let db = legacy_db(&t);
    t.ok(&t.root, a!["--db", db, "create", "legacy card"]);
    let legacy = t.p("legacy");
    git(&legacy, &["init", "-q", "."]);
    let out = t.ok(&legacy, a!["import", db]);
    has(&out, "3 card(s) imported", "import count");
    let new: i64 = Regex::new(r"#1 → #(\d+)").unwrap().captures(&out).expect("no #1 → #N")[1].parse().unwrap();
    has(&t.ok(&legacy, a!["show", new]), "imported: was #1", "legacy id not shown");
    has(&t.ok(&legacy, a!["notes", new]), "one line of notes", "legacy notes not imported");
    has(&t.ok(&legacy, a!["project", "current"]), " legacy ", "legacy dir not registered by import");
    has(
        &t.ok(&legacy, a!["import", db]),
        "0 card(s) imported, 0 note(s), 3 already present",
        "import not idempotent",
    );
}

#[test]
fn migrate_scan_imports_a_repo_level_db() {
    let (t, _) = alpha_only();
    let gamma = t.repo("gamma");
    t.ok(&gamma, a!["init", "--db", "./backlog.db"]);
    t.run(&gamma, a!["create", "gamma local"]);
    must(&t.ok(&t.root, a!["migrate", "--scan", t.root.display(), "--dry-run"]), "would import.*gamma", "migrate dry-run");
    must(&t.ok(&t.root, a!["migrate", "--scan", t.root.display()]), r"gamma.*1 card\(s\) imported", "migrate gamma");
    std::fs::remove_file(gamma.join("backlog.db")).unwrap();
    has(&t.ok(&gamma, a!["list"]), "gamma local", "gamma not served from the store after migrate");
}

#[test]
fn pages_carry_the_project_list() {
    let (t, alpha, _) = alpha_beta();
    let all = t.p("all.html");
    t.ok(&t.root, a!["export", "-o", all.display()]);
    let page = read(&all);
    has(&page, r#""projects":["#, "store page has no projects list");
    has(&page, r#""central":true"#, "store page not marked central");
    has(&page, r#"id="projectpick""#, "no project picker in page");
    let scoped = t.p("alpha2.html");
    t.ok(&alpha, a!["export", "-o", scoped.display()]);
    must(&read(&scoped), r#""project":\{"id":[0-9]*,"name":"alpha""#, "scoped page not scoped to alpha");
}

#[test]
fn edit_retitle_and_move() {
    let (t, alpha, beta) = alpha_beta();
    let eid = t.create(&alpha, a!["typo in tilte", "-l", "ui", "-p", 4000]);
    has(&t.ok(&alpha, a!["retitle", eid, "typo in title, fixed"]), "retitled", "retitle");
    has(&t.ok(&alpha, a!["show", eid]), "typo in title, fixed", "retitle not applied");
    has(
        &t.ok(&alpha, a!["edit", eid, "--label", "polish", "--priority", 6500, "--status", "ready"]),
        "label, priority, status",
        "edit fields",
    );
    must(&t.ok(&alpha, a!["show", eid]), r"\[ 6500\]  ready         alpha: \[polish\]", "edit not applied");
    t.ok(&alpha, a!["note", eid, "first note"]);
    has(&t.ok(&alpha, a!["edit", eid, "--notes", "replaced line one\nreplaced line two"]), "notes", "edit --notes");
    let notes = t.ok(&alpha, a!["notes", eid]);
    assert_eq!(notes.lines().filter(|l| l.contains("replaced line")).count(), 2, "notes not replaced as rows:\n{notes}");
    lacks(&notes, "first note", "old note survived the replace");
    t.fails(&alpha, a!["edit", eid]);
    has(&t.ok(&alpha, a!["edit", eid, "--move", "beta"]), "project → beta", "move");
    has(&t.ok(&beta, a!["list"]), "typo in title, fixed", "card not in beta after move");
    lacks(&t.ok(&alpha, a!["list"]), "typo in title, fixed", "card still in alpha after move");
    t.fails(&alpha, a!["edit", eid, "--title", ""]);
}

#[test]
fn every_change_lands_in_history_with_who_did_it() {
    let (t, alpha) = alpha_only();
    let hid = t.create(&alpha, a!["audited card", "-p", 3000, "--by", "minter"]);
    t.ok(&alpha, a!["claim", hid, "--by", "worker"]);
    t.ok(&alpha, a!["release", hid, "--by", "worker"]);
    t.ok(&alpha, a!["set-priority", hid, 3500, "--by", "ranker"]);
    t.ok(&alpha, a!["edit", hid, "--title", "audited card, renamed", "--label", "audit", "--by", "editor"]);
    t.ok(&alpha, a!["status", hid, "done", "--outcome", "shipped", "--by", "closer"]);
    let h = t.ok(&alpha, a!["history", hid]);
    must(&h, "created .*→ audited card  by minter", "created event");
    must(&h, "claim .*→ worker  by worker", "claim event");
    must(&h, "status .*new → in_progress  by worker", "claim status event");
    must(&h, "release .*worker →  by worker", "release event");
    must(&h, "priority .*3000 → 3500  by ranker", "priority event");
    must(&h, "title .*audited card → audited card, renamed  by editor", "title event");
    must(&h, "label .*→ audit  by editor", "label event");
    must(&h, "status .*ready → done  by closer", "done event");
    must(&h, "outcome .*→ shipped  by closer", "outcome event");
    assert_eq!(arr(&t.json(&alpha, a!["history", hid, "--json"])).len(), 10, "history --json count");
    let nid = id(&t.ok(&alpha, a!["next", "--claim", "--by", "looper"]));
    must(&t.ok(&alpha, a!["history", nid]), "claim .*→ looper", "next --claim not logged");
    t.ok(&alpha, a!["release", nid]);
    let page = t.p("hist.html");
    t.ok(&alpha, a!["export", "-o", page.display()]);
    has(&read(&page), r#""events":["#, "page carries no events");
    t.fails(&alpha, a!["history", 99999]);
}

#[test]
fn delete_keeps_the_card_in_history() {
    let (t, alpha) = alpha_only();
    let did = t.create(&alpha, a!["filed by mistake", "-l", "oops"]);
    t.ok(&alpha, a!["note", did, "this note must survive in history", "-k", "finding", "--by", "noter"]);
    t.ok(&alpha, a!["claim", did, "--by", "holder"]);
    t.fails(&alpha, a!["delete", did]);
    t.ok(&alpha, a!["show", did]);
    has(
        &t.ok(&alpha, a!["delete", did, "--force", "--why", "duplicate of #1", "--by", "janitor"]),
        "deleted: filed by mistake",
        "delete output",
    );
    t.fails(&alpha, a!["show", did]);
    let h = t.ok(&alpha, a!["history", did]);
    must(&h, "deleted .*title: filed by mistake", "deleted event lacks title");
    has(&h, "this note must survive in history", "deleted event lacks notes");
    has(&h, "→ duplicate of #1  by janitor", "deleted event lacks why/by");
    let deleted = arr(&t.json(&alpha, a!["history", did, "--json"])).iter().filter(|e| e["kind"] == "deleted").count();
    assert_eq!(deleted, 1, "deleted event count");
}

#[test]
fn bulk_edit_by_ids_and_where() {
    let (t, alpha) = alpha_only();
    let b1 = t.create(&alpha, a!["bulk a", "-l", "art", "-p", 1000]);
    let b2 = t.create(&alpha, a!["bulk b", "-l", "art", "-p", 1100]);
    let b3 = t.create(&alpha, a!["bulk c", "-l", "code", "-p", 1200]);
    has(
        &t.ok(&alpha, a!["edit", "--ids", format!("{b1},{b2}"), "--priority", 100, "--by", "mover"]),
        "edited 2 card(s): priority",
        "edit --ids",
    );
    must(&t.ok(&alpha, a!["show", b2]), r"\[  100\]", "--ids priority not applied");
    let set = a!["edit", "--where", "label=art", "--where", "status=new", "--set", "priority=200", "--set", "label=visual"];
    let mut dry = set.to_vec();
    dry.push("--dry-run".into());
    has(&t.ok(&alpha, &dry), "would edit 2 card(s); nothing written", "dry-run count");
    has(&t.ok(&alpha, a!["show", b1]), "[art]", "dry-run wrote");
    has(&t.ok(&alpha, set), "edited 2 card(s)", "where/set");
    must(&t.ok(&alpha, a!["show", b1]), r"\[  200\]  new           alpha: \[visual\]", "where/set not applied");
    has(&t.ok(&alpha, a!["show", b3]), "[code]", "where touched a non-matching card");
    t.fails(&alpha, a!["edit", "--where", "priority<150", "--set", "priority=300"]);
    assert_eq!(t.run(&alpha, a!["edit", "--where", "label=nothing-here", "--set", "priority=1"]).code, 2, "no match should exit 2");
    must(&t.ok(&alpha, a!["history", b1]), "label .*art → visual", "bulk edit event missing");
    t.fails(&alpha, a!["edit", "--set", "priority=1"]);
}

#[test]
fn note_edit_and_rm_by_note_id() {
    let (t, alpha) = alpha_only();
    let nc = t.create(&alpha, a!["note surgery"]);
    t.ok(&alpha, a!["note", nc, "first"]);
    t.ok(&alpha, a!["note", nc, "secnod, with typo", "-k", "finding"]);
    t.ok(&alpha, a!["note", nc, "third"]);
    let notes = t.ok(&alpha, a!["notes", nc]);
    let lines: Vec<&str> = notes.lines().collect();
    let at = lines.iter().position(|l| l.contains("secnod")).expect("no secnod line");
    let n2: i64 = Regex::new(r"\(note (\d+)\)").unwrap().captures(lines[at - 1]).unwrap_or_else(|| panic!("bl notes prints no note id:\n{notes}"))[1]
        .parse()
        .unwrap();
    has(
        &t.ok(&alpha, a!["note", "edit", n2, "second, fixed", "-k", "decision", "--by", "fixer"]),
        &format!("note {n2} on #{nc} edited"),
        "note edit",
    );
    must(&t.ok(&alpha, a!["notes", nc]), &format!(r"(?m)^\[decision\].*\(note {n2}\)"), "kind not changed");
    has(&t.ok(&alpha, a!["show", nc]), "first | [decision] second, fixed | third", "mirror not rebuilt after edit");
    has(&t.ok(&alpha, a!["note", "rm", n2, "--by", "fixer"]), &format!("removed from #{nc}"), "note rm");
    must(&t.ok(&alpha, a!["show", nc]), r"(?m)notes: first \| third$", "mirror not rebuilt after rm");
    let notes = t.ok(&alpha, a!["notes", nc]);
    assert_eq!(notes.lines().filter(|l| l.starts_with('[')).count(), 2, "note count after rm:\n{notes}");
    let h = t.ok(&alpha, a!["history", nc]);
    must(&h, "note_edit .*secnod, with typo → second, fixed  by fixer", "note_edit event");
    must(&h, "note_rm .*second, fixed →  by fixer", "note_rm event");
    t.fails(&alpha, a!["note", "rm", 999999]);
    t.fails(&alpha, a!["note", nc]);
}

#[test]
fn import_stdin_takes_arrays_and_json_lines() {
    let (t, alpha, _) = alpha_beta();
    let batch = r#"[{"title":"planned one","label":"plan","priority":6100,"notes":"from the planner"},{"title":"planned two","notes":[{"kind":"finding","body":"typed note"},"plain note"]}]"#;
    let out: Value = serde_json::from_str(&t.ok_stdin(&alpha, a!["import", "--stdin", "--by", "planner"], batch)).unwrap();
    let rows = arr(&out);
    assert_eq!(rows.iter().filter(|r| r["created"] == true).count(), 2, "stdin array created count: {out}");
    let p1 = rows[0]["id"].as_i64().unwrap();
    let p2 = rows[rows.len() - 1]["id"].as_i64().unwrap();
    has(&t.ok(&alpha, a!["show", p1]), "[ 6100]  new           alpha: [plan] planned one", "stdin card fields");
    has(&t.ok(&alpha, a!["notes", p1]), "planner", "stdin note author");
    must(&t.ok(&alpha, a!["notes", p2]), r"(?m)^\[finding\]", "typed stdin note");
    must(&t.ok(&alpha, a!["history", p1]), "created .*→ planned one  by planner", "stdin created event");
    let lines = "{\"title\":\"planned one\"}\n{\"title\":\"planned three\",\"project\":\"beta\"}\n";
    let out: Value = serde_json::from_str(&t.ok_stdin(&alpha, a!["import", "--stdin", "--if-absent"], lines)).unwrap();
    assert!(arr(&out).iter().any(|r| r["created"] == false), "--if-absent did not report the existing card: {out}");
    assert!(arr(&out).iter().any(|r| r["project"] == "beta"), "per-card project: {out}");
    let count = || arr(&t.json(&alpha, a!["list", "--all", "--json"])).len();
    let before = count();
    assert_ne!(t.run_stdin(&alpha, a!["import", "--stdin"], "{\"title\":\"ok\"}\n{\"title\":\"\"}\n").code, 0, "empty title accepted");
    assert_eq!(count(), before, "bad batch left cards behind");
    let out: Value = serde_json::from_str(&t.ok_stdin(&alpha, a!["import", "--stdin", "--dry-run"], r#"[{"title":"dry"}]"#)).unwrap();
    assert_eq!(arr(&out)[0]["created"], false, "dry run: {out}");
    t.fails(&alpha, a!["search", "dry", "--open"]);
}

#[test]
fn long_titles_and_outcomes_need_force() {
    let (t, alpha) = alpha_only();
    let long = "x".repeat(121);
    let longer = "y".repeat(301);
    must(&t.run(&alpha, a!["create", long]).all(), r"title is 121 characters \(limit 120\).*note", "long title accepted or wrong hint");
    let gid = t.create(&alpha, a![long, "--force"]);
    t.fails(&alpha, a!["retitle", gid, long]);
    t.fails(&alpha, a!["edit", gid, "--title", long]);
    t.ok(&alpha, a!["edit", gid, "--title", long, "--force"]);
    must(
        &t.run(&alpha, a!["status", gid, "done", "--outcome", longer]).all(),
        r"outcome is 301 characters \(limit 300\).*note",
        "long outcome accepted",
    );
    must(&t.ok(&alpha, a!["show", gid]), "  in_progress|  new ", "refused status still moved the card");
    has(&t.ok(&alpha, a!["status", gid, "done", "--outcome", longer, "--force"]), "done", "status --force");
    t.fails(&alpha, a!["edit", gid, "--outcome", longer]);
    assert_ne!(t.run_stdin(&alpha, a!["import", "--stdin"], &format!("{{\"title\":\"{long}\"}}")).code, 0, "stdin long title accepted");
    has(&t.ok(&alpha, a!["prompt"]), "120 characters", "prompt does not state the limit");
}

#[test]
fn note_body_from_stdin_or_a_file_arrives_untouched() {
    let (t, alpha) = alpha_only();
    let sid = t.create(&alpha, a!["stdin notes"]);
    let body = "it's the \"$PATH\" glob (*) case\nsecond line";
    has(&t.ok_stdin(&alpha, a!["note", sid, "--stdin", "-k", "finding", "--by", "piper"], body), "note added", "note --stdin");
    let notes = t.ok(&alpha, a!["notes", sid]);
    has(&notes, "it's the \"$PATH\" glob (*) case", "stdin body mangled");
    must(&notes, "(?m)^    second line", "stdin second line lost");
    let file = t.p("note.txt");
    std::fs::write(&file, "from a file\n").unwrap();
    has(&t.ok(&alpha, a!["note", sid, "-f", file.display()]), "note added", "note -f");
    has(&t.ok(&alpha, a!["notes", sid]), "from a file", "file body missing");
    assert_ne!(t.run_stdin(&alpha, a!["note", sid, "--stdin"], "").code, 0, "empty stdin note accepted");
    t.fails(&alpha, a!["note", sid, "text", "--stdin"]);
}

#[test]
fn tags_filter_everywhere_and_legacy_labels_split() {
    let (t, alpha) = alpha_only();
    let t1 = t.create(&alpha, a!["tagged one", "-l", "art,enemies", "-p", 2100]);
    let t2 = t.create(&alpha, a!["tagged two", "-l", "ui enemies", "-p", 2000]);
    has(&t.ok(&alpha, a!["show", t1]), "[art,enemies]", "tags not normalized");
    has(&t.ok(&alpha, a!["show", t2]), "[ui,enemies]", "space input not normalized");
    assert_eq!(titled(&t.json(&alpha, a!["list", "-l", "enemies", "--json"]), "tagged"), 2, "list -l tag any-match");
    assert_eq!(titled(&t.json(&alpha, a!["list", "-l", "art", "--json"]), "tagged"), 1, "list -l art");
    assert_eq!(titled(&t.json(&alpha, a!["list", "-l", "art,ui", "--json"]), "tagged"), 2, "list -l art,ui");
    has(&t.ok(&alpha, a!["next", "-l", "enemies"]), "tagged one", "next -l tag");
    has(&t.ok(&alpha, a!["search", "tagged", "-l", "ui"]), "tagged two", "search -l tag");
    let board = t.ok(&alpha, a!["board", "--no-color", "-l", "art"]);
    has(&board, "tagged one", "board -l tag");
    lacks(&board, "tagged two", "board -l art matched a card without it");
    has(&t.ok(&alpha, a!["edit", t1, "--add-tag", "build", "--rm-tag", "art"]), "label", "edit add/rm tag");
    has(&t.ok(&alpha, a!["show", t1]), "[enemies,build]", "add/rm result");
    has(&t.ok(&alpha, a!["edit", "--where", "label=enemies", "--set", "priority=2200"]), "edited 2 card(s)", "--where label= is tag match");
    must(&t.ok(&alpha, a!["history", t1]), "label .*art,enemies → enemies,build", "tag event");
    let db = legacy_db(&t);
    has(&t.ok(&t.root, a!["--db", db, "list", "-l", "enemies"]), "[art,enemies,c676] space labelled", "legacy space label not split");
    has(&t.ok(&alpha, a!["prompt"]), "Tags in use", "prompt tag section");
}

#[test]
fn links_block_next_until_the_blocker_is_done() {
    let (t, alpha) = alpha_only();
    let l1 = t.create(&alpha, a!["dep first", "-p", 9800]);
    let l2 = t.create(&alpha, a!["dep second", "-p", 9900]);
    let ep = t.create(&alpha, a!["dep epic", "-p", 100]);
    has(&t.ok(&alpha, a!["next"]), "dep second", "precondition: second is top");
    has(&t.ok(&alpha, a!["block", l2, "--on", l1, "--by", "planner"]), &format!("linked: #{l1} blocks #{l2}"), "block --on");
    has(&t.ok(&alpha, a!["next"]), "dep first", "next did not skip the blocked card");
    has(&t.ok(&alpha, a!["show", l2]), &format!("blocked by: #{l1} (new) dep first"), "show lacks blocked by");
    has(&t.ok(&alpha, a!["list"]), &format!("blocks: #{l2} dep second"), "list lacks blocks line");
    let shown = t.json(&alpha, a!["show", l2, "--json"]);
    assert!(arr(&shown["links"]).iter().any(|l| l["rel"] == "blocked_by"), "json links: {shown}");
    t.fails(&alpha, a!["link", l2, "--blocks", l1]);
    t.ok(&alpha, a!["link", l1, "--child-of", ep, "--related", l2]);
    has(&t.ok(&alpha, a!["show", ep]), &format!("children: #{l1}"), "epic children");
    has(&t.ok(&alpha, a!["show", l2]), &format!("related: #{l1}"), "related is symmetric");
    has(&t.ok(&alpha, a!["link", l1, "--child-of", ep]), "already linked", "duplicate link");
    has(&t.ok(&alpha, a!["history", l2]), &format!("→ #{l1} blocks #{l2}  by planner"), "link event");
    must(&t.ok(&alpha, a!["history", l2]), &format!("link .*→ #{l1} blocks #{l2}  by planner"), "link event kind");
    assert_eq!(id(&t.ok(&alpha, a!["next", "--claim", "--by", "dep"])), l1, "next --claim did not pick the blocker");
    t.ok(&alpha, a!["status", l1, "done", "--outcome", "done"]);
    has(&t.ok(&alpha, a!["next"]), "dep second", "done blocker still blocks");
    let page = t.p("links.html");
    t.ok(&alpha, a!["export", "-o", page.display()]);
    has(&read(&page), r#""links":["#, "page carries no links");
    has(&t.ok(&alpha, a!["unlink", l1, "--related", l2]), "unlinked", "unlink");
    lacks(&t.ok(&alpha, a!["show", l2]), "related:", "related survived unlink");
    t.ok(&alpha, a!["delete", l1, "--why", "test"]);
    lacks(&t.ok(&alpha, a!["show", l2]), "blocked by", "link survived delete of the blocker");
}

#[test]
fn blocked_status_parks_a_card_with_its_reason() {
    let (t, alpha) = alpha_only();
    let bk = t.create(&alpha, a!["needs jonathan", "-p", 9950]);
    has(&t.ok(&alpha, a!["next"]), "needs jonathan", "precondition: blocked candidate is top");
    t.fails(&alpha, a!["status", bk, "blocked"]);
    t.ok(&alpha, a!["claim", bk, "--by", "parker"]);
    has(&t.ok(&alpha, a!["status", bk, "blocked", "--on", "jonathan", "--by", "parker"]), "blocked", "status blocked");
    lacks(&t.ok(&alpha, a!["next"]), "needs jonathan", "next picked a blocked card");
    has(&t.ok(&alpha, a!["list"]), "blocked on: jonathan", "list lacks the reason");
    let page = t.p("blocked.html");
    t.ok(&alpha, a!["export", "-o", page.display()]);
    has(&read(&page), r#""blocked_on":"jonathan""#, "page lacks the reason");
    let shown = t.ok(&alpha, a!["show", bk]);
    has(&shown, "  blocked       ", "status column");
    lacks(&shown, "claimed_by=", "claim survived blocking");
    must_not(&t.run(&alpha, a!["reap", "--older-than", "0s"]).all(), &format!(r"#{bk}\b"), "reap touched a blocked card");
    has(&t.ok(&alpha, a!["board", "--no-color", "--width", 120]), "BLOCKED", "terminal board lacks the blocked column");
    must(&t.ok(&alpha, a!["history", bk]), "blocked_on .*→ jonathan  by parker", "blocked_on event");
    t.ok(&alpha, a!["status", bk, "ready"]);
    lacks(&t.ok(&alpha, a!["show", bk]), "blocked on:", "ready did not clear the reason");
    lacks(&t.ok(&alpha, a!["board", "--no-color", "--width", 120]), "BLOCKED", "blocked column shown with nothing blocked");
    let other = t.create(&alpha, a!["the decision card", "-p", 10]);
    t.ok(&alpha, a!["status", bk, "blocked", "--on", format!("#{other}")]);
    has(&t.ok(&alpha, a!["show", bk]), &format!("blocked by: #{other}"), "--on #card did not link");
}

#[test]
fn a_v04_database_is_rebuilt_with_its_notes_intact() {
    let t = T::new();
    let p = t.p("v04/backlog.db");
    sqlite(&p, V04);
    let db = p.display().to_string();
    has(&t.ok(&t.root, a!["--db", db, "list"]), "kept card", "0.4 db unreadable");
    let notes = t.ok(&t.root, a!["--db", db, "notes", 1]);
    assert_eq!(notes.matches("must survive").count(), 1, "rebuild lost or duplicated the note row:\n{notes}");
    t.ok(&t.root, a!["--db", db, "status", 1, "blocked", "--on", "someone"]);
    has(&t.ok(&t.root, a!["--db", db, "notes", 1]), "must survive", "note gone after blocked");
}

#[test]
fn version_reports_the_schema_without_migrating() {
    let (t, alpha) = alpha_only();
    let v = t.ok(&alpha, a!["--version"]);
    must(&v, r"^bl [0-9][0-9.]*  \(schema [0-9]*\)", "--version header");
    has(&v, "schema 5  (current)", "store not stamped current");
    let p = t.p("v04b/backlog.db");
    sqlite(
        &p,
        "CREATE TABLE cards (id INTEGER PRIMARY KEY, title TEXT NOT NULL, notes TEXT NOT NULL DEFAULT '', label TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'new' CHECK(status IN ('new','ready','in_progress','done')), priority INTEGER NOT NULL DEFAULT 5000, outcome TEXT NOT NULL DEFAULT '', claimed_by TEXT NOT NULL DEFAULT '', claimed_at TEXT NOT NULL DEFAULT '', commits TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL DEFAULT (datetime('now')), updated_at TEXT NOT NULL DEFAULT (datetime('now')));",
    );
    let db = p.display().to_string();
    has(&t.ok(&alpha, a!["--db", db, "--version"]), "schema ?  (unstamped", "old db not reported unstamped");
    let meta: i64 = rusqlite::Connection::open(&p)
        .unwrap()
        .query_row("SELECT count(*) FROM sqlite_master WHERE name = 'meta'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(meta, 0, "--version migrated the database");
    t.run(&alpha, a!["--db", db, "list"]);
    has(&t.ok(&alpha, a!["--db", db, "--version"]), "schema 5  (current)", "db not stamped after a command");
    let nowhere = t.p("nowhere");
    std::fs::create_dir_all(&nowhere).unwrap();
    let mut c = t.cmd(&nowhere, a!["--version"]);
    c.env("BL_HOME", t.p("empty-home"));
    has(&T::exec(c).stdout, "database: none found", "no-db case");
}

#[test]
fn n_is_the_limit_and_a_numeric_tag_warns() {
    let (t, alpha) = alpha_only();
    t.ok(&alpha, a!["create", "alpha two"]);
    t.ok(&alpha, a!["create", "alpha three"]);
    let help = t.ok(&alpha, a!["list", "--help"]);
    has(&help, "-n, --limit <N>", "list --help lacks -n/--limit");
    has(&help, "Not a count: that is -n", "list --help does not warn about -l");
    has(&t.ok(&alpha, a!["prompt"]), "-n`/`--limit` is the count", "prompt does not name -n");
    let capped = t.ok(&alpha, a!["list", "-n", 2]);
    assert_eq!(capped.lines().filter(|l| l.starts_with('#')).count(), 2, "-n 2 did not cap:\n{capped}");
    has(&t.run(&alpha, a!["list", "-l", 5]).stderr, "no card carries the tag '5'; to cap the count use -n 5", "numeric -l warning");
    lacks(&t.run(&alpha, a!["list", "-l", "art"]).stderr, "to cap the count", "warned for a word tag");
    has(&t.run(&alpha, a!["next", "-l", 7]).stderr, "use -n 7", "next lacks the warning");
}

#[test]
fn prompt_carries_the_current_rules() {
    let (t, alpha) = alpha_only();
    t.ok(&alpha, a!["create", "tagged", "-l", "art"]);
    let pr = t.ok(&alpha, a!["prompt"]);
    has(&pr, "This repository is project **alpha**", "prompt lacks the project name");
    for want in ["blocked --on", "bl delete", "--stdin", "120 characters", "bl history", "bl block", "--add-tag", "bl note edit", "bl edit --where"] {
        has(&pr, want, "prompt lacks a rule");
    }
    lacks(&pr, "BL_AGENT", "prompt describes BL_AGENT, which does not exist yet");
    has(&pr, "Tags in use", "prompt lacks the tag list");
    let readme = read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"));
    has(&readme, "bl status 14 blocked --on jonathan", "README lacks the blocked example");
}

#[test]
fn open_prints_the_board_url_and_registers_auto_export() {
    let (t, alpha) = alpha_only();
    let own = alpha.join("view/index.html");
    t.ok(&alpha, a!["auto", "on", "-o", own.display()]);
    let delta = t.repo("delta");
    t.ok(&delta, a!["project", "add"]);
    let did = t.create(&delta, a!["delta one", "-p", 100]);
    let page = t.home().join("view/delta/index.html");
    let url = format!("file://{}", page.display());
    let o = t.run(&delta, a!["open", "--print"]);
    assert_eq!(o.stdout.trim(), url, "open url for delta");
    must(&o.stderr, "auto-export on.*delta", "open did not register auto-export for delta");
    let html = read(&page);
    has(&html, r#""name":"delta""#, "delta page not scoped");
    has(&html, r#"id="q""#, "page has no search box");
    assert_eq!(t.run(&delta, a!["open", "--print"]).stderr, "", "second open registered again");
    assert_eq!(t.ok(&alpha, a!["open", did, "--print"]).trim(), format!("{url}#card-{did}"), "open <card> did not land on delta");
    assert_eq!(t.ok(&alpha, a!["open", "delta", "--print"]).trim(), url, "open <project>");
    assert_eq!(t.ok(&alpha, a!["open", "--print"]).trim(), format!("file://{}", own.display()), "open keeps alpha's own snapshot path");
    let store = format!("file://{}", t.home().join("view/index.html").display());
    assert_eq!(t.ok(&t.root, a!["open", "--print"]).trim(), store, "open outside a project is the store page");
    t.fails(&t.root, a!["open", "nosuch", "--print"]);
}

#[test]
fn project_hide_keeps_a_project_off_the_store_page() {
    let (t, _) = alpha_only();
    let delta = t.repo("delta");
    t.ok(&delta, a!["project", "add"]);
    t.ok(&delta, a!["create", "delta one", "-p", 100]);
    let root = &t.root;
    t.fails(root, a!["project", "hide", "delta", "nosuch"]);
    lacks(&t.ok(root, a!["project", "list"]), "hidden from the store-wide board", "a failed hide changed the setting");
    has(&t.ok(root, a!["project", "hide", "delta"]), "'delta' hidden from", "hide delta");
    has(&t.ok(root, a!["project", "hide", "delta"]), "already", "second hide should say already");
    let list = t.ok(root, a!["project", "list"]);
    let lines: Vec<&str> = list.lines().collect();
    let at = lines.iter().position(|l| l.contains(" delta ")).expect("no delta row");
    assert!(lines[at..(at + 2).min(lines.len())].iter().any(|l| l.contains("hidden from the store-wide board")), "project list does not mark delta:\n{list}");
    let projects = t.json(root, a!["project", "list", "--json"]);
    let d = arr(&projects).iter().find(|p| p["name"] == "delta").expect("no delta in json");
    assert_eq!(d["hidden_on_board"], true, "json lacks hidden_on_board");
    let all3 = t.p("all3.html");
    t.ok(root, a!["export", "-o", all3.display()]);
    let html = read(&all3);
    has(&html, r#""hidden_projects":["delta"]"#, "store page does not name delta as hidden");
    lacks(&html, r#""title":"delta one""#, "store page still carries delta's cards");
    has(&html, r#""project":"alpha""#, "store page lost alpha");
    has(&html, r#"id="projectsmenu""#, "page has no projects menu");
    has(&t.ok(root, a!["list"]), "delta one", "hide leaked into bl list");
    let delta3 = t.p("delta3.html");
    t.ok(&delta, a!["export", "-o", delta3.display()]);
    has(&read(&delta3), r#""title":"delta one""#, "delta's own page lost its cards");
    has(&t.ok(root, a!["project", "unhide", "delta"]), "shown on", "unhide delta");
    let all4 = t.p("all4.html");
    t.ok(root, a!["export", "-o", all4.display()]);
    let html = read(&all4);
    has(&html, r#""hidden_projects":[]"#, "unhide left delta named as hidden");
    has(&html, r#""title":"delta one""#, "unhide did not bring delta back");
}
