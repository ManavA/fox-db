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
fn subagent_stop_saves_the_report_before_blocking() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", LIB)]);
    edit(repo.path(), "src/lib.rs", "compute(a: i32)", "compute(a: i32, b: i32)");
    let on = config::parse("[hooks]\nstop_check = true\n");
    let input = |active: bool, message: &str| {
        json!({"hook_event_name": "SubagentStop", "session_id": "s1", "agent_id": "a1", "agent_type": "general-purpose",
               "cwd": repo.path().to_string_lossy(), "stop_hook_active": active, "last_assistant_message": message})
        .to_string()
    };
    let reports =
        || -> i64 { db::open_ro().unwrap().query_row("SELECT COUNT(*) FROM handoff WHERE kind = 'report'", [], |r| r.get(0)).unwrap() };
    let out = hook::run_with(&on, "subagent-stop", &input(false, "first attempt"), hook::Harness::Claude);
    assert!(out.contains("\"decision\":\"block\""), "{out}");
    assert_eq!(reports(), 1);
    // The retry lets the agent stop and keeps the saved report.
    assert_eq!(hook::run_with(&on, "subagent-stop", &input(true, "all callers fixed"), hook::Harness::Claude), "");
    assert_eq!(reports(), 1);
    let report: String = db::open_ro().unwrap().query_row("SELECT body FROM handoff WHERE kind = 'report'", [], |r| r.get(0)).unwrap();
    assert_eq!(report, "all callers fixed");
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

#[test]
fn editing_an_inline_body_does_not_block() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", "pub fn compute(a: i32) -> i32 { a * 2 }\npub fn caller() -> i32 { compute(1) }\n")]);
    edit(repo.path(), "src/lib.rs", "a * 2", "a * 4");
    let report = run(repo.path());
    assert!(!report.blocks(), "{}", check::render(&report));
}

#[test]
fn renaming_a_definition_reports_its_remaining_caller() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", LIB)]);
    edit(repo.path(), "src/lib.rs", "fn compute", "fn renamed");
    let report = run(repo.path());
    assert!(report.blocks(), "{}", check::render(&report));
    let repeat = run(repo.path());
    assert_eq!(sites(&repeat.missed), sites(&report.missed));
}

#[test]
fn a_stale_index_cannot_block_on_removed_calls() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[
        ("src/lib.rs", "pub fn compute(a: i32) -> i32 { a * 2 }\n"),
        ("src/caller.rs", "pub fn caller() -> i32 { crate::compute(1) }\n"),
    ]);
    edit(repo.path(), "src/lib.rs", "a: i32", "a: i64");
    fs::remove_file(repo.path().join("src/caller.rs")).unwrap();
    let mut c = db::open_rw().unwrap();
    let p = project::find(&c, &project::locate(repo.path())).unwrap().unwrap();
    let report = check::run(&mut c, &p, repo.path(), false).unwrap();
    assert!(!report.blocks(), "{}", check::render(&report));
}

#[test]
fn detects_return_type_change_after_multiline_parameters() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) =
        setup(&[("src/lib.rs", "pub fn compute(\n    a: i32,\n)\n-> i32\n{ a * 2 }\npub fn caller() -> i32 { compute(1) }\n")]);
    edit(repo.path(), "src/lib.rs", "-> i32", "-> i64");
    let report = run(repo.path());
    assert!(report.blocks(), "{}", check::render(&report));
}

#[test]
fn precise_index_callers_are_certain() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", LIB)]);
    edit(repo.path(), "src/lib.rs", "compute(a: i32)", "compute(a: i32, b: i32)");
    let mut c = db::open_rw().unwrap();
    let p = project::find(&c, &project::locate(repo.path())).unwrap().unwrap();
    c.execute("UPDATE ts_ref SET amb = -1 WHERE name = 'compute' AND kind = 'call'", []).unwrap();
    let report = check::run(&mut c, &p, repo.path(), false).unwrap();
    assert_eq!(report.missed.len(), 3, "{}", check::render(&report));
}

