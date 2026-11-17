//! Read paths: `find`, `mem brief`, `status`. Read-only connections, budgeted line output.

use crate::markdown::OPEN_STATES;
use crate::project::{self, Project};
use anyhow::Result;
use rusqlite::Connection;
use std::fmt::Write;

/// Budgeted line writer: tokens ~= bytes / 4.
pub struct Out {
    buf: String,
    max: usize,
    pub dropped: usize,
}
impl Out {
    pub fn new(budget_tokens: usize) -> Out {
        Out { buf: String::new(), max: budget_tokens.max(32) * 4, dropped: 0 }
    }
    /// Push a line if it fits; returns false once the budget is exhausted.
    pub fn line(&mut self, s: &str) -> bool {
        if self.dropped > 0 || self.buf.len() + s.len() + 1 > self.max {
            self.dropped += 1;
            return false;
        }
        self.buf.push_str(s);
        self.buf.push('\n');
        true
    }
    /// Like `finish`, but the `+N more` footer also fits the budget: lines are dropped from the
    /// end until it does.
    pub fn finish_fit(mut self, more_hint: &str) -> String {
        if self.dropped > 0 {
            loop {
                let footer = format!("+{} more{more_hint}", self.dropped);
                if self.buf.len() + footer.len() < self.max || self.buf.is_empty() {
                    let _ = writeln!(self.buf, "{footer}");
                    return self.buf;
                }
                let cut = self.buf[..self.buf.len() - 1].rfind('\n').map(|i| i + 1).unwrap_or(0);
                self.buf.truncate(cut);
                self.dropped += 1;
            }
        }
        self.buf
    }
    pub fn finish(mut self, more_hint: &str) -> String {
        if self.dropped > 0 {
            let _ = writeln!(self.buf, "+{} more{}", self.dropped, more_hint);
        }
        self.buf
    }
}

/// Emit `lines[offset..]` within a token budget. When truncated, the footer carries the total and
/// the cursor (the offset of the first hidden line) to pass back for the next page.
pub fn page(lines: &[String], offset: usize, budget_tokens: usize) -> String {
    page_n(lines, offset, budget_tokens, usize::MAX)
}

pub fn page_n(lines: &[String], offset: usize, budget_tokens: usize, max_rows: usize) -> String {
    let max = budget_tokens.clamp(32, 4000) * 4;
    let mut out = String::new();
    let mut shown = offset;
    for l in lines.iter().skip(offset) {
        if (out.len() + l.len() + 1 > max && shown > offset) || shown - offset >= max_rows {
            break;
        }
        out.push_str(l);
        out.push('\n');
        shown += 1;
    }
    if shown < lines.len() {
        let _ = writeln!(out, "+{} more \u{b7} cursor={shown}", lines.len() - shown);
    }
    out
}

pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
    t.push('…');
    t
}

pub fn human_bytes(b: i64) -> String {
    let b = b as f64;
    if b >= 1e9 {
        format!("{:.1}G", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0}M", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0}K", b / 1e3)
    } else {
        format!("{b}B")
    }
}

pub(crate) fn ago(ts: i64) -> String {
    let d = (crate::db::now() - ts).max(0);
    if d < 90 {
        format!("{d}s ago")
    } else if d < 5400 {
        format!("{}m ago", d / 60)
    } else if d < 172_800 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86400)
    }
}

/// Drop a leading date and trailing state from a milestone heading (they are shown separately).
fn tidy(title: &str) -> String {
    let mut t = title.trim();
    if t.len() >= 10 && t.as_bytes()[4] == b'-' && t[..4].chars().all(|c| c.is_ascii_digit()) {
        t = t[10..].trim_start_matches(|c: char| c == ' ' || c == '-' || c == '|' || c == '\u{2014}' || c.is_ascii_digit() || c == ':');
        t = t.trim_start_matches("PDT").trim_start_matches([' ', '|', '\u{2014}']);
    }
    for st in crate::markdown::STATES {
        if let Some(i) = t.rfind(st) {
            let head = t[..i].trim_end_matches([' ', '-', '\u{2014}', '|']);
            if !head.is_empty() && t[i + st.len()..].trim_start().starts_with(['(', '\0']) || t[i + st.len()..].trim().is_empty() {
                let tail = t[i + st.len()..].trim();
                return if tail.is_empty() { head.to_string() } else { format!("{head} {tail}") };
            }
        }
    }
    t.to_string()
}

pub fn handle(kind: &str, id: i64) -> String {
    let p = match kind {
        "file" => 'f',
        "doc_section" => 'd',
        "milestone" => 'm',
        "artifact_dir" => 'a',
        "memory" => 'g',
        "dir" => 'f',
        "symbol" => 's',
        "session" => 'x',
        _ => 'n',
    };
    format!("{p}:{id:x}")
}

