//! The handoff board: findings, claims, requests, decisions and subagent reports that agents
//! post for each other, scoped to a project. Text is redacted on the way in and rows stay short
//! plain text. A request can be picked up exactly once; `take` adds a lease that another agent may
//! retake once it expires. Rows carry a topic and a parent handoff (lineage).
//!
//! Swarm entries (`fact`, `failed`, `summary`, `question`) are short ([`ENTRY_MAX`]) and attach
//! files and symbols by handle, never by pasted content (see [`attachments`]); the board shows
//! their whole text on one row, so an agent's read of what is new to it stays one small call.

use crate::{db, query, redact};
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::fmt::Write;

pub const KINDS: &[&str] = &["finding", "claim", "request", "decision", "report", "packet", "fact", "failed", "summary", "question"];

/// The swarm entry kinds: a fact found, an approach that failed (and why), a worker's summary,
/// an open question. Short, whole on their board row.
pub const SWARM_KINDS: &[&str] = &["fact", "failed", "summary", "question"];

/// Longest swarm entry: a claim plus handles, not a dump.
pub const ENTRY_MAX: usize = 600;

/// Most attachments kept on one entry.
pub const ATTACH_MAX: usize = 8;

/// Longest attachment list kept on one swarm entry, in bytes: handles and paths, not text.
pub const EVIDENCE_MAX: usize = 400;

/// Longest body kept; reports are summaries, not dumps.
pub const BODY_MAX: usize = 4_000;

#[derive(Default)]
pub struct Post<'a> {
    pub project_id: i64,
    pub kind: &'a str,
    pub text: &'a str,
    pub title: Option<&'a str>,
    pub evidence: Option<&'a str>,
    pub scope: Option<&'a str>,
    pub harness: Option<&'a str>,
    pub session: Option<&'a str>,
    pub agent: Option<&'a str>,
    pub topic: Option<&'a str>,
    pub parent: Option<i64>,
}

pub fn handle(id: i64) -> String {
    format!("h:{id:x}")
}

pub fn parse_handle(h: &str) -> Option<i64> {
    i64::from_str_radix(h.trim().strip_prefix("h:")?, 16).ok()
}

pub fn post(w: &Connection, p: &Post) -> Result<i64> {
    if !KINDS.contains(&p.kind) {
        bail!("kind must be one of {}", KINDS.join("|"));
    }
    let swarm = SWARM_KINDS.contains(&p.kind);
    let text = redact::redact(p.text.trim());
    let text: String = if swarm { query::clip(&text, ENTRY_MAX) } else { text.chars().take(BODY_MAX).collect() };
    if text.is_empty() {
        bail!("text is empty");
    }
    let title = p.title.map(|t| redact::redact(t.trim())).unwrap_or_else(|| text.lines().next().unwrap_or("").to_string());
    let title: String = title.chars().take(100).collect();
    let ev = match p.evidence {
        Some(e) if swarm => Some(attachments(w, p.project_id, e, true)?).filter(|e| !e.is_empty()),
        e => e.map(redact::redact),
    };
    let topic = p.topic.map(|t| t.trim().chars().take(40).collect::<String>()).filter(|t| !t.is_empty());
    w.execute(
        "INSERT INTO handoff(project_id, kind, title, body, scope, harness, session, agent, evidence, created_at, topic, parent_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![p.project_id, p.kind, title, text, p.scope, p.harness, p.session, p.agent, ev, db::now(), topic, p.parent],
    )?;
    let id = w.last_insert_rowid();
    db::bump_generation(w)?;
    Ok(id)
}

/// Newest first: `h:1f request 2026-11-16 hana-impl  Title  [claimed by x]`.
pub fn read(c: &Connection, pid: i64, kind: Option<&str>, q: Option<&str>, budget: usize, cursor: usize) -> Result<String> {
    let like = q.map(|q| format!("%{}%", q.replace('%', "")));
    let mut s = c.prepare_cached(
        "SELECT id, kind, title, created_at, COALESCE(agent, harness, ''), claimed_by, evidence, topic, body FROM handoff
         WHERE project_id = ?1 AND (?2 IS NULL OR kind = ?2) AND (?3 IS NULL OR title LIKE ?3 OR body LIKE ?3)
           AND scope IS NOT 'snapshot'
         ORDER BY id DESC LIMIT 200",
    )?;
    let rows: Vec<String> = s.query_map(params![pid, kind, like], row_line)?.flatten().collect();
    if rows.is_empty() {
        return Ok("no handoffs yet (fox_memory op=post)\n".into());
    }
    Ok(query::page(&rows, cursor, budget))
}

