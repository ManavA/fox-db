//! Orchestration: `fox dispatch` starts a `codex exec` or `claude -p` worker under a detached
//! supervisor, `fox wait` blocks until workers report, and `fox switch` prints the command that
//! continues a task in the other harness (or natively resumes it in the same one).
//!
//! The supervisor (`fox _supervise <id>`, in its own session) owns the child: it feeds the
//! prompt on stdin, keeps the child's JSON events, posts the final message as a `report`
//! handoff, marks the agent done or failed, frees its writer lease and touches
//! `$FOXDB_HOME/agents/<id>/done`. `fox wait` watches those files with inotify and falls back
//! to polling SQLite's `data_version`.
//!
//! Prompts are cache-friendly: one fixed brief that names the parent's packet handle (identical
//! for siblings), then the task. Nothing goes into a system prompt. Reviewers get the task only.
//!
//! Handoffs are verifiable: the worktree is recorded as a commit id when the worker starts and
//! when it ends (`gitsnap`), and the final answer must be a fixed-format report (`report`),
//! enforced by the harness's own schema flag and parsed again here. A malformed report fails the
//! worker; rule checks add short findings to a good one.

use crate::{agents, db, gitsnap, handoff, hook, packet, project, query, report};
use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Environment names a child's foxdb MCP server must see.
pub const ENV_NAMES: &[&str] = &["FOXDB_AGENT", "FOXDB_PARENT", "FOXDB_HOME", "FOXDB_SESSION"];

pub fn agents_dir() -> PathBuf {
    db::home().join("agents")
}

/// This `fox` binary (foxd and fox-bench sit next to it).
pub fn fox_exe() -> String {
    let mut exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fox"));
    if exe.file_name().is_some_and(|n| n != "fox") {
        exe.set_file_name("fox");
    }
    exe.to_string_lossy().into_owned()
}

fn harness_bin(h: &str) -> String {
    let var = if h == "codex" { "FOXDB_CODEX_BIN" } else { "FOXDB_CLAUDE_BIN" };
    std::env::var(var).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| h.to_string())
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// Codex per-process MCP config: no `config.toml` edit. Codex passes a server only the env it is
/// told to, so the identity variables are forwarded by name.
pub fn codex_mcp_flags(fox: &str) -> Vec<String> {
    let names: Vec<String> = ENV_NAMES.iter().map(|n| toml_str(n)).collect();
    vec![
        "-c".into(),
        format!("mcp_servers.foxdb.command={}", toml_str(fox)),
        "-c".into(),
        "mcp_servers.foxdb.args=[\"mcp\"]".into(),
        "-c".into(),
        format!("mcp_servers.foxdb.env_vars=[{}]", names.join(",")),
    ]
}

/// Claude `--mcp-config` JSON string for the foxdb server, with the identity env set explicitly.
pub fn claude_mcp_json(fox: &str, env: &[(&str, &str)]) -> String {
    let env: serde_json::Map<String, Value> = env.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
    json!({"mcpServers": {"foxdb": {"type": "stdio", "command": fox, "args": ["mcp"], "env": env}}}).to_string()
}

/// POSIX shell quoting for printed commands.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn uuid4() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    }
    if b == [0u8; 16] {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        b = (n ^ (std::process::id() as u128) << 64).to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// The fixed part of every dispatch prompt. It depends only on the parent's packet handle, so
/// siblings share it byte for byte and it can sit in the cached prefix.
pub fn dispatch_brief(packet: &str) -> String {
    format!(
        "You are a worker started by `fox dispatch`. Shared context for this task tree is in foxdb:\n\
         - parent packet {packet} - read it with fox_read target={packet} (or `fox handoff render {packet}`)\n\
         - live board: fox_memory op=read since=auto; post findings with fox_memory op=post\n\
         Your final answer is your report for the parent, in the JSON format the harness enforces: \
         status, summary, files (path:line), tests (exact commands run, passed), decisions, next.\n\n--- task ---\n"
    )
}

pub struct Dispatch<'a> {
    pub harness: &'a str,
    pub task: &'a str,
    pub repo: &'a Path,
    pub writer: bool,
    pub label: Option<&'a str>,
    pub model: Option<&'a str>,
    /// The parent's packet; default: the parent's board snapshot.
    pub packet: Option<&'a str>,
    /// The dispatching agent (`FOXDB_AGENT`), if any.
    pub parent: Option<&'a str>,
    /// Start a reviewer: the task alone, no brief, whatever the label.
    pub reviewer: bool,
}

