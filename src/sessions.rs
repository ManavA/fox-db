//! Read-only ingest of Claude Code and Codex session transcripts, so one harness can find what
//! another did. Transcripts are opened read-only (`.jsonl`, or `.jsonl.zst` for Codex) and never
//! written. Each becomes one `session` node holding a digest, not a dump: the first real prompt,
//! the last answer, up to 15 file anchors, turn count and dates. Everything is redacted first.
//!
//! - Claude: `~/.claude/projects/<project>/<session>.jsonl` and `<session>/subagents/*.jsonl`
//!   (root override `FOXDB_CLAUDE_PROJECTS`).
//! - Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl[.zst]` (override `FOXDB_CODEX_SESSIONS`).

use crate::{db, project, query, redact};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

const TITLE_MAX: usize = 160;
const LAST_MAX: usize = 1_200;
const ANCHORS_MAX: usize = 15;
/// Lines longer than this are tool output or blobs; skipped unparsed.
const LINE_MAX: usize = 1 << 20;

#[derive(Debug, Default)]
pub struct Digest {
    pub harness: &'static str,
    pub sid: String,
    pub cwd: Option<String>,
    pub started: Option<String>,
    pub ended: Option<String>,
    pub turns: usize,
    pub title: String,
    pub last: String,
    pub anchors: Vec<String>,
    pub parent: Option<String>,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub files: usize,
    pub sessions: usize,
    pub skipped: usize,
}

fn open(path: &Path) -> Option<Box<dyn BufRead>> {
    let f = std::fs::File::open(path).ok()?;
    if path.extension().is_some_and(|e| e == "zst") {
        let d = ruzstd::decoding::StreamingDecoder::new(f).ok()?;
        Some(Box::new(BufReader::new(d)))
    } else {
        Some(Box::new(BufReader::new(f)))
    }
}

/// Lines as JSON values; oversized or broken lines are skipped.
fn json_lines(r: Box<dyn BufRead>) -> impl Iterator<Item = Value> {
    let mut r = r;
    let mut buf = Vec::new();
    std::iter::from_fn(move || {
        loop {
            buf.clear();
            let n = (&mut r).take(LINE_MAX as u64 + 1).read_until(b'\n', &mut buf).ok()?;
            if n == 0 {
                return None;
            }
            if n > LINE_MAX {
                // Drain the rest of this line.
                let mut sink = Vec::new();
                let _ = r.read_until(b'\n', &mut sink);
                continue;
            }
            if let Ok(v) = serde_json::from_slice::<Value>(&buf) {
                return Some(v);
            }
        }
    })
}

fn is_noise_prompt(t: &str) -> bool {
    let t = t.trim_start();
    t.is_empty() || t.starts_with('<') || t.starts_with("Caveat:") || t.starts_with("[Request interrupted")
}

fn clip_redacted(s: &str, n: usize) -> String {
    let s = redact::redact(s.trim());
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    query::clip(&one, n)
}

struct Anchors {
    cwd: Option<String>,
    v: Vec<String>,
}

impl Anchors {
    fn add(&mut self, p: &str) {
        let p = p.trim();
        if p.is_empty() || p.len() > 300 {
            return;
        }
        let rel = match &self.cwd {
            Some(c) => p.strip_prefix(c.as_str()).map(|r| r.trim_start_matches('/')).unwrap_or(p),
            None => p,
        };
        if !rel.is_empty() && !self.v.iter().any(|x| x == rel) && self.v.len() < ANCHORS_MAX {
            self.v.push(rel.to_string());
        }
    }
    /// `path:line` mentions in prose.
    fn scan(&mut self, text: &str) {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"\b([A-Za-z0-9_\-./]+\.[A-Za-z0-9]{1,6}:\d+)").unwrap());
        for m in re.captures_iter(text) {
            self.add(&m[1]);
        }
    }
}