fn row_line(r: &rusqlite::Row) -> rusqlite::Result<String> {
    let (id, kind, title, at, who): (i64, String, String, i64, String) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
    let (claimed, ev, topic): (Option<String>, Option<String>, Option<String>) = (r.get(5)?, r.get(6)?, r.get(7)?);
    let mut l = format!("{} {kind} {} {}", handle(id), crate::mcp::ymd(at), if who.is_empty() { "-" } else { &who });
    if let Some(t) = topic {
        let _ = write!(l, " #{t}");
    }
    if SWARM_KINDS.contains(&kind.as_str()) {
        // A swarm entry is short: its whole text on one line, and its handles in full.
        let body: String = r.get(8)?;
        let _ = write!(l, "  {}", body.split_whitespace().collect::<Vec<_>>().join(" "));
        if let Some(e) = ev.filter(|e| !e.is_empty()) {
            let _ = write!(l, "  @{e}");
        }
    } else {
        let _ = write!(l, "  {}", query::clip(&title, 90));
        if let Some(e) = ev.filter(|e| !e.is_empty()) {
            let _ = write!(l, "  @{}", query::clip(&e, 60));
        }
    }
    if let Some(cb) = claimed {
        let _ = write!(l, "  [claimed by {cb}]");
    }
    Ok(l)
}

/// Where a `read --since` starts.
pub enum Since<'a> {
    /// After this handoff id (stateless).
    After(i64),
    /// After this agent's stored cursor, which then advances. The agent's own posts are skipped.
    Auto(&'a str),
}

/// Board rows newer than a cursor, oldest first, ending in `since=h:N` for the next call.
pub fn read_since(w: &Connection, pid: i64, since: Since, kind: Option<&str>, topic: Option<&str>, budget: usize) -> Result<String> {
    let (after, me, fresh) = match since {
        Since::After(n) => (n, None, false),
        Since::Auto(a) => {
            let n: Option<i64> = w
                .query_row("SELECT last FROM read_cursor WHERE agent = ?1 AND project_id = ?2", params![a, pid], |r| r.get(0))
                .optional()?;
            (n.unwrap_or(0), Some(a), n.is_none())
        }
    };
    if fresh {
        return read_newest(w, pid, me.unwrap_or_default(), kind, topic, budget);
    }
    let mut s = w.prepare_cached(
        "SELECT id, kind, title, created_at, COALESCE(agent, harness, ''), claimed_by, evidence, topic, body FROM handoff
         WHERE project_id = ?1 AND id > ?2 AND (?3 IS NULL OR kind = ?3) AND (?4 IS NULL OR topic = ?4)
           AND (?5 IS NULL OR agent IS NULL OR agent != ?5) AND scope IS NOT 'snapshot'
         ORDER BY id LIMIT 200",
    )?;
    let rows: Vec<(i64, String)> =
        s.query_map(params![pid, after, kind, topic, me], |r| Ok((r.get::<_, i64>(0)?, row_line(r)?)))?.flatten().collect();
    // Only what fits is marked read: the cursor stops at the last row shown.
    let (mut text, mut last, mut used) = (String::new(), after, 0usize);
    let max = budget.max(32) * 4;
    let mut shown = 0;
    for (id, l) in &rows {
        if used + l.len() + 1 > max {
            // The first row always shows, clipped, so a row longer than the budget cannot
            // freeze the cursor (`fox_read` of its handle gives all of it).
            if shown == 0 {
                text.push_str(&query::clip(l, max.saturating_sub(3)));
                text.push('\n');
                last = *id;
                shown = 1;
            }
            break;
        }
        text.push_str(l);
        text.push('\n');
        used += l.len() + 1;
        last = *id;
        shown += 1;
    }
    if shown == 0 {
        text.push_str("nothing new\n");
    } else if shown < rows.len() {
        let _ = writeln!(text, "+{} more", rows.len() - shown);
    }
    let next = last;
    if let Some(a) = me {
        w.execute(
            "INSERT INTO read_cursor(agent, project_id, last) VALUES (?1, ?2, ?3)
             ON CONFLICT(agent, project_id) DO UPDATE SET last = excluded.last",
            params![a, pid, next],
        )?;
    }
    let _ = writeln!(text, "since={}", handle(next));
    Ok(text)
}

