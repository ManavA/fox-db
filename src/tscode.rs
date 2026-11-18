//! `fox code` over the native tree-sitter index. Rows carry `c:` handles and a leading `~`
//! (name-resolved, not compiler-resolved). Output is grouped under file headers so paths are
//! never repeated, and call sites are line numbers, not code.

use crate::project::Project;
use crate::query::{clip, page};
use crate::tsindex::{is_callable_kind, is_type_kind};
use anyhow::{Result, bail};
use rusqlite::{Connection, params};
use std::collections::{BTreeMap, HashMap, HashSet};

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
        Some((a, b)) if !b.is_empty() => (Some(a.rsplit('.').next().unwrap_or(a)), b),
        _ => (None, q),
    };
    let mut s = c.prepare_cached(&resolve_sql())?;
    let mut v: Vec<(TSym, i64, bool)> = s
        .query_map(params![pid, name], |r| Ok((map(r)?, r.get::<_, i64>(7)?, r.get::<_, Option<i64>>(8)?.is_some())))?
        .flatten()
        .filter(|(t, ..)| {
            qual.is_none_or(|w| t.parent.as_deref().is_some_and(|p| p.rsplit('.').next().unwrap_or(p).eq_ignore_ascii_case(w)))
        })
        .collect();
    // The name as typed (and its type as typed) beats a same-name symbol in another case
    // (`render` the method, not `Render` the class); a definition beats its C prototype.
    v.sort_by_key(|(t, n, decl)| {
        let rank = if is_callable_kind(&t.kind) || is_type_kind(&t.kind) { 0 } else { 1 };
        let exact_qual = qual.is_none_or(|w| t.parent.as_deref().is_some_and(|p| p.rsplit('.').next().unwrap_or(p) == w));
        (t.name != name || !exact_qual, rank, *decl, is_test(&t.path) as u8, std::cmp::Reverse(*n), t.id)
    });
    Ok(v.into_iter().map(|x| x.0).collect())
}

/// Call or reference sites of `targets`, grouped by file: `path (n)` then `  :line Caller c:h`.
/// A site whose target was guessed by name among several same-name definitions ends in
/// `guess 1 of N`. Returns (sites, their files, guessed sites).
fn sites(c: &Connection, targets: &[i64], calls_only: bool, lines: &mut Vec<String>) -> Result<(usize, HashSet<String>, usize)> {
    let ids = targets.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let kind = if calls_only { "AND r.kind = 'call'" } else { "" };
    let sql = format!(
        "SELECT r.path, r.line, r.src, s.name, json_extract(s.meta,'$.parent'), r.amb FROM ts_ref r LEFT JOIN node s ON s.id = r.src
         WHERE r.dst != 0 AND r.dst IN ({ids}) {kind} ORDER BY r.path, r.line"
    );
    let mut q = c.prepare(&sql)?;
    // path -> [(caller display, [lines], most same-name candidates)], callers in first-seen order.
    type Who = (String, Vec<i64>, i64);
    let mut by: BTreeMap<String, Vec<Who>> = BTreeMap::new();
    let mut seen = HashSet::new();
    let (mut n, mut guessed) = (0, 0);
    for (path, line, src, sname, sparent, amb) in q
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?
        .flatten()
    {
        if !seen.insert((path.clone(), line, src)) {
            continue;
        }
        let who = match (sname, sparent) {
            (Some(nm), Some(p)) if !p.is_empty() => format!("{p}.{nm} c:{src:x}"),
            (Some(nm), _) => format!("{nm} c:{src:x}"),
            _ => "(top)".into(),
        };
        let e = by.entry(path).or_default();
        match e.iter_mut().find(|(w, ..)| *w == who) {
            Some((_, ls, a)) => {
                ls.push(line);
                *a = (*a).max(amb);
            }
            None => e.push((who, vec![line], amb)),
        }
        n += 1;
        if amb > 0 {
            guessed += 1;
        }
    }
    let files: HashSet<String> = by.keys().cloned().collect();
    group_by_dir(
        by.into_iter().map(|(path, v)| {
            let count: usize = v.iter().map(|x| x.1.len()).sum();
            let rows = v
                .into_iter()
                .map(|(who, ls, amb)| {
                    format!("  :{} {who}{}", ls.iter().map(i64::to_string).collect::<Vec<_>>().join(","), guess_note(amb))
                })
                .collect();
            (path, count, rows)
        }),
        lines,
    );
    Ok((n, files, guessed))
}

