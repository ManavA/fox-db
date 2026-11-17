//! `fox code`: code-graph queries over the Swift index-store import (precise, hot path: SQLite only),
//! with `~`-labelled native tree-sitter results (`tscode`) for every other language, and the
//! optional codanna fallback when `codanna = true` in config.toml.

use crate::indexstore as ix;
use crate::project::Project;
use crate::query::{clip, handle, page};
use anyhow::{Result, bail};
use rusqlite::Connection;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;

#[derive(Debug, Clone)]
pub struct Sym {
    pub id: i64,
    pub name: String,
    pub sig: String,
    pub kind: String,
    pub path: Option<String>,
    pub line: i64,
    pub parent: Option<String>,
    pub stale: bool,
    pub source: String,
    pub module: String,
}

impl Sym {
    pub fn qname(&self) -> String {
        match &self.parent {
            Some(p) if !p.is_empty() => format!("{p}.{}", self.name),
            _ => self.name.clone(),
        }
    }
    pub fn h(&self) -> String {
        handle("symbol", self.id)
    }
    /// `s:1f3a method Type.name(label:)  path:line`
    pub fn row(&self) -> String {
        let mark = if self.stale { "~" } else { "" };
        let disp = match &self.parent {
            Some(p) if !p.is_empty() => format!("{p}.{}", self.sig),
            _ => self.sig.clone(),
        };
        let at = match &self.path {
            Some(p) => format!("  {p}:{}", self.line),
            None => "  (external)".into(),
        };
        format!("{mark}{} {} {}{at}", self.h(), self.kind, disp)
    }
}

const COLS: &str = "n.id, n.name, n.sig, n.meta, n.path, n.line, n.stale, n.source";

fn map_sym(r: &rusqlite::Row) -> rusqlite::Result<Sym> {
    let meta: serde_json::Value = r.get::<_, Option<String>>(3)?.and_then(|m| serde_json::from_str(&m).ok()).unwrap_or_default();
    Ok(Sym {
        id: r.get(0)?,
        name: r.get(1)?,
        sig: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        kind: meta["k"].as_str().unwrap_or("symbol").to_string(),
        path: r.get(4)?,
        line: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
        parent: meta["parent"].as_str().map(str::to_string),
        stale: r.get::<_, i64>(6)? != 0,
        source: r.get(7)?,
        module: meta["mod"].as_str().unwrap_or("").to_string(),
    })
}

pub fn sym_by_id(c: &Connection, id: i64) -> Option<Sym> {
    c.query_row(&format!("SELECT {COLS} FROM node n WHERE n.id = ?1 AND n.kind = 'symbol'"), [id], map_sym).ok()
}

/// Resolve a name or handle to candidate symbols, best first (project-defined, most referenced).
pub fn resolve(c: &Connection, pid: i64, q: &str) -> Result<Vec<Sym>> {
    let q = q.trim();
    if let Some(h) = q.strip_prefix("s:")
        && let Ok(id) = i64::from_str_radix(h, 16)
    {
        return Ok(sym_by_id(c, id).into_iter().collect());
    }
    let (qual, name) = match q.rsplit_once('.') {
        Some((a, b)) if !b.is_empty() && !a.contains('(') => (Some(a.rsplit('.').next().unwrap_or(a)), b),
        _ => (None, q),
    };
    let base = name.split('(').next().unwrap_or(name);
    let has_labels = name.contains('(');
    let mut out: Vec<Sym> = vec![];
    let sql = format!(
        "SELECT {COLS} FROM node n INDEXED BY node_sym_name
         WHERE n.project_id = ?1 AND n.kind = 'symbol' AND n.source = 'indexstore' AND n.name = ?2 COLLATE NOCASE LIMIT 200"
    );
    let mut s = c.prepare_cached(&sql)?;
    for r in s.query_map(rusqlite::params![pid, base], map_sym)?.flatten() {
        if let Some(qq) = qual
            && !r.parent.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(qq))
        {
            continue;
        }
        if has_labels && !r.sig.eq_ignore_ascii_case(name) {
            continue;
        }
        if r.kind == "extension" {
            continue;
        }
        out.push(r);
    }
    // Rank: defined in project, then by inbound edges.
    let mut score: HashMap<i64, i64> = HashMap::new();
    for r in &out {
        let n: i64 =
            c.query_row("SELECT COUNT(*) FROM edge WHERE dst = ?1 AND kind IN ('CALLS','REFERENCES')", [r.id], |x| x.get(0)).unwrap_or(0);
        score.insert(r.id, n + if r.path.is_some() { 1_000_000 } else { 0 });
    }
    out.sort_by_key(|r| std::cmp::Reverse(score[&r.id]));
    Ok(out)
}

