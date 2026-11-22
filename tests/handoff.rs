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

// ---- swarm board: typed entries, attachments by handle, per-agent cursors ----

fn memory_call(args: serde_json::Value) -> (String, bool) {
    let r = mcp::handle(&json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"fox_memory","arguments":args}})).unwrap();
    (r["result"]["content"][0]["text"].as_str().unwrap().to_string(), r["result"]["isError"].as_bool().unwrap())
}

fn read_call(target: &str) -> (String, bool) {
    let r = mcp::handle(&json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"fox_read","arguments":{"target": target}}}))
        .unwrap();
    (r["result"]["content"][0]["text"].as_str().unwrap().to_string(), r["result"]["isError"].as_bool().unwrap())
}

/// [`setup`] plus the tree-sitter pass, so `alpha` is a symbol.
fn setup_ts() -> (tempfile::TempDir, tempfile::TempDir, rusqlite::Connection, project::Project) {
    let (h, r, mut c, p) = setup();
    foxdb::tsindex::index_project(&mut c, &p).unwrap();
    let p = project::find(&c, &project::locate(r.path())).unwrap().unwrap();
    (h, r, c, p)
}

fn node_handle(c: &rusqlite::Connection, kind: &str, name: &str) -> String {
    let id: i64 = c.query_row("SELECT id FROM node WHERE kind = ?1 AND name = ?2", [kind, name], |r| r.get(0)).unwrap();
    foxdb::query::handle(kind, id)
}

fn posted(t: &str) -> i64 {
    let h = t.split_whitespace().find(|w| w.starts_with("h:")).unwrap_or_else(|| panic!("no handle in {t}"));
    handoff::parse_handle(h).unwrap()
}

/// fact, failed, summary and question are board kinds of their own. Each stays short (the cap
/// is [`handoff::ENTRY_MAX`]), and the board shows such an entry's whole text on its row, so a
/// read of what is new needs no second call per entry.
#[test]
fn swarm_entries_are_typed_short_and_read_whole_on_the_board() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, c, p) = setup();
    for kind in ["fact", "failed", "summary", "question"] {
        assert!(handoff::KINDS.contains(&kind), "{kind}");
    }
    let post = |kind: &str, text: &str| handoff::post(&c, &handoff::Post { project_id: p.id, kind, text, ..Default::default() }).unwrap();
    let fact = post("fact", "The cache key omits the generation.\nSo two generations share an entry.");
    let failed = post("failed", "Raising the TTL did not help: the stale entry is keyed, not timed.");
    let long: String = (0..400).map(|i| format!("w{i} ")).collect();
    let q = post("question", &long);
    let body: String = c.query_row("SELECT body FROM handoff WHERE id = ?1", [q], |r| r.get(0)).unwrap();
    assert!(body.chars().count() <= handoff::ENTRY_MAX, "{} chars", body.chars().count());
    // Other kinds keep the long cap.
    let rep = post("report", &long);
    let rbody: String = c.query_row("SELECT body FROM handoff WHERE id = ?1", [rep], |r| r.get(0)).unwrap();
    assert!(rbody.chars().count() > handoff::ENTRY_MAX);
    let board = handoff::read_since(&c, p.id, handoff::Since::After(0), None, None, 2000).unwrap();
    let row = board.lines().find(|l| l.starts_with(&handoff::handle(fact))).unwrap();
    assert!(row.contains("fact") && row.contains("So two generations share an entry."), "whole text on the row: {row}");
    let row = board.lines().find(|l| l.starts_with(&handoff::handle(failed))).unwrap();
    assert!(row.contains("failed") && row.contains("keyed, not timed"), "{row}");
    assert!(handoff::read(&c, p.id, Some("failed"), None, 500, 0).unwrap().contains("keyed, not timed"));
    // An unknown kind is refused and names the kinds.
    let e = handoff::post(&c, &handoff::Post { project_id: p.id, kind: "fail", text: "x", ..Default::default() }).unwrap_err().to_string();
    assert!(e.contains("failed") && e.contains("fact"), "{e}");
}

