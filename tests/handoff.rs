//! Agent handoff: the board (post/read/pickup), SubagentStart briefs (identical for siblings,
//! skipped for goldfish reviewers), SubagentStop capture, and read-only transcript ingest.

use foxdb::{db, handoff, hook, inventory, mcp, project, sessions};
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
    write(repo.path(), "AGENT_MILESTONE_LOG.md", "# Log\n\n## Current gate\n\n- Gate: HX-1 is active.\n");
    write(repo.path(), "src/lib.rs", "pub fn alpha() {}\n");
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    let p = project::find(&c, &project::locate(repo.path())).unwrap().unwrap();
    (home, repo, c, p)
}

#[test]
fn board_post_read_and_pickup_once() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, c, p) = setup();
    let id = handoff::post(
        &c,
        &handoff::Post {
            project_id: p.id,
            kind: "request",
            text: "Review the cache key\nIt must include the generation.",
            ..Default::default()
        },
    )
    .unwrap();
    handoff::post(
        &c,
        &handoff::Post { project_id: p.id, kind: "finding", text: "token=abcdef1234567890SECRET leaked", ..Default::default() },
    )
    .unwrap();
    let list = handoff::read(&c, p.id, None, None, 500, 0).unwrap();
    assert!(list.contains("request") && list.contains("Review the cache key"), "{list}");
    assert!(!list.contains("SECRET"), "secrets must be redacted on the way in: {list}");
    assert!(handoff::pickup(&c, id, "worker-a").unwrap().starts_with("claimed"));
    let again = handoff::pickup(&c, id, "worker-b").unwrap();
    assert!(again.contains("already_taken by worker-a"), "{again}");
    let only = handoff::read(&c, p.id, Some("finding"), None, 500, 0).unwrap();
    assert!(!only.contains("Review the cache key"), "{only}");
}

#[test]
fn mcp_memory_ops_post_and_read() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, _c, _p) = setup();
    std::env::set_current_dir(repo.path()).unwrap();
    let call = |args: serde_json::Value| -> (String, bool) {
        let r =
            mcp::handle(&json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"fox_memory","arguments":args}})).unwrap();
        (r["result"]["content"][0]["text"].as_str().unwrap().to_string(), r["result"]["isError"].as_bool().unwrap())
    };
    let (t, err) = call(json!({"op": "post", "kind": "decision", "text": "Use RRF with k=60", "evidence": "src/search.rs:21"}));
    assert!(!err && t.starts_with("posted h:"), "{t}");
    let (t, err) = call(json!({"op": "read"}));
    assert!(!err && t.contains("decision") && t.contains("Use RRF with k=60"), "{t}");
    let h = t.split_whitespace().find(|w| w.starts_with("h:")).unwrap().to_string();
    let (t, err) = call(json!({"op": "pickup", "id": h}));
    assert!(!err && t.starts_with("claimed"), "{t}");
}

fn subagent_input(cwd: &Path, agent_type: &str, id: &str) -> String {
    json!({"hook_event_name": "SubagentStart", "session_id": "s1", "cwd": cwd, "agent_id": id, "agent_type": agent_type}).to_string()
}

#[test]
fn subagent_start_brief_is_identical_for_siblings_and_skips_goldfish() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (home, repo, c, p) = setup();
    handoff::post(
        &c,
        &handoff::Post { project_id: p.id, kind: "decision", text: "Keep SQLite as the system of record", ..Default::default() },
    )
    .unwrap();
    let a = hook::run("subagent-start", &subagent_input(repo.path(), "hana-impl", "a1"), hook::Harness::Claude);
    let b = hook::run("subagent-start", &subagent_input(repo.path(), "hana-impl", "b2"), hook::Harness::Claude);
    assert!(!a.is_empty());
    assert_eq!(a, b, "sibling briefs must be byte-identical");
    let v: serde_json::Value = serde_json::from_str(&a).unwrap();
    let ctx = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SubagentStart");
    assert!(ctx.contains("Gate: HX-1 is active.") && ctx.contains("parent packet h:"), "{ctx}");
    // The brief points at the parent's board snapshot; the snapshot holds the decision.
    let h = ctx.split_whitespace().find(|w| w.starts_with("h:")).unwrap().trim_end_matches(':');
    assert!(handoff::show(&c, handoff::parse_handle(h).unwrap()).unwrap().contains("Keep SQLite"));
    assert!(ctx.len() <= 1_600, "subagent brief should stay ~1,500 chars: {}", ctx.len());
    // Goldfish reviewers start empty by default.
    for t in ["egm-reviewer", "code-goldfish", "spec-reviewer", "Reviewer"] {
        assert_eq!(hook::run("subagent-start", &subagent_input(repo.path(), t, "x"), hook::Harness::Claude), "", "{t} got a brief");
    }
    // Per-agent opt-out in the agent definition.
    write(repo.path(), ".claude/agents/quiet-helper.md", "---\nname: quiet-helper\nfoxdb: off\n---\nDo things.\n");
    assert_eq!(hook::run("subagent-start", &subagent_input(repo.path(), "quiet-helper", "q"), hook::Harness::Claude), "");
    // config.toml patterns replace the defaults.
    fs::write(home.path().join("config.toml"), "[hooks]\nskip_agents = [\"hana-*\"]\n").unwrap();
    assert!(hook::skip_agent_with(&foxdb::config::parse("[hooks]\nskip_agents = [\"hana-*\"]\n"), "hana-impl", "a1", Some(repo.path())));
    assert!(!hook::skip_agent_with(
        &foxdb::config::parse("[hooks]\nskip_agents = [\"hana-*\"]\n"),
        "egm-reviewer",
        "a1",
        Some(repo.path())
    ));
    assert!(hook::skip_agent_with(&foxdb::config::parse(""), "egm-reviewer", "a1", Some(repo.path())));
}

