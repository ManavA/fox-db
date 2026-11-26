//! `fox check`, the edit-completeness gate: after a signature change, every certain caller the
//! diff leaves untouched is a missed site; guessed callers are listed apart and never block. The
//! opt-in Stop hook blocks once with the missed sites as its reason.

use foxdb::check;
use foxdb::config;
use foxdb::{db, hook, inventory, project, tsindex};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

const LIB: &str = "pub fn compute(a: i32) -> i32 {
    a * 2
}

pub fn first() -> i32 {
    compute(1)
}

pub fn second() -> i32 {
    compute(2)
}

pub fn third() -> i32 {
    compute(3)
}
";

/// Two same-name methods: a call through an untyped receiver is only a guess between them.
const SHAPES: &str = "pub struct Square;
pub struct Circle;

impl Square {
    pub fn area(&self, k: i32) -> i32 {
        k
    }
}

impl Circle {
    pub fn area(&self, k: i32) -> i32 {
        k * 3
    }
}

pub fn total<T: Shape>(s: &T) -> i32 {
    s.area(1)
}

pub trait Shape {}
";

fn git(repo: &Path, args: &[&str]) {
    let st = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(st.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&st.stderr));
}

/// A scratch store and a committed, indexed git repo holding `files`.
fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    for (rel, text) in files {
        let p = repo.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    (home, repo)
}

fn edit(repo: &Path, rel: &str, from: &str, to: &str) {
    let p = repo.join(rel);
    let t = fs::read_to_string(&p).unwrap();
    assert!(t.contains(from), "{rel} has no {from:?}");
    fs::write(p, t.replacen(from, to, 1)).unwrap();
}

fn run(repo: &Path) -> check::Report {
    let mut c = db::open_rw().unwrap();
    let p = project::find(&c, &project::locate(repo)).unwrap().unwrap();
    check::run(&mut c, &p, repo, true).unwrap()
}

fn sites(v: &[check::Site]) -> Vec<String> {
    v.iter().map(|s| format!("{}:{}", s.path, s.line)).collect()
}

#[test]
fn reports_the_one_certain_caller_the_diff_missed() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", LIB)]);
    edit(repo.path(), "src/lib.rs", "compute(a: i32)", "compute(a: i32, b: i32)");
    edit(repo.path(), "src/lib.rs", "compute(1)", "compute(1, 0)");
    edit(repo.path(), "src/lib.rs", "compute(2)", "compute(2, 0)");
    let r = run(repo.path());
    assert_eq!(sites(&r.missed), ["src/lib.rs:14"], "{}", check::render(&r));
    assert_eq!(r.certain, 3);
    assert!(r.blocks());
    let out = check::render(&r);
    assert!(out.contains("1 of 3 certain callers"), "{out}");
    assert!(out.contains("src/lib.rs:14 third"), "{out}");
}

#[test]
fn an_edit_that_updates_every_caller_passes() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", LIB)]);
    edit(repo.path(), "src/lib.rs", "compute(a: i32)", "compute(a: i32, b: i32)");
    for n in ["1", "2", "3"] {
        edit(repo.path(), "src/lib.rs", &format!("compute({n})"), &format!("compute({n}, 0)"));
    }
    let r = run(repo.path());
    assert!(r.missed.is_empty(), "{}", check::render(&r));
    assert!(!r.blocks());
    assert!(check::render(&r).starts_with("fox check: ok"), "{}", check::render(&r));
}

#[test]
fn a_body_only_edit_names_no_callers() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", LIB)]);
    edit(repo.path(), "src/lib.rs", "a * 2", "a * 4");
    let r = run(repo.path());
    assert!(r.changed.is_empty() && !r.blocks(), "{}", check::render(&r));
}

#[test]
fn a_guess_only_caller_is_listed_but_does_not_block() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/shapes.rs", SHAPES)]);
    edit(
        repo.path(),
        "src/shapes.rs",
        "pub fn area(&self, k: i32) -> i32 {\n        k\n",
        "pub fn area(&self, k: i64) -> i32 {\n        k as i32\n",
    );
    let r = run(repo.path());
    assert!(r.missed.is_empty(), "{}", check::render(&r));
    assert_eq!(sites(&r.guesses), ["src/shapes.rs:17"], "{}", check::render(&r));
    assert!(!r.blocks());
    let out = check::render(&r);
    assert!(out.contains("not blocking") && out.contains("guess"), "{out}");
}

