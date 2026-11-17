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
fn completeness(c: &Connection, pid: i64, top: &TSym, listed: &HashSet<String>, lead: &str) -> Result<Option<String>> {
    let n = unlisted_files(c, pid, top, listed)?;
    let name = &top.name;
    Ok((n > 0)
        .then(|| format!("{lead}: `{name}` also appears in {n} other file{}; confirm with: rg -nw {name}", if n == 1 { "" } else { "s" })))
}

fn unlisted_files(c: &Connection, pid: i64, top: &TSym, listed: &HashSet<String>) -> Result<usize> {
    let name = top.name.as_str();
    let mut skip: HashSet<String> = listed.clone();
    skip.insert(top.path.clone());
    // Files with an occurrence that may be ours, and files whose references all go elsewhere.
    let (mut open, mut closed): (HashSet<String>, HashSet<String>) = Default::default();
    let mut q = c.prepare_cached("SELECT path, dst, amb FROM ts_ref WHERE project_id = ?1 AND name = ?2 AND kind != 'import'")?;
    for (path, dst, amb) in
        q.query_map(params![pid, name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?.flatten()
    {
        if dst == top.id {
            skip.insert(path);
        } else if dst != 0 && amb <= 0 {
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
            if let Some(l) = completeness(c, p.id, top, &files, "may be incomplete")? {
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
        "impact" => {
            // Per dependent: None when some chain of certain edges reaches it, else the most
            // same-name candidates among the guessed edges (0: only through a guessed dependent).
            let mut guess: HashMap<i64, Option<i64>> = HashMap::from([(top.id, None)]);
            let mut frontier = vec![top.id];
            let mut levels: Vec<Vec<i64>> = vec![];
            for _ in 0..depth.clamp(1, 4) {
                if frontier.is_empty() || guess.len() > 5000 {
                    break;
                }
                let ids = frontier.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
                let mut q = c.prepare(&format!("SELECT src, dst, amb FROM ts_ref WHERE dst != 0 AND dst IN ({ids}) AND src != 0"))?;
                let mut next: Vec<i64> = vec![];
                let mut found: HashMap<i64, Option<i64>> = HashMap::new();
                for (src, dst, amb) in q.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?.flatten() {
                    if guess.contains_key(&src) {
                        continue;
                    }
                    let via = if amb > 0 { Some(amb) } else { guess.get(&dst).copied().flatten().map(|_| 0) };
                    let e = found.entry(src).or_insert_with(|| {
                        next.push(src);
                        via
                    });
                    *e = match (*e, via) {
                        (None, _) | (_, None) => None,
                        (Some(a), Some(b)) => Some(a.max(b)),
                    };
                }
                guess.extend(found);
                levels.push(next.clone());
                frontier = next;
            }
            let direct: HashSet<String> = {
                let mut q = c.prepare_cached("SELECT DISTINCT path FROM ts_ref WHERE dst = ?1 AND dst != 0")?;
                q.query_map([top.id], |r| r.get::<_, String>(0))?.flatten().collect()
            };
            let partial = completeness(c, p.id, top, &direct, "partial at d1")?;
            let total: usize = levels.iter().map(Vec::len).sum();
            let guessed = levels.iter().flatten().filter(|i| guess.get(i).is_some_and(Option::is_some)).count();
            let g = if guessed > 0 { format!(", {guessed} through name guesses") } else { String::new() };
            lines.push(format!(
                "impact of {}: {total} symbols; per depth {}{g}",
                top.row(),
                levels.iter().map(|l| l.len().to_string()).collect::<Vec<_>>().join("/")
            ));
            lines.extend(partial);
            for (d, lvl) in levels.iter().enumerate() {
                let mut v: Vec<TSym> = lvl.iter().filter_map(|i| by_id(c, *i)).collect();
                v.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
                lines.push(format!("d{}", d + 1));
                let mut last = String::new();
                for s in v {
                    if s.path != last {
                        lines.push(format!(" {}", s.path));
                        last = s.path.clone();
                    }
                    let mark = match guess.get(&s.id).copied().flatten() {
                        Some(0) => " guess (via a guessed caller)".to_string(),
                        Some(n) => guess_note(n),
                        None => String::new(),
                    };
                    lines.push(format!("  :{} {} {}{mark}", s.line, s.qname(), s.h()));
                }
            }
        }
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
