//! Line accounting for `callers`, `refs` and `impact`: every line of the code files of the
//! target's language that names it as a whole word (what `rg -w name` finds there), sorted into
//! groups, so an answer is a labelled superset of that grep. The first line of the answer counts
//! the groups and says whether every line is accounted for; a few rows (unresolved lines first,
//! then references that are not calls, declarations and imports) follow at the end when it is
//! not.
//!
//! A line is accounted for only when it is positively one of: a resolved use, the target's
//! definition, an import under the same name, a declaration, another same-name definition or a
//! use resolved with certainty to one, or a comment or string literal as the file's tree-sitter
//! parse says. Anything else is unresolved. "All accounted for" also needs a complete scan and
//! a fresh index: every file naming it as indexed, no code file added or changed since that
//! names it, none too big to index, and no re-index or re-score pending.

use crate::project::Project;
use crate::tscode::{TSym, clip_around, word_at};
use anyhow::Result;
use rusqlite::{Connection, params};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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

/// A line's group.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Cat {
    /// May use the target: not resolved, guessed elsewhere, a weak guess, a renamed import, or a
    /// line the parse could not settle.
    Unresolved,
    /// Not parsed (past the parse budget): a comment, a string or a use; never accounted for.
    Unparsed,
    /// A reference to the target that is not a call (`callers` only).
    NotCall,
    /// A C/C++ prototype, `extern`, a forward declaration, a TypeScript `declare`, a Python
    /// `@overload` stub of the target.
    Decl,
    Import,
    /// The definition line of another same-name symbol, or a use resolved with certainty to one.
    Same,
    /// Only in comments (Python docstrings included).
    Comment,
    /// Only in string literals: may be a dynamic use (`getattr(x, "plan")`).
    Str,
    /// The target's own definition line.
    Definition,
}

/// Groups that list rows, in order, with the rows each keeps before the rest is shared out.
const LISTED: [(Cat, usize); 4] = [(Cat::Unresolved, 8), (Cat::NotCall, 4), (Cat::Decl, 4), (Cat::Import, 4)];
/// Most rows an answer lists, at any budget; the rest are counted.
const ROWS_MAX: usize = 20;

/// Most files, bytes and time one accounting reads; past any, the scan stops and says so.
const FILES_MAX: usize = 5000;
const BYTES_MAX: u64 = 200 << 20;
const TIME_MAX: Duration = Duration::from_millis(2000);
/// Most source bytes one accounting parses to tell comments and strings from code; past it,
/// lines that need a parse are counted as not parsed.
const PARSE_MAX: u64 = 16 << 20;
/// Most entries the freshness walk takes before it gives up proving freshness.
const WALK_MAX: usize = 400_000;

/// A line of a file: (path, line).
pub type Line = (String, i64);
/// A reference on a line: (kind, target, ambiguity).
type RefAt = (String, i64, i64);

/// How fresh the index is for this name.
#[derive(Clone, Default)]
struct Fresh {
    /// Files naming it whose content differs from what was indexed, gone, or added since.
    stale: Vec<String>,
    /// Files naming it too big to index.
    too_big: Vec<String>,
    /// A re-index or re-score is pending, or files await the upgrade's re-parse.
    pending: bool,
    /// The project's files could not all be checked.
    unproven: bool,
}

/// The accounting of one answer: counts, and the few rows it lists.
pub struct Ledger {
    name: String,
    /// The code the verdict covers (`indexed Python code`).
    scope: String,
    mode: Mode,
    resolved: usize,
    counts: BTreeMap<Cat, usize>,
    same_defs: usize,
    /// (group, rows listed), in listing order.
    rows: Vec<(Cat, Vec<String>)>,
    /// Lines of listed groups not shown.
    more: usize,
    fresh: Fresh,
    /// (files read, files naming it) when the scan stopped at a cap.
    capped: Option<(usize, usize)>,
    /// Resolved lines whose use of the target is only a guess by name: they keep the verdict
    /// from reading complete.
    guessed: usize,
    /// (file, size, mtime) of every file the scan read, and the changed-files listing it used:
    /// a cached ledger is rebuilt when either moved.
    seen: Vec<(String, u64, i64)>,
    changed_sig: u64,
    /// When `seen` was last checked against the disk.
    checked: Mutex<Instant>,
}

/// Same-name definition.
struct Def {
    id: i64,
    line: i64,
    decl: bool,
    kind: String,
    qname: String,
}

type Key = (String, i64, i64, String, i64, Mode);
/// Recently built ledgers, most recent last.
static CACHE: Mutex<Vec<(Key, Arc<Ledger>)>> = Mutex::new(Vec::new());
const CACHE_MAX: usize = 32;

/// The scan's own threads: a few, so an answer never takes every core.
fn pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let n = std::thread::available_parallelism().map_or(1, |n| n.get());
        rayon::ThreadPoolBuilder::new().num_threads((n / 4).clamp(1, 4)).build().expect("thread pool")
    })
}

