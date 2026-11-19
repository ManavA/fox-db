//! `fox mcp`: MCP stdio server (newline-delimited JSON-RPC) exposing four tools.
//! Output is compact line text under a token budget (bytes = 4 x tokens), with short handles and
//! `+N more · cursor=K` footers. Fail-open: a tool error is a one-line `isError` result, never a crash.

use crate::{bg, codanna, code, db, project, query, redact};
use anyhow::{Result, bail};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::io::{BufRead, Write};

const DEFAULT_BUDGET: usize = 500;
/// Rows per find page; the budget then picks full, skeleton or id rows (see `spare`).
const FIND_ROWS: usize = 20;

static CLIENT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The MCP client's name from `initialize` (e.g. "claude-code", "codex-mcp-client").
fn client_name() -> Option<String> {
    CLIENT.get().cloned()
}

/// This client's agent identity: `FOXDB_AGENT` from the env the harness gave this server
/// process (each harness spawns its own `fox mcp`), else an explicit `agent` argument, else the
/// session it serves (see [`crate::agents::ambient_id`]): never the bare client name, which every
/// session of one harness shares.
fn agent_id(a: &Value) -> Option<String> {
    if let Ok(v) = std::env::var("FOXDB_AGENT")
        && !v.trim().is_empty()
    {
        return Some(v.trim().to_string());
    }
    if let Some(x) = arg_s(a, "agent") {
        return Some(x.to_string());
    }
    let c = db::open_ro();
    Some(crate::agents::ambient_id(c.as_ref(), &client_name().unwrap_or_else(|| "mcp".into())))
}

/// Longest a `wait` blocks inside one tool call: below Codex's default 60 s tool timeout.
pub const WAIT_MAX_SECS: u64 = 50;

/// Warm search state for this server process (one harness session).
fn warm() -> &'static std::sync::Mutex<crate::search::Warm> {
    static W: std::sync::OnceLock<std::sync::Mutex<crate::search::Warm>> = std::sync::OnceLock::new();
    W.get_or_init(Default::default)
}

/// Short server instructions, sent once in `initialize`. Frozen with the tool list (see the
/// hash test): both sit in every request's cached prefix.
pub const INSTRUCTIONS: &str = "foxdb answers code questions from an index of this repo. \
Where is X: fox_find q=X. Who calls X, what breaks if X changes: fox_graph target=X rel=callers or impact. \
X's body, or a file's declarations: fox_read target=X. A past decision or handoff: fox_memory op=recall. \
Call them first, even when a grep looks quicker: they resolve qualified names and show each call site. \
For a literal string, error message or config value, use Grep. \
Rows end in path:line; pass a row's handle to fox_read or fox_graph.";