fn ids_sql(v: &[i64]) -> String {
    v.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

fn is_callable(kind: &str) -> bool {
    matches!(kind, "func" | "method" | "classmethod" | "staticmethod" | "init" | "deinit" | "conversion")
}

fn stale_paths(c: &Connection, pid: i64) -> HashSet<String> {
    let mut q = match c
        .prepare_cached("SELECT DISTINCT path FROM node WHERE project_id = ?1 AND source = 'indexstore' AND stale = 1 AND path IS NOT NULL")
    {
        Ok(q) => q,
        Err(_) => return HashSet::new(),
    };
    q.query_map([pid], |r| r.get::<_, String>(0)).map(|it| it.flatten().collect()).unwrap_or_default()
}

pub struct CodeOpts<'a> {
    pub rel: &'a str,
    pub depth: usize,
    pub budget: usize,
    pub cursor: usize,
    /// Merge codanna `~` results (stale files, uncovered files, Kotlin/Python).
    pub fallback: bool,
}

pub const RELS: &[&str] = &["callers", "callees", "refs", "conforms", "subtypes", "extensions", "overrides", "outline", "src", "impact"];

/// Callers' call sites grouped by file.
#[derive(Default)]
struct Sites {
    // path -> (line, caller display, caller handle, stale)
    by_file: BTreeMap<String, Vec<(i64, String, String)>>,
    callers: HashSet<i64>,
}

fn collect_sites(c: &Connection, targets: &[i64], call_only: bool, stale: &HashSet<String>) -> Result<(Sites, usize)> {
    let mut s = Sites::default();
    let role_sql = if call_only { "o.roles & 32 != 0 AND o.rel_roles & 8192 != 0" } else { "o.roles & 4 != 0" };
    let sql = format!(
        "SELECT r.path, o.line, o.col, o.rel_sym, c.name, c.meta, o.sym FROM occ o JOIN swift_rec r ON r.id = o.rec
         LEFT JOIN node c ON c.id = o.rel_sym WHERE o.sym IN ({}) AND {role_sql} GROUP BY r.path, o.line, o.col, o.rel_sym ORDER BY r.path, o.line",
        ids_sql(targets)
    );
    let mut q = c.prepare_cached(&sql)?;
    let rows = q.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut n = 0;
    for r in rows.flatten() {
        let (path, line, rel, cname, cmeta) = r;
        let parent =
            cmeta.and_then(|m| serde_json::from_str::<serde_json::Value>(&m).ok()).and_then(|m| m["parent"].as_str().map(str::to_string));
        let who = match (rel, cname) {
            (0, _) | (_, None) => "(top level)".to_string(),
            (_, Some(nm)) => match parent {
                Some(p) if !p.is_empty() => format!("{p}.{nm}"),
                _ => nm,
            },
        };
        let h = if rel != 0 { handle("symbol", rel) } else { String::new() };
        let mark = if stale.contains(&path) { "~" } else { "" };
        s.by_file.entry(path).or_default().push((line, format!("{mark}{who}"), h));
        if rel != 0 {
            s.callers.insert(rel);
        }
        n += 1;
    }
    Ok((s, n))
}

fn fmt_sites(s: &Sites, lines: &mut Vec<String>) {
    for (path, v) in &s.by_file {
        lines.push(format!("{path} ({})", v.len()));
        for (line, who, h) in v {
            lines.push(format!("  :{line} {who} {h}").trim_end().to_string());
        }
    }
}

