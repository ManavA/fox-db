//! The precise tier: SCIP indexes produced by the user's own builds (scip-java, rust-analyzer
//! `scip`, scip-python, scip-typescript, ...), imported with `fox scip import <index.scip>`.
//!
//! foxdb never runs a build or a language server: it only reads the protobuf file. A document is
//! trusted at import only when the index embedded its text and that text is the file's current
//! content (or, with the opt-in `--trust-mtime`, when the file was last modified before the index
//! file was written). A trusted document is stored with that content hash, and only documents
//! whose hash still matches the tree-sitter index (`ts_file.hash`) are used afterwards. Resolution then upgrades a
//! tree-sitter reference at the same file, line and name to the definition SCIP names, marking it
//! precise (`ts_ref.amb = -1`), wherever both the reference's file and the definition's file are
//! fresh; everything else keeps the tree-sitter answer. A re-import replaces the previous one.
//!
//! The tables are created by the first import, so a store that never imports gains nothing.

use crate::project::Project;
use crate::tsindex;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS scip_import(
  project_id INTEGER PRIMARY KEY, file TEXT NOT NULL, tool TEXT NOT NULL, imported_at INTEGER NOT NULL,
  docs INTEGER NOT NULL, occs INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS scip_doc(
  project_id INTEGER NOT NULL, path TEXT NOT NULL, hash TEXT NOT NULL, PRIMARY KEY(project_id, path)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS scip_sym(
  project_id INTEGER NOT NULL, symbol TEXT NOT NULL, name TEXT NOT NULL, parent TEXT NOT NULL, path TEXT NOT NULL,
  line INTEGER NOT NULL, prev_path TEXT, moved_at INTEGER, PRIMARY KEY(project_id, symbol)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS scip_sym_name ON scip_sym(project_id, name COLLATE NOCASE);
CREATE TABLE IF NOT EXISTS scip_occ(
  project_id INTEGER NOT NULL, path TEXT NOT NULL, line INTEGER NOT NULL, name TEXT NOT NULL, symbol TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS scip_occ_path ON scip_occ(project_id, path);
"#;

/// Minimal protobuf reader for the SCIP schema (scip.proto): only the fields foxdb uses.
mod pb {
    use anyhow::{Result, bail};

    pub enum Val<'a> {
        Int(u64),
        Bytes(&'a [u8]),
        Skip,
    }

    pub struct Reader<'a> {
        b: &'a [u8],
        i: usize,
    }

    impl<'a> Reader<'a> {
        pub fn new(b: &'a [u8]) -> Self {
            Reader { b, i: 0 }
        }

        pub fn varint(&mut self) -> Result<u64> {
            let mut v = 0u64;
            for shift in (0..64).step_by(7) {
                let Some(&byte) = self.b.get(self.i) else { bail!("truncated varint") };
                self.i += 1;
                v |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    return Ok(v);
                }
            }
            bail!("varint too long")
        }

        fn take(&mut self, n: usize) -> Result<&'a [u8]> {
            let end = self.i.checked_add(n).filter(|&e| e <= self.b.len());
            let Some(end) = end else { bail!("truncated field") };
            let s = &self.b[self.i..end];
            self.i = end;
            Ok(s)
        }

        /// The next (field number, value), or None at the end.
        pub fn next(&mut self) -> Result<Option<(u64, Val<'a>)>> {
            if self.i >= self.b.len() {
                return Ok(None);
            }
            let key = self.varint()?;
            let v = match key & 7 {
                0 => Val::Int(self.varint()?),
                1 => {
                    self.take(8)?;
                    Val::Skip
                }
                2 => {
                    let n = self.varint()? as usize;
                    Val::Bytes(self.take(n)?)
                }
                5 => {
                    self.take(4)?;
                    Val::Skip
                }
                w => bail!("unsupported wire type {w}"),
            };
            Ok(Some((key >> 3, v)))
        }
    }

    pub fn string(b: &[u8]) -> String {
        String::from_utf8_lossy(b).into_owned()
    }

    /// A repeated int32, packed or not.
    pub fn ints(v: &Val, out: &mut Vec<i64>) -> Result<()> {
        match v {
            Val::Int(x) => out.push(*x as i32 as i64),
            Val::Bytes(b) => {
                let mut r = Reader::new(b);
                while r.i < b.len() {
                    out.push(r.varint()? as i32 as i64);
                }
            }
            Val::Skip => {}
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Occurrence {
    /// 0-based [start line, start char, (end line,) end char].
    pub range: Vec<i64>,
    pub symbol: String,
    pub roles: i64,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Document {
    pub relative_path: String,
    pub occurrences: Vec<Occurrence>,
    /// The document text, when the indexer embedded it.
    pub text: Option<String>,
    /// 1 UTF-8 code units, 2 UTF-16, 3 UTF-32; 0 unspecified.
    pub position_encoding: i64,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Index {
    pub tool: String,
    pub project_root: String,
    pub documents: Vec<Document>,
}

/// The `Definition` bit of `Occurrence.symbol_roles`.
pub const DEFINITION: i64 = 1;

/// Decode a SCIP index (concatenated `Index` messages merge, as in protobuf).
pub fn decode(b: &[u8]) -> Result<Index> {
    use pb::{Reader, Val, string};
    let mut ix = Index::default();
    let mut r = Reader::new(b);
    while let Some((f, v)) = r.next()? {
        match (f, v) {
            (1, Val::Bytes(meta)) => {
                let mut m = Reader::new(meta);
                while let Some((f, v)) = m.next()? {
                    match (f, v) {
                        (2, Val::Bytes(tool)) => {
                            let (mut name, mut version) = (String::new(), String::new());
                            let mut t = Reader::new(tool);
                            while let Some((f, v)) = t.next()? {
                                match (f, v) {
                                    (1, Val::Bytes(s)) => name = string(s),
                                    (2, Val::Bytes(s)) => version = string(s),
                                    _ => {}
                                }
                            }
                            ix.tool = format!("{name} {version}").trim().to_string();
                        }
                        (3, Val::Bytes(s)) => ix.project_root = string(s),
                        _ => {}
                    }
                }
            }
            (2, Val::Bytes(doc)) => {
                let mut d = Document::default();
                let mut dr = Reader::new(doc);
                while let Some((f, v)) = dr.next()? {
                    match (f, v) {
                        (1, Val::Bytes(s)) => d.relative_path = string(s),
                        (2, Val::Bytes(occ)) => {
                            let mut o = Occurrence::default();
                            let mut or = Reader::new(occ);
                            while let Some((f, v)) = or.next()? {
                                match f {
                                    1 => pb::ints(&v, &mut o.range)?,
                                    2 => {
                                        if let Val::Bytes(s) = v {
                                            o.symbol = string(s);
                                        }
                                    }
                                    3 => {
                                        if let Val::Int(x) = v {
                                            o.roles = x as i64;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            d.occurrences.push(o);
                        }
                        (5, Val::Bytes(s)) => d.text = Some(string(s)),
                        (6, Val::Int(x)) => d.position_encoding = x as i64,
                        _ => {}
                    }
                }
                ix.documents.push(d);
            }
            _ => {}
        }
    }
    Ok(ix)
}

/// Encode an index (the inverse of [`decode`], for tests and fixtures).
pub fn encode(ix: &Index) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }
    fn bytes(out: &mut Vec<u8>, field: u64, b: &[u8]) {
        varint(out, field << 3 | 2);
        varint(out, b.len() as u64);
        out.extend_from_slice(b);
    }
    fn int(out: &mut Vec<u8>, field: u64, v: u64) {
        varint(out, field << 3);
        varint(out, v);
    }
    let mut out = vec![];
    let mut meta = vec![];
    let mut tool = vec![];
    let (name, version) = ix.tool.split_once(' ').unwrap_or((&ix.tool, ""));
    bytes(&mut tool, 1, name.as_bytes());
    bytes(&mut tool, 2, version.as_bytes());
    bytes(&mut meta, 2, &tool);
    bytes(&mut meta, 3, ix.project_root.as_bytes());
    bytes(&mut out, 1, &meta);
    for d in &ix.documents {
        let mut doc = vec![];
        bytes(&mut doc, 1, d.relative_path.as_bytes());
        for o in &d.occurrences {
            let mut occ = vec![];
            let mut packed = vec![];
            for &x in &o.range {
                varint(&mut packed, x as i32 as i64 as u64);
            }
            bytes(&mut occ, 1, &packed);
            bytes(&mut occ, 2, o.symbol.as_bytes());
            if o.roles != 0 {
                int(&mut occ, 3, o.roles as u64);
            }
            bytes(&mut doc, 2, &occ);
        }
        if let Some(t) = &d.text {
            bytes(&mut doc, 5, t.as_bytes());
        }
        if d.position_encoding != 0 {
            int(&mut doc, 6, d.position_encoding as u64);
        }
        bytes(&mut out, 2, &doc);
    }
    out
}

/// The descriptor names of a global SCIP symbol (`scheme manager package version descriptors`),
/// outermost first: `rust-analyzer cargo demo 0.1.0 util/Greeter#greet().` gives
/// `[util, Greeter, greet]`. Empty for local symbols.
pub fn descriptor_names(symbol: &str) -> Vec<String> {
    if symbol.starts_with("local ") {
        return vec![];
    }
    // Skip the four space-separated header fields; a doubled space escapes a space.
    let b = symbol.as_bytes();
    let (mut i, mut fields) = (0, 0);
    while i < b.len() && fields < 4 {
        if b[i] == b' ' {
            if b.get(i + 1) == Some(&b' ') {
                i += 2;
                continue;
            }
            fields += 1;
        }
        i += 1;
    }
    let mut out = vec![];
    let mut cur = String::new();
    let chars: Vec<char> = symbol[i.min(symbol.len())..].chars().collect();
    let mut k = 0;
    while k < chars.len() {
        let ch = chars[k];
        match ch {
            '`' => {
                k += 1;
                while k < chars.len() {
                    if chars[k] == '`' {
                        if chars.get(k + 1) == Some(&'`') {
                            cur.push('`');
                            k += 2;
                            continue;
                        }
                        break;
                    }
                    cur.push(chars[k]);
                    k += 1;
                }
            }
            '/' | '#' | '.' | ':' | '!' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            '(' | '[' => {
                // Method disambiguator `(+1)`, type parameter `[T]` or parameter `(x)`.
                let close = if ch == '(' { ')' } else { ']' };
                let mut inner = String::new();
                k += 1;
                while k < chars.len() && chars[k] != close {
                    inner.push(chars[k]);
                    k += 1;
                }
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    // `name(+1).` is a method: the trailing `.` is consumed with it.
                    if chars.get(k + 1) == Some(&'.') {
                        k += 1;
                    }
                } else if !inner.is_empty() {
                    out.push(inner);
                }
            }
            _ => cur.push(ch),
        }
        k += 1;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn is_ident(s: &str) -> bool {
    let mut ch = s.chars();
    ch.next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$') && ch.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// The source text of a single-line range, read in the document's position encoding.
fn range_text(line: &str, start: i64, end: i64, encoding: i64) -> Option<String> {
    if start < 0 || end <= start {
        return None;
    }
    let (s, e) = (start as usize, end as usize);
    match encoding {
        2 => {
            let u: Vec<u16> = line.encode_utf16().collect();
            u.get(s..e).map(String::from_utf16_lossy)
        }
        3 => Some(line.chars().skip(s).take(e - s).collect()),
        _ => line.get(s..e).map(str::to_string),
    }
}

/// The occurrence's name: its source text when that is an identifier, else the last descriptor.
fn occ_name(lines: &[&str], o: &Occurrence, encoding: i64) -> Option<String> {
    let (line, start, end) = match o.range.as_slice() {
        [l, s, e] => (*l, *s, *e),
        [l, s, el, e] if el == l => (*l, *s, *e),
        _ => (-1, 0, 0),
    };
    if line >= 0
        && let Some(t) = lines.get(line as usize).and_then(|l| range_text(l, start, end, encoding))
        && is_ident(&t)
    {
        return Some(t);
    }
    descriptor_names(&o.symbol).pop().filter(|n| is_ident(n))
}

/// What an import read and stored.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub documents: usize,
    /// Documents whose file is unchanged since the index was built.
    pub fresh: usize,
    /// Documents for files changed since the index was built (ignored until re-imported).
    pub stale: usize,
    /// Documents without embedded text, so with no proof of what the indexer read (ignored unless
    /// the import trusts modification times).
    pub unverified: usize,
    /// Documents for files that are not in the project.
    pub outside: usize,
    pub definitions: usize,
    pub references: usize,
    /// Tree-sitter references that are now precise.
    pub precise: usize,
    /// Of those, references that were name guesses, unresolved, or resolved elsewhere.
    pub upgraded: usize,
    pub ms: u128,
}

/// Is the SCIP tier present in this store at all (the first import creates its tables)?
fn has_tables(c: &Connection) -> bool {
    c.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'scip_import'", [], |_| Ok(())).is_ok()
}

/// Does project `pid` have a SCIP tier?
pub fn active(c: &Connection, pid: i64) -> bool {
    has_tables(c) && c.query_row("SELECT 1 FROM scip_import WHERE project_id = ?1", [pid], |_| Ok(())).is_ok()
}

fn strip_file_uri(s: &str) -> PathBuf {
    let p = s.strip_prefix("file://").unwrap_or(s);
    // Minimal percent-decoding (spaces and other escaped bytes in paths).
    let b = p.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(h) = p.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(h);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}

/// Map a document's path (relative to the index's project root) to one relative to `p.root`, or
/// None when it lies outside the project.
fn project_path(root: &Path, index_root: &str, rel: &str) -> Option<String> {
    let rel = rel.trim_start_matches("./");
    if Path::new(rel).components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
        return None;
    }
    if index_root.is_empty() {
        return Some(rel.to_string());
    }
    let ir = strip_file_uri(index_root);
    let ir = std::fs::canonicalize(&ir).unwrap_or(ir);
    let abs = ir.join(rel);
    match abs.strip_prefix(root) {
        Ok(r) => Some(r.to_string_lossy().into_owned()),
        // An index built in another checkout of the same repository: trust the relative path.
        Err(_) if !ir.starts_with(root) && !root.starts_with(&ir) => Some(rel.to_string()),
        Err(_) => None,
    }
}

/// `fox scip import <index.scip> [--trust-mtime]`: read the index, replace the project's previous
/// import, and re-resolve the references of every file either import covers.
///
/// A document is trusted only when the index embedded its text and that text is the file's
/// current content. Most indexers leave the text out; such documents are unverified and ignored,
/// because an index written after an edit it did not see would otherwise turn a tree-sitter guess
/// into a wrong precise answer. `trust_mtime` opts in to trusting them when the file was last
/// modified before the index file was written (the index's mtime is not proof of what it read).
pub fn import(c: &mut Connection, p: &Project, file: &Path, trust_mtime: bool) -> Result<Stats> {
    let t0 = std::time::Instant::now();
    let bytes = std::fs::read(file).with_context(|| format!("cannot read {}", file.display()))?;
    let ix = decode(&bytes).with_context(|| format!("{} is not a SCIP index", file.display()))?;
    if ix.documents.is_empty() {
        bail!("{} holds no documents", file.display());
    }
    let built = std::fs::metadata(file).and_then(|m| m.modified()).ok();
    let abs = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let mut st = Stats { documents: ix.documents.len(), ..Default::default() };
    // Per document: (path, hash or "" when stale, occurrences as (line, name, symbol, roles)).
    type Occ = (i64, String, String, i64);
    let mut docs: Vec<(String, String, Vec<Occ>)> = vec![];
    for d in &ix.documents {
        let Some(path) = project_path(&p.root, &ix.project_root, &d.relative_path) else {
            st.outside += 1;
            continue;
        };
        let full = p.root.join(&path);
        let Ok(src) = std::fs::read(&full) else {
            st.outside += 1;
            continue;
        };
        // Fresh when the index embedded this exact text; without text, only on opt-in and when
        // the file was last modified strictly before the index file was written.
        let fresh = match &d.text {
            Some(t) => t.as_bytes() == src.as_slice(),
            None if !trust_mtime => {
                st.unverified += 1;
                false
            }
            None => {
                let m = std::fs::metadata(&full).and_then(|m| m.modified()).ok();
                matches!((m, built), (Some(m), Some(b)) if m < b)
            }
        };
        let text = String::from_utf8_lossy(&src);
        let lines: Vec<&str> = text.lines().collect();
        let mut occs = vec![];
        for o in &d.occurrences {
            if o.symbol.is_empty() || o.range.len() < 3 {
                continue;
            }
            let Some(name) = occ_name(&lines, o, d.position_encoding) else { continue };
            // Local symbols are numbered per document.
            let sym = if o.symbol.starts_with("local ") { format!("{}@{path}", o.symbol) } else { o.symbol.clone() };
            occs.push((o.range[0] + 1, name, sym, o.roles));
        }
        if fresh {
            st.fresh += 1;
        } else if d.text.is_some() || trust_mtime {
            st.stale += 1;
        }
        docs.push((path, if fresh { tsindex::content_hash(&src) } else { String::new() }, occs));
    }
    let tx = c.transaction()?;
    tx.execute_batch(SCHEMA)?;
    let mut paths: HashSet<String> = {
        let mut q = tx.prepare("SELECT path FROM scip_doc WHERE project_id = ?1")?;
        q.query_map([p.id], |r| r.get(0))?.flatten().collect()
    };
    let old_syms: HashMap<String, (String, Option<String>, Option<i64>)> = {
        let mut q = tx.prepare("SELECT symbol, path, prev_path, moved_at FROM scip_sym WHERE project_id = ?1")?;
        q.query_map([p.id], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?.flatten().collect()
    };
    for t in ["scip_doc", "scip_sym", "scip_occ"] {
        tx.execute(&format!("DELETE FROM {t} WHERE project_id = ?1"), [p.id])?;
    }
    let now = crate::db::now();
    {
        let mut ins_doc = tx.prepare("INSERT OR REPLACE INTO scip_doc(project_id, path, hash) VALUES (?1,?2,?3)")?;
        let mut ins_sym = tx.prepare(
            "INSERT OR IGNORE INTO scip_sym(project_id, symbol, name, parent, path, line, prev_path, moved_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        )?;
        let mut ins_occ = tx.prepare("INSERT INTO scip_occ(project_id, path, line, name, symbol) VALUES (?1,?2,?3,?4,?5)")?;
        for (path, hash, occs) in &docs {
            ins_doc.execute(params![p.id, path, hash])?;
            paths.insert(path.clone());
            for (line, name, sym, roles) in occs {
                if roles & DEFINITION != 0 {
                    let names = descriptor_names(sym);
                    let parent = if names.len() > 1 { names[names.len() - 2].clone() } else { String::new() };
                    // Remember where a symbol was before it moved to another file.
                    let (prev, moved) = match old_syms.get(sym) {
                        Some((old, _, _)) if old != path => (Some(old.clone()), Some(now)),
                        Some((_, prev, moved)) => (prev.clone(), *moved),
                        None => (None, None),
                    };
                    if ins_sym.execute(params![p.id, sym, name, parent, path, line, prev, moved])? > 0 {
                        st.definitions += 1;
                    }
                } else {
                    ins_occ.execute(params![p.id, path, line, name, sym])?;
                    st.references += 1;
                }
            }
        }
    }
    let tool = if ix.tool.is_empty() { "unknown".to_string() } else { ix.tool.clone() };
    tx.execute(
        "INSERT OR REPLACE INTO scip_import(project_id, file, tool, imported_at, docs, occs) VALUES (?1,?2,?3,?4,?5,?6)",
        params![p.id, abs.to_string_lossy(), tool, now, docs.len() as i64, (st.definitions + st.references) as i64],
    )?;
    let r = tsindex::resolve_scoped(&tx, p.id, Some((&paths, &HashSet::new())))?;
    (st.precise, st.upgraded) = (r.precise, r.upgraded);
    crate::db::bump_project(&tx, p.id)?;
    tx.commit()?;
    st.ms = t0.elapsed().as_millis();
    Ok(st)
}

/// The precise targets for the references in `path`, keyed by (line, name): only when `path` and
/// the definition's file are both unchanged since the import, the definition maps to exactly one
/// tree-sitter symbol, and the line holds no other same-name occurrence with another target.
pub(crate) fn targets(c: &Connection, pid: i64, path: &str) -> Result<HashMap<(i64, String), i64>> {
    let fresh = |path: &str| -> Result<bool> {
        Ok(c.prepare_cached(
            "SELECT 1 FROM scip_doc d JOIN ts_file f ON f.project_id = d.project_id AND f.path = d.path AND f.hash = d.hash
             WHERE d.project_id = ?1 AND d.path = ?2",
        )?
        .exists(params![pid, path])?)
    };
    let mut out: HashMap<(i64, String), Option<i64>> = HashMap::new();
    if !fresh(path)? {
        return Ok(HashMap::new());
    }
    let mut q = c.prepare_cached(
        "SELECT o.line, o.name,
           (SELECT n.id FROM node n WHERE n.project_id = s.project_id AND n.path = s.path AND n.kind = 'symbol'
              AND n.source = 'treesitter' AND n.name = s.name AND n.line = s.line ORDER BY n.id LIMIT 1)
         FROM scip_occ o
         JOIN scip_sym s ON s.project_id = o.project_id AND s.symbol = o.symbol
         JOIN scip_doc d ON d.project_id = s.project_id AND d.path = s.path
         JOIN ts_file f ON f.project_id = d.project_id AND f.path = d.path AND f.hash = d.hash
         WHERE o.project_id = ?1 AND o.path = ?2",
    )?;
    for (line, name, id) in q.query_map(params![pid, path], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<i64>>(2)?)))?.flatten() {
        let e = out.entry((line, name)).or_insert(id);
        if *e != id {
            *e = None;
        }
    }
    Ok(out.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect())
}

/// `fox status` detail for a project with a SCIP tier: tool, age and how many files are fresh.
pub fn status_line(c: &Connection, pid: i64) -> Option<String> {
    if !has_tables(c) {
        return None;
    }
    let (file, tool, at): (String, String, i64) = c
        .query_row("SELECT file, tool, imported_at FROM scip_import WHERE project_id = ?1", [pid], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()
        .ok()
        .flatten()?;
    let (docs, fresh): (i64, i64) = c
        .query_row(
            "SELECT COUNT(*), COUNT(f.path) FROM scip_doc d
             LEFT JOIN ts_file f ON f.project_id = d.project_id AND f.path = d.path AND f.hash = d.hash
             WHERE d.project_id = ?1",
            [pid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()?;
    let stale = if fresh < docs { "; stale files fall back to tree-sitter until a fresh import" } else { "" };
    Some(format!("    scip: {tool}, imported {}, {fresh} of {docs} files fresh{stale}  ({file})", crate::query::ago(at)))
}

/// A definition SCIP knows, for `fox drift`.
pub struct Def {
    pub symbol: String,
    pub path: String,
    pub line: i64,
    pub end: i64,
    /// The file it was in before it moved there, and when the move was imported.
    pub prev_path: Option<String>,
    pub moved_at: Option<i64>,
}

/// The fresh SCIP definitions named `name` (with container `parent`, when given), by path.
pub fn defs_named(c: &Connection, pid: i64, name: &str, parent: Option<&str>) -> Vec<Def> {
    if !active(c, pid) {
        return vec![];
    }
    let Ok(mut q) = c.prepare_cached(
        "SELECT s.symbol, s.path, s.line, s.parent, s.prev_path, s.moved_at,
           (SELECT MAX(COALESCE(n.end_line, n.line)) FROM node n WHERE n.project_id = s.project_id AND n.path = s.path
              AND n.kind = 'symbol' AND n.name = s.name AND n.line = s.line)
         FROM scip_sym s
         JOIN scip_doc d ON d.project_id = s.project_id AND d.path = s.path
         JOIN ts_file f ON f.project_id = d.project_id AND f.path = d.path AND f.hash = d.hash
         WHERE s.project_id = ?1 AND s.name = ?2 COLLATE NOCASE AND s.symbol NOT LIKE 'local %' ORDER BY s.path, s.line",
    ) else {
        return vec![];
    };
    let rows = q.query_map(params![pid, name], |r| {
        Ok((
            Def {
                symbol: r.get(0)?,
                path: r.get(1)?,
                line: r.get(2)?,
                end: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                prev_path: r.get(4)?,
                moved_at: r.get(5)?,
            },
            r.get::<_, String>(3)?,
        ))
    });
    let Ok(rows) = rows else { return vec![] };
    rows.flatten()
        .filter(|(_, par)| parent.is_none_or(|p| par.eq_ignore_ascii_case(p)))
        .map(|(mut d, _)| {
            d.end = d.end.max(d.line);
            d
        })
        .collect()
}