/// Protocol versions this server implements, newest first. `initialize` answers with the
/// client's version when it is one of these, else with the newest.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The tool list (`fox_find`, `fox_graph` and `fox_read` carry `_meta["anthropic/alwaysLoad"]`). Never add `outputSchema` or return `structuredContent`: Codex replaces a
/// result's text with the structured JSON, losing foxdb's budgeted line format. The three
/// read tools carry `readOnlyHint` so Claude Code may run them in parallel; `fox_memory` writes.
pub fn tools() -> Value {
    let ro = json!({"readOnlyHint": true});
    // Claude Code defers MCP tools behind tool search; these three load up front, so a where-is,
    // who-calls or read-this question can reach them without a search round-trip. Never toggled mid-session.
    let always = json!({"anthropic/alwaysLoad": true});
    let budget = json!({"type": "integer", "description": "tokens, default 500, max 4000"});
    let cursor = json!({"type": "integer", "description": "from a '+N more · cursor=K' footer"});
    json!([
        {"name": "fox_find", "_meta": always,
         "description": "Where is X? Finds a function, type, field, file or doc section by name or words; ranked rows end in path:line. Use it for every where-is or which-file question, even when a grep looks quicker. For a literal string, error message or config value, use Grep. Earlier decisions and handoffs: fox_memory op=recall. Handles: s/c symbol, f file, d doc, m milestone, a dir, g memory, x session. ~ = not compiler-resolved (tree-sitter, or Swift changed since the build).",
         "annotations": ro,
         "inputSchema": {"type": "object", "required": ["q"], "properties": {
            "q": {"type": "string", "description": "name, substring or words"},
            "kind": {"type": "string", "enum": ["any", "symbol", "file", "dir", "doc", "milestone", "memory"]},
            "project": {"type": "string", "description": "project name or 'all'"},
            "budget": budget, "cursor": cursor}}},
        {"name": "fox_graph", "_meta": always,
         "description": "Who calls X, and what breaks if X changes? Callers, callees, refs, overrides, conformers or impact of a symbol; in tree-sitter languages callers, refs and impact rows carry the line of code. Use it before changing, renaming or deleting a function, field or type, even when a grep looks quicker: it follows qualified and aliased names. target: handle, name, Type.member or pkg.Name; impact without target = git diff.",
         "annotations": ro,
         "inputSchema": {"type": "object", "properties": {
            "target": {"type": "string"},
            "rel": {"type": "string", "enum": ["callers", "callees", "refs", "conforms", "subtypes", "extensions", "overrides", "impact"]},
            "depth": {"type": "integer", "description": "impact depth, default 2"},
            "budget": budget, "cursor": cursor}, "required": ["rel"]}},
        {"name": "fox_read", "_meta": always,
         "description": "What does X say? A symbol's body, a file's declarations with line ranges, a doc section, a handoff (h:) or a session digest (x:). Use it to read one symbol or section instead of a whole file.",
         "annotations": ro,
         "inputSchema": {"type": "object", "required": ["target"], "properties": {
            "target": {"type": "string", "description": "handle, symbol name or repo path"},
            "mode": {"type": "string", "enum": ["outline", "signature", "body", "section", "context"]},
            "lines": {"type": "string", "description": "a-b, for files"},
            "budget": budget, "cursor": cursor}}},
        {"name": "fox_memory",
         "description": "What was decided or found before? recall searches notes, memory files, milestones and past sessions. Also: brief; note; post a finding/claim/request/decision; read the board (since=auto: only what is new to you); pickup or take (leased) a handoff; packet: create (text=goal) or render id=h:; agents: the agent tree; dispatch a codex/claude worker (text=task); wait for agents or h: (bounded, call again).",
         "inputSchema": {"type": "object", "required": ["op"], "properties": {
            "op": {"type": "string", "enum": ["brief", "recall", "note", "post", "read", "pickup", "take", "packet", "agents", "dispatch", "wait"]},
            "q": {"type": "string", "description": "recall or read filter"},
            "text": {"type": "string", "description": "note/post text, packet goal, dispatch task"},
            "kind": {"type": "string", "enum": ["finding", "claim", "request", "decision", "report", "packet"]},
            "topic": {"type": "string", "description": "board topic, for post and read"},
            "since": {"type": "string", "description": "read cursor: 'auto' or h:"},
            "evidence": {"type": "string", "description": "path:line or handles"},
            "next": {"type": "string", "description": "packet next steps"},
            "id": {"type": "string", "description": "h: handle; for wait, agent ids or h: (space-separated)"},
            "harness": {"type": "string", "enum": ["codex", "claude"]},
            "writer": {"type": "boolean", "description": "dispatch holding the repo writer lease"},
            "reviewer": {"type": "boolean", "description": "dispatch a reviewer: task only, no brief"},
            "any": {"type": "boolean", "description": "wait until any one target finishes"},
            "timeout": {"type": "integer", "description": "wait seconds, max 50"},
            "budget": budget}}}
    ])
}

fn arg_s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}
fn arg_n(a: &Value, k: &str) -> Option<usize> {
    a.get(k).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).map(|n| n as usize)
}

/// Project for the server's cwd (registering unknown projects, indexing them in the background).
fn current_project(c: Option<&Connection>) -> Option<project::Project> {
    let loc = project::locate(&std::env::current_dir().ok()?);
    if let Some(c) = c
        && let Ok(Some(p)) = project::find(c, &loc)
    {
        return Some(p);
    }
    if loc.kind == "huge" {
        return None;
    }
    let w = db::open_rw().ok()?;
    let p = project::ensure(&w, &loc).ok()?;
    bg::spawn_background_index(&p);
    Some(p)
}