/// Optional codanna fallback (`codanna = true` in config.toml).
fn codanna_code(p: &Project, sym: &str, o: &CodeOpts) -> Option<String> {
    if o.rel == "outline" && (sym.contains('/') || sym.contains('.')) && p.root.join(sym.trim()).is_file() {
        let abs = crate::codanna::mirror_dir(p).join(sym.trim());
        let mut lines = vec![format!("~ outline {} (tree-sitter-style scan)", sym.trim())];
        lines.extend(crate::codanna::outline_text(&abs, sym.trim()));
        return Some(page(&lines, o.cursor, o.budget));
    }
    if (o.rel != "outline" || !(sym.contains('/') || sym.ends_with(".swift")))
        && let Ok(mut lines) = crate::codanna::run_code(p, sym, o.rel, o.depth)
    {
        lines.push("~ = tree-sitter result (codanna), not compiler-resolved".into());
        return Some(page(&lines, o.cursor, o.budget));
    }
    None
}

/// Entry point used by the CLI and the MCP server.
pub fn run(c: &Connection, p: &Project, sym: &str, o: &CodeOpts) -> Result<String> {
    if !RELS.contains(&o.rel) {
        bail!("unknown --rel {:?}; one of {}", o.rel, RELS.join("|"));
    }
    let t0 = std::time::Instant::now();
    let swift = crate::swiftimport::has_symbols(c, p.id);
    if std::env::var_os("FOXDB_TRACE").is_some() {
        eprintln!("has_symbols {:?}", t0.elapsed());
    }
    let cands = if sym.trim().is_empty() || !swift { vec![] } else { resolve(c, p.id, sym)? };
    if cands.is_empty() && !sym.trim().is_empty() && !o.rel.is_empty() && (!swift || !sym.starts_with("s:")) {
        // Kotlin/Java/Python/..., or a name the index store does not know: tree-sitter, labelled `~`.
        if crate::config::get().codanna && crate::codanna::mirror_ready(p) {
            if let Some(out) = codanna_code(p, sym, o) {
                return Ok(out);
            }
        } else if crate::tsindex::has_symbols(c, p.id)
            && let Some(out) = crate::tscode::run(c, p, sym, o.rel, o.depth, o.cursor, o.budget)?
        {
            return Ok(out);
        }
    }
    if o.rel == "impact" && sym.trim().is_empty() {
        return impact_diff(c, p, o);
    }
    if o.rel == "outline" && cands.is_empty() {
        // `outline <path>`
        let rel = sym.trim().trim_start_matches("./");
        if let Ok(abs) = std::fs::canonicalize(rel)
            && let Ok(r) = abs.strip_prefix(&p.root)
        {
            return outline_file(c, p, &r.to_string_lossy(), o);
        }
        return outline_file(c, p, rel, o);
    }
    if cands.is_empty() {
        return Ok(format!("no symbol matches {sym:?} (try `fox find --kind symbol {sym}`)\n"));
    }
    if std::env::var_os("FOXDB_TRACE").is_some() {
        eprintln!("resolved {:?}", t0.elapsed());
    }
    let top = &cands[0];
    let mut lines: Vec<String> = vec![];
    let ids: Vec<i64> = vec![top.id];
    if cands.len() > 1 {
        let others: Vec<String> = cands.iter().skip(1).take(6).map(|s| format!("{} {}", s.h(), s.qname())).collect();
        lines.push(format!(
            "{} matches for {sym:?}; using {} {} (others: {}{})",
            cands.len(),
            top.h(),
            top.qname(),
            others.join(", "),
            if cands.len() > 7 { ", ..." } else { "" }
        ));
    }
    let stale = stale_paths(c, p.id);
    match o.rel {
        "callers" => {
            let mut targets = ids.clone();
            let mut via: Vec<Sym> = vec![];
            if is_callable(&top.kind) {
                // Calls through the protocol requirement / superclass method this one overrides.
                let mut q = c.prepare_cached(&format!(
                    "SELECT {COLS} FROM edge e JOIN node n ON n.id = e.dst WHERE e.src = ?1 AND e.kind = 'OVERRIDES'"
                ))?;
                for r in q.query_map([top.id], map_sym)?.flatten() {
                    targets.push(r.id);
                    via.push(r);
                }
            }
            let (direct, nd) = collect_sites(c, &ids, is_callable(&top.kind), &stale)?;
            lines.push(
                format!("callers of {}  {}", top.row().trim_start_matches('~'), if top.stale { "~stale" } else { "" })
                    .trim_end()
                    .to_string(),
            );
            let mut total = nd;
            let mut files = direct.by_file.len();
            let mut sites = direct;
            let mut note = String::new();
            if !via.is_empty() {
                let (vs, nv) = collect_sites(c, &targets[1..], is_callable(&top.kind), &stale)?;
                // Count only sites not already listed.
                let before = sites.by_file.clone();
                for (path, v) in vs.by_file {
                    let e = sites.by_file.entry(path.clone()).or_default();
                    for item in v {
                        if !before.get(&path).is_some_and(|b| b.iter().any(|x| x.0 == item.0)) {
                            e.push((item.0, format!("{} (via {})", item.1, via[0].qname()), item.2));
                            total += 1;
                        }
                    }
                    e.sort();
                }
                let _ = nv;
                files = sites.by_file.len();
                note = format!(" incl. via {}", via.iter().map(|v| v.qname()).collect::<Vec<_>>().join(","));
            }
            lines.push(format!("{total} call sites in {files} files{note}"));
            fmt_sites(&sites, &mut lines);
            if (o.fallback || top.stale) && crate::config::get().codanna {
                let known: HashSet<(String, String)> = sites
                    .by_file
                    .iter()
                    .flat_map(|(f, v)| {
                        v.iter().map(move |x| (f.clone(), x.1.trim_start_matches('~').rsplit('.').next().unwrap_or("").to_string()))
                    })
                    .collect();
                crate::codanna::merge_callers(c, p, top, &stale, &known, &mut lines);
            }
        }
        "refs" => {
            let (sites, n) = collect_sites(c, &ids, false, &stale)?;
            lines.push(format!("refs of {}", top.row()));
            lines.push(format!("{n} references in {} files", sites.by_file.len()));
            fmt_sites(&sites, &mut lines);
        }
        "callees" => {
            let sql = "SELECT d.id, d.name, d.sig, d.meta, d.path, d.line, d.stale, d.source, r.path, o.line FROM occ o JOIN swift_rec r ON r.id = o.rec JOIN node d ON d.id = o.sym
                       WHERE o.rel_sym = ?1 AND o.roles & 32 != 0 AND o.rel_roles & 8192 != 0 GROUP BY o.sym, o.line, o.col ORDER BY o.line";
            let mut q = c.prepare_cached(sql)?;
            let rows = q.query_map([top.id], |r| Ok((map_sym(r)?, r.get::<_, i64>(9)?)))?;
            let mut by: Vec<(Sym, Vec<i64>)> = vec![];
            for (s, line) in rows.flatten() {
                match by.iter_mut().find(|(x, _)| x.id == s.id) {
                    Some((_, v)) => v.push(line),
                    None => by.push((s, vec![line])),
                }
            }
            lines.push(format!("callees of {}  (project symbols; framework calls omitted)", top.row()));
            lines.push(format!("{} callees", by.len()));
            for (s, ls) in by {
                let at = s.path.as_deref().map(|p| format!("{p}:{}", s.line)).unwrap_or_default();
                lines.push(format!(
                    "{} {} {}  {at}  @{}",
                    s.h(),
                    s.kind,
                    s.qname(),
                    ls.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
                ));
            }
        }
        "conforms" | "subtypes" | "extensions" | "overrides" => relation(c, top, o.rel, &mut lines)?,
        "outline" => outline_sym(c, p, top, &mut lines)?,
        "src" => src(p, top, o, &mut lines)?,
        "impact" => impact(c, p, &cands[..1], o, &stale, &mut lines)?,
        _ => unreachable!(),
    }
    if lines.iter().any(|l| l.starts_with('~') || l.contains(" ~") || l.starts_with("  ~")) {
        lines.push("~ = stale (file changed since its unit was built) or merged tree-sitter result".into());
    }
    Ok(page(&lines, o.cursor, o.budget))
}

