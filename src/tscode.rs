//! `fox code` over the native tree-sitter index. Rows carry `c:` handles and a leading `~`
//! (name-resolved, not compiler-resolved). Output is grouped under file headers so paths are
//! never repeated, and call sites are line numbers, not code.

use crate::account;
use crate::project::Project;
use crate::query::{clip, page};
use crate::tsindex::{is_callable_kind, is_type_kind};
use anyhow::{Result, bail};
use rusqlite::{Connection, params};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct TSym {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: i64,
    pub end_line: i64,
    pub parent: Option<String>,
}

impl TSym {
    pub fn qname(&self) -> String {
        match &self.parent {
            Some(p) if !p.is_empty() => format!("{p}.{}", self.name),
            _ => self.name.clone(),
        }
    }
    pub fn h(&self) -> String {
        format!("c:{:x}", self.id)
    }
    /// `~c:1f method Type.name  path:line`
    pub fn row(&self) -> String {
        format!("~{} {} {}  {}:{}", self.h(), self.kind, self.qname(), self.path, self.line)
    }
}

const COLS: &str = "n.id, n.name, json_extract(n.meta,'$.k'), n.path, n.line, n.end_line, json_extract(n.meta,'$.parent')";

fn map(r: &rusqlite::Row) -> rusqlite::Result<TSym> {
    Ok(TSym {
        id: r.get(0)?,
        name: r.get(1)?,
        kind: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        path: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
        line: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
        end_line: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
        parent: r.get(6)?,
    })
}

pub fn by_id(c: &Connection, id: i64) -> Option<TSym> {
    c.query_row(&format!("SELECT {COLS} FROM node n WHERE n.id = ?1 AND n.source = 'treesitter'"), [id], map).ok()
}

fn is_test(p: &str) -> bool {
    p.contains("/test/") || p.contains("/tests/") || p.contains("Test.") || p.contains("_test.") || p.contains("androidTest")
}

/// Resolve a name, `Type.name`, `name(...)` or `c:` handle, best first.
/// Tree-sitter symbols of project ?1 named ?2 (any case), with their inbound reference counts.
/// Without statistics SQLite prefers the (project, source, kind) probe index and walks every
/// symbol of the project; the name index is a direct seek.
fn resolve_sql() -> String {
    format!(
        "SELECT {COLS}, (SELECT COUNT(*) FROM ts_ref r WHERE r.dst = n.id AND r.dst != 0), json_extract(n.meta,'$.d')
         FROM node n INDEXED BY node_sym_name
         WHERE n.project_id = ?1 AND n.kind = 'symbol' AND n.source = 'treesitter' AND n.name = ?2 COLLATE NOCASE LIMIT 300"
    )
}

pub fn resolve(c: &Connection, pid: i64, q: &str) -> Result<Vec<TSym>> {
    let q = q.trim();
    if let Some(h) = q.strip_prefix("c:")
        && let Ok(id) = i64::from_str_radix(h, 16)
    {
        return Ok(by_id(c, id).into_iter().collect());
    }
    let q = q.split('(').next().unwrap_or(q);
    let (qual, name) = match q.rsplit_once('.') {
        Some((a, b)) if !b.is_empty() => (Some(a), b),
        _ => (None, q),
    };
    let mut s = c.prepare_cached(&resolve_sql())?;
    // (symbol, inbound references, prototype, qualifier matched as typed, qualifier score)
    let mut v: Vec<(TSym, i64, bool, bool, usize)> = s
        .query_map(params![pid, name], |r| Ok((map(r)?, r.get::<_, i64>(7)?, r.get::<_, Option<i64>>(8)?.is_some())))?
        .flatten()
        .filter_map(|(t, n, decl)| {
            let (exact, score) = match qual {
                None => (true, 0),
                Some(w) => qual_matches(&t, w)?,
            };
            Some((t, n, decl, exact, score))
        })
        .collect();
    // The name as typed (and its type as typed) beats a same-name symbol in another case
    // (`render` the method, not `Render` the class); a definition beats its C prototype. A shell
    // function (a test or build script's helper) yields to a same-name definition in the
    // project's code, however often the scripts call their own.
    v.sort_by_key(|(t, n, decl, exact, score)| {
        let rank = if is_callable_kind(&t.kind) || is_type_kind(&t.kind) { 0 } else { 1 };
        let script = crate::lang::for_path(&t.path).is_some_and(|l| l.family == "bash");
        (t.name != name || !exact, std::cmp::Reverse(*score), rank, script, *decl, is_test(&t.path) as u8, std::cmp::Reverse(*n), t.id)
    });
    Ok(v.into_iter().map(|x| x.0).collect())
}

/// How well `t` sits where `qual` (a target's text before its last `.`) points: None when it
/// does not, else (case as typed, score; higher is a closer match). Every segment must match.
/// A qualifier ends in the symbol's containers (`Type`, `Outer.Inner`), and what comes before
/// them names its package or module: a directory (`pod`, `pkg/api/v1/pod`), an import path that
/// ends in it (`k8s.io/kubernetes/pkg/api/v1/pod`; the longest match wins, and a one-segment
/// directory only weakly matches a longer path), or a dotted module (`pkg.fields.json`). A Go
/// file stem is never a package.
pub fn qual_matches(t: &TSym, qual: &str) -> Option<(bool, usize)> {
    qual_matches_at(&t.path, t.parent.as_deref(), qual)
}

/// [`qual_matches`] for a symbol at `path` inside `parent`.
pub fn qual_matches_at(path: &str, parent: Option<&str>, qual: &str) -> Option<(bool, usize)> {
    let qual = qual.trim_start_matches('.');
    // Readings of the qualifier as (location segments, container segments). An import path's
    // location ends at its last `/` plus one segment, which may itself hold dots
    // (`gopkg.in/yaml.v3`): each split of what follows the `/` is a reading.
    let mut readings: Vec<(Option<Vec<String>>, Vec<&str>)> = vec![];
    match qual.rsplit_once('/') {
        Some((head, tail)) => {
            let parts: Vec<&str> = tail.split('.').collect();
            for j in 1..=parts.len() {
                let mut loc: Vec<String> = head.split('/').filter(|x| !x.is_empty()).map(str::to_string).collect();
                loc.push(parts[..j].join("."));
                readings.push((Some(loc), parts[j..].to_vec()));
            }
        }
        None => readings.push((None, qual.split('.').filter(|x| !x.is_empty()).collect())),
    }
    let ps: Vec<&str> = parent.unwrap_or("").split('.').filter(|x| !x.is_empty()).collect();
    let go = crate::lang::for_path(path).is_some_and(|l| l.family == "go");
    let dir: Vec<&str> = crate::tsindex::dir_of(path).split('/').filter(|x| !x.is_empty()).collect();
    // The directory with its dotted segments split (`gopkg.in/yaml.v3` -> gopkg, in, yaml, v3),
    // for a dotted qualifier naming a dotted directory (`yaml.v3.Marshal`).
    let flat: Vec<&str> = dir.iter().flat_map(|d| d.split('.')).filter(|x| !x.is_empty()).collect();
    let module: Vec<&str> = path.rsplit_once('.').map_or(path, |x| x.0).split('/').filter(|x| !x.is_empty()).collect();
    let mut best: Option<(bool, usize)> = None;
    for (loc_fixed, conts) in &readings {
        // Score of `loc` as the symbol's location: segments matched, or None.
        let place = |loc: &[&str]| -> Option<usize> {
            if loc.is_empty() {
                return Some(0);
            }
            let suffix = |have: &[&str]| have.len() >= loc.len() && have[have.len() - loc.len()..] == *loc;
            if suffix(&dir) || !go && suffix(&module) || loc_fixed.is_none() && flat.len() > dir.len() && suffix(&flat) {
                return Some(loc.len());
            }
            // An import path longer than the directory: the directory ends it. A one-segment
            // directory says little about a longer path.
            if loc_fixed.is_some() && !dir.is_empty() && loc.len() > dir.len() && loc[loc.len() - dir.len()..] == *dir {
                return Some(if dir.len() == 1 { 0 } else { dir.len() });
            }
            None
        };
        let ks: Vec<usize> = match loc_fixed {
            Some(_) => vec![conts.len()],
            None => (0..=conts.len().min(ps.len())).rev().collect(),
        };
        for k in ks {
            if k > ps.len() {
                continue;
            }
            let (want, have) = (&conts[conts.len() - k..], &ps[ps.len() - k..]);
            if !want.iter().zip(have).all(|(a, b)| a.eq_ignore_ascii_case(b)) {
                continue;
            }
            let exact = want.iter().zip(have).all(|(a, b)| a == b);
            let loc: Vec<&str> = match loc_fixed {
                Some(l) => l.iter().map(String::as_str).collect(),
                None => conts[..conts.len() - k].to_vec(),
            };
            if let Some(sc) = place(&loc) {
                let cand = (exact, k * 2 + sc);
                if best.is_none_or(|b| cand > b) {
                    best = Some(cand);
                }
            }
        }
    }
    best
}

/// (sites, their files, their lines, guessed sites, weak guesses left out).
type Sites = (usize, HashSet<String>, HashSet<account::Line>, usize, usize);

