//! Handoff packets: a compact, redacted, budgeted summary of where a piece of work stands, stored
//! as a `packet` handoff so any agent in any harness can take it over.
//!
//! A packet holds the goal, the state, decisions with reasons, touched files and symbols, open
//! questions, next steps and evidence handles. It never inlines a diff: touched files come from
//! `git status`, the size from `git diff --stat`, and symbols from the changed hunk ranges (only
//! `@@` headers are read), so code and secrets in the diff stay out. `fox_read` expands handles.

use crate::{handoff, hook, project, query, redact, sessions};
use anyhow::Result;
use rusqlite::{Connection, params};
use std::fmt::Write;
use std::path::Path;
use std::process::Command;

#[derive(Default)]
pub struct Spec<'a> {
    pub goal: Option<&'a str>,
    pub state: Option<&'a str>,
    pub next: Vec<&'a str>,
    pub questions: Vec<&'a str>,
    pub parent: Option<i64>,
    pub agent: Option<&'a str>,
    pub harness: Option<&'a str>,
    pub transcript: Option<&'a Path>,
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let o = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Touched paths with their `git status` code, tracked first. At most 20.
fn touched(root: &Path) -> Vec<(String, String)> {
    let Some(s) = git(root, &["status", "--porcelain=v1", "--no-renames"]) else { return vec![] };
    let mut v: Vec<(String, String)> =
        s.lines().filter(|l| l.len() > 3).map(|l| (l[..2].trim().to_string(), l[3..].trim_matches('"').to_string())).collect();
    v.sort_by_key(|(code, _)| code == "??");
    v.truncate(20);
    v
}

/// Changed line ranges per file, from `@@` hunk headers only.
fn hunks(root: &Path) -> Vec<(String, i64, i64)> {
    let Some(d) = git(root, &["diff", "-U0", "--no-color", "--no-ext-diff", "HEAD"]) else { return vec![] };
    let mut out = vec![];
    let mut file = String::new();
    for l in d.lines() {
        if let Some(f) = l.strip_prefix("+++ b/") {
            file = f.to_string();
        } else if l.starts_with("@@")
            && let Some(plus) = l.split_whitespace().find(|w| w.starts_with('+'))
        {
            let (a, n) = plus[1..].split_once(',').unwrap_or((&plus[1..], "1"));
            let (a, n): (i64, i64) = (a.parse().unwrap_or(0), n.parse().unwrap_or(1));
            out.push((file.clone(), a, a + n.max(1) - 1));
        }
    }
    out
}

/// Symbols whose span overlaps a changed hunk: `s:1a alpha src/lib.rs:1`. At most 12.
fn symbols(c: &Connection, pid: i64, hunks: &[(String, i64, i64)]) -> Vec<String> {
    let Ok(mut s) = c.prepare_cached(
        "SELECT id, name, line FROM node WHERE project_id = ?1 AND kind = 'symbol' AND path = ?2
         AND line <= ?4 AND COALESCE(end_line, line) >= ?3 ORDER BY line LIMIT 6",
    ) else {
        return vec![];
    };
    let mut out: Vec<String> = vec![];
    for (f, a, b) in hunks {
        let rows = s.query_map(params![pid, f, a, b], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)));
        for (id, name, line) in rows.into_iter().flatten().flatten() {
            let l = format!("{} {name} {f}:{line}", query::handle("symbol", id));
            if !out.contains(&l) && out.len() < 12 {
                out.push(l);
            }
        }
    }
    out
}