fn relation(c: &Connection, top: &Sym, rel: &str, lines: &mut Vec<String>) -> Result<()> {
    let list = |sql: &str, id: i64| -> Result<Vec<Sym>> {
        let mut q = c.prepare_cached(sql)?;
        Ok(q.query_map([id], map_sym)?.flatten().collect())
    };
    match rel {
        "conforms" => {
            let conformers = list(
                &format!(
                    "SELECT {COLS} FROM edge e JOIN node n ON n.id = e.src WHERE e.dst = ?1 AND e.kind = 'CONFORMS_TO' AND json_extract(n.meta,'$.k') != 'extension' ORDER BY n.path, n.line"
                ),
                top.id,
            )?;
            let supers = list(
                &format!(
                    "SELECT {COLS} FROM edge e JOIN node n ON n.id = e.dst WHERE e.src = ?1 AND e.kind IN ('CONFORMS_TO','INHERITS') ORDER BY n.name"
                ),
                top.id,
            )?;
            lines.push(format!("conformance of {}", top.row()));
            if top.kind == "protocol" || !conformers.is_empty() {
                lines.push(format!("{} conformers", conformers.len()));
                lines.extend(conformers.iter().map(Sym::row));
            }
            if !supers.is_empty() {
                lines.push(format!("conforms to / inherits ({})", supers.len()));
                lines.extend(supers.iter().map(Sym::row));
            }
        }
        "subtypes" => {
            let v = list(
                &format!(
                    "SELECT {COLS} FROM edge e JOIN node n ON n.id = e.src WHERE e.dst = ?1 AND e.kind = 'INHERITS' ORDER BY n.path, n.line"
                ),
                top.id,
            )?;
            lines.push(format!("{} subtypes of {}", v.len(), top.row()));
            lines.extend(v.iter().map(Sym::row));
        }
        "extensions" => {
            let v = list(
                &format!(
                    "SELECT {COLS} FROM edge e JOIN node n ON n.id = e.src WHERE e.dst = ?1 AND e.kind = 'EXTENDS' ORDER BY n.path, n.line"
                ),
                top.id,
            )?;
            lines.push(format!("{} extensions of {}", v.len(), top.row()));
            for s in &v {
                lines.push(format!("{} ext {}  {}:{}", s.h(), s.name, s.path.clone().unwrap_or_default(), s.line));
            }
        }
        _ => {
            let up =
                list(&format!("SELECT {COLS} FROM edge e JOIN node n ON n.id = e.dst WHERE e.src = ?1 AND e.kind = 'OVERRIDES'"), top.id)?;
            let down = list(
                &format!(
                    "SELECT {COLS} FROM edge e JOIN node n ON n.id = e.src WHERE e.dst = ?1 AND e.kind = 'OVERRIDES' ORDER BY n.path, n.line"
                ),
                top.id,
            )?;
            lines.push(format!("overrides of {}", top.row()));
            if !up.is_empty() {
                lines.push("overrides / witnesses:".into());
                lines.extend(up.iter().map(Sym::row));
            }
            if !down.is_empty() {
                lines.push(format!("overridden / implemented by ({}):", down.len()));
                lines.extend(down.iter().map(Sym::row));
            }
        }
    }
    Ok(())
}

