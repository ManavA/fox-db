//! `fox hook <event>`: Claude Code / Codex command hooks. Input is the hook's JSON on stdin; output
//! is the `hookSpecificOutput` JSON form, or nothing. Hooks fail open: any error prints nothing.
//!
//! Everything a hook injects is a deterministic function of the index (no clocks, no relative
//! ages), so siblings and resumed sessions share the prompt-cache prefix.

use crate::{db, project, query};
use anyhow::Result;
use rusqlite::Connection;
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

/// Save a handoff packet from the transcript before the harness compacts it. Prints nothing.
fn pre_compact(input: &Value, h: Harness) -> Result<String> {
    if !db::db_path().exists() {
        return Ok(String::new());
    }
    let w = db::open_rw()?;
    let Some(p) = project::find(&w, &project::locate(&cwd_of(input)))? else { return Ok(String::new()) };
    let owner = owner_of(input);
    let tx = input["transcript_path"].as_str().map(PathBuf::from);
    let spec = crate::packet::Spec { agent: owner.as_deref(), harness: Some(h.name()), transcript: tx.as_deref(), ..Default::default() };
    let id = crate::packet::create(&w, &p, &spec)?;
    w.execute("UPDATE handoff SET scope = 'compact' WHERE id = ?1", [id])?;
    Ok(String::new())
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
fn subagent_start(input: &Value, h: Harness) -> Result<String> {
    let agent_type = input["agent_type"].as_str().unwrap_or("");
    let agent_id = input["agent_id"].as_str().or_else(|| input["agent_name"].as_str()).unwrap_or("");
    let cwd = cwd_of(input);
    if skip_agent_with(crate::config::get(), agent_type, agent_id, Some(&cwd)) {
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
    // The savings line is volatile and of no use to a helper.
    text = text.lines().filter(|l| !l.starts_with("fox saved")).map(|l| format!("{l}\n")).collect();
    // A pointer to the parent's frozen board snapshot, not the live board: siblings started
    // before and after a post still get the same bytes.
    if let Ok(w) = db::open_rw()
        && let Some(sid) = input["session_id"].as_str().filter(|s| !s.is_empty())
        && let Ok(h) =
            crate::handoff::snapshot(&w, p.id, &format!("session:{sid}"), Some(&owner_of(input).unwrap_or_else(|| sid.to_string())))
    {
        let h = crate::handoff::handle(h);
        text.push_str(&format!(
            "parent packet {h} - open decisions/requests (fox_read target={h}; new posts: fox_memory op=read since=auto)\n"
        ));
    }
    text.push_str("Report findings with fox_memory op=post; your final answer is saved for the parent.\n");
    Ok(context("SubagentStart", cap_marked(&text, SUBAGENT_BRIEF_CHARS.min(h.cap_chars()))))
}

/// Save the subagent's final report as a handoff row. Prints nothing: SubagentStop output can
/// block the agent from stopping.
fn subagent_stop(input: &Value, h: Harness) -> Result<String> {
    let Some(msg) = input["last_assistant_message"].as_str().filter(|m| !m.trim().is_empty()) else { return Ok(String::new()) };
    if input["stop_hook_active"] == true {
        return Ok(String::new());
    }
    let cwd = cwd_of(input);
    let w = db::open_rw()?;
    let Some(p) = project::find(&w, &project::locate(&cwd))? else { return Ok(String::new()) };
    let agent = input["agent_type"].as_str().filter(|s| !s.is_empty()).or_else(|| input["agent_id"].as_str());
    let report = crate::handoff::post(
        &w,
        &crate::handoff::Post {
            project_id: p.id,
            kind: "report",
            text: msg,
            harness: Some(h.name()),
            session: input["session_id"].as_str(),
            agent,
            evidence: input["agent_transcript_path"].as_str(),
            ..Default::default()
        },
    )?;
    if let Some(id) = input["agent_id"].as_str().filter(|s| !s.is_empty()) {
        crate::agents::finish(&w, id, "done", Some(report))?;
    }
    Ok(String::new())
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
        "subagent-start" => subagent_start(&input, h),
        "subagent-stop" => subagent_stop(&input, h),
        "pre-compact" => pre_compact(&input, h),
        // Opt-in (`[hooks] grep_context` / `prompt_context`): off, they print nothing.
        "post-tool-use" if cfg.hooks.grep_context => crate::toolhook::grep_context(&input).map(|t| context("PostToolUse", t)),
        "user-prompt-submit" if cfg.hooks.prompt_context => crate::toolhook::prompt_context(&input).map(|t| context("UserPromptSubmit", t)),
        _ => Ok(String::new()),
    };
    r.unwrap_or_default()
}