/// Account for every line naming `name`. `resolved` holds the lines the answer already lists
/// (its calls, refs or d1 uses), `top` the target (None: no definition), `linked` same-name
/// definitions the answer lists too (implementations at d1). Built once per store generation.
pub fn account(
    c: &Connection,
    p: &Project,
    name: &str,
    top: Option<&TSym>,
    resolved: &HashSet<Line>,
    linked: &[i64],
    mode: Mode,
) -> Result<Arc<Ledger>> {
    let key: Key =
        (c.path().unwrap_or_default().to_string(), crate::db::generation(c), p.id, name.to_string(), top.map_or(-1, |t| t.id), mode);
    {
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = cache.iter().position(|(k, _)| *k == key) {
            let hit = cache.remove(i);
            let l = hit.1.clone();
            drop(cache);
            if l.still_fresh(c, p) {
                let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                cache.push(hit);
                return Ok(l);
            }
        }
    }
    let l = Arc::new(build(c, p, name, top, resolved, linked, mode)?);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= CACHE_MAX {
        cache.remove(0);
    }
    cache.push((key, l.clone()));
    Ok(l)
}

/// A line's group, its row (sort rank, text) when it lists one, and whether it is another
/// same-name definition's own line.
type FileLine = (Cat, Option<(u8, String)>, bool);

/// One file's lines: (group, row) per line naming it, and the uses of `linked` definitions.
#[derive(Default)]
struct FileOut {
    lines: Vec<FileLine>,
    linked: usize,
    /// Lines the answer resolves that hold another, unsettled occurrence: counted in their
    /// group instead.
    demoted: usize,
    /// Lines the answer resolves only through a guess by name.
    guessed: usize,
    /// Its content differs from what was indexed.
    stale: bool,
}

/// What the scan of every file shares.
struct Ctx<'a> {
    /// Source bytes parsed so far, and the most that may be.
    parsed: &'a AtomicU64,
    parse_max: u64,
    name: &'a str,
    top: Option<&'a TSym>,
    top_id: i64,
    mode: Mode,
    resolved: &'a HashSet<Line>,
    linked: &'a [i64],
    /// C: lines storing a function in the target member (`.read_folio = foo,`), certainly.
    stores: &'a HashSet<Line>,
    defs: &'a HashMap<String, Vec<Def>>,
    refs: &'a HashMap<Line, Vec<RefAt>>,
    /// Indexed (size, mtime, hash) of each file.
    indexed: &'a HashMap<String, (i64, i64, String)>,
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
        scope: scope(top.and_then(|t| family(&t.path))),
        mode,
        resolved: resolved.len(),
        counts: BTreeMap::new(),
        same_defs: 0,
        rows: vec![],
        more: 0,
        fresh: Fresh::default(),
        capped: None,
        guessed: 0,
        seen: vec![],
        changed_sig: 0,
        checked: Mutex::new(Instant::now()),
    };
    if name.is_empty() || !name.chars().all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$') {
        return Ok(l);
    }
    let pid = p.id;
    let t_all = Instant::now();
    let fam = top.and_then(|t| family(&t.path));
    let in_fam = |f: &str| fam.is_none_or(|a| family(f).is_some_and(|b| crate::lang::interop(a, b)));
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
    // C: a function stored in the target member names it on that line (`.read_folio = foo,`):
    // a reference to the member, not a call; `refs` and `impact` count it as resolved.
    let mut stores: HashSet<Line> = HashSet::new();
    if let Some(t) = top.filter(|t| t.kind == "method" && crate::lang::for_path(&t.path).is_some_and(|l| l.family == "c")) {
        let mut q = c.prepare_cached("SELECT path, line FROM ts_ref INDEXED BY ts_ref_slot WHERE slot != 0 AND slot = ?1")?;
        for k in q.query_map([t.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.flatten() {
            refs.entry(k.clone()).or_default().push(("ref".to_string(), t.id, 0));
            stores.insert(k);
        }
    }
    let pending = crate::db::reindex_pending(c, pid) || crate::db::rescore_pending(c, pid);
    let mut files: HashSet<String> = if crate::db::reindex_pending(c, pid) {
        crate::tscode::body_paths(c, pid, name)?.into_iter().collect()
    } else {
        let mut f = c.prepare_cached("SELECT path FROM fts_word WHERE fts_word MATCH ?1 AND project_id = ?2 LIMIT 50000")?;
        f.query_map(params![format!("words:\"{}\"", crate::tsindex::case_word(name)), pid], |r| r.get::<_, String>(0))?.flatten().collect()
    };
    files.extend(defs.keys().cloned());
    files.extend(refs.keys().map(|k| k.0.clone()));
    files.extend(resolved.iter().map(|k| k.0.clone()));
    let mut files: Vec<String> = files.into_iter().filter(|f| in_fam(f)).collect();
    files.sort();
    crate::tsindex::trace("account: names", t_all);
    let indexed = indexed_files(c, pid)?;
    let total = files.len();
    let parsed = AtomicU64::new(0);
    // A reference the answer does not resolve already makes it incomplete: telling comments and
    // strings from code then only splits counts, and is skipped.
    let top_id = top.map_or(-1, |t| t.id);
    let open_ref = refs.iter().any(|((path, line), rs)| {
        !resolved.contains(&(path.clone(), *line))
            && rs.iter().any(|r| r.0 != "import" && r.1 != top_id && !(r.1 != 0 && r.2 <= 0))
            && in_fam(path)
    });
    let parse_max = if open_ref { 0 } else { PARSE_MAX };
    let cx = Ctx {
        parsed: &parsed,
        parse_max,
        name,
        top,
        top_id: top.map_or(-1, |t| t.id),
        mode,
        resolved,
        linked,
        stores: &stores,
        defs: &defs,
        refs: &refs,
        indexed: &indexed,
    };
    let t0 = Instant::now();
    let bytes = AtomicU64::new(0);
    crate::tsindex::trace("account: indexed files", t_all);
    let outs: Vec<Option<FileOut>> = pool().install(|| {
        use rayon::prelude::*;
        files
            .par_iter()
            .enumerate()
            .map(|(i, path)| {
                if i >= FILES_MAX || t0.elapsed() > TIME_MAX || bytes.load(Ordering::Relaxed) > BYTES_MAX {
                    return None;
                }
                let Ok(raw) = std::fs::read(p.root.join(path)) else {
                    // Gone since it was indexed.
                    return Some(FileOut { stale: true, ..Default::default() });
                };
                bytes.fetch_add(raw.len() as u64, Ordering::Relaxed);
                Some(scan_file(&cx, p, path, &raw))
            })
            .collect()
    });
    let read = outs.iter().filter(|o| o.is_some()).count();
    if read < total {
        l.capped = Some((read, total));
    }
    let mut rows: BTreeMap<Cat, Vec<(u8, String, i64, String)>> = BTreeMap::new();
    for (path, out) in files.iter().zip(outs) {
        let Some(out) = out else { continue };
        if out.stale {
            l.fresh.stale.push(path.clone());
        }
        l.resolved += out.linked;
        l.resolved -= out.demoted.min(l.resolved);
        l.guessed += out.guessed;
        for (n, (cat, row, same_def)) in out.lines.into_iter().enumerate() {
            *l.counts.entry(cat).or_default() += 1;
            l.same_defs += same_def as usize;
            if let Some((rank, text)) = row {
                rows.entry(cat).or_default().push((rank, path.clone(), n as i64, text));
            }
        }
    }
    for v in rows.values_mut() {
        v.sort();
    }
    // A few rows: each listed group up to its share, then the rest of the room in order.
    let mut keep: Vec<usize> = LISTED.iter().map(|(c, k)| rows.get(c).map_or(0, |v| v.len().min(*k))).collect();
    let mut room = ROWS_MAX.saturating_sub(keep.iter().sum());
    for (i, (c, _)) in LISTED.iter().enumerate() {
        let have = rows.get(c).map_or(0, Vec::len);
        let add = have.saturating_sub(keep[i]).min(room);
        keep[i] += add;
        room -= add;
    }
    for (i, (c, _)) in LISTED.iter().enumerate() {
        let Some(v) = rows.get(c) else { continue };
        l.more += l.counts.get(c).copied().unwrap_or(0) - keep[i];
        if keep[i] > 0 {
            l.rows.push((*c, v[..keep[i]].iter().map(|r| r.3.clone()).collect()));
        }
    }
    l.fresh.pending = pending || files.iter().any(|f| indexed.get(f).is_some_and(|x| x.0 < 0 || x.2.is_empty()));
    crate::tsindex::trace("account: scan", t_all);
    freshness(c, p, name, fam, &indexed, &files, &mut l.fresh);
    l.changed_sig = sig(&changed(c, p, &indexed));
    l.seen = pool().install(|| {
        use rayon::prelude::*;
        files
            .par_iter()
            .filter_map(|f| std::fs::metadata(p.root.join(f)).ok().map(|md| (f.clone(), md.len(), crate::inventory::mtime_ms(&md))))
            .collect()
    });
    crate::tsindex::trace("account: freshness", t_all);
    l.fresh.stale.sort();
    l.fresh.stale.dedup();
    Ok(l)
}

/// Every indexed code file: path -> (size, mtime, hash).
fn indexed_files(c: &Connection, pid: i64) -> Result<HashMap<String, (i64, i64, String)>> {
    let mut q = c.prepare_cached("SELECT path, size, mtime, hash FROM ts_file WHERE project_id = ?1")?;
    let v = q
        .query_map([pid], |r| Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?))))?
        .flatten()
        .collect();
    Ok(v)
}

