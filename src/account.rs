//! Line accounting for `callers`, `refs` and `impact`: every line of the indexed code files of
//! the target's language that names it as a whole word (what `rg -w name` finds there), sorted
//! into groups, so an answer is a labelled superset of that grep. The first line of the answer
//! counts the groups and says whether every line is accounted for; the groups the answer does
//! not already list follow at the end, as `path:line | code` rows, trimmed to the budget.
//!
//! A line is accounted for only when it is positively one of: the target's definition, a
//! resolved use, an import under the same name, a declaration, another same-name definition or
//! a use resolved with certainty to one, a comment, or a string. Anything else, including a line
//! whose comments and strings could not be told apart with confidence, is unresolved.

use crate::project::Project;
use crate::tscode::{TSym, clip_around, word_at};
use anyhow::Result;
use rayon::prelude::*;
use rusqlite::{Connection, params};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the resolved lines are: the answer's own rows.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Mode {
    /// `callers` of a function: resolved calls.
    Calls,
    /// `refs`, or `callers` of a type: resolved references.
    Refs,
    /// `impact`: the uses its d1 rows stand for.
    Impact,
    /// No definition of the name is indexed: nothing is resolved.
    Name,
}

/// A line's group. Ordered as the groups are listed at the end of an answer.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Cat {
    /// May use the target: not resolved, guessed to another definition, a weak guess, a renamed
    /// import, or a line whose masking was uncertain.
    Unresolved,
    /// A reference to the target that is not a call (`callers` only).
    NotCall,
    /// A C/C++ prototype, `extern`, a forward declaration, a TypeScript `declare`, a Python
    /// `@overload` stub of the target.
    Decl,
    /// The definition line of another symbol of the same name, or a use resolved with certainty
    /// to one.
    Same,
    Import,
    /// Only in a comment (a docstring included).
    Comment,
    /// Only in a string literal: may be a dynamic use (`getattr(x, "plan")`).
    Str,
    /// The target's own definition line.
    Definition,
}

const LISTED: [Cat; 7] = [Cat::Unresolved, Cat::NotCall, Cat::Decl, Cat::Same, Cat::Import, Cat::Comment, Cat::Str];

/// More files naming it than this are not read line by line.
const FILES_MAX: usize = 20_000;
/// Most bytes read, and longest time spent, accounting for one name; past either the scan
/// stops and the answer says how many files it covered.
const BYTES_MAX: u64 = 400 << 20;
const TIME_MAX: Duration = Duration::from_millis(2500);
/// Most rows a group lists at any budget; the rest are counted.
fn cap(c: Cat) -> usize {
    match c {
        Cat::Unresolved => 200,
        Cat::Comment | Cat::Str => 5,
        _ => 50,
    }
}
/// Rows a group keeps when the budget is short, before the rest of the room is shared out.
fn quota(c: Cat) -> usize {
    match c {
        Cat::Unresolved => 8,
        Cat::NotCall => 5,
        Cat::Comment | Cat::Str => 2,
        _ => 3,
    }
}

/// A line of a file: (path, line).
pub type Line = (String, i64);
/// A reference on a line: (kind, target, ambiguity).
type RefAt = (String, i64, i64);

/// A listed row: (sort rank, path, line, text).
type Row = (u8, String, i64, String);

/// The accounting of one answer.
#[derive(Clone)]
pub struct Ledger {
    name: String,
    mode: Mode,
    resolved: usize,
    guessed: usize,
    counts: BTreeMap<Cat, usize>,
    rows: BTreeMap<Cat, Vec<Row>>,
    /// Same-name definitions elsewhere (their definition lines are the `Same` rows).
    same_defs: usize,
    stale: BTreeSet<String>,
    /// Files cited: their staleness is checked on every answer.
    cited: Vec<String>,
    /// A rebuild is pending: files naming it only in text may be missed.
    reindexing: bool,
    /// (files read, files naming it) when the scan stopped at a cap.
    capped: Option<(usize, usize)>,
}

/// Same-name definition.
struct Def {
    id: i64,
    line: i64,
    decl: bool,
    kind: String,
    qname: String,
}

/// What the scan of every file shares.
struct Ctx<'a> {
    name: &'a str,
    top: Option<&'a TSym>,
    top_id: i64,
    mode: Mode,
    resolved: &'a HashSet<Line>,
    linked: &'a [i64],
    defs: &'a HashMap<String, Vec<Def>>,
    refs: &'a HashMap<Line, Vec<RefAt>>,
}

/// One file's lines: (group, row) per line naming it, and the uses of `linked` definitions.
#[derive(Default)]
struct FileOut {
    lines: Vec<(Cat, Option<Row>, bool)>,
    linked: usize,
    any: bool,
}

/// Recently built ledgers: (store, its generation, project, name, target, mode) -> ledger. A
/// paged answer builds its ledger once.
type Key = (String, i64, i64, String, i64, Mode);
static CACHE: Mutex<Vec<(Key, Arc<Ledger>)>> = Mutex::new(Vec::new());
const CACHE_MAX: usize = 16;

/// Account for every line of the indexed code naming `name`. `resolved` holds the lines the
/// answer already lists (its calls, refs or d1 uses), `top` the target (None: no definition),
/// `linked` same-name definitions the answer lists too (implementations at d1). Built once per
/// store generation; staleness is checked on every call.
pub fn account(
    c: &Connection,
    p: &Project,
    name: &str,
    top: Option<&TSym>,
    resolved: &HashSet<Line>,
    linked: &[i64],
    mode: Mode,
) -> Result<Ledger> {
    let key: Key =
        (c.path().unwrap_or_default().to_string(), crate::db::generation(c), p.id, name.to_string(), top.map_or(-1, |t| t.id), mode);
    let hit = CACHE.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(k, _)| *k == key).map(|x| x.1.clone());
    let mut l = match hit {
        Some(l) => (*l).clone(),
        None => {
            let l = build(c, p, name, top, resolved, linked, mode)?;
            let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if cache.len() >= CACHE_MAX {
                cache.remove(0);
            }
            cache.push((key, Arc::new(l.clone())));
            l
        }
    };
    l.stale = stale(c, p, &l.cited, name, top)?;
    l.reindexing = crate::db::reindex_pending(c, p.id);
    Ok(l)
}

