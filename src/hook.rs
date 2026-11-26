//! `fox hook <event>`: Claude Code / Codex command hooks. Input is the hook's JSON on stdin; output
//! is the `hookSpecificOutput` JSON form, nothing, or (PreCompact, Claude Code) plain-text
//! instructions for the compaction summariser. Hooks fail open: any error prints nothing.
//!
//! Everything a hook injects is a deterministic function of the index (no clocks, no relative
//! ages), so siblings and resumed sessions share the prompt-cache prefix.

use crate::{db, project, query};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Which harness reads the output; each caps hook context differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    /// Claude Code: `additionalContext` over 10,000 characters is spilled to a file.
    Claude,
    /// Codex: hook output over 2,500 tokens is spilled; we estimate 4 chars per token with margin.
    Codex,
}

impl Harness {
    pub fn parse(s: &str) -> Harness {
        if s.eq_ignore_ascii_case("codex") { Harness::Codex } else { Harness::Claude }
    }
    pub fn cap_chars(self) -> usize {
        match self {
            Harness::Claude => 9_500,
            Harness::Codex => 2_200 * 4,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
        }
    }
}

/// Clip to at most `cap` chars on a line boundary.
pub fn cap_lines(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let mut out = String::new();
    let mut n = 0;
    for l in s.lines() {
        let k = l.chars().count() + 1;
        if n + k > cap {
            break;
        }
        out.push_str(l);
        out.push('\n');
        n += k;
    }
    out
}

/// Token budget of the SessionStart brief (`context-budget.json`).
pub const SESSION_BRIEF_TOKENS: usize = 400;
/// Token budget of the SubagentStart brief (`context-budget.json`).
pub const SUBAGENT_BRIEF_TOKENS: usize = 250;
/// The most `fox_memory op=brief` returns: well above both hook budgets, so the call the cut
/// marker names gives back the lines the hook cut.
pub const FULL_BRIEF_TOKENS: usize = 2_000;

/// The last line of a brief that was cut: says so, and the call that returns the rest
/// (`budget` is [`FULL_BRIEF_TOKENS`]).
pub const CUT_MARK: &str = "(brief cut short; fox_memory op=brief budget=2000)";

/// Like [`cap_lines`], but a cut text ends with a `+N lines` [`CUT_MARK`] line, still within `cap`.
pub fn cap_marked(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let total = s.lines().count();
    let room = cap.saturating_sub(CUT_MARK.chars().count() + 16);
    let kept = cap_lines(s, room);
    let n = total - kept.lines().count();
    format!("{kept}+{n} lines {CUT_MARK}\n")
}

/// The session brief for `harness`, never over its cap whatever `budget` (tokens) asks for.
pub fn brief_for(c: &Connection, p: &project::Project, branch: Option<&str>, h: Harness, budget: usize) -> Result<String> {
    let budget = budget.min(h.cap_chars() / 4);
    Ok(cap_marked(&query::brief(c, p, branch, budget)?, h.cap_chars()))
}

fn cwd_of(input: &Value) -> PathBuf {
    input["cwd"].as_str().map(PathBuf::from).or_else(|| std::env::current_dir().ok()).unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn context(event: &str, text: String) -> String {
    if text.trim().is_empty() {
        return String::new();
    }
    json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}}).to_string()
}

fn session_start(input: &Value, h: Harness) -> Result<String> {
    let loc = project::locate(&cwd_of(input));
    let Some(c) = db::open_ro() else { return Ok(String::new()) };
    let Some(p) = project::find(&c, &loc)? else { return Ok(String::new()) };
    let branch = loc.git_dir.as_deref().and_then(project::branch);
    // A dispatched reviewer starts empty: `claude -p` and `codex exec` still run the user's
    // SessionStart hook, so the brief is withheld here too (its own pre-compact packet is not).
    let mut text = if reviewer_env(&c) { String::new() } else { brief_for(&c, &p, branch.as_deref(), h, SESSION_BRIEF_TOKENS)? };
    // After a compaction, point at the packet PreCompact saved for this session.
    if input["source"] == "compact"
        && let Some(owner) = owner_of(input)
        && let Ok(id) = c.query_row(
            "SELECT id FROM handoff WHERE project_id = ?1 AND kind = 'packet' AND scope = 'compact' AND agent = ?2 ORDER BY id DESC LIMIT 1",
            rusqlite::params![p.id, owner],
            |r| r.get::<_, i64>(0),
        )
    {
        let hs = crate::handoff::handle(id);
        text = cap_marked(&format!("{text}pre-compact packet {hs} (fox_read target={hs})\n"), h.cap_chars());
    }
    Ok(context("SessionStart", text))
}