/// The registered project for the server's cwd, without registering anything.
fn cwd_project_id() -> Option<i64> {
    let c = db::open_ro()?;
    let loc = project::locate(&std::env::current_dir().ok()?);
    project::find(&c, &loc).ok().flatten().map(|p| p.id)
}

fn named_project(c: &Connection, name: &str) -> Option<project::Project> {
    project::by_name(c, name).ok().flatten()
}

struct NodeRow {
    kind: String,
    name: String,
    path: Option<String>,
    line: i64,
    end_line: i64,
    root: std::path::PathBuf,
    project_id: i64,
}

fn node_by_handle(c: &Connection, h: &str) -> Option<NodeRow> {
    let (pfx, hex) = h.split_once(':')?;
    if pfx.len() != 1 {
        return None;
    }
    let id = i64::from_str_radix(hex, 16).ok()?;
    c.query_row(
        "SELECT n.kind, n.name, n.path, COALESCE(n.line,0), COALESCE(n.end_line,0), p.root, n.project_id FROM node n JOIN project p ON p.id = n.project_id WHERE n.id = ?1",
        [id],
        |r| Ok(NodeRow { kind: r.get(0)?, name: r.get(1)?, path: r.get(2)?, line: r.get(3)?, end_line: r.get(4)?, root: std::path::PathBuf::from(r.get::<_, String>(5)?), project_id: r.get(6)? }),
    )
    .ok()
}

fn file_lines(root: &std::path::Path, rel: &str, from: usize, to: usize, a: &Value, header: String) -> Result<String> {
    let Some(text) = crate::guard::read_text(&root.join(rel), rel, 16 << 20, false) else { bail!("cannot read {rel} (denied or binary)") };
    let all: Vec<&str> = text.lines().collect();
    let from = from.max(1);
    let to = to.min(all.len()).max(from);
    let mut lines = vec![format!("{header} {rel}:{from}-{to} of {}", all.len())];
    for (i, l) in all.iter().enumerate().take(to).skip(from - 1) {
        lines.push(format!("{}\t{}", i + 1, query::clip(l, 240)));
    }
    Ok(query::page(&lines, arg_n(a, "cursor").unwrap_or(0), arg_n(a, "budget").unwrap_or(DEFAULT_BUDGET)))
}