/// One search result row.
pub struct Hit {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub tier: Option<i64>,
    pub size: Option<i64>,
    pub meta: Option<String>,
    pub project: String,
    pub stale: bool,
    pub sig: Option<String>,
    pub refs: Option<i64>,
}

pub(crate) fn render(h: &Hit, show_project: bool) -> String {
    let proj = if show_project { format!(" ({})", h.project) } else { String::new() };
    let path = h.path.clone().unwrap_or_default();
    let meta: serde_json::Value = h.meta.as_deref().and_then(|m| serde_json::from_str(m).ok()).unwrap_or_default();
    match h.kind.as_str() {
        "milestone" => {
            let st = meta["state"].as_str().unwrap_or("-");
            let date = meta["date"].as_str().unwrap_or("");
            let title = meta["title"].as_str().unwrap_or(&h.name);
            format!(
                "{} milestone {} {} {}  {}:{}{}  {}",
                handle(&h.kind, h.id),
                h.name,
                st,
                date,
                path,
                h.line.unwrap_or(0),
                proj,
                clip(title, 70)
            )
        }
        "doc_section" => format!("{} § {}  {}:{}{}", handle(&h.kind, h.id), clip(&h.name, 80), path, h.line.unwrap_or(0), proj),
        "artifact_dir" => {
            let pol = meta["policy"].as_str().unwrap_or("");
            let extra = if pol == "name_only" {
                format!("name-only, {} files, {}", meta["files"], human_bytes(meta["bytes"].as_i64().unwrap_or(0)))
            } else {
                pol.replace('_', "-")
            };
            format!("{} dir {}/  ({}){}", handle(&h.kind, h.id), path, extra, proj)
        }
        "symbol" => {
            let k = meta["k"].as_str().unwrap_or("symbol");
            let sig = h.sig.clone().unwrap_or_else(|| h.name.clone());
            let disp = match meta["parent"].as_str() {
                Some(pn) if !pn.is_empty() => format!("{pn}.{sig}"),
                _ => sig,
            };
            let tag = meta["mod"].as_str().and_then(crate::config::module_tag).map(|t| format!(" [{t}]")).unwrap_or_default();
            let refs = h.refs.map(|n| format!("  refs:{n}")).unwrap_or_default();
            let (mark, hd) = if meta["ts"].is_number() {
                ("~", format!("c:{:x}", h.id))
            } else {
                (if h.stale { "~" } else { "" }, handle(&h.kind, h.id))
            };
            format!("{mark}{hd} {} {}  {}:{}{}{}{}", k, clip(&disp, 90), path, h.line.unwrap_or(0), refs, tag, proj)
        }
        "memory" => format!("{} memory {}  {}:1", handle(&h.kind, h.id), clip(&h.name, 60), path),
        "session" => {
            let day = meta["started"].as_str().map(|s| s.chars().take(10).collect::<String>()).unwrap_or_default();
            let who = meta["harness"].as_str().unwrap_or("");
            format!("{} session {day} {who} {}  {}:1{}", handle(&h.kind, h.id), clip(&h.name, 70), path, proj)
        }
        _ => {
            let t = match h.tier {
                Some(3) => "code",
                Some(2) => "text",
                Some(1) => "name-only",
                _ => "",
            };
            format!("{} {}:1  [{} {}]{}", handle(&h.kind, h.id), path, t, human_bytes(h.size.unwrap_or(0)), proj)
        }
    }
}

/// Skeleton form of a row: handle, kind, name and `path:line` only.
pub(crate) fn render_skeleton(h: &Hit, show_project: bool) -> String {
    let proj = if show_project { format!(" ({})", h.project) } else { String::new() };
    let path = h.path.clone().unwrap_or_default();
    let meta: serde_json::Value = h.meta.as_deref().and_then(|m| serde_json::from_str(m).ok()).unwrap_or_default();
    let (hd, kind) = match h.kind.as_str() {
        "symbol" if meta["ts"].is_number() => (format!("~c:{:x}", h.id), meta["k"].as_str().unwrap_or("symbol").to_string()),
        "symbol" => (handle(&h.kind, h.id), meta["k"].as_str().unwrap_or("symbol").to_string()),
        "doc_section" => (handle(&h.kind, h.id), "§".into()),
        k => (handle(k, h.id), k.replace("artifact_dir", "dir")),
    };
    let name = match meta["parent"].as_str() {
        Some(pn) if !pn.is_empty() && h.kind == "symbol" => format!("{pn}.{}", h.name),
        _ => h.name.clone(),
    };
    format!("{hd} {kind} {}  {path}:{}{proj}", clip(&name, 60), h.line.unwrap_or(1).max(1))
}