/// Recent decisions with the reason (the text after the title line).
fn decisions(c: &Connection, pid: i64) -> Vec<String> {
    let Ok(mut s) =
        c.prepare_cached("SELECT id, title, body FROM handoff WHERE project_id = ?1 AND kind = 'decision' ORDER BY id DESC LIMIT 6")
    else {
        return vec![];
    };
    s.query_map([pid], |r| {
        let (id, title, body): (i64, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
        let why: String = body.lines().skip(1).map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
        let mut l = format!("  {} {}", handoff::handle(id), query::clip(&title, 90));
        if !why.is_empty() {
            let _ = write!(l, " — {}", query::clip(&why, 120));
        }
        Ok(l)
    })
    .map(|it| it.flatten().collect())
    .unwrap_or_default()
}

/// Open (unclaimed) requests, as questions for whoever takes over.
fn open_requests(c: &Connection, pid: i64) -> Vec<String> {
    let Ok(mut s) = c.prepare_cached(
        "SELECT id, title FROM handoff WHERE project_id = ?1 AND kind = 'request' AND claimed_by IS NULL ORDER BY id DESC LIMIT 4",
    ) else {
        return vec![];
    };
    s.query_map([pid], |r| Ok(format!("  - {} {}", handoff::handle(r.get(0)?), query::clip(&r.get::<_, String>(1)?, 90))))
        .map(|it| it.flatten().collect())
        .unwrap_or_default()
}

/// Recent reports and findings of this work as evidence handles: posted by the agent or its
/// descendants, in its session, or answering its parent handoff. With none of those to go on
/// (an anonymous packet), the project's latest.
fn evidence(c: &Connection, pid: i64, agent: Option<&str>, session: Option<&str>, parent: Option<i64>) -> Vec<String> {
    let scoped = agent.is_some() || session.is_some() || parent.is_some();
    let Ok(mut s) = c.prepare_cached(
        "WITH RECURSIVE tree(id) AS (SELECT ?2 WHERE ?2 IS NOT NULL UNION SELECT a.id FROM agent a JOIN tree t ON a.parent = t.id)
         SELECT id, kind FROM handoff WHERE project_id = ?1 AND kind IN ('report','finding')
           AND (?5 = 0 OR agent IN (SELECT id FROM tree) OR (?3 IS NOT NULL AND session = ?3) OR (?4 IS NOT NULL AND parent_id = ?4))
         ORDER BY id DESC LIMIT 6",
    ) else {
        return vec![];
    };
    s.query_map(params![pid, agent, session, parent, scoped as i64], |r| {
        Ok(format!("{} {}", handoff::handle(r.get(0)?), r.get::<_, String>(1)?))
    })
    .map(|it| it.flatten().collect())
    .unwrap_or_default()
}

/// Digest a transcript of either harness (`.jsonl`, or Codex `.jsonl.zst`).
pub fn digest(path: &Path) -> Option<sessions::Digest> {
    let name = path.file_name()?.to_string_lossy();
    let codex_first = name.starts_with("rollout-") || name.ends_with(".zst");
    type Digester = fn(&Path) -> Option<sessions::Digest>;
    let (a, b): (Digester, Digester) =
        if codex_first { (sessions::digest_codex, sessions::digest_claude) } else { (sessions::digest_claude, sessions::digest_codex) };
    a(path).filter(|d| d.turns > 0 || !d.title.is_empty()).or_else(|| b(path))
}

fn session_handle(c: &Connection, harness: &str, sid: &str) -> Option<String> {
    c.query_row(
        "SELECT id FROM node WHERE kind = 'session' AND source = ?1 AND json_extract(meta, '$.sid') = ?2",
        params![harness, sid],
        |r| r.get::<_, i64>(0),
    )
    .ok()
    .map(|id| query::handle("session", id))
}

/// Build and store a packet for project `p`. Returns its handoff id.
pub fn create(w: &Connection, p: &project::Project, s: &Spec) -> Result<i64> {
    let d = s.transcript.and_then(digest);
    // A registered agent (a dispatched worker has no transcript) still says what it was doing:
    // its task, its last output and its status.
    let reg = s.agent.and_then(|a| crate::agents::get(w, a));
    let task_h = reg.as_ref().and_then(|r| r.task.as_deref()).and_then(handoff::parse_handle);
    let reg_goal = reg.as_ref().and_then(|r| r.task.clone()).map(|t| match handoff::parse_handle(&t) {
        Some(h) => {
            let title: String = w.query_row("SELECT title FROM handoff WHERE id = ?1", [h], |r| r.get(0)).unwrap_or_default();
            format!("{} {title}", handoff::handle(h))
        }
        None => t,
    });
    let reg_state = reg.as_ref().map(|r| {
        let mut st = format!("{} {} {} {}", r.id, r.harness, r.role.as_deref().unwrap_or("-"), r.status);
        if let Some(o) = crate::orch::last_output(&r.id) {
            let _ = write!(st, "; last output: {}", query::clip(&o, 400));
        }
        st
    });
    let goal = s
        .goal
        .map(str::to_string)
        .or_else(|| d.as_ref().map(|d| d.title.clone()).filter(|g| !g.trim().is_empty()))
        .or(reg_goal)
        .filter(|g| !g.trim().is_empty());
    let state = s
        .state
        .map(str::to_string)
        .or_else(|| d.as_ref().filter(|d| !d.last.is_empty()).map(|d| format!("last answer: {}", d.last)))
        .or(reg_state);
    let parent = s.parent.or(task_h);
    // A registered worker's exact change: its base commit to its head (or the live tree).
    let change = reg.as_ref().and_then(|r| {
        let base = r.base_commit.clone()?;
        let head = r.head_commit.clone().or_else(|| crate::gitsnap::snapshot(Path::new(r.cwd.as_deref()?)))?;
        Some((base, head))
    });
    let mut b = String::new();
    let _ = writeln!(b, "goal: {}", query::clip(goal.as_deref().unwrap_or("(unstated)"), 400));
    if let Some(st) = &state {
        let _ = writeln!(b, "state: {}", query::clip(st, 600));
    }
    if let Some((base, head)) = &change {
        let _ = writeln!(b, "change: git diff {base} {head}");
    }
    let ds = decisions(w, p.id);
    if !ds.is_empty() {
        b.push_str("decisions:\n");
        for l in ds {
            b.push_str(&l);
            b.push('\n');
        }
    }
    let t = touched(&p.root);
    let mut files: Vec<String> = t.iter().map(|(code, f)| format!("{f} ({code})")).collect();
    if let Some(d) = &d {
        for a in &d.anchors {
            if files.len() < 20 && !files.iter().any(|f| f.starts_with(a.as_str())) {
                files.push(a.clone());
            }
        }
    }
    if !files.is_empty() {
        let _ = writeln!(b, "files: {}", files.join(", "));
    }
    if let Some(stat) =
        git(&p.root, &["diff", "--stat", "HEAD"]).and_then(|s| s.lines().last().map(|l| l.trim().to_string())).filter(|l| !l.is_empty())
    {
        let _ = writeln!(b, "diffstat: {stat}");
    }
    let syms = symbols(w, p.id, &hunks(&p.root));
    if !syms.is_empty() {
        let _ = writeln!(b, "symbols: {}", syms.join(", "));
    }
    let mut qs: Vec<String> = s.questions.iter().filter(|q| !q.trim().is_empty()).map(|q| format!("  - {}", q.trim())).collect();
    qs.extend(open_requests(w, p.id));
    if !qs.is_empty() {
        b.push_str("open questions:\n");
        for q in qs {
            b.push_str(&q);
            b.push('\n');
        }
    }
    let next: Vec<&str> = s.next.iter().flat_map(|n| n.lines()).map(str::trim).filter(|n| !n.is_empty()).collect();
    if !next.is_empty() {
        b.push_str("next:\n");
        for n in next {
            let _ = writeln!(b, "  - {n}");
        }
    }
    let mut ev = vec![];
    if let Some(d) = &d {
        match session_handle(w, d.harness, &d.sid) {
            Some(x) => ev.push(format!("{x} {} session", d.harness)),
            None => ev.push(format!("{} session {}", d.harness, d.sid)),
        }
    }
    ev.extend(evidence(w, p.id, s.agent, d.as_ref().map(|d| d.sid.as_str()), parent));
    if !ev.is_empty() {
        let _ = writeln!(b, "evidence: {}", ev.join(", "));
    }
    let body = redact::redact(&b);
    let title = format!("packet: {}", query::clip(goal.as_deref().unwrap_or("(unstated)"), 80));
    // The same packet again (a rerun switch from one transcript) is the open one already stored.
    let stored: String = redact::redact(body.trim()).chars().take(handoff::BODY_MAX).collect();
    if let Ok(id) = w.query_row(
        "SELECT id FROM handoff WHERE project_id = ?1 AND kind = 'packet' AND scope = 'packet' AND claimed_by IS NULL
           AND body = ?2 AND agent IS ?3 AND parent_id IS ?4 ORDER BY id DESC LIMIT 1",
        params![p.id, stored, s.agent, parent],
        |r| r.get::<_, i64>(0),
    ) {
        return Ok(id);
    }
    handoff::post(
        w,
        &handoff::Post {
            project_id: p.id,
            kind: "packet",
            text: &body,
            title: Some(&title),
            scope: Some("packet"),
            harness: s.harness.or(d.as_ref().map(|d| d.harness)),
            session: d.as_ref().map(|d| d.sid.as_str()),
            agent: s.agent,
            parent,
            ..Default::default()
        },
    )
}

/// The packet as prompt text: deterministic, redacted, capped at `cap` characters on a line
/// boundary. `--for` only picks the cap; there is one format.
pub fn render(c: &Connection, id: i64, cap: usize) -> Result<String> {
    let h = handoff::handle(id);
    let mut s = format!("foxdb handoff {h}. Take it once: fox_memory op=take id={h} (or `fox handoff take {h}`).\n");
    // The claim line changes when taken; the packet itself does not.
    let shown = handoff::show(c, id)?;
    for l in shown.lines().filter(|l| !l.starts_with("claimed by ")) {
        s.push_str(l);
        s.push('\n');
    }
    Ok(hook::cap_lines(&redact::redact(&s), cap))
}

/// Cap for a packet read inside a T3 Code thread. T3 already carries the conversation across
/// providers in its own portable handoff, so foxdb's packet stays a short pointer to the board
/// (its handles recover the rest with fox_read) instead of a second copy of the history.
pub const T3_CAP_CHARS: usize = 2_000;

/// The character cap for `--for` and `--budget` (tokens).
pub fn cap_for(harness: Option<&str>, budget: Option<usize>) -> usize {
    let h = match harness {
        Some("claude") => hook::Harness::Claude.cap_chars(),
        Some("codex") => hook::Harness::Codex.cap_chars(),
        Some("t3") => T3_CAP_CHARS,
        _ => 4_000,
    };
    budget.map(|b| (b * 4).min(h)).unwrap_or(h)
}