/// What a verdict covers: the indexed code of the target's language family. Other languages
/// (templates, FFI), files the index skips and calls under another spelling are out of scope.
fn scope(fam: Option<&str>) -> String {
    let what = match fam {
        Some("python") => "Python",
        Some("js") => "JS/TS",
        Some("go") => "Go",
        Some("c") | Some("swift") => "C/C++/Objective-C/Swift",
        Some("jvm") => "Java/Kotlin/Scala",
        Some("rust") => "Rust",
        Some("dotnet") => "C#",
        Some("ruby") => "Ruby",
        Some("php") => "PHP",
        Some("bash") => "shell",
        Some("dart") => "Dart",
        Some("lua") => "Lua",
        _ => return "indexed code".into(),
    };
    format!("indexed {what} code")
}

/// The interop family of a file's language.
fn family(path: &str) -> Option<&'static str> {
    crate::lang::for_path(path).map(|l| l.family)
}

/// Files changed or added since indexing, per (store, project, generation): every indexed code
/// file whose size or time moved, and every code file the index does not know in a directory
/// created or changed since then (found by a walk with the index's ignore rules that enters only
/// those directories and the ones above them). None when the walk ran past [`WALK_MAX`].
type Changed = Arc<Option<Vec<(String, u64)>>>;
/// (store, project, generation).
type WalkKey = (String, i64, i64);
static WALKS: Mutex<Vec<(WalkKey, Changed, Instant)>> = Mutex::new(Vec::new());

