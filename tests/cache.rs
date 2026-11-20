//! Prompt-cache discipline: byte-stable briefs, per-harness caps, frozen MCP tool definitions.

use foxdb::{db, hook, inventory, mcp, project, query};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    (home, repo)
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn indexed(repo: &Path) -> (rusqlite::Connection, project::Project) {
    let mut log = String::from("# Log\n\n## Current gate\n\n- Gate: QX-9 is active.\n\n");
    for i in 0..60 {
        log.push_str(&format!("## 2026-10-{:02} — QX-{i} step {i} — IN_PROGRESS\n\nbody {i}\n\n", 1 + i % 28));
    }
    write(repo, "AGENT_MILESTONE_LOG.md", &log);
    write(repo, "src/lib.rs", "pub fn alpha() {}\n");
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo)).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    let p = project::find(&c, &project::locate(repo)).unwrap().unwrap();
    (c, p)
}

#[test]
fn brief_is_byte_identical_when_nothing_changed() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    let (mut c, p) = indexed(repo.path());
    let a = query::brief(&c, &p, Some("main"), 400).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let b = query::brief(&c, &p, Some("main"), 400).unwrap();
    assert_eq!(a, b);
    assert!(!a.contains(" ago"), "relative age in brief: {a}");
    // A no-op re-index does not change a byte either.
    inventory::index_project(&mut c, &p).unwrap();
    let p = project::find(&c, &project::locate(repo.path())).unwrap().unwrap();
    assert_eq!(a, query::brief(&c, &p, Some("main"), 400).unwrap());
}

#[test]
fn volatile_savings_line_comes_last() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    let (c, p) = indexed(repo.path());
    foxdb::savings::record(&c, "find", 123_456).unwrap();
    let b = query::brief(&c, &p, Some("main"), 400).unwrap();
    let last = b.lines().last().unwrap();
    assert!(last.contains("fox saved"), "savings line is not last:\n{b}");
}

#[test]
fn harness_caps_hold_even_with_a_huge_budget() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    let (c, p) = indexed(repo.path());
    for h in [hook::Harness::Claude, hook::Harness::Codex] {
        let b = hook::brief_for(&c, &p, Some("main"), h, 100_000).unwrap();
        assert!(b.chars().count() <= h.cap_chars(), "{h:?} brief {} chars over its cap", b.chars().count());
    }
    assert!(hook::Harness::Claude.cap_chars() <= 10_000);
    assert!(hook::Harness::Codex.cap_chars() <= 2_500 * 4);
}

#[test]
fn session_start_hook_emits_additional_context_json() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    let (_c, _p) = indexed(repo.path());
    let input = serde_json::json!({"hook_event_name": "SessionStart", "source": "startup", "cwd": repo.path()}).to_string();
    let out = hook::run("session-start", &input, hook::Harness::Claude);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("Gate: QX-9 is active."));
    // Fail open: garbage in, nothing out.
    assert_eq!(hook::run("session-start", "not json", hook::Harness::Claude), "");
}

/// The tool list is part of every request's cached prefix. Changing it invalidates every cache, so
/// it changes only on purpose: update the hash here in the same commit.
#[test]
fn mcp_tool_definitions_are_frozen() {
    let tools = mcp::tools().to_string();
    let init = mcp::handle(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})).unwrap();
    let instructions = init["result"]["instructions"].as_str().unwrap_or_default().to_string();
    let h = fnv_hex(&format!("{tools}\n{instructions}"));
    assert_eq!(h, FROZEN_TOOLS_HASH, "MCP tool definitions or instructions changed; if deliberate, update FROZEN_TOOLS_HASH");
    // Small: tool definitions sit in every cached prefix, so keep them far below 1.5k tokens.
    assert!((tools.len() + instructions.len()) / 4 < 1_500, "tool definitions grew to ~{} tokens", (tools.len() + instructions.len()) / 4);
    // Byte-stable across calls (no maps with random order, no timestamps).
    assert_eq!(tools, mcp::tools().to_string());
}

const FROZEN_TOOLS_HASH: &str = "3e1a740de4a3c65d";

