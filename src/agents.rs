//! Agent registry and per-repo writer leases.
//!
//! An agent is any harness session or worker: a Claude Code session or subagent (registered by the
//! hooks), a `codex exec` / `claude -p` child started by `fox dispatch`, or anything that calls
//! `fox agent register`. Liveness comes from the process, not from heartbeats the agent would have
//! to send: a pid plus its `/proc/<pid>/stat` start time (so a reused pid is not mistaken for the
//! agent). Hook-registered sessions have no pid we can trust, so their transcript's mtime stands
//! in; agents with neither fall back to `fox agent heartbeat`.
//!
//! A writer lease is advisory, one per repo root. It is held while its holder lives and is freed
//! by an explicit release or the holder's death, never by a timer: a long build must not let a
//! second writer in.

use crate::{db, project, query};
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::fmt::Write;
use std::path::Path;

pub const HARNESSES: &[&str] = &["claude", "codex", "opencode", "deepseek", "other"];

/// Seconds without a heartbeat (or transcript write) before a pid-less agent is stale.
pub fn stale_secs() -> i64 {
    std::env::var("FOXDB_STALE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(1800)
}

/// `YYYY-MM-DD HH:MM` (UTC).
pub fn hms(ts: i64) -> String {
    let s = ts.rem_euclid(86_400);
    format!("{} {:02}:{:02}", crate::mcp::ymd(ts), s / 3600, s % 3600 / 60)
}

/// A process's start time in clock ticks since boot (field 22 of `/proc/<pid>/stat`).
pub fn proc_start(pid: i64) -> Option<i64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parens; fields resume after the last ')'.
    let rest = &s[s.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// The process exists, is not a zombie, and (when known) started when we recorded it.
pub fn alive(pid: i64, start: Option<i64>) -> bool {
    let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
    let Some(rest) = s.rfind(')').map(|i| &s[i + 1..]) else { return false };
    let mut f = rest.split_whitespace();
    if matches!(f.next(), Some("Z" | "X")) {
        return false;
    }
    start.is_none_or(|st| proc_start(pid) == Some(st))
}

#[derive(Debug, Clone, Default)]
pub struct Agent {
    pub id: String,
    pub harness: String,
    pub model: Option<String>,
    pub role: Option<String>,
    pub label: Option<String>,
    pub parent: Option<String>,
    pub project_id: Option<i64>,
    pub cwd: Option<String>,
    pub status: String,
    pub pid: Option<i64>,
    pub pid_start: Option<i64>,
    /// A dispatched worker's harness process (the supervisor's child).
    pub child_pid: Option<i64>,
    pub child_start: Option<i64>,
    pub session: Option<String>,
    pub thread: Option<String>,
    pub transcript: Option<String>,
    pub task: Option<String>,
    pub writer: bool,
    pub started_at: i64,
    pub heartbeat_at: i64,
    pub ended_at: Option<i64>,
    pub report: Option<i64>,
    /// The worktree when the worker started and when it ended, as commit ids (see `gitsnap`).
    pub base_commit: Option<String>,
    pub head_commit: Option<String>,
}

const COLS: &str = "id, harness, model, role, label, parent, project_id, cwd, status, pid, pid_start, session, thread, transcript, task, writer, started_at, heartbeat_at, ended_at, report, child_pid, child_start, base_commit, head_commit";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Agent> {
    Ok(Agent {
        id: r.get(0)?,
        harness: r.get(1)?,
        model: r.get(2)?,
        role: r.get(3)?,
        label: r.get(4)?,
        parent: r.get(5)?,
        project_id: r.get(6)?,
        cwd: r.get(7)?,
        status: r.get(8)?,
        pid: r.get(9)?,
        pid_start: r.get(10)?,
        session: r.get(11)?,
        thread: r.get(12)?,
        transcript: r.get(13)?,
        task: r.get(14)?,
        writer: r.get::<_, i64>(15)? != 0,
        started_at: r.get(16)?,
        heartbeat_at: r.get(17)?,
        ended_at: r.get(18)?,
        report: r.get(19)?,
        child_pid: r.get(20)?,
        child_start: r.get(21)?,
        base_commit: r.get(22)?,
        head_commit: r.get(23)?,
    })
}

pub fn get(c: &Connection, id: &str) -> Option<Agent> {
    c.query_row(&format!("SELECT {COLS} FROM agent WHERE id = ?1"), [id], row).optional().ok().flatten()
}

/// A fresh short id: `<prefix>-<n>` from a store-wide counter.
pub fn next_id(w: &Connection, prefix: &str) -> Result<String> {
    let n: i64 = w.query_row("UPDATE meta SET v = v + 1 WHERE k = 'agent_seq' RETURNING v", [], |r| r.get(0))?;
    Ok(format!("{prefix}-{n:x}"))
}

/// What a registration knows. Missing fields keep what an earlier registration stored.
#[derive(Default)]
pub struct Reg<'a> {
    pub id: &'a str,
    pub harness: &'a str,
    pub model: Option<&'a str>,
    pub role: Option<&'a str>,
    pub label: Option<&'a str>,
    pub parent: Option<&'a str>,
    pub project_id: Option<i64>,
    pub cwd: Option<&'a str>,
    pub pid: Option<i64>,
    pub session: Option<&'a str>,
    pub transcript: Option<&'a str>,
    pub task: Option<&'a str>,
    pub writer: bool,
}

/// Insert or refresh an agent; it is `running` again afterwards.
pub fn register(w: &Connection, r: &Reg) -> Result<()> {
    if r.id.trim().is_empty() {
        bail!("agent id is empty");
    }
    let harness = if HARNESSES.contains(&r.harness) { r.harness } else { "other" };
    let now = db::now();
    let start = r.pid.and_then(proc_start);
    w.execute(
        "INSERT INTO agent(id, harness, model, role, label, parent, project_id, cwd, status, pid, pid_start, session, transcript, task, writer, started_at, heartbeat_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'running',?9,?10,?11,?12,?13,?14,?15,?15)
         ON CONFLICT(id) DO UPDATE SET harness = excluded.harness, model = COALESCE(excluded.model, model),
           role = COALESCE(excluded.role, role), label = COALESCE(excluded.label, label), parent = COALESCE(excluded.parent, parent),
           project_id = COALESCE(excluded.project_id, project_id), cwd = COALESCE(excluded.cwd, cwd), status = 'running',
           pid = COALESCE(excluded.pid, pid), pid_start = CASE WHEN excluded.pid IS NULL THEN pid_start ELSE excluded.pid_start END,
           session = COALESCE(excluded.session, session), transcript = COALESCE(excluded.transcript, transcript),
           task = COALESCE(excluded.task, task), writer = MAX(writer, excluded.writer), heartbeat_at = excluded.heartbeat_at, ended_at = NULL",
        params![
            r.id, harness, r.model, r.role, r.label, r.parent, r.project_id, r.cwd, r.pid, start, r.session, r.transcript, r.task,
            r.writer as i64, now
        ],
    )?;
    Ok(())
}

pub fn heartbeat(w: &Connection, id: &str) -> Result<()> {
    let n = w.execute(
        "UPDATE agent SET heartbeat_at = ?2, status = CASE WHEN status = 'stale' THEN 'running' ELSE status END WHERE id = ?1",
        params![id, db::now()],
    )?;
    if n == 0 {
        bail!("no agent {id}; run `fox agent register` first");
    }
    Ok(())
}

/// Record the process that stands for this agent (the dispatch supervisor, or a session's pid).
pub fn set_pid(w: &Connection, id: &str, pid: i64) -> Result<()> {
    w.execute("UPDATE agent SET pid = ?2, pid_start = ?3 WHERE id = ?1", params![id, pid, proc_start(pid)])?;
    Ok(())
}

/// Record a worker's base or head commit.
pub fn set_commit(w: &Connection, id: &str, base: Option<&str>, head: Option<&str>) -> Result<()> {
    w.execute(
        "UPDATE agent SET base_commit = COALESCE(?2, base_commit), head_commit = COALESCE(?3, head_commit) WHERE id = ?1",
        params![id, base, head],
    )?;
    Ok(())
}

/// Native resume ids learned after launch (a Codex thread id from `thread.started`).
pub fn set_resume(w: &Connection, id: &str, session: Option<&str>, thread: Option<&str>, child_pid: Option<i64>) -> Result<()> {
    w.execute(
        "UPDATE agent SET session = COALESCE(?2, session), thread = COALESCE(?3, thread), child_pid = COALESCE(?4, child_pid),
           child_start = CASE WHEN ?4 IS NULL THEN child_start ELSE ?5 END WHERE id = ?1",
        params![id, session, thread, child_pid, child_pid.and_then(proc_start)],
    )?;
    Ok(())
}

/// How a finished agent can end. Only `done` closes the work it was given: `failed` (crash or
/// malformed report), `blocked` and `partial` (its own report says so) and `flagged` (a done
/// report with check findings) leave the handoff open for a retry and make `fox wait` exit 4.
pub const END_STATUSES: &[&str] = &["done", "failed", "blocked", "partial", "flagged"];

/// Mark an agent finished (one of [`END_STATUSES`]; anything else is `done`), keep its report
/// handle, free its leases.
pub fn finish(w: &Connection, id: &str, status: &str, report: Option<i64>) -> Result<()> {
    let status = if END_STATUSES.contains(&status) { status } else { "done" };
    w.execute(
        "UPDATE agent SET status = ?2, ended_at = ?3, report = COALESCE(?4, report), heartbeat_at = ?3 WHERE id = ?1",
        params![id, status, db::now(), report],
    )?;
    w.execute("DELETE FROM lease WHERE agent = ?1", [id])?;
    db::bump_generation(w)?;
    Ok(())
}

/// A dispatched worker's harness child still runs (it may outlive a killed supervisor).
fn child_alive(a: &Agent) -> bool {
    a.child_pid.is_some_and(|p| alive(p, a.child_start))
}

/// Is a `running` agent still there?
fn is_live(a: &Agent, now: i64) -> bool {
    if let Some(pid) = a.pid {
        return alive(pid, a.pid_start) || child_alive(a);
    }
    let mut last = a.heartbeat_at;
    if let Some(t) = &a.transcript
        && let Ok(m) = std::fs::metadata(t).and_then(|m| m.modified())
        && let Ok(d) = m.duration_since(std::time::UNIX_EPOCH)
    {
        last = last.max(d.as_secs() as i64);
    }
    now - last <= stale_secs()
}

/// How many `running` children of `parent` are alive (see `is_live`): a session with live
/// subagents shares its MCP server, and so its identity, with them.
pub fn live_children(c: &Connection, parent: &str) -> usize {
    let now = db::now();
    let Ok(mut s) = c.prepare_cached(&format!("SELECT {COLS} FROM agent WHERE parent = ?1 AND status = 'running'")) else { return 0 };
    s.query_map([parent], row).map(|it| it.flatten().filter(|a| is_live(a, now)).count()).unwrap_or(0)
}

/// Mark `running` agents whose process died (or whose heartbeat lapsed) as `stale`; free their
/// leases. Returns how many changed.
pub fn refresh(w: &Connection) -> Result<usize> {
    let now = db::now();
    let running: Vec<Agent> = {
        let mut s = w.prepare_cached(&format!("SELECT {COLS} FROM agent WHERE status = 'running'"))?;
        s.query_map([], row)?.flatten().collect()
    };
    let mut n = 0;
    for a in running.iter().filter(|a| !is_live(a, now)) {
        let k = w.execute("UPDATE agent SET status = 'stale' WHERE id = ?1 AND status = 'running'", [&a.id])?;
        w.execute("DELETE FROM lease WHERE agent = ?1", [&a.id])?;
        // A dead worker's partial work stays addressable: record where it left the tree.
        if k > 0
            && a.base_commit.is_some()
            && a.head_commit.is_none()
            && let Some(head) = a.cwd.as_deref().and_then(|d| crate::gitsnap::snapshot(Path::new(d)))
        {
            set_commit(w, &a.id, None, Some(&head))?;
        }
        n += k;
    }
    Ok(n)
}

fn age(to: i64, from: i64) -> String {
    let d = (to - from).max(0);
    match d {
        0..60 => format!("{d}s"),
        60..3600 => format!("{}m", d / 60),
        3600..86_400 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86_400),
    }
}