#[test]
fn a_caller_the_diff_adds_is_seen_though_the_index_predates_it() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", LIB)]);
    // New lines above every caller shift them; the stale index would put `third` at line 14.
    edit(repo.path(), "src/lib.rs", "compute(a: i32)", "compute(a: i32, b: i32)");
    edit(repo.path(), "src/lib.rs", "pub fn first", "pub fn zeroth() -> i32 {\n    compute(0, 0)\n}\n\npub fn first");
    edit(repo.path(), "src/lib.rs", "compute(1)", "compute(1, 0)");
    edit(repo.path(), "src/lib.rs", "compute(2)", "compute(2, 0)");
    let r = run(repo.path());
    assert_eq!(r.certain, 4, "{}", check::render(&r));
    assert_eq!(sites(&r.missed), ["src/lib.rs:18"], "{}", check::render(&r));
}

fn stop_input(repo: &Path, active: bool) -> String {
    json!({"hook_event_name": "Stop", "session_id": "s1", "cwd": repo.to_string_lossy(), "stop_hook_active": active,
           "last_assistant_message": "done"})
    .to_string()
}

#[test]
fn stop_hook_blocks_once_with_the_missed_sites() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", LIB)]);
    edit(repo.path(), "src/lib.rs", "compute(a: i32)", "compute(a: i32, b: i32)");
    let on = config::parse("[hooks]\nstop_check = true\n");
    let out = hook::run_with(&on, "stop", &stop_input(repo.path(), false), hook::Harness::Claude);
    let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out:?}"));
    assert_eq!(v["decision"], "block");
    let reason = v["reason"].as_str().unwrap();
    for at in ["src/lib.rs:6", "src/lib.rs:10", "src/lib.rs:14"] {
        assert!(reason.contains(at), "{reason}");
    }
    // Claude Code is already continuing because of a stop hook: never block twice in a row.
    assert_eq!(hook::run_with(&on, "stop", &stop_input(repo.path(), true), hook::Harness::Claude), "");
    // Off by default.
    assert_eq!(hook::run_with(&config::Config::default(), "stop", &stop_input(repo.path(), false), hook::Harness::Claude), "");
}

#[test]
fn subagent_stop_blocks_before_saving_the_report() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", LIB)]);
    edit(repo.path(), "src/lib.rs", "compute(a: i32)", "compute(a: i32, b: i32)");
    let on = config::parse("[hooks]\nstop_check = true\n");
    let input = |active: bool| {
        json!({"hook_event_name": "SubagentStop", "session_id": "s1", "agent_id": "a1", "agent_type": "general-purpose",
               "cwd": repo.path().to_string_lossy(), "stop_hook_active": active, "last_assistant_message": "all callers fixed"})
        .to_string()
    };
    let reports =
        || -> i64 { db::open_ro().unwrap().query_row("SELECT COUNT(*) FROM handoff WHERE kind = 'report'", [], |r| r.get(0)).unwrap() };
    let out = hook::run_with(&on, "subagent-stop", &input(false), hook::Harness::Claude);
    assert!(out.contains("\"decision\":\"block\""), "{out}");
    assert_eq!(reports(), 0);
    // The second stop is not blocked, and the report the first one held back is saved now.
    assert_eq!(hook::run_with(&on, "subagent-stop", &input(true), hook::Harness::Claude), "");
    assert_eq!(reports(), 1);
}

#[test]
fn the_cli_exits_3_on_a_missed_caller_and_0_once_it_is_fixed() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (home, repo) = setup(&[("src/lib.rs", LIB)]);
    let fox = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_fox"))
            .arg("check")
            .args(args)
            .current_dir(repo.path())
            .env("HOME", home.path())
            .env("FOXDB_HOME", home.path())
            .env("FOXDB_NO_DAEMON", "1")
            .output()
            .unwrap()
    };
    edit(repo.path(), "src/lib.rs", "compute(a: i32)", "compute(a: i32, b: i32)");
    edit(repo.path(), "src/lib.rs", "compute(1)", "compute(1, 0)");
    edit(repo.path(), "src/lib.rs", "compute(2)", "compute(2, 0)");
    let out = fox(&[]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(3), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("src/lib.rs:14 third"), "{text}");
    edit(repo.path(), "src/lib.rs", "compute(3)", "compute(3, 0)");
    let out = fox(&["--fresh"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
}