fn member_line(s: &Sym, indent: usize) -> String {
    let mark = if s.stale { "~" } else { "" };
    format!("{}{mark}{} {} {}", " ".repeat(indent), s.line, s.kind, s.sig)
}

fn outline_file(c: &Connection, p: &Project, rel: &str, _o: &CodeOpts) -> Result<String> {
    let mut q = c.prepare(&format!(
        "SELECT {COLS} FROM node n WHERE n.project_id = ?1 AND n.kind = 'symbol' AND n.path = ?2 ORDER BY n.line, n.id"
    ))?;
    let syms: Vec<Sym> = q.query_map(rusqlite::params![p.id, rel], map_sym)?.flatten().collect();
    if syms.is_empty() {
        return Ok(format!("no indexed symbols in {rel:?}\n"));
    }
    let mut lines = vec![format!("outline {rel}  ({} symbols)", syms.len())];
    for s in &syms {
        let indent = if s.parent.as_deref().is_some_and(|x| !x.is_empty()) { 2 } else { 0 };
        let mut l = member_line(s, indent);
        if s.parent.is_some() && indent == 2 {
            l = format!("{l}  [{}]", s.parent.clone().unwrap_or_default());
        }
        lines.push(format!("{l} {}", s.h()));
    }
    Ok(page(&lines, _o.cursor, _o.budget))
}