fn fnv_hex(s: &str) -> String {
    // FNV-1a 64: dependency-free and stable across platforms; collisions are irrelevant here.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

// ---- milestone 3: cache safety ----

/// Claude Code rewrites the cached history when PreToolUse/PostToolUse return context
/// (anthropics/claude-code#83913), and UserPromptSubmit context grows every turn. By default
/// foxdb injects only at session start and subagent start; the grep and prompt hooks are opt-in
/// (`[hooks] grep_context` / `prompt_context`, tested in tests/toolhooks.rs).
#[test]
fn context_is_injected_only_at_session_and_subagent_start() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    let (_c, _p) = indexed(repo.path());
    let events = [
        ("session-start", "SessionStart"),
        ("subagent-start", "SubagentStart"),
        ("subagent-stop", "SubagentStop"),
        ("pre-compact", "PreCompact"),
        ("pre-tool-use", "PreToolUse"),
        ("post-tool-use", "PostToolUse"),
        ("user-prompt-submit", "UserPromptSubmit"),
        ("stop", "Stop"),
        ("notification", "Notification"),
        ("session-end", "SessionEnd"),
    ];
    let mut injected = vec![];
    for (event, name) in events {
        let input = serde_json::json!({
            "hook_event_name": name, "source": "startup", "cwd": repo.path(), "session_id": "s1",
            "agent_id": "a1", "agent_type": "helper", "last_assistant_message": "done", "prompt": "hi",
            "tool_name": "Bash", "tool_input": {"command": "ls"}
        })
        .to_string();
        let out = hook::run_with(&foxdb::config::Config::default(), event, &input, hook::Harness::Claude);
        if out.is_empty() {
            continue;
        }
        // PreCompact stdout is instructions to the compaction summariser, plain text that never
        // enters a cached prefix as context.
        if event == "pre-compact" {
            assert!(serde_json::from_str::<serde_json::Value>(&out).is_err(), "PreCompact output is plain text: {out}");
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        if v["hookSpecificOutput"].get("additionalContext").is_some() {
            injected.push(v["hookSpecificOutput"]["hookEventName"].as_str().unwrap_or_default().to_string());
        }
    }
    assert_eq!(injected, ["SessionStart", "SubagentStart"], "context injected outside session/subagent start");
    // The source names no other event as a context carrier, except the two opt-in tool hooks,
    // each behind its config switch.
    let src = include_str!("../src/hook.rs");
    for l in src.lines().filter(|l| l.contains("context(\"")) {
        let ok = l.contains("context(\"SessionStart\"")
            || l.contains("context(\"SubagentStart\"")
            || (l.contains("context(\"PostToolUse\"") && l.contains("cfg.hooks.grep_context"))
            || (l.contains("context(\"UserPromptSubmit\"") && l.contains("cfg.hooks.prompt_context"));
        assert!(ok, "new context carrier: {l}");
    }
    // Each opt-in hook is wired exactly once, as a guarded match arm.
    for (arm, carrier) in [
        ("\"post-tool-use\" if cfg.hooks.grep_context =>", "context(\"PostToolUse\""),
        ("\"user-prompt-submit\" if cfg.hooks.prompt_context =>", "context(\"UserPromptSubmit\""),
    ] {
        let lines: Vec<&str> = src.lines().filter(|l| l.contains(carrier)).collect();
        assert_eq!(lines.len(), 1, "{carrier} must appear once: {lines:?}");
        assert!(lines[0].trim_start().starts_with(arm), "{carrier} must sit behind its switch: {}", lines[0]);
    }
}

/// Claude Code passes PreCompact stdout to the compaction summariser as instructions: ask it to
/// keep the working set and the packet the hook just saved. Short, and the same bytes for the
/// same packet. Codex gets nothing.
#[test]
fn pre_compact_asks_the_summary_to_keep_the_working_set() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    let (c, _p) = indexed(repo.path());
    let input = serde_json::json!({"hook_event_name": "PreCompact", "cwd": repo.path(), "session_id": "s1"}).to_string();
    let out = hook::run("pre-compact", &input, hook::Harness::Claude);
    let id: i64 = c.query_row("SELECT MAX(id) FROM handoff WHERE kind = 'packet' AND scope = 'compact'", [], |r| r.get(0)).unwrap();
    let h = foxdb::handoff::handle(id);
    assert_eq!(out, hook::compact_instructions(&h), "{out}");
    for want in ["path:line", "symbols", "ruled out", &format!("fox_read target={h}")] {
        assert!(out.contains(want), "missing {want:?}:\n{out}");
    }
    assert!(out.lines().count() <= 6 && out.len() < 500, "{out}");
    // Asking for the lines "verbatim" made summaries copy the bullet list itself.
    assert!(!out.contains("verbatim"), "{out}");
    assert_eq!(hook::run("pre-compact", &input, hook::Harness::Codex), "", "Codex reads no PreCompact instructions");
}