/// An agent's first `since=auto` read: the newest rows that fit, shown oldest first, and the
/// cursor set past everything, so a new agent sees the latest decisions instead of the oldest rows.
fn read_newest(w: &Connection, pid: i64, me: &str, kind: Option<&str>, topic: Option<&str>, budget: usize) -> Result<String> {
    // The cursor first, and only rows up to it: a post landing between the two reads is left for
    // the next read instead of being skipped.
    let last: i64 = w.query_row("SELECT COALESCE(MAX(id), 0) FROM handoff WHERE project_id = ?1", [pid], |r| r.get(0))?;
    let mut s = w.prepare_cached(
        "SELECT id, kind, title, created_at, COALESCE(agent, harness, ''), claimed_by, evidence, topic, body FROM handoff
         WHERE project_id = ?1 AND (?2 IS NULL OR kind = ?2) AND (?3 IS NULL OR topic = ?3)
           AND (agent IS NULL OR agent != ?4) AND scope IS NOT 'snapshot' AND id <= ?5
         ORDER BY id DESC LIMIT 200",
    )?;
    let rows: Vec<(i64, String)> =
        s.query_map(params![pid, kind, topic, me, last], |r| Ok((r.get::<_, i64>(0)?, row_line(r)?)))?.flatten().collect();
    let max = budget.max(32) * 4;
    let mut used = 0usize;
    let mut shown: Vec<String> = vec![];
    for (_, l) in &rows {
        if used + l.len() + 1 > max {
            // The newest row always shows, clipped (see `read_since`).
            if shown.is_empty() {
                shown.push(query::clip(l, max.saturating_sub(3)));
            }
            break;
        }
        used += l.len() + 1;
        shown.push(l.clone());
    }
    let mut text = String::new();
    if shown.is_empty() {
        text.push_str("nothing new\n");
    } else if shown.len() < rows.len() {
        let _ = writeln!(text, "+{} older (fox_memory op=read)", rows.len() - shown.len());
    }
    for l in shown.into_iter().rev() {
        text.push_str(&l);
        text.push('\n');
    }
    w.execute(
        "INSERT INTO read_cursor(agent, project_id, last) VALUES (?1, ?2, ?3)
         ON CONFLICT(agent, project_id) DO UPDATE SET last = excluded.last",
        params![me, pid, last],
    )?;
    let _ = writeln!(text, "since={}", handle(last));
    Ok(text)
}

/// What a handle names: `f:1a file lib.rs src/lib.rs:1`, or `h:2 fact <title>`. None when the
/// text is not a handle or names nothing.
pub fn resolve(c: &Connection, h: &str) -> Option<String> {
    let (pfx, hex) = h.split_once(':')?;
    let id = i64::from_str_radix(hex, 16).ok()?;
    match pfx {
        "h" => c
            .query_row("SELECT kind, title FROM handoff WHERE id = ?1", [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .ok()
            .map(|(k, t)| format!("{h} {k} {}", query::clip(&t, 80))),
        p if p.len() == 1 && "sfcdmagx".contains(p) => c
            .query_row("SELECT kind, name, COALESCE(path, ''), COALESCE(line, 0) FROM node WHERE id = ?1", [id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?))
            })
            .ok()
            .map(|(k, n, p, l)| match (p.is_empty(), l > 0) {
                (true, _) => format!("{h} {k} {n}"),
                (false, false) => format!("{h} {k} {n} {p}"),
                (false, true) => format!("{h} {k} {n} {p}:{l}"),
            }),
        _ => None,
    }
}