/// Is this hook running for a registered reviewer (`FOXDB_AGENT` with role `reviewer`)?
fn reviewer_env(c: &Connection) -> bool {
    std::env::var("FOXDB_AGENT")
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|id| crate::agents::get(c, &id))
        .is_some_and(|a| a.role.as_deref() == Some("reviewer"))
}

/// Who a session hook speaks for: a dispatched worker's `FOXDB_AGENT`, else the session id.
fn owner_of(input: &Value) -> Option<String> {
    std::env::var("FOXDB_AGENT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| input["session_id"].as_str().filter(|s| !s.is_empty()).map(String::from))
}

/// What `fox hook pre-compact --harness claude` prints. Claude Code hands PreCompact stdout to
/// the compaction summariser as instructions (not context): keep the working set and the packet
/// `handle` the hook just saved. A function of the handle only.
pub fn compact_instructions(handle: &str) -> String {
    format!(
        "foxdb: keep these in the summary:\n\
         - the path:line anchors of files being read or edited, and the symbols being changed\n\
         - leads already ruled out, and why\n\
         - handoff packet {handle} (fox_read target={handle} restores the working set)\n"
    )
}

/// Save a handoff packet from the transcript before the harness compacts it. For Claude Code,
/// prints [`compact_instructions`]; for Codex, nothing.
fn pre_compact(input: &Value, h: Harness) -> Result<String> {
    if !db::db_path().exists() {
        return Ok(String::new());
    }
    let w = db::open_rw()?;
    // The compaction drops what the CLI sent this session (`FOXDB_SESSION`, a dispatched worker's
    // id): forget it, so the next read returns the text, not an "already sent" reference.
    if let Ok(s) = std::env::var("FOXDB_SESSION")
        && !s.is_empty()
    {
        w.execute("DELETE FROM seen WHERE session = ?1", [&s])?;
    }
    let Some(p) = project::find(&w, &project::locate(&cwd_of(input)))? else { return Ok(String::new()) };
    let owner = owner_of(input);
    let tx = input["transcript_path"].as_str().map(PathBuf::from);
    let spec = crate::packet::Spec { agent: owner.as_deref(), harness: Some(h.name()), transcript: tx.as_deref(), ..Default::default() };
    let id = crate::packet::create(&w, &p, &spec)?;
    w.execute("UPDATE handoff SET scope = 'compact' WHERE id = ?1", [id])?;
    Ok(if h == Harness::Claude { compact_instructions(&crate::handoff::handle(id)) } else { String::new() })
}

fn glob_match(pat: &str, s: &str) -> bool {
    globset::GlobBuilder::new(pat).case_insensitive(true).build().map(|g| g.compile_matcher().is_match(s)).unwrap_or(false)
}

/// Agent definition files that opt out with a `foxdb: off` line.
fn opted_out(agent_type: &str, cwd: Option<&Path>) -> bool {
    if agent_type.is_empty() || agent_type.contains(['/', '\\']) {
        return false;
    }
    let file = format!("{agent_type}.md");
    let mut dirs: Vec<PathBuf> = vec![];
    if let Some(c) = cwd {
        dirs.push(c.join(".claude/agents"));
        dirs.push(c.join(".codex/agents"));
    }
    if let Some(h) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(&h).join(".claude/agents"));
        dirs.push(PathBuf::from(&h).join(".codex/agents"));
    }
    dirs.iter().any(|d| {
        std::fs::read_to_string(d.join(&file)).is_ok_and(|t| {
            t.lines().take(40).any(|l| l.trim().eq_ignore_ascii_case("foxdb: off") || l.trim().eq_ignore_ascii_case("fox-brief: off"))
        })
    })
}