/// Attachments are handles, never pasted content: a path becomes its file's `f:` handle, a
/// known handle stays, an unknown handle is refused. Each attached handle opens with fox_read,
/// and fox_read of the entry lists what its handles point at.
#[test]
fn attachments_are_handles_that_fox_read_resolves() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, c, _p) = setup_ts();
    std::env::set_current_dir(repo.path()).unwrap();
    let (f, s) = (node_handle(&c, "file", "lib.rs"), node_handle(&c, "symbol", "alpha"));
    let (t, err) = memory_call(
        json!({"op": "post", "kind": "fact", "agent": "w1", "text": "alpha has no callers", "evidence": format!("src/lib.rs, {s}")}),
    );
    assert!(!err && t.starts_with("posted h:"), "{t}");
    let id = posted(&t);
    let ev: String = c.query_row("SELECT evidence FROM handoff WHERE id = ?1", [id], |r| r.get(0)).unwrap();
    assert_eq!(ev, format!("{f} {s}"), "paths become file handles");
    let (shown, err) = read_call(&handoff::handle(id));
    assert!(!err && shown.contains("attached") && shown.contains("src/lib.rs") && shown.contains("alpha"), "{shown}");
    for h in [&f, &s] {
        let (out, err) = read_call(h);
        assert!(!err && !out.trim().is_empty(), "{h} must open with fox_read: {out}");
    }
    let (t, err) = memory_call(json!({"op": "post", "kind": "failed", "agent": "w1", "text": "tried it", "evidence": "c:fffffff"}));
    assert!(err && t.contains("unknown handle c:fffffff"), "{t}");
    // An over-long entry is clipped, and the reply says so.
    let long = "z".repeat(900);
    let (t, err) = memory_call(json!({"op": "post", "kind": "fact", "agent": "w1", "text": long}));
    assert!(!err && t.contains("clipped"), "{t}");
}

/// `since=me` reads what is new to the calling agent: its first read shows the latest entries,
/// later reads only entries posted since, never its own. Subagents of one session share one MCP
/// server, so each names itself with `agent`; `since=h:N` is the stateless form.
#[test]
fn since_me_returns_only_entries_new_to_that_agent() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, _c, _p) = setup();
    std::env::set_current_dir(repo.path()).unwrap();
    let post = |who: &str, kind: &str, text: &str| {
        let (t, err) = memory_call(json!({"op": "post", "kind": kind, "agent": who, "text": text}));
        assert!(!err, "{t}");
        posted(&t)
    };
    let read = |who: &str| {
        let (t, err) = memory_call(json!({"op": "read", "since": "me", "agent": who}));
        assert!(!err, "{t}");
        t
    };
    post("A", "fact", "A one: the lease is per repo");
    let b1 = post("B", "failed", "B one: retrying the lock did not help");
    let r = read("A");
    assert!(r.contains("B one") && !r.contains("A one"), "own posts are skipped: {r}");
    assert!(r.trim_end().lines().last().unwrap().starts_with("since=h:"), "{r}");
    assert!(read("A").contains("nothing new"));
    post("B", "question", "B two: is the lease renewed on take?");
    let r = read("A");
    assert!(r.contains("B two") && !r.contains("B one"), "{r}");
    let r = read("C");
    assert!(r.contains("A one") && r.contains("B one") && r.contains("B two"), "a new agent sees the latest: {r}");
    // `auto` is the same cursor as `me`.
    let (t, _) = memory_call(json!({"op": "read", "since": "auto", "agent": "C"}));
    assert!(t.contains("nothing new"), "{t}");
    // Stateless: after a handle, whoever asks.
    let (t, _) = memory_call(json!({"op": "read", "since": handoff::handle(b1), "agent": "A"}));
    assert!(t.contains("B two") && !t.contains("B one"), "{t}");
}

fn parent_transcript(dir: &Path, repo: &Path) -> std::path::PathBuf {
    let r = repo.to_string_lossy();
    let line = |name: &str, input: serde_json::Value| {
        json!({"type": "assistant", "sessionId": "s1", "cwd": r, "message": {"role": "assistant",
            "content": [{"type": "tool_use", "id": "t", "name": name, "input": input}]}})
        .to_string()
    };
    let lines = [
        line("Read", json!({"file_path": format!("{r}/src/lib.rs")})),
        line("Read", json!({"file_path": "/etc/hostname"})),
        line("Grep", json!({"pattern": "alpha", "path": format!("{r}/src")})),
        line("mcp__foxdb__fox_read", json!({"target": "AGENT_MILESTONE_LOG.md"})),
    ];
    let p = dir.join("parent.jsonl");
    fs::write(&p, lines.join("\n") + "\n").unwrap();
    p
}