/// Call or reference sites of `targets`, grouped by file: `path (n)` then `  :line Caller c:h`.
/// A site whose target was guessed by name among several same-name definitions ends in
/// `guess 1 of N`; one guessed among more than [`WEAK_GUESS`] is only counted. A site of a
/// target in `macros` (a C macro that stands for the first target) is labelled `via macro M`;
/// the call in M's own body is no site.
/// Returns (sites, their files, their lines, guessed sites, weak guesses left out).
fn sites(
    c: &Connection,
    root: &std::path::Path,
    name: &str,
    targets: &[i64],
    macros: &HashMap<i64, String>,
    calls_only: bool,
    lines: &mut Vec<String>,
) -> Result<Sites> {
    let ids = targets.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let kind = if calls_only { "AND r.kind = 'call'" } else { "" };
    let sql = format!(
        "SELECT r.path, r.line, r.src, s.name, json_extract(s.meta,'$.parent'), r.amb, r.recv, r.via, json_extract(d.meta,'$.parent'),
                d.path, r.project_id, r.dst
         FROM ts_ref r LEFT JOIN node s ON s.id = r.src LEFT JOIN node d ON d.id = r.dst
         WHERE r.dst != 0 AND r.dst IN ({ids}) {kind} ORDER BY r.path, r.line"
    );
    let mut q = c.prepare(&sql)?;
    // path -> [(caller display, [lines], most same-name candidates, the name the lines hold)],
    // callers in first-seen order.
    type Who = (String, Vec<i64>, i64, String);
    let mut by: BTreeMap<String, Vec<Who>> = BTreeMap::new();
    let mut seen = HashSet::new();
    let (mut n, mut guessed, mut weak) = (0, 0, 0);
    let mut at: HashSet<(String, i64)> = HashSet::new();
    let mut imports = FileImportNames::default();
    for (path, line, src, sname, sparent, amb, recv, via, dparent, dpath, pid, dst) in q
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, bool>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, i64>(10)?,
                r.get::<_, i64>(11)?,
            ))
        })?
        .flatten()
    {
        if !seen.insert((path.clone(), line, src)) || macros.contains_key(&src) {
            continue;
        }
        if is_weak(amb, recv, via.as_deref(), dparent.as_deref())
            && !imports.reach(c, pid, &path, dpath.as_deref().unwrap_or(""), dparent.as_deref())?
        {
            weak += 1;
            continue;
        }
        at.insert((path.clone(), line));
        let mut who = match (sname, sparent) {
            (Some(nm), Some(p)) if !p.is_empty() => format!("{p}.{nm} c:{src:x}"),
            (Some(nm), _) => format!("{nm} c:{src:x}"),
            _ => "(top)".into(),
        };
        let named = match macros.get(&dst) {
            Some(m) => {
                who = format!("{who} via macro {m}");
                m.clone()
            }
            None => name.to_string(),
        };
        let e = by.entry(path).or_default();
        match e.iter_mut().find(|(w, ..)| *w == who) {
            Some((_, ls, a, _)) => {
                ls.push(line);
                *a = (*a).max(amb);
            }
            None => e.push((who, vec![line], amb, named)),
        }
        n += 1;
        if amb > 0 {
            guessed += 1;
        }
    }
    let files: HashSet<String> = by.keys().cloned().collect();
    let mut src = SrcLines::new(root, n <= CODE_SITES_MAX);
    group_by_dir(
        by.into_iter().map(|(path, v)| {
            let count: usize = v.iter().map(|x| x.1.len()).sum();
            let mut rows = vec![];
            for (who, ls, amb, named) in v {
                let head = format!("  :{} {who}{}", ls.iter().map(i64::to_string).collect::<Vec<_>>().join(","), guess_note(amb));
                match ls.as_slice() {
                    [one] => rows.push(src.with_code(head, &path, *one, &named)),
                    _ => {
                        rows.push(head);
                        rows.extend(ls.iter().filter_map(|l| src.line(&path, *l, &named).map(|code| format!("   :{l} | {code}"))));
                    }
                }
            }
            (path, count, rows)
        }),
        lines,
    );
    Ok((n, files, at, guessed, weak))
}

/// Bytes kept for the accounting line that leads `callers`, `refs` and `impact`.
const HEAD_RESERVE: usize = 300;

/// Most sites or dependents whose code an answer carries; past this, rows are locations only.
const CODE_SITES_MAX: usize = 400;
/// Longest code shown on a row.
const CODE_CHARS: usize = 140;

/// Source lines of the files an answer cites, each file read once.
struct SrcLines<'a> {
    root: &'a std::path::Path,
    on: bool,
    files: HashMap<String, Option<Vec<String>>>,
}

impl<'a> SrcLines<'a> {
    fn new(root: &'a std::path::Path, on: bool) -> Self {
        SrcLines { root, on, files: HashMap::new() }
    }

    /// Line `n` of `path`, trimmed and clipped around `name`; None when off or unreadable, or
    /// when the line no longer names `name` (the file changed since it was indexed).
    fn line(&mut self, path: &str, n: i64, name: &str) -> Option<String> {
        if !self.on || n < 1 {
            return None;
        }
        let l = self.text(path)?.get(n as usize - 1)?.trim();
        clip_around(l, name)
    }

    /// `path`'s lines, read once.
    fn text(&mut self, path: &str) -> Option<&Vec<String>> {
        let root = self.root;
        self.files
            .entry(path.to_string())
            .or_insert_with(|| {
                crate::guard::read_text(&root.join(path), path, 4 << 20, false).map(|t| t.lines().map(str::to_string).collect())
            })
            .as_ref()
    }

    /// `row | code` when line `n` of `path` names `name`, else `row`.
    fn with_code(&mut self, row: String, path: &str, n: i64, name: &str) -> String {
        match self.line(path, n, name) {
            Some(code) => format!("{row} | {code}"),
            None => row,
        }
    }
}

/// `line` when it names `name`, clipped to [`CODE_CHARS`] around the first mention; else None.
pub(crate) fn clip_around(line: &str, name: &str) -> Option<String> {
    let at = word_at(line, name)?;
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= CODE_CHARS {
        return Some(line.to_string());
    }
    let pos = line[..at].chars().count();
    let start = pos.saturating_sub(40).min(chars.len().saturating_sub(CODE_CHARS - 1));
    let end = (start + CODE_CHARS - 2).min(chars.len());
    let mut out: String = if start > 0 { "…".into() } else { String::new() };
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push('…');
    }
    Some(out)
}

/// Byte offset of `name` in `line` as a whole word: a call (`name(`) first, else the first.
pub(crate) fn word_at(line: &str, name: &str) -> Option<usize> {
    let w = |ch: Option<char>| ch.is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
    let mut from = 0;
    let mut first = None;
    while let Some(i) = line[from..].find(name).map(|i| i + from) {
        let after = &line[i + name.len()..];
        if !w(line[..i].chars().next_back()) && !w(after.chars().next()) {
            if after.trim_start().starts_with('(') {
                return Some(i);
            }
            first = first.or(Some(i));
        }
        from = i + name.len();
    }
    first
}

/// Most files listed by path:line when an answer names files it could not resolve.
const MENTION_FILES: usize = 15;

/// `  path:line | code` for the lines (two per file at most) of `files` naming `name`.
fn mention_rows(root: &std::path::Path, files: &[String], name: &str) -> Vec<String> {
    let mut src = SrcLines::new(root, true);
    let mut out = vec![];
    for f in files {
        let Some(text) = src.text(f) else { continue };
        let hits: Vec<(usize, String)> =
            text.iter().enumerate().filter_map(|(i, l)| clip_around(l.trim(), name).map(|c| (i + 1, c))).take(2).collect();
        out.extend(hits.into_iter().map(|(n, code)| format!("  {f}:{n} | {code}")));
    }
    out
}

/// Code rows (`   :line | code`) and inline code (`  :line Caller c:h | code`) ride along only
/// when the whole answer with them fits the budget. Otherwise the rows are locations only, as
/// before, and a last line names the budget that shows the code, when there is one (at most
/// 4000 tokens); a larger budget never shows less.
fn fit_code(lines: &mut Vec<String>, budget: usize) {
    // The accounting line goes in front afterwards: room for it is kept, the same on every page.
    let room = (budget.clamp(32, 4000) * 4).saturating_sub(64 + HEAD_RESERVE);
    let size = |v: &[String]| v.iter().map(|l| l.len() + 1).sum::<usize>();
    let full = size(lines);
    if full <= room {
        return;
    }
    let mut plain: Vec<String> = lines.iter().filter(|l| !(l.starts_with("   :") && l.contains(" | "))).cloned().collect();
    for l in plain.iter_mut().filter(|l| l.starts_with("  :")) {
        if let Some((a, _)) = l.split_once(" | ") {
            *l = a.to_string();
        }
    }
    if size(&plain) == full {
        return;
    }
    // The smallest budget whose room holds the answer with code.
    let need = (full + 64 + HEAD_RESERVE).div_ceil(4);
    // Near the top, so the first page carries it.
    plain.insert(
        plain.len().min(2),
        if need <= 4000 {
            format!("code left out to fit the budget; budget={need} shows it")
        } else {
            "code left out: the answer is too long to carry it; fox_read target=<handle> shows one site's code".to_string()
        },
    );
    *lines = plain;
}

/// A guess among more same-name definitions than this says nothing about which one is meant
/// (`response.close()` with dozens of `close` methods): counted, not listed.
const WEAK_GUESS: i64 = 8;

/// Each calling file's imports, read once per file: a guess whose target's type or module the
/// caller imports by name (`from scrapy.extensions.memdebug import MemoryDebugger`, then
/// `ext.spider_closed()`) is no weak guess. A shared module stem (`conn`) says nothing.
#[derive(Default)]
struct FileImportNames(HashMap<String, (HashSet<String>, Vec<String>)>);