/// The committed size budget (`context-budget.json`): briefs, the tool list, the server
/// instructions, the dispatch brief and the report schema all sit in cached prefixes. Any change
/// in size is deliberate: update the file in the same commit.
#[test]
fn context_budget_file_matches() {
    let budget: serde_json::Value = serde_json::from_str(include_str!("../context-budget.json")).unwrap();
    let init = mcp::handle(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})).unwrap();
    let actual = [
        ("session_brief_cap_claude_chars", hook::Harness::Claude.cap_chars()),
        ("session_brief_cap_codex_chars", hook::Harness::Codex.cap_chars()),
        ("subagent_brief_cap_chars", hook::SUBAGENT_BRIEF_CHARS),
        ("session_brief_budget_tokens", hook::SESSION_BRIEF_TOKENS),
        ("subagent_brief_budget_tokens", hook::SUBAGENT_BRIEF_TOKENS),
        ("tools_list_bytes", mcp::tools().to_string().len()),
        ("instructions_bytes", init["result"]["instructions"].as_str().unwrap_or_default().len()),
        ("dispatch_brief_bytes", foxdb::orch::dispatch_brief("h:0").len()),
        ("report_schema_bytes", foxdb::report::SCHEMA.len()),
        ("brief_tools_line_bytes", query::BRIEF_TOOLS_LINE.len()),
        // What every session pays before its first tool call (T3's agentContextBudget).
        (
            "agent_context_bytes",
            init["result"]["instructions"].as_str().unwrap_or_default().len()
                + mcp::tools().to_string().len()
                + query::BRIEF_TOOLS_LINE.len(),
        ),
    ];
    let mut bad = vec![];
    for (k, n) in actual {
        let want = budget[k].as_u64().unwrap_or_else(|| panic!("context-budget.json lacks {k}")) as usize;
        if n != want {
            bad.push(format!("{k}: budget {want}, actual {n}"));
        }
    }
    assert!(bad.is_empty(), "context sizes moved; growth must be deliberate, update context-budget.json:\n{}", bad.join("\n"));
    // Tools Claude Code loads up front instead of deferring behind tool search.
    let always: Vec<String> = mcp::tools()
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["_meta"]["anthropic/alwaysLoad"] == true)
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    let want: Vec<String> =
        serde_json::from_value(budget["always_load_tools"].clone()).expect("context-budget.json lacks always_load_tools");
    assert_eq!(always, want, "always-loaded tools changed; update context-budget.json");
    assert_eq!(budget.as_object().unwrap().len(), actual.len() + 1, "context-budget.json has keys this test does not check");
}

/// headroom replaces tool output with markers like `Retrieve more: hash=…` and `[↑4L same as
/// msg 3]`. foxdb output must never look like one, or a reader (or headroom) mistakes it.
#[test]
fn foxdb_output_never_resembles_headroom_markers() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    let (c, p) = indexed(repo.path());
    let marker = regex::Regex::new(r"(?i)hash=|compressed|<<ccr:|same as msg|retrieve (more|original)").unwrap();
    let mut outs = vec![mcp::tools().to_string(), mcp::INSTRUCTIONS.to_string()];
    outs.push(query::brief(&c, &p, Some("main"), 400).unwrap());
    outs.push(query::brief(&c, &p, Some("main"), 40).unwrap());
    let input = serde_json::json!({"hook_event_name": "SessionStart", "source": "startup", "cwd": repo.path()}).to_string();
    outs.push(hook::run("session-start", &input, hook::Harness::Claude));
    let body = "x".repeat(foxdb::spare::DEDUP_MIN_BYTES + 100);
    outs.push(foxdb::spare::filter_db(&c, Some("s1"), "s:1", body.clone(), false));
    outs.push(foxdb::spare::filter_db(&c, Some("s1"), "s:1", body, false));
    let o = query::FindOpts { kind: "any", all: false, budget: 500, limit: 20, offset: 0 };
    outs.push(foxdb::search::find(&c, Some(&p), "alpha", &o).unwrap());
    outs.push(foxdb::orch::dispatch_brief("h:1"));
    outs.push(foxdb::report::SCHEMA.to_string());
    for o in &outs {
        assert!(!marker.is_match(o), "output resembles a headroom marker: {o}");
    }
}