pub struct Started {
    pub id: String,
    pub line: String,
    pub child: std::process::Child,
}

fn env_n(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// What a registered parent may start. The foxdb MCP server runs outside the caller's sandbox, so
/// these limits stand in for it: a reviewer starts nothing, a read-only worker no writer, and a
/// tree is at most `FOXDB_MAX_DEPTH` (default 3) deep with at most `FOXDB_MAX_CHILDREN`
/// (default 8) running children per parent.
fn guard(w: &Connection, parent: &str, writer: bool) -> Result<()> {
    let me = agents::get(w, parent);
    match me.as_ref().and_then(|a| a.role.as_deref()) {
        Some("reviewer") => bail!("{parent} is a reviewer: reviewers do not dispatch"),
        Some("worker") if writer => bail!("{parent} is a read-only worker: it cannot start a writer"),
        _ => {}
    }
    let mut depth = 1;
    let mut cur = me;
    while let Some(a) = cur {
        depth += 1;
        if depth > 16 {
            break;
        }
        cur = a.parent.as_deref().and_then(|p| agents::get(w, p));
    }
    let max = env_n("FOXDB_MAX_DEPTH", 3);
    if depth > max {
        bail!("dispatch depth {depth} would exceed FOXDB_MAX_DEPTH={max}");
    }
    agents::refresh(w)?;
    let running: usize =
        w.query_row("SELECT COUNT(*) FROM agent WHERE parent = ?1 AND status = 'running'", [parent], |r| r.get::<_, i64>(0))? as usize;
    let cap = env_n("FOXDB_MAX_CHILDREN", 8);
    if running >= cap {
        bail!("{parent} already has {running} running children (FOXDB_MAX_CHILDREN={cap}); fox wait for one first");
    }
    Ok(())
}

/// Register a worker, take the writer lease if asked, write its prompt and start its supervisor.
pub fn dispatch(d: &Dispatch) -> Result<Started> {
    if !matches!(d.harness, "codex" | "claude") {
        bail!("--harness must be codex or claude");
    }
    if d.task.trim().is_empty() {
        bail!("--task is empty");
    }
    let repo = std::fs::canonicalize(d.repo).with_context(|| format!("repo {}", d.repo.display()))?;
    let w = db::open_rw()?;
    if let Some(parent) = d.parent {
        guard(&w, parent, d.writer)?;
    }
    let loc = project::locate(&repo);
    let p = project::ensure(&w, &loc)?;
    let id = agents::next_id(&w, if d.harness == "codex" { "cx" } else { "cl" })?;
    let reviewer = d.reviewer || hook::skip_agent_with(crate::config::get(), d.label.unwrap_or(""), &id, Some(&repo));
    if reviewer && d.writer {
        bail!("a reviewer cannot hold the writer lease");
    }
    let task_h = handoff::parse_handle(d.task);
    let task_text = match task_h {
        Some(h) => {
            let shown = handoff::show(&w, h)?;
            let hs = handoff::handle(h);
            format!(
                "Handoff {hs}: take it first (fox_memory op=take id={hs}, or `fox handoff take {hs}`).\n{}",
                hook::cap_lines(&shown, 2_000)
            )
        }
        None => d.task.trim().to_string(),
    };
    let prompt = if reviewer {
        format!("{task_text}\n")
    } else {
        let pk = match d.packet {
            Some(h) => h.to_string(),
            None => {
                let owner = d.parent.map(String::from).unwrap_or_else(|| format!("controller:{}", crate::mcp::ymd(db::now())));
                handoff::handle(handoff::snapshot(&w, p.id, &owner, d.parent)?)
            }
        };
        format!("{}{task_text}\n", dispatch_brief(&pk))
    };
    let session = (d.harness == "claude").then(uuid4);
    let role = if reviewer {
        "reviewer"
    } else if d.writer {
        "writer"
    } else {
        "worker"
    };
    let repo_s = repo.to_string_lossy().into_owned();
    agents::register(
        &w,
        &agents::Reg {
            id: &id,
            harness: d.harness,
            model: d.model,
            role: Some(role),
            label: d.label,
            parent: d.parent,
            project_id: Some(p.id),
            cwd: Some(&repo_s),
            session: session.as_deref(),
            task: Some(d.task),
            writer: d.writer,
            ..Default::default()
        },
    )?;
    let key = agents::repo_key(&repo);
    if d.writer
        && let Err(holder) = agents::lease_take(&w, &key, &id, None)?
    {
        w.execute("DELETE FROM agent WHERE id = ?1", [&id])?;
        bail!("writer lease on {key} held by {holder}");
    }
    // The tree the worker starts from, so its report can cite an exact `git diff base head`.
    if let Some(base) = gitsnap::snapshot(&repo) {
        agents::set_commit(&w, &id, Some(&base), None)?;
    }
    let dir = agents_dir().join(&id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("prompt.txt"), &prompt)?;
    std::fs::write(dir.join("report.schema.json"), report::SCHEMA)?;
    let log = std::fs::File::create(dir.join("supervise.log"))?;
    let mut cmd = Command::new(fox_exe());
    cmd.args(["_supervise", &id]).current_dir(&repo).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe; the supervisor outlives a closed terminal.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            agents::finish(&w, &id, "failed", None)?;
            bail!("cannot start the supervisor: {e}");
        }
    };
    let pid = child.id() as i64;
    agents::set_pid(&w, &id, pid)?;
    if d.writer {
        agents::lease_set_pid(&w, &id, pid)?;
    }
    db::bump_generation(&w)?;
    let line = format!(
        "{id} dispatched {} {role}{}  (fox wait {id})",
        d.harness,
        task_h.map(|h| format!(" for {}", handoff::handle(h))).unwrap_or_default()
    );
    Ok(Started { id, line, child })
}

