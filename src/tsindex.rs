//! Native tree-sitter code index (replaces the external codanna binary).
//!
//! Each code file is parsed with its grammar and the language's tag query (`languages/*.scm`):
//! definitions become `symbol` nodes (`source = 'treesitter'`, handle `c:`), references land in
//! `ts_ref` and are resolved by name with scope and import awareness into `dst` plus `CALLS` /
//! `REFERENCES` edges. Files are re-parsed only when their size/mtime moved *and* their content
//! hash changed; parsing runs in parallel (rayon), writes in one transaction.

use crate::db;
use crate::lang::{self, Lang};
use crate::project::Project;
use anyhow::Result;
use rayon::prelude::*;
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, QueryCursor};

pub const MAX_FILE: u64 = 2 << 20;

#[derive(Debug, Clone, PartialEq)]
pub struct Def {
    pub kind: String,
    pub name: String,
    pub line: u32,
    pub end_line: u32,
    start: usize,
    end: usize,
    /// Index of the enclosing definition in `Parsed::defs`.
    pub container: Option<usize>,
    /// Explicit container name from a `@container` capture (Go receivers, C++ `A::f`).
    pub container_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ref {
    /// call | ref | import
    pub kind: &'static str,
    pub name: String,
    pub line: u32,
    /// Innermost definition containing the reference.
    pub caller: Option<usize>,
    /// Called through a receiver (`x.f()`), not bare (`f()`).
    pub recv: bool,
    pos: usize,
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
}

impl Parsed {
    /// Display name of a definition's container (`Outer.Inner`).
    pub fn container_of(&self, i: usize) -> Option<String> {
        let d = &self.defs[i];
        if let Some(n) = &d.container_name {
            return Some(n.clone());
        }
        let mut names = vec![];
        let mut cur = d.container;
        while let Some(c) = cur {
            names.push(self.defs[c].name.clone());
            cur = self.defs[c].container;
        }
        if names.is_empty() {
            return None;
        }
        names.reverse();
        Some(names.join("."))
    }
}

pub fn is_type_kind(k: &str) -> bool {
    matches!(k, "class" | "interface" | "object" | "struct" | "enum" | "module" | "trait" | "type" | "protocol")
}

pub fn is_callable_kind(k: &str) -> bool {
    matches!(k, "function" | "method" | "macro" | "constructor")
}

fn ref_kind(suffix: &str) -> &'static str {
    match suffix {
        "call" | "send" => "call",
        "import" => "import",
        _ => "ref",
    }
}

/// Is this call name reached through a receiver expression (`x.f`, `x->f`, `[x f]`, `x::f`)?
fn has_receiver(n: tree_sitter::Node) -> bool {
    let Some(p) = n.parent() else { return false };
    let k = p.kind();
    if ["navigation", "member", "field", "selector", "attribute", "scoped", "qualified"].iter().any(|w| k.contains(w)) {
        return p.named_child(0).is_some_and(|c| c.id() != n.id());
    }
    ["object", "receiver", "scope"].iter().any(|f| p.child_by_field_name(f).is_some_and(|c| c.id() != n.id()))
}

/// Parse one file and extract definitions and references.
pub fn extract(l: &Lang, src: &[u8]) -> Parsed {
    let mut out = Parsed::default();
    let Some(q) = l.query() else { return out };
    let mut parser = Parser::new();
    if parser.set_language(&l.language()).is_err() {
        return out;
    }
    let Some(tree) = parser.parse(src, None) else { return out };
    let names = q.capture_names();
    let mut cur = QueryCursor::new();
    let mut it = cur.matches(q, tree.root_node(), src);
    let text = |n: tree_sitter::Node| n.utf8_text(src).unwrap_or("").to_string();
    while let Some(m) = it.next() {
        let mut name_node = None;
        let mut outer: Option<(tree_sitter::Node, &str)> = None;
        let mut container = None;
        for cap in m.captures() {
            let cn = names[cap.index as usize];
            if cn == "name" {
                name_node = Some(cap.node);
            } else if cn == "container" || cn == "local.scope" {
                container = Some(text(cap.node));
            } else if cn.starts_with("definition.") || cn.starts_with("reference.") {
                outer = Some((cap.node, cn));
            }
        }
        let Some((node, cn)) = outer else { continue };
        let name = match name_node {
            Some(n) => text(n),
            None if l.name == "kotlin" && node.kind() == "companion_object" => "Companion".to_string(),
            None => continue,
        };
        let name = name.trim().trim_matches('"').to_string();
        if name.is_empty() || name.len() > 200 {
            continue;
        }
        let nn = name_node.unwrap_or(node);
        if let Some(k) = cn.strip_prefix("definition.") {
            let mut kind = k.to_string();
            if kind == "class" {
                // Grammars that share one node for class/interface/enum (Kotlin, Swift).
                let mut w = node.walk();
                for ch in node.children(&mut w) {
                    if !ch.is_named() && matches!(ch.kind(), "interface" | "enum" | "struct" | "protocol" | "extension") {
                        kind = ch.kind().to_string();
                        break;
                    }
                }
            }
            // Grammars that keep a body beside its signature (Dart): extend over the body.
            let mut last = node;
            if let Some(b) = node.next_named_sibling()
                && matches!(b.kind(), "function_body" | "block")
                && node.parent().is_some_and(|p| p.kind() != "function_definition")
            {
                last = b;
            }
            out.defs.push(Def {
                kind,
                name,
                line: nn.start_position().row as u32 + 1,
                end_line: last.end_position().row as u32 + 1,
                start: node.start_byte(),
                end: last.end_byte(),
                container: None,
                container_name: container,
            });
        } else if let Some(k) = cn.strip_prefix("reference.") {
            out.refs.push(Ref {
                kind: ref_kind(k),
                name,
                line: nn.start_position().row as u32 + 1,
                caller: None,
                recv: has_receiver(nn),
                pos: nn.start_byte(),
            });
        }
    }
    nest(&mut out);
    out
}