fn changed(c: &Connection, p: &Project, indexed: &HashMap<String, (i64, i64, String)>) -> Changed {
    let key = (c.path().unwrap_or_default().to_string(), p.id, crate::db::generation(c));
    if let Some(w) = WALKS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(k, _, at)| *k == key && at.elapsed() < RECHECK) {
        return w.1.clone();
    }
    let v = Arc::new(changed_now(c, p, indexed));
    let mut walks = WALKS.lock().unwrap_or_else(|e| e.into_inner());
    walks.retain(|(k, ..)| k.0 != key.0 || k.1 != key.1);
    if walks.len() >= 4 {
        walks.remove(0);
    }
    walks.push((key, v.clone(), Instant::now()));
    v
}

fn changed_now(c: &Connection, p: &Project, indexed: &HashMap<String, (i64, i64, String)>) -> Option<Vec<(String, u64)>> {
    // When the index last looked at the tree (ms), less a margin for coarse directory times.
    let since: i64 = c
        .query_row("SELECT COALESCE(MAX(updated_at), 0) FROM node WHERE project_id = ?1 AND kind = 'file'", [p.id], |r| r.get(0))
        .unwrap_or(0)
        * 1000
        - 2000;
    let mut q = c.prepare_cached("SELECT path FROM node WHERE project_id = ?1 AND kind = 'file'").ok()?;
    let files: Vec<String> = q.query_map([p.id], |r| r.get::<_, String>(0)).ok()?.flatten().collect();
    let known: HashSet<&str> = files.iter().map(String::as_str).collect();
    let mut dirs: HashSet<String> = HashSet::from([String::new()]);
    for f in &files {
        let mut d = f.as_str();
        while let Some((up, _)) = d.rsplit_once('/') {
            if !dirs.insert(up.to_string()) {
                break;
            }
            d = up;
        }
    }
    let dirs: Vec<String> = dirs.into_iter().collect();
    let root = p.root.clone();
    let (moved_dirs, mut out): (Vec<String>, Vec<(String, u64)>) = pool().install(|| {
        use rayon::prelude::*;
        let moved: Vec<String> = dirs
            .par_iter()
            .filter(|d| std::fs::metadata(root.join(d.as_str())).is_ok_and(|md| crate::inventory::mtime_ms(&md) >= since))
            .cloned()
            .collect();
        let edited: Vec<(String, u64)> = indexed
            .par_iter()
            .filter_map(|(f, (size, mtime, _))| {
                let md = std::fs::metadata(root.join(f)).ok()?;
                (md.len() as i64 != *size || crate::inventory::mtime_ms(&md) != *mtime).then(|| (f.clone(), md.len()))
            })
            .collect();
        (moved, edited)
    });
    // An ignore rule that changed may have let in files the index never saw: walk everything.
    let ov_path = crate::db::home().join("projects").join(format!("{}.toml", p.name));
    let mut sources: Vec<std::path::PathBuf> = vec![ov_path, p.root.join(".git/info/exclude")];
    sources.extend(ignore::gitignore::gitconfig_excludes_path());
    let ignores_moved = sources
        .iter()
        .chain(dirs.iter().flat_map(|d| [root.join(d).join(".gitignore"), root.join(d).join(".ignore")]).collect::<Vec<_>>().iter())
        .any(|f| std::fs::metadata(f).is_ok_and(|md| crate::inventory::mtime_ms(&md) >= since));
    if ignores_moved {
        let ov = Arc::new(crate::inventory::overrides_for(&p.name));
        let w = crate::inventory::walk(&p.root, &ov, None);
        if w.entries.len() > WALK_MAX {
            return None;
        }
        out.extend(w.entries.into_iter().filter(|e| !known.contains(e.rel.as_str()) && family(&e.rel).is_some()).map(|e| (e.rel, e.size)));
    } else if !moved_dirs.is_empty() {
        let known_dirs: HashSet<String> = dirs.iter().cloned().collect();
        let enter: HashSet<String> = moved_dirs
            .iter()
            .flat_map(|d| {
                let mut v = vec![d.clone()];
                let mut x = d.as_str();
                while let Some((up, _)) = x.rsplit_once('/') {
                    v.push(up.to_string());
                    x = up;
                }
                v
            })
            .collect();
        let keep: crate::inventory::Keep = Arc::new(move |rel: &str| !known_dirs.contains(rel) || enter.contains(rel));
        let ov = Arc::new(crate::inventory::overrides_for(&p.name));
        let w = crate::inventory::walk(&p.root, &ov, Some(keep));
        if w.entries.len() > WALK_MAX {
            return None;
        }
        out.extend(w.entries.into_iter().filter(|e| !known.contains(e.rel.as_str()) && family(&e.rel).is_some()).map(|e| (e.rel, e.size)));
    }
    Some(out)
}