/// One compact line: `cx-3 codex writer running 4m [impl] th-019… report h:1f`.
fn line(a: &Agent, now: i64) -> String {
    let mut l = format!("{} {} {} {}", a.id, a.harness, a.role.as_deref().unwrap_or("-"), a.status);
    // Running time so far, or how long it ran.
    let _ = write!(l, " {}", age(a.ended_at.unwrap_or(now), a.started_at));
    if let Some(m) = &a.model {
        let _ = write!(l, " {m}");
    }
    if let Some(x) = &a.label {
        let _ = write!(l, " [{x}]");
    }
    if let Some(t) = a.thread.as_ref().or(a.session.as_ref()).filter(|t| **t != a.id) {
        let _ = write!(l, " {}", query::clip(t, 40));
    }
    if a.writer {
        l.push_str(" writer");
    }
    if let Some(r) = a.report {
        let _ = write!(l, " report {}", crate::handoff::handle(r));
    }
    if let (Some(b), Some(h)) = (&a.base_commit, &a.head_commit) {
        let short = |x: &str| x.chars().take(10).collect::<String>();
        let _ = write!(l, " {}..{}", short(b), short(h));
    }
    l
}

/// `fox agents [--tree] [--all]`: newest first, or as a parent/child tree. Without `all`, only
/// the last day's agents and anything still running.
pub fn list(c: &Connection, tree: bool, all: bool) -> Result<String> {
    let now = db::now();
    let since = if all { 0 } else { now - 86_400 };
    let mut s = c.prepare_cached(&format!(
        "SELECT {COLS} FROM agent WHERE status = 'running' OR heartbeat_at >= ?1 ORDER BY started_at DESC, id DESC LIMIT 200"
    ))?;
    let v: Vec<Agent> = s.query_map([since], row)?.flatten().collect();
    if v.is_empty() {
        return Ok("no agents yet (fox dispatch, fox agent register, or the session hooks)\n".into());
    }
    let mut out = String::new();
    if !tree {
        for a in &v {
            out.push_str(&line(a, now));
            out.push('\n');
        }
        return Ok(out);
    }
    let ids: std::collections::HashSet<&str> = v.iter().map(|a| a.id.as_str()).collect();
    fn walk(v: &[Agent], parent: &str, depth: usize, now: i64, out: &mut String) {
        for a in v.iter().rev().filter(|a| a.parent.as_deref() == Some(parent)) {
            let _ = writeln!(out, "{}{}", "  ".repeat(depth), line(a, now));
            if depth < 8 {
                walk(v, &a.id, depth + 1, now, out);
            }
        }
    }
    // Roots: no parent, or a parent outside this listing (shown as a bare id so the tree reads).
    let mut orphans_of: Vec<&str> = vec![];
    for a in v.iter().rev() {
        match a.parent.as_deref() {
            None => {
                let _ = writeln!(out, "{}", line(a, now));
                walk(&v, &a.id, 1, now, &mut out);
            }
            Some(p) if !ids.contains(p) && !orphans_of.contains(&p) => orphans_of.push(p),
            _ => {}
        }
    }
    for p in orphans_of {
        let _ = writeln!(out, "{p}");
        walk(&v, p, 1, now, &mut out);
    }
    Ok(out)
}