/// Dedupe definitions, link each to its innermost enclosing definition, and refs to theirs.
fn nest(p: &mut Parsed) {
    p.defs.sort_by(|a, b| (a.start, std::cmp::Reverse(a.end)).cmp(&(b.start, std::cmp::Reverse(b.end))));
    // Same name on the same line from two patterns (declaration vs definition): keep the widest,
    // and an explicit container name from either.
    let mut first: HashMap<(String, u32), usize> = HashMap::new();
    let mut keep = vec![true; p.defs.len()];
    for (i, k) in keep.iter_mut().enumerate() {
        match first.get(&(p.defs[i].name.clone(), p.defs[i].line)) {
            Some(&j) => {
                *k = false;
                if p.defs[j].container_name.is_none() {
                    p.defs[j].container_name = p.defs[i].container_name.take();
                }
            }
            None => {
                first.insert((p.defs[i].name.clone(), p.defs[i].line), i);
            }
        }
    }
    let mut k = keep.iter();
    p.defs.retain(|_| *k.next().unwrap_or(&true));
    link(&mut p.defs);
    // Locals (variables inside a function body) are not symbols.
    let local: Vec<bool> = p
        .defs
        .iter()
        .map(|d| {
            matches!(d.kind.as_str(), "property" | "variable" | "constant" | "field")
                && d.container.is_some_and(|c| is_callable_kind(&p.defs[c].kind))
        })
        .collect();
    if local.iter().any(|x| *x) {
        let mut l = local.iter();
        p.defs.retain(|_| !*l.next().unwrap_or(&false));
        link(&mut p.defs);
    }
    for i in 0..p.defs.len() {
        let in_type = p.defs[i].container.is_some_and(|c| is_type_kind(&p.defs[c].kind)) || p.defs[i].container_name.is_some();
        if p.defs[i].kind == "function" && in_type {
            p.defs[i].kind = "method".into();
        }
    }
    // Refs: innermost definition whose byte range contains the reference (definitions are sorted).
    p.refs.sort_by_key(|r| r.pos);
    p.refs.dedup_by(|a, b| a.pos == b.pos && a.name == b.name && a.kind == b.kind);
    let mut stack: Vec<usize> = vec![];
    let mut di = 0;
    for r in p.refs.iter_mut() {
        while di < p.defs.len() && p.defs[di].start <= r.pos {
            while stack.last().is_some_and(|&t| p.defs[t].end <= p.defs[di].start) {
                stack.pop();
            }
            stack.push(di);
            di += 1;
        }
        while stack.last().is_some_and(|&t| p.defs[t].end <= r.pos) {
            stack.pop();
        }
        r.caller = stack.last().copied();
    }
}

fn link(defs: &mut [Def]) {
    let mut stack: Vec<usize> = vec![];
    for i in 0..defs.len() {
        while stack.last().is_some_and(|&t| defs[t].end <= defs[i].start) {
            stack.pop();
        }
        defs[i].container = stack.last().copied();
        stack.push(i);
    }
}

pub fn content_hash(b: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub files: usize,
    pub parsed: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub symbols: usize,
    pub refs: usize,
    /// References scored this run: all of them on a cold index, the change's on an update.
    pub rescored: usize,
    /// References with a target, among those scored.
    pub resolved: usize,
    pub ms: u128,
}

enum Work {
    Touch(String, i64, i64),
    Parse {
        path: String,
        lang: &'static str,
        hash: String,
        size: i64,
        mtime: i64,
        parsed: Parsed,
        texts: Vec<String>,
    },
    /// An unchanged file whose symbols' body text is written again (a backfill or a rebuild).
    Body {
        path: String,
        size: i64,
        mtime: i64,
        texts: Vec<String>,
    },
}