/// Files of the family that the scan did not read but that name `name` now: added since
/// indexing, changed in place, or too big to index.
fn freshness(
    c: &Connection,
    p: &Project,
    name: &str,
    fam: Option<&str>,
    indexed: &HashMap<String, (i64, i64, String)>,
    scanned: &[String],
    out: &mut Fresh,
) {
    let in_fam = |f: &str| fam.is_none_or(|a| family(f).is_some_and(|b| crate::lang::interop(a, b)));
    // Code files too big to index that name it.
    if let Ok(mut q) = c.prepare_cached("SELECT path FROM node WHERE project_id = ?1 AND kind = 'file' AND size > ?2") {
        let big: Vec<String> = q
            .query_map(params![p.id, crate::tsindex::MAX_FILE as i64], |r| r.get::<_, String>(0))
            .map(|r| r.flatten().collect())
            .unwrap_or_default();
        for f in big.into_iter().filter(|f| in_fam(f) && !crate::tiers::is_denied_path(f)) {
            if std::fs::read(p.root.join(&f)).is_ok_and(|raw| word_at(&String::from_utf8_lossy(&raw), name).is_some()) {
                out.too_big.push(f);
            }
        }
    }
    let ch = changed(c, p, indexed);
    let Some(entries) = ch.as_ref() else {
        out.unproven = true;
        return;
    };
    let scanned: HashSet<&str> = scanned.iter().map(String::as_str).collect();
    let todo: Vec<&(String, u64)> =
        entries.iter().filter(|(rel, _)| in_fam(rel) && !scanned.contains(rel.as_str()) && !crate::tiers::is_denied_path(rel)).collect();
    let found: Vec<(String, bool)> = pool().install(|| {
        use rayon::prelude::*;
        todo.par_iter()
            .filter_map(|(rel, size)| {
                let raw = std::fs::read(p.root.join(rel)).ok()?;
                word_at(&String::from_utf8_lossy(&raw), name)?;
                if *size > crate::tsindex::MAX_FILE {
                    return Some((rel.clone(), true));
                }
                // Changed in time only: the content decides.
                let same = indexed.get(rel).is_some_and(|x| crate::tsindex::content_hash(&raw) == x.2);
                (!same).then(|| (rel.clone(), false))
            })
            .collect()
    });
    for (f, big) in found {
        if big {
            if !out.too_big.contains(&f) {
                out.too_big.push(f);
            }
        } else {
            out.stale.push(f);
        }
    }
}