/// The SubagentStart brief carries the handles of files the parent already read (from its
/// transcript), never their content, and the board protocol (read posts after the parent's
/// snapshot, post fact/failed/question with handles) only with `[hooks] swarm_board = true`.
/// Siblings get the same bytes, within the cap, with the option on or off.
#[test]
fn subagent_brief_has_the_parents_files_as_handles_and_the_board_protocol_only_when_asked() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, c, _p) = setup_ts();
    let tx = tempfile::tempdir().unwrap();
    let t = parent_transcript(tx.path(), repo.path());
    let input = |id: &str| {
        json!({"hook_event_name": "SubagentStart", "session_id": "s1", "cwd": repo.path(), "agent_id": id,
            "agent_type": "worker", "transcript_path": t})
        .to_string()
    };
    let ctx_of = |out: &str| {
        let v: serde_json::Value = serde_json::from_str(out).unwrap();
        v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string()
    };
    // Off by default: the parent packet and parent read lines, no board line.
    let off = foxdb::config::Config::default();
    let a = hook::run_with(&off, "subagent-start", &input("a1"), hook::Harness::Claude);
    let b = hook::run_with(&off, "subagent-start", &input("b2"), hook::Harness::Claude);
    assert_eq!(a, b, "sibling briefs must be byte-identical");
    let plain = ctx_of(&a);
    assert!(plain.contains("parent packet h:") && plain.contains("parent read"), "{plain}");
    assert!(!plain.lines().any(|l| l.starts_with("board:")) && !plain.contains("since=h:"), "the board line is opt-in: {plain}");
    // On: the board protocol, from the snapshot.
    let on = foxdb::config::parse("[hooks]\nswarm_board = true\n");
    let a = hook::run_with(&on, "subagent-start", &input("a1"), hook::Harness::Claude);
    let b = hook::run_with(&on, "subagent-start", &input("b2"), hook::Harness::Claude);
    assert_eq!(a, b, "sibling briefs must be byte-identical");
    let ctx = ctx_of(&a);
    let ctx = ctx.as_str();
    let snap = ctx.split_whitespace().find(|w| w.starts_with("h:")).unwrap();
    assert!(ctx.contains(&format!("since={snap}")), "board cursor from the snapshot: {ctx}");
    for want in ["fact", "failed", "question", "agent="] {
        assert!(ctx.contains(want), "missing {want:?}: {ctx}");
    }
    let f = node_handle(&c, "file", "lib.rs");
    assert!(ctx.contains(&format!("{f} src/lib.rs")), "the parent's file as a handle: {ctx}");
    assert!(!ctx.contains("hostname") && !ctx.contains("pub fn alpha"), "no outside paths, no content: {ctx}");
    assert!(ctx.chars().count() <= hook::SUBAGENT_BRIEF_CHARS, "{} chars", ctx.chars().count());
    // Without a transcript the brief just leaves the line out.
    let bare =
        json!({"hook_event_name": "SubagentStart", "session_id": "s1", "cwd": repo.path(), "agent_id": "c3", "agent_type": "worker"});
    let out = hook::run_with(&on, "subagent-start", &bare.to_string(), hook::Harness::Claude);
    assert!(!out.contains("src/lib.rs") && out.contains("since=h:"), "{out}");
}