/// The child command for a dispatched worker.
fn child_command(a: &agents::Agent, dir: &Path) -> (String, Vec<String>) {
    let fox = fox_exe();
    let repo = a.cwd.clone().unwrap_or_else(|| ".".into());
    if a.harness == "codex" {
        let mut v: Vec<String> = vec!["exec".into(), "--json".into(), "-o".into(), dir.join("last.txt").to_string_lossy().into_owned()];
        v.extend(["--output-schema".into(), dir.join("report.schema.json").to_string_lossy().into_owned()]);
        v.extend(["-C".into(), repo.clone()]);
        v.extend(["-s".into(), if a.writer { "workspace-write" } else { "read-only" }.into()]);
        if project::locate(Path::new(&repo)).git_dir.is_none() {
            v.push("--skip-git-repo-check".into());
        }
        v.extend(codex_mcp_flags(&fox));
        if let Some(m) = &a.model {
            v.extend(["-m".into(), m.clone()]);
        }
        v.push("-".into());
        (harness_bin("codex"), v)
    } else {
        let mut env: Vec<(&str, &str)> = vec![("FOXDB_AGENT", &a.id), ("FOXDB_SESSION", &a.id)];
        if let Some(p) = &a.parent {
            env.push(("FOXDB_PARENT", p));
        }
        let home = db::home().to_string_lossy().into_owned();
        env.push(("FOXDB_HOME", &home));
        let mut v: Vec<String> = vec!["-p".into(), "--output-format".into(), "json".into()];
        v.extend(["--json-schema".into(), report::SCHEMA.into()]);
        if let Some(s) = &a.session {
            v.extend(["--session-id".into(), s.clone()]);
        }
        v.extend(["--mcp-config".into(), claude_mcp_json(&fox, &env)]);
        v.extend(["--allowedTools".into(), "mcp__foxdb__*".into()]);
        v.push("--exclude-dynamic-system-prompt-sections".into());
        if a.writer {
            v.extend(["--permission-mode".into(), "acceptEdits".into()]);
        }
        if let Some(m) = &a.model {
            v.extend(["--model".into(), m.clone()]);
        }
        (harness_bin("claude"), v)
    }
}