/// Each line of one file naming the name, grouped.
fn scan_file(cx: &Ctx, p: &Project, path: &str, raw: &[u8]) -> FileOut {
    let mut out = FileOut::default();
    let name = cx.name;
    let text = String::from_utf8_lossy(raw);
    // Freshness of a file the answer cites: size and time, then its content.
    if let Some((size, mtime, hash)) = cx.indexed.get(path)
        && *size >= 0
        && !hash.is_empty()
    {
        let md = std::fs::metadata(p.root.join(path)).ok();
        let moved = md.is_none_or(|md| md.len() as i64 != *size || crate::inventory::mtime_ms(&md) != *mtime);
        out.stale = moved && crate::tsindex::content_hash(raw) != *hash;
    } else if !cx.indexed.contains_key(path) {
        out.stale = true;
    }
    // Occurrences as a whole word, by line.
    let starts: Vec<usize> = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
    let bytes = text.as_bytes();
    let w = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b >= 0x80);
    let mut hits: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, _) in text.match_indices(name) {
        if w(i.checked_sub(1).and_then(|k| bytes.get(k))) || w(bytes.get(i + name.len())) {
            continue;
        }
        hits.entry(starts.partition_point(|s| *s <= i) - 1).or_default().push(i);
    }
    if hits.is_empty() {
        return out;
    }
    // Where each occurrence sits in the parse: code, comment or string. Parsed only when a line
    // needs it (a line with a reference row is code); no parse reads as code.
    let lang = crate::lang::for_path(path);
    // (parse allowed, tree)
    let tree: std::cell::OnceCell<(bool, Option<tree_sitter::Tree>)> = std::cell::OnceCell::new();
    let place = |at: usize| -> Option<Place> {
        let t = tree.get_or_init(|| {
            if cx.parsed.fetch_add(text.len() as u64, Ordering::Relaxed) + text.len() as u64 > cx.parse_max {
                return (false, None);
            }
            let t = lang.and_then(|l| {
                let mut ps = tree_sitter::Parser::new();
                ps.set_language(&l.language()).ok()?;
                ps.parse(text.as_bytes(), None)
            });
            (true, t)
        });
        if !t.0 {
            return None;
        }
        Some(t.1.as_ref().map_or(Place::Code, |t| place_of(t, bytes, at, name.len(), lang.map_or("", |l| l.family))))
    };
    let line_text = |ln: usize| -> &str {
        let a = starts[ln];
        let b = starts.get(ln + 1).map_or(text.len(), |x| x - 1);
        text[a..b.max(a)].trim_end_matches('\r')
    };
    let mine = cx.defs.get(path).map(Vec::as_slice).unwrap_or(&[]);
    let mut def_at: HashMap<i64, &Def> = HashMap::new();
    for d in mine {
        if let Some(ln) = hits.keys().map(|h| *h as i64 + 1).find(|n| *n >= d.line && *n <= d.line + 5) {
            def_at.entry(ln).or_insert(d);
        }
    }
    let ext = path.rsplit_once('.').map(|x| x.1).unwrap_or("");
    let ty = cx.top.and_then(|t| t.parent.as_deref()).map(|p| p.rsplit('.').next().unwrap_or(p)).unwrap_or("");
    let stem = cx.top.map(|t| t.path.rsplit('/').next().unwrap_or(&t.path)).map(|f| f.split('.').next().unwrap_or(f)).unwrap_or("");
    let method = cx.top.is_some_and(|t| t.parent.as_deref().is_some_and(|p| !p.is_empty()));
    // Python methods may be called bare inside their class body: no skip there.
    let needs_receiver = method && matches!(family(path), Some("go" | "js" | "rust"));
    for (&ln, occ) in &hits {
        let n = ln as i64 + 1;
        let key = (path.to_string(), n);
        let store = cx.mode != Mode::Calls && cx.stores.contains(&key);
        let resolved_line = cx.resolved.contains(&key) || store;
        let rs: &[RefAt] = cx.refs.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        let def = def_at.get(&n).copied();
        let raw_line = line_text(ln);
        let row = |note: &str, rank: u8| -> Option<(u8, String)> {
            let code = clip_around(raw_line.trim(), name).unwrap_or_default();
            Some((rank, format!("  {path}:{n}{note} | {code}")))
        };
        // Each occurrence needs its own reference row or definition; occurrences beyond those
        // must be shown by the parse to sit in comments or strings.
        let covered = rs.len() + def.is_some() as usize;
        let places: Option<Vec<Place>> = if occ.len() > covered { occ.iter().map(|&i| place(i)).collect() } else { Some(vec![]) };
        let Some(places) = places else {
            out.lines.push((if covered == 0 { Cat::Unparsed } else { Cat::Unresolved }, row(" not parsed", 2), false));
            continue;
        };
        let code_occ = places.iter().filter(|p| **p == Place::Code).count();
        let all_bare = || needs_receiver && occ.iter().all(|&i| bare(bytes, i));
        if covered == 0 {
            let (cat, r) =
                if code_occ == 0 && family(path) == Some("js") && import_line(raw_line.trim()) && !places.contains(&Place::Comment) {
                    // `import doIt from './plan'`: the module named, its names bound under others.
                    (Cat::Unresolved, row(" imports the module under other names", 0))
                } else if code_occ == 0 {
                    if places.contains(&Place::Comment) { (Cat::Comment, None) } else { (Cat::Str, None) }
                } else if renamed_import(raw_line, name) {
                    (Cat::Unresolved, row(" renamed import", 0))
                } else if import_line(raw_line.trim()) {
                    (Cat::Import, row("", 0))
                } else if prototype(raw_line, name, ext) {
                    (Cat::Decl, row("", 0))
                } else if py_stub(raw_line, name, ext) {
                    if cx.top.is_some_and(|t| t.path == path) { (Cat::Decl, row("", 0)) } else { (Cat::Same, None) }
                } else if all_bare() {
                    continue;
                } else {
                    (Cat::Unresolved, row("", plausible(raw_line, name, ty, stem)))
                };
            out.lines.push((cat, r, false));
            continue;
        }
        // Each covering item's group; the line takes the least settled.
        let mut unresolved: Option<&str> = None;
        let mut not_call = false;
        let mut import = false;
        let mut other_ref = false;
        // A guess by name covers its occurrence for the listing, not for the verdict.
        if resolved_line && rs.iter().any(|r| r.1 == cx.top_id) && rs.iter().filter(|r| r.1 == cx.top_id).all(|r| r.2 > 0) {
            out.guessed += 1;
        }
        if code_occ > covered {
            unresolved = Some(" more uses on the line");
        }
        if renamed_import(raw_line, name) {
            unresolved = Some(" renamed import");
        }
        for r in rs {
            if r.1 == cx.top_id {
                if cx.mode == Mode::Calls && r.0 != "call" {
                    not_call = true;
                } else if resolved_line {
                    continue;
                } else {
                    unresolved = unresolved.or(Some(" weak guess"));
                }
            } else if r.0 == "import" {
                import = true;
            } else if r.1 != 0 && r.2 <= 0 {
                other_ref = true;
            } else if !(all_bare() && def.is_none()) {
                unresolved = unresolved.or(Some(if r.1 != 0 { " guessed elsewhere" } else { "" }));
            }
        }
        let def_cat = def.map(|d| {
            if cx.linked.contains(&d.id) {
                None
            } else if d.id == cx.top_id {
                Some(Cat::Definition)
            } else if d.decl || prototype(raw_line, name, ext) {
                Some(Cat::Decl)
            } else {
                Some(Cat::Same)
            }
        });
        // Another definition of the name on a line that also uses the target (`plan = core.plan`,
        // `fun plan() = inner.plan()`): an alias or wrapper, through which calls may reach it.
        if matches!(def_cat, Some(Some(Cat::Same | Cat::Decl))) && rs.iter().any(|r| r.1 == cx.top_id || r.1 == 0 || r.2 > 0) {
            unresolved = unresolved.or(Some(" alias or wrapper"));
        }
        if let Some(note) = unresolved {
            let rank = if note.is_empty() { plausible(raw_line, name, ty, stem) } else { 0 };
            out.lines.push((Cat::Unresolved, row(note, rank), false));
            out.demoted += resolved_line as usize;
            continue;
        }
        if not_call {
            out.lines.push((Cat::NotCall, row("", 0), false));
            out.demoted += resolved_line as usize;
            continue;
        }
        if matches!(def_cat, Some(None)) {
            out.linked += 1;
            continue;
        }
        if resolved_line {
            // A store is resolved without being one of the answer's rows: counted here.
            out.linked += (store && !cx.resolved.contains(&key)) as usize;
            continue;
        }
        let (cat, r, same_def) = if not_call {
            (Cat::NotCall, row("", 0), false)
        } else if let Some(Some(c)) = def_cat {
            match c {
                Cat::Decl => {
                    let d = def.expect("a definition");
                    let code = clip_around(raw_line.trim(), name).unwrap_or_default();
                    (Cat::Decl, Some((0, format!("  {path}:{n} {} {} c:{:x} | {code}", d.kind, d.qname, d.id))), false)
                }
                Cat::Same => (Cat::Same, None, true),
                c => (c, None, false),
            }
        } else if import {
            (Cat::Import, row("", 0), false)
        } else if other_ref {
            (Cat::Same, None, false)
        } else {
            continue;
        };
        out.lines.push((cat, r, same_def));
    }
    out
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Place {
    Code,
    Comment,
    Str,
}