/// The savings note is never appended to tool answers: identical answers stay identical. The
/// session brief and `fox stats` still show savings.
#[test]
fn tool_answers_carry_no_savings_note() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    let (c, p) = indexed(repo.path());
    for _ in 0..45 {
        let out = foxdb::savings::account_with(&c, Some(p.id), "find", "src/lib.rs:1 alpha\n".into());
        assert_eq!(out, "src/lib.rs:1 alpha\n");
    }
    assert!(foxdb::savings::stats(&c).unwrap().contains("45 answers"));
}

/// The savings baseline is what an agent would read without foxdb: a ranged read around each
/// cited line, never more than the file. Reading every cited file whole overstated it ~10x.
#[test]
fn savings_compare_with_a_ranged_read_of_each_cited_line() {
    use foxdb::savings::{RANGE_BYTES, estimate, tokens};
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    let big: String = (0..3000).map(|i| format!("pub fn filler_function_number_{i}() {{}}\n")).collect();
    write(repo.path(), "src/big.rs", &big);
    let (c, p) = indexed(repo.path());
    let small = fs::metadata(repo.path().join("src/lib.rs")).unwrap().len() as usize;
    let out = "src/big.rs:10 filler_function_number_9\nsrc/big.rs:2000 filler_function_number_1999\nsrc/lib.rs:1 alpha\n";
    let want = tokens(2 * RANGE_BYTES) + tokens(small) - tokens(out.len());
    assert_eq!(estimate(&c, Some(p.id), out), want);
    assert!(want < tokens(big.len()) / 10, "two cited lines are not the whole file");
    // A grouped answer: a file header with its site count, then `:line` rows.
    let grouped = "src/\n big.rs (3)\n  :4 a\n  :9 b\n  :70 c\n";
    assert_eq!(estimate(&c, Some(p.id), grouped), tokens(3 * RANGE_BYTES) - tokens(grouped.len()));
    assert!(foxdb::savings::stats(&c).unwrap().starts_with("tokens saved (est., vs a ranged read of each cited line)"));
}

#[test]
fn read_only_tools_are_annotated_and_nothing_is_structured() {
    let tools = mcp::tools();
    for t in tools.as_array().unwrap() {
        let name = t["name"].as_str().unwrap();
        let ro = t["annotations"]["readOnlyHint"] == true;
        assert_eq!(ro, name != "fox_memory", "{name} readOnlyHint");
        // Codex replaces a result's text with structuredContent JSON; foxdb answers are text.
        assert!(t.get("outputSchema").is_none(), "{name} has an outputSchema");
    }
    let r = mcp::handle(&serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "fox_find", "arguments": {}}}))
    .unwrap();
    assert!(r["result"].get("structuredContent").is_none(), "{r}");
    assert!(r["result"]["content"][0]["text"].is_string());
}

#[test]
fn protocol_version_is_one_foxdb_implements() {
    let init = |v: Option<&str>| {
        let params = match v {
            Some(v) => serde_json::json!({"protocolVersion": v}),
            None => serde_json::json!({}),
        };
        let r = mcp::handle(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params})).unwrap();
        r["result"]["protocolVersion"].as_str().unwrap().to_string()
    };
    let latest = mcp::PROTOCOL_VERSIONS[0];
    for v in mcp::PROTOCOL_VERSIONS {
        assert_eq!(init(Some(v)), *v);
    }
    assert_eq!(init(Some("2099-01-01")), latest, "an unknown version gets foxdb's latest, not an echo");
    assert_eq!(init(Some("garbage")), latest);
    assert_eq!(init(None), latest);
}

/// A project whose brief overflows the hook budgets: a long gate paragraph and long titles.
fn indexed_long(repo: &Path) {
    let gate: String = (0..60).map(|i| format!("gate clause {i} ")).collect();
    let mut log = format!("# Log\n\n## Current gate\n\n- {gate}\n\n");
    for i in 0..12 {
        let title: String = (0..25).map(|k| format!("word{k} ")).collect();
        log.push_str(&format!("## 2026-10-{:02} — QX-{i} {title}— IN_PROGRESS\n\nbody {i}\n\n", 1 + i));
    }
    write(repo, "docs/planning/milestones/archive-and-current/long-running-programme/AGENT_MILESTONE_LOG.md", &log);
    write(repo, "src/lib.rs", "pub fn alpha() {}\n");
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo)).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
}