/// `fox _supervise <id>`: run the worker to completion and record its report.
pub fn supervise(id: &str) -> Result<()> {
    let w = db::open_rw()?;
    let Some(a) = agents::get(&w, id) else { bail!("no agent {id}") };
    let dir = agents_dir().join(id);
    let prompt = std::fs::read_to_string(dir.join("prompt.txt")).unwrap_or_default();
    let (bin, args) = child_command(&a, &dir);
    let out = std::fs::File::create(dir.join("out.jsonl"))?;
    let err = std::fs::File::create(dir.join("err.log"))?;
    let mut cmd = Command::new(&bin);
    cmd.args(&args)
        .current_dir(a.cwd.as_deref().unwrap_or("."))
        .env("FOXDB_AGENT", id)
        .env("FOXDB_SESSION", id)
        .env("FOXDB_HOME", db::home())
        .env("FOXDB_HARNESS", &a.harness)
        // The worker is not the controller's Claude session.
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .stdin(Stdio::piped())
        .stdout(out)
        .stderr(err);
    match &a.parent {
        Some(p) => cmd.env("FOXDB_PARENT", p),
        None => cmd.env_remove("FOXDB_PARENT"),
    };
    let (status, mut final_text, thread, session) = match cmd.spawn() {
        Err(e) => ("failed", format!("dispatch failed: cannot start {bin}: {e}"), None, None),
        Ok(mut child) => {
            agents::set_resume(&w, id, None, None, Some(child.id() as i64))?;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(prompt.as_bytes());
            }
            let st = child.wait()?;
            let (text, thread, session, is_err) = collect(&a.harness, &dir);
            if text.trim().is_empty() {
                let tail = std::fs::read_to_string(dir.join("err.log")).unwrap_or_default();
                let tail: Vec<&str> = tail.lines().rev().take(3).collect();
                let msg = format!(
                    "(no final message; {bin} exit {}) {}",
                    st.code().unwrap_or(-1),
                    tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
                );
                ("failed", msg, thread, session)
            } else {
                match report::parse(&text) {
                    Err(why) => ("failed", format!("malformed report: {why}\n{}", hook::cap_lines(&text, 1_500)), thread, session),
                    Ok(r) => {
                        let (text, checked) = finish_report(&a, &dir, &r);
                        let status = if !st.success() || is_err {
                            "failed"
                        } else if r.status != "done" {
                            if r.status == "blocked" { "blocked" } else { "partial" }
                        } else if checked {
                            "flagged"
                        } else {
                            "done"
                        };
                        (status, text, thread, session)
                    }
                }
            }
        }
    };
    // Every finished worker records where it left the tree, failed or not, so its work is
    // addressable; a failure's report names the change too.
    if let Some(head) = a.base_commit.as_ref().and_then(|_| gitsnap::snapshot(&PathBuf::from(a.cwd.as_deref().unwrap_or(".")))) {
        let _ = agents::set_commit(&w, id, None, Some(&head));
        let line = format!("change: git diff {} {head}", a.base_commit.as_deref().unwrap_or_default());
        if !final_text.contains(&line) {
            final_text = format!("{}\n{line}\n", final_text.trim_end());
        }
    }
    agents::set_resume(&w, id, session.as_deref(), thread.as_deref(), None)?;
    let task_h = a.task.as_deref().and_then(handoff::parse_handle);
    // The report and the agent's end status land together: a reader never sees a failed
    // worker's report before its status says so.
    w.execute_batch("BEGIN IMMEDIATE")?;
    let report = handoff::post(
        &w,
        &handoff::Post {
            project_id: a.project_id.unwrap_or(0),
            kind: "report",
            text: &final_text,
            harness: Some(&a.harness),
            session: thread.as_deref().or(session.as_deref()).or(a.session.as_deref()),
            agent: Some(id),
            evidence: Some(&dir.join("out.jsonl").to_string_lossy()),
            topic: a.label.as_deref(),
            parent: task_h,
            ..Default::default()
        },
    );
    let report = match report.and_then(|r| agents::finish(&w, id, status, Some(r)).map(|_| r)) {
        Ok(r) => {
            w.execute_batch("COMMIT")?;
            r
        }
        Err(e) => {
            let _ = w.execute_batch("ROLLBACK");
            return Err(e);
        }
    };
    let tmp = dir.join("done.tmp");
    std::fs::write(&tmp, format!("{status} {}\n", handoff::handle(report)))?;
    std::fs::rename(tmp, dir.join("done"))?;
    Ok(())
}