fn outline_sym(c: &Connection, p: &Project, top: &Sym, lines: &mut Vec<String>) -> Result<()> {
    // Members: symbols whose recorded parent is this type or one of its extensions (any file).
    let ext_ids: Vec<i64> = {
        let mut e = c.prepare("SELECT src FROM edge WHERE dst = ?1 AND kind = 'EXTENDS'")?;
        e.query_map([top.id], |r| r.get(0))?.flatten().collect()
    };
    let mut pids = vec![top.id];
    pids.extend(ext_ids);
    let mut q = c.prepare(&format!(
        "SELECT {COLS} FROM node n WHERE n.kind = 'symbol' AND json_extract(n.meta, '$.pid') IN ({}) ORDER BY n.path, n.line",
        ids_sql(&pids)
    ))?;
    let _ = p;
    let members: Vec<Sym> = q.query_map([], map_sym)?.flatten().collect();
    lines.push(format!("outline {}  ({} members)", top.row(), members.len()));
    let mut last = String::new();
    for m in &members {
        let path = m.path.clone().unwrap_or_default();
        if path != last {
            lines.push(path.clone());
            last = path;
        }
        lines.push(format!("{} {}", member_line(m, 1), m.h()));
    }
    Ok(())
}

/// Locate the symbol's current line even if the file was edited since the build (stale).
fn locate(src: &[&str], s: &Sym) -> Option<usize> {
    let want = (s.line as usize).saturating_sub(1);
    let hit = |i: usize| src.get(i).is_some_and(|l| l.contains(s.name.as_str()));
    if hit(want) {
        return Some(want);
    }
    if !s.stale {
        return None;
    }
    for d in 1..400usize {
        for i in [want.checked_sub(d), Some(want + d)].into_iter().flatten() {
            if hit(i) && src[i].contains(&format!("func {}", s.name)) || (hit(i) && src[i].contains(&format!("{} ", s.kind))) {
                return Some(i);
            }
        }
    }
    None
}

/// End line (0-based, inclusive) of the declaration starting at `start`, by brace matching.
pub fn decl_end(src: &[&str], start: usize) -> usize {
    let mut depth = 0i32;
    let mut paren = 0i32;
    let mut opened = false;
    let mut in_block_comment = false;
    for (i, line) in src.iter().enumerate().skip(start).take(4000) {
        let b: Vec<char> = line.chars().collect();
        let mut j = 0;
        let mut in_str = false;
        while j < b.len() {
            let ch = b[j];
            if in_block_comment {
                if ch == '*' && b.get(j + 1) == Some(&'/') {
                    in_block_comment = false;
                    j += 1;
                }
            } else if in_str {
                if ch == '\\' {
                    j += 1;
                } else if ch == '"' {
                    in_str = false;
                }
            } else {
                match ch {
                    '/' if b.get(j + 1) == Some(&'/') => break,
                    '/' if b.get(j + 1) == Some(&'*') => {
                        in_block_comment = true;
                        j += 1;
                    }
                    '"' => in_str = true,
                    '(' | '[' => paren += 1,
                    ')' | ']' => paren -= 1,
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => {
                        depth -= 1;
                        if opened && depth <= 0 {
                            return i;
                        }
                    }
                    _ => {}
                }
            }
            j += 1;
        }
        if !opened && paren <= 0 {
            // Declaration without a body unless the next line opens one.
            let next = src.get(i + 1).map(|l| l.trim_start()).unwrap_or("");
            if !(next.starts_with('{')
                || next.starts_with("where ")
                || next.starts_with("-> ")
                || next.starts_with("throws")
                || next.starts_with("async")
                || line.trim_end().ends_with(['{', ',', '(']))
            {
                return i;
            }
        }
    }
    start
}