/// The budgets that size the injected briefs are in `context-budget.json` too, and the hooks
/// stay inside them: raising either budget fails here, not only a cap.
#[test]
fn injected_briefs_stay_within_their_token_budgets() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    indexed_long(repo.path());
    let budget: serde_json::Value = serde_json::from_str(include_str!("../context-budget.json")).unwrap();
    let tokens = |k: &str| budget[k].as_u64().unwrap_or_else(|| panic!("context-budget.json lacks {k}")) as usize;
    let inject = |name: &str| {
        let input = serde_json::json!({"hook_event_name": name, "source": "startup", "cwd": repo.path(), "session_id": "s1",
            "agent_id": "a1", "agent_type": "helper"})
        .to_string();
        let event = if name == "SessionStart" { "session-start" } else { "subagent-start" };
        let v: serde_json::Value = serde_json::from_str(&hook::run(event, &input, hook::Harness::Claude)).unwrap();
        v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string()
    };
    let s = inject("SessionStart");
    assert!(s.contains("cut short"), "the fixture must overflow the budget: {s}");
    assert!(s.chars().count() <= tokens("session_brief_budget_tokens") * 4, "session brief {} chars", s.chars().count());
    let sub = inject("SubagentStart");
    let brief: String = sub.lines().take_while(|l| !l.starts_with("parent packet")).map(|l| format!("{l}\n")).collect();
    assert!(brief.chars().count() <= tokens("subagent_brief_budget_tokens") * 4, "subagent brief {} chars", brief.chars().count());
}

/// The cut-short marker names a call that returns more than the cut brief: the lines it cut.
#[test]
fn the_cut_marker_call_returns_the_cut_lines() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    indexed_long(repo.path());
    let input = serde_json::json!({"hook_event_name": "SessionStart", "source": "startup", "cwd": repo.path()}).to_string();
    let v: serde_json::Value = serde_json::from_str(&hook::run("session-start", &input, hook::Harness::Claude)).unwrap();
    let cut = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string();
    let mark = cut.lines().last().unwrap();
    assert!(mark.contains("cut short"), "{cut}");
    let args: serde_json::Map<String, serde_json::Value> = mark
        .split(|c: char| c.is_whitespace() || c == ')' || c == ';')
        .filter_map(|w| w.split_once('='))
        .map(|(k, v)| (k.to_string(), v.parse::<u64>().map(serde_json::Value::from).unwrap_or_else(|_| v.into())))
        .collect();
    assert_eq!(args["op"], "brief", "{mark}");
    std::env::set_current_dir(repo.path()).unwrap();
    let r = mcp::handle(&serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "fox_memory", "arguments": args}}))
    .unwrap();
    let full = r["result"]["content"][0]["text"].as_str().unwrap();
    let kept: Vec<&str> = cut.lines().filter(|l| !l.contains("cut short")).collect();
    let more: Vec<&str> = full.lines().filter(|l| !l.contains("cut short") && !kept.contains(l)).collect();
    assert!(!more.is_empty() && !full.contains("cut short"), "the marker's call did not give back the cut lines:\n{full}");
}

/// The brief names the MCP tools an agent can call (it may have no shell for the `fox` CLI), and
/// that pointer survives a brief that was cut short for its budget.
#[test]
fn brief_names_the_mcp_tools_even_when_cut() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    indexed_long(repo.path());
    for h in [hook::Harness::Claude, hook::Harness::Codex] {
        let input = serde_json::json!({"hook_event_name": "SessionStart", "source": "startup", "cwd": repo.path()}).to_string();
        let v: serde_json::Value = serde_json::from_str(&hook::run("session-start", &input, h)).unwrap();
        let s = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string();
        assert!(s.contains("cut short"), "the fixture must overflow the budget: {s}");
        for tool in ["fox_find", "fox_graph", "fox_read", "fox_memory"] {
            assert!(s.contains(tool), "{h:?} brief does not name {tool}:\n{s}");
        }
    }
}