/// Render a worker's report with the exact change and the checks; true when a check fired.
fn finish_report(a: &agents::Agent, dir: &Path, r: &report::Report) -> (String, bool) {
    let repo = PathBuf::from(a.cwd.as_deref().unwrap_or("."));
    let head = a.base_commit.as_ref().and_then(|_| gitsnap::snapshot(&repo));
    let change = a.base_commit.as_deref().zip(head.as_deref());
    let changed = change.and_then(|(b, h)| gitsnap::changed(&repo, b, h));
    let events = std::fs::read_to_string(dir.join("out.jsonl")).unwrap_or_default();
    let events = (a.harness == "codex").then_some(events.as_str());
    let found = report::checks(r, &report::Ctx { writer: a.writer, changed: changed.as_deref(), events });
    (report::render(r, change, &found), !found.is_empty())
}

/// Final message, Codex thread id, Claude session id, and whether the harness flagged an error.
fn collect(harness: &str, dir: &Path) -> (String, Option<String>, Option<String>, bool) {
    let lines: Vec<Value> = std::fs::File::open(dir.join("out.jsonl"))
        .map(|f| std::io::BufReader::new(f).lines().map_while(|l| l.ok()).filter_map(|l| serde_json::from_str(&l).ok()).collect())
        .unwrap_or_default();
    if harness == "codex" {
        let thread = lines.iter().find(|v| v["type"] == "thread.started").and_then(|v| v["thread_id"].as_str()).map(String::from);
        let text = std::fs::read_to_string(dir.join("last.txt")).unwrap_or_default();
        let failed = lines.iter().any(|v| v["type"] == "turn.failed");
        (text.trim().to_string(), thread, None, failed)
    } else {
        let r = lines.iter().rev().find(|v| v["type"] == "result");
        // With `--json-schema` the validated object is `structured_output`; else the text.
        let text = match r.map(|v| &v["structured_output"]).filter(|so| so.is_object()) {
            Some(so) => so.to_string(),
            None => r.and_then(|v| v["result"].as_str()).unwrap_or_default().trim().to_string(),
        };
        let sid = r.and_then(|v| v["session_id"].as_str()).map(String::from);
        let is_err = r.is_some_and(|v| v["is_error"] == true);
        (text, None, sid, is_err)
    }
}

enum Target {
    Agent(String),
    Handoff(i64),
}