/// Most source characters (doc comment first, then the definition) the body arm reads per symbol.
pub const BODY_CHARS: usize = 1600;

/// Does a trimmed line belong to a comment block or an attribute above a definition?
fn is_doc_line(t: &str) -> bool {
    ["//", "#", "/*", "*", "--", ";", "@"].iter().any(|p| t.starts_with(p))
}

/// Body-arm text for definition `d` of `src`: its leading comment block, then its own source,
/// at most [`BODY_CHARS`] characters, every identifier followed by its parts (see [`split_words`]).
pub fn body_text(src: &[u8], d: &Def) -> String {
    let (start, end) = (d.start.min(src.len()), d.end.min(src.len()));
    let before = String::from_utf8_lossy(&src[..start]);
    let mut doc: Vec<&str> = vec![];
    // Skip the partial line the definition starts on (indentation, `pub `), then walk upwards.
    let mut lines = before.lines().rev();
    if !before.is_empty() && !before.ends_with('\n') {
        lines.next();
    }
    for l in lines.take(60) {
        let t = l.trim();
        if t.is_empty() || !is_doc_line(t) {
            break;
        }
        doc.push(t);
    }
    doc.reverse();
    let mut raw = doc.join("\n");
    raw.push('\n');
    raw.push_str(&String::from_utf8_lossy(&src[start..end.max(start)]));
    let cut: String = raw.chars().take(BODY_CHARS).collect();
    split_words(&cut)
}