/// Should this subagent start without a foxdb brief? Fresh-eyes reviewers must stay
/// zero-context: matched by name or type against `[hooks] skip_agents` (default
/// `egm-*`, `*goldfish*`, `*reviewer*`), or opted out in their agent definition.
pub fn skip_agent_with(cfg: &crate::config::Config, agent_type: &str, agent_id: &str, cwd: Option<&Path>) -> bool {
    let pats: Vec<String> = match &cfg.hooks.skip_agents {
        Some(v) => v.clone(),
        None => crate::config::DEFAULT_SKIP_AGENTS.iter().map(|s| s.to_string()).collect(),
    };
    pats.iter().any(|p| glob_match(p, agent_type) || (!agent_id.is_empty() && glob_match(p, agent_id))) || opted_out(agent_type, cwd)
}

/// Subagent briefs stay around 1,500 characters.
pub const SUBAGENT_BRIEF_CHARS: usize = 1_500;

/// The brief for a subagent. It depends only on the project's index and board, never on the
/// agent's id or task, so sibling subagents of one type get byte-identical text.
fn subagent_start(cfg: &crate::config::Config, input: &Value, h: Harness) -> Result<String> {
    let agent_type = input["agent_type"].as_str().unwrap_or("");
    let agent_id = input["agent_id"].as_str().or_else(|| input["agent_name"].as_str()).unwrap_or("");
    let cwd = cwd_of(input);
    if skip_agent_with(cfg, agent_type, agent_id, Some(&cwd)) {
        return Ok(String::new());
    }
    let loc = project::locate(&cwd);
    let Some(c) = db::open_ro() else { return Ok(String::new()) };
    if reviewer_env(&c) {
        return Ok(String::new());
    }
    let Some(p) = project::find(&c, &loc)? else { return Ok(String::new()) };
    let branch = loc.git_dir.as_deref().and_then(project::branch);
    let mut text = query::brief(&c, &p, branch.as_deref(), SUBAGENT_BRIEF_TOKENS)?;
    // The savings and stale-memory lines are of no use to a helper.
    text = text.lines().filter(|l| !l.starts_with("fox saved") && !l.starts_with("stale memory:")).map(|l| format!("{l}\n")).collect();
    // A pointer to the parent's frozen board snapshot, not the live board: siblings started
    // before and after a post still get the same bytes. The board protocol (opt-in, `[hooks]
    // swarm_board`: in A/B runs it cost parallel subagents more and gained nothing) reads from
    // that snapshot on; the subagent carries its own cursor (`since=h:N`), since subagents call
    // through their parent's MCP server and share its identity.
    if let Ok(w) = db::open_rw()
        && let Some(sid) = input["session_id"].as_str().filter(|s| !s.is_empty())
        && let Ok(h) =
            crate::handoff::snapshot(&w, p.id, &format!("session:{sid}"), Some(&owner_of(input).unwrap_or_else(|| sid.to_string())))
    {
        let h = crate::handoff::handle(h);
        text.push_str(&format!("parent packet {h} - open decisions/requests (fox_read target={h})\n"));
        if cfg.hooks.swarm_board {
            text.push_str(&format!(
                "board: before each new lead, fox_memory op=read since={h}, then the since=h:N it ends with. \
             Post fact|failed (approach, why)|question with agent=<your name>, evidence=handles, not content.\n"
            ));
        }
    }
    const LAST: &str = "Final answer: findings and handles (path:line, s:/f:), not file contents; it is saved for the parent.\n";
    let cap = SUBAGENT_BRIEF_CHARS.min(h.cap_chars());
    // Files the parent already read, as handles that fit in what the cap leaves.
    if let Some(t) = input["transcript_path"].as_str().filter(|_| h == Harness::Claude) {
        let room = cap.saturating_sub(text.chars().count() + LAST.len());
        text.push_str(&parent_read_line(&c, &p, Path::new(t), room));
    }
    text.push_str(LAST);
    Ok(context("SubagentStart", cap_marked(&text, cap)))
}

/// The tail of a parent transcript that [`parent_reads`] scans: recent reads matter most, and a
/// long session's transcript runs to tens of megabytes.
const PARENT_TAIL_BYTES: u64 = 16 << 20;

