//! Token sparing end to end: tiered find rows under a tight budget, "already sent" references
//! for repeated reads in a CLI session, and none in the MCP server.

use foxdb::{cli, db, inventory, mcp, project, query, tsindex};
use serde_json::json;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir, rusqlite::Connection, project::Project) {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    let mut body = String::from("pub fn long_reader() -> u64 {\n    let mut total = 0;\n");
    for i in 0..40 {
        body.push_str(&format!("    total += compute_value_number_{i}(total);\n"));
    }
    body.push_str("    total\n}\n");
    for i in 0..30 {
        body.push_str(&format!("pub fn reader_helper_{i}(input_value: u64) -> u64 {{ input_value + {i} }}\n"));
    }
    write(repo.path(), "src/reader.rs", &body);
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    let p = project::find(&c, &project::locate(repo.path())).unwrap().unwrap();
    (home, repo, c, p)
}

#[test]
fn tight_budget_keeps_the_top_rows_full_and_pages_the_rest() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, c, p) = setup();
    let o = |budget, offset| query::FindOpts { kind: "symbol", all: false, budget, limit: 20, offset };
    let full = query::find(&c, Some(&p), "reader_helper", &o(4000, 0)).unwrap();
    let tight = query::find(&c, Some(&p), "reader_helper", &o(150, 0)).unwrap();
    let rows = |s: &str| s.lines().filter(|l| l.contains("src/reader.rs:")).map(str::to_string).collect::<Vec<_>>();
    assert!(tight.len() <= 150 * 4, "{}", tight.len());
    assert!(tight.len() < full.len(), "a tight budget must cost less:\n{tight}");
    // The top rows are the same rows, in the same full form, as on the unconstrained page.
    assert_eq!(rows(&tight), rows(&full)[..5].to_vec(), "{tight}");
    assert!(tight.contains("cursor=5"), "{tight}");
    let next = query::find(&c, Some(&p), "reader_helper", &o(150, 5)).unwrap();
    assert_eq!(rows(&next)[0], rows(&full)[5], "{next}");
}

#[test]
fn mcp_read_twice_resends_the_full_text() {
    // One `fox mcp` process serves a Claude Code session and every subagent it starts (calls
    // carry no agent identity), and outlives compactions. A per-process "already sent" set
    // therefore pointed subagents and compacted sessions at text they never had: a repeat read
    // returns the body again.
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, _c, _p) = setup();
    std::env::set_current_dir(repo.path()).unwrap();
    let call = |args: serde_json::Value| -> String {
        let r = mcp::handle(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fox_read","arguments":args}})).unwrap();
        r["result"]["content"][0]["text"].as_str().unwrap().to_string()
    };
    let first = call(json!({"target": "long_reader", "mode": "body", "budget": 2000}));
    assert!(first.contains("compute_value_number_39"), "{first}");
    let second = call(json!({"target": "long_reader", "mode": "body", "budget": 2000}));
    assert_eq!(first, second, "a repeat read is the full text, not a reference");
    assert!(!mcp::tools().to_string().contains("already sent"), "the tool list still promises references");
}

#[test]
fn pre_compact_forgets_what_the_cli_session_was_sent() {
    // The CLI's set is keyed by a dispatched worker's FOXDB_SESSION. Its compaction drops the
    // text, so the PreCompact hook clears that session's set.
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, _c, _p) = setup();
    let args =
        || cli::parse(vec!["code".into(), "long_reader".into(), "--rel".into(), "src".into(), "--budget".into(), "2000".into()]).unwrap();
    unsafe { std::env::set_var("FOXDB_SESSION", "sess-compact") };
    let a = args();
    let x = cli::code(&a, repo.path(), None).unwrap();
    let y = cli::code(&a, repo.path(), None).unwrap();
    let input = json!({"hook_event_name": "PreCompact", "cwd": repo.path(), "session_id": "s1"}).to_string();
    foxdb::hook::run("pre-compact", &input, foxdb::hook::Harness::Claude);
    let z = cli::code(&a, repo.path(), None).unwrap();
    unsafe { std::env::remove_var("FOXDB_SESSION") };
    assert!(x.contains("compute_value_number_39"));
    assert!(y.starts_with("= already sent"), "{y}");
    assert!(z.contains("compute_value_number_39"), "after compaction the full text again: {z}");
}

#[test]
fn cli_session_dedup_is_opt_in_by_session_id() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, _c, _p) = setup();
    // The session is the caller's: `parse` takes it from FOXDB_SESSION.
    let args =
        || cli::parse(vec!["code".into(), "long_reader".into(), "--rel".into(), "src".into(), "--budget".into(), "2000".into()]).unwrap();
    unsafe { std::env::remove_var("FOXDB_SESSION") };
    let a = args();
    let x = cli::code(&a, repo.path(), None).unwrap();
    let y = cli::code(&a, repo.path(), None).unwrap();
    assert!(x.contains("compute_value_number_39") && y.contains("compute_value_number_39"), "no session, no dedup");
    unsafe { std::env::set_var("FOXDB_SESSION", "sess-1") };
    let a = args();
    let x = cli::code(&a, repo.path(), None).unwrap();
    let y = cli::code(&a, repo.path(), None).unwrap();
    unsafe { std::env::set_var("FOXDB_SESSION", "sess-2") };
    let a = args();
    let z = cli::code(&a, repo.path(), None).unwrap();
    unsafe { std::env::remove_var("FOXDB_SESSION") };
    assert!(x.contains("compute_value_number_39"));
    assert!(y.starts_with("= already sent"), "{y}");
    assert!(z.contains("compute_value_number_39"), "another session never saw it");
}