/// One status line per target: (finished, finished without a good report, line).
fn check(c: &Connection, t: &Target) -> Result<(bool, bool, String)> {
    match t {
        Target::Agent(id) => {
            let Some(a) = agents::get(c, id) else { bail!("no agent {id}") };
            let fin = a.status != "running";
            let bad = fin && a.status != "done";
            let mut l = format!("{id} {}", a.status);
            if let Some(r) = a.report {
                let (title, body): (String, String) =
                    c.query_row("SELECT title, body FROM handoff WHERE id = ?1", [r], |x| Ok((x.get(0)?, x.get(1)?))).unwrap_or_default();
                let n = body.lines().filter(|l| l.starts_with("check: ")).count();
                if n > 0 {
                    let _ = write!(l, " ({n} checks)");
                }
                let _ = write!(l, " report {}  {}", handoff::handle(r), query::clip(&title, 80));
            } else if a.status == "stale" {
                let _ = write!(l, " (process gone; continue with `fox handoff create --agent {id}`)");
            }
            Ok((fin, bad, l))
        }
        Target::Handoff(h) => {
            let r: Option<(i64, String, String)> = handoff::done_report(c, *h)?.and_then(|id| {
                c.query_row("SELECT id, COALESCE(agent, harness, '-'), title FROM handoff WHERE id = ?1", [id], |x| {
                    Ok((x.get(0)?, x.get(1)?, x.get(2)?))
                })
                .ok()
            });
            if let Some((id, who, title)) = r {
                return Ok((
                    true,
                    false,
                    format!("{} report {} by {who}  {}", handoff::handle(*h), handoff::handle(id), query::clip(&title, 80)),
                ));
            }
            // No done report yet: finished anyway once every worker dispatched for it has ended
            // (failed, blocked, partial, flagged or stale). A retry still running is waited for.
            let hs = handoff::handle(*h);
            let ws: Vec<(String, String, Option<i64>)> = {
                let mut s = c.prepare_cached("SELECT id, status, report FROM agent WHERE lower(trim(task)) = ?1 ORDER BY started_at")?;
                s.query_map([&hs], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.flatten().collect()
            };
            if !ws.is_empty() && ws.iter().all(|(_, st, _)| st != "running") {
                let who: Vec<String> = ws
                    .iter()
                    .map(|(i, st, r)| match r {
                        Some(r) => format!("{i} {st} report {}", handoff::handle(*r)),
                        None => format!("{i} {st}"),
                    })
                    .collect();
                return Ok((true, true, format!("{hs} no done report; {}", who.join(", "))));
            }
            Ok((false, false, format!("{hs} waiting for a report")))
        }
    }
}

/// How a wait ended.
pub struct Waited {
    /// All (or, with `any`, one) of the targets finished.
    pub met: bool,
    /// A finished target went stale or failed, or ended without a report.
    pub bad: bool,
    pub text: String,
}

/// Block until all (or any) targets finish, or the timeout. Returns one line per target.
/// Wakes on `agents/*/done` writes (inotify) and on SQLite `data_version` changes; checks
/// process liveness about once a second so a dead worker turns `stale` instead of hanging.
pub fn wait(targets: &[String], any: bool, timeout: Duration) -> Result<Waited> {
    if targets.is_empty() {
        bail!("wait needs agent ids or h: handles");
    }
    let ts: Vec<Target> =
        targets.iter().map(|t| handoff::parse_handle(t).map(Target::Handoff).unwrap_or_else(|| Target::Agent(t.clone()))).collect();
    let w = db::open_rw()?;
    let dir = agents_dir();
    let _ = std::fs::create_dir_all(&dir);
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    use notify::Watcher;
    let watcher = notify::recommended_watcher(move |_: notify::Result<notify::Event>| {
        let _ = tx.send(());
    })
    .ok()
    .and_then(|mut wt| wt.watch(&dir, notify::RecursiveMode::Recursive).ok().map(|_| wt));
    let tick = if watcher.is_some() { Duration::from_millis(1000) } else { Duration::from_millis(250) };
    let start = Instant::now();
    let mut last_dv: i64 = -1;
    let mut last_live: Option<Instant> = None;
    let mut dirty = true;
    loop {
        if last_live.is_none_or(|t| t.elapsed() >= Duration::from_millis(900)) {
            dirty |= agents::refresh(&w)? > 0;
            last_live = Some(Instant::now());
        }
        let dv: i64 = w.query_row("PRAGMA data_version", [], |r| r.get(0)).unwrap_or(0);
        dirty |= dv != last_dv;
        last_dv = dv;
        let elapsed = start.elapsed();
        if dirty || elapsed >= timeout {
            dirty = false;
            let rs: Vec<(bool, bool, String)> = ts.iter().map(|t| check(&w, t)).collect::<Result<_>>()?;
            let done = rs.iter().filter(|r| r.0).count();
            let met = if any { done > 0 } else { done == rs.len() };
            if met || elapsed >= timeout {
                let bad = rs.iter().any(|r| r.0 && r.1);
                let text: String = rs.into_iter().map(|(_, _, l)| l + "\n").collect();
                return Ok(Waited { met, bad, text });
            }
        }
        if rx.recv_timeout(tick.min(timeout - elapsed)).is_ok() {
            dirty = true;
            while rx.try_recv().is_ok() {}
        }
    }
}

fn harness_of(a: &agents::Agent) -> &str {
    a.harness.as_str()
}

/// A dispatched Codex worker's thread id from its event log (the first event names it, long
/// before the supervisor records it at exit).
pub fn codex_thread(id: &str) -> Option<String> {
    let f = std::fs::File::open(agents_dir().join(id).join("out.jsonl")).ok()?;
    std::io::BufReader::new(f)
        .lines()
        .map_while(|l| l.ok())
        .take(50)
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .find(|v| v["type"] == "thread.started")
        .and_then(|v| v["thread_id"].as_str().map(String::from))
}

/// A dispatched worker's last message so far: Codex's `-o` file, else its last agent message or
/// Claude's result in the event log.
pub fn last_output(id: &str) -> Option<String> {
    let dir = agents_dir().join(id);
    if let Ok(t) = std::fs::read_to_string(dir.join("last.txt"))
        && !t.trim().is_empty()
    {
        return Some(match report::parse(&t) {
            Ok(r) => report::render(&r, None, &[]).lines().next().unwrap_or_default().to_string(),
            Err(_) => t.trim().to_string(),
        });
    }
    let f = std::fs::File::open(dir.join("out.jsonl")).ok()?;
    let mut last = None;
    for v in std::io::BufReader::new(f).lines().map_while(|l| l.ok()).filter_map(|l| serde_json::from_str::<Value>(&l).ok()) {
        let t = if v["item"]["type"] == "agent_message" { v["item"]["text"].as_str() } else { v["result"].as_str() };
        if let Some(t) = t.filter(|t| !t.trim().is_empty()) {
            last = Some(t.trim().to_string());
        }
    }
    last
}

pub struct Switch<'a> {
    pub to: &'a str,
    pub from: Option<&'a str>,
    pub handoff: Option<&'a str>,
    pub agent: Option<&'a str>,
    pub session: Option<&'a str>,
    pub transcript: Option<&'a Path>,
    pub inline: bool,
    pub cwd: &'a Path,
    /// The command will be exec'd by this process: register the new session under its pid.
    pub exec: bool,
    /// `--for t3`: print the message to paste into a T3 Code thread (T3 starts the provider and
    /// carries the conversation), with the packet capped for T3, instead of a harness command.
    pub for_t3: bool,
}