/// Where the occurrence at byte `at` (length `len`) sits: inside a comment, inside a string
/// literal (outside any interpolation), or code. Text the parse could not read is code.
fn place_of(tree: &tree_sitter::Tree, src: &[u8], at: usize, len: usize, fam: &str) -> Place {
    let p = place_in(tree, at, len, fam);
    // `"$name"` (Kotlin, Groovy, Scala, Dart, PHP, shell): a template, whatever the parse.
    let dollar = at > 0 && src.get(at - 1) == Some(&b'$') && matches!(fam, "jvm" | "dart" | "php" | "bash");
    if p == Place::Str && dollar { Place::Code } else { p }
}

fn place_in(tree: &tree_sitter::Tree, at: usize, len: usize, fam: &str) -> Place {
    let Some(mut n) = tree.root_node().descendant_for_byte_range(at, at + len) else { return Place::Code };
    loop {
        let k = n.kind();
        if n.is_error() || n.is_missing() {
            return Place::Code;
        }
        if k.contains("comment") {
            return Place::Comment;
        }
        if k.contains("interpolation")
            || k.contains("interpolated")
            || k.contains("substitution")
            || k == "embedded_expression"
            || k == "template_expression"
            || k == "jsx_expression"
        {
            return Place::Code;
        }
        if stringy(k) {
            // The whole literal (`string` around its `string_content`).
            let mut top = n;
            while let Some(p) = top.parent().filter(|p| stringy(p.kind())) {
                top = p;
            }
            // A Python docstring is documentation.
            if fam == "python" && top.parent().is_some_and(|p| p.kind() == "expression_statement" && p.named_child_count() == 1) {
                return Place::Comment;
            }
            return Place::Str;
        }
        match n.parent() {
            Some(p) => n = p,
            None => return Place::Code,
        }
    }
}

/// A node kind holding literal text: strings, heredocs, regexes, JSX text, char literals.
fn stringy(k: &str) -> bool {
    k.contains("string") || k.contains("heredoc") || k == "regex" || k == "regex_pattern" || k == "jsx_text" || k.ends_with("char_literal")
}

/// The occurrence at `i` is a bare call (`name(` with nothing before it but an operator or a
/// keyword): no receiver.
fn bare(b: &[u8], i: usize) -> bool {
    let before = b[..i].iter().rev().find(|c| !c.is_ascii_whitespace());
    let called = b[i..].iter().skip_while(|c| c.is_ascii_alphanumeric() || **c == b'_').find(|c| !c.is_ascii_whitespace()) == Some(&b'(');
    called && !matches!(before, Some(b'.' | b':' | b'>' | b'@'))
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
/// (`struct name;`), a TypeScript `declare`.
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

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Least time between two disk checks of a cached ledger or listing.
const RECHECK: Duration = Duration::from_secs(1);

/// A signature of a changed-files listing.
fn sig(ch: &Changed) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ch.as_ref().hash(&mut h);
    h.finish()
}

impl Ledger {
    /// Whether nothing the ledger read moved since: at most once per [`RECHECK`], every file
    /// it read is stat'ed and the changed-files listing compared.
    fn still_fresh(&self, c: &Connection, p: &Project) -> bool {
        let mut checked = self.checked.lock().unwrap_or_else(|e| e.into_inner());
        if checked.elapsed() < RECHECK {
            return true;
        }
        let same = pool().install(|| {
            use rayon::prelude::*;
            self.seen.par_iter().all(|(f, size, mtime)| {
                std::fs::metadata(p.root.join(f)).is_ok_and(|md| md.len() == *size && crate::inventory::mtime_ms(&md) == *mtime)
            })
        });
        let indexed = indexed_files(c, p.id).unwrap_or_default();
        let fresh = same && sig(&changed(c, p, &indexed)) == self.changed_sig;
        *checked = Instant::now();
        fresh
    }

    fn n(&self, c: Cat) -> usize {
        self.counts.get(&c).copied().unwrap_or(0)
    }