/// Id form of a row: handle and `path:line`.
pub(crate) fn render_id(h: &Hit) -> String {
    let hd = if h.kind == "symbol" && h.meta.as_deref().is_some_and(|m| m.contains("\"ts\"")) {
        format!("c:{:x}", h.id)
    } else {
        handle(&h.kind, h.id)
    };
    format!("{hd} {}:{}", h.path.clone().unwrap_or_default(), h.line.unwrap_or(1).max(1))
}

pub struct FindOpts<'a> {
    pub kind: &'a str,
    pub all: bool,
    pub budget: usize,
    pub limit: usize,
    pub offset: usize,
}

/// Ranked search; see `search` (parallel arms fused with RRF, code-aware re-ranking).
pub fn find(c: &Connection, cur: Option<&Project>, q: &str, o: &FindOpts) -> Result<String> {
    crate::search::find(c, cur, q, o)
}

/// The body of a section (the lines after its heading), read from the file: the FTS index is
/// contentless and keeps no copy of the text.
pub fn section_text(root: &std::path::Path, rel: &str, line: i64, end: i64) -> String {
    let Some(text) = crate::guard::read_text(&root.join(rel), rel, 4 << 20, false) else { return String::new() };
    let from = line.max(1) as usize; // 1-based heading line; the body starts after it
    let to = (end.max(line) as usize).max(from);
    text.lines().skip(from).take(to - from).collect::<Vec<_>>().join("\n")
}