#[test]
fn active_stop_does_not_repeat_an_unregistered_agents_saved_report() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", LIB)]);
    let on = config::parse("[hooks]\nstop_check = true\n");
    for active in [false, true] {
        let input = json!({"hook_event_name": "SubagentStop", "session_id": "s1", "agent_id": "a1",
            "cwd": repo.path().to_string_lossy(), "stop_hook_active": active,
            "last_assistant_message": "all callers fixed"});
        assert_eq!(hook::run_with(&on, "subagent-stop", &input.to_string(), hook::Harness::Claude), "");
    }
    let reports: i64 = db::open_ro().unwrap().query_row("SELECT COUNT(*) FROM handoff WHERE kind = 'report'", [], |r| r.get(0)).unwrap();
    assert_eq!(reports, 1);
}

#[test]
fn deleting_a_definition_reports_its_remaining_callers() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", LIB)]);
    edit(repo.path(), "src/lib.rs", "pub fn compute(a: i32) -> i32 {\n    a * 2\n}\n\n", "");
    let report = run(repo.path());
    assert!(report.blocks(), "{}", check::render(&report));
    let repeat = run(repo.path());
    assert_eq!(sites(&repeat.missed), sites(&report.missed));
}

#[test]
fn a_reference_that_does_not_call_is_not_a_missed_call() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("api.c", "void compute(int old_name) {}\nvoid (*callback)(int) = compute;\n")]);
    edit(repo.path(), "api.c", "compute(int old_name)", "compute(int new_name)");
    let report = run(repo.path());
    assert!(report.missed.is_empty(), "{}", check::render(&report));
}

#[test]
fn changing_a_top_level_call_line_marks_it_touched() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("api.py", "def compute(arg):\n    return arg\n\ncompute(1)\n")]);
    edit(repo.path(), "api.py", "compute(arg)", "compute(arg, extra)");
    edit(repo.path(), "api.py", "compute(1)", "compute(1, 0)");
    let report = run(repo.path());
    assert_eq!(report.certain, 1, "{}", check::render(&report));
    assert!(!report.blocks(), "{}", check::render(&report));
}

#[test]
fn hunks_use_literal_paths_for_tracked_and_untracked_files() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/café.rs", LIB)]);
    git(repo.path(), &["config", "core.quotePath", "true"]);
    edit(repo.path(), "src/café.rs", "a * 2", "a * 4");
    fs::write(repo.path().join("src/niño.rs"), "pub fn new_fn() {}").unwrap();
    let hunks = check::hunks(repo.path()).unwrap();
    assert!(hunks.contains_key("src/café.rs") && hunks.contains_key("src/niño.rs"), "{hunks:?}");
}

#[test]
fn a_stale_index_cannot_block_on_replaced_calls() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", LIB), ("src/use.rs", "pub fn caller() -> i32 { crate::compute(9) }\n")]);
    edit(repo.path(), "src/use.rs", "crate::compute(9)", "9");
    edit(repo.path(), "src/lib.rs", "a: i32", "a: i64");
    let mut c = db::open_rw().unwrap();
    let p = project::find(&c, &project::locate(repo.path())).unwrap().unwrap();
    let report = check::run(&mut c, &p, repo.path(), false).unwrap();
    assert_eq!(report.certain, 3, "{}", check::render(&report));
}

#[test]
fn replacing_a_removed_functions_import_does_not_block_its_old_caller() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[
        ("src/old.rs", "pub fn compute(a: i32) -> i32 { a }\n"),
        ("src/new.rs", "pub fn compute(a: i32) -> i32 { a }\n"),
        ("src/lib.rs", "use crate::old::compute;\npub fn caller() -> i32 { compute(1) }\n"),
    ]);
    fs::remove_file(repo.path().join("src/old.rs")).unwrap();
    edit(repo.path(), "src/lib.rs", "crate::old", "crate::new");
    let report = run(repo.path());
    assert!(!report.blocks(), "{}", check::render(&report));
}

#[cfg(unix)]
#[test]
fn subagent_stop_keeps_the_report_when_check_exceeds_its_deadline() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;

    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (home, repo) = setup(&[("src/lib.rs", LIB)]);
    fs::write(home.path().join("config.toml"), "[hooks]\nstop_check = true\n").unwrap();
    let git = home.path().join("git");
    fs::write(&git, "#!/bin/sh\nexec sleep 2\n").unwrap();
    fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
    let path =
        std::env::join_paths(std::iter::once(home.path().to_path_buf()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())))
            .unwrap();
    let input = json!({"hook_event_name": "SubagentStop", "session_id": "s1", "agent_id": "a1",
        "cwd": repo.path(), "last_assistant_message": "still saved"});
    let mut child = Command::new(env!("CARGO_BIN_EXE_fox"))
        .args(["hook", "subagent-stop", "--harness", "claude"])
        .env("HOME", home.path())
        .env("FOXDB_HOME", home.path())
        .env("FOXDB_BRIEF_DEADLINE_MS", "500")
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
    let report: String = db::open_ro().unwrap().query_row("SELECT body FROM handoff WHERE kind = 'report'", [], |r| r.get(0)).unwrap();
    assert_eq!(report, "still saved");
}