/// The command that continues the work, as argv plus env, and the printable text.
pub struct SwitchCmd {
    pub env: Vec<(String, String)>,
    pub argv: Vec<String>,
    pub text: String,
}

/// `fox switch`: same harness, print its native resume; across harnesses, print a command that
/// starts the other harness with the foxdb MCP and the packet referenced (or `--inline`). The
/// new session gets its own `FOXDB_AGENT`, with the source as `FOXDB_PARENT`.
pub fn switch(s: &Switch) -> Result<SwitchCmd> {
    if !s.for_t3 && !matches!(s.to, "codex" | "claude") {
        bail!("--to must be codex or claude");
    }
    let w = db::open_rw()?;
    let agent_id = s.agent.map(String::from).or_else(|| std::env::var("FOXDB_AGENT").ok().filter(|x| !x.is_empty()));
    let agent = agent_id.as_deref().and_then(|id| agents::get(&w, id));
    let digest = s.transcript.and_then(packet::digest);
    let from: Option<String> = s
        .from
        .map(String::from)
        .or_else(|| agent.as_ref().map(|a| harness_of(a).to_string()))
        .or_else(|| std::env::var("FOXDB_HARNESS").ok().filter(|x| !x.is_empty()))
        .or_else(|| digest.as_ref().map(|d| d.harness.to_string()));
    if !s.for_t3 && from.as_deref() == Some(s.to) && s.handoff.is_none() && s.transcript.is_none() {
        let sid = s.session.map(String::from).or_else(|| {
            agent.as_ref().and_then(|a| {
                a.thread.clone().or_else(|| if a.harness == "codex" { codex_thread(&a.id) } else { None }).or(a.session.clone())
            })
        });
        // A known agent with no resumable id falls through to a packet: `--last` would resume
        // whichever session ran last, not this one.
        if sid.is_some() || agent.is_none() {
            let argv: Vec<String> = match (s.to, sid) {
                ("claude", Some(id)) => vec!["claude".into(), "--resume".into(), id],
                ("claude", None) => vec!["claude".into(), "--continue".into()],
                (_, Some(id)) => vec!["codex".into(), "resume".into(), id],
                (_, None) => vec!["codex".into(), "resume".into(), "--last".into()],
            };
            let mut text = argv.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ");
            text.push('\n');
            if s.to == "claude" && argv.len() == 3 {
                text.push_str("# add --fork-session to branch instead of continuing\n");
            }
            return Ok(SwitchCmd { env: vec![], argv, text });
        }
    }
    let loc = project::locate(s.cwd);
    let p = project::ensure(&w, &loc)?;
    let (h, created) = match s.handoff.and_then(handoff::parse_handle) {
        Some(h) => (h, false),
        None => {
            let tx = s.transcript.map(PathBuf::from).or_else(|| agent.as_ref().and_then(|a| a.transcript.clone()).map(PathBuf::from));
            let spec =
                packet::Spec { agent: agent_id.as_deref(), harness: from.as_deref(), transcript: tx.as_deref(), ..Default::default() };
            (packet::create(&w, &p, &spec)?, true)
        }
    };
    let hs = handoff::handle(h);
    if s.for_t3 {
        let body = if s.inline {
            format!("{}\nContinue this task from where it stopped.", packet::render(&w, h, packet::T3_CAP_CHARS)?)
        } else {
            format!(
                "Continue the task in foxdb handoff {hs}: take it with fox_memory op=take id={hs}, read it with fox_read target={hs}, then carry on."
            )
        };
        let mut text = String::new();
        if created {
            let _ = writeln!(text, "# packet {hs} (~{} tokens)", packet::render(&w, h, usize::MAX)?.len().div_ceil(4));
        }
        let _ = writeln!(text, "# paste into a T3 Code thread (any provider with foxdb registered; see docs/t3code.md):");
        let _ = writeln!(text, "{body}");
        return Ok(SwitchCmd { env: vec![], argv: vec![], text });
    }
    let prompt = if s.inline {
        format!("{}\nContinue this task from where it stopped.", packet::render(&w, h, packet::cap_for(Some(s.to), None))?)
    } else {
        format!(
            "Continue the task in foxdb handoff {hs}: take it with fox_memory op=take id={hs}, read it with fox_read target={hs}, then carry on."
        )
    };
    let fox = fox_exe();
    // The continuing session's own identity; its parent is the agent or session it continues.
    let id = agents::next_id(&w, if s.to == "codex" { "cx" } else { "cl" })?;
    let parent = agent_id.clone().or_else(|| digest.as_ref().map(|d| d.sid.clone())).filter(|x| !x.is_empty());
    let mut env: Vec<(String, String)> = vec![("FOXDB_AGENT".into(), id.clone())];
    if let Some(a) = &parent {
        env.push(("FOXDB_PARENT".into(), a.clone()));
    }
    env.push(("FOXDB_HOME".into(), db::home().to_string_lossy().into_owned()));
    if s.exec {
        let root = loc.root.to_string_lossy().into_owned();
        agents::register(
            &w,
            &agents::Reg {
                id: &id,
                harness: s.to,
                role: Some("session"),
                parent: parent.as_deref(),
                project_id: Some(p.id),
                cwd: Some(&root),
                pid: Some(std::process::id() as i64),
                task: Some(&hs),
                ..Default::default()
            },
        )?;
    }
    let root = loc.root.to_string_lossy().into_owned();
    // The prompt goes first: Claude's `--allowedTools <tools...>` is variadic and would swallow it.
    let mut argv: Vec<String> = vec![s.to.into()];
    if s.to == "codex" {
        argv.extend(["-C".into(), root]);
        argv.extend(codex_mcp_flags(&fox));
        argv.push(prompt);
    } else {
        argv.push(prompt);
        let e: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        argv.extend(["--mcp-config".into(), claude_mcp_json(&fox, &e)]);
        argv.extend(["--allowedTools".into(), "mcp__foxdb__*".into()]);
    }
    let mut text = String::new();
    if created {
        let _ = writeln!(text, "# packet {hs} (~{} tokens)", packet::render(&w, h, usize::MAX)?.len().div_ceil(4));
    }
    let envs: Vec<String> = env.iter().map(|(k, v)| format!("{k}={}", sh_quote(v))).collect();
    let cmd: Vec<String> = argv.iter().map(|a| sh_quote(a)).collect();
    let _ = writeln!(text, "{}", envs.into_iter().chain(cmd).collect::<Vec<_>>().join(" "));
    Ok(SwitchCmd { env, argv, text })
}