#[test]
fn subagent_stop_captures_the_final_report() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, c, p) = setup();
    let input = json!({"hook_event_name": "SubagentStop", "session_id": "s1", "cwd": repo.path(), "agent_id": "a1",
        "agent_type": "hana-impl", "last_assistant_message": "Done: fixed the cache key in src/search.rs:120. password=hunter2hunter2"})
    .to_string();
    assert_eq!(hook::run("subagent-stop", &input, hook::Harness::Claude), "", "SubagentStop must print nothing");
    let list = handoff::read(&c, p.id, Some("report"), None, 500, 0).unwrap();
    assert!(list.contains("hana-impl") && list.contains("Done: fixed the cache key"), "{list}");
    assert!(!list.contains("hunter2"), "{list}");
}

const CLAUDE_JSONL: &str = r#"{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>"},"sessionId":"c-1","cwd":"CWD","timestamp":"2026-11-01T10:00:00Z"}
{"type":"user","message":{"role":"user","content":"Find why the brief changes every session"},"sessionId":"c-1","cwd":"CWD","timestamp":"2026-11-01T10:00:01Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"CWD/src/query.rs"}}]},"sessionId":"c-1","timestamp":"2026-11-01T10:00:02Z"}
{"type":"api-request-blob","message":{"role":"user","content":"duplicate blob that must be ignored"},"sessionId":"c-1"}
not json at all
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"The relative age in src/query.rs:84 changes the brief. api_key=sk-abcdefghijklmnop1234"}]},"sessionId":"c-1","timestamp":"2026-11-01T10:05:00Z"}
"#;

const CODEX_JSONL: &str = r#"{"timestamp":"2026-11-02T09:00:00Z","type":"session_meta","payload":{"id":"x-9","cwd":"CWD","timestamp":"2026-11-02T09:00:00Z"}}
{"timestamp":"2026-11-02T09:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Add a trigram arm to search"}}
{"timestamp":"2026-11-02T09:00:02Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","input":"*** Begin Patch\n*** Update File: src/search.rs\n@@\n*** End Patch"}}
{"timestamp":"2026-11-02T09:00:03Z","type":"event_msg","payload":{"type":"agent_message","message":"Added the trigram arm in src/search.rs."}}
"#;

#[test]
fn transcripts_are_ingested_read_only_with_redaction_including_zst() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, mut c, p) = setup();
    let cwd = repo.path().to_string_lossy().to_string();
    let tx = tempfile::tempdir().unwrap();
    let claude = tx.path().join("claude");
    let codex = tx.path().join("codex");
    write(&claude, "-proj/c-1.jsonl", &CLAUDE_JSONL.replace("CWD", &cwd));
    let raw = CODEX_JSONL.replace("CWD", &cwd);
    let zst = ruzstd::encoding::compress_to_vec(raw.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest);
    fs::create_dir_all(codex.join("2026/11/02")).unwrap();
    fs::write(codex.join("2026/11/02/rollout-2026-11-02T09-00-00-x-9.jsonl.zst"), zst).unwrap();
    let before: Vec<_> = walk(tx.path());

    let st = sessions::ingest(&mut c, Some(&claude), Some(&codex)).unwrap();
    assert_eq!(st.sessions, 2, "{st:?}");
    assert_eq!(walk(tx.path()), before, "ingest must not write next to transcripts");
    // Second run: unchanged files are skipped.
    assert_eq!(sessions::ingest(&mut c, Some(&claude), Some(&codex)).unwrap().sessions, 0);

    let o = foxdb::query::FindOpts { kind: "recall", all: false, budget: 800, limit: 10, offset: 0 };
    let out = foxdb::query::find(&c, Some(&p), "brief changes every session", &o).unwrap();
    assert!(out.contains("session") && out.contains("Find why the brief changes"), "{out}");
    let out = foxdb::query::find(&c, Some(&p), "trigram arm", &o).unwrap();
    assert!(out.contains("Add a trigram arm"), "{out}");
    let d = sessions::digest_by_key(&c, "claude", "c-1").unwrap();
    assert!(d.contains("src/query.rs") && d.contains("src/query.rs:84"), "{d}");
    assert!(!d.contains("sk-abcdefghijklmnop1234") && !d.contains("duplicate blob") && !d.contains("/clear"), "{d}");
    let d = sessions::digest_by_key(&c, "codex", "x-9").unwrap();
    assert!(d.contains("src/search.rs") && d.contains("Added the trigram arm"), "{d}");
}