fn src(p: &Project, top: &Sym, o: &CodeOpts, lines: &mut Vec<String>) -> Result<()> {
    let Some(rel) = &top.path else { bail!("{} is external (no source in this project)", top.qname()) };
    let abs = p.root.join(rel);
    let Some(text) = crate::guard::read_text(&abs, rel, 4 << 20, false) else { bail!("cannot read {rel}") };
    let all: Vec<&str> = text.lines().collect();
    let Some(start) = locate(&all, top) else {
        bail!("{} no longer at {rel}:{} (file changed; rebuild to refresh)", top.qname(), top.line)
    };
    let end = decl_end(&all, start);
    let mut first = start;
    while first > 0 {
        let t = all[first - 1].trim_start();
        if t.starts_with("///") || t.starts_with('@') || t.starts_with("/**") || t.starts_with("* ") {
            first -= 1;
        } else {
            break;
        }
    }
    lines.push(format!(
        "{} {}:{}-{}{}",
        top.h(),
        rel,
        first + 1,
        end + 1,
        if top.stale { "  ~ file edited since build; located by name" } else { "" }
    ));
    for (i, l) in all.iter().enumerate().take(end + 1).skip(first) {
        lines.push(format!("{}\t{}", i + 1, clip(l, 220)));
    }
    let _ = o;
    Ok(())
}

fn impact(c: &Connection, _p: &Project, roots: &[Sym], o: &CodeOpts, stale: &HashSet<String>, lines: &mut Vec<String>) -> Result<()> {
    let ids: Vec<i64> = roots.iter().map(|s| s.id).collect();
    let (levels, ..) = bfs_callers(c, &ids, o.depth.max(1))?;
    emit_impact(c, &format!("impact of {}", roots.iter().map(|s| s.qname()).collect::<Vec<_>>().join(", ")), &levels, stale, lines)
}

/// Reverse BFS over CALLS / REFERENCES / OVERRIDES edges. Returns (levels, visited count).
fn bfs_callers(c: &Connection, roots: &[i64], depth: usize) -> Result<(Vec<Vec<i64>>, usize)> {
    let mut seen: HashSet<i64> = roots.iter().copied().collect();
    let mut frontier: Vec<i64> = roots.to_vec();
    let mut levels = vec![];
    for _ in 0..depth {
        if frontier.is_empty() || seen.len() > 5000 {
            break;
        }
        let sql = format!("SELECT DISTINCT src FROM edge WHERE dst IN ({}) AND kind IN ('CALLS','REFERENCES')", ids_sql(&frontier));
        let mut q = c.prepare(&sql)?;
        let next: Vec<i64> = q.query_map([], |r| r.get(0))?.flatten().filter(|i| seen.insert(*i)).collect();
        levels.push(next.clone());
        frontier = next;
    }
    Ok((levels, seen.len()))
}