/// Does `t` look like a handle (`f:1a`, `h:2`), whether or not it names anything?
fn looks_like_handle(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() > 2 && b[1] == b':' && b"hsfcdmagx".contains(&b[0]) && b[2..].iter().all(u8::is_ascii_hexdigit)
}

/// A swarm entry's attachments as handles: a known handle stays, the path of an indexed file
/// becomes its `f:` handle (`src/a.rs` and `src/a.rs:40` alike), another path is kept as written, and
/// other words are dropped. Split on whitespace and commas; at most [`ATTACH_MAX`] items and
/// [`EVIDENCE_MAX`] bytes. Handle prefixes are case-insensitive. An unknown handle is an error
/// when `strict` (an agent's own post) and dropped otherwise, as are unindexed paths (handles
/// gathered from a report's prose).
pub fn attachments(c: &Connection, pid: i64, raw: &str, strict: bool) -> Result<String> {
    let mut out: Vec<String> = vec![];
    for tok in raw.split(|ch: char| ch.is_whitespace() || ch == ',') {
        let tok = tok.trim_matches(|ch: char| "()[]<>`'\";.".contains(ch));
        if tok.is_empty() || out.len() >= ATTACH_MAX {
            continue;
        }
        // `F:1A` is the handle `f:1a`.
        let lower = tok.to_ascii_lowercase();
        let t = if looks_like_handle(&lower) {
            if resolve(c, &lower).is_none() {
                if strict {
                    bail!("unknown handle {tok}: attach handles from fox_find, fox_graph or fox_read rows");
                }
                continue;
            }
            lower
        } else if !looks_like_path(tok) {
            // Prose is not an attachment: it would carry content past the entry cap.
            continue;
        } else {
            let path = tok.split(':').next().unwrap_or(tok);
            let id: Option<i64> = c
                .query_row("SELECT id FROM node WHERE project_id = ?1 AND kind = 'file' AND path = ?2", params![pid, path], |r| r.get(0))
                .optional()?;
            match id {
                Some(id) => query::handle("file", id),
                None if strict => redact::redact(tok),
                None => continue,
            }
        };
        if !out.contains(&t) && out.iter().map(|x| x.len() + 1).sum::<usize>() + t.len() <= EVIDENCE_MAX {
            out.push(t);
        }
    }
    Ok(out.join(" "))
}

/// A path or `path:line` someone may attach: no wider than a path, with a `/` or an extension.
fn looks_like_path(t: &str) -> bool {
    t.len() <= 200
        && (t.contains('/')
            || t.rsplit_once('.').is_some_and(|(a, e)| !a.is_empty() && (1..=6).contains(&e.split(':').next().unwrap_or("").len())))
}

/// Child handoffs (reports for a task, packets that continue a packet).
pub fn children(c: &Connection, id: i64) -> Vec<(i64, String)> {
    let Ok(mut s) = c.prepare_cached("SELECT id, kind FROM handoff WHERE parent_id = ?1 ORDER BY id") else { return vec![] };
    s.query_map([id], |r| Ok((r.get(0)?, r.get(1)?))).map(|it| it.flatten().collect()).unwrap_or_default()
}

/// Full text of one handoff.
pub fn show(c: &Connection, id: i64) -> Result<String> {
    let r = c
        .query_row(
            "SELECT kind, title, body, created_at, COALESCE(agent, harness, ''), claimed_by, evidence, session, parent_id, lease_until, topic
             FROM handoff WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, title, body, at, who, claimed, ev, session, parent, lease, topic)) = r else { bail!("no handoff {}", handle(id)) };
    let mut s = format!("{} {kind} {} {}", handle(id), crate::mcp::ymd(at), if who.is_empty() { "-" } else { &who });
    if let Some(t) = topic {
        let _ = write!(s, " #{t}");
    }
    let _ = writeln!(s, "  {title}");
    if let Some(p) = parent {
        let _ = writeln!(s, "parent: {}", handle(p));
    }
    let kids = children(c, id);
    if !kids.is_empty() {
        let k: Vec<String> = kids.iter().map(|(i, k)| format!("{} {k}", handle(*i))).collect();
        let _ = writeln!(s, "children: {}", k.join(", "));
    }
    if let Some(e) = ev {
        let _ = writeln!(s, "evidence: {e}");
        let found: Vec<String> = e.split_whitespace().filter_map(|h| resolve(c, h)).collect();
        if !found.is_empty() {
            let _ = writeln!(s, "attached (fox_read target=<handle>):");
            for f in found {
                let _ = writeln!(s, "  {f}");
            }
        }
    }
    if let Some(x) = session {
        let _ = writeln!(s, "session: {x}");
    }
    if let Some(cb) = claimed {
        match lease {
            Some(t) => {
                let _ = writeln!(s, "claimed by {cb}, lease until {}", crate::agents::hms(t));
            }
            None => {
                let _ = writeln!(s, "claimed by {cb}");
            }
        }
    }
    s.push_str(&body);
    s.push('\n');
    Ok(s)
}