fn parse_range(s: &str) -> Option<(usize, usize)> {
    let (a, b) = s.split_once('-').unwrap_or((s, s));
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// An empty answer from a project whose first index is still running says so: the index is
/// filling in the background, so grep is the tool until it finishes.
pub fn while_indexing(p: Option<&project::Project>, out: String) -> String {
    let empty = out.starts_with("no matches for ") || out.starts_with("no symbol matches ");
    if empty && p.is_some_and(|p| p.indexed_at.is_none()) {
        return format!("foxdb is indexing this project now; use Grep and Read until it finishes ({})\n", out.trim_end());
    }
    out
}

pub fn call_tool(name: &str, a: &Value) -> Result<String> {
    let budget = arg_n(a, "budget").unwrap_or(DEFAULT_BUDGET);
    let cursor = arg_n(a, "cursor").unwrap_or(0);
    let ro = db::open_ro();
    match name {
        "fox_find" => {
            let q = arg_s(a, "q").ok_or_else(|| anyhow::anyhow!("q is required"))?;
            let c = match ro {
                Some(c) => c,
                None => db::open_rw()?,
            };
            let cur = match arg_s(a, "project") {
                Some("all") => None,
                Some(n) => named_project(&c, n),
                None => current_project(Some(&c)),
            };
            let all = arg_s(a, "project") == Some("all") || cur.is_none();
            let o = query::FindOpts { kind: arg_s(a, "kind").unwrap_or("any"), all, budget, limit: FIND_ROWS, offset: cursor };
            let mut w = warm().lock().unwrap_or_else(|e| e.into_inner());
            Ok(while_indexing(cur.as_ref(), crate::search::find_warm(&mut w, &c, cur.as_ref(), q, &o)?))
        }
        "fox_graph" => {
            let Some(c) = ro else { bail!("no store yet; run `fox index`") };
            let Some(p) = current_project(Some(&c)) else { bail!("not in a project") };
            let rel = arg_s(a, "rel").unwrap_or("callers");
            let o = code::CodeOpts { rel, depth: arg_n(a, "depth").unwrap_or(2), budget, cursor, fallback: false };
            Ok(while_indexing(Some(&p), code::run(&c, &p, arg_s(a, "target").unwrap_or(""), &o)?))
        }
        // No "already sent" references here: this one process serves the session and all its
        // subagents, and outlives compactions, so it cannot know what a caller still holds.
        "fox_read" => read_tool(ro, a, budget, cursor),
        "fox_memory" => memory_tool(ro, a, budget, cursor),
        n => bail!("unknown tool {n}"),
    }
}

fn read_tool(ro: Option<Connection>, a: &Value, budget: usize, cursor: usize) -> Result<String> {
    {
        {
            let Some(c) = ro else { bail!("no store yet; run `fox index`") };
            let t = arg_s(a, "target").ok_or_else(|| anyhow::anyhow!("target is required"))?;
            let mode = arg_s(a, "mode").unwrap_or("");
            let range = arg_s(a, "lines").and_then(parse_range);
            if let Some(id) = crate::handoff::parse_handle(t) {
                return crate::handoff::show(&c, id);
            }
            if t.starts_with("x:")
                && let Some(id) = t.strip_prefix("x:").and_then(|h| i64::from_str_radix(h, 16).ok())
                && let Ok((name, path, meta)) =
                    c.query_row("SELECT name, path, meta FROM node WHERE id = ?1 AND kind = 'session'", [id], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                    })
            {
                return Ok(crate::sessions::render_digest(&name, &path, &meta));
            }
            if let Some(n) = node_by_handle(&c, t)
                && !t.starts_with("s:")
                && !t.starts_with("c:")
            {
                let rel = n.path.clone().unwrap_or_default();
                return match n.kind.as_str() {
                    "doc_section" | "milestone" => {
                        let to = if n.end_line > 0 { n.end_line } else { n.line + 80 };
                        let (f, e) = range.unwrap_or((n.line as usize, to as usize));
                        file_lines(&n.root, &rel, f, e, a, format!("§ {}", n.name))
                    }
                    "file" if mode == "outline" || mode.is_empty() && range.is_none() => {
                        let p = project::Project {
                            id: n.project_id,
                            name: String::new(),
                            root: n.root.clone(),
                            kind: String::new(),
                            indexed_at: None,
                        };
                        let o = code::CodeOpts { rel: "outline", depth: 1, budget, cursor, fallback: false };
                        code::run(&c, &p, &rel, &o)
                    }
                    "file" => {
                        let (f, e) = range.unwrap_or((1, 120));
                        file_lines(&n.root, &rel, f, e, a, "file".into())
                    }
                    "memory" => {
                        let (f, e) = range.unwrap_or((1, 60));
                        if rel.starts_with("~note") {
                            Ok(format!("{}\n", n.name))
                        } else {
                            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default().join(".claude/projects");
                            file_lines(&home, &rel, f, e, a, "memory".into())
                        }
                    }
                    k => Ok(format!("{t}: {k} {} {}:{}\n", n.name, rel, n.line)),
                };
            }
            let Some(p) = current_project(Some(&c)) else { bail!("not in a project") };
            if p.root.join(t).is_file()
                && !mode.eq("outline")
                && let Some((f, e)) = range
            {
                return file_lines(&p.root, t, f, e, a, "file".into());
            }
            // No mode: a path's declarations, a symbol's body.
            let is_path = t.contains('/') && crate::lang::for_path(t).is_some() || p.root.join(t).is_file();
            let rel = match mode {
                "" if is_path => "outline",
                "" => "src",
                "outline" => "outline",
                "body" | "signature" => "src",
                "section" => "src",
                "context" => "callers",
                m => bail!("unknown mode {m:?}"),
            };
            let o = code::CodeOpts {
                rel,
                depth: 1,
                budget: if mode == "signature" {
                    64
                } else if mode == "context" {
                    budget.min(400)
                } else {
                    budget
                },
                cursor,
                fallback: false,
            };
            let mut out = code::run(&c, &p, t, &o)?;
            if mode == "signature" {
                out = out.lines().take(3).collect::<Vec<_>>().join("\n") + "\n";
            }
            if mode == "context" {
                let o2 = code::CodeOpts { rel: "src", depth: 1, budget: 160, cursor: 0, fallback: false };
                let head = code::run(&c, &p, t, &o2).unwrap_or_default();
                let head: Vec<&str> = head.lines().take(8).collect();
                out = format!("{}\n{}", head.join("\n"), out);
            }
            Ok(out)
        }
    }
}