/// `fox status --agents`: one line per live agent.
pub fn live(c: &Connection) -> Result<String> {
    let now = db::now();
    let mut s = c.prepare_cached(&format!("SELECT {COLS} FROM agent WHERE status = 'running' ORDER BY started_at, id"))?;
    let v: Vec<String> = s.query_map([], row)?.flatten().map(|a| line(&a, now)).collect();
    Ok(if v.is_empty() { "no live agents\n".into() } else { v.join("\n") + "\n" })
}

/// The lease key for a directory: its project root.
pub fn repo_key(dir: &Path) -> String {
    project::locate(dir).root.to_string_lossy().into_owned()
}

/// A lease holder is live while its process, or (for a dispatched writer) its harness child, runs.
fn holder_live(w: &Connection, agent: &str, pid: Option<i64>, start: Option<i64>) -> bool {
    match pid {
        Some(p) => alive(p, start) || get(w, agent).is_some_and(|a| child_alive(&a)),
        None => get(w, agent).is_some_and(|a| a.status == "running" && is_live(&a, db::now())),
    }
}

/// Shells sit between a harness and the commands it runs; they never stand for an agent.
const SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "fish", "ksh", "mksh", "busybox"];

/// `(command name, parent pid)` from `/proc/<pid>/stat`.
fn comm_ppid(pid: i64) -> Option<(String, i64)> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (open, close) = (s.find('(')?, s.rfind(')')?);
    let ppid = s[close + 1..].split_whitespace().nth(1)?.parse().ok()?;
    Some((s[open + 1..close].to_string(), ppid))
}