/// SubagentStop still saves the whole report, and also posts a short `summary` entry that links
/// it, with the paths it cites as file handles: siblings reading the board get the gist.
#[test]
fn subagent_stop_posts_a_summary_entry_with_handles() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, c, p) = setup_ts();
    std::env::set_current_dir(repo.path()).unwrap();
    let filler: String = (0..300).map(|i| format!("detail{i} ")).collect();
    let msg = format!("Found it: alpha in src/lib.rs:1 is never called, see c:fffffff.\n{filler}");
    let input = json!({"hook_event_name": "SubagentStop", "session_id": "s1", "cwd": repo.path(), "agent_id": "a9",
        "agent_type": "worker", "last_assistant_message": msg})
    .to_string();
    assert_eq!(hook::run("subagent-stop", &input, hook::Harness::Claude), "");
    let rep: i64 = c.query_row("SELECT id FROM handoff WHERE project_id = ?1 AND kind = 'report'", [p.id], |r| r.get(0)).unwrap();
    let (sid, body, ev, parent): (i64, String, String, i64) = c
        .query_row("SELECT id, body, evidence, parent_id FROM handoff WHERE project_id = ?1 AND kind = 'summary'", [p.id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap();
    assert_eq!(parent, rep, "the summary hangs off the report");
    assert!(body.starts_with("Found it") && body.contains(&handoff::handle(rep)), "{body}");
    assert!(body.chars().count() <= handoff::ENTRY_MAX, "{} chars", body.chars().count());
    let f = node_handle(&c, "file", "lib.rs");
    assert_eq!(ev, f, "cited paths become handles; unknown handles are dropped");
    let who: String = c.query_row("SELECT agent FROM handoff WHERE id = ?1", [sid], |r| r.get(0)).unwrap();
    assert_eq!(who, "a9", "the summary is the subagent's own, not its type's");
    let board = handoff::read_since(&c, p.id, handoff::Since::After(0), Some("summary"), None, 500).unwrap();
    assert!(board.contains(&handoff::handle(sid)) && board.contains("Found it"), "{board}");
    let (out, err) = read_call(&f);
    assert!(!err && out.contains("alpha"), "{out}");
}

/// Evidence on a swarm entry is handles and paths only, and short: prose there would carry
/// content past the 600-character cap. Handle prefixes are case-insensitive.
#[test]
fn evidence_keeps_handles_and_paths_only_within_its_cap() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, c, p) = setup_ts();
    let s = node_handle(&c, "symbol", "alpha");
    let f = node_handle(&c, "file", "lib.rs");
    let prose: String = (0..300).map(|i| format!("word{i} ")).collect();
    let raw = format!("{prose} src/lib.rs:1 {} docs/not-indexed.md", s.to_uppercase());
    let id = handoff::post(&c, &handoff::Post { project_id: p.id, kind: "fact", text: "x", evidence: Some(&raw), ..Default::default() })
        .unwrap();
    let ev: String = c.query_row("SELECT evidence FROM handoff WHERE id = ?1", [id], |r| r.get(0)).unwrap();
    assert_eq!(ev, format!("{f} {s} docs/not-indexed.md"), "prose dropped, upper-case handle accepted");
    // Many long paths stop at the byte cap.
    let paths: String = (0..8).map(|i| format!("dir{i}/{}.rs ", "p".repeat(120))).collect();
    let id = handoff::post(&c, &handoff::Post { project_id: p.id, kind: "fact", text: "y", evidence: Some(&paths), ..Default::default() })
        .unwrap();
    let ev: String = c.query_row("SELECT evidence FROM handoff WHERE id = ?1", [id], |r| r.get(0)).unwrap();
    assert!(!ev.is_empty() && ev.len() <= handoff::EVIDENCE_MAX, "{} bytes", ev.len());
}

/// A row longer than the whole budget still shows, clipped, and the cursor moves past it: a long
/// entry can never leave an agent reading "nothing new" forever.
#[test]
fn a_row_longer_than_the_budget_still_shows_and_moves_the_cursor() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, c, p) = setup();
    let long = |tag: &str| format!("{tag} {}", "long text ".repeat(55));
    let post = |t: &str| {
        handoff::post(&c, &handoff::Post { project_id: p.id, kind: "fact", text: t, agent: Some("other"), ..Default::default() }).unwrap()
    };
    let a = post(&long("first"));
    let r = handoff::read_since(&c, p.id, handoff::Since::Auto("me"), None, None, 32).unwrap();
    assert!(r.contains(&handoff::handle(a)) && r.contains("first"), "a first read shows the newest row: {r}");
    let b = post(&long("second"));
    let r = handoff::read_since(&c, p.id, handoff::Since::Auto("me"), None, None, 32).unwrap();
    assert!(r.contains(&handoff::handle(b)) && !r.contains("nothing new"), "{r}");
    assert!(r.lines().next().unwrap().len() <= 32 * 4, "clipped to the budget: {r}");
    assert!(r.trim_end().ends_with(&format!("since={}", handoff::handle(b))), "{r}");
    let r = handoff::read_since(&c, p.id, handoff::Since::Auto("me"), None, None, 32).unwrap();
    assert!(r.contains("nothing new"), "{r}");
}