fn build(
    c: &Connection,
    p: &Project,
    name: &str,
    top: Option<&TSym>,
    resolved: &HashSet<Line>,
    linked: &[i64],
    mode: Mode,
) -> Result<Ledger> {
    let mut l = Ledger {
        name: name.to_string(),
        mode,
        resolved: resolved.len(),
        guessed: 0,
        counts: BTreeMap::new(),
        rows: BTreeMap::new(),
        same_defs: 0,
        stale: BTreeSet::new(),
        cited: vec![],
        reindexing: false,
        capped: None,
    };
    if name.is_empty() || !name.chars().all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$') {
        return Ok(l);
    }
    let pid = p.id;
    let top_id = top.map(|t| t.id).unwrap_or(-1);
    let fam = top.and_then(|t| family(&t.path));
    // Every definition of that exact name, by file.
    let mut defs: HashMap<String, Vec<Def>> = HashMap::new();
    let mut q = c.prepare_cached(
        "SELECT n.id, n.path, n.line, json_extract(n.meta,'$.k'), json_extract(n.meta,'$.parent'), json_extract(n.meta,'$.d'), n.name
         FROM node n INDEXED BY node_sym_name
         WHERE n.project_id = ?1 AND n.kind = 'symbol' AND n.source = 'treesitter' AND n.name = ?2 COLLATE NOCASE LIMIT 20000",
    )?;
    for (id, path, line, kind, parent, decl, nm) in q
        .query_map(params![pid, name], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                r.get::<_, Option<i64>>(5)?.is_some(),
                r.get::<_, String>(6)?,
            ))
        })?
        .flatten()
    {
        if nm != name {
            continue;
        }
        let qname = if parent.is_empty() { nm } else { format!("{parent}.{nm}") };
        defs.entry(path).or_default().push(Def { id, line, decl, kind, qname });
    }
    // Every reference row of that name, by line: (kind, target, ambiguity).
    let mut refs: HashMap<Line, Vec<RefAt>> = HashMap::new();
    let mut q = c.prepare_cached("SELECT path, line, kind, dst, amb FROM ts_ref WHERE project_id = ?1 AND name = ?2")?;
    for (path, line, kind, dst, amb) in q
        .query_map(params![pid, name], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?))
        })?
        .flatten()
    {
        refs.entry((path, line)).or_default().push((kind, dst, amb));
    }
    // Files naming it as a whole word (comments and strings included), of the target's language.
    let mut files: HashSet<String> = if crate::db::reindex_pending(c, pid) {
        crate::tscode::body_paths(c, pid, name)?.into_iter().collect()
    } else {
        let mut f = c.prepare_cached("SELECT path FROM fts_word WHERE fts_word MATCH ?1 AND project_id = ?2 LIMIT 50000")?;
        f.query_map(params![format!("words:\"{}\"", crate::tsindex::case_word(name)), pid], |r| r.get::<_, String>(0))?.flatten().collect()
    };
    files.extend(defs.keys().cloned());
    files.extend(refs.keys().map(|k| k.0.clone()));
    files.extend(resolved.iter().map(|k| k.0.clone()));
    let mut files: Vec<String> = files.into_iter().filter(|f| fam.is_none() || family(f) == fam).collect();
    files.sort();
    let total = files.len();
    files.truncate(FILES_MAX);
    let cx = Ctx { name, top, top_id, mode, resolved, linked, defs: &defs, refs: &refs };
    let t0 = Instant::now();
    let bytes = AtomicU64::new(0);
    let outs: Vec<Option<FileOut>> = files
        .par_iter()
        .map(|path| {
            if t0.elapsed() > TIME_MAX || bytes.load(Ordering::Relaxed) > BYTES_MAX {
                return None;
            }
            let Some(text) = crate::guard::read_text(&p.root.join(path), path, 4 << 20, false) else {
                // A file gone since indexing is cited, so its staleness shows.
                return Some(FileOut { any: !p.root.join(path).exists(), ..Default::default() });
            };
            bytes.fetch_add(text.len() as u64, Ordering::Relaxed);
            Some(scan_file(&cx, path, &text))
        })
        .collect();
    let read = outs.iter().filter(|o| o.is_some()).count();
    if read < total {
        l.capped = Some((read, total));
    }
    for (path, out) in files.iter().zip(outs) {
        let Some(out) = out else { continue };
        l.resolved += out.linked;
        if out.any || out.linked > 0 {
            l.cited.push(path.clone());
        }
        for (cat, row, same_def) in out.lines {
            *l.counts.entry(cat).or_default() += 1;
            if same_def {
                l.same_defs += 1;
            }
            if let Some(r) = row {
                l.rows.entry(cat).or_default().push(r);
            }
        }
    }
    for v in l.rows.values_mut() {
        v.sort_by(|a, b| (a.0, &a.1, a.2).cmp(&(b.0, &b.1, b.2)));
    }
    l.cited.extend(resolved.iter().map(|k| k.0.clone()));
    l.cited.sort();
    l.cited.dedup();
    Ok(l)
}

/// The interop family of a file's language (`c` for C and C++ headers, `js` for JS and TS).
fn family(path: &str) -> Option<&'static str> {
    crate::lang::for_path(path).map(|l| l.family)
}