/// One line when `top`'s name occurs in more files than `listed` (its resolved sites) and its
/// own file: `{lead}: `name` also appears in N other files; confirm with: rg -nw name`. None when
/// the counts agree. Read from the index only (no file is opened): files with a reference of
/// that name not resolved with certainty to another symbol, and files holding it as a whole word.
/// With `calls_only`, a file that only names `top` as a value (a callback) still counts: what
/// it calls through that value is unknown.
fn completeness(c: &Connection, pid: i64, top: &TSym, listed: &HashSet<String>, calls_only: bool, lead: &str) -> Result<Option<String>> {
    let n = unlisted_files(c, pid, top, listed, calls_only)?;
    let name = &top.name;
    Ok((n > 0)
        .then(|| format!("{lead}: `{name}` also appears in {n} other file{}; confirm with: rg -nw {name}", if n == 1 { "" } else { "s" })))
}

fn unlisted_files(c: &Connection, pid: i64, top: &TSym, listed: &HashSet<String>, calls_only: bool) -> Result<usize> {
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
            "SELECT path, name FROM node INDEXED BY node_sym_name WHERE project_id = ?1 AND kind = 'symbol' AND name = ?2 COLLATE NOCASE AND source = 'treesitter'",
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
    Ok(open.iter().filter(|p| !skip.contains(*p)).count())
}