/// Without `agent=`, a session with running subagents is one identity with all of them: a
/// `since=me` read would move every sibling's cursor. It is refused with the fix, and an unnamed
/// post is tagged so the session's own reads still show it.
#[test]
fn since_me_without_agent_is_refused_while_subagents_share_the_session() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, c, _p) = setup();
    std::env::set_current_dir(repo.path()).unwrap();
    let saved = std::env::var("CLAUDE_CODE_SESSION_ID").ok();
    unsafe {
        std::env::remove_var("FOXDB_AGENT");
        std::env::set_var("CLAUDE_CODE_SESSION_ID", "sess-1");
    }
    let start = json!({"hook_event_name": "SubagentStart", "session_id": "sess-1", "cwd": repo.path(), "agent_id": "sub-1", "agent_type": "worker"});
    hook::run("subagent-start", &start.to_string(), hook::Harness::Claude);
    let (t, err) = memory_call(json!({"op": "read", "since": "me"}));
    assert!(err && t.contains("agent=") && t.contains("since=h:"), "{t}");
    let (t, err) = memory_call(json!({"op": "read", "since": "me", "agent": "sub-1"}));
    assert!(!err, "{t}");
    let (t, err) = memory_call(json!({"op": "post", "kind": "fact", "text": "posted without a name"}));
    assert!(!err, "{t}");
    let who: String = c.query_row("SELECT agent FROM handoff WHERE id = ?1", [posted(&t)], |r| r.get(0)).unwrap();
    assert_eq!(who, "sess-1/unnamed");
    // Once the subagent stops, the session reads as itself, and sees the tagged post.
    let stop = json!({"hook_event_name": "SubagentStop", "session_id": "sess-1", "cwd": repo.path(), "agent_id": "sub-1",
        "agent_type": "worker", "last_assistant_message": "done"});
    hook::run("subagent-stop", &stop.to_string(), hook::Harness::Claude);
    let (t, err) = memory_call(json!({"op": "read", "since": "me"}));
    assert!(!err && t.contains("posted without a name"), "{t}");
    unsafe {
        match saved {
            Some(v) => std::env::set_var("CLAUDE_CODE_SESSION_ID", v),
            None => std::env::remove_var("CLAUDE_CODE_SESSION_ID"),
        }
    }
}

/// The parent's reads, newest first and each once; a sibling directory that only shares the
/// root's name prefix is not under the root.
#[test]
fn parent_reads_are_newest_first_once_and_under_the_root_only() {
    let repo = tempfile::tempdir().unwrap();
    let tx = tempfile::tempdir().unwrap();
    let r = repo.path().to_string_lossy();
    let read = |p: String| {
        json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "Read", "input": {"file_path": p}}]}})
            .to_string()
    };
    let lines = [read(format!("{r}/a.rs")), read(format!("{r}/b.rs")), read(format!("{r}-old/c.rs")), read(format!("{r}/a.rs"))];
    let t = tx.path().join("t.jsonl");
    fs::write(&t, lines.join("\n") + "\n").unwrap();
    assert_eq!(hook::parent_reads(&t, repo.path()), ["a.rs", "b.rs"]);
    // The transcript names the repo through a symlink (macOS: /var -> /private/var) while the
    // project root is canonical, or the other way round: the same files either way.
    let link = tx.path().join("repo-link");
    std::os::unix::fs::symlink(repo.path(), &link).unwrap();
    fs::write(repo.path().join("a.rs"), "").unwrap();
    let l = link.to_string_lossy();
    let t2 = tx.path().join("t2.jsonl");
    fs::write(&t2, [read(format!("{l}/a.rs")), read(format!("{l}/sub/new.rs"))].join("\n") + "\n").unwrap();
    let canon = fs::canonicalize(repo.path()).unwrap();
    assert_eq!(hook::parent_reads(&t2, &canon), ["sub/new.rs", "a.rs"]);
    assert_eq!(hook::parent_reads(&t, &link), ["a.rs", "b.rs"]);
}