/// Each line of one file naming the name, grouped.
fn scan_file(cx: &Ctx, path: &str, text: &str) -> FileOut {
    let mut out = FileOut::default();
    let name = cx.name;
    let nb = name.as_bytes();
    // Lines holding the name as a whole word, before anything costlier.
    let starts: Vec<usize> = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
    let mut hits: Vec<usize> = vec![];
    for (i, _) in text.match_indices(name) {
        let w = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b >= 0x80);
        let bytes = text.as_bytes();
        if w(i.checked_sub(1).and_then(|k| bytes.get(k))) || w(bytes.get(i + nb.len())) {
            continue;
        }
        let ln = starts.partition_point(|s| *s <= i) - 1;
        if hits.last() != Some(&ln) {
            hits.push(ln);
        }
    }
    let resolved_here = cx.resolved.iter().any(|(p, _)| p == path);
    if hits.is_empty() {
        out.any = resolved_here;
        return out;
    }
    let ext = path.rsplit_once('.').map(|x| x.1).unwrap_or("");
    let end = starts.get(hits[hits.len() - 1] + 1).copied().unwrap_or(text.len());
    let m = mask(&text.as_bytes()[..end], ext);
    let line_of = |s: &[u8], ln: usize| -> String {
        let a = starts[ln];
        let b = starts.get(ln + 1).map(|x| x - 1).unwrap_or(s.len()).min(s.len()).max(a);
        String::from_utf8_lossy(&s[a..b]).trim_end_matches('\r').to_string()
    };
    let mine = cx.defs.get(path).map(Vec::as_slice).unwrap_or(&[]);
    // Definition line of each same-name symbol: the first line from its start naming it.
    let mut def_at: HashMap<i64, &Def> = HashMap::new();
    for d in mine {
        if let Some(ln) = hits.iter().map(|h| *h as i64 + 1).find(|n| *n >= d.line && *n <= d.line + 5) {
            def_at.entry(ln).or_insert(d);
        }
    }
    let ty = cx.top.and_then(|t| t.parent.as_deref()).map(|p| p.rsplit('.').next().unwrap_or(p)).unwrap_or("");
    let stem = cx.top.map(|t| t.path.rsplit('/').next().unwrap_or(&t.path)).map(|f| f.split('.').next().unwrap_or(f)).unwrap_or("");
    for &ln in &hits {
        let n = ln as i64 + 1;
        let key = (path.to_string(), n);
        if cx.resolved.contains(&key) {
            out.any = true;
            continue;
        }
        let raw = line_of(text.as_bytes(), ln);
        let code_line = line_of(&m.text, ln);
        let unsure = m.unsure.get(ln).copied().unwrap_or(true);
        let row = |note: &str, rank: u8| -> Option<Row> {
            let code = clip_around(raw.trim(), name).unwrap_or_default();
            Some((rank, path.to_string(), n, format!("  {path}:{n}{note} | {code}")))
        };
        let in_code = word_at(&code_line, name).is_some();
        let renamed = in_code && renamed_import(&code_line, name);
        let (cat, r, same_def) = if let Some(d) = def_at.get(&n) {
            if cx.linked.contains(&d.id) {
                out.linked += 1;
                out.any = true;
                continue;
            }
            if d.id == cx.top_id {
                (Cat::Definition, None, false)
            } else {
                let cat = if d.decl || prototype(&code_line, name, ext) { Cat::Decl } else { Cat::Same };
                let code = clip_around(raw.trim(), name).unwrap_or_default();
                (
                    cat,
                    Some((0, path.to_string(), n, format!("  {path}:{n} {} {} c:{:x} | {code}", d.kind, d.qname, d.id))),
                    cat == Cat::Same,
                )
            }
        } else if renamed {
            (Cat::Unresolved, row(" renamed import", 0), false)
        } else if let Some(rs) = cx.refs.get(&key) {
            let to_top: Vec<&RefAt> = rs.iter().filter(|r| r.1 == cx.top_id).collect();
            if !to_top.is_empty() {
                if cx.mode == Mode::Calls && to_top.iter().all(|r| r.0 != "call") {
                    (Cat::NotCall, row("", 0), false)
                } else {
                    (Cat::Unresolved, row(" weak guess", 0), false)
                }
            } else if rs.iter().any(|r| r.0 == "import") && in_code {
                (Cat::Import, row("", 0), false)
            } else if rs.iter().all(|r| r.1 != 0 && r.2 <= 0) {
                (Cat::Same, None, false)
            } else {
                let note = if rs.iter().any(|r| r.1 != 0) { " guessed elsewhere" } else { "" };
                (Cat::Unresolved, row(note, plausible(&code_line, name, ty, stem)), false)
            }
        } else if import_line(raw.trim()) {
            if in_code {
                (Cat::Import, row("", 0), false)
            } else if family(path) == Some("js") {
                // `import doIt from './plan'`: the module's names, bound under others.
                (Cat::Unresolved, row(" imports the module under other names", 0), false)
            } else {
                (Cat::Import, row("", 0), false)
            }
        } else if !in_code && !unsure {
            if m.in_string(text, &starts, ln, name) { (Cat::Str, row("", 0), false) } else { (Cat::Comment, row("", 0), false) }
        } else if in_code && prototype(&code_line, name, ext) {
            (Cat::Decl, row("", 0), false)
        } else if in_code && py_stub(&code_line, name, ext) {
            // An `@overload` stub or property setter beside its definition: the target's own
            // in its file, else part of another same-name definition.
            if cx.top.is_some_and(|t| t.path == path) { (Cat::Decl, row("", 0), false) } else { (Cat::Same, None, false) }
        } else {
            (Cat::Unresolved, row("", plausible(&code_line, name, ty, stem)), false)
        };
        out.any = true;
        out.lines.push((cat, r, same_def));
    }
    out
}

/// Sort rank of an unresolved line: 0 when a receiver or qualifier before the name is named like
/// the target's type (or, for a free function, its module), 1 for a bare use, 2 otherwise.
fn plausible(code: &str, name: &str, ty: &str, stem: &str) -> u8 {
    let Some(at) = word_at(code, name) else { return 2 };
    let before = code[..at].trim_end();
    let Some(q) = before.strip_suffix('.').or_else(|| before.strip_suffix("::")).or_else(|| before.strip_suffix("->")) else {
        return 1;
    };
    let recv: String = q.chars().rev().take_while(|ch| ch.is_alphanumeric() || *ch == '_').collect::<String>().chars().rev().collect();
    if recv.is_empty() {
        return 2;
    }
    let hit = if ty.is_empty() { recv.eq_ignore_ascii_case(stem) } else { crate::tsindex::via_match(&recv, ty) > 0 };
    if hit { 0 } else { 2 }
}

/// `plan as p` (JS, Python, Rust) on a line: the name bound under another.
fn renamed_import(code: &str, name: &str) -> bool {
    let mut from = 0;
    while let Some(i) = code[from..].find(name).map(|i| i + from) {
        let rest = code[i + name.len()..].trim_start();
        if rest.strip_prefix("as").is_some_and(|r| r.starts_with(char::is_whitespace)) && word_at(&code[i..], name) == Some(0) {
            return true;
        }
        from = i + name.len();
    }
    false
}