    /// The accounting line that leads the answer (with `guessed` resolved uses guessed by name);
    /// None when nothing names it.
    pub fn head(&self, guessed: usize) -> Option<String> {
        let name = &self.name;
        let total = self.resolved + self.counts.values().sum::<usize>();
        if total == 0 && self.capped.is_none() && self.fresh.stale.is_empty() && self.fresh.too_big.is_empty() {
            return None;
        }
        let mut parts = vec![];
        let g = if guessed > 0 { format!(" ({guessed} guessed)") } else { String::new() };
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
        let u = self.n(Cat::Unresolved);
        if u > 0 {
            parts.push(format!("{u} unresolved"));
        }
        let np = self.n(Cat::Unparsed);
        if np > 0 {
            parts.push(format!("{np} not parsed (comments, strings or uses)"));
        }
        let f = &self.fresh;
        let incomplete = if self.mode == Mode::Impact { "partial at d1" } else { "may be incomplete" };
        let status = if !f.stale.is_empty() {
            let k = f.stale.len();
            let some: Vec<&str> = f.stale.iter().take(3).map(String::as_str).collect();
            format!(
                "the index is stale for {} ({}{}): counts may be off; reindex (fox index) first",
                plural(k, "file", "files"),
                some.join(", "),
                if k > 3 { ", ..." } else { "" }
            )
        } else if f.pending {
            format!("{incomplete}: the index is being updated (a re-index or re-score is pending)")
        } else if let Some((read, of)) = self.capped {
            format!("{incomplete}: counted in {read} of the {of} files naming it (the scan stops at a size and time cap)")
        } else if !f.too_big.is_empty() {
            format!("{incomplete}: {} too big to index name it ({})", plural(f.too_big.len(), "file", "files"), f.too_big[0])
        } else if f.unproven {
            format!("{incomplete}: the project is too big to prove no file changed since indexing")
        } else if u + np > 0 {
            let u = u + np;
            match self.mode {
                Mode::Name => format!("no definition of `{name}` is indexed; its lines are listed below"),
                _ => format!("{incomplete}: {} may use it", plural(u, "unresolved line", "unresolved lines")),
            }
        } else if self.n(Cat::NotCall) > 0 {
            let k = self.n(Cat::NotCall);
            format!(
                "may be incomplete: {} it as a value, and calls through {} are not followed (rel=impact lists the code passing it)",
                plural(k, "line passes", "lines pass"),
                if k == 1 { "it" } else { "them" }
            )
        } else if self.guessed > 0 {
            let k = self.guessed;
            format!(
                "{incomplete}: {} the target only through a guess by name (marked guess); confirm {}",
                plural(k, "line reaches", "lines reach"),
                if k == 1 { "it" } else { "them" }
            )
        } else {
            format!("all accounted for in {}", self.scope)
        };
        let total = plural(total, "line names", "lines name");
        Some(format!("{total} `{name}`: {}; {status}", parts.join(", ")))
    }

    fn complete(&self) -> bool {
        self.head(0).is_some_and(|h| h.ends_with(&format!("all accounted for in {}", self.scope)))
    }

    /// The few listed rows for the end of the answer: none when every line is accounted for.
    pub fn tail(&self) -> Vec<String> {
        if self.complete() {
            return vec![];
        }
        let mut out = vec![];
        for (c, v) in &self.rows {
            out.push(
                match c {
                    Cat::Unresolved => "unresolved mentions:",
                    Cat::NotCall => "references, not calls (what is called through them is not followed):",
                    Cat::Decl => "declarations and prototypes:",
                    _ => "import lines:",
                }
                .to_string(),
            );
            out.extend(v.iter().cloned());
        }
        if self.more > 0 {
            out.push(format!("{} counted, not listed", plural(self.more, "more line is", "more lines are")));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{Place, place_of, prototype};

    fn places(src: &str, ext: &str, name: &str) -> Vec<Place> {
        let l = crate::lang::for_path(&format!("x.{ext}")).unwrap();
        let mut ps = tree_sitter::Parser::new();
        ps.set_language(&l.language()).unwrap();
        let t = ps.parse(src, None).unwrap();
        src.match_indices(name).map(|(i, _)| place_of(&t, src.as_bytes(), i, name.len(), l.family)).collect()
    }

    /// Comments and strings come from the parse; interpolations are code.
    #[test]
    fn places_come_from_the_parse() {
        use Place::*;
        assert_eq!(places("x = 1  # plan\ny = \"plan\"\nz = f\"{plan()}\"\nplan()\n", "py", "plan"), [Comment, Str, Code, Code]);
        assert_eq!(places("def f():\n    \"\"\"plan it\"\"\"\n    return 1\n", "py", "plan"), [Comment]);
        assert_eq!(places("a = fr\"{o.plan(1)}\"\nb = rf\"\"\"{o.plan(2)}\"\"\"\n", "py", "plan"), [Code, Code]);
        assert_eq!(places("const a = `x ${ok ? `y ${b}` : ''} z`;\nobj.plan(1);\nconst f = obj[\"plan\"];\n", "js", "plan"), [Code, Str]);
        assert_eq!(places("const Q = /[`'\"]/;\nexport const h = plan;\n", "js", "plan"), [Code]);
        assert_eq!(places("function A(o) { return <p><b>a</b>{o.plan(1)}<i>b</i></p>; }\n", "jsx", "plan"), [Code]);
        assert_eq!(places("fn main() { let u = \"a\n src/*.rs\n\"; plan(); let p = r\"C:\\\"; plan(); }\n", "rs", "plan"), [Code, Code]);
        assert_eq!(places("val h = \"\"\"\n src/*.kt\n\"\"\"\nfun f() = plan()\n", "kt", "plan"), [Code]);
        assert_eq!(places("void go(dynamic o) { print('v ${o.plan(1)}'); }\n", "dart", "plan"), [Code]);
        assert_eq!(places("<?php\n$d = <<<EOT\nsrc/*.php\nEOT;\n$o->plan(1); // plan\n", "php", "plan"), [Code, Comment]);
        assert_eq!(places("s = \"line one\n#{o.plan(1)} two\"\n", "rb", "plan"), [Code]);
        assert_eq!(places("val plan = 3\nfun go(): String {\n    return \"v $plan\"\n}\n", "kt", "plan"), [Code, Code]);
        assert_eq!(places("let plan = 3\nfunc go() -> String {\n    return \"v \\(plan)\"\n}\n", "swift", "plan"), [Code, Code]);
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
