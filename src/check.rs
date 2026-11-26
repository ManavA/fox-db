//! `fox check`: the edit-completeness gate. It reads the working-tree diff against HEAD, finds
//! the functions whose signature lines the diff touches, and lists each caller whose enclosing
//! function (or, at file top level, whose line) the diff leaves alone. Callers resolved with
//! certainty block; callers guessed by name are listed apart and never block.
//!
//! The tree-sitter index answers, so `fresh` re-indexes changed files first: the agent's own
//! edits move lines and add callers the index has not seen yet.

use crate::project::Project;
use anyhow::{Result, bail};
use rusqlite::Connection;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

/// One call site the diff did not touch.
#[derive(Debug, Clone)]
pub struct Site {
    pub path: String,
    pub line: i64,
    /// `Type.name c:1f`, or `(top)` for a file-level use.
    pub caller: String,
    /// The changed definitions it uses, comma-separated.
    pub target: String,
    /// Same-name candidates of a guessed resolution (0: certain).
    pub amb: i64,
}

#[derive(Debug, Default)]
pub struct Report {
    /// Changed definitions as `name path:line`.
    pub changed: Vec<String>,
    /// Certain call sites of the changed definitions, touched or not.
    pub certain: usize,
    pub missed: Vec<Site>,
    pub guesses: Vec<Site>,
}

impl Report {
    pub fn blocks(&self) -> bool {
        !self.missed.is_empty()
    }
}

/// Most rows of each list that [`render`] prints.
const MISSED_ROWS: usize = 30;
const GUESS_ROWS: usize = 10;
/// Longest signature [`header_end`] reads past the definition's first line.
const HEADER_LINES: i64 = 20;

/// Lines of the working tree the diff touches, per path: inclusive ranges.
type Hunks = HashMap<String, Vec<(i64, i64)>>;

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Hunks of `git diff -U0 HEAD` on the new side, plus every untracked file whole. A pure
/// deletion touches the line before it.
pub fn hunks(root: &Path) -> Result<Hunks> {
    let mut h = Hunks::new();
    let mut cur: Option<String> = None;
    for l in git(root, &["diff", "-U0", "--no-color", "--no-ext-diff", "HEAD", "--"])?.lines() {
        if let Some(p) = l.strip_prefix("+++ ") {
            cur = p.strip_prefix("b/").map(String::from);
        } else if let (Some(path), Some(rest)) = (&cur, l.strip_prefix("@@ ")) {
            let Some(new) = rest.split_whitespace().find_map(|w| w.strip_prefix('+')) else { continue };
            let (start, n): (i64, i64) = match new.split_once(',') {
                Some((s, n)) => (s.parse().unwrap_or(0), n.parse().unwrap_or(1)),
                None => (new.parse().unwrap_or(0), 1),
            };
            let range = if n == 0 { (start.max(1), start.max(1)) } else { (start, start + n - 1) };
            h.entry(path.clone()).or_default().push(range);
        }
    }
    for f in git(root, &["ls-files", "--others", "--exclude-standard"])?.lines().filter(|f| !f.is_empty()) {
        h.entry(f.to_string()).or_default().push((1, i64::MAX));
    }
    Ok(h)
}

fn touched(h: &Hunks, path: &str, a: i64, b: i64) -> bool {
    h.get(path).is_some_and(|v| v.iter().any(|&(s, e)| s <= b && a <= e))
}

/// Last line of a definition's signature: the first line, from `line` on, where the
/// parentheses opened so far are closed and that opens a body (`{`), ends one (`:` in Python,
/// `;` for a prototype) or closed a parameter list.
fn header_end(lines: &[&str], line: i64, end_line: i64) -> i64 {
    let mut depth = 0i64;
    let mut paren = false;
    let last = end_line.max(line).min(line + HEADER_LINES);
    for n in line..=last {
        let Some(t) = lines.get((n - 1) as usize) else { break };
        for ch in t.chars() {
            match ch {
                '(' => {
                    depth += 1;
                    paren = true;
                }
                ')' => depth -= 1,
                _ => {}
            }
        }
        let t = t.trim_end();
        if depth <= 0 && (paren || t.contains('{') || t.ends_with(':') || t.ends_with(';')) {
            return n;
        }
    }
    line
}

/// The checkout `cwd` is in must be the one the project indexes: a linked worktree's files are
/// not in the index.
fn same_checkout(p: &Project, cwd: &Path) -> Result<()> {
    let top = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?.trim());
    let canon = |x: &Path| std::fs::canonicalize(x).unwrap_or_else(|_| x.to_path_buf());
    if canon(&top) != canon(&p.root) {
        bail!("{} is a linked worktree; fox indexes {} only, so run fox check there", top.display(), p.root.display());
    }
    Ok(())
}