/// Cited files whose content differs from what was indexed (size, or time and then content),
/// files gone, and files added since indexing beside indexed ones that name it.
fn stale(c: &Connection, p: &Project, files: &[String], name: &str, top: Option<&TSym>) -> Result<BTreeSet<String>> {
    let mut q = c.prepare_cached("SELECT size, mtime, hash FROM ts_file WHERE project_id = ?1 AND path = ?2")?;
    let mut rows = vec![];
    for f in files {
        if let Ok(r) = q.query_row(params![p.id, f], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))) {
            rows.push((f.clone(), r));
        }
    }
    let mut out: BTreeSet<String> = rows
        .par_iter()
        .filter(|(f, (size, mtime, hash))| {
            // An upgrade's re-parse marker: the next index run reads it again.
            if *size < 0 || hash.is_empty() {
                return false;
            }
            let Ok(md) = std::fs::metadata(p.root.join(f)) else { return true };
            if md.len() as i64 != *size {
                return true;
            }
            crate::inventory::mtime_ms(&md) != *mtime
                && std::fs::read(p.root.join(f)).is_ok_and(|b| crate::tsindex::content_hash(&b) != *hash)
        })
        .map(|(f, _)| f.clone())
        .collect();
    out.extend(added(c, p, name, top.and_then(|t| family(&t.path)))?);
    Ok(out)
}

/// Code files of family `fam` added since indexing, in directories holding indexed files, that
/// name `name`: a directory whose time is past the last index change is listed again.
fn added(c: &Connection, p: &Project, name: &str, fam: Option<&str>) -> Result<Vec<String>> {
    let mut q = c.prepare_cached("SELECT path FROM ts_file WHERE project_id = ?1")?;
    let known: HashSet<String> = q.query_map([p.id], |r| r.get::<_, String>(0))?.flatten().collect();
    let since: i64 = c
        .query_row("SELECT COALESCE(MAX(updated_at), 0) FROM node WHERE project_id = ?1 AND kind = 'file'", [p.id], |r| r.get(0))
        .unwrap_or(0)
        * 1000;
    let dirs: BTreeSet<&str> =
        known.iter().filter(|f| fam.is_none() || family(f) == fam).map(|f| f.rsplit_once('/').map_or("", |x| x.0)).collect();
    let dirs: Vec<&str> = dirs.into_iter().collect();
    let out: Vec<String> = dirs
        .par_iter()
        .flat_map_iter(|d| {
            let abs = if d.is_empty() { p.root.clone() } else { p.root.join(d) };
            let newer = std::fs::metadata(&abs).is_ok_and(|md| crate::inventory::mtime_ms(&md) >= since);
            let mut found = vec![];
            if newer && let Ok(rd) = std::fs::read_dir(&abs) {
                for e in rd.flatten() {
                    let fname = e.file_name().to_string_lossy().into_owned();
                    let rel = if d.is_empty() { fname.clone() } else { format!("{d}/{fname}") };
                    if known.contains(&rel) || fname.starts_with('.') || family(&rel).is_none() || fam.is_some() && family(&rel) != fam {
                        continue;
                    }
                    if e.file_type().is_ok_and(|t| t.is_file())
                        && let Some(t) = crate::guard::read_text(&e.path(), &rel, 4 << 20, false)
                        && word_at(&t, name).is_some()
                    {
                        found.push(rel);
                    }
                }
            }
            found
        })
        .collect();
    Ok(out)
}

/// An import line: `import`, `from ... import`, `use`, `using`, `#include`, `require`, a
/// re-export, a Go `package` clause.
fn import_line(l: &str) -> bool {
    ["import ", "from ", "use ", "pub use ", "pub(crate) use ", "using ", "package ", "#include", "#import", "require ", "require("]
        .iter()
        .any(|p| l.starts_with(p))
        || l.starts_with("export {") && l.contains(" from ")
        || l.starts_with("export * from")
}

/// A declaration naming it with no body: `extern`, a C-family prototype (only type words
/// before the name, a parameter list after, a `;` at the end), a forward declaration
/// (`struct name;`), a TypeScript `declare`. `m` is the line with comments and strings blanked.
fn prototype(m: &str, name: &str, ext: &str) -> bool {
    let t = m.trim();
    if t.starts_with("extern ") || t.starts_with("declare ") || t.starts_with("export declare ") {
        return true;
    }
    let cfam = matches!(ext, "h" | "hh" | "hpp" | "hxx" | "c" | "cc" | "cpp" | "cxx" | "m" | "mm");
    if !cfam {
        return false;
    }
    let Some(at) = word_at(t, name) else { return false };
    let before = t[..at].trim();
    let after = t[at + name.len()..].trim_start();
    if matches!(before, "struct" | "class" | "union" | "enum") && after == ";" {
        return true;
    }
    const NOT_TYPES: &[&str] = &["return", "else", "case", "do", "new", "delete", "throw", "goto", "sizeof", "co_return", "co_await"];
    !before.is_empty()
        && after.starts_with('(')
        && !t.contains('{')
        && (t.ends_with(';') || t.ends_with(','))
        && before.chars().all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | ' ' | '\t' | '*' | '&' | ':'))
        && !before.split(|ch: char| !(ch.is_alphanumeric() || ch == '_')).any(|w| NOT_TYPES.contains(&w))
}

/// A Python `def name(` line (an `@overload` stub or property setter has no symbol of its own).
fn py_stub(m: &str, name: &str, ext: &str) -> bool {
    if !matches!(ext, "py" | "pyi") {
        return false;
    }
    let t = m.trim();
    let rest = t.strip_prefix("async ").unwrap_or(t);
    rest.strip_prefix("def ").is_some_and(|r| r.trim_start().strip_prefix(name).is_some_and(|x| x.trim_start().starts_with('(')))
}

/// A file with its comments and string literals blanked, byte for byte (so lines and offsets
/// match the source): comments become spaces, string contents [`STR`]. `unsure` marks lines
/// whose masking may be wrong (a string still open at the end of a line where strings cannot
/// span lines, JSX text); such a line is never taken for a comment or a string.
pub(crate) struct Masked {
    pub text: Vec<u8>,
    pub unsure: Vec<bool>,
}

/// The byte a string's contents become.
const STR: u8 = 0x01;

impl Masked {
    /// Whether a mention of `name` on line `ln` of `src` sits inside a string literal (not a
    /// comment).
    fn in_string(&self, src: &str, starts: &[usize], ln: usize, name: &str) -> bool {
        let a = starts[ln];
        let b = starts.get(ln + 1).copied().unwrap_or(src.len()).min(self.text.len());
        src.get(a..b).is_some_and(|l| l.match_indices(name).any(|(i, _)| self.text.get(a + i) == Some(&STR)))
    }
}