#[test]
fn detects_deletion_of_a_return_type_on_its_own_line() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", "pub fn compute()\n-> ()\n{}\npub fn caller() { compute(); }\n")]);
    edit(repo.path(), "src/lib.rs", "-> ()\n", "");
    let report = run(repo.path());
    assert_eq!(sites(&report.missed), ["src/lib.rs:3"], "{}", check::render(&report));
}

#[test]
fn maps_a_removed_functions_caller_before_an_insertion() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("src/lib.rs", "pub fn compute() {}\npub fn caller() { compute(); }\n")]);
    edit(repo.path(), "src/lib.rs", "pub fn compute() {}", "\n\npub fn renamed() {}");
    edit(repo.path(), "src/lib.rs", "compute(); }\n", "compute(); }\n\npub fn added() {}\n");
    let report = run(repo.path());
    assert_eq!(sites(&report.missed), ["src/lib.rs:4"], "{}", check::render(&report));
}

#[test]
fn spacing_around_python_parameters_does_not_change_the_signature() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("api.py", "def leaf(x):\n    return x\n\nleaf(1)\n")]);
    edit(repo.path(), "api.py", "leaf(x)", "leaf( x )");
    let report = run(repo.path());
    assert!(report.changed.is_empty(), "{}", check::render(&report));
    edit(repo.path(), "api.py", "leaf( x )", "leaf(\n    x  # Parameter comment.\n)");
    let report = run(repo.path());
    assert!(report.changed.is_empty(), "{}", check::render(&report));
}

#[test]
fn spacing_inside_a_python_default_still_changes_the_signature() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("api.py", "def leaf(x='a b'):\n    return x\n\nleaf()\n")]);
    edit(repo.path(), "api.py", "'a b'", "'ab'");
    let report = run(repo.path());
    assert_eq!(sites(&report.missed), ["api.py:4"], "{}", check::render(&report));
}

#[test]
fn a_diff_attribute_cannot_hide_a_caller_update() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[
        (".gitattributes", "*.py -diff\n"),
        ("api.py", "def leaf(x):\n    return x\n"),
        ("caller.py", "from api import leaf\n\nleaf(1)\n"),
    ]);
    edit(repo.path(), "api.py", "leaf(x)", "leaf(x, y)");
    edit(repo.path(), "caller.py", "leaf(1)", "leaf(1, 0)");
    let report = run(repo.path());
    assert_eq!(report.certain, 1, "{}", check::render(&report));
    assert!(!report.blocks(), "{}", check::render(&report));
}

#[test]
fn an_external_import_rebind_does_not_leave_a_certain_caller_of_the_removed_definition() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("api.py", "def leaf(x):\n    return x\n"), ("caller.py", "from api import leaf\n\nleaf(1)\n")]);
    fs::remove_file(repo.path().join("api.py")).unwrap();
    edit(repo.path(), "caller.py", "from api", "from external_package");
    let report = run(repo.path());
    assert_eq!(report.certain, 0, "{}", check::render(&report));
}

#[test]
fn a_cached_check_reports_untouched_callers_of_a_removed_definition() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup(&[("api.py", "def leaf(x):\n    return x\n"), ("caller.py", "from api import leaf\n\nleaf(1)\n")]);
    fs::remove_file(repo.path().join("api.py")).unwrap();
    let mut c = db::open_rw().unwrap();
    let p = project::find(&c, &project::locate(repo.path())).unwrap().unwrap();
    let cached = check::run(&mut c, &p, repo.path(), false).unwrap();
    let fresh = check::run(&mut c, &p, repo.path(), true).unwrap();
    assert_eq!(sites(&fresh.missed), ["caller.py:3"], "{}", check::render(&fresh));
    assert_eq!(sites(&cached.missed), sites(&fresh.missed), "{}", check::render(&cached));
}