impl FileImportNames {
    fn reach(&mut self, c: &Connection, pid: i64, from: &str, target_path: &str, target_parent: Option<&str>) -> Result<bool> {
        if !self.0.contains_key(from) {
            let mut q = c.prepare_cached("SELECT name, via FROM ts_ref WHERE project_id = ?1 AND path = ?2 AND kind = 'import'")?;
            let (mut names, mut modules) = (HashSet::new(), vec![]);
            for (name, via) in q.query_map(params![pid, from], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?.flatten()
            {
                for n in std::iter::once(name).chain(via) {
                    let n = n.trim_matches('"');
                    names.extend(n.rsplit(['.', '/', ':']).next().map(str::to_string));
                    // A module of two or more parts (`scrapy.extensions.memdebug`, `pkg/b/stats`).
                    let m = n.trim_start_matches('.').replace('.', "/");
                    if m.contains('/') {
                        modules.push(m);
                    }
                }
            }
            self.0.insert(from.to_string(), (names, modules));
        }
        let (names, modules) = &self.0[from];
        let ty = target_parent.map(|p| p.rsplit('.').next().unwrap_or(p)).unwrap_or("");
        let stem = target_path.rsplit_once('.').map_or(target_path, |x| x.0);
        let ends = |a: &str, b: &str| a == b || a.ends_with(b) && a[..a.len() - b.len()].ends_with('/');
        Ok(!ty.is_empty() && names.contains(ty) || modules.iter().any(|m| ends(stem, m)))
    }
}

/// A guess among more than [`WEAK_GUESS`] candidates with no receiver named like the target's
/// type (`router.match()` for `Router.match` is informative; `response.close()` is not).
fn is_weak(amb: i64, recv: bool, via: Option<&str>, target_parent: Option<&str>) -> bool {
    if amb <= WEAK_GUESS {
        return false;
    }
    let ty = target_parent.map(|p| p.rsplit('.').next().unwrap_or(p)).unwrap_or("");
    !(recv && via.is_some_and(|v| crate::tsindex::SELF_NAMES.contains(&v) || !ty.is_empty() && crate::tsindex::via_match(v, ty) > 0))
}

/// The warning for weak guesses left out: they may be real.
fn weak_left_out(n: usize) -> String {
    let (calls, were) = if n == 1 { ("1 call", "was") } else { (&*format!("{n} calls"), "were") };
    format!("{calls} guessed among many same-name definitions {were} left out; confirm the ones that matter")
}

/// `, N weak guesses left out` for a header, or nothing.
fn weak_note(weak: usize) -> String {
    match weak {
        0 => String::new(),
        1 => ", 1 weak guess left out".into(),
        n => format!(", {n} weak guesses left out"),
    }
}

/// One `may be incomplete` line when precise (SCIP) references in files `listed` leaves out go to
/// a method `top` overrides or that overrides it: a call resolved there may still run `top`, and
/// the completeness check counts such files as settled. Only stores with an import have them.
fn dispatched_elsewhere(c: &Connection, pid: i64, top: &TSym, listed: &HashSet<String>) -> Result<Option<String>> {
    if !crate::scip::active(c, pid) {
        return Ok(None);
    }
    let mut q = c.prepare_cached(
        "SELECT DISTINCT path, dst FROM ts_ref WHERE project_id = ?1 AND name = ?2 AND amb < 0 AND dst != ?3 AND dst != 0 AND kind = 'call'",
    )?;
    let rows: Vec<(String, i64)> = q
        .query_map(params![pid, top.name, top.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .flatten()
        .filter(|(path, _)| !listed.contains(path))
        .collect();
    if rows.is_empty() {
        return Ok(None);
    }
    let d = dispatch(c, pid, top)?;
    let related: HashMap<i64, &TSym> = d.up.iter().chain(&d.down).map(|s| (s.id, s)).collect();
    let mut files: BTreeSet<&str> = BTreeSet::new();
    let mut via: BTreeSet<String> = BTreeSet::new();
    for (path, dst) in &rows {
        if let Some(s) = related.get(dst) {
            files.insert(path);
            via.insert(s.qname());
        }
    }
    if files.is_empty() {
        return Ok(None);
    }
    let n = files.len();
    let shown: Vec<&str> = files.iter().take(5).copied().collect();
    let more = if n > shown.len() { format!(", +{} more", n - shown.len()) } else { String::new() };
    Ok(Some(format!(
        "may be incomplete: calls in {n} file{} resolve to {}, which may dispatch here: {}{more}",
        if n == 1 { "" } else { "s" },
        via.into_iter().collect::<Vec<_>>().join(", "),
        shown.join(", ")
    )))
}

/// The head line and, for at most [`MENTION_FILES`] files, the `path:line | code` rows of files
/// naming `name` that an answer does not resolve.
fn mentions(p: &Project, name: &str, files: &[String], lead: &str, why: &str) -> (Option<String>, Vec<String>) {
    let n = files.len();
    if n == 0 {
        return (None, vec![]);
    }
    let s = if n == 1 { "" } else { "s" };
    if n > MENTION_FILES {
        return (Some(format!("{lead}: `{name}` is also named in {n} other file{s}, {why}")), vec![]);
    }
    let mut rows = mention_rows(&p.root, files, name);
    if !rows.is_empty() {
        rows.insert(0, "unresolved mentions:".into());
    }
    (Some(format!("{lead}: `{name}` is also named in {n} other file{s}, {why}; unresolved mentions at the end")), rows)
}

/// Files naming `top` that an answer listing `listed` leaves out, sorted.
fn unlisted_files(c: &Connection, pid: i64, top: &TSym, listed: &HashSet<String>, calls_only: bool) -> Result<Vec<String>> {
    let name = top.name.as_str();
    let mut skip: HashSet<String> = listed.clone();
    skip.insert(top.path.clone());
    // Files with an occurrence that may be ours, and files whose references all go elsewhere.
    let (mut open, mut closed): (HashSet<String>, HashSet<String>) = Default::default();
    let mut q = c.prepare_cached("SELECT path, dst, amb, kind FROM ts_ref WHERE project_id = ?1 AND name = ?2 AND kind != 'import'")?;
    for (path, dst, amb, kind) in q
        .query_map(params![pid, name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?)))?
        .flatten()
    {
        if dst == top.id && (!calls_only || kind == "call") {
            skip.insert(path);
        } else if dst != 0 && dst != top.id && amb <= 0 {
            closed.insert(path);
        } else {
            open.insert(path);
        }
    }
    // Files holding the name as a whole word anywhere (file-scope initializers, macro bodies,
    // comments), except those that define a same-name symbol of their own or whose references
    // of it all go elsewhere. A store still owing its first word pass falls back to symbol text.
    if name.chars().all(|ch| ch.is_alphanumeric() || ch == '_') {
        let mut d = c.prepare_cached(
            "SELECT path, name FROM node INDEXED BY node_sym_name WHERE project_id = ?1 AND kind = 'symbol' AND name = ?2 COLLATE NOCASE AND source = 'treesitter'
               AND COALESCE(json_extract(meta,'$.k'), '') != 'field'",
        )?;
        // The words are matched case as typed, so only a definition of that exact name counts.
        let defs: HashSet<String> = d
            .query_map(params![pid, name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .flatten()
            .filter(|(_, n)| n == name)
            .map(|(p, _)| p)
            .collect();
        let paths: Vec<String> = if crate::db::reindex_pending(c, pid) {
            body_paths(c, pid, name)?
        } else {
            let mut f = c.prepare_cached("SELECT path FROM fts_word WHERE fts_word MATCH ?1 AND project_id = ?2 LIMIT 50000")?;
            f.query_map(params![format!("words:\"{}\"", crate::tsindex::case_word(name)), pid], |r| r.get::<_, String>(0))?
                .flatten()
                .collect()
        };
        for path in paths {
            if !closed.contains(&path) && !defs.contains(&path) {
                open.insert(path);
            }
        }
    }
    // Code files only: documentation naming it is no use of it.
    let mut v: Vec<String> = open.into_iter().filter(|p| !skip.contains(p) && crate::lang::for_path(p).is_some()).collect();
    v.sort();
    Ok(v)
}

/// Files whose indexed symbol text holds `name` as a whole word, for a name of several parts
/// (a single word is also a part of longer names in that index).
pub(crate) fn body_paths(c: &Connection, pid: i64, name: &str) -> Result<Vec<String>> {
    let word: String = name.chars().filter(|ch| *ch != '_').collect::<String>().to_lowercase();
    if crate::search::ident_parts(name).len() < 2 {
        return Ok(vec![]);
    }
    let mut f = c.prepare_cached(
        "SELECT DISTINCT n.path FROM fts_body f JOIN node n ON n.id = f.rowid
         WHERE fts_body MATCH ?1 AND n.project_id = ?2 AND n.kind = 'symbol' AND n.source = 'treesitter' LIMIT 20000",
    )?;
    Ok(f.query_map(params![format!("body:\"{word}\""), pid], |r| r.get::<_, String>(0))?.flatten().collect())
}

/// C: the function-like macros whose body is one call of `top` (`#define abbrev_branch(n)
/// abbrev_ref(n, "refs/heads/")`), by id: a call of one runs `top`.
fn alias_macros(c: &Connection, top: &TSym) -> Result<HashMap<i64, String>> {
    if crate::lang::for_path(&top.path).is_none_or(|l| l.family != "c") {
        return Ok(HashMap::new());
    }
    let mut q = c.prepare_cached(
        "SELECT m.id, m.name FROM ts_ref r JOIN node m ON m.id = r.src
         WHERE r.dst = ?1 AND r.dst != 0 AND r.kind = 'call' AND r.amb = 0 AND json_extract(m.meta,'$.k') = 'macro' AND m.lang = 'c'",
    )?;
    Ok(q.query_map([top.id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?.flatten().filter(|(id, _)| *id != top.id).collect())
}

/// ` guess 1 of N` after a site resolved by name among N same-name definitions; ` guess by
/// name` when the only candidate was taken on its name alone (a call through a receiver).
pub(crate) fn guess_note(amb: i64) -> String {
    match amb {
        a if a > 1 => format!(" guess 1 of {a}"),
        1 => " guess by name".into(),
        _ => String::new(),
    }
}

/// Emit `dir/` headers, then ` file (n)` and its rows, so no directory or path is repeated.
pub fn group_by_dir(files: impl Iterator<Item = (String, usize, Vec<String>)>, lines: &mut Vec<String>) {
    let mut cur_dir: Option<String> = None;
    for (path, n, rows) in files {
        let (dir, file) = match path.rsplit_once('/') {
            Some((d, f)) => (format!("{d}/"), f.to_string()),
            None => (String::new(), path.clone()),
        };
        if cur_dir.as_deref() != Some(dir.as_str()) {
            if !dir.is_empty() {
                lines.push(dir.clone());
            }
            cur_dir = Some(dir.clone());
        }
        lines.push(format!("{}{file} ({n})", if dir.is_empty() { "" } else { " " }));
        lines.extend(rows);
    }
}

/// Entry point: `fox code` when the symbol lives in the tree-sitter index.
pub fn run(c: &Connection, p: &Project, sym: &str, rel: &str, depth: usize, cursor: usize, budget: usize) -> Result<Option<String>> {
    let sym = sym.trim();
    if rel == "outline" && (sym.contains('/') || crate::lang::for_path(sym).is_some()) && !sym.starts_with("c:") {
        return outline_file(c, p, sym.trim_start_matches("./"), cursor, budget);
    }
    let cands = resolve(c, p.id, sym)?;
    let Some(top) = cands.first() else {
        let bare = sym.split('(').next().unwrap_or(sym);
        if sym.starts_with("c:") || bare.contains(['.', ':', '/']) {
            return Ok(None);
        }
        return name_only(c, p, bare, rel, cursor, budget);
    };
    let mut lines: Vec<String> = vec![];
    // A qualified target matched equally well by several definitions (vendored copies): say so.
    if let Some((qual, _)) = sym.split('(').next().unwrap_or(sym).rsplit_once('.')
        && cands.len() > 1
    {
        let q0 = qual_matches(top, qual);
        let tied = cands.iter().filter(|s| s.name == top.name && qual_matches(s, qual) == q0).count();
        if tied > 1 {
            lines.push(format!(
                "{tied} equally good matches for {sym:?}; using {} at {}, the lowest id (name a longer path to pick another)",
                top.h(),
                top.path
            ));
        }
    }
    if cands.len() > 1 {
        let others: Vec<String> = cands.iter().skip(1).take(4).map(|s| format!("{} {}", s.h(), s.qname())).collect();
        lines.push(format!("{} matches; others: {}{}", cands.len(), others.join(", "), if cands.len() > 5 { ", ..." } else { "" }));
    }
    // Rows that go last: files naming the target that the answer could not resolve.
    let mut tail: Vec<String> = vec![];
    // The accounting of every line naming it (`callers`, `refs`, `impact`).
    let mut ledger: Option<(HashSet<account::Line>, Vec<i64>, account::Mode, usize)> = None;
    match rel {
        "callers" | "refs" | "impact" if top.kind == crate::tsindex::FIELD => {
            // A field is no reference target: its reads and writes are not resolved, so the
            // answer is the files that name it, never a confident 0.
            lines.push(format!("{rel} of {}", top.row()));
            // Its own file first (the definition line left out), then the other files naming it.
            let mut src = SrcLines::new(&p.root, true);
            let own: Vec<String> = src
                .text(&top.path)
                .map(|t| {
                    t.iter()
                        .enumerate()
                        .filter(|(i, _)| *i as i64 + 1 != top.line)
                        .filter_map(|(i, l)| clip_around(l.trim(), &top.name).map(|code| format!("  {}:{} | {code}", top.path, i + 1)))
                        .take(5)
                        .collect()
                })
                .unwrap_or_default();
            let files = unlisted_files(c, p.id, top, &HashSet::new(), false)?;
            let lead = "field uses are not tracked (reads and writes are not resolved)";
            let (head, rows) = mentions(p, &top.name, &files, lead, "by name");
            match head {
                Some(h) => lines.push(h),
                None if !own.is_empty() => lines.push(format!("{lead}; lines in its own file that name it at the end")),
                None => lines.push(format!(
                    "{lead}; the index finds no line naming `{}` besides its definition, which does not mean it is unused (it may be reached by another name); Grep can confirm",
                    top.name
                )),
            }
            if !own.is_empty() {
                tail.push("mentions in its own file:".into());
                tail.extend(own);
            }
            tail.extend(rows);
        }
        "callers" | "refs" => {
            let calls = rel == "callers" && is_callable_kind(&top.kind);
            lines.push(format!("{rel} of {}", top.row()));
            let at = lines.len();
            // C: calls through a macro whose body is one call of the target count as its calls.
            let macros = if calls { alias_macros(c, top)? } else { HashMap::new() };
            let targets: Vec<i64> = std::iter::once(top.id).chain(macros.keys().copied()).collect();
            let (n, files, at_lines, guessed, weak) = sites(c, &p.root, &top.name, &targets, &macros, calls, &mut lines)?;
            let g = if guessed > 0 { format!(", {guessed} guessed by name") } else { String::new() } + &weak_note(weak);
            lines.insert(at, format!("{n} {} in {} files{g}", if calls { "call sites" } else { "refs" }, files.len()));
            if weak > 0 {
                lines.insert(at + 1, format!("may be incomplete: {}", weak_left_out(weak)));
            } else if let Some(l) = dispatched_elsewhere(c, p.id, top, &files)? {
                lines.insert(at + 1, l);
            }
            let mode = if calls { account::Mode::Calls } else { account::Mode::Refs };
            ledger = Some((at_lines, vec![], mode, guessed));
        }
        "callees" => {
            let mut q = c.prepare_cached(&format!(
                "SELECT {COLS}, group_concat(r.line), MIN(r.amb) FROM ts_ref r JOIN node n ON n.id = r.dst
                 WHERE r.src = ?1 AND r.src != 0 AND r.kind = 'call' GROUP BY n.id ORDER BY n.path, n.line"
            ))?;
            let v: Vec<(TSym, String, i64)> =
                q.query_map([top.id], |r| Ok((map(r)?, r.get::<_, String>(7)?, r.get::<_, i64>(8)?)))?.flatten().collect();
            // A target counts as a guess only when every call to it was resolved by name alone.
            let guessed = v.iter().filter(|x| x.2 > 0).count();
            lines.push(format!("callees of {}", top.row()));
            let g = if guessed > 0 { format!(", {guessed} guessed by name") } else { String::new() };
            lines.push(format!("{} callees (project symbols){g}", v.len()));
            let mut by: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (s, at, amb) in v {
                by.entry(s.path.clone()).or_default().push(format!(
                    "  :{} {} {} {} @{at}{}",
                    s.line,
                    s.kind,
                    s.qname(),
                    s.h(),
                    guess_note(amb)
                ));
            }
            group_by_dir(by.into_iter().map(|(p, rows)| (p, rows.len(), rows)), &mut lines);
        }
        "outline" => {
            let target = if is_type_kind(&top.kind) {
                top.clone()
            } else {
                // A member: outline its type.
                c.query_row(
                    &format!("SELECT {COLS} FROM node n WHERE n.id = (SELECT json_extract(meta,'$.pid') FROM node WHERE id = ?1)"),
                    [top.id],
                    map,
                )
                .unwrap_or_else(|_| top.clone())
            };
            let mut q = c.prepare_cached(&format!(
                "SELECT {COLS} FROM node n WHERE n.kind = 'symbol' AND json_extract(n.meta, '$.pid') = ?1 ORDER BY n.line"
            ))?;
            let v: Vec<TSym> = q.query_map([target.id], map)?.flatten().collect();
            lines.push(format!("outline {}  ({} members)", target.row(), v.len()));
            lines.extend(v.iter().map(|m| format!(" {} {} {} {}", m.line, m.kind, m.name, m.h())));
        }
        "src" => {
            let abs = p.root.join(&top.path);
            let Some(text) = crate::guard::read_text(&abs, &top.path, 4 << 20, false) else { bail!("cannot read {}", top.path) };
            lines.push(format!("~{} {}:{}-{}", top.h(), top.path, top.line, top.end_line));
            let first = (top.line as usize).saturating_sub(1);
            for (i, l) in text.lines().enumerate().take(top.end_line.max(top.line) as usize).skip(first) {
                lines.push(format!("{}\t{}", i + 1, clip(l, 220)));
            }
        }
        "impact" => {
            let (d1, implementations) = impact(c, p, top, depth, &mut lines)?;
            ledger = Some((d1, implementations, account::Mode::Impact, 0));
        }
        "overrides" if is_callable_kind(&top.kind) => method_overrides(c, p.id, top, &mut lines)?,
        "conforms" | "subtypes" | "extensions" | "overrides" => {
            // Supertype references (`reference.implementation` / `reference.class`) resolved to this type.
            lines.push(format!("{rel} of {}", top.row()));
            let mut q = c.prepare_cached(&format!(
                "SELECT DISTINCT {COLS} FROM ts_ref r JOIN node n ON n.id = r.src WHERE r.dst = ?1 AND r.dst != 0 AND r.kind IN ('ref', 'impl') AND json_extract(n.meta,'$.k') IN ('class','interface','object','struct','enum','trait') ORDER BY n.path, n.line"
            ))?;
            let v: Vec<TSym> = q.query_map([top.id], map)?.flatten().collect();
            lines.push(format!("{} types reference it", v.len()));
            lines.extend(v.iter().map(TSym::row));
        }
        _ => bail!("unsupported --rel {rel}"),
    }
    lines.extend(tail);
    lines.push("~ tree-sitter, name-resolved".into());
    if matches!(rel, "callers" | "refs" | "impact") {
        fit_code(&mut lines, budget);
    }
    if let Some((at_lines, linked, mode, guessed)) = ledger {
        let l = account::account(c, p, &top.name, Some(top), &at_lines, &linked, mode)?;
        lines.insert(0, l.head(guessed).unwrap_or_else(|| format!("no line names `{}` in the indexed code", top.name)));
        ledger_rows(&mut lines, &l);
    }
    Ok(Some(page(&lines, cursor, budget)))
}

/// The ledger's few listed rows (imports, declarations, some unresolved lines), before the
/// answer's last line.
fn ledger_rows(lines: &mut Vec<String>, l: &account::Ledger) {
    let rows = l.tail();
    let at = lines.len().saturating_sub(1);
    lines.splice(at..at, rows);
}

/// No definition of `name` is indexed: when grep would find it, the lines naming it, by group.
fn name_only(c: &Connection, p: &Project, name: &str, rel: &str, cursor: usize, budget: usize) -> Result<Option<String>> {
    if !matches!(rel, "callers" | "refs" | "impact") {
        return Ok(None);
    }
    let l = account::account(c, p, name, None, &HashSet::new(), &[], account::Mode::Name)?;
    let Some(head) = l.head(0) else { return Ok(None) };
    let mut lines = vec![head, format!("{rel} of `{name}`: no indexed definition"), "~ tree-sitter, by name".to_string()];
    ledger_rows(&mut lines, &l);
    Ok(Some(page(&lines, cursor, budget)))
}

/// Deepest chain of named supertypes `overrides` follows (Field -> DateField -> DateTimeField ...).
const OVERRIDE_STEPS: usize = 8;

/// `overrides` of a method: the methods it overrides (same name, in a supertype) and its
/// overrides (same name, in a subtype at any depth). Go has no inheritance: the interface
/// methods a method implements, or an interface method's implementations, by method set.
fn method_overrides(c: &Connection, pid: i64, top: &TSym, lines: &mut Vec<String>) -> Result<()> {
    lines.push(format!("overrides of {}", top.row()));
    let (mut up, mut down, mut capped) = (vec![], vec![], None);
    if crate::lang::for_path(&top.path).is_some_and(|l| l.family == "go" || l.name == "c") {
        let d = dispatch(c, pid, top)?;
        (up, down, capped) = (d.up, d.down, d.capped);
    } else if top.parent.as_deref().is_some_and(|p| !p.is_empty()) {
        let mut owners = Owners::default();
        if let Some(owner) = owner_type(c, pid, top, &mut owners)? {
            let ups = related_types_n(c, owner.id, true, OVERRIDE_STEPS)?;
            let downs = related_types_n(c, owner.id, false, OVERRIDE_STEPS)?;
            let mut q = c.prepare_cached(&format!(
                "SELECT {COLS}, {SYM_PID} FROM node n INDEXED BY node_sym_name
                 WHERE n.project_id = ?1 AND n.kind = 'symbol' AND n.source = 'treesitter' AND n.name = ?2 COLLATE NOCASE
                   AND COALESCE(json_extract(n.meta,'$.k'), '') != 'field'
                 ORDER BY n.path, n.line LIMIT {DISPATCH_SAME_MAX}"
            ))?;
            let fam = |p: &str| crate::lang::for_path(p).map(|l| l.family).unwrap_or("");
            let same: Vec<(TSym, i64)> = q.query_map(params![pid, top.name], |r| Ok((map(r)?, r.get::<_, i64>(7)?)))?.flatten().collect();
            if same.len() >= DISPATCH_SAME_MAX {
                capped = Some(format!("partial: checked the first {DISPATCH_SAME_MAX} same-name methods; the rest may override it too"));
            }
            for (s, spid) in same {
                if s.name != top.name || s.id == top.id || fam(&s.path) != fam(&top.path) || !is_callable_kind(&s.kind) {
                    continue;
                }
                let t = if spid != 0 { Some(spid) } else { owner_type(c, pid, &s, &mut owners)?.map(|t| t.id) };
                match t {
                    Some(t) if ups.contains(&t) => up.push(s),
                    Some(t) if downs.contains(&t) => down.push(s),
                    _ => {}
                }
            }
        }
    }
    lines.extend(up.iter().map(|m| format!("overrides {}", m.row())));
    lines.push(format!("{} override{}", down.len(), if down.len() == 1 { "" } else { "s" }));
    lines.extend(capped);
    down.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    lines.extend(down.iter().map(TSym::row));
    Ok(())
}

/// Most dependents the impact walk collects before it stops.
const IMPACT_CAP: usize = 5000;
/// Deepest level the impact walk goes to, whatever depth is asked for.
const IMPACT_MAX_DEPTH: usize = 4;

/// Per dependent: None when some chain of certain edges reaches it, else the most same-name
/// candidates among the guessed edges (0: only through a guessed dependent).
type Guess = Option<i64>;

fn merge_guess(a: Guess, b: Guess) -> Guess {
    match (a, b) {
        (None, _) | (_, None) => None,
        (Some(a), Some(b)) => Some(a.max(b)),
    }
}

fn guess_mark(g: Guess) -> String {
    match g {
        Some(0) => " guess (via a guessed caller)".to_string(),
        Some(n) => guess_note(n),
        None => String::new(),
    }
}

/// One level of the impact walk: dependent symbols, and files using the level above at their
/// top level (lines, guess).
#[derive(Default)]
struct Level {
    syms: Vec<i64>,
    tops: BTreeMap<String, (Vec<i64>, Guess)>,
}

/// How a first-level dependent reached `top` through dispatch: a call to the interface or base
/// method (` via I.m`), or an implementation (` implements I.m`). None: directly.
type Disp = Option<String>;

fn merge_disp(a: &mut Disp, b: Disp) {
    if b.is_none() {
        *a = None;
    }
}

/// `impact`: what depends on `top`, level by level. A use outside any definition (file top level)
/// makes its file a dependent; it has no symbol to expand further. Lines starting `partial` say
/// where the answer may be missing something: names used in files it does not list, dependents
/// found only through name guesses, or a walk that stopped early.
/// Returns the lines its d1 rows stand for and the implementations it lists at d1.
fn impact(c: &Connection, p: &Project, top: &TSym, depth: usize, lines: &mut Vec<String>) -> Result<(HashSet<account::Line>, Vec<i64>)> {
    let (pid, root) = (p.id, p.root.as_path());
    let mut guess: HashMap<i64, Guess> = HashMap::from([(top.id, None)]);
    let mut top_seen: HashSet<String> = HashSet::new();
    let mut frontier = vec![top.id];
    let mut levels: Vec<Level> = vec![];
    // Dispatch: calls to the methods `top` implements count as calls to it; implementations of
    // `top` are first-level dependents.
    let t0 = std::time::Instant::now();
    let dsp = dispatch(c, pid, top)?;
    crate::tsindex::trace("impact: dispatch", t0);
    let t_walk = std::time::Instant::now();
    let up: HashMap<i64, &TSym> = dsp.up.iter().map(|m| (m.id, m)).collect();
    let dynamic = matches!(crate::lang::for_path(&top.path).map(|l| l.family), Some("python" | "js"));
    // Dependents reached only through a guessed call to an interface or base method: listed,
    // not followed (a guess through dispatch must not multiply).
    let mut leaf: HashMap<i64, bool> = HashMap::new();
    // Dependents (or top-level files) whose only link so far is a weak guess.
    let mut weak: HashSet<(i64, String, usize)> = HashSet::new();
    let mut parents: HashMap<i64, (String, Option<String>)> = HashMap::new();
    let mut imports = FileImportNames::default();
    let mut disp: HashMap<i64, Disp> = HashMap::new();
    let mut disp_top: HashMap<String, Disp> = HashMap::new();
    // A first-level dependent's first line using `top`, shown as its row's code.
    let mut use_at: HashMap<i64, i64> = HashMap::new();
    // A link known only by a guess makes what it reaches a guess too.
    let link = |id: i64| if dsp.guessed.contains(&id) { Some(1) } else { None };
    for m in &dsp.up {
        guess.insert(m.id, link(m.id));
        frontier.push(m.id);
    }
    let mut first = Level::default();
    for m in &dsp.down {
        if guess.insert(m.id, link(m.id)).is_none() {
            first.syms.push(m.id);
            disp.insert(m.id, Some(format!(" implements {}", top.qname())));
        }
    }
    let mut d1_paths: HashSet<String> = dsp.down.iter().map(|m| m.path.clone()).collect();
    // The lines the d1 rows stand for.
    let mut d1_lines: HashSet<(String, i64)> = HashSet::new();
    let max = depth.clamp(1, IMPACT_MAX_DEPTH);
    let mut cut = None;
    for d in 0..max {
        if frontier.is_empty() {
            break;
        }
        if guess.len() > IMPACT_CAP {
            cut = Some(format!("partial at d{}: stopped after {IMPACT_CAP} dependents", d + 1));
            break;
        }
        let ids = frontier.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let mut q = c.prepare(&format!(
            "SELECT src, dst, amb, path, line, kind, recv, via FROM ts_ref WHERE dst != 0 AND dst IN ({ids}) AND (src != 0 OR kind != 'import') ORDER BY path, line"
        ))?;
        let mut lvl = if d == 0 { std::mem::take(&mut first) } else { Level::default() };
        let mut found: HashMap<i64, Guess> = HashMap::new();
        let mut tops: BTreeMap<String, (Vec<i64>, Guess)> = BTreeMap::new();
        for (src, dst, amb, path, line, kind, recv, rvia) in q
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, bool>(6)?,
                    r.get::<_, Option<String>>(7)?,
                ))
            })?
            .flatten()
        {
            if amb > WEAK_GUESS && {
                let (dpath, parent) =
                    parents.entry(dst).or_insert_with(|| by_id(c, dst).map(|t| (t.path, t.parent)).unwrap_or_default()).clone();
                is_weak(amb, recv, rvia.as_deref(), parent.as_deref()) && !imports.reach(c, pid, &path, &dpath, parent.as_deref())?
            } {
                if src == 0 || !guess.contains_key(&src) {
                    weak.insert((src, path.clone(), d));
                }
                continue;
            }
            let base = if d == 0 { up.get(&dst).copied() } else { None };
            if let Some(m) = base {
                // Only a call to the interface or base method may dispatch to `top`; a guessed one
                // in Python or JS only through `self`/`this` or a receiver named like the base type.
                let named = |v: &str| {
                    let ty = m.parent.as_deref().map(|p| p.rsplit('.').next().unwrap_or(p)).unwrap_or("");
                    crate::tsindex::SELF_NAMES.contains(&v) || crate::tsindex::via_match(v, ty) > 0
                };
                if kind != "call" || amb > 0 && dynamic && !(recv && rvia.as_deref().is_some_and(named)) {
                    continue;
                }
            }
            let dl: Disp = base.map(|m| format!(" via {}", m.qname()));
            if d == 0 && src != 0 {
                let only = base.is_some() && amb > 0;
                leaf.entry(src).and_modify(|x| *x &= only).or_insert(only);
            }
            let via = if amb > 0 { Some(amb) } else { guess.get(&dst).copied().flatten().map(|_| 0) };
            if d == 0 {
                d1_paths.insert(path.clone());
                d1_lines.insert((path.clone(), line));
            }
            if src == 0 {
                if top_seen.contains(&path) {
                    continue;
                }
                if d == 0 {
                    match disp_top.get_mut(&path) {
                        Some(x) => merge_disp(x, dl),
                        None => {
                            disp_top.insert(path.clone(), dl);
                        }
                    }
                }
                let e = tops.entry(path).or_insert_with(|| (vec![], via));
                if e.0.last() != Some(&line) {
                    e.0.push(line);
                }
                e.1 = merge_guess(e.1, via);
                continue;
            }
            if guess.contains_key(&src) {
                continue;
            }
            if !found.contains_key(&src) && guess.len() + found.len() >= IMPACT_CAP {
                cut = Some(format!("partial at d{}: stopped after {IMPACT_CAP} dependents", d + 1));
                break;
            }
            if d == 0 {
                use_at.entry(src).or_insert(line);
                match disp.get_mut(&src) {
                    Some(x) => merge_disp(x, dl),
                    None => {
                        disp.insert(src, dl);
                    }
                }
            }
            let e = found.entry(src).or_insert_with(|| {
                lvl.syms.push(src);
                via
            });
            *e = merge_guess(*e, via);
        }
        top_seen.extend(tops.keys().cloned());
        lvl.tops = tops;
        guess.extend(found);
        frontier = lvl.syms.iter().copied().filter(|i| !leaf.get(i).copied().unwrap_or(false)).collect();
        levels.push(lvl);
        if cut.is_some() {
            break;
        }
    }
    if cut.is_none() && depth > max && !frontier.is_empty() && has_dependents(c, &frontier, &guess, &top_seen)? {
        cut = Some(format!("partial at d{max}: dependents continue past depth {max}, the deepest walked"));
    }
    let nsyms: usize = levels.iter().map(|l| l.syms.len()).sum();
    let nfiles: usize = levels.iter().map(|l| l.tops.len()).sum();
    let guessed = levels
        .iter()
        .map(|l| {
            l.syms.iter().filter(|i| guess.get(i).is_some_and(Option::is_some)).count() + l.tops.values().filter(|t| t.1.is_some()).count()
        })
        .sum::<usize>();
    let g = if guessed > 0 { format!(", {guessed} through name guesses") } else { String::new() };
    let f = if nfiles > 0 { format!(", {nfiles} files at top level") } else { String::new() };
    let weak: Vec<usize> = weak
        .iter()
        .filter(|(src, path, _)| if *src == 0 { !top_seen.contains(path) } else { !guess.contains_key(src) })
        .map(|x| x.2)
        .collect();
    let weak_n = weak.len();
    let g = g + &weak_note(weak_n);
    lines.push(format!(
        "impact of {}: {nsyms} symbols{f}; per depth {}{g}",
        top.row(),
        levels.iter().map(|l| (l.syms.len() + l.tops.len()).to_string()).collect::<Vec<_>>().join("/")
    ));
    // Every file the answer lists, at any depth.
    crate::tsindex::trace("impact: walk", t_walk);
    let syms: Vec<Vec<TSym>> = levels.iter().map(|l| l.syms.iter().filter_map(|i| by_id(c, *i)).collect()).collect();
    let mut listed = d1_paths;
    for (l, v) in levels.iter().zip(&syms) {
        listed.extend(l.tops.keys().cloned());
        listed.extend(v.iter().map(|s| s.path.clone()));
    }
    let t0 = std::time::Instant::now();
    lines.extend(guessed_elsewhere(c, pid, &syms, &guess, &listed)?);
    crate::tsindex::trace("impact: guessed elsewhere", t0);
    lines.extend(cut);
    lines.extend(dsp.capped);
    if let Some(d) = weak.iter().min() {
        lines.push(format!("partial at d{}: {}", d + 1, weak_left_out(weak_n)));
    }
    let mut src = SrcLines::new(root, nsyms <= CODE_SITES_MAX);
    for (d, (lvl, v)) in levels.iter().zip(syms).enumerate() {
        lines.push(format!("d{}", d + 1));
        let mut by: BTreeMap<String, Vec<(i64, String)>> = BTreeMap::new();
        for s in v {
            let mark = guess_mark(guess.get(&s.id).copied().flatten()) + disp.get(&s.id).cloned().flatten().as_deref().unwrap_or("");
            let head = format!("  :{} {} {}{mark}", s.line, s.qname(), s.h());
            let row = match use_at.get(&s.id).filter(|_| d == 0) {
                Some(&u) if u != s.line => match src.line(&s.path, u, &top.name) {
                    Some(code) => format!("{head} | :{u} {code}"),
                    None => src.with_code(head, &s.path, s.line, &s.name),
                },
                Some(&u) => src.with_code(head, &s.path, u, &top.name),
                None => src.with_code(head, &s.path, s.line, &s.name),
            };
            by.entry(s.path.clone()).or_default().push((s.line, row));
        }
        for (path, (ls, g)) in &lvl.tops {
            let at = ls.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            let dm = if d == 0 { disp_top.get(path).cloned().flatten().unwrap_or_default() } else { String::new() };
            let head = format!("  :{at} (top){}{dm}", guess_mark(*g));
            // The first top-level use's line, for a use of `top` itself.
            let row = match ls.first() {
                Some(&l) if d == 0 => src.with_code(head, path, l, &top.name),
                _ => head,
            };
            by.entry(path.clone()).or_default().push((i64::MAX, row));
        }
        for (path, mut rows) in by {
            rows.sort_by_key(|r| r.0);
            lines.push(format!(" {path}"));
            lines.extend(rows.into_iter().map(|r| r.1));
        }
    }
    Ok((d1_lines, dsp.down.iter().map(|m| m.id).collect()))
}

/// Same-name methods `top` is dispatched with: `up`, the interface or base methods it implements
/// or overrides (a call to one may reach `top`), and `down`, when `top` is declared in an
/// interface, the methods implementing it. Named supertypes (`implements`, `extends`, base
/// classes) are followed both ways. A Go type implements an interface when it has every method
/// of it (embedded interfaces included) and the link has evidence: the same directory, or a
/// file of the type's package that imports the interface's package and names the interface.
struct Dispatch {
    up: Vec<TSym>,
    down: Vec<TSym>,
    /// Of `up` and `down`, those linked only by a guess (C: a function stored in a member whose
    /// struct is not certain).
    guessed: HashSet<i64>,
    /// `partial at d1:` when a candidate list was capped.
    capped: Option<String>,
}

/// Most same-name Go methods checked against an interface by method set.
const DISPATCH_GO_MAX: usize = 300;
/// Most same-name symbols read for dispatch.
const DISPATCH_SAME_MAX: usize = 2000;

const SYM_PID: &str = "COALESCE(json_extract(n.meta, '$.pid'), 0)";

fn dispatch(c: &Connection, pid: i64, top: &TSym) -> Result<Dispatch> {
    if crate::lang::for_path(&top.path).is_some_and(|l| l.name == "c") {
        return c_dispatch(c, top);
    }
    let mut d = Dispatch { up: vec![], down: vec![], guessed: HashSet::new(), capped: None };
    if !is_callable_kind(&top.kind) || top.parent.as_deref().is_none_or(str::is_empty) {
        return Ok(d);
    }
    // Same-name symbols with their containers, interface members (which have one) first.
    let mut q = c.prepare_cached(&format!(
        "SELECT {COLS}, {SYM_PID} FROM node n INDEXED BY node_sym_name
         WHERE n.project_id = ?1 AND n.kind = 'symbol' AND n.source = 'treesitter' AND n.name = ?2 COLLATE NOCASE
           AND COALESCE(json_extract(n.meta,'$.k'), '') != 'field'
         ORDER BY {SYM_PID} = 0, n.id LIMIT {DISPATCH_SAME_MAX}"
    ))?;
    let same: Vec<(TSym, i64)> = q.query_map(params![pid, top.name], |r| Ok((map(r)?, r.get::<_, i64>(7)?)))?.flatten().collect();
    let read = same.len();
    // Interface members come first: the list was cut inside them only if its last row is one.
    let cut_in_members = read >= DISPATCH_SAME_MAX && same.last().is_some_and(|(_, spid)| *spid != 0);
    let fam = |p: &str| crate::lang::for_path(p).map(|l| l.family).unwrap_or("");
    let same: Vec<(TSym, i64)> =
        same.into_iter().filter(|(s, _)| s.name == top.name && s.id != top.id && fam(&s.path) == fam(&top.path)).collect();
    if same.is_empty() {
        return Ok(d);
    }
    let cap = |checked: usize, of: &str| {
        format!("partial at d1: dispatch checked the first {checked} of {of} same-name methods; the rest may implement or override it too")
    };
    let over = format!("over {DISPATCH_SAME_MAX}");
    let mut owners = Owners::default();
    let Some(owner) = owner_type(c, pid, top, &mut owners)? else { return Ok(d) };
    if fam(&top.path) == "go" {
        let many = go_dispatch(c, pid, &owner, &same, &mut owners, &mut d)?;
        // A concrete type looks only at interface members; an interface at every other method.
        if many {
            d.capped = Some(format!(
                "partial at d1: dispatch checked the first {DISPATCH_GO_MAX} same-name methods with evidence of use as {}; the rest may implement it too",
                owner.name
            ));
        } else if owner.kind == "interface" && read >= DISPATCH_SAME_MAX || owner.kind != "interface" && cut_in_members {
            d.capped = Some(cap(DISPATCH_SAME_MAX, &over));
        }
        return Ok(d);
    }
    let ups = related_types(c, owner.id, true)?;
    let downs =
        if matches!(owner.kind.as_str(), "interface" | "trait" | "protocol") { related_types(c, owner.id, false)? } else { HashSet::new() };
    if ups.is_empty() && downs.is_empty() {
        return Ok(d);
    }
    if read >= DISPATCH_SAME_MAX {
        d.capped = Some(cap(DISPATCH_SAME_MAX, &over));
    }
    for (s, spid) in &same {
        let spid = if *spid != 0 { Some(*spid) } else { owner_type(c, pid, s, &mut owners)?.map(|t| t.id) };
        match spid {
            Some(t) if ups.contains(&t) => d.up.push(s.clone()),
            Some(t) if downs.contains(&t) => d.down.push(s.clone()),
            _ => {}
        }
    }
    Ok(d)
}

/// C: a function is dispatched through the function-pointer members it is stored in
/// (`.bmap = ext4_bmap`, `chip->irq_mask = foo_mask`), `up`; a member dispatches to the
/// functions stored in it, `down`.
fn c_dispatch(c: &Connection, top: &TSym) -> Result<Dispatch> {
    let mut d = Dispatch { up: vec![], down: vec![], guessed: HashSet::new(), capped: None };
    // (linked id, certain): a negative slot is a guess.
    let sql = if top.kind == "function" {
        "SELECT abs(slot), MAX(slot > 0) FROM ts_ref WHERE dst = ?1 AND dst != 0 AND slot != 0 GROUP BY abs(slot)"
    } else if top.kind == "method" {
        "SELECT dst, MAX(slot > 0) FROM ts_ref INDEXED BY ts_ref_slot WHERE slot != 0 AND slot IN (?1, -?1) AND dst != 0 GROUP BY dst"
    } else {
        return Ok(d);
    };
    let mut q = c.prepare_cached(&format!("{sql} LIMIT {}", DISPATCH_SAME_MAX + 1))?;
    let ids: Vec<(i64, bool)> = q.query_map([top.id], |r| Ok((r.get(0)?, r.get(1)?)))?.flatten().collect();
    if ids.len() > DISPATCH_SAME_MAX {
        d.capped = Some(format!("partial at d1: dispatch read the first {DISPATCH_SAME_MAX} functions stored in it; there are more"));
    }
    d.guessed = ids.iter().filter(|x| !x.1).map(|x| x.0).collect();
    let mut v: Vec<TSym> =
        ids.into_iter().take(DISPATCH_SAME_MAX).map(|x| x.0).filter(|&i| i != top.id).filter_map(|i| by_id(c, i)).collect();
    v.sort_by(|a, b| (&a.path, a.line, a.id).cmp(&(&b.path, b.line, b.id)));
    if top.kind == "function" {
        d.up = v
    } else {
        d.down = v
    }
    Ok(d)
}

/// Go: implicit interfaces by method set, with evidence for each link (see [`Dispatch`]).
/// Returns whether the candidates with evidence were more than [`DISPATCH_GO_MAX`].
fn go_dispatch(c: &Connection, pid: i64, owner: &TSym, same: &[(TSym, i64)], owners: &mut Owners, d: &mut Dispatch) -> Result<bool> {
    let mut sets: HashMap<i64, Option<HashSet<String>>> = HashMap::new();
    let mut evidence = Evidence::default();
    if owner.kind == "interface" {
        let Some(want) = method_set(c, pid, owner, &mut sets, 0)? else { return Ok(false) };
        if want.is_empty() {
            return Ok(false);
        }
        // Types in the interface's own directory first; only directories with evidence (cheap,
        // from the word index) get their method sets read.
        let mut cands: Vec<&TSym> = same.iter().filter(|(_, spid)| *spid == 0).map(|(s, _)| s).collect();
        let here = crate::tsindex::dir_of(&owner.path);
        cands.sort_by_key(|s| crate::tsindex::dir_of(&s.path) != here);
        let mut checked = 0;
        for s in cands {
            if !go_evidence(c, pid, owner, crate::tsindex::dir_of(&s.path), &mut evidence)? {
                continue;
            }
            checked += 1;
            if checked > DISPATCH_GO_MAX {
                return Ok(true);
            }
            let Some(t) = owner_type(c, pid, s, owners)? else { continue };
            if t.kind != "interface" && method_set(c, pid, &t, &mut sets, 0)?.is_some_and(|have| want.is_subset(&have)) {
                d.down.push(s.clone());
            }
        }
    } else {
        let Some(have) = method_set(c, pid, owner, &mut sets, 0)? else { return Ok(false) };
        let tdir = crate::tsindex::dir_of(&owner.path);
        // Interface methods: their container is the interface.
        for (n, (s, spid)) in same.iter().filter(|(_, spid)| *spid != 0).enumerate() {
            if n >= DISPATCH_GO_MAX {
                return Ok(true);
            }
            let Some(iface) = by_id(c, *spid).filter(|t| t.kind == "interface") else { continue };
            if !method_set(c, pid, &iface, &mut sets, 0)?.is_some_and(|want| !want.is_empty() && want.is_subset(&have)) {
                continue;
            }
            if go_evidence(c, pid, &iface, tdir, &mut evidence)? {
                d.up.push(s.clone());
            }
        }
    }
    Ok(false)
}

/// Evidence that Go type `t` is used as interface `iface`: both in one directory, or a file in
/// `t`'s directory that imports `iface`'s package and names the interface (`var _ I = (*T)(nil)`,
/// a parameter or conversion).
#[derive(Default)]
struct Evidence {
    /// Per interface: the files naming it, by directory.
    named: HashMap<i64, HashMap<String, Vec<String>>>,
    /// Per (interface, directory): the answer.
    done: HashMap<(i64, String), bool>,
}

fn go_evidence(c: &Connection, pid: i64, iface: &TSym, tdir: &str, ev: &mut Evidence) -> Result<bool> {
    let dir_of = crate::tsindex::dir_of;
    let idir = dir_of(&iface.path);
    if idir == tdir {
        return Ok(true);
    }
    if let Some(v) = ev.done.get(&(iface.id, tdir.to_string())) {
        return Ok(*v);
    }
    if let std::collections::hash_map::Entry::Vacant(e) = ev.named.entry(iface.id) {
        let mut q = c.prepare_cached("SELECT path FROM fts_word WHERE fts_word MATCH ?1 AND project_id = ?2 LIMIT 50000")?;
        let w = format!("words:\"{}\"", crate::tsindex::case_word(&iface.name));
        let mut by: HashMap<String, Vec<String>> = HashMap::new();
        for f in q.query_map(params![w, pid], |r| r.get::<_, String>(0))?.flatten() {
            by.entry(dir_of(&f).to_string()).or_default().push(f);
        }
        e.insert(by);
    }
    let mut imports = c.prepare_cached("SELECT name FROM ts_ref WHERE project_id = ?1 AND path = ?2 AND kind = 'import'")?;
    let mut found = false;
    for f in ev.named.get(&iface.id).and_then(|by| by.get(tdir)).into_iter().flatten() {
        let mut rows = imports.query_map(params![pid, f], |r| r.get::<_, String>(0))?.flatten();
        if rows.any(|i| crate::tsindex::go_import_names_dir(i.trim_matches('"'), idir)) {
            found = true;
            break;
        }
    }
    ev.done.insert((iface.id, tdir.to_string()), found);
    Ok(found)
}

/// The type declaring method `m`: its container, or (Go methods sit outside their type) the
/// type of that name in the same directory. Types outside their methods are cached by
/// (directory, name).
type Owners = HashMap<(String, String), Option<TSym>>;

fn owner_type(c: &Connection, pid: i64, m: &TSym, cache: &mut Owners) -> Result<Option<TSym>> {
    let spid: i64 = c.query_row(&format!("SELECT {SYM_PID} FROM node n WHERE n.id = ?1"), [m.id], |r| r.get(0)).unwrap_or(0);
    if spid != 0 {
        return Ok(by_id(c, spid).filter(|t| is_type_kind(&t.kind)));
    }
    let Some(parent) = m.parent.as_deref().map(|p| p.rsplit('.').next().unwrap_or(p)) else { return Ok(None) };
    let dir = m.path.rsplit_once('/').map(|x| x.0).unwrap_or("");
    let key = (dir.to_string(), parent.to_string());
    if let Some(t) = cache.get(&key) {
        return Ok(t.clone());
    }
    let lo = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let hi = if dir.is_empty() { "\u{10ffff}".to_string() } else { format!("{dir}0") };
    let mut q = c.prepare_cached(&format!(
        "SELECT {COLS} FROM node n INDEXED BY node_project_path
         WHERE n.project_id = ?1 AND n.path >= ?2 AND n.path < ?3 AND n.kind = 'symbol' AND n.source = 'treesitter' AND n.name = ?4"
    ))?;
    let t = q
        .query_map(params![pid, lo, hi, parent], map)?
        .flatten()
        .find(|t| is_type_kind(&t.kind) && t.path.rsplit_once('/').map(|x| x.0).unwrap_or("") == dir);
    cache.insert(key, t.clone());
    Ok(t)
}

/// Types reachable from `t` by named supertype references, upward (`up`) or downward, three
/// steps at most. A supertype reference is an `impl` row (`implements`, `extends`, a base
/// class), or, for grammars that tag supertypes as plain type references, one on the type's
/// own line; a type named in the body (a field's type) is no supertype.
fn related_types(c: &Connection, t: i64, up: bool) -> Result<HashSet<i64>> {
    related_types_n(c, t, up, 3)
}

/// [`related_types`], `steps` deep.
fn related_types_n(c: &Connection, t: i64, up: bool, steps: usize) -> Result<HashSet<i64>> {
    let sql = if up {
        "SELECT DISTINCT r.dst FROM ts_ref r JOIN node n ON n.id = r.dst JOIN node s ON s.id = r.src
         WHERE r.src = ?1 AND r.src != 0 AND r.dst != 0 AND r.amb <= 0 AND (r.kind = 'impl' OR r.kind = 'ref' AND r.line = s.line)
           AND json_extract(n.meta,'$.k') IN ('class','interface','object','struct','enum','trait','protocol')"
    } else {
        "SELECT DISTINCT r.src FROM ts_ref r JOIN node n ON n.id = r.src
         WHERE r.dst = ?1 AND r.dst != 0 AND r.amb <= 0 AND (r.kind = 'impl' OR r.kind = 'ref' AND r.line = n.line)
           AND json_extract(n.meta,'$.k') IN ('class','interface','object','struct','enum','trait','protocol')"
    };
    let mut q = c.prepare_cached(sql)?;
    let mut seen: HashSet<i64> = HashSet::new();
    let mut frontier = vec![t];
    for _ in 0..steps {
        let mut next = vec![];
        for x in frontier {
            for y in q.query_map([x], |r| r.get::<_, i64>(0))?.flatten() {
                if y != t && seen.insert(y) {
                    next.push(y);
                }
            }
        }
        frontier = next;
    }
    Ok(seen)
}

/// Method names of Go type `t`: an interface's own methods and those of the interfaces it
/// embeds, or a type's methods (named with it as receiver in its directory). None when an
/// embedded interface is not in the project: its methods are unknown.
fn method_set(
    c: &Connection,
    pid: i64,
    t: &TSym,
    cache: &mut HashMap<i64, Option<HashSet<String>>>,
    depth: usize,
) -> Result<Option<HashSet<String>>> {
    if let Some(s) = cache.get(&t.id) {
        return Ok(s.clone());
    }
    let mut set: HashSet<String> = HashSet::new();
    if t.kind == "interface" {
        let mut q = c.prepare_cached(
            "SELECT name FROM node WHERE kind = 'symbol' AND json_extract(meta, '$.pid') = ?1 AND json_extract(meta,'$.k') = 'method'",
        )?;
        set.extend(q.query_map([t.id], |r| r.get::<_, String>(0))?.flatten());
        let embeds: Vec<i64> = {
            let mut q = c.prepare_cached("SELECT dst FROM ts_ref WHERE src = ?1 AND src != 0 AND kind = 'impl'")?;
            q.query_map([t.id], |r| r.get::<_, i64>(0))?.flatten().collect()
        };
        for e in embeds {
            let inner = match by_id(c, e).filter(|x| x.kind == "interface") {
                Some(x) if depth < 5 => method_set(c, pid, &x, cache, depth + 1)?,
                _ => None,
            };
            let Some(inner) = inner else {
                cache.insert(t.id, None);
                return Ok(None);
            };
            set.extend(inner);
        }
    } else {
        let dir = t.path.rsplit_once('/').map(|x| x.0).unwrap_or("");
        let lo = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        let hi = if dir.is_empty() { "\u{10ffff}".to_string() } else { format!("{dir}0") };
        let mut q = c.prepare_cached(
            "SELECT name, path FROM node WHERE project_id = ?1 AND path >= ?2 AND path < ?3 AND kind = 'symbol'
               AND json_extract(meta,'$.k') = 'method' AND json_extract(meta,'$.parent') = ?4",
        )?;
        for (name, path) in q.query_map(params![pid, lo, hi, t.name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.flatten() {
            if !path[lo.len()..].contains('/') {
                set.insert(name);
            }
        }
    }
    cache.insert(t.id, Some(set.clone()));
    Ok(Some(set))
}

/// `partial at dN:` when a call named like a dependent the walk expanded (any level but the
/// last), in a file the answer does not list, was guessed to another same-name definition that
/// ours was a real alternative to: through a receiver named like our dependent's type, or
/// (no receiver) to a definition in our dependent's directory or no nearer the call than ours.
/// That call may reach the dependent instead. One line, for the shallowest such level.
fn guessed_elsewhere(
    c: &Connection,
    pid: i64,
    levels: &[Vec<TSym>],
    walked: &HashMap<i64, Guess>,
    listed: &HashSet<String>,
) -> Result<Option<String>> {
    use crate::tsindex::{common_dirs, dir_of, via_match};
    let mut q = c.prepare_cached(
        "SELECT path, dst, recv, via FROM ts_ref WHERE project_id = ?1 AND name = ?2 AND amb > 0 AND (amb <= ?3 OR recv != 0) AND dst != 0 AND kind = 'call' LIMIT 5000",
    )?;
    let mut dst_path: HashMap<i64, Option<String>> = HashMap::new();
    let mut asked = 0;
    for (d, lvl) in levels.iter().enumerate().take(levels.len().saturating_sub(1)) {
        let mut by_name: BTreeMap<&str, Vec<&TSym>> = BTreeMap::new();
        for s in lvl {
            by_name.entry(s.name.as_str()).or_default().push(s);
        }
        let mut files: HashSet<String> = HashSet::new();
        let mut which: Vec<&str> = vec![];
        for (name, ours) in by_name {
            asked += 1;
            if asked > 100 {
                return Ok(None);
            }
            let rows = q.query_map(params![pid, name, WEAK_GUESS], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, bool>(2)?, r.get::<_, Option<String>>(3)?))
            })?;
            for (path, dst, recv, via) in rows.flatten() {
                if walked.contains_key(&dst) || listed.contains(&path) || files.contains(&path) {
                    continue;
                }
                let could = if recv {
                    via.as_deref().is_some_and(|v| {
                        ours.iter().any(|o| {
                            let ty = o.parent.as_deref().map(|p| p.rsplit('.').next().unwrap_or(p)).unwrap_or("");
                            !ty.is_empty() && via_match(v, ty) > 0
                        })
                    })
                } else {
                    let dp = dst_path.entry(dst).or_insert_with(|| by_id(c, dst).map(|s| s.path)).clone();
                    dp.is_some_and(|dp| {
                        ours.iter().any(|o| dir_of(&dp) == dir_of(&o.path) || common_dirs(&path, &dp) == common_dirs(&path, &o.path))
                    })
                };
                if could {
                    files.insert(path);
                    if which.last() != Some(&name) {
                        which.push(name);
                    }
                }
            }
        }
        if !files.is_empty() {
            let n = files.len();
            let more = if which.len() > 1 { format!(" (and {} other names)", which.len() - 1) } else { String::new() };
            return Ok(Some(format!(
                "partial at d{}: calls in {n} other file{} were guessed to another `{}`{more}; they may reach a d{} dependent",
                d + 2,
                if n == 1 { "" } else { "s" },
                which[0],
                d + 1
            )));
        }
    }
    Ok(None)
}

/// Does anything not yet walked depend on `ids`?
fn has_dependents(c: &Connection, ids: &[i64], seen: &HashMap<i64, Guess>, top_seen: &HashSet<String>) -> Result<bool> {
    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let mut q = c.prepare(&format!("SELECT src, path FROM ts_ref WHERE dst != 0 AND dst IN ({list}) AND kind != 'import'"))?;
    let rows: Vec<(i64, String)> = q.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?.flatten().collect();
    Ok(rows.iter().any(|(s, path)| if *s == 0 { !top_seen.contains(path) } else { !seen.contains_key(s) }))
}

fn outline_file(c: &Connection, p: &Project, rel: &str, cursor: usize, budget: usize) -> Result<Option<String>> {
    let mut q = c.prepare(&format!("SELECT {COLS} FROM node n WHERE n.project_id = ?1 AND n.kind = 'symbol' AND n.source = 'treesitter' AND n.path = ?2 ORDER BY n.line, n.id"))?;
    let v: Vec<TSym> = q.query_map(params![p.id, rel], map)?.flatten().collect();
    if v.is_empty() {
        return Ok(None);
    }
    // Fields are left to their type's outline, so types and functions stay on the first page.
    let fields = v.iter().filter(|s| s.kind == crate::tsindex::FIELD).count();
    let note = if fields > 0 { format!(", {fields} fields not listed: outline a type for them") } else { String::new() };
    let mut lines = vec![format!(
        "~outline {rel} ({} symbols{note}; ..N = last line; fox_read target=<handle> or lines=a-b reads one)",
        v.len() - fields
    )];
    for s in v.iter().filter(|s| s.kind != crate::tsindex::FIELD) {
        let depth = s.parent.as_deref().map(|x| x.matches('.').count() + 1).unwrap_or(0);
        let end = if s.end_line > s.line { format!(" ..{}", s.end_line) } else { String::new() };
        lines.push(format!("{}{} {} {} {}{end}", " ".repeat(depth), s.line, s.kind, s.name, s.h()));
    }
    Ok(Some(page(&lines, cursor, budget)))
}

#[cfg(test)]
mod tests {
    /// Name resolution is the hot step of every `fox code` call. It must seek the name index, not
    /// walk every tree-sitter symbol of the project (15 ms on a 28k-symbol Kotlin app).
    #[test]
    fn resolve_seeks_the_name_index() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&c).unwrap();
        let plan: Vec<String> = c
            .prepare(&format!("EXPLAIN QUERY PLAN {}", super::resolve_sql()))
            .unwrap()
            .query_map(rusqlite::params![1, "x"], |r| r.get::<_, String>(3))
            .unwrap()
            .flatten()
            .collect();
        assert!(plan.iter().any(|l| l.contains("node_sym_name")), "{plan:?}");
    }
}