#[derive(Clone, Copy, PartialEq)]
enum St {
    Code,
    /// Block comment, with its closer (`*/`, `]]`).
    Block(&'static [u8]),
    /// A string: its closing bytes, raw (no escapes), may span lines, interpolation opener.
    Str {
        close: &'static [u8],
        raw: bool,
        multi: bool,
        interp: Interp,
    },
    /// A C++ raw string `R"d(...)d"` or a Rust `r#"..."#`: its closer is built at the opening.
    RawDyn,
}

#[derive(Clone, Copy, PartialEq)]
enum Interp {
    None,
    /// `${...}` (JS template, Kotlin, Dart, Groovy, Scala, PHP, shell).
    DollarBrace,
    /// `{...}` (Python f-string, C# `$"..."`).
    Brace,
    /// `\(...)` (Swift).
    BackslashParen,
    /// `#{...}` (Ruby).
    HashBrace,
    /// `$(...)`, `${...}` and backticks (shell).
    Shell,
}

/// Mask comments and strings of `src`, a file of extension `ext`. See [`Masked`].
pub(crate) fn mask(src: &[u8], ext: &str) -> Masked {
    let py = matches!(ext, "py" | "pyi");
    let rs = ext == "rs";
    let go = ext == "go";
    let js = matches!(ext, "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts");
    let jsx = matches!(ext, "jsx" | "tsx");
    let shell = matches!(ext, "sh" | "bash" | "zsh");
    let hash = py || shell || matches!(ext, "rb" | "pl" | "r" | "R" | "php" | "ex" | "exs" | "toml" | "yaml" | "yml");
    let slash = !(py || shell || matches!(ext, "rb" | "pl" | "r" | "R" | "lua" | "ex" | "exs"));
    let lua = ext == "lua";
    let squote = !matches!(ext, "rs" | "ml" | "clj");
    let triple = py || matches!(ext, "kt" | "kts" | "java" | "scala" | "swift" | "groovy" | "dart" | "cs");
    let cpp = matches!(ext, "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "h");
    let cs = ext == "cs";
    let dollar = matches!(ext, "kt" | "kts" | "dart" | "groovy" | "scala" | "php");
    let mut out = src.to_vec();
    let mut unsure = vec![false];
    let mut st = St::Code;
    let mut dyn_close: Vec<u8> = vec![];
    // Interpolation: the string to return to and the code nesting depth inside it.
    let mut back: Option<(St, u32, u8, u8)> = None;
    let at = |i: usize, s: &[u8]| src.len() >= i + s.len() && &src[i..i + s.len()] == s;
    let mut i = 0;
    // Last significant code byte, for telling a JS regex from a division.
    let mut last_sig: u8 = b'\n';
    let mut esc = false;
    while i < src.len() {
        let b = src[i];
        if b == b'\n' {
            match st {
                St::Str { multi: false, raw, .. } => {
                    // A C line continuation keeps the string open; anything else is a string we
                    // did not understand.
                    if !esc || raw {
                        *unsure.last_mut().unwrap() = true;
                        st = St::Code;
                    }
                }
                St::Code if back.is_some() => {}
                _ => {}
            }
            esc = false;
            unsure.push(false);
            i += 1;
            continue;
        }
        match st {
            St::Code => {
                // Inside an interpolation: its closer returns to the string.
                if let Some((s, depth, open, close)) = back {
                    if b == open {
                        back = Some((s, depth + 1, open, close));
                    } else if b == close {
                        if depth == 0 {
                            back = None;
                            st = s;
                            out[i] = STR;
                            i += 1;
                            continue;
                        }
                        back = Some((s, depth - 1, open, close));
                    }
                }
                if slash && at(i, b"//")
                    || hash && b == b'#' && (!shell || i == 0 || src[i - 1].is_ascii_whitespace())
                    || lua && at(i, b"--")
                {
                    if lua && at(i, b"--[[") {
                        st = St::Block(b"]]");
                        continue;
                    }
                    // Rust `#[attr]`, PHP `#[...]`: not comments.
                    if b == b'#' && at(i, b"#[") && !py {
                        out[i] = b;
                        i += 1;
                        continue;
                    }
                    while i < src.len() && src[i] != b'\n' {
                        out[i] = b' ';
                        i += 1;
                    }
                    continue;
                }
                if slash && at(i, b"/*") {
                    st = St::Block(b"*/");
                    out[i] = b' ';
                    out[i + 1] = b' ';
                    i += 2;
                    continue;
                }
                // A JS regex literal: after an operator or an opening bracket.
                if js && b == b'/' && b"(,=:[!&|?{};+-*%<>~^\n".contains(&last_sig) {
                    let mut j = i + 1;
                    let mut class = false;
                    let mut ok = false;
                    while j < src.len() && src[j] != b'\n' {
                        match src[j] {
                            b'\\' => j += 1,
                            b'[' => class = true,
                            b']' => class = false,
                            b'/' if !class => {
                                ok = true;
                                break;
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                    if ok {
                        out[i..=j].fill(STR);
                        i = j + 1;
                        last_sig = b'/';
                        continue;
                    }
                }
                // Raw strings: Rust `r"..."`, `r#"..."#`, `br"..."`; C++ `R"d(...)d"`; C# `@"..."`.
                let word_before = i > 0 && (src[i - 1].is_ascii_alphanumeric() || src[i - 1] == b'_');
                if rs && (b == b'r' || at(i, b"br")) && !word_before {
                    let k = if b == b'b' { i + 2 } else { i + 1 };
                    let hashes = src[k..].iter().take_while(|x| **x == b'#').count();
                    if src.get(k + hashes) == Some(&b'"') {
                        dyn_close = std::iter::once(b'"').chain(std::iter::repeat_n(b'#', hashes)).collect();
                        st = St::RawDyn;
                        for x in out.iter_mut().take(k + hashes + 1).skip(i) {
                            *x = STR;
                        }
                        i = k + hashes + 1;
                        continue;
                    }
                }
                if cpp && b == b'R' && src.get(i + 1) == Some(&b'"') && !word_before {
                    let d: Vec<u8> = src[i + 2..].iter().take_while(|x| **x != b'(' && **x != b'\n').copied().collect();
                    if src.get(i + 2 + d.len()) == Some(&b'(') {
                        dyn_close = std::iter::once(b')').chain(d.iter().copied()).chain(std::iter::once(b'"')).collect();
                        st = St::RawDyn;
                        let k = i + 3 + d.len();
                        for x in out.iter_mut().take(k).skip(i) {
                            *x = STR;
                        }
                        i = k;
                        continue;
                    }
                }
                if cs && (at(i, b"@\"") || at(i, b"$@\"") || at(i, b"@$\"")) {
                    let k = if b == b'@' && src[i + 1] == b'"' { i + 2 } else { i + 3 };
                    let interp = if at(i, b"@\"") { Interp::None } else { Interp::Brace };
                    st = St::Str { close: b"\"", raw: true, multi: true, interp };
                    for x in out.iter_mut().take(k).skip(i) {
                        *x = STR;
                    }
                    i = k;
                    continue;
                }
                if triple && (at(i, b"\"\"\"") || py && at(i, b"'''")) {
                    let close: &'static [u8] = if b == b'"' { b"\"\"\"" } else { b"'''" };
                    let fpre = py && i > 0 && matches!(src[i - 1], b'f' | b'F');
                    let interp = if fpre {
                        Interp::Brace
                    } else if dollar {
                        Interp::DollarBrace
                    } else if ext == "swift" {
                        Interp::BackslashParen
                    } else {
                        Interp::None
                    };
                    // A Python docstring is documentation: a comment.
                    st = St::Str { close, raw: false, multi: true, interp };
                    out[i] = STR;
                    out[i + 1] = STR;
                    out[i + 2] = STR;
                    i += 3;
                    if py && !fpre {
                        st = St::Block(close);
                        out[i - 3..i].fill(b' ');
                    }
                    continue;
                }
                let quote = b == b'"' || squote && b == b'\'' || (go || js || shell) && b == b'`';
                if quote {
                    // A char literal in Rust is not a string (`'"'`).
                    let close: &'static [u8] = match b {
                        b'"' => b"\"",
                        b'\'' => b"'",
                        _ => b"`",
                    };
                    let multi = b == b'`' || rs && b == b'"';
                    let raw = go && b == b'`';
                    let prev = if i > 0 { src[i - 1] } else { b' ' };
                    let interp = match b {
                        b'`' if js => Interp::DollarBrace,
                        b'`' if shell => Interp::None,
                        b'"' if dollar => Interp::DollarBrace,
                        b'"' if shell => Interp::Shell,
                        b'"' if ext == "swift" => Interp::BackslashParen,
                        b'"' if ext == "rb" => Interp::HashBrace,
                        b'"' | b'\'' if py && matches!(prev, b'f' | b'F') => Interp::Brace,
                        b'"' if cs && prev == b'$' => Interp::Brace,
                        _ => Interp::None,
                    };
                    if b == b'`' && shell {
                        // A command substitution: code.
                        i += 1;
                        continue;
                    }
                    // JSX text (`<p>It's</p>`): an apostrophe there opens nothing we can trust.
                    if jsx && b == b'\'' {
                        *unsure.last_mut().unwrap() = true;
                    }
                    st = St::Str { close, raw, multi, interp };
                    out[i] = STR;
                    i += 1;
                    continue;
                }
                if rs && b == b'\'' {
                    // `'"'`, `'\''`: a char literal; `'a` a lifetime.
                    let len = if src.get(i + 1) == Some(&b'\\') {
                        src[i + 2..].iter().skip(1).position(|x| *x == b'\'').map_or(1, |k| k + 4)
                    } else if src.get(i + 2) == Some(&b'\'') {
                        3
                    } else {
                        1
                    };
                    let len = len.min(src.len() - i);
                    if len > 1 {
                        out[i..i + len].fill(STR);
                    }
                    i += len;
                    continue;
                }
                if !b.is_ascii_whitespace() {
                    last_sig = if b.is_ascii_alphanumeric() || b == b'_' || b == b')' || b == b']' { b'a' } else { b };
                }
                i += 1;
            }
            St::Block(close) => {
                if at(i, close) {
                    out[i..i + close.len()].fill(b' ');
                    i += close.len();
                    st = St::Code;
                    continue;
                }
                out[i] = b' ';
                i += 1;
            }
            St::RawDyn => {
                if at(i, &dyn_close) {
                    let n = dyn_close.len();
                    out[i..i + n].fill(STR);
                    i += n;
                    st = St::Code;
                    last_sig = b'a';
                    continue;
                }
                out[i] = STR;
                i += 1;
            }
            St::Str { close, raw, interp, .. } => {
                if esc {
                    esc = false;
                    out[i] = STR;
                    i += 1;
                    continue;
                }
                if b == b'\\' && !raw && !(interp == Interp::BackslashParen && src.get(i + 1) == Some(&b'(')) {
                    esc = true;
                    out[i] = STR;
                    i += 1;
                    continue;
                }
                // C# verbatim: `""` is a quote.
                if raw && cs && at(i, b"\"\"") {
                    out[i..i + 2].fill(STR);
                    i += 2;
                    continue;
                }
                if at(i, close) {
                    out[i..i + close.len()].fill(STR);
                    i += close.len();
                    st = St::Code;
                    last_sig = b'a';
                    continue;
                }
                let open = match interp {
                    Interp::DollarBrace if at(i, b"${") => Some((2, b'{', b'}')),
                    Interp::Brace if b == b'{' && !at(i, b"{{") => Some((1, b'{', b'}')),
                    Interp::BackslashParen if at(i, b"\\(") => Some((2, b'(', b')')),
                    Interp::HashBrace if at(i, b"#{") => Some((2, b'{', b'}')),
                    Interp::Shell if at(i, b"$(") => Some((2, b'(', b')')),
                    Interp::Shell if at(i, b"${") => Some((2, b'{', b'}')),
                    _ => None,
                };
                if let Some((n, o, c)) = open {
                    out[i..i + n].fill(STR);
                    i += n;
                    back = Some((st, 0, o, c));
                    st = St::Code;
                    continue;
                }
                if interp == Interp::Brace && at(i, b"{{") {
                    out[i..i + 2].fill(STR);
                    i += 2;
                    continue;
                }
                if interp == Interp::DollarBrace
                    && dollar
                    && b == b'$'
                    && src.get(i + 1).is_some_and(|x| x.is_ascii_alphabetic() || *x == b'_')
                {
                    // `"$name"`: the name is code.
                    out[i] = STR;
                    i += 1;
                    while i < src.len() && (src[i].is_ascii_alphanumeric() || src[i] == b'_') {
                        i += 1;
                    }
                    continue;
                }
                out[i] = STR;
                i += 1;
            }
        }
    }
    // A string or interpolation still open at the end: the last line is not understood.
    if matches!(st, St::Str { .. } | St::RawDyn) && !src.ends_with(b"\n") || back.is_some() {
        *unsure.last_mut().unwrap() = true;
    }
    Masked { text: out, unsure }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

impl Ledger {
    pub fn set_guessed(&mut self, n: usize) {
        self.guessed = n;
    }

    fn n(&self, c: Cat) -> usize {
        self.counts.get(&c).copied().unwrap_or(0)
    }

    /// Lines naming it that may use the target and are listed only at the end.
    pub fn unresolved(&self) -> usize {
        self.n(Cat::Unresolved)
    }

    /// The accounting line that leads the answer; None when nothing was read.
    pub fn head(&self) -> Option<String> {
        let name = &self.name;
        let total = self.resolved + self.counts.values().sum::<usize>();
        if total == 0 && self.capped.is_none() {
            return None;
        }
        let mut parts = vec![];
        let g = if self.guessed > 0 { format!(" ({} guessed)", self.guessed) } else { String::new() };
        match self.mode {
            Mode::Calls => parts.push(plural(self.resolved, "call", "calls") + &g),
            Mode::Refs => parts.push(plural(self.resolved, "ref", "refs") + &g),
            Mode::Impact => parts.push(plural(self.resolved, "use", "uses") + " (d1)"),
            Mode::Name => {}
        }
        if self.n(Cat::Definition) > 0 {
            parts.push(plural(self.n(Cat::Definition), "definition", "definition lines"));
        }
        if self.n(Cat::NotCall) > 0 {
            parts.push(plural(self.n(Cat::NotCall), "reference, not a call", "references, not calls"));
        }
        if self.n(Cat::Import) > 0 {
            parts.push(plural(self.n(Cat::Import), "import", "imports"));
        }
        if self.n(Cat::Decl) > 0 {
            parts.push(plural(self.n(Cat::Decl), "declaration/prototype", "declarations/prototypes"));
        }
        if self.n(Cat::Same) > 0 {
            parts.push(format!(
                "{} in {} elsewhere",
                self.n(Cat::Same),
                plural(self.same_defs, "same-name definition", "same-name definitions")
            ));
        }
        if self.n(Cat::Comment) > 0 {
            parts.push(plural(self.n(Cat::Comment), "comment", "comments"));
        }
        if self.n(Cat::Str) > 0 {
            parts.push(format!("{} in strings (may be dynamic uses)", self.n(Cat::Str)));
        }
        if self.unresolved() > 0 {
            parts.push(format!("{} unresolved", self.unresolved()));
        }
        let status = if !self.stale.is_empty() {
            let k = self.stale.len();
            let some: Vec<&str> = self.stale.iter().take(3).map(String::as_str).collect();
            format!(
                "the index is stale for {} ({}{}): counts may be off; reindex (fox index) first",
                plural(k, "file", "files"),
                some.join(", "),
                if k > 3 { ", ..." } else { "" }
            )
        } else if self.reindexing {
            "the index is being rebuilt: lines in files not yet re-read may be missing".to_string()
        } else if let Some((read, of)) = self.capped {
            format!("counted in {read} of the {of} files naming it (the scan stops at a size and time cap); the rest are not accounted for")
        } else if self.unresolved() == 0 && self.n(Cat::NotCall) > 0 {
            // A function passed as a value may be called through it: not followed.
            let k = self.n(Cat::NotCall);
            format!(
                "may be incomplete: {} it as a value, and calls through {} are not followed (rel=impact lists the code passing it)",
                plural(k, "line passes", "lines pass"),
                if k == 1 { "it" } else { "them" }
            )
        } else if self.unresolved() == 0 {
            "all accounted for".to_string()
        } else {
            let u = plural(self.unresolved(), "unresolved line", "unresolved lines");
            match self.mode {
                Mode::Impact => format!("partial at d1: {u} may use it, listed under unresolved mentions"),
                Mode::Name => format!("no definition of `{name}` is indexed; its lines are listed below"),
                _ => format!("may be incomplete: {u} may use it, listed under unresolved mentions"),
            }
        };
        let total = plural(total, "line names", "lines name");
        Some(format!("{total} `{name}`: {}; {status}", parts.join(", ")))
    }

    fn header(&self, c: Cat) -> String {
        match c {
            Cat::Unresolved => "unresolved mentions:".into(),
            Cat::NotCall => "references, not calls (what is called through them is not followed):".into(),
            Cat::Decl => "declarations and prototypes:".into(),
            Cat::Same => {
                let inner = self.n(Cat::Same) - self.same_defs;
                if inner > 0 {
                    format!(
                        "same-name definitions elsewhere (different symbols; {} using them not listed):",
                        plural(inner, "line", "lines")
                    )
                } else {
                    "same-name definitions elsewhere (different symbols):".into()
                }
            }
            Cat::Import => "import lines:".into(),
            Cat::Comment => "comment lines:".into(),
            Cat::Str => "strings (may be dynamic uses):".into(),
            Cat::Definition => String::new(),
        }
    }

    /// The groups' rows for the end of the answer, within `room` bytes; `main` is the size of
    /// the rest of the answer. Each group lists at most [`cap`] rows at any budget; when the
    /// room is short each keeps a few. A last line says what is left out, and names the budget
    /// that lists the rest only when one can.
    pub fn tail(&self, room: usize, main: usize) -> Vec<String> {
        let size = |v: &[Row]| v.iter().map(|l| l.3.len() + 1).sum::<usize>();
        let groups: Vec<(Cat, &Vec<Row>)> = LISTED.iter().filter_map(|c| self.rows.get(c).map(|v| (*c, v))).collect();
        let capped = |c: Cat, n: usize| n.min(cap(c));
        let full: usize = groups.iter().map(|(c, v)| self.header(*c).len() + 24 + size(&v[..capped(*c, v.len())])).sum();
        let mut keep: Vec<usize> = groups.iter().map(|(c, v)| capped(*c, v.len())).collect();
        if full + 80 > room {
            // Headers and the hint first; then each group up to its quota, in order; then the rest.
            keep = vec![0; groups.len()];
            let mut used: usize = groups.iter().map(|(c, _)| self.header(*c).len() + 24).sum::<usize>() + 80;
            for pass in 0..2 {
                for (i, (c, v)) in groups.iter().enumerate() {
                    let upto = if pass == 0 { quota(*c).min(capped(*c, v.len())) } else { capped(*c, v.len()) };
                    while keep[i] < upto && (pass == 0 || used + v[keep[i]].3.len() < room) {
                        used += v[keep[i]].3.len() + 1;
                        keep[i] += 1;
                    }
                }
            }
        }
        // Rows only counted: past a cap (comments and strings always), or past the room.
        let past_cap: usize =
            groups.iter().filter(|(c, _)| !matches!(c, Cat::Comment | Cat::Str)).map(|(c, v)| v.len() - capped(*c, v.len())).sum();
        let past_room: usize = groups.iter().zip(&keep).map(|((c, v), k)| capped(*c, v.len()) - k).sum();
        let mut out = vec![];
        if past_room > 0 || past_cap > 0 {
            let need = (main + full + 80 + 64).div_ceil(4);
            let n = past_room + past_cap;
            out.push(if past_cap == 0 && need <= 4000 {
                format!("rows left out to fit the budget; budget={need} lists them all")
            } else {
                format!("{} counted, not listed", plural(n, "more line is", "more lines are"))
            });
        }
        for ((c, v), k) in groups.iter().zip(&keep) {
            if *k == 0 {
                continue;
            }
            let h = self.header(*c);
            if *k < v.len() {
                out.push(format!("{} (first {k} of {})", h.trim_end_matches(':'), v.len()) + ":");
            } else {
                out.push(h);
            }
            out.extend(v[..*k].iter().map(|r| r.3.clone()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{mask, prototype};

    fn lines(src: &str, ext: &str) -> Vec<String> {
        let m = mask(src.as_bytes(), ext);
        String::from_utf8_lossy(&m.text).split('\n').map(|l| l.replace('\u{1}', " ")).collect()
    }

    fn unsure(src: &str, ext: &str) -> Vec<bool> {
        mask(src.as_bytes(), ext).unsure
    }

    #[test]
    fn mask_blanks_comments_and_strings() {
        let m = lines("x = 1  # plan it\ny = \"plan\"\n\"\"\"\nplan here\n\"\"\"\nplan()\n", "py");
        assert!(!m[0].contains("plan") && !m[1].contains("plan") && !m[3].contains("plan") && m[5].contains("plan"), "{m:?}");
        let m = lines("/* plan\n * plan\n */ plan(1); // plan\nchar *s = \"plan\";\n", "c");
        assert!(!m[0].contains("plan") && !m[1].contains("plan") && m[2].matches("plan").count() == 1 && !m[3].contains("plan"), "{m:?}");
        let m = lines("fn go<'a>(x: &'a str) { plan(x) }\n", "rs");
        assert!(m[0].contains("plan"), "{m:?}");
        let m = lines("const s = `${plan(1)}`\n", "ts");
        assert!(m[0].contains("plan"), "{m:?}");
        let m = lines("const s = `it's plan`; plan()\n", "ts");
        assert!(m[0].matches("plan").count() == 1, "{m:?}");
        let m = lines("if c == '\"' { plan(x) }\nlet q = '\\''; plan(y)\n", "rs");
        assert!(m[0].contains("plan") && m[1].contains("plan"), "{m:?}");
        for (l, ext) in [
            ("x = f\"{plan()}\"", "py"),
            ("val s = \"${plan()}\"", "kt"),
            ("let s = \"\\(plan())\"", "swift"),
            ("var s = $\"{plan()}\";", "cs"),
            ("s = \"#{plan}\"", "rb"),
            ("x=\"$(plan)\"", "sh"),
            ("n=${#plan} plan", "sh"),
        ] {
            assert!(lines(l, ext)[0].contains("plan"), "{l}");
        }
        for (l, ext) in [("x = \"{plan}\"", "py"), ("s = 'plan'", "rb"), ("x='$(plan)'", "sh")] {
            assert!(!lines(l, ext)[0].contains("plan"), "{l}");
        }
    }

    /// Strings that span lines keep their state; raw strings take no escapes; a regex is no
    /// template literal; uncertain lines say so.
    #[test]
    fn mask_tracks_multi_line_and_raw_strings() {
        let m = lines("let u = \"a\n src/*.rs\n\";\nplan();\n", "rs");
        assert!(m[3].contains("plan"), "{m:?}");
        let m = lines("let p = r\"C:\\\"; plan();\nlet q = r#\"a \"# plan\"#; plan();\n", "rs");
        assert!(m[0].contains("plan") && m[1].matches("plan").count() == 1, "{m:?}");
        let m = lines("val h = \"\"\"\n src/*.kt\n\"\"\"\nplan()\n", "kt");
        assert!(m[3].contains("plan"), "{m:?}");
        let m = lines("auto s = R\"x(a /* \")x\"; plan();\n", "cpp");
        assert!(m[0].contains("plan"), "{m:?}");
        let m = lines("var s = @\"C:\\\"; plan();\n", "cs");
        assert!(m[0].contains("plan"), "{m:?}");
        let m = lines("const Q = /[`'\"]/;\nexport const h = plan;\n", "js");
        assert!(m[1].contains("plan"), "{m:?}");
        assert!(unsure("return <p>It's {plan()}</p>;\n", "jsx")[0]);
        assert!(unsure("s = 'open\nplan()\n", "py")[0]);
        let m = lines("static const char *s = \"a\\\nb\"; int y = plan(2);\n", "c");
        assert!(m[1].contains("plan"), "{m:?}");
    }

    #[test]
    fn prototypes_and_forward_declarations() {
        for (l, ext) in [
            ("int plan(int q);", "h"),
            ("PlannedStmt *plan(Query *q,", "h"),
            ("static int plan(void);", "c"),
            ("extern int plan;", "c"),
            ("struct plan;", "h"),
            ("declare function plan(x: number): void;", "ts"),
        ] {
            assert!(prototype(l, "plan", ext), "{l}");
        }
        for (l, ext) in [("return plan(q);", "c"), ("int x = plan(q);", "c"), ("int plan(int q) { return q; }", "c"), ("plan(1);", "c")] {
            assert!(!prototype(l, "plan", ext), "{l}");
        }
    }
}