/// What the model reads before choosing a tool (server instructions, tool descriptions, the
/// skill, the brief's tool line) leads with the question each tool answers, says to call it
/// even when a grep looks quicker, and hands it no reason to skip foxdb.
#[test]
fn tool_wording_leads_with_the_question_and_offers_no_way_out() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    let (c, p) = indexed(repo.path());
    let tools = mcp::tools();
    let desc = |n: &str| -> String {
        tools.as_array().unwrap().iter().find(|t| t["name"] == n).unwrap()["description"].as_str().unwrap().to_string()
    };
    let skill = include_str!("../skill/SKILL.md");
    let skill_desc = skill.lines().find_map(|l| l.strip_prefix("description: ")).unwrap();
    let brief = query::brief(&c, &p, Some("main"), 400).unwrap();
    let tool_line = brief.lines().nth(1).unwrap().to_string();
    let mut texts = vec![("instructions", mcp::INSTRUCTIONS.to_string()), ("skill", skill.to_string()), ("brief", tool_line.clone())];
    for n in ["fox_find", "fox_graph", "fox_read", "fox_memory"] {
        let d = desc(n);
        let first = d.split_inclusive(['?', '.']).next().unwrap_or("");
        assert!(first.ends_with('?'), "{n} does not lead with its question: {d}");
        texts.push((n, d));
    }
    for (what, t) in &texts {
        let l = t.to_lowercase();
        for bad in ["skip", "one grep", "already in context answers", "before grepping"] {
            assert!(!l.contains(bad), "{what} says {bad:?}: {t}");
        }
    }
    for (what, t) in [
        ("instructions", mcp::INSTRUCTIONS.to_string()),
        ("fox_find", desc("fox_find")),
        ("fox_graph", desc("fox_graph")),
        ("skill", skill_desc.to_string()),
    ] {
        assert!(t.contains("even when a grep looks quicker"), "{what}: {t}");
    }
    assert!(skill_desc.len() <= 1024, "skill description is {} chars", skill_desc.len());
    for tool in ["fox_find", "fox_graph", "fox_read", "fox_memory"] {
        assert!(tool_line.contains(tool), "{tool_line}");
    }
}

/// `fox_find` and `fox_graph` carry `_meta["anthropic/alwaysLoad"]`, so Claude Code loads them
/// up front instead of deferring them behind tool search; the brief's second line is the tool line.
#[test]
fn find_and_graph_are_always_loaded() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let r = mcp::handle(&serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
    for t in r["result"]["tools"].as_array().unwrap() {
        let on = t["_meta"]["anthropic/alwaysLoad"] == true;
        assert_eq!(on, matches!(t["name"].as_str(), Some("fox_find" | "fox_graph" | "fox_read")), "{t}");
    }
    let (_home, repo) = setup();
    let (c, p) = indexed(repo.path());
    let brief = query::brief(&c, &p, Some("main"), 400).unwrap();
    assert_eq!(brief.lines().nth(1), Some(query::BRIEF_TOOLS_LINE));
}

/// Literal strings go to Grep (a content kind, not an effort call); past decisions to
/// fox_memory; only tree-sitter graph rows carry code; `~` covers changed Swift files too.
#[test]
fn tool_wording_routes_literals_and_decisions() {
    let tools = mcp::tools();
    let desc = |n: &str| -> String {
        tools.as_array().unwrap().iter().find(|t| t["name"] == n).unwrap()["description"].as_str().unwrap().to_string()
    };
    let skill = include_str!("../skill/SKILL.md");
    let literal = "For a literal string, error message or config value, use Grep.";
    for (what, t) in [("instructions", mcp::INSTRUCTIONS.to_string()), ("fox_find", desc("fox_find")), ("skill", skill.to_string())] {
        assert!(t.contains(literal), "{what}: {t}");
    }
    assert!(!desc("fox_find").contains("past decision") && desc("fox_find").contains("fox_memory op=recall"), "{}", desc("fox_find"));
    assert!(desc("fox_graph").contains("tree-sitter"), "{}", desc("fox_graph"));
    assert!(desc("fox_find").contains("~ = not compiler-resolved (tree-sitter, or Swift changed since the build)"), "{}", desc("fox_find"));
    assert!(!skill.contains("rg` command") && !skill.contains("Rows carry the code at each call site."), "{skill}");
}

/// A project still being indexed for the first time says so instead of answering from an
/// empty index.
#[test]
fn first_index_answers_say_indexing() {
    let p = project::Project { id: 1, name: "x".into(), root: "/nonexistent".into(), kind: "git".into(), indexed_at: None };
    let out = mcp::while_indexing(Some(&p), "no matches for \"Foo\"\n".into());
    assert!(out.contains("indexing this project now; use Grep and Read until it finishes"), "{out}");
    let done = project::Project { indexed_at: Some(1), ..p };
    assert_eq!(mcp::while_indexing(Some(&done), "no matches for \"Foo\"\n".into()), "no matches for \"Foo\"\n");
}