/// Files whose indexed symbol text holds `name` as a whole word, for a name of several parts
/// (a single word is also a part of longer names in that index).
fn body_paths(c: &Connection, pid: i64, name: &str) -> Result<Vec<String>> {
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

/// ` guess 1 of N` after a site resolved by name among N same-name definitions; ` guess by
/// name` when the only candidate was taken on its name alone (a call through a receiver).
fn guess_note(amb: i64) -> String {
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
    let Some(top) = cands.first() else { return Ok(None) };
    let mut lines: Vec<String> = vec![];
    if cands.len() > 1 {
        let others: Vec<String> = cands.iter().skip(1).take(4).map(|s| format!("{} {}", s.h(), s.qname())).collect();
        lines.push(format!("{} matches; others: {}{}", cands.len(), others.join(", "), if cands.len() > 5 { ", ..." } else { "" }));
    }
    match rel {
        "callers" | "refs" => {
            let calls = rel == "callers" && is_callable_kind(&top.kind);
            lines.push(format!("{rel} of {}", top.row()));
            let at = lines.len();
            let (n, files, guessed) = sites(c, &[top.id], calls, &mut lines)?;
            let g = if guessed > 0 { format!(", {guessed} guessed by name") } else { String::new() };
            lines.insert(at, format!("{n} {} in {} files{g}", if calls { "call sites" } else { "refs" }, files.len()));
            if let Some(l) = completeness(c, p.id, top, &files, calls, "may be incomplete")? {
                lines.insert(at + 1, l);
            }
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
        "impact" => impact(c, p.id, top, depth, &mut lines)?,
        "conforms" | "subtypes" | "extensions" | "overrides" => {
            // Supertype references (`reference.implementation` / `reference.class`) resolved to this type.
            lines.push(format!("{rel} of {}", top.row()));
            let mut q = c.prepare_cached(&format!(
                "SELECT DISTINCT {COLS} FROM ts_ref r JOIN node n ON n.id = r.src WHERE r.dst = ?1 AND r.dst != 0 AND r.kind = 'ref' AND json_extract(n.meta,'$.k') IN ('class','interface','object','struct','enum','trait') ORDER BY n.path, n.line"
            ))?;
            let v: Vec<TSym> = q.query_map([top.id], map)?.flatten().collect();
            lines.push(format!("{} types reference it", v.len()));
            lines.extend(v.iter().map(TSym::row));
        }
        _ => bail!("unsupported --rel {rel}"),
    }
    lines.push("~ tree-sitter, name-resolved".into());
    Ok(Some(page(&lines, cursor, budget)))
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
fn impact(c: &Connection, pid: i64, top: &TSym, depth: usize, lines: &mut Vec<String>) -> Result<()> {
    let mut guess: HashMap<i64, Guess> = HashMap::from([(top.id, None)]);
    let mut top_seen: HashSet<String> = HashSet::new();
    let mut frontier = vec![top.id];
    let mut levels: Vec<Level> = vec![];
    // Dispatch: calls to the methods `top` implements count as calls to it; implementations of
    // `top` are first-level dependents.
    let dsp = dispatch(c, pid, top)?;
    let up: HashMap<i64, String> = dsp.up.iter().map(|m| (m.id, format!(" via {}", m.qname()))).collect();
    let mut disp: HashMap<i64, Disp> = HashMap::new();
    let mut disp_top: HashMap<String, Disp> = HashMap::new();
    for m in &dsp.up {
        guess.insert(m.id, None);
        frontier.push(m.id);
    }
    let mut first = Level::default();
    for m in &dsp.down {
        if guess.insert(m.id, None).is_none() {
            first.syms.push(m.id);
            disp.insert(m.id, Some(format!(" implements {}", top.qname())));
        }
    }
    let mut d1_paths: HashSet<String> = dsp.down.iter().map(|m| m.path.clone()).collect();
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
            "SELECT src, dst, amb, path, line, kind FROM ts_ref WHERE dst != 0 AND dst IN ({ids}) AND (src != 0 OR kind != 'import') ORDER BY path, line"
        ))?;
        let mut lvl = if d == 0 { std::mem::take(&mut first) } else { Level::default() };
        let mut found: HashMap<i64, Guess> = HashMap::new();
        let mut tops: BTreeMap<String, (Vec<i64>, Guess)> = BTreeMap::new();
        for (src, dst, amb, path, line, kind) in q
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .flatten()
        {
            let dl: Disp = if d == 0 { up.get(&dst).cloned() } else { None };
            // Only a call to the interface or base method may dispatch to `top`.
            if dl.is_some() && kind != "call" {
                continue;
            }
            let via = if amb > 0 { Some(amb) } else { guess.get(&dst).copied().flatten().map(|_| 0) };
            if d == 0 {
                d1_paths.insert(path.clone());
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
        frontier = lvl.syms.clone();
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
    lines.push(format!(
        "impact of {}: {nsyms} symbols{f}; per depth {}{g}",
        top.row(),
        levels.iter().map(|l| (l.syms.len() + l.tops.len()).to_string()).collect::<Vec<_>>().join("/")
    ));
    // Every file the answer lists, at any depth.
    let syms: Vec<Vec<TSym>> = levels.iter().map(|l| l.syms.iter().filter_map(|i| by_id(c, *i)).collect()).collect();
    let mut listed = d1_paths;
    for (l, v) in levels.iter().zip(&syms) {
        listed.extend(l.tops.keys().cloned());
        listed.extend(v.iter().map(|s| s.path.clone()));
    }
    lines.extend(completeness(c, pid, top, &listed, false, "partial at d1")?);
    lines.extend(guessed_elsewhere(c, pid, &syms, &guess, &listed)?);
    lines.extend(cut);
    for (d, (lvl, v)) in levels.iter().zip(syms).enumerate() {
        lines.push(format!("d{}", d + 1));
        let mut by: BTreeMap<String, Vec<(i64, String)>> = BTreeMap::new();
        for s in v {
            let mark = guess_mark(guess.get(&s.id).copied().flatten()) + disp.get(&s.id).cloned().flatten().as_deref().unwrap_or("");
            by.entry(s.path.clone()).or_default().push((s.line, format!("  :{} {} {}{mark}", s.line, s.qname(), s.h())));
        }
        for (path, (ls, g)) in &lvl.tops {
            let at = ls.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            let dm = if d == 0 { disp_top.get(path).cloned().flatten().unwrap_or_default() } else { String::new() };
            by.entry(path.clone()).or_default().push((i64::MAX, format!("  :{at} (top){}{dm}", guess_mark(*g))));
        }
        for (path, mut rows) in by {
            rows.sort_by_key(|r| r.0);
            lines.push(format!(" {path}"));
            lines.extend(rows.into_iter().map(|r| r.1));
        }
    }
    Ok(())
}

/// Same-name methods `top` is dispatched with: `up`, the interface or base methods it implements
/// or overrides (a call to one may reach `top`), and `down`, when `top` is declared in an
/// interface, the methods implementing it. Named supertypes (`implements`, `extends`, base
/// classes) are followed both ways; a Go type implements an interface whose methods it all has.
struct Dispatch {
    up: Vec<TSym>,
    down: Vec<TSym>,
}

/// Most same-name Go methods checked against interfaces by method set.
const DISPATCH_GO_MAX: usize = 300;

const SYM_PID: &str = "COALESCE(json_extract(n.meta, '$.pid'), 0)";

fn dispatch(c: &Connection, pid: i64, top: &TSym) -> Result<Dispatch> {
    let mut d = Dispatch { up: vec![], down: vec![] };
    if !is_callable_kind(&top.kind) || top.parent.as_deref().is_none_or(str::is_empty) {
        return Ok(d);
    }
    // Same-name symbols with their containers.
    let mut q = c.prepare_cached(&format!(
        "SELECT {COLS}, {SYM_PID} FROM node n INDEXED BY node_sym_name
         WHERE n.project_id = ?1 AND n.kind = 'symbol' AND n.source = 'treesitter' AND n.name = ?2 COLLATE NOCASE LIMIT 2000"
    ))?;
    let same: Vec<(TSym, i64)> = q
        .query_map(params![pid, top.name], |r| Ok((map(r)?, r.get::<_, i64>(7)?)))?
        .flatten()
        .filter(|(s, _)| s.name == top.name)
        .collect();
    let fam = |p: &str| crate::lang::for_path(p).map(|l| l.family).unwrap_or("");
    let same: Vec<(TSym, i64)> = same.into_iter().filter(|(s, _)| s.id != top.id && fam(&s.path) == fam(&top.path)).collect();
    if same.is_empty() {
        return Ok(d);
    }
    let mut owners = Owners::default();
    let Some(owner) = owner_type(c, pid, top, &mut owners)? else { return Ok(d) };
    let ups = related_types(c, owner.id, true)?;
    let downs =
        if matches!(owner.kind.as_str(), "interface" | "trait" | "protocol") { related_types(c, owner.id, false)? } else { HashSet::new() };
    for (s, spid) in &same {
        let spid = if *spid != 0 { Some(*spid) } else { owner_type(c, pid, s, &mut owners)?.map(|t| t.id) };
        match spid {
            Some(t) if ups.contains(&t) => d.up.push(s.clone()),
            Some(t) if downs.contains(&t) => d.down.push(s.clone()),
            _ => {}
        }
    }
    // Go: implicit interfaces, by method set (names only), within a bounded candidate list.
    if fam(&top.path) == "go" {
        let mut sets: HashMap<i64, HashSet<String>> = HashMap::new();
        let owner_set = method_set(c, pid, &owner, &mut sets)?;
        for (s, _) in same.iter().take(DISPATCH_GO_MAX) {
            if d.up.iter().chain(&d.down).any(|x| x.id == s.id) {
                continue;
            }
            let Some(t) = owner_type(c, pid, s, &mut owners)? else { continue };
            let t_set = method_set(c, pid, &t, &mut sets)?;
            if t.kind == "interface" && owner.kind != "interface" && !t_set.is_empty() && t_set.is_subset(&owner_set) {
                d.up.push(s.clone());
            } else if owner.kind == "interface" && t.kind != "interface" && !owner_set.is_empty() && owner_set.is_subset(&t_set) {
                d.down.push(s.clone());
            }
        }
    }
    Ok(d)
}

/// The type declaring method `m`: its container, or (Go methods sit outside their type) the
/// type of that name in the same directory.
/// Types found by `owner_type`, by (directory, name) for types outside their methods.
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
    let mut q = c.prepare_cached(&format!(
        "SELECT {COLS} FROM node n INDEXED BY node_sym_name
         WHERE n.project_id = ?1 AND n.kind = 'symbol' AND n.source = 'treesitter' AND n.name = ?2 COLLATE NOCASE LIMIT 2000"
    ))?;
    let t = q
        .query_map(params![pid, parent], map)?
        .flatten()
        .find(|t| t.name == parent && is_type_kind(&t.kind) && t.path.rsplit_once('/').map(|x| x.0).unwrap_or("") == dir);
    cache.insert(key, t.clone());
    Ok(t)
}

/// Types reachable from `t` by named supertype references, upward (`up`) or downward, three
/// steps at most.
fn related_types(c: &Connection, t: i64, up: bool) -> Result<HashSet<i64>> {
    // A supertype is named on the type's own line (`class A(Base)`, `class R implements Router`);
    // a type named in its body (a field's type) is no supertype.
    let sql = if up {
        "SELECT DISTINCT r.dst FROM ts_ref r JOIN node n ON n.id = r.dst JOIN node s ON s.id = r.src
         WHERE r.src = ?1 AND r.src != 0 AND r.dst != 0 AND r.kind = 'ref' AND r.line = s.line
           AND json_extract(n.meta,'$.k') IN ('class','interface','object','struct','enum','trait','protocol')"
    } else {
        "SELECT DISTINCT r.src FROM ts_ref r JOIN node n ON n.id = r.src WHERE r.dst = ?1 AND r.dst != 0 AND r.kind = 'ref' AND r.line = n.line
           AND json_extract(n.meta,'$.k') IN ('class','interface','object','struct','enum','trait','protocol')"
    };
    let mut q = c.prepare_cached(sql)?;
    let mut seen: HashSet<i64> = HashSet::new();
    let mut frontier = vec![t];
    for _ in 0..3 {
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

/// Method names of type `t`: its members, or (Go) the methods named with it as receiver in its
/// directory.
fn method_set(c: &Connection, pid: i64, t: &TSym, cache: &mut HashMap<i64, HashSet<String>>) -> Result<HashSet<String>> {
    if let Some(s) = cache.get(&t.id) {
        return Ok(s.clone());
    }
    let mut set: HashSet<String> = {
        let mut q = c.prepare_cached(
            "SELECT name FROM node WHERE kind = 'symbol' AND json_extract(meta, '$.pid') = ?1 AND json_extract(meta,'$.k') = 'method'",
        )?;
        q.query_map([t.id], |r| r.get::<_, String>(0))?.flatten().collect()
    };
    if t.kind != "interface" {
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
    cache.insert(t.id, set.clone());
    Ok(set)
}

/// `partial at dN:` when a call named like a dependent the walk expanded (any level but the
/// last) was guessed to another same-name definition, in a file the answer does not list: that
/// call may reach the dependent instead. One line, for the shallowest such level.
fn guessed_elsewhere(
    c: &Connection,
    pid: i64,
    levels: &[Vec<TSym>],
    walked: &HashMap<i64, Guess>,
    listed: &HashSet<String>,
) -> Result<Option<String>> {
    let mut q =
        c.prepare_cached("SELECT path, dst FROM ts_ref WHERE project_id = ?1 AND name = ?2 AND amb > 0 AND dst != 0 AND kind = 'call'")?;
    let mut asked = 0;
    for (d, lvl) in levels.iter().enumerate().take(levels.len().saturating_sub(1)) {
        let mut names: Vec<&str> = lvl.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        let mut files: HashSet<String> = HashSet::new();
        let mut which: Vec<&str> = vec![];
        for name in names {
            asked += 1;
            if asked > 500 {
                return Ok(None);
            }
            for (path, dst) in q.query_map(params![pid, name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.flatten() {
                if !walked.contains_key(&dst) && !listed.contains(&path) && files.insert(path) && which.last() != Some(&name) {
                    which.push(name);
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
    let mut lines = vec![format!("~outline {rel} ({} symbols)", v.len())];
    for s in &v {
        let depth = s.parent.as_deref().map(|x| x.matches('.').count() + 1).unwrap_or(0);
        lines.push(format!("{}{} {} {} {}", " ".repeat(depth), s.line, s.kind, s.name, s.h()));
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