fn memory_tool(ro: Option<Connection>, a: &Value, budget: usize, cursor: usize) -> Result<String> {
    {
        let op = arg_s(a, "op").unwrap_or("brief");
        match op {
            "brief" => {
                let Some(c) = ro else { return Ok("foxdb: no store yet\n".into()) };
                let loc = project::locate(&std::env::current_dir()?);
                let Some(p) = project::find(&c, &loc)? else { return Ok("foxdb: project not indexed yet\n".into()) };
                query::brief(
                    &c,
                    &p,
                    loc.git_dir.as_deref().and_then(project::branch).as_deref(),
                    budget.clamp(64, crate::hook::FULL_BRIEF_TOKENS),
                )
            }
            "recall" => {
                let q = arg_s(a, "q").ok_or_else(|| anyhow::anyhow!("q is required for recall"))?;
                let Some(c) = ro else { bail!("no store yet") };
                let cur = current_project(Some(&c));
                let o = query::FindOpts { kind: "recall", all: false, budget, limit: FIND_ROWS, offset: cursor };
                let mut w = warm().lock().unwrap_or_else(|e| e.into_inner());
                crate::search::find_warm(&mut w, &c, cur.as_ref(), q, &o)
            }
            "note" => {
                let text = arg_s(a, "text").ok_or_else(|| anyhow::anyhow!("text is required for note"))?;
                let text = redact::redact(&text.chars().take(2000).collect::<String>());
                let w = db::open_rw()?;
                let Some(p) = current_project(None) else { bail!("not in a project") };
                let id = save_note(&w, p.id, &text)?;
                Ok(format!(
                    "saved {} ({} chars){}\n",
                    query::handle("memory", id),
                    text.len(),
                    if text.contains("[redacted]") { ", secrets redacted" } else { "" }
                ))
            }
            "post" => {
                let text = arg_s(a, "text").ok_or_else(|| anyhow::anyhow!("text is required for post"))?;
                let w = db::open_rw()?;
                let Some(p) = current_project(None) else { bail!("not in a project") };
                let client = client_name();
                let who = agent_id(a);
                let sess = std::env::var("FOXDB_SESSION").ok();
                let id = crate::handoff::post(
                    &w,
                    &crate::handoff::Post {
                        project_id: p.id,
                        kind: arg_s(a, "kind").unwrap_or("finding"),
                        text,
                        title: arg_s(a, "title"),
                        evidence: arg_s(a, "evidence"),
                        scope: arg_s(a, "scope"),
                        harness: client.as_deref(),
                        agent: who.as_deref(),
                        topic: arg_s(a, "topic"),
                        session: sess.as_deref(),
                        ..Default::default()
                    },
                )?;
                Ok(format!("posted {}\n", crate::handoff::handle(id)))
            }
            "read" => {
                let Some(c) = ro else { bail!("no store yet") };
                let Some(p) = current_project(Some(&c)) else { bail!("not in a project") };
                match arg_s(a, "since") {
                    Some(s) => {
                        let w = db::open_rw()?;
                        let me = agent_id(a).unwrap_or_else(|| "agent".into());
                        let since = match crate::handoff::parse_handle(s) {
                            Some(n) => crate::handoff::Since::After(n),
                            None => crate::handoff::Since::Auto(&me),
                        };
                        crate::handoff::read_since(&w, p.id, since, arg_s(a, "kind"), arg_s(a, "topic"), budget)
                    }
                    None => crate::handoff::read(&c, p.id, arg_s(a, "kind"), arg_s(a, "q"), budget, cursor),
                }
            }
            "pickup" | "take" => {
                let id = arg_s(a, "id").and_then(crate::handoff::parse_handle).ok_or_else(|| anyhow::anyhow!("id (h:...) is required"))?;
                let w = db::open_rw()?;
                let by = agent_id(a).unwrap_or_else(|| "agent".into());
                let lease = (op == "take").then_some(TAKE_LEASE_SECS);
                crate::handoff::take(&w, id, &by, lease)
            }
            "packet" => {
                if let Some(id) = arg_s(a, "id").and_then(crate::handoff::parse_handle) {
                    let Some(c) = ro else { bail!("no store yet") };
                    return crate::packet::render(&c, id, crate::packet::cap_for(harness_of_client().as_deref(), arg_n(a, "budget")));
                }
                let w = db::open_rw()?;
                let Some(p) = current_project(Some(&w)) else { bail!("not in a project") };
                let who = agent_id(a);
                let hc = harness_of_client();
                let spec = crate::packet::Spec {
                    goal: arg_s(a, "text"),
                    next: arg_s(a, "next").into_iter().collect(),
                    agent: who.as_deref(),
                    harness: hc.as_deref(),
                    parent: arg_s(a, "parent").and_then(crate::handoff::parse_handle),
                    ..Default::default()
                };
                let id = crate::packet::create(&w, &p, &spec)?;
                let n = crate::packet::render(&w, id, usize::MAX)?.len().div_ceil(4);
                Ok(format!("packet {} (~{n} tokens)\n", crate::handoff::handle(id)))
            }
            "agents" => {
                let w = db::open_rw()?;
                crate::agents::refresh(&w)?;
                let out = crate::agents::list(&w, true, false)?;
                let lines: Vec<String> = out.lines().map(String::from).collect();
                Ok(query::page(&lines, cursor, budget))
            }
            "dispatch" => {
                let task = arg_s(a, "text").ok_or_else(|| anyhow::anyhow!("text (the task, or h:) is required"))?;
                let cwd = std::env::current_dir()?;
                let me = agent_id(a);
                let started = crate::orch::dispatch(&crate::orch::Dispatch {
                    harness: arg_s(a, "harness").unwrap_or("codex"),
                    task,
                    repo: &cwd,
                    writer: a.get("writer").and_then(Value::as_bool).unwrap_or(false),
                    label: arg_s(a, "label"),
                    model: arg_s(a, "model"),
                    packet: arg_s(a, "packet"),
                    parent: me.as_deref(),
                    reviewer: a.get("reviewer").and_then(Value::as_bool).unwrap_or(false),
                })?;
                // This server lives on; reap the supervisor when it exits.
                let mut child = started.child;
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                Ok(started.line + "\n")
            }
            "wait" => {
                let ids: Vec<String> = arg_s(a, "id").unwrap_or("").split([' ', ',']).filter(|s| !s.is_empty()).map(String::from).collect();
                let secs = a.get("timeout").and_then(Value::as_u64).unwrap_or(45).min(WAIT_MAX_SECS);
                let any = a.get("any").and_then(Value::as_bool).unwrap_or(false);
                let r = crate::orch::wait(&ids, any, std::time::Duration::from_secs(secs))?;
                let (met, out) = (r.met, r.text);
                if met {
                    Ok(out)
                } else {
                    Ok(format!(
                        "{out}still running: call op=wait again, or run `fox wait {}` as a background shell command\n",
                        ids.join(" ")
                    ))
                }
            }
            o => bail!("unknown op {o:?}"),
        }
    }
}