fn emit_impact(c: &Connection, title: &str, levels: &[Vec<i64>], stale: &HashSet<String>, lines: &mut Vec<String>) -> Result<()> {
    let all: Vec<i64> = levels.iter().flatten().copied().collect();
    let mut by_file: BTreeMap<String, usize> = BTreeMap::new();
    let mut tests = 0;
    let mut syms: HashMap<i64, Sym> = HashMap::new();
    for chunk in all.chunks(500) {
        let mut q = c.prepare(&format!("SELECT {COLS} FROM node n WHERE n.id IN ({})", ids_sql(chunk)))?;
        for s in q.query_map([], map_sym)?.flatten() {
            syms.insert(s.id, s);
        }
    }
    for s in syms.values() {
        if let Some(p) = &s.path {
            *by_file.entry(p.clone()).or_default() += 1;
            if p.contains("Tests/") || p.contains("Tests.swift") {
                tests += 1;
            }
        }
    }
    lines.push(format!(
        "{title}: {} symbols in {} files ({} in tests); depth counts {}",
        all.len(),
        by_file.len(),
        tests,
        levels.iter().map(|l| l.len().to_string()).collect::<Vec<_>>().join("/")
    ));
    for (d, lvl) in levels.iter().enumerate() {
        let mut v: Vec<&Sym> = lvl.iter().filter_map(|i| syms.get(i)).collect();
        v.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
        lines.push(format!("depth {}: {} symbols", d + 1, v.len()));
        for s in v {
            let mark = if s.path.as_ref().is_some_and(|p| stale.contains(p)) { "~" } else { "" };
            lines.push(format!("  {mark}{} {} {}  {}:{}", s.h(), s.kind, s.qname(), s.path.clone().unwrap_or_default(), s.line));
        }
    }
    let mut files: Vec<(&String, &usize)> = by_file.iter().collect();
    files.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    lines.push(format!("files: {}", files.iter().take(12).map(|(f, n)| format!("{f}({n})")).collect::<Vec<_>>().join(" ")));
    Ok(())
}

/// `impact` with no target: symbols defined in files changed vs HEAD (git diff), then their callers.
fn impact_diff(c: &Connection, p: &Project, o: &CodeOpts) -> Result<String> {
    let out = std::process::Command::new("git").arg("-C").arg(&p.root).args(["diff", "--name-only", "HEAD"]).output()?;
    let files: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).filter(|f| f.ends_with(".swift")).collect();
    if files.is_empty() {
        return Ok("no changed Swift files vs HEAD\n".into());
    }
    let mut roots: Vec<i64> = vec![];
    for f in &files {
        let mut q = c.prepare("SELECT id FROM node WHERE project_id = ?1 AND kind = 'symbol' AND path = ?2 AND json_extract(meta,'$.k') IN ('func','method','classmethod','staticmethod','init','property','var','classproperty','staticproperty')")?;
        roots.extend(q.query_map(rusqlite::params![p.id, f], |r| r.get::<_, i64>(0))?.flatten());
    }
    let stale = stale_paths(c, p.id);
    let (levels, ..) = bfs_callers(c, &roots, o.depth.max(1))?;
    let mut lines = vec![format!("{} changed swift files, {} symbols defined in them", files.len(), roots.len())];
    emit_impact(c, "impact of working-tree diff", &levels, &stale, &mut lines)?;
    Ok(page(&lines, o.cursor, o.budget))
}

/// Write the `parent id` into symbol meta (second pass after all definitions exist).
pub fn link_parents(tx: &rusqlite::Connection, pid: i64) -> Result<()> {
    // meta.parent is a name; the parent id comes from the CHILD_OF relation stored on the def occurrence.
    tx.execute(
        "UPDATE node SET meta = json_set(COALESCE(meta,'{}'), '$.pid', (
            SELECT o.rel_sym FROM occ o WHERE o.sym = node.id AND o.roles & ?2 != 0 AND o.rel_roles & ?3 != 0 LIMIT 1))
         WHERE project_id = ?1 AND source = 'indexstore' AND kind = 'symbol' AND path IS NOT NULL",
        rusqlite::params![pid, ix::R_DEF as i64, ix::REL_CHILD_OF as i64],
    )?;
    Ok(())
}

pub fn summary(c: &Connection, pid: i64) -> String {
    let mut s = String::new();
    let n: i64 = c
        .query_row("SELECT COUNT(*) FROM node WHERE project_id = ?1 AND kind = 'symbol' AND path IS NOT NULL", [pid], |r| r.get(0))
        .unwrap_or(0);
    let e: i64 =
        c.query_row("SELECT COUNT(*) FROM edge WHERE src IN (SELECT id FROM node WHERE project_id = ?1)", [pid], |r| r.get(0)).unwrap_or(0);
    let _ = write!(s, "{n} symbols, {e} edges");
    s
}
