//! Docs anchored to code. A Markdown doc's backticked, code-shaped spans (`Name`, `Type.member`,
//! `path/to/file.rs:12`) are its citations, each tied to the section it appears in.
//!
//! - `drift`: which cited symbols and paths changed, moved or vanished since the doc's last commit,
//!   from git and the index only (no LLM), so the answer is deterministic.
//! - `pack`: the doc plus the exact signature skeletons of what it cites, within a budget: the
//!   minimal, reproducible context for a fresh (zero-context) reviewer.

use crate::project::Project;
use anyhow::{Result, bail};
use rusqlite::{Connection, params};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Cite {
    pub text: String,
    /// 1-based line in the doc.
    pub line: usize,
    pub section: String,
    pub section_line: usize,
}

fn is_ident(s: &str) -> bool {
    let s = s.strip_suffix("()").unwrap_or(s);
    !s.is_empty()
        && s.split("::").flat_map(|p| p.split('.')).all(|seg| {
            let mut ch = seg.chars();
            ch.next().is_some_and(|c| c.is_alphabetic() || c == '_') && ch.all(|c| c.is_alphanumeric() || c == '_')
        })
}

fn path_part(s: &str) -> &str {
    match s.rsplit_once(':') {
        Some((p, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => p,
        _ => s,
    }
}

fn is_path(s: &str) -> bool {
    let p = path_part(s);
    !p.contains(char::is_whitespace)
        && (p.contains('/')
            || p.rsplit_once('.').is_some_and(|(a, ext)| !a.is_empty() && (crate::lang::for_path(p).is_some() || ext == "md")))
        && p.chars().all(|c| c.is_alphanumeric() || "/._-+@".contains(c))
}

/// Identifier with a code signal: `_`, `.`, `::`, `()` or an uppercase letter. Plain lowercase
/// words still count when they resolve to a symbol.
fn strong(s: &str) -> bool {
    s.contains('_') || s.contains('.') || s.contains("::") || s.ends_with("()") || s.chars().any(char::is_uppercase)
}

/// Code-shaped backtick spans outside fenced blocks, in order, with their section.
pub fn citations(md: &str) -> Vec<Cite> {
    let mut out = vec![];
    let (mut section, mut section_line) = (String::new(), 0);
    let mut fence = false;
    for (i, l) in md.lines().enumerate() {
        let t = l.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        if let Some(h) = t.strip_prefix('#') {
            section = h.trim_start_matches('#').trim().to_string();
            section_line = i + 1;
            continue;
        }
        let parts: Vec<&str> = l.split('`').collect();
        for (k, span) in parts.iter().enumerate() {
            if k % 2 == 1 && k + 1 < parts.len() {
                let s = span.trim();
                if is_path(s) || is_ident(s) {
                    out.push(Cite { text: s.to_string(), line: i + 1, section: section.clone(), section_line });
                }
            }
        }
    }
    out
}

struct Resolved {
    path: String,
    line: i64,
    end: i64,
    name: String,
}

fn resolve_symbol(c: &Connection, pid: i64, cite: &str) -> Option<Resolved> {
    let s = cite.strip_suffix("()").unwrap_or(cite);
    let segs: Vec<&str> = s.split("::").flat_map(|p| p.split('.')).collect();
    let name = *segs.last()?;
    let parent = (segs.len() > 1).then(|| segs[segs.len() - 2]);
    let mut q = c
        .prepare_cached(
            "SELECT path, COALESCE(line, 1), COALESCE(end_line, line, 1), json_extract(meta, '$.parent') FROM node
             WHERE project_id = ?1 AND kind = 'symbol' AND name = ?2 COLLATE NOCASE AND path IS NOT NULL ORDER BY path, line",
        )
        .ok()?;
    let rows: Vec<(String, i64, i64, Option<String>)> =
        q.query_map(params![pid, name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).ok()?.flatten().collect();
    let pick = rows
        .iter()
        .filter(|r| parent.is_none_or(|p| r.3.as_deref().is_some_and(|x| x.rsplit('.').next().unwrap_or(x).eq_ignore_ascii_case(p))))
        .min_by_key(|r| (crate::search::is_test_path(&r.0), r.0.clone(), r.1))?;
    Some(Resolved { path: pick.0.clone(), line: pick.1, end: pick.2.max(pick.1), name: s.to_string() })
}

/// The cited symbol's definition in a fresh SCIP import, when the project has one: the same
/// name and container, preferring non-test paths.
fn scip_symbol(c: &Connection, pid: i64, cite: &str) -> Option<crate::scip::Def> {
    let s = cite.strip_suffix("()").unwrap_or(cite);
    let segs: Vec<&str> = s.split("::").flat_map(|p| p.split('.')).collect();
    let name = *segs.last()?;
    let parent = (segs.len() > 1).then(|| segs[segs.len() - 2]);
    crate::scip::defs_named(c, pid, name, parent).into_iter().min_by_key(|d| (crate::search::is_test_path(&d.path), d.path.clone(), d.line))
}

fn file_known(c: &Connection, p: &Project, rel: &str) -> bool {
    c.query_row("SELECT 1 FROM node WHERE project_id = ?1 AND path = ?2 AND kind = 'file'", params![p.id, rel], |_| Ok(())).is_ok()
        || p.root.join(rel).is_file()
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let o = Command::new("git").args(args).current_dir(root).stderr(std::process::Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

#[derive(Default)]
struct Changes {
    base: Option<String>,
    /// Commit time of `base` (unix seconds).
    base_time: i64,
    /// New-side line ranges touched per current path.
    hunks: HashMap<String, Vec<(i64, i64)>>,
    renamed: HashMap<String, String>,
    deleted: HashSet<String>,
}

fn changes_since_doc(root: &Path, doc: &str) -> Changes {
    let mut ch = Changes::default();
    let Some(head) = git(root, &["log", "-1", "--format=%H %ct", "--", doc]).filter(|s| !s.trim().is_empty()) else {
        return ch;
    };
    let mut it = head.split_whitespace();
    let base = it.next().unwrap_or_default().to_string();
    ch.base_time = it.next().and_then(|t| t.parse().ok()).unwrap_or(0);
    if let Some(ns) = git(root, &["diff", "--name-status", "-M", &base]) {
        for l in ns.lines() {
            let f: Vec<&str> = l.split('\t').collect();
            match f.first().map(|s| &s[..1]) {
                Some("R") if f.len() == 3 => {
                    ch.renamed.insert(f[1].to_string(), f[2].to_string());
                }
                Some("D") if f.len() == 2 => {
                    ch.deleted.insert(f[1].to_string());
                }
                _ => {}
            }
        }
    }
    if let Some(diff) = git(root, &["diff", "-U0", "--no-color", "-M", &base]) {
        let mut cur: Option<String> = None;
        for l in diff.lines() {
            if let Some(p) = l.strip_prefix("+++ ") {
                cur = p.strip_prefix("b/").map(String::from);
            } else if let (Some(f), Some(h)) = (&cur, l.strip_prefix("@@ ")) {
                // @@ -a,b +c,d @@
                if let Some(plus) = h.split_whitespace().find(|t| t.starts_with('+')) {
                    let mut it = plus[1..].split(',');
                    let start: i64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
                    let n: i64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
                    // A pure deletion (n = 0) still touches the line it sits at.
                    ch.hunks.entry(f.clone()).or_default().push((start, start + n.max(1) - 1));
                }
            }
        }
    }
    ch.base = Some(base);
    ch
}

fn read_doc(p: &Project, doc: &str) -> Result<String> {
    let Some(t) = crate::guard::read_text(&p.root.join(doc), doc, 4 << 20, false) else { bail!("cannot read {doc}") };
    Ok(t)
}

/// `fox drift <doc>`: cited symbols and paths that changed, moved or vanished since the doc's last
/// commit. Unchanged citations are not listed. Deterministic: git and the index only.
pub fn drift(c: &Connection, p: &Project, doc: &str) -> Result<String> {
    let text = read_doc(p, doc)?;
    let cites = citations(&text);
    let ch = changes_since_doc(&p.root, doc);
    let mut by_section: BTreeMap<(usize, String), Vec<String>> = BTreeMap::new();
    let (mut n_changed, mut n_moved, mut n_gone) = (0, 0, 0);
    let mut seen = HashSet::new();
    for ci in &cites {
        if !seen.insert((ci.section_line, ci.text.clone())) {
            continue;
        }
        let row = if is_path(&ci.text) {
            let rel = path_part(&ci.text).trim_start_matches("./");
            if file_known(c, p, rel) {
                ch.hunks.contains_key(rel).then(|| {
                    n_changed += 1;
                    format!("  changed  `{}` -> {rel} (edited since the doc)", ci.text)
                })
            } else if let Some(new) = ch.renamed.get(rel) {
                n_moved += 1;
                Some(format!("  moved    `{}` -> {new}", ci.text))
            } else {
                n_gone += 1;
                Some(format!("  vanished `{}`{}", ci.text, if ch.deleted.contains(rel) { " (deleted)" } else { "" }))
            }
        } else if let Some(d) = scip_symbol(c, p.id, &ci.text) {
            // Followed by its SCIP id: a move to another file is reported as a move, not an edit.
            let renamed = ch.renamed.values().any(|n| *n == d.path);
            let moved_after = d.prev_path.as_ref().filter(|_| d.moved_at.is_some_and(|t| t > ch.base_time) && ch.base.is_some());
            if renamed || moved_after.is_some() {
                n_moved += 1;
                let from = moved_after.map(|f| format!(" from {f}")).unwrap_or_default();
                Some(format!("  moved    `{}` -> {}:{}{from} (SCIP {})", ci.text, d.path, d.line, d.symbol))
            } else {
                let touched = ch.hunks.get(&d.path).is_some_and(|hs| hs.iter().any(|&(a, b)| a <= d.end && b >= d.line));
                touched.then(|| {
                    n_changed += 1;
                    let name = ci.text.strip_suffix("()").unwrap_or(&ci.text);
                    format!("  changed  `{}` -> {}:{} ({name} edited since the doc)", ci.text, d.path, d.line)
                })
            }
        } else {
            match resolve_symbol(c, p.id, &ci.text) {
                Some(r) => {
                    let touched = ch.hunks.get(&r.path).is_some_and(|hs| hs.iter().any(|&(a, b)| a <= r.end && b >= r.line));
                    touched.then(|| {
                        n_changed += 1;
                        format!("  changed  `{}` -> {}:{} ({} edited since the doc)", ci.text, r.path, r.line, r.name)
                    })
                }
                None if strong(&ci.text) => {
                    n_gone += 1;
                    Some(format!("  vanished `{}` (no symbol by that name now)", ci.text))
                }
                None => None,
            }
        };
        if let Some(r) = row {
            by_section.entry((ci.section_line, ci.section.clone())).or_default().push(r);
        }
    }
    let base = ch.base.as_deref().map(|b| b[..b.len().min(10)].to_string()).unwrap_or_else(|| "(not committed)".into());
    let mut out =
        format!("drift {doc} since {base}: {n_changed} changed, {n_moved} moved, {n_gone} vanished of {} citations\n", seen.len());
    for ((line, title), rows) in by_section {
        let _ = writeln!(out, "§ {} ({doc}:{line})", if title.is_empty() { "(top)" } else { &title });
        for r in rows {
            out.push_str(&r);
            out.push('\n');
        }
    }
    Ok(out)
}

/// `fox drift` with no doc: every Markdown file in the project with at least one drifted
/// citation, one summary line each (at most 200 docs are checked).
pub fn drift_all(c: &Connection, p: &Project) -> Result<String> {
    let mut q =
        c.prepare("SELECT path FROM node WHERE project_id = ?1 AND kind = 'file' AND lower(path) LIKE '%.md' ORDER BY path LIMIT 200")?;
    let docs: Vec<String> = q.query_map([p.id], |r| r.get(0))?.flatten().collect();
    let mut out = String::new();
    for d in docs {
        if let Ok(r) = drift(c, p, &d)
            && let Some(first) = r.lines().next()
            && !first.contains(": 0 changed, 0 moved, 0 vanished")
        {
            out.push_str(first);
            out.push('\n');
        }
    }
    if out.is_empty() {
        out.push_str("no drift: every cited symbol and path is unchanged since its doc's last commit\n");
    }
    Ok(out)
}

/// The declaration lines of a symbol: from its first line to the one that opens the body.
fn signature(p: &Project, path: &str, line: i64) -> Vec<String> {
    let Some(text) = crate::guard::read_text(&p.root.join(path), path, 4 << 20, false) else { return vec![] };
    let mut out = vec![];
    for l in text.lines().skip((line.max(1) - 1) as usize).take(6) {
        out.push(crate::query::clip(l.trim_end(), 160));
        let t = l.trim_end();
        if t.contains('{') || t.ends_with(':') || t.ends_with('=') || t.ends_with(';') {
            break;
        }
    }
    out
}

/// `fox pack <doc>`: the doc, then the signature skeletons of every symbol it cites and the
/// symbols of every file it cites, within `budget` tokens (4 bytes each). Code first gets up to
/// 40% of the budget; the doc gets the rest and is cut at a line if it must be.
pub fn pack(c: &Connection, p: &Project, doc: &str, budget: usize) -> Result<String> {
    let text = read_doc(p, doc)?;
    let max = budget.max(32) * 4;
    let mut code = String::from("## cited code\n");
    let mut missing: Vec<String> = vec![];
    let mut seen = HashSet::new();
    for ci in citations(&text) {
        if !seen.insert(ci.text.clone()) {
            continue;
        }
        if is_path(&ci.text) {
            let rel = path_part(&ci.text).trim_start_matches("./").to_string();
            if !file_known(c, p, &rel) {
                missing.push(ci.text);
                continue;
            }
            let mut q = c.prepare_cached(
                "SELECT name FROM node WHERE project_id = ?1 AND path = ?2 AND kind = 'symbol'
                 AND json_extract(meta, '$.parent') IS NULL ORDER BY line LIMIT 12",
            )?;
            let names: Vec<String> = q.query_map(params![p.id, rel], |r| r.get(0))?.flatten().collect();
            let _ = writeln!(code, "{rel}: {}", if names.is_empty() { "(no symbols)".into() } else { names.join(", ") });
        } else if let Some(r) = resolve_symbol(c, p.id, &ci.text) {
            let _ = writeln!(code, "{}:{} {}", r.path, r.line, r.name);
            for l in signature(p, &r.path, r.line) {
                let _ = writeln!(code, "  {l}");
            }
        } else if strong(&ci.text) {
            missing.push(ci.text);
        }
    }
    if !missing.is_empty() {
        let _ = writeln!(code, "not found: {}", missing.join(", "));
    }
    let code = crate::hook::cap_lines(&code, max * 2 / 5);
    let head = format!("# pack {doc}\n");
    let room = max.saturating_sub(code.len() + head.len() + 40);
    let mut body = crate::hook::cap_lines(&text, room);
    if body.len() < text.len() {
        body.push_str("... (doc cut to fit the budget)\n");
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    Ok(format!("{head}{body}{code}"))
}