/// Lease on a handoff taken through MCP; the taker renews by taking again.
pub const TAKE_LEASE_SECS: i64 = 1800;

/// "claude" or "codex" for the connected client, for render caps.
fn harness_of_client() -> Option<String> {
    let n = client_name()?.to_ascii_lowercase();
    if n.contains("codex") {
        Some("codex".into())
    } else if n.contains("claude") {
        Some("claude".into())
    } else {
        None
    }
}

pub fn save_note(w: &Connection, pid: i64, text: &str) -> Result<i64> {
    let now = db::now();
    let date = crate::mcp::ymd(now);
    let title: String = text.lines().next().unwrap_or("").chars().take(70).collect();
    let key = format!("note:{pid}:{now}:{}", text.len());
    w.execute(
        "INSERT OR REPLACE INTO node(project_id, kind, key, name, path, line, tier, source, meta, updated_at) VALUES (?1,'memory',?2,?3,?4,1,2,'note',?5,?6)",
        params![pid, key, title, format!("~note/{date}"), json!({"text": text, "ts": now}).to_string(), now],
    )?;
    let id = w.last_insert_rowid();
    w.execute(
        "INSERT INTO fts_node(rowid, name, sig, path, body) VALUES (?1,?2,'note',?3,?4)",
        params![id, crate::inventory::split_ident(&title), format!("~note/{date}"), text],
    )?;
    w.execute("INSERT INTO fts_trigram(rowid, path, name) VALUES (?1,?2,?3)", params![id, format!("~note/{date}"), title])?;
    let sink = crate::outbox::Sink::open(w);
    if sink.on(pid) {
        let path = format!("~note/{date}");
        sink.put(w, pid, crate::outbox::TBL_NODE, id, &[&crate::inventory::split_ident(&title), "note", &path, text])?;
        sink.put(w, pid, crate::outbox::TBL_TRI, id, &[&path, &title])?;
    }
    db::bump_generation(w)?;
    Ok(id)
}