/// This process's ancestors, nearest first (at most 8, stopping at init).
fn ancestors() -> Vec<i64> {
    let mut v = vec![];
    let mut pid = std::os::unix::process::parent_id() as i64;
    while pid > 1 && v.len() < 8 {
        v.push(pid);
        match comm_ppid(pid) {
            Some((_, pp)) => pid = pp,
            None => break,
        }
    }
    v
}

/// The nearest ancestor that is not a shell: the harness a hook or command runs under.
pub fn harness_pid() -> Option<i64> {
    ancestors().into_iter().find(|p| comm_ppid(*p).is_some_and(|(c, _)| !SHELLS.contains(&c.as_str())))
}

/// Who this process speaks for. `FOXDB_AGENT` first; else Claude Code's session id (set for the
/// commands it runs, and the id its hooks register); else a running agent registered with an
/// ancestor's pid (a hook-registered session, for its MCP server and shell commands); else
/// `<prefix>-<parent pid>`, unique per harness process or shell, so two sessions never share a
/// take or a read cursor.
pub fn ambient_id(c: Option<&Connection>, prefix: &str) -> String {
    for var in ["FOXDB_AGENT", "CLAUDE_CODE_SESSION_ID"] {
        if let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            return v.trim().to_string();
        }
    }
    let up = ancestors();
    if let Some(c) = c
        && let Ok(mut s) = c.prepare_cached(
            "SELECT id, pid_start FROM agent WHERE pid = ?1 AND status = 'running' ORDER BY started_at DESC, rowid DESC LIMIT 1",
        )
    {
        for &pid in &up {
            if let Ok(Some((id, start))) = s.query_row([pid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))).optional()
                && alive(pid, start)
            {
                return id;
            }
        }
    }
    let prefix: String = prefix.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(24).collect();
    format!("{}-{}", if prefix.is_empty() { "agent" } else { &prefix }, up.first().copied().unwrap_or(0))
}