/// Paths under `root` that a Claude Code transcript read (`Read`, or `fox_read` of a path), most
/// recent first, relative to `root`. Only tool_use lines are parsed; nothing is read but the
/// transcript's last [`PARENT_TAIL_BYTES`].
pub fn parent_reads(transcript: &Path, root: &Path) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(transcript) else { return vec![] };
    let start = f.metadata().map(|m| m.len()).unwrap_or(0).saturating_sub(PARENT_TAIL_BYTES);
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(start)).is_err() || f.take(PARENT_TAIL_BYTES).read_to_end(&mut buf).is_err() {
        return vec![];
    }
    let text = String::from_utf8_lossy(&buf);
    // The root as given and as canonical: either may be the form the transcript uses.
    let mut roots = vec![root.to_path_buf()];
    if let Ok(c) = std::fs::canonicalize(root)
        && c != root
    {
        roots.push(c);
    }
    let mut seen: Vec<String> = vec![];
    // A tail that starts mid-line skips that line.
    for l in text.lines().skip(usize::from(start > 0)).filter(|l| l.contains("\"tool_use\"")) {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        for b in v["message"]["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "tool_use") {
            let name = b["name"].as_str().unwrap_or("");
            let path = match name {
                "Read" => b["input"]["file_path"].as_str(),
                n if n.ends_with("fox_read") => b["input"]["target"].as_str(),
                _ => None,
            };
            let Some(path) = path else { continue };
            // Component-wise (`/repo-old/a.rs` is not under `/repo`), and through symlinks: a
            // transcript may name the repo as `/var/…` while the root is `/private/var/…` (macOS).
            let rel = if path.starts_with('/') {
                match relative_to(Path::new(path), &roots) {
                    Some(r) => r,
                    None => continue,
                }
            } else {
                path.to_string()
            };
            if !rel.is_empty() {
                seen.push(rel);
            }
        }
    }
    // Newest first, each path once.
    let mut once = std::collections::HashSet::new();
    seen.into_iter().rev().filter(|p| once.insert(p.clone())).collect()
}

/// `path` relative to the first of `roots` it lies under, component-wise. Failing that, its
/// nearest existing ancestor is canonicalized (the file itself may be gone) and tried again, so a
/// path through a symlink still matches.
fn relative_to(path: &Path, roots: &[PathBuf]) -> Option<String> {
    let under = |p: &Path| roots.iter().find_map(|r| p.strip_prefix(r).ok().map(Path::to_path_buf));
    if let Some(r) = under(path) {
        return Some(r.to_string_lossy().into_owned());
    }
    let mut tail: Vec<&std::ffi::OsStr> = vec![];
    let mut cur = path;
    let base = loop {
        if let Ok(c) = std::fs::canonicalize(cur) {
            break c;
        }
        tail.push(cur.file_name()?);
        cur = cur.parent()?;
    };
    let full = tail.iter().rev().fold(base, |p, n| p.join(n));
    under(&full).map(|r| r.to_string_lossy().into_owned())
}

/// `parent read (...): f:1a src/a.rs, f:2b src/b.rs` for the indexed files among the parent's
/// reads, as many as fit in `room` chars; empty when none does.
fn parent_read_line(c: &Connection, p: &project::Project, transcript: &Path, room: usize) -> String {
    const HEAD: &str = "parent read (fox_read a handle, lines=a-b for a slice):";
    let Ok(mut s) = c.prepare_cached("SELECT id FROM node WHERE project_id = ?1 AND kind = 'file' AND path = ?2") else {
        return String::new();
    };
    let mut line = HEAD.to_string();
    for rel in parent_reads(transcript, &p.root) {
        // No item fits in under ` f:1 x`: stop looking files up once the line is full.
        if line.chars().count() + 8 > room {
            break;
        }
        let Ok(id) = s.query_row(rusqlite::params![p.id, rel], |r| r.get::<_, i64>(0)) else { continue };
        let item = format!("{}{} {rel}", if line.len() == HEAD.len() { " " } else { ", " }, query::handle("file", id));
        if line.chars().count() + item.chars().count() + 1 > room {
            break;
        }
        line.push_str(&item);
    }
    if line.len() == HEAD.len() { String::new() } else { line + "\n" }
}