/// Session-start brief. Hard cap `budget` tokens.
pub fn brief(c: &Connection, p: &Project, branch: Option<&str>, budget: usize) -> Result<String> {
    let mut out = Out::new(budget);
    let (files, docs): (i64, i64) =
        c.query_row("SELECT SUM(kind='file'), SUM(kind IN ('doc_section','milestone')) FROM node WHERE project_id = ?1", [p.id], |r| {
            Ok((r.get::<_, Option<i64>>(0)?.unwrap_or(0), r.get::<_, Option<i64>>(1)?.unwrap_or(0)))
        })?;
    // No clocks or relative ages: the brief must be byte-identical while nothing changes.
    let idx = if p.indexed_at.is_none() { " · indexing in background" } else { "" };
    out.line(&format!("foxdb · {} ({}) · {} files, {} doc sections{}", p.name, branch.unwrap_or("no git"), files, docs, idx));
    if p.kind == "huge" {
        out.line("home/root directory: name-only inventory, nothing parsed.");
    }

    // Current gate: pinned milestone-log section, first 3 meaningful lines.
    let gate: Option<(i64, String, i64, i64)> = c
        .query_row(
            "SELECT n.id, n.path, n.line, COALESCE(n.end_line, n.line + 40) FROM node n
         WHERE n.project_id = ?1 AND n.kind = 'milestone' AND json_extract(n.meta, '$.pinned') = 1 ORDER BY n.id LIMIT 1",
            [p.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok();
    if let Some((id, path, line, end)) = gate {
        let body = section_text(&p.root, &path, line, end);
        out.line(&format!("current gate ({path}:{line}, {}):", handle("milestone", id)));
        let mut n = 0;
        let mut cur = String::new();
        for l in body.lines().map(str::trim) {
            if l.is_empty() {
                if !cur.is_empty() {
                    break;
                }
                continue;
            }
            cur.push_str(if cur.is_empty() { "" } else { " " });
            cur.push_str(l.trim_start_matches("- "));
        }
        // Wrap the first paragraph into up to 3 lines of ~150 chars.
        let words: Vec<&str> = cur.split_whitespace().collect();
        let mut line_buf = String::new();
        for w in words {
            if line_buf.len() + w.len() + 1 > 150 {
                out.line(&format!("  {line_buf}"));
                n += 1;
                line_buf.clear();
                if n == 3 {
                    break;
                }
            }
            if !line_buf.is_empty() {
                line_buf.push(' ');
            }
            line_buf.push_str(w);
        }
        if n < 3 && !line_buf.is_empty() {
            out.line(&format!("  {line_buf}"));
        } else if n == 3 {
            out.line("  … (fox find --kind milestone \"Current gate\")");
        }
    }

    // Open milestones: latest sections in an open state, newest first (by file order).
    let mut s = c.prepare_cached(
        "SELECT n.id, n.name, json_extract(n.meta,'$.state'), json_extract(n.meta,'$.date'), json_extract(n.meta,'$.title'), n.path, n.line
         FROM node n WHERE n.project_id = ?1 AND n.kind = 'milestone' AND json_extract(n.meta,'$.pinned') = 0
         ORDER BY n.path, n.line DESC LIMIT 400",
    )?;
    type Row = (i64, String, Option<String>, Option<String>, Option<String>, String, i64);
    let rows: Vec<Row> =
        s.query_map([p.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.flatten().collect();
    if !rows.is_empty() {
        let mut latest: Vec<_> = rows.iter().collect();
        latest.sort_by(|a, b| b.3.cmp(&a.3).then(b.6.cmp(&a.6)).then(a.0.cmp(&b.0)));
        let open: Vec<_> = latest.iter().filter(|r| r.2.as_deref().is_some_and(|st| OPEN_STATES.contains(&st))).take(4).collect();
        if !open.is_empty() {
            out.line("open milestones (newest first):");
            for r in open {
                out.line(&format!(
                    "  {} {} {} {}  {}",
                    handle("milestone", r.0),
                    r.2.as_deref().unwrap_or("-"),
                    r.3.as_deref().unwrap_or(""),
                    clip(&tidy(r.4.as_deref().unwrap_or(&r.1)), 80),
                    r.5
                ));
            }
        }
        if let Some(r) = latest.first() {
            out.line(&format!(
                "latest entry: {} {} {}  {}",
                handle("milestone", r.0),
                r.2.as_deref().unwrap_or("-"),
                r.3.as_deref().unwrap_or(""),
                clip(&tidy(r.4.as_deref().unwrap_or(&r.1)), 90)
            ));
        }
    }
    let mem: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM node n JOIN project p ON p.id = n.project_id WHERE p.name = '~global' AND n.kind = 'memory'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    out.line(&format!(
        "fox find <q> [--kind file|doc|milestone|dir|memory] searches this project + {mem} global memory notes; fox status."
    ));
    // Volatile part last, so a change here only invalidates the cache from this line on.
    if let Some(l) = crate::savings::brief_line(c) {
        out.line(&l);
    }
    Ok(out.finish_fit(&format!(" lines {}", crate::hook::CUT_MARK)))
}

pub fn status(c: &Connection, cur: Option<&Project>) -> Result<String> {
    let mut s = String::new();
    let mut q = c.prepare(
        "SELECT p.id, p.name, p.root, p.kind, p.indexed_at, p.index_ms, p.truncated,
           (SELECT COUNT(*) FROM node WHERE project_id = p.id AND kind = 'file'),
           (SELECT COUNT(*) FROM node WHERE project_id = p.id AND kind IN ('doc_section','milestone')),
           (SELECT COUNT(*) FROM node WHERE project_id = p.id AND kind = 'milestone'),
           (SELECT COUNT(*) FROM node WHERE project_id = p.id AND kind = 'artifact_dir'),
           (SELECT COUNT(*) FROM node WHERE project_id = p.id AND kind = 'memory')
         FROM project p ORDER BY p.name",
    )?;
    let rows = q.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, Option<i64>>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
            r.get::<_, i64>(9)?,
            r.get::<_, i64>(10)?,
            r.get::<_, i64>(11)?,
        ))
    })?;
    for r in rows.flatten() {
        let mark = if cur.is_some_and(|p| p.id == r.0) { "*" } else { " " };
        let idx = r.4.map(|t| format!("indexed {} in {}ms", ago(t), r.5.unwrap_or(0))).unwrap_or_else(|| "not indexed".into());
        let _ = writeln!(
            s,
            "{mark} {} [{}] {}  files={} sections={} milestones={} summary_dirs={} memory={}  {}{}",
            r.1,
            r.3,
            r.2,
            r.7,
            r.8,
            r.9,
            r.10,
            r.11,
            idx,
            if r.6 != 0 { " TRUNCATED by an older build's file cap: run fox index to list every file" } else { "" }
        );
        if let Some(l) = crate::scip::status_line(c, r.0) {
            let _ = writeln!(s, "{l}");
        }
        #[cfg(feature = "tantivy")]
        if let Some(l) = crate::tantivy_index::status_line(c, r.0) {
            let _ = writeln!(s, "{l}");
        }
        #[cfg(not(feature = "tantivy"))]
        if crate::outbox::state(c, r.0).is_some() {
            let _ = writeln!(s, "    search: a tantivy index is configured, but this build has none; FTS5 serves every search");
        }
    }
    let size = std::fs::metadata(crate::db::db_path()).map(|m| m.len() as i64).unwrap_or(0);
    let _ = writeln!(s, "store: {} ({})", crate::db::db_path().display(), human_bytes(size));
    Ok(s)
}

pub fn current(c: &Connection, cwd: &std::path::Path) -> Option<(Project, project::Located)> {
    let loc = project::locate(cwd);
    project::find(c, &loc).ok().flatten().map(|p| (p, loc))
}