/// Take the writer lease on `repo` for `agent`. `Err(holder)` when someone else holds it and is
/// alive. A dead holder's lease is taken over.
pub fn lease_take(w: &Connection, repo: &str, agent: &str, pid: Option<i64>) -> Result<std::result::Result<(), String>> {
    w.execute_batch("BEGIN IMMEDIATE")?;
    let r = (|| -> Result<std::result::Result<(), String>> {
        let cur: Option<(String, Option<i64>, Option<i64>)> = w
            .query_row("SELECT agent, pid, pid_start FROM lease WHERE repo = ?1", [repo], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        if let Some((h, p, s)) = cur
            && h != agent
            && holder_live(w, &h, p, s)
        {
            return Ok(Err(h));
        }
        w.execute(
            "INSERT OR REPLACE INTO lease(repo, agent, pid, pid_start, taken_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![repo, agent, pid, pid.and_then(proc_start), db::now()],
        )?;
        Ok(Ok(()))
    })();
    match &r {
        Ok(_) => w.execute_batch("COMMIT")?,
        Err(_) => {
            let _ = w.execute_batch("ROLLBACK");
        }
    }
    r
}

/// Point an agent's leases at the process that now stands for it.
pub fn lease_set_pid(w: &Connection, agent: &str, pid: i64) -> Result<()> {
    w.execute("UPDATE lease SET pid = ?2, pid_start = ?3 WHERE agent = ?1", params![agent, pid, proc_start(pid)])?;
    Ok(())
}

/// Release the lease on `repo` (only if `agent` holds it, when given).
pub fn lease_release(w: &Connection, repo: &str, agent: Option<&str>) -> Result<usize> {
    Ok(w.execute("DELETE FROM lease WHERE repo = ?1 AND (?2 IS NULL OR agent = ?2)", params![repo, agent])?)
}

/// `fox lease list`: live leases; dead holders' leases are dropped on the way.
pub fn lease_list(w: &Connection) -> Result<String> {
    type LeaseRow = (String, String, Option<i64>, Option<i64>, i64);
    let rows: Vec<LeaseRow> = {
        let mut s = w.prepare_cached("SELECT repo, agent, pid, pid_start, taken_at FROM lease ORDER BY repo")?;
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.flatten().collect()
    };
    let now = db::now();
    let mut out = String::new();
    for (repo, agent, pid, start, at) in rows {
        if !holder_live(w, &agent, pid, start) {
            lease_release(w, &repo, Some(&agent))?;
            continue;
        }
        let _ = writeln!(out, "{repo}  writer {agent}  {}", age(now, at));
    }
    Ok(if out.is_empty() { "no writer leases\n".into() } else { out })
}