/// Words of `text` for the full-text index: each identifier as written, then its parts when it
/// has several (`fooBar foo bar`), and snake_case also as one squashed word (`foo_bar foobar`),
/// so a whole snake_case name is one term, as a camelCase one already is.
pub fn split_words(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for tok in text.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|t| t.chars().any(char::is_alphanumeric)) {
        out.push_str(tok);
        out.push(' ');
        let parts = crate::search::ident_parts(tok);
        if parts.len() > 1 {
            if tok.contains('_') {
                out.push_str(&parts.concat());
                out.push(' ');
            }
            if tok.chars().any(|c| c.is_uppercase() || c.is_ascii_digit()) {
                out.push_str(&parts.join(" "));
                out.push(' ');
            }
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// Incrementally (re)index the project's code files from the inventory.
pub fn index_project(c: &mut Connection, p: &Project) -> Result<Stats> {
    let t0 = std::time::Instant::now();
    let mut st = Stats::default();
    // Inventory code files (tier 3) with a known grammar. Swift files the index store covers are skipped.
    let files: Vec<(String, i64, i64)> = {
        let mut q = c.prepare(
            "SELECT f.path, COALESCE(f.size,0), COALESCE(f.mtime,0) FROM node f WHERE f.project_id = ?1 AND f.kind = 'file' AND f.tier >= 2
             AND NOT (f.lang = 'swift' AND EXISTS (SELECT 1 FROM swift_rec r WHERE r.path = f.path))",
        )?;
        q.query_map([p.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .flatten()
            .filter(|(path, size, _): &(String, i64, i64)| {
                lang::for_path(path).is_some() && (*size as u64) <= MAX_FILE && !crate::tiers::is_denied_path(path)
            })
            .collect()
    };
    let old: HashMap<String, (String, i64, i64)> = {
        let mut q = c.prepare("SELECT path, hash, size, mtime FROM ts_file WHERE project_id = ?1")?;
        q.query_map([p.id], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?.flatten().collect()
    };
    st.files = files.len();
    let now: HashSet<&str> = files.iter().map(|f| f.0.as_str()).collect();
    let removed: Vec<String> = old.keys().filter(|k| !now.contains(k.as_str())).cloned().collect();
    // A body backfill (or a requested rebuild) reads every file once; unchanged ones only have
    // their symbols' body text written again, so their ids and handles stay.
    let reindex = db::reindex_pending(c, p.id);
    let todo: Vec<&(String, i64, i64)> =
        files.iter().filter(|(path, size, mtime)| reindex || old.get(path).is_none_or(|o| o.1 != *size || o.2 != *mtime)).collect();
    let root = p.root.clone();
    let work: Vec<Work> = todo
        .par_iter()
        .filter_map(|(path, size, mtime)| {
            let l = lang::for_path(path)?;
            let bytes = std::fs::read(root.join(path)).ok()?;
            let hash = content_hash(&bytes);
            let same = old.get(path).is_some_and(|o| o.0 == hash);
            if same && !reindex {
                return Some(Work::Touch(path.clone(), *size, *mtime));
            }
            let parsed = extract(l, &bytes);
            let texts = parsed.defs.iter().map(|d| body_text(&bytes, d)).collect();
            if same {
                return Some(Work::Body { path: path.clone(), size: *size, mtime: *mtime, texts });
            }
            Some(Work::Parse { path: path.clone(), lang: l.name, hash, size: *size, mtime: *mtime, parsed, texts })
        })
        .collect();
    trace("ts parse", t0);
    let tx = c.transaction()?;
    // A big cold run builds the name index once at the end: a sort, not millions of random inserts.
    let bulk = old.is_empty() && work.len() > 5000;
    if bulk {
        tx.execute_batch("DROP INDEX IF EXISTS ts_ref_name")?;
    }
    // Scope for incremental resolution: changed paths, and names defined there before or after.
    let mut sc_paths: HashSet<String> = HashSet::new();
    let mut sc_names: HashSet<String> = HashSet::new();
    let full = old.is_empty();
    let old_names = |path: &str, out: &mut HashSet<String>| -> Result<()> {
        let mut q =
            tx.prepare_cached("SELECT name FROM node WHERE project_id = ?1 AND path = ?2 AND kind = 'symbol' AND source = 'treesitter'")?;
        out.extend(q.query_map(params![p.id, path], |r| r.get::<_, String>(0))?.flatten());
        Ok(())
    };
    for path in &removed {
        old_names(path, &mut sc_names)?;
        sc_paths.insert(path.clone());
        drop_file(&tx, p.id, path)?;
        tx.execute("DELETE FROM ts_file WHERE project_id = ?1 AND path = ?2", params![p.id, path])?;
        st.removed += 1;
    }
    for w in work {
        match w {
            Work::Touch(path, size, mtime) => {
                tx.execute(
                    "UPDATE ts_file SET size = ?3, mtime = ?4 WHERE project_id = ?1 AND path = ?2",
                    params![p.id, path, size, mtime],
                )?;
                st.unchanged += 1;
            }
            Work::Body { path, size, mtime, texts } => {
                tx.execute(
                    "UPDATE ts_file SET size = ?3, mtime = ?4 WHERE project_id = ?1 AND path = ?2",
                    params![p.id, path, size, mtime],
                )?;
                store_bodies(&tx, p.id, &path, &texts)?;
                st.unchanged += 1;
            }
            Work::Parse { path, lang, hash, size, mtime, parsed, texts } => {
                if old.contains_key(&path) {
                    old_names(&path, &mut sc_names)?;
                    drop_file(&tx, p.id, &path)?;
                }
                sc_names.extend(parsed.defs.iter().map(|d| d.name.clone()));
                sc_paths.insert(path.clone());
                store_file(&tx, p.id, &path, lang, &parsed, &texts)?;
                tx.execute(
                    "INSERT OR REPLACE INTO ts_file(project_id, path, lang, hash, size, mtime) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![p.id, path, lang, hash, size, mtime],
                )?;
                st.parsed += 1;
                st.symbols += parsed.defs.len();
                st.refs += parsed.refs.len();
            }
        }
    }
    if bulk {
        tx.execute_batch("CREATE INDEX IF NOT EXISTS ts_ref_name ON ts_ref(project_id, name)")?;
    }
    trace("ts parse+store", t0);
    // A store upgraded past the ambiguity column re-scores every reference once.
    let rescore = db::rescore_pending(&tx, p.id);
    if st.parsed > 0 || st.removed > 0 || rescore {
        let r = resolve_scoped(&tx, p.id, (!full && !rescore).then_some((&sc_paths, &sc_names)))?;
        (st.rescored, st.resolved) = (r.rescored, r.resolved);
    }
    if rescore {
        db::rescore_done(&tx, p.id)?;
    }
    if reindex {
        db::reindex_done(&tx, p.id)?;
    }
    trace("ts resolve", t0);
    let changed = st.parsed > 0 || st.removed > 0 || rescore || reindex;
    if changed {
        db::bump_project(&tx, p.id)?;
    }
    tx.commit()?;
    if changed {
        db::optimize(c, full);
        trace("ts optimize", t0);
    }
    st.ms = t0.elapsed().as_millis();
    Ok(st)
}

fn trace(what: &str, t0: std::time::Instant) {
    if std::env::var_os("FOXDB_TRACE").is_some() {
        eprintln!("{what} {:?}", t0.elapsed());
    }
}

fn drop_file(tx: &Connection, pid: i64, path: &str) -> Result<()> {
    let sel = "SELECT id FROM node WHERE project_id = ?1 AND path = ?2 AND kind = 'symbol' AND source = 'treesitter'";
    tx.execute(&format!("DELETE FROM edge WHERE src IN ({sel})"), params![pid, path])?;
    tx.execute(&format!("DELETE FROM edge WHERE dst IN ({sel})"), params![pid, path])?;
    tx.execute(&format!("DELETE FROM fts_node WHERE rowid IN ({sel})"), params![pid, path])?;
    tx.execute(&format!("DELETE FROM fts_trigram WHERE rowid IN ({sel})"), params![pid, path])?;
    tx.execute(&format!("DELETE FROM fts_body WHERE rowid IN ({sel})"), params![pid, path])?;
    tx.execute(&format!("UPDATE ts_ref SET dst = 0 WHERE dst != 0 AND dst IN ({sel})"), params![pid, path])?;
    tx.execute("DELETE FROM node WHERE project_id = ?1 AND path = ?2 AND kind = 'symbol' AND source = 'treesitter'", params![pid, path])?;
    tx.execute("DELETE FROM ts_ref WHERE project_id = ?1 AND path = ?2", params![pid, path])?;
    Ok(())
}

/// Write the body-arm rows of an unchanged file's symbols again, found by their stable keys.
fn store_bodies(tx: &Connection, pid: i64, path: &str, texts: &[String]) -> Result<()> {
    let mut get =
        tx.prepare_cached("SELECT id, name, COALESCE(json_extract(meta, '$.parent') || '.' || name, name) FROM node WHERE key = ?1")?;
    let mut del = tx.prepare_cached("DELETE FROM fts_body WHERE rowid = ?1")?;
    let mut ins = tx.prepare_cached("INSERT INTO fts_body(rowid, name, sig, path, body) VALUES (?1,?2,?3,?4,?5)")?;
    for (i, text) in texts.iter().enumerate() {
        let row: Option<(i64, String, String)> =
            get.query_row([format!("t:{pid}:{path}#{i}")], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).ok();
        let Some((id, name, q)) = row else { continue };
        del.execute([id])?;
        ins.execute(params![id, crate::inventory::split_ident(&name), q, path, text])?;
    }
    Ok(())
}

fn store_file(tx: &Connection, pid: i64, path: &str, lang: &str, p: &Parsed, texts: &[String]) -> Result<()> {
    let mut ins = tx.prepare_cached(
        "INSERT INTO node(project_id, kind, key, name, path, line, end_line, sig, lang, tier, source, meta, updated_at)
         VALUES (?1,'symbol',?2,?3,?4,?5,?6,?3,?7,3,'treesitter',?8,?9)",
    )?;
    let mut fts = tx.prepare_cached("INSERT INTO fts_node(rowid, name, sig, path, body) VALUES (?1,?2,?3,?4,'')")?;
    let mut tri = tx.prepare_cached("INSERT INTO fts_trigram(rowid, path, name) VALUES (?1,?2,?3)")?;
    let mut body = tx.prepare_cached("INSERT INTO fts_body(rowid, name, sig, path, body) VALUES (?1,?2,?3,?4,?5)")?;
    let now = crate::db::now();
    let mut ids: Vec<i64> = Vec::with_capacity(p.defs.len());
    for (i, d) in p.defs.iter().enumerate() {
        let parent = p.container_of(i);
        let pid_ref = d.container.map(|c| ids[c]);
        let meta = serde_json::json!({"k": d.kind, "parent": parent, "pid": pid_ref, "ts": 1});
        ins.execute(params![pid, format!("t:{pid}:{path}#{i}"), d.name, path, d.line, d.end_line, lang, meta.to_string(), now])?;
        let id = tx.last_insert_rowid();
        ids.push(id);
        let q = match &parent {
            Some(pn) => format!("{pn}.{}", d.name),
            None => d.name.clone(),
        };
        let split = crate::inventory::split_ident(&d.name);
        fts.execute(params![id, split, q, path])?;
        tri.execute(params![id, path, d.name])?;
        body.execute(params![id, split, q, path, texts.get(i).map(String::as_str).unwrap_or("")])?;
    }
    let mut r =
        tx.prepare_cached("INSERT INTO ts_ref(project_id, path, src, name, line, kind, recv, dst) VALUES (?1,?2,?3,?4,?5,?6,?7,0)")?;
    for x in &p.refs {
        let src = x.caller.map(|i| ids[i]).unwrap_or(0);
        r.execute(params![pid, path, src, x.name, x.line, x.kind, x.recv])?;
    }
    Ok(())
}

/// A candidate definition, with what scoring needs precomputed.
struct Cand {
    id: i64,
    path: String,
    kind: String,
    /// Enclosing definition's id (0 at top level).
    pid: i64,
    /// Last segment of the container name.
    parent: String,
    family: &'static str,
    /// Declared inside a function body: visible only in its own file.
    local: bool,
}

/// One reference row to score.
struct RefRow {
    rowid: i64,
    path: String,
    src: i64,
    name: String,
    kind: String,
    dst: i64,
    recv: bool,
    amb: i64,
    /// Container of the calling definition (-1 when unknown).
    caller_parent: i64,
}

/// One import: its last segment (imported name, module or `*`) and, for dotted JVM imports, the
/// package as a directory path (`a.b.C` -> `a/b`), which must match the candidate's directory.
struct Import {
    last: String,
    pkg: String,
}

impl Import {
    fn parse(name: &str) -> Import {
        let n = name.trim().trim_end_matches(';');
        let (pkg, last) = n.rsplit_once(['.', '/', ':', '\\']).unwrap_or(("", n));
        Import { last: last.to_string(), pkg: pkg.trim_end_matches(':').replace(['.', ':'], "/") }
    }
}

/// A file's imports, with their last segments in a set: a candidate is imported when one of its
/// names (own, container, file stem, Go package directory) is among them.
#[derive(Default)]
struct FileImports {
    all: Vec<Import>,
    lasts: HashSet<String>,
}

impl FileImports {
    fn push(&mut self, name: &str) {
        let i = Import::parse(name);
        self.lasts.insert(i.last.clone());
        self.all.push(i);
    }
    fn hits(&self, fam: &str, name: &str, s: &Cand) -> bool {
        let dir = dir_of(&s.path);
        if fam == "jvm" {
            return self.all.iter().any(|m| {
                (m.last == "*" || m.last == name || m.last == s.parent)
                    && (m.pkg.is_empty() || dir.ends_with(&m.pkg) || !dir.is_empty() && m.pkg.ends_with(dir))
            });
        }
        self.lasts.contains(name)
            || self.lasts.contains(&s.parent)
            || self.lasts.contains(stem_of(&s.path))
            || fam == "go" && self.lasts.contains(dir.rsplit('/').next().unwrap_or(""))
    }
}

const UNIQUE_FALLBACK: &[&str] = &["c", "bash", "lua", "ruby", "php", "dart", "dotnet", "swift"];

fn dir_of(p: &str) -> &str {
    p.rsplit_once('/').map(|x| x.0).unwrap_or("")
}
fn stem_of(p: &str) -> &str {
    let f = p.rsplit('/').next().unwrap_or(p);
    f.split('.').next().unwrap_or(f)
}

/// Score one reference against its same-name candidates (in id order): same file > same type >
/// imported > same directory > unique elsewhere. Returns (target or 0, ambiguity): ambiguity is
/// the number of same-name candidates a name-only guess chose between, 0 when the target is
/// certain (same file, the only candidate, or the single best by scope).
fn score(r: &RefRow, cands: &[Cand], imp: Option<&FileImports>) -> (i64, i64) {
    let fam = lang::for_path(&r.path).map(|l| l.family).unwrap_or("");
    let call = r.kind == "call";
    let mut best = (0i32, 0i64, 0usize, false);
    let mut eligible = 0i64;
    for s in cands {
        if s.family != fam || s.id == r.src && call && is_type_kind(&s.kind) {
            continue;
        }
        let same_file = s.path == r.path;
        // Local declarations (inside a function) are only visible in their own file.
        if !same_file && s.local {
            continue;
        }
        eligible += 1;
        let mut score = if same_file {
            100
        } else if imp.is_some_and(|v| v.hits(fam, &r.name, s)) {
            60
        } else if dir_of(&s.path) == dir_of(&r.path) {
            40
        } else {
            10
        };
        if s.pid != 0 && s.pid == r.caller_parent {
            score += 15;
        }
        if call && !(is_callable_kind(&s.kind) || is_type_kind(&s.kind)) {
            score -= 5;
        }
        if call && !same_file {
            // `f()` rarely reaches another type's member; `x.f()` rarely a free function.
            if !r.recv && s.pid != 0 && s.pid != r.caller_parent {
                score -= 25;
            }
            if r.recv && s.pid == 0 && s.parent.is_empty() {
                score -= 20;
            }
        }
        if score > best.0 {
            best = (score, s.id, 1, same_file);
        } else if score == best.0 {
            best.2 += 1;
        }
    }
    // No scope signal: only languages without captured imports may fall back to a unique name.
    let dst = if best.0 >= 20 || best.0 > 0 && best.2 == 1 && UNIQUE_FALLBACK.contains(&fam) { best.1 } else { 0 };
    let guess = dst != 0 && !best.3 && eligible > 1 && (best.2 > 1 || call && r.recv);
    (dst, if guess { eligible } else { 0 })
}

fn map_cand(r: &rusqlite::Row) -> rusqlite::Result<(String, Cand)> {
    let lang: String = r.get(6)?;
    Ok((
        r.get(1)?,
        Cand {
            id: r.get(0)?,
            path: r.get(2)?,
            kind: r.get(3)?,
            pid: r.get(4)?,
            parent: r.get::<_, String>(5)?.rsplit('.').next().unwrap_or("").to_string(),
            family: lang::by_name(&lang).map(|l| l.family).unwrap_or(""),
            local: false,
        },
    ))
}

const CAND_COLS: &str =
    "id, name, path, json_extract(meta,'$.k'), COALESCE(json_extract(meta,'$.pid'),0), COALESCE(json_extract(meta,'$.parent'),''), lang";
const REF_COLS: &str = "rowid, path, src, name, kind, dst, recv, amb";

fn map_ref(r: &rusqlite::Row) -> rusqlite::Result<RefRow> {
    Ok(RefRow {
        rowid: r.get(0)?,
        path: r.get(1)?,
        src: r.get(2)?,
        name: r.get(3)?,
        kind: r.get(4)?,
        dst: r.get(5)?,
        recv: r.get(6)?,
        amb: r.get(7)?,
        caller_parent: -1,
    })
}

/// What resolution did: references scored, and how many of those now have a target.
#[derive(Debug, Default, Clone, Copy)]
pub struct Resolved {
    pub rescored: usize,
    pub resolved: usize,
}

/// Resolve every reference of the project by name: same file > same type > imported > same
/// directory > unique elsewhere. Ambiguous names with no scope signal stay unresolved (`dst = 0`).
/// Rebuilds the project's tree-sitter edges.
pub fn resolve(tx: &Connection, pid: i64) -> Result<Resolved> {
    resolve_scoped(tx, pid, None)
}

/// Like [`resolve`], but only re-scores references in `paths` or named in `names` (the files
/// that changed and the names they define or used to define); the rest keep their target. Only
/// those references, their files' imports and their names' candidates are read, so the cost
/// follows the change, not the project. Scoring runs in parallel.
pub fn resolve_scoped(tx: &Connection, pid: i64, scope: Option<(&HashSet<String>, &HashSet<String>)>) -> Result<Resolved> {
    let t0 = std::time::Instant::now();
    // 1. The references to score, and the imports of their files.
    let mut refs: Vec<RefRow> = vec![];
    let mut imports: HashMap<String, FileImports> = HashMap::new();
    match scope {
        None => {
            // A sequential scan: through an index, millions of rows cost a random read each.
            let mut q = tx.prepare(&format!("SELECT {REF_COLS} FROM ts_ref NOT INDEXED WHERE project_id = ?1"))?;
            for r in q.query_map([pid], map_ref)?.flatten() {
                if r.kind == "import" {
                    imports.entry(r.path.clone()).or_default().push(&r.name);
                } else {
                    refs.push(r);
                }
            }
        }
        Some((paths, names)) => {
            let mut seen: HashSet<i64> = HashSet::new();
            let mut by_path = tx.prepare_cached(&format!("SELECT {REF_COLS} FROM ts_ref WHERE project_id = ?1 AND path = ?2"))?;
            let mut by_name = tx.prepare_cached(&format!("SELECT {REF_COLS} FROM ts_ref WHERE project_id = ?1 AND name = ?2"))?;
            let mut sorted_paths: Vec<&String> = paths.iter().collect();
            sorted_paths.sort();
            for path in sorted_paths {
                refs.extend(by_path.query_map(params![pid, path], map_ref)?.flatten().filter(|r| seen.insert(r.rowid)));
            }
            let mut sorted_names: Vec<&String> = names.iter().collect();
            sorted_names.sort();
            for name in sorted_names {
                refs.extend(by_name.query_map(params![pid, name], map_ref)?.flatten().filter(|r| seen.insert(r.rowid)));
            }
            refs.retain(|r| r.kind != "import");
            let files: HashSet<&str> = refs.iter().map(|r| r.path.as_str()).collect();
            let mut q = tx.prepare_cached("SELECT name FROM ts_ref WHERE project_id = ?1 AND path = ?2 AND kind = 'import'")?;
            for f in files {
                let e = imports.entry(f.to_string()).or_default();
                for n in q.query_map(params![pid, f], |r| r.get::<_, String>(0))?.flatten() {
                    e.push(&n);
                }
            }
        }
    }
    trace("resolve: refs loaded", t0);
    // 2. Candidates by name (id order, so ties break the same way in full and scoped runs), and
    //    the container of every definition involved.
    let mut cands: HashMap<String, Vec<Cand>> = HashMap::new();
    let mut parent_of: HashMap<i64, (i64, bool)> = HashMap::new(); // id -> (container id, callable)
    match scope {
        None => {
            let mut q = tx.prepare(&format!(
                "SELECT {CAND_COLS} FROM node WHERE project_id = ?1 AND kind = 'symbol' AND source = 'treesitter' ORDER BY id"
            ))?;
            for (name, c) in q.query_map([pid], map_cand)?.flatten() {
                parent_of.insert(c.id, (c.pid, is_callable_kind(&c.kind)));
                cands.entry(name).or_default().push(c);
            }
        }
        Some(_) => {
            let mut q = tx.prepare_cached(&format!(
                "SELECT {CAND_COLS} FROM node WHERE project_id = ?1 AND kind = 'symbol' AND name = ?2 COLLATE NOCASE AND source = 'treesitter'"
            ))?;
            let names: HashSet<&str> = refs.iter().map(|r| r.name.as_str()).collect();
            for n in names {
                let mut v: Vec<Cand> =
                    q.query_map(params![pid, n], map_cand)?.flatten().filter(|(name, _)| name == n).map(|x| x.1).collect();
                v.sort_by_key(|c| c.id);
                cands.insert(n.to_string(), v);
            }
            let mut one = tx.prepare_cached(
                "SELECT COALESCE(json_extract(meta,'$.pid'),0), json_extract(meta,'$.k') FROM node WHERE id = ?1 AND kind = 'symbol'",
            )?;
            let mut want: Vec<i64> =
                refs.iter().map(|r| r.src).chain(cands.values().flatten().map(|c| c.pid)).filter(|&i| i != 0).collect();
            want.sort_unstable();
            want.dedup();
            for id in want {
                if let Ok((p, k)) = one.query_row([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))) {
                    parent_of.insert(id, (p, is_callable_kind(&k)));
                }
            }
        }
    }
    for v in cands.values_mut() {
        for c in v.iter_mut() {
            c.local = parent_of.get(&c.pid).is_some_and(|p| p.1);
        }
    }
    for r in refs.iter_mut() {
        r.caller_parent = parent_of.get(&r.src).map(|p| p.0).unwrap_or(-1);
    }
    trace("resolve: candidates loaded", t0);
    // 3. Score in parallel; write only what changed.
    let scored: Vec<(i64, i64)> =
        refs.par_iter().map(|r| score(r, cands.get(&r.name).map(Vec::as_slice).unwrap_or(&[]), imports.get(&r.path))).collect();
    trace("resolve: scored", t0);
    let mut upd = tx.prepare_cached("UPDATE ts_ref SET dst = ?2, amb = ?3 WHERE rowid = ?1")?;
    let mut out = Resolved { rescored: refs.len(), resolved: 0 };
    let class = |k: &str| if k == "call" { "CALLS" } else { "REFERENCES" };
    let mut touched: HashSet<(i64, i64, &'static str)> = HashSet::new();
    let mut edges: HashSet<(i64, i64, &'static str)> = HashSet::new();
    for (r, &(dst, amb)) in refs.iter().zip(&scored) {
        if dst != r.dst || amb != r.amb {
            upd.execute(params![r.rowid, dst, amb])?;
        }
        if dst != 0 {
            out.resolved += 1;
        }
        for d in [r.dst, dst] {
            if r.src != 0 && d != 0 && r.src != d {
                touched.insert((r.src, d, class(&r.kind)));
            }
        }
        if dst != 0 && r.src != 0 && r.src != dst {
            edges.insert((r.src, dst, class(&r.kind)));
        }
    }
    trace("resolve: refs written", t0);
    let mut del = tx.prepare_cached("DELETE FROM edge WHERE src = ?1 AND dst = ?2 AND kind = ?3")?;
    let mut ins = tx.prepare_cached("INSERT OR IGNORE INTO edge(src, dst, kind, source, confidence) VALUES (?1,?2,?3,'treesitter',0.8)")?;
    match scope {
        None => {
            // Write only the edge differences.
            let old: HashSet<(i64, i64, String)> = {
                let mut q = tx.prepare(
                    "SELECT e.src, e.dst, e.kind FROM edge e JOIN node n ON n.id = e.src WHERE n.project_id = ?1 AND e.source = 'treesitter'",
                )?;
                q.query_map([pid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.flatten().collect()
            };
            for (s, d, k) in &old {
                if !edges.contains(&(*s, *d, k.as_str())) {
                    del.execute(params![s, d, k])?;
                }
            }
            for (s, d, k) in edges {
                if !old.contains(&(s, d, k.to_string())) {
                    ins.execute(params![s, d, k])?;
                }
            }
        }
        Some(_) => {
            // An edge exists while any reference still carries it: re-check only the pairs whose
            // references were re-scored.
            let mut calls = tx.prepare_cached("SELECT 1 FROM ts_ref WHERE src = ?1 AND src != 0 AND dst = ?2 AND kind = 'call' LIMIT 1")?;
            let mut other = tx.prepare_cached(
                "SELECT 1 FROM ts_ref WHERE src = ?1 AND src != 0 AND dst = ?2 AND kind NOT IN ('call', 'import') LIMIT 1",
            )?;
            for (s, d, k) in touched {
                let q = if k == "CALLS" { &mut calls } else { &mut other };
                if q.exists(params![s, d])? {
                    ins.execute(params![s, d, k])?;
                } else {
                    del.execute(params![s, d, k])?;
                }
            }
        }
    }
    Ok(out)
}

/// Does the project have native tree-sitter symbols?
pub fn has_symbols(c: &Connection, pid: i64) -> bool {
    c.query_row("SELECT 1 FROM node WHERE project_id = ?1 AND source = 'treesitter' AND kind = 'symbol' LIMIT 1", [pid], |_| Ok(())).is_ok()
}