fn walk(dir: &Path) -> Vec<(String, u64)> {
    let mut v = vec![];
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            v.extend(walk(&p));
        } else {
            v.push((p.to_string_lossy().to_string(), fs::metadata(&p).unwrap().len()));
        }
    }
    v.sort();
    v
}

// ---- milestone 3 ----

/// A refused take says why, with a stable reason word: not_found, already_taken (with the
/// holder) or already_done (with the report). The holder taking again renews.
#[test]
fn refused_takes_say_why() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, c, p) = setup();
    let post = |kind: &str, text: &str, parent: Option<i64>| {
        handoff::post(&c, &handoff::Post { project_id: p.id, kind, text, parent, ..Default::default() }).unwrap()
    };
    let req = post("request", "Fix the lease docs", None);
    assert!(handoff::take(&c, req, "a", Some(600)).unwrap().starts_with("claimed"));
    let again = handoff::take(&c, req, "a", Some(600)).unwrap();
    assert!(again.starts_with("claimed") && again.contains("renewed"), "{again}");
    let refused = handoff::take(&c, req, "b", Some(600)).unwrap();
    assert!(refused.starts_with(&format!("refused {} already_taken by a until ", handoff::handle(req))), "{refused}");
    let held = post("request", "Pick me up", None);
    handoff::pickup(&c, held, "a").unwrap();
    let r = handoff::pickup(&c, held, "b").unwrap();
    assert!(r.contains("already_taken by a") && r.contains("no expiry"), "{r}");
    let done = post("request", "Already handled", None);
    let rep = post("report", "handled it", Some(done));
    let r = handoff::take(&c, done, "b", Some(600)).unwrap();
    assert!(r.starts_with(&format!("refused {} already_done", handoff::handle(done))) && r.contains(&handoff::handle(rep)), "{r}");
    let e = handoff::take(&c, 0xfff, "b", None).unwrap_err().to_string();
    assert!(e.contains("refused h:fff not_found"), "{e}");
}

/// A brief cut at its budget or cap says so on its last line, with the call that gets the rest.
#[test]
fn a_cut_brief_says_it_was_cut() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, mut c, _p) = setup();
    let mut log = String::from("# Log\n\n## Current gate\n\n- Gate: HX-1 is active.\n\n");
    for i in 0..40 {
        log.push_str(&format!("## 2026-10-{:02} — HX-{i} a long milestone title number {i} — IN_PROGRESS\n\nbody\n\n", 1 + i % 28));
    }
    write(repo.path(), "AGENT_MILESTONE_LOG.md", &log);
    let p = project::find(&c, &project::locate(repo.path())).unwrap().unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    let p = project::find(&c, &project::locate(repo.path())).unwrap().unwrap();
    let b = foxdb::query::brief(&c, &p, Some("main"), 40).unwrap();
    let last = b.lines().last().unwrap();
    assert!(last.contains("brief cut short") && last.contains("fox_memory op=brief"), "{b}");
    let full = foxdb::query::brief(&c, &p, Some("main"), 400).unwrap();
    assert!(!full.contains("cut short"), "{full}");
    // The hook cap marks its cut the same way and stays within the cap.
    let long: String = (0..200).map(|i| format!("line {i} of the brief\n")).collect();
    let capped = hook::cap_marked(&long, 300);
    assert!(capped.chars().count() <= 300 && capped.lines().last().unwrap().contains("brief cut short"), "{capped}");
    assert_eq!(hook::cap_marked("short\n", 300), "short\n");
}