pub fn ymd(ts: i64) -> String {
    let days = ts.div_euclid(86400);
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

/// Handle one JSON-RPC message; None for notifications.
pub fn handle(msg: &Value) -> Option<Value> {
    let method = msg.get("method")?.as_str()?;
    let id = msg.get("id").cloned();
    let reply = |result: Value| id.clone().map(|id| json!({"jsonrpc": "2.0", "id": id, "result": result}));
    let err = |code: i64, m: &str| id.clone().map(|id| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": m}}));
    match method {
        "initialize" => {
            let asked = msg["params"]["protocolVersion"].as_str().unwrap_or("");
            let v = PROTOCOL_VERSIONS.iter().find(|p| **p == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
            if let Some(n) = msg["params"]["clientInfo"]["name"].as_str() {
                let _ = CLIENT.set(n.chars().take(40).collect());
            }
            reply(
                json!({"protocolVersion": v, "capabilities": {"tools": {}}, "serverInfo": {"name": "foxdb", "version": env!("CARGO_PKG_VERSION")}, "instructions": INSTRUCTIONS}),
            )
        }
        "ping" => reply(json!({})),
        "tools/list" => reply(json!({"tools": tools()})),
        "tools/call" => {
            let name = msg["params"]["name"].as_str().unwrap_or("");
            let args = msg["params"].get("arguments").cloned().unwrap_or(json!({}));
            let (text, is_err) = match std::panic::catch_unwind(|| call_tool(name, &args)) {
                // Recorded for `fox stats`; the answer itself is returned unchanged.
                Ok(Ok(t)) if name != "fox_memory" => (crate::savings::account(cwd_project_id(), name, t), false),
                Ok(Ok(t)) => (t, false),
                Ok(Err(e)) => (format!("fox: {e:#}"), true),
                Err(_) => ("fox: internal error".to_string(), true),
            };
            // Text only, never `structuredContent` (see `tools`).
            reply(json!({"content": [{"type": "text", "text": text}], "isError": is_err}))
        }
        m if m.starts_with("notifications/") => None,
        _ => err(-32601, "method not found"),
    }
}

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            writeln!(out, "{}", json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}))?;
            out.flush()?;
            continue;
        };
        if let Some(r) = handle(&msg) {
            writeln!(out, "{r}")?;
            out.flush()?;
        }
    }
    let _ = codanna::available;
    Ok(())
}