pub fn digest_claude(path: &Path) -> Option<Digest> {
    let mut d = Digest { harness: "claude", ..Default::default() };
    let mut anchors = Anchors { cwd: None, v: vec![] };
    for v in json_lines(open(path)?) {
        let ty = v["type"].as_str().unwrap_or("");
        if ty != "user" && ty != "assistant" {
            continue;
        }
        if d.sid.is_empty()
            && let Some(s) = v["sessionId"].as_str()
        {
            d.sid = s.to_string();
        }
        if d.cwd.is_none()
            && let Some(c) = v["cwd"].as_str()
        {
            d.cwd = Some(c.to_string());
            anchors.cwd = Some(c.to_string());
        }
        if let Some(t) = v["timestamp"].as_str() {
            if d.started.is_none() {
                d.started = Some(t.to_string());
            }
            d.ended = Some(t.to_string());
        }
        let content = &v["message"]["content"];
        if ty == "user" {
            if v["isMeta"] == true {
                continue;
            }
            if let Some(t) = content.as_str()
                && !is_noise_prompt(t)
            {
                d.turns += 1;
                if d.title.is_empty() {
                    d.title = clip_redacted(t, TITLE_MAX);
                }
            }
            continue;
        }
        for block in content.as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(t) = block["text"].as_str().filter(|t| !t.trim().is_empty()) {
                        d.last = t.to_string();
                        anchors.scan(t);
                    }
                }
                Some("tool_use") => {
                    for k in ["file_path", "path", "notebook_path"] {
                        if let Some(p) = block["input"][k].as_str() {
                            anchors.add(p);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if d.sid.is_empty() {
        d.sid = path.file_stem()?.to_string_lossy().into_owned();
    }
    d.last = clip_redacted(&d.last, LAST_MAX);
    d.anchors = anchors.v.iter().map(|a| redact::redact(a)).collect();
    (d.turns > 0 || !d.last.is_empty()).then_some(d)
}

pub fn digest_codex(path: &Path) -> Option<Digest> {
    let mut d = Digest { harness: "codex", ..Default::default() };
    let mut anchors = Anchors { cwd: None, v: vec![] };
    for v in json_lines(open(path)?) {
        if let Some(t) = v["timestamp"].as_str() {
            if d.started.is_none() {
                d.started = Some(t.to_string());
            }
            d.ended = Some(t.to_string());
        }
        let p = &v["payload"];
        match (v["type"].as_str().unwrap_or(""), p["type"].as_str().unwrap_or("")) {
            ("session_meta", _) => {
                if let Some(id) = p["id"].as_str() {
                    d.sid = id.to_string();
                }
                if let Some(c) = p["cwd"].as_str() {
                    d.cwd = Some(c.to_string());
                    anchors.cwd = Some(c.to_string());
                }
                if let Some(parent) = p["parent_thread_id"].as_str().or_else(|| p["source"]["subagent"]["parent_thread_id"].as_str()) {
                    d.parent = Some(parent.to_string());
                }
            }
            ("event_msg", "user_message") => {
                if let Some(t) = p["message"].as_str()
                    && !is_noise_prompt(t)
                {
                    d.turns += 1;
                    if d.title.is_empty() {
                        d.title = clip_redacted(t, TITLE_MAX);
                    }
                }
            }
            ("event_msg", "agent_message") => {
                if let Some(t) = p["message"].as_str().filter(|t| !t.trim().is_empty()) {
                    d.last = t.to_string();
                    anchors.scan(t);
                }
            }
            ("event_msg", "task_complete") => {
                if let Some(t) = p["last_agent_message"].as_str().filter(|t| !t.trim().is_empty()) {
                    d.last = t.to_string();
                }
            }
            ("response_item", "custom_tool_call") | ("response_item", "function_call") => {
                let input = p["input"].as_str().or_else(|| p["arguments"].as_str()).unwrap_or("");
                for l in input.lines() {
                    for pre in ["*** Update File: ", "*** Add File: ", "*** Delete File: "] {
                        if let Some(f) = l.strip_prefix(pre) {
                            anchors.add(f);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if d.sid.is_empty() {
        d.sid = path.file_name()?.to_string_lossy().trim_end_matches(".zst").trim_end_matches(".jsonl").to_string();
    }
    d.last = clip_redacted(&d.last, LAST_MAX);
    d.anchors = anchors.v.iter().map(|a| redact::redact(a)).collect();
    (d.turns > 0 || !d.last.is_empty()).then_some(d)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

pub fn claude_root() -> PathBuf {
    std::env::var_os("FOXDB_CLAUDE_PROJECTS").map(PathBuf::from).unwrap_or_else(|| home().join(".claude/projects"))
}

pub fn codex_root() -> PathBuf {
    std::env::var_os("FOXDB_CODEX_SESSIONS").map(PathBuf::from).unwrap_or_else(|| home().join(".codex/sessions"))
}

fn claude_files(root: &Path) -> Vec<PathBuf> {
    let mut v = vec![];
    for proj in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let pd = proj.path();
        for e in std::fs::read_dir(&pd).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "jsonl") {
                v.push(p);
            } else if p.is_dir() {
                for s in std::fs::read_dir(p.join("subagents")).into_iter().flatten().flatten() {
                    if s.path().extension().is_some_and(|x| x == "jsonl") {
                        v.push(s.path());
                    }
                }
            }
        }
    }
    v.sort();
    v
}

fn codex_files(root: &Path) -> Vec<PathBuf> {
    let mut v = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.is_dir() {
                stack.push(p);
            } else if name.starts_with("rollout-") && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")) {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

/// `~/...` display form of a transcript path.
fn display(p: &Path) -> String {
    let h = home();
    match p.strip_prefix(&h) {
        Ok(r) if !h.as_os_str().is_empty() => format!("~/{}", r.display()),
        _ => p.display().to_string(),
    }
}

fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Ingest new or changed transcripts. Unchanged files (size and mtime) are skipped unread.
pub fn ingest(c: &mut Connection, claude: Option<&Path>, codex: Option<&Path>) -> Result<Stats> {
    ingest_since(c, claude, codex, None)
}

/// `ingest`, limited to transcripts modified at or after `since_ms` (unix ms) when given.
pub fn ingest_since(c: &mut Connection, claude: Option<&Path>, codex: Option<&Path>, since_ms: Option<i64>) -> Result<Stats> {
    let mut st = Stats::default();
    let global = project::global(c)?.id;
    let mut files: Vec<(&'static str, PathBuf)> = vec![];
    if let Some(r) = claude {
        files.extend(claude_files(r).into_iter().map(|p| ("claude", p)));
    }
    if let Some(r) = codex {
        files.extend(codex_files(r).into_iter().map(|p| ("codex", p)));
    }
    let mut project_of: HashMap<String, i64> = HashMap::new();
    let tx = c.transaction()?;
    let sink = crate::outbox::Sink::open(&tx);
    for (harness, path) in files {
        st.files += 1;
        let Ok(md) = std::fs::metadata(&path) else { continue };
        let (size, mtime) = (md.len() as i64, mtime_ms(&md));
        if since_ms.is_some_and(|s| mtime < s) {
            st.skipped += 1;
            continue;
        }
        let shown = display(&path);
        let prev: Option<(i64, i64, i64)> = tx
            .query_row("SELECT id, size, mtime FROM node WHERE key = ?1", [format!("sess:{shown}")], |r| {
                Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(-1), r.get::<_, Option<i64>>(2)?.unwrap_or(-1)))
            })
            .optional()?;
        if prev.is_some_and(|p| p.1 == size && p.2 == mtime) {
            st.skipped += 1;
            continue;
        }
        let d = if harness == "claude" { digest_claude(&path) } else { digest_codex(&path) };
        if let Some((id, _, _)) = prev {
            if let Ok(old_pid) = tx.query_row("SELECT project_id FROM node WHERE id = ?1", [id], |r| r.get::<_, i64>(0)) {
                sink.del(&tx, old_pid, crate::outbox::TBL_NODE, id)?;
                sink.del(&tx, old_pid, crate::outbox::TBL_TRI, id)?;
            }
            tx.execute("DELETE FROM fts_node WHERE rowid = ?1", [id])?;
            tx.execute("DELETE FROM fts_trigram WHERE rowid = ?1", [id])?;
            tx.execute("DELETE FROM node WHERE id = ?1", [id])?;
        }
        let Some(d) = d else { continue };
        let pid = match &d.cwd {
            Some(cwd) => *project_of
                .entry(cwd.clone())
                .or_insert_with(|| project::find(&tx, &project::locate(Path::new(cwd))).ok().flatten().map(|p| p.id).unwrap_or(global)),
            None => global,
        };
        // A subagent transcript's parent is the session directory it lives under.
        let parent = d.parent.clone().or_else(|| {
            (path.parent().and_then(|p| p.file_name()).is_some_and(|n| n == "subagents"))
                .then(|| path.parent()?.parent()?.file_name().map(|n| n.to_string_lossy().into_owned()))
                .flatten()
        });
        let title = if d.title.is_empty() { "(no prompt)".to_string() } else { d.title.clone() };
        let meta = serde_json::json!({"harness": d.harness, "sid": d.sid, "cwd": d.cwd, "started": d.started, "ended": d.ended,
            "turns": d.turns, "anchors": d.anchors, "parent": parent, "last": d.last});
        tx.execute(
            "INSERT OR REPLACE INTO node(project_id, kind, key, name, path, line, tier, size, mtime, source, meta, updated_at)
             VALUES (?1,'session',?2,?3,?4,1,2,?5,?6,?7,?8,?9)",
            params![pid, format!("sess:{shown}"), title, shown, size, mtime, d.harness, meta.to_string(), db::now()],
        )?;
        let id = tx.last_insert_rowid();
        let body = format!("{}\n{}\n{}", d.title, d.last, d.anchors.join(" "));
        tx.execute(
            "INSERT INTO fts_node(rowid, name, sig, path, body) VALUES (?1,?2,?3,?4,?5)",
            params![id, title, d.harness, shown, body],
        )?;
        tx.execute("INSERT INTO fts_trigram(rowid, path, name) VALUES (?1,?2,?3)", params![id, shown, title])?;
        sink.put(&tx, pid, crate::outbox::TBL_NODE, id, &[&title, d.harness, &shown, &body])?;
        sink.put(&tx, pid, crate::outbox::TBL_TRI, id, &[&shown, &title])?;
        st.sessions += 1;
    }
    if st.sessions > 0 {
        db::bump_generation(&tx)?;
    }
    tx.commit()?;
    Ok(st)
}

/// Render a stored digest (from `meta`) for `fox_read x:<id>`.
pub fn render_digest(name: &str, path: &str, meta: &str) -> String {
    let m: Value = serde_json::from_str(meta).unwrap_or_default();
    let mut s = format!(
        "{} session {} {}..{} turns:{}  {path}:1\nprompt: {name}\n",
        m["harness"].as_str().unwrap_or("?"),
        m["sid"].as_str().unwrap_or(""),
        m["started"].as_str().unwrap_or("?"),
        m["ended"].as_str().unwrap_or("?"),
        m["turns"]
    );
    if let Some(c) = m["cwd"].as_str() {
        s.push_str(&format!("cwd: {c}\n"));
    }
    if let Some(p) = m["parent"].as_str() {
        s.push_str(&format!("parent: {p}\n"));
    }
    let anchors: Vec<&str> = m["anchors"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if !anchors.is_empty() {
        s.push_str(&format!("files: {}\n", anchors.join(", ")));
    }
    if let Some(l) = m["last"].as_str().filter(|l| !l.is_empty()) {
        s.push_str(&format!("last answer: {l}\n"));
    }
    s
}

/// Digest of one session by harness and session id (tests, `fox ingest` checks).
pub fn digest_by_key(c: &Connection, harness: &str, sid: &str) -> Option<String> {
    c.query_row(
        "SELECT name, path, meta FROM node WHERE kind = 'session' AND source = ?1 AND json_extract(meta, '$.sid') = ?2",
        params![harness, sid],
        |r| Ok(render_digest(&r.get::<_, String>(0)?, &r.get::<_, String>(1)?, &r.get::<_, String>(2)?)),
    )
    .ok()
}