/// Claim a handoff exactly once, for good.
pub fn pickup(w: &Connection, id: i64, by: &str) -> Result<String> {
    take(w, id, by, None)
}

/// Claim a handoff exactly once. With `lease_secs` the claim expires unless renewed (the same
/// agent taking again extends it), and an expired claim can be retaken by exactly one agent: the
/// retake compares-and-sets the old `(claimed_by, claimed_at)` inside `BEGIN IMMEDIATE`.
/// A refusal starts `refused h:N <reason>`: `not_found` (an error), `already_taken by <holder>
/// until <time>` (or `with no expiry`), or `already_done: report h:M` (only a report from a `done` worker, or one posted by hand, counts; see [`done_report`]).
pub fn take(w: &Connection, id: i64, by: &str, lease_secs: Option<i64>) -> Result<String> {
    w.execute_batch("BEGIN IMMEDIATE")?;
    let r = take_in_tx(w, id, by, lease_secs);
    match &r {
        Ok(_) => w.execute_batch("COMMIT")?,
        Err(_) => {
            let _ = w.execute_batch("ROLLBACK");
        }
    }
    r
}

fn take_in_tx(w: &Connection, id: i64, by: &str, lease_secs: Option<i64>) -> Result<String> {
    let now = db::now();
    let hs = handle(id);
    let row: Option<(Option<String>, Option<i64>, Option<i64>)> = w
        .query_row("SELECT claimed_by, claimed_at, lease_until FROM handoff WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let Some((holder, at, until)) = row else { bail!("refused {hs} not_found") };
    let renew = holder.as_deref() == Some(by);
    if !renew {
        // Work that already has a report is done; taking it again would repeat it.
        if let Some(r) = done_report(w, id)? {
            return Ok(format!("refused {hs} already_done: report {}\n", handle(r)));
        }
    }
    let free = holder.is_none() || renew || until.is_some_and(|u| u < now);
    if !free {
        let when = match until {
            Some(u) => format!("until {}", crate::agents::hms(u)),
            None => "with no expiry".to_string(),
        };
        return Ok(format!("refused {hs} already_taken by {} {when}\n", holder.unwrap_or_default()));
    }
    let lease_until = lease_secs.map(|s| now + s.max(0));
    let n = w.execute(
        "UPDATE handoff SET claimed_by = ?2, claimed_at = ?3, lease_until = ?4
         WHERE id = ?1 AND claimed_by IS ?5 AND claimed_at IS ?6",
        params![id, by, now, lease_until, holder, at],
    )?;
    if n != 1 {
        return Ok(format!("refused {hs} already_taken (lost a race; read it again)\n"));
    }
    db::bump_generation(w)?;
    let again = if renew { " (renewed)" } else { "" };
    Ok(match lease_until {
        Some(u) => format!("claimed {hs} for {by} until {}{again}\n", crate::agents::hms(u)),
        None => format!("claimed {hs} for {by}{again}\n"),
    })
}

/// The report that closes handoff `id`: its first `report` child, unless a dispatched worker
/// posted it and ended other than `done` (failed, blocked, partial, flagged). Such a report stays
/// on the board but leaves the handoff open for a retry.
pub fn done_report(c: &Connection, id: i64) -> Result<Option<i64>> {
    Ok(c.query_row(
        "SELECT h.id FROM handoff h WHERE h.parent_id = ?1 AND h.kind = 'report'
           AND NOT EXISTS (SELECT 1 FROM agent a WHERE a.report = h.id AND a.status <> 'done')
         ORDER BY h.id LIMIT 1",
        [id],
        |r| r.get(0),
    )
    .optional()?)
}

/// Seconds a parent's snapshot is reused once the board has moved on (`FOXDB_SNAPSHOT_SECS`).
pub fn snapshot_secs() -> i64 {
    std::env::var("FOXDB_SNAPSHOT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(600)
}

/// The packet a parent's children are pointed at: the newest of the parent's own packet (one it
/// made with `handoff create` / `op=packet`, as `author`) and its board snapshot (frozen open
/// decisions and requests, keyed by `owner`). Siblings started together get the same handle, so
/// their briefs stay byte-identical; a snapshot is rebuilt only once it is older than
/// [`snapshot_secs`] and the board has new decisions or requests. Check and insert run in one
/// write transaction, so parallel spawns cannot each make their own.
pub fn snapshot(w: &Connection, pid: i64, owner: &str, author: Option<&str>) -> Result<i64> {
    w.execute_batch("BEGIN IMMEDIATE")?;
    let r = snapshot_in_tx(w, pid, owner, author);
    match &r {
        Ok(_) => w.execute_batch("COMMIT")?,
        Err(_) => {
            let _ = w.execute_batch("ROLLBACK");
        }
    }
    r
}

fn snapshot_in_tx(w: &Connection, pid: i64, owner: &str, author: Option<&str>) -> Result<i64> {
    let latest = |sql: &str, who: &str| -> Result<Option<(i64, i64)>> {
        Ok(w.query_row(sql, params![pid, who], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    };
    let snap = latest(
        "SELECT id, created_at FROM handoff WHERE project_id = ?1 AND kind = 'packet' AND scope = 'snapshot' AND agent = ?2 ORDER BY id DESC LIMIT 1",
        owner,
    )?;
    let own = match author {
        Some(a) => latest(
            "SELECT id, created_at FROM handoff WHERE project_id = ?1 AND kind = 'packet' AND scope = 'packet' AND agent = ?2 ORDER BY id DESC LIMIT 1",
            a,
        )?,
        None => None,
    };
    let newest = match (snap, own) {
        (Some(s), Some(o)) => Some(if o.0 > s.0 { o } else { s }),
        (s, o) => s.or(o),
    };
    if let Some((id, at)) = newest {
        let moved: bool = w.query_row(
            "SELECT EXISTS(SELECT 1 FROM handoff WHERE project_id = ?1 AND id > ?2 AND kind IN ('decision','request','question'))",
            params![pid, id],
            |r| r.get(0),
        )?;
        if !moved || db::now() - at < snapshot_secs() {
            return Ok(id);
        }
    }
    let lines = brief_lines(w, pid, 8);
    let mut body = if lines.is_empty() {
        "open decisions/requests: none\n".to_string()
    } else {
        format!("open decisions/requests:\n{}\n", lines.join("\n"))
    };
    if let Some((o, _)) = own {
        let _ = writeln!(body, "parent's packet: {} (fox_read target={})", handle(o), handle(o));
    }
    post(
        w,
        &Post {
            project_id: pid,
            kind: "packet",
            text: &body,
            title: Some("board snapshot"),
            scope: Some("snapshot"),
            agent: Some(owner),
            ..Default::default()
        },
    )
}

/// The few most recent open requests and decisions, for subagent briefs. Deterministic.
pub fn brief_lines(c: &Connection, pid: i64, n: usize) -> Vec<String> {
    let Ok(mut s) = c.prepare_cached(
        "SELECT id, kind, title FROM handoff WHERE project_id = ?1 AND kind IN ('decision','request','question') AND claimed_by IS NULL
         ORDER BY id DESC LIMIT ?2",
    ) else {
        return vec![];
    };
    s.query_map(params![pid, n as i64], |r| {
        Ok(format!("  {} {} {}", handle(r.get(0)?), r.get::<_, String>(1)?, query::clip(&r.get::<_, String>(2)?, 100)))
    })
    .map(|it| it.flatten().collect())
    .unwrap_or_default()
}