fn save_subagent_report(input: &Value, h: Harness, agent_id: Option<&str>) -> Result<Option<(Connection, i64)>> {
    let Some(msg) = input["last_assistant_message"].as_str().filter(|m| !m.trim().is_empty()) else { return Ok(None) };
    let transcript = input["agent_transcript_path"].as_str().filter(|s| !s.is_empty());
    let scope = agent_id
        .map(|id| format!("subagent-stop:id:{id}"))
        .or_else(|| transcript.map(|path| format!("subagent-stop:transcript:{}", crate::redact::redact(path))));
    if input["stop_hook_active"] == true && scope.is_none() {
        return Ok(None);
    }
    let mut w = db::open_rw()?;
    let Some(p) = project::find(&w, &project::locate(&cwd_of(input)))? else { return Ok(None) };
    let session = input["session_id"].as_str();
    let agent = input["agent_type"].as_str().filter(|s| !s.is_empty()).or(agent_id);
    let tx = w.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing = if let Some(scope) = scope.as_deref() {
        tx.query_row(
            "SELECT id FROM handoff WHERE project_id = ?1 AND kind = 'report' AND scope = ?2 AND harness = ?3 AND session IS ?4
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![p.id, scope, h.name(), session],
            |r| r.get(0),
        )
        .optional()?
    } else {
        None
    };
    if existing.is_none()
        && input["stop_hook_active"] == true
        && agent_id.and_then(|id| crate::agents::get(&tx, id)).is_some_and(|a| a.report.is_some())
    {
        return Ok(None);
    }
    let report = crate::handoff::post_at(
        &tx,
        &crate::handoff::Post {
            project_id: p.id,
            kind: "report",
            text: msg,
            scope: scope.as_deref(),
            harness: Some(h.name()),
            session,
            agent,
            evidence: transcript,
            ..Default::default()
        },
        existing,
    )?;
    // Keep the board summary linked to the same report when an agent retries.
    let summary = existing.and_then(|id| {
        tx.query_row("SELECT id FROM handoff WHERE kind = 'summary' AND parent_id = ?1 ORDER BY id LIMIT 1", [id], |r| r.get(0)).ok()
    });
    let cites = crate::handoff::attachments(&tx, p.id, &cited(msg), false).unwrap_or_default();
    let _ = crate::handoff::post_at(
        &tx,
        &crate::handoff::Post {
            project_id: p.id,
            kind: "summary",
            text: &summary_of(msg, report),
            harness: Some(h.name()),
            session,
            // The subagent itself, not its type: siblings of one type are different posters.
            agent: agent_id.or(agent),
            evidence: Some(cites.as_str()).filter(|e| !e.is_empty()),
            parent: Some(report),
            ..Default::default()
        },
        summary,
    );
    tx.commit()?;
    Ok(Some((w, report)))
}

fn subagent_stop(cfg: &crate::config::Config, input: &Value, h: Harness) -> Result<String> {
    let agent_id = input["agent_id"].as_str().filter(|s| !s.is_empty());
    // Commit the report before the check can spend the rest of the hook deadline.
    let saved = save_subagent_report(input, h, agent_id);
    if let Some(block) = stop_check(cfg, input) {
        return Ok(block);
    }
    if let (Some((w, report)), Some(id)) = (saved?, agent_id) {
        crate::agents::finish(&w, id, "done", Some(report))?;
    }
    Ok(String::new())
}

/// The opt-in stop gate (`[hooks] stop_check`): a `decision: block` whose reason lists the
/// certain callers of changed signatures that the diff missed, else None. A stop that a stop
/// hook already sent back (`stop_hook_active`) is never blocked again.
fn stop_check(cfg: &crate::config::Config, input: &Value) -> Option<String> {
    if !cfg.hooks.stop_check || input["stop_hook_active"] == true || !db::db_path().exists() {
        return None;
    }
    let cwd = cwd_of(input);
    let mut w = db::open_rw().ok()?;
    let p = project::find(&w, &project::locate(&cwd)).ok()??;
    let r = crate::check::run(&mut w, &p, &cwd, true).ok()?;
    let reason = format!("{}Update these call sites, or say why each needs no change.", crate::check::render(&r));
    r.blocks().then(|| json!({"decision": "block", "reason": reason}).to_string())
}

/// A report's opening, clipped to fit a swarm entry with the report's handle at its end.
pub fn summary_of(report: &str, report_id: i64) -> String {
    let tail = format!(" (report {})", crate::handoff::handle(report_id));
    let room = crate::handoff::ENTRY_MAX - tail.chars().count();
    let one: String = report.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{}{tail}", query::clip(&one, room))
}