/// Run the gate for the checkout at `cwd` (re-indexing first when `fresh`).
pub fn run(c: &mut Connection, p: &Project, cwd: &Path, fresh: bool) -> Result<Report> {
    same_checkout(p, cwd)?;
    let h = hunks(&p.root)?;
    if fresh && !h.is_empty() {
        let st = crate::inventory::index_project(c, p)?;
        crate::tsindex::index_project_with(c, p, st.delta.as_ref())?;
    }
    let mut r = Report::default();
    let mut ids: Vec<i64> = vec![];
    let mut names: HashMap<i64, String> = HashMap::new();
    let mut paths: Vec<&String> = h.keys().collect();
    paths.sort();
    let mut q = c.prepare(
        "SELECT id, name, json_extract(meta,'$.k'), line, end_line FROM node
         WHERE project_id = ?1 AND kind = 'symbol' AND source = 'treesitter' AND path = ?2 ORDER BY line",
    )?;
    for path in paths {
        let defs: Vec<(i64, String, String, i64, i64)> = q
            .query_map(rusqlite::params![p.id, path], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?.unwrap_or_default(), r.get(3)?, r.get(4)?))
            })?
            .flatten()
            .filter(|d| crate::tsindex::is_callable_kind(&d.2))
            .collect();
        if defs.is_empty() {
            continue;
        }
        let text = std::fs::read_to_string(p.root.join(path)).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        for (id, name, _, line, end) in defs {
            if touched(&h, path, line, header_end(&lines, line, end)) {
                r.changed.push(format!("{name} {path}:{line}"));
                names.insert(id, name);
                ids.push(id);
            }
        }
    }
    if ids.is_empty() {
        return Ok(r);
    }
    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let mut q = c.prepare(&format!(
        "SELECT r.path, r.line, r.src, s.name, json_extract(s.meta,'$.parent'), s.line, s.end_line, r.amb, r.dst
         FROM ts_ref r LEFT JOIN node s ON s.id = r.src AND r.src != 0
         WHERE r.dst IN ({list}) ORDER BY r.path, r.line"
    ))?;
    // (path, line) -> (site, touched)
    let mut by: BTreeMap<(String, i64), (Site, bool)> = BTreeMap::new();
    for row in q
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
            ))
        })?
        .flatten()
    {
        let (path, line, src, sname, sparent, sline, send, amb, dst) = row;
        let hit = match (sname.is_some(), sline, send) {
            (true, Some(a), Some(b)) => touched(&h, &path, a, b),
            _ => touched(&h, &path, line, line),
        };
        let caller = match (sname, sparent) {
            (Some(n), Some(p)) if !p.is_empty() => format!("{p}.{n} c:{src:x}"),
            (Some(n), _) => format!("{n} c:{src:x}"),
            _ => "(top)".into(),
        };
        let target = names.get(&dst).cloned().unwrap_or_default();
        let e = by.entry((path.clone(), line)).or_insert_with(|| (Site { path, line, caller, target: String::new(), amb }, hit));
        if !e.0.target.split(", ").any(|t| t == target) {
            e.0.target = if e.0.target.is_empty() { target } else { format!("{}, {target}", e.0.target) };
        }
        e.0.amb = e.0.amb.min(amb);
    }
    for (site, hit) in by.into_values() {
        if site.amb == 0 {
            r.certain += 1;
        }
        match (hit, site.amb) {
            (true, _) => {}
            (false, 0) => r.missed.push(site),
            (false, _) => r.guesses.push(site),
        }
    }
    Ok(r)
}

fn row(s: &Site) -> String {
    format!("  {}:{} {} calls {}{}", s.path, s.line, s.caller, s.target, crate::tscode::guess_note(s.amb))
}

fn rows(out: &mut String, v: &[Site], cap: usize) {
    for s in v.iter().take(cap) {
        out.push_str(&row(s));
        out.push('\n');
    }
    if v.len() > cap {
        out.push_str(&format!("  +{} more\n", v.len() - cap));
    }
}

/// The gate's answer: a head line, then one row per missed site, then the guessed ones.
pub fn render(r: &Report) -> String {
    let mut out = if r.changed.is_empty() {
        "fox check: ok; the diff changes no function signature\n".to_string()
    } else if r.certain == 0 {
        "fox check: ok; the changed definitions have no certain callers\n".to_string()
    } else if r.missed.is_empty() {
        format!("fox check: ok; all {} certain callers of the changed definitions are in the diff\n", r.certain)
    } else {
        format!("fox check: {} of {} certain callers of the changed definitions are not in the diff\n", r.missed.len(), r.certain)
    };
    if !r.changed.is_empty() {
        out.push_str(&format!("changed: {}\n", r.changed.join(", ")));
    }
    rows(&mut out, &r.missed, MISSED_ROWS);
    if !r.guesses.is_empty() {
        out.push_str("guessed callers not in the diff (not blocking; confirm the ones that matter):\n");
        rows(&mut out, &r.guesses, GUESS_ROWS);
    }
    out
}

/// `fox check [--project P] [--no-fresh]` (`--fresh`, the default, is accepted too).
pub fn cli(a: &crate::cli::Args, cwd: &Path) -> Result<Report> {
    let mut c = crate::db::open_rw()?;
    let p = match a.opt.get("project") {
        Some(name) => crate::project::by_name(&c, name)?.ok_or_else(|| anyhow::anyhow!("no project named {name}; see fox status"))?,
        None => crate::project::find(&c, &crate::project::locate(cwd))?
            .ok_or_else(|| anyhow::anyhow!("project not indexed; run `fox index` first"))?,
    };
    let at = if a.opt.contains_key("project") { p.root.clone() } else { cwd.to_path_buf() };
    run(&mut c, &p, &at, !a.flags.contains("no-fresh"))
}