/// Handles and `path[:line]` mentions in a report, space-separated, first mention first.
fn cited(text: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\b[hsfcdmagx]:[0-9a-f]+\b|[A-Za-z0-9_\-./]+\.[A-Za-z0-9]{1,6}(?::\d+)?").unwrap());
    re.find_iter(text).take(40).map(|m| m.as_str()).collect::<Vec<_>>().join(" ")
}

/// Record the session or subagent in the agent registry. Best effort and silent: identity comes
/// from the hook payload (session_id, agent_id, transcript) or, for a dispatched worker, from
/// `FOXDB_AGENT`. Liveness for these is the transcript's mtime.
fn register(event: &str, input: &Value, h: Harness) {
    // Never create a store from a hook: no store, no registry.
    if !db::db_path().exists() {
        return;
    }
    let cwd = cwd_of(input);
    let Ok(w) = db::open_rw() else { return };
    let pid = project::find(&w, &project::locate(&cwd)).ok().flatten().map(|p| p.id);
    let cwd_s = cwd.to_string_lossy().into_owned();
    let sid = input["session_id"].as_str().filter(|s| !s.is_empty());
    let env_agent = std::env::var("FOXDB_AGENT").ok().filter(|s| !s.is_empty());
    let r = match event {
        "session-start" => {
            let id = env_agent.as_deref().or(sid);
            let Some(id) = id else { return };
            let parent = std::env::var("FOXDB_PARENT").ok().filter(|s| !s.is_empty());
            // A session's liveness is its harness process, which its MCP server and shell
            // commands also descend from (so they resolve to this id). A dispatched worker keeps
            // its supervisor's pid.
            let hpid = if env_agent.is_none() { crate::agents::harness_pid() } else { None };
            crate::agents::register(
                &w,
                &crate::agents::Reg {
                    id,
                    harness: h.name(),
                    model: input["model"].as_str(),
                    role: env_agent.is_none().then_some("session"),
                    parent: parent.as_deref(),
                    project_id: pid,
                    cwd: Some(&cwd_s),
                    session: sid,
                    transcript: input["transcript_path"].as_str(),
                    pid: hpid,
                    ..Default::default()
                },
            )
        }
        "subagent-start" => {
            let Some(id) = input["agent_id"].as_str().filter(|s| !s.is_empty()) else { return };
            let parent = env_agent.as_deref().or(sid);
            crate::agents::register(
                &w,
                &crate::agents::Reg {
                    id,
                    harness: h.name(),
                    role: input["agent_type"].as_str().filter(|s| !s.is_empty()),
                    parent,
                    project_id: pid,
                    cwd: Some(&cwd_s),
                    transcript: input["agent_transcript_path"].as_str(),
                    ..Default::default()
                },
            )
        }
        _ => Ok(()),
    };
    let _ = r;
}

/// Run one hook. Never errors: a failure is an empty string (the hook then adds nothing).
pub fn run(event: &str, stdin: &str, h: Harness) -> String {
    run_with(crate::config::get(), event, stdin, h)
}

/// [`run`] with explicit settings (the opt-in tool hooks read `[hooks]`).
pub fn run_with(cfg: &crate::config::Config, event: &str, stdin: &str, h: Harness) -> String {
    let Ok(input) = serde_json::from_str::<Value>(stdin) else { return String::new() };
    if matches!(event, "session-start" | "subagent-start") {
        register(event, &input, h);
    }
    let r = match event {
        "session-start" => session_start(&input, h),
        "subagent-start" => subagent_start(cfg, &input, h),
        // Opt-in (`[hooks] stop_check`): off, Stop prints nothing and SubagentStop only saves.
        "stop" => Ok(stop_check(cfg, &input).unwrap_or_default()),
        "subagent-stop" => subagent_stop(cfg, &input, h),
        "pre-compact" => pre_compact(&input, h),
        // Opt-in hooks only add context when they have a result.
        "pre-tool-use" if cfg.hooks.fact_gate => crate::toolhook::fact_gate(&input).map(|t| context("PreToolUse", t)),
        "post-tool-use" if cfg.hooks.grep_context => crate::toolhook::grep_context(&input).map(|t| context("PostToolUse", t)),
        "user-prompt-submit" if cfg.hooks.prompt_context => crate::toolhook::prompt_context(&input).map(|t| context("UserPromptSubmit", t)),
        _ => Ok(String::new()),
    };
    r.unwrap_or_default()
}
