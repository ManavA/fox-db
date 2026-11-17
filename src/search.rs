//! `find`: several retrieval arms run in parallel, each producing a ranked list of node ids, fused
//! with weighted reciprocal rank fusion and re-ranked with cheap code-aware features.
//!
//! Arms: exact name, keyword (FTS5 BM25 over identifier-split text), trigram substring, name
//! prefix, and graph neighbours of exact matches. Warm processes share the arms with the cold CLI
//! (the CSR graph only replaces the SQL walk), so both return the same answer. Re-ranking boosts the definition of the
//! queried name, demotes tests, vendored and generated code, and damps repeat hits from one file.
//!
//! Builds with the `vectors` feature can add an opt-in vector arm for sentences (`vectors`).
//!
//! The keyword and substring arms sit behind [`SearchBackend`]. SQLite FTS5 serves them; in
//! builds with the `tantivy` feature a project big enough to get a tantivy index (see
//! `tantivy_index`) is served by that index whenever it is current, and by FTS5 otherwise.

use crate::graph::Graph;
use crate::project::Project;
use crate::query::{FindOpts, Hit, render, render_id, render_skeleton};
use crate::spare::{Row, tiered};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

/// RRF constant (Cormack et al.).
pub const RRF_K: f64 = 60.0;

const W_EXACT: f64 = 2.0;
const W_KEYWORD: f64 = 1.0;
const W_SUBSTRING: f64 = 0.6;
const W_NAME: f64 = 0.8;
const W_GRAPH: f64 = 0.3;

const ARM_K: usize = 60;

/// Most id ranges a scoped full-text arm reads (each is one query per term and column).
const MAX_RANGES: usize = 8;

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "do", "does", "did", "for", "from", "how", "i", "in", "is", "it", "its", "of", "on",
    "or", "that", "the", "this", "to", "was", "what", "when", "where", "which", "who", "why", "with", "work", "works", "use", "used",
    "get", "find", "show", "me", "we", "can", "should", "there",
];

/// What a search may return: projects, node kinds and a path prefix. Empty lists mean "any".
/// Backends render it themselves: SQLite as SQL fragments ([`Filter`]), a later engine as its
/// own filter terms.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scope {
    pub projects: Vec<i64>,
    pub kinds: Vec<String>,
    pub path_prefix: Option<String>,
}

/// Normalized query terms, built once per query; backends build their own syntax from them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Terms {
    /// Words for the keyword arms: whole identifiers and their parts, stopwords dropped,
    /// longest first (see [`terms`]).
    pub words: Vec<String>,
    /// The bare identifier the query is, if it is one (see [`route`]).
    pub ident: Option<String>,
    /// The query contains a qualifier (`Type.member`, `a::b`).
    pub qualified: bool,
}

impl Terms {
    pub fn new(q: &str) -> Terms {
        let ident = identifier(q).map(str::to_string);
        Terms { words: terms_capped(q, if ident.is_some() { 8 } else { BODY_TERMS }), ident, qualified: q.contains(['.', ':']) }
    }
}

/// Which text a keyword search reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// Names, qualified names and paths only: identifier lookups.
    Names,
    /// Everything, symbol doc comments and bodies included: questions in words.
    Body,
}

/// Where a query goes. A bare identifier reads names (exact, keyword over names, prefix,
/// trigram, graph); another single token (a path fragment) reads names and trigrams; words
/// read body text in place of names (exact, body keyword). Mixing names into a question's
/// arms hurt it in the retrieval eval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Ident(String),
    Token,
    Text,
}

pub fn route(q: &str) -> Route {
    match identifier(q) {
        Some(i) => Route::Ident(i.to_string()),
        None if q.trim().contains(char::is_whitespace) => Route::Text,
        None => Route::Token,
    }
}

/// Keyword, substring and prefix retrieval: ranked node ids. SQLite FTS5 implements it today;
/// a tantivy index can implement it later behind the same calls.
pub trait SearchBackend {
    fn keyword(&self, t: &Terms, field: Field, s: &Scope, k: usize) -> Result<Vec<i64>>;
    /// Ids whose path or name contains `needle` (case-insensitive substring).
    fn substring(&self, needle: &str, s: &Scope, k: usize) -> Result<Vec<i64>>;
    /// Ids whose name starts with `ident` (case-insensitive), shortest name first.
    fn prefix(&self, ident: &str, s: &Scope, k: usize) -> Result<Vec<i64>>;
}

/// One change to the searchable rows, for an index kept beside SQLite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub rowid: i64,
    pub project_id: i64,
    pub op: ChangeOp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeOp {
    Upsert,
    Delete,
}

/// The write side of a derived search index. SQLite stays the system of record: an index
/// applies changes in order (`seq`), reports the generation it reflects per project, and can
/// always be rebuilt from SQLite.
pub trait IndexSink {
    fn apply(&mut self, changes: &[Change], seq: u64) -> Result<()>;
    fn generation(&self, project: i64) -> u64;
    fn rebuild(&mut self, project: i64) -> Result<()>;
}

/// Nearest neighbours of a query vector inside a scope: (node id, similarity), best first.
pub trait VectorBackend {
    fn nearest(&self, v: &[f32], s: &Scope, k: usize) -> Result<Vec<(i64, f32)>>;
}

/// Text to vectors. `vectors::Model` implements it in builds with the `vectors` feature.
pub trait Embedder {
    fn dims(&self) -> usize;
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}

/// No vector arm: no neighbours. (`vectors::Index` is the opt-in one.)
pub struct NoVectors;

impl VectorBackend for NoVectors {
    fn nearest(&self, _v: &[f32], _s: &Scope, _k: usize) -> Result<Vec<(i64, f32)>> {
        Ok(vec![])
    }
}

/// SQL fragments restricting kind and project, rendered from a [`Scope`]. Built only from fixed
/// strings, integer ids and an escaped path prefix.
#[derive(Clone)]
pub struct Filter {
    pub kind_sql: String,
    pub scope_sql: String,
    /// Node id ranges holding every row in scope (see `db::cover`; they may hold other rows
    /// too), set when the scope is a minority of a shared store: full-text arms then read only
    /// these slices of the index, so a busy neighbouring project cannot slow them. None reads
    /// the whole index. Either way the scope is checked per match before anything is ranked or
    /// cut, so a neighbour cannot push this project's matches out of the window.
    pub ranges: Option<Vec<(i64, i64)>>,
    /// Ids the scope holds (its exact ranges' span), for in-scope term weights.
    pub size: i64,
    /// The scope may hold non-symbol rows (docs, files, notes).
    pub other_kinds: bool,
    /// The scope may hold symbols.
    pub symbols: bool,
}

/// The node kinds behind a `--kind` value (empty: any).
pub fn kinds_for(kind: &str) -> &'static [&'static str] {
    match kind {
        "file" => &["file"],
        "dir" => &["artifact_dir"],
        "doc" => &["doc_section", "milestone"],
        "milestone" => &["milestone"],
        "memory" => &["memory", "session"],
        "recall" => &["memory", "session", "milestone", "doc_section"],
        "symbol" => &["symbol"],
        _ => &[],
    }
}

/// The scope of a search from the current project and the options: that project plus the
/// global notes, unless `--all`.
pub fn scope(c: &Connection, cur: Option<&Project>, o: &FindOpts) -> Scope {
    let projects = match (cur, o.all) {
        (Some(p), false) => {
            let global: Option<i64> = c.query_row("SELECT id FROM project WHERE name = '~global'", [], |r| r.get(0)).ok();
            std::iter::once(p.id).chain(global).collect()
        }
        _ => vec![],
    };
    Scope { projects, kinds: kinds_for(o.kind).iter().map(|k| k.to_string()).collect(), path_prefix: None }
}

impl Filter {
    /// Render `s` as SQL. With `c`, a scope that is a minority of a shared store also gets the
    /// id ranges its full-text arms read.
    pub fn render(c: Option<&Connection>, s: &Scope) -> Filter {
        let kind_sql = if s.kinds.is_empty() {
            String::new()
        } else {
            format!(" AND n.kind IN ({})", s.kinds.iter().map(|k| format!("'{}'", k.replace('\'', "''"))).collect::<Vec<_>>().join(","))
        } + " AND NOT (n.kind = 'symbol' AND n.path IS NULL)";
        let mut scope_sql = match s.projects.as_slice() {
            [] => String::new(),
            // Unary `+`: the scope is a filter, never the index to drive the query from.
            ps => format!(" AND (+n.project_id IN ({}))", ps.iter().map(i64::to_string).collect::<Vec<_>>().join(",")),
        };
        if let Some(pre) = s.path_prefix.as_deref().filter(|p| !p.is_empty()) {
            let lit = pre.replace('\'', "''");
            scope_sql += &format!(" AND substr(n.path, 1, {}) = '{lit}'", pre.chars().count());
        }
        let symbols = s.kinds.is_empty() || s.kinds.iter().any(|k| k == "symbol");
        let other_kinds = s.kinds.is_empty() || s.kinds.iter().any(|k| k != "symbol");
        let mut f = Filter { kind_sql, scope_sql, ranges: None, size: 0, other_kinds, symbols };
        if let (Some(c), false) = (c, s.projects.is_empty())
            && let Some(rs) = crate::db::ranges(c, &s.projects)
            && crate::db::minority(c, &rs)
        {
            // A bounded list of slices; if bridging the interleaved saves made them span most
            // of the store, read the whole index instead (the scope is still applied per match).
            let cov = crate::db::cover(&rs, MAX_RANGES);
            f.size = crate::db::span(&rs);
            f.ranges = (crate::db::span(&cov) * 2 < crate::db::max_id(c)).then_some(cov);
        }
        f
    }
}

/// Split an identifier into lowercase parts: camelCase, PascalCase, snake_case, acronyms, digits.
pub fn ident_parts(s: &str) -> Vec<String> {
    let mut parts = vec![];
    let mut cur = String::new();
    let cs: Vec<char> = s.chars().collect();
    for (i, &ch) in cs.iter().enumerate() {
        if !ch.is_alphanumeric() {
            if !cur.is_empty() {
                parts.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let prev = if i > 0 { Some(cs[i - 1]) } else { None };
        let next = cs.get(i + 1).copied();
        let boundary = match prev {
            Some(p) if p.is_alphanumeric() => {
                (ch.is_uppercase() && (p.is_lowercase() || p.is_ascii_digit()))
                    || (ch.is_uppercase() && p.is_uppercase() && next.is_some_and(|n| n.is_lowercase()))
                    || (ch.is_ascii_digit() != p.is_ascii_digit())
            }
            _ => false,
        };
        if boundary && !cur.is_empty() {
            parts.push(std::mem::take(&mut cur));
        }
        cur.extend(ch.to_lowercase());
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

/// Most terms a sentence query keeps for the body arm (identifier lookups keep 8). In the
/// retrieval eval 12 found as much as 16.
const BODY_TERMS: usize = 12;

/// A body-arm word found in more than this share of the store's rows is left out: BM25 gives it
/// little weight, yet scoring its matches is much of a question's search time. In the retrieval
/// eval 0.1 lost nothing and 0.05 already lost vague questions.
const COMMON_SHARE: f64 = 0.1;

/// Fewest words a body query keeps, common or not (the rarest ones).
const MIN_WORDS: usize = 3;

/// `words` without the ones `table` holds in more than [`COMMON_SHARE`] of the store's rows,
/// keeping at least the [`MIN_WORDS`] rarest. Unchanged when the vocabulary view is missing.
fn prune_common(c: &Connection, table: &str, words: &[String]) -> Vec<String> {
    let Ok(mut q) = c.prepare_cached(&format!("SELECT doc FROM {table}_v WHERE term = ?1")) else { return words.to_vec() };
    let limit = (crate::db::max_id(c) as f64 * COMMON_SHARE).max(50.0) as i64;
    let mut df: Vec<(i64, &String)> = words.iter().map(|w| (q.query_row([w], |r| r.get::<_, i64>(0)).unwrap_or(0), w)).collect();
    if df.iter().filter(|x| x.0 <= limit).count() >= MIN_WORDS {
        return df.into_iter().filter(|x| x.0 <= limit).map(|x| x.1.clone()).collect();
    }
    df.sort_by_key(|x| x.0);
    let keep: HashSet<&String> = df.iter().take(MIN_WORDS).map(|x| x.1).collect();
    words.iter().filter(|w| keep.contains(w)).cloned().collect()
}

/// Normalized query terms: whole identifiers plus their parts, stopwords and one-letter terms
/// dropped, deduplicated, longest (rarest-looking) first, capped at 8. A snake_case word's
/// whole form is squashed (`omap_dma` → `omapdma`), the one term its name indexes as.
pub fn terms(q: &str) -> Vec<String> {
    terms_capped(q, 8)
}

fn terms_capped(q: &str, cap: usize) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut seen = HashSet::new();
    for word in q.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()) {
        let whole = squash(word);
        let parts = ident_parts(word);
        for t in std::iter::once(whole).chain(parts) {
            let t = t.trim_matches('_').to_string();
            if t.chars().count() < 2 || STOPWORDS.contains(&t.as_str()) {
                continue;
            }
            if seen.insert(t.clone()) {
                out.push(t);
            }
        }
    }
    out.sort_by(|a, b| b.chars().count().cmp(&a.chars().count()).then(a.cmp(b)));
    out.truncate(cap);
    out
}

/// One bare identifier (no spaces): exact, name and graph arms apply.
fn identifier(q: &str) -> Option<&str> {
    let q = q.trim();
    if q.is_empty() || q.contains(char::is_whitespace) {
        return None;
    }
    let last = q.rsplit(['.', ':']).find(|s| !s.is_empty()).unwrap_or(q);
    let last = last.split('(').next().unwrap_or(last);
    (!last.is_empty() && last.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$')).then_some(last)
}

/// The SQLite FTS5 backend. Holds a scope already rendered by the caller (ranges included), so
/// arms running in parallel do not render it again; any other scope is rendered on the call.
pub struct Sqlite<'a> {
    c: &'a Connection,
    rendered: Option<(&'a Scope, &'a Filter)>,
}

impl<'a> Sqlite<'a> {
    pub fn new(c: &'a Connection) -> Sqlite<'a> {
        Sqlite { c, rendered: None }
    }

    pub fn with_filter(c: &'a Connection, s: &'a Scope, f: &'a Filter) -> Sqlite<'a> {
        Sqlite { c, rendered: Some((s, f)) }
    }

    fn filter(&self, s: &Scope) -> std::borrow::Cow<'a, Filter> {
        match self.rendered {
            Some((rs, f)) if rs == s => std::borrow::Cow::Borrowed(f),
            _ => std::borrow::Cow::Owned(Filter::render(Some(self.c), s)),
        }
    }
}

fn quoted(t: &str, prefix: bool) -> String {
    format!("\"{}\"{}", t.replace('"', ""), if prefix { "*" } else { "" })
}

/// FTS5 expression for normalized terms. Words of a sentence are OR'ed and ranked by BM25.
/// Prefix matching is kept to terms of four or more characters, where it is cheap; body text
/// is matched by whole words only (its vocabulary is large, so prefixes there are not cheap).
pub fn match_expr(terms: &[String], prefix: bool) -> String {
    terms.iter().map(|t| quoted(t, prefix && t.chars().count() >= 4)).collect::<Vec<_>>().join(" OR ")
}

/// A single identifier matches as a whole (prefix) or as all of its parts together. A
/// snake_case identifier's whole form is its squashed word (see `inventory::split_ident`).
pub fn ident_expr(ident: &str) -> String {
    let parts = ident_parts(ident);
    let whole = quoted(&squash(ident), true);
    if parts.len() < 2 {
        return whole;
    }
    let n = parts.len();
    let all = parts.iter().enumerate().map(|(i, p)| quoted(p, i + 1 == n && p.chars().count() >= 3)).collect::<Vec<_>>().join(" AND ");
    format!("{whole} OR ({all})")
}

/// An identifier lowercased, underscores dropped: the one term a snake_case name indexes as.
pub fn squash(ident: &str) -> String {
    ident.to_lowercase().chars().filter(|c| *c != '_').collect()
}

impl SearchBackend for Sqlite<'_> {
    fn keyword(&self, t: &Terms, field: Field, s: &Scope, k: usize) -> Result<Vec<i64>> {
        let f = self.filter(s);
        match field {
            Field::Names => {
                let expr = match t.ident.as_deref() {
                    Some(i) if !t.qualified => ident_expr(i),
                    _ => match_expr(&t.words, true),
                };
                Ok(fts_keyword(self.c, "fts_node", &expr, &f, k)?.into_iter().map(|x| x.0).collect())
            }
            Field::Body => {
                // Symbols from the body table; docs, files and notes keep their text in
                // `fts_node`. Both are scored with the same column weights, so one list is
                // merged by score (interleaving by rank gave files half the places). The two
                // run side by side when a second connection opens.
                let sym = f.symbols.then(|| {
                    let mut sf = f.as_ref().clone();
                    sf.kind_sql += " AND n.kind = 'symbol'";
                    sf
                });
                let other = f.other_kinds.then(|| {
                    let mut of = f.as_ref().clone();
                    of.kind_sql += " AND n.kind != 'symbol'";
                    of
                });
                let run = |c: &Connection, table: &str, f: &Filter| -> Result<Vec<(i64, f64)>> {
                    fts_keyword(c, table, &match_expr(&prune_common(c, table, &t.words), false), f, k)
                };
                let mut all: Vec<(i64, f64)> = vec![];
                match (sym, other) {
                    (Some(sf), Some(of)) => {
                        let side = crate::db::open_ro();
                        let run = &run;
                        let (a, b) = std::thread::scope(|s| {
                            let h = side.map(|c2| {
                                let of = of.clone();
                                s.spawn(move || run(&c2, "fts_node", &of))
                            });
                            let a = run(self.c, "fts_body", &sf);
                            let b = match h {
                                Some(h) => h.join().unwrap_or_else(|_| Ok(vec![])),
                                None => run(self.c, "fts_node", &of),
                            };
                            (a, b)
                        });
                        all.extend(a?);
                        all.extend(b?);
                    }
                    (Some(sf), None) => all.extend(run(self.c, "fts_body", &sf)?),
                    (None, Some(of)) => all.extend(run(self.c, "fts_node", &of)?),
                    (None, None) => {}
                }
                all.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                Ok(all.into_iter().take(k).map(|x| x.0).collect())
            }
        }
    }

    fn substring(&self, needle: &str, s: &Scope, k: usize) -> Result<Vec<i64>> {
        substring(self.c, needle, &self.filter(s), k)
    }

    fn prefix(&self, ident: &str, s: &Scope, k: usize) -> Result<Vec<i64>> {
        let f = self.filter(s);
        // A NOCASE range scan on the name index: [ident, ident + U+10FFFF).
        let hi = format!("{ident}\u{10FFFF}");
        let sql = format!(
            "SELECT n.id FROM node n INDEXED BY node_name WHERE n.name >= ?1 COLLATE NOCASE AND n.name < ?2 COLLATE NOCASE{}{}
             ORDER BY length(n.name), n.id LIMIT {k}",
            f.kind_sql, f.scope_sql
        );
        let mut st = self.c.prepare_cached(&sql)?;
        let ids = st.query_map([ident, hi.as_str()], |r| r.get(0))?.flatten().collect();
        Ok(ids)
    }
}

/// Keyword retrieval inside a project's id ranges, for a store where other projects hold most
/// of the index. BM25 would read every project's postings for its term statistics, so here the
/// matches (prefix terms included) are weighted by in-scope frequency instead: a whole-word
/// match in the name counts 10, signature 4, path 2, body 1, each times the term's in-scope IDF,
/// and a prefix-only match counts 1 times its IDF, so it ranks below whole-word matches.
fn scoped_keyword(c: &Connection, table: &str, expr: &str, f: &Filter, rs: &[(i64, i64)], k: usize) -> Result<Vec<(i64, f64)>> {
    let exact = expr.replace("\"*", "\"");
    let mut prefixes: Vec<&str> = expr.split('"').collect::<Vec<_>>().windows(2).filter(|w| w[1].starts_with('*')).map(|w| w[0]).collect();
    prefixes.sort_unstable();
    prefixes.dedup();
    let terms: Vec<String> = {
        let mut v: Vec<String> = exact.split('"').skip(1).step_by(2).map(String::from).filter(|t| !t.is_empty()).collect();
        v.sort();
        v.dedup();
        v
    };
    let in_ranges = |m: &str, extra: &str| -> Result<Vec<i64>> {
        let mut out = vec![];
        for (lo, hi) in rs {
            let mut q =
                c.prepare_cached(&format!("SELECT rowid FROM {table} WHERE {table} MATCH ?1 AND rowid BETWEEN {lo} AND {hi}{extra}"))?;
            out.extend(q.query_map([m], |r| r.get::<_, i64>(0))?.flatten());
        }
        Ok(out)
    };
    // The scope and kind are checked per match: the ranges may hold other projects' rows.
    let mut score: HashMap<i64, f64> = in_ranges(expr, &prefilter(f, table))?.into_iter().map(|id| (id, 0.0)).collect();
    if score.is_empty() {
        return Ok(vec![]);
    }
    let total = f.size.max(1) as f64;
    for t in &prefixes {
        let n = in_ranges(&format!("\"{t}\"*"), "")?;
        let idf = ((total - n.len() as f64 + 0.5) / (n.len() as f64 + 0.5) + 1.0).ln();
        for id in n {
            if let Some(s) = score.get_mut(&id) {
                *s += idf;
            }
        }
    }
    for t in &terms {
        let quoted = format!("\"{t}\"");
        let n = in_ranges(&quoted, "")?.len() as f64;
        let idf = ((total - n + 0.5) / (n + 0.5) + 1.0).ln();
        for (col, w) in [("name", 10.0), ("sig", 4.0), ("path", 2.0), ("body", 1.0)] {
            for id in in_ranges(&format!("{{{col}}}: {quoted}"), "")? {
                if let Some(s) = score.get_mut(&id) {
                    *s += w * idf;
                }
            }
        }
    }
    let mut ranked: Vec<(i64, f64)> = score.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.truncate(1000);
    let order: HashMap<i64, (usize, f64)> = ranked.iter().enumerate().map(|(i, x)| (x.0, (i, x.1))).collect();
    let list = ranked.iter().map(|x| x.0.to_string()).collect::<Vec<_>>().join(",");
    let mut q = c.prepare(&format!("SELECT n.id FROM node n WHERE n.id IN ({list}){}{}", f.kind_sql, f.scope_sql))?;
    let mut ids: Vec<i64> = q.query_map([], |r| r.get(0))?.flatten().collect();
    ids.sort_by_key(|id| order[id].0);
    ids.truncate(k);
    Ok(ids.into_iter().map(|id| (id, order[&id].1)).collect())
}

/// The filter as a per-match condition inside the FTS query: only matches that pass are scored
/// and ranked. Empty when nothing restricts the search.
fn prefilter(f: &Filter, table: &str) -> String {
    format!(" AND EXISTS (SELECT 1 FROM node n WHERE n.id = {table}.rowid{}{})", f.kind_sql, f.scope_sql)
}

/// BM25 keyword retrieval over one full-text table (columns name, sig, path, body).
/// Each id comes with its score, higher better, comparable between tables searched the same way.
fn fts_keyword(c: &Connection, table: &str, expr: &str, f: &Filter, k: usize) -> Result<Vec<(i64, f64)>> {
    if expr.is_empty() {
        return Ok(vec![]);
    }
    if let Some(rs) = &f.ranges {
        return scoped_keyword(c, table, expr, f, rs, k);
    }
    // In a scope, only matches inside it are scored and ranked (one id lookup per match).
    let pre = if f.scope_sql.is_empty() { String::new() } else { prefilter(f, table) };
    let inner = format!("SELECT rowid AS rid, bm25({table}, 10.0, 4.0, 2.0, 1.0) AS score FROM {table} WHERE {table} MATCH ?1{pre}");
    let sql = format!(
        "SELECT n.id, -x.score FROM (SELECT rid, score FROM ({inner}) ORDER BY score LIMIT 1000) x CROSS JOIN node n ON n.id = x.rid
         WHERE 1{}{} ORDER BY x.score LIMIT {k}",
        f.kind_sql, f.scope_sql
    );
    let mut s = c.prepare_cached(&sql)?;
    let ids = s.query_map([expr], |r| Ok((r.get(0)?, r.get(1)?)))?.flatten().collect();
    Ok(ids)
}

/// Ranked ids whose path or name contains `needle` (trigram index).
fn substring(c: &Connection, needle: &str, f: &Filter, k: usize) -> Result<Vec<i64>> {
    if needle.chars().count() < 3 {
        return Ok(vec![]);
    }
    let quoted = format!("\"{}\"", needle.replace('"', ""));
    let Some(rs) = &f.ranges else {
        let pre = if f.scope_sql.is_empty() { String::new() } else { prefilter(f, "fts_trigram") };
        let sql = format!(
            "SELECT n.id FROM (SELECT rowid AS rid, rank FROM fts_trigram WHERE fts_trigram MATCH ?1{pre} ORDER BY rank LIMIT 500) t
               CROSS JOIN node n ON n.id = t.rid WHERE 1{}{} ORDER BY t.rank LIMIT {k}",
            f.kind_sql, f.scope_sql
        );
        let mut s = c.prepare_cached(&sql)?;
        let ids = s.query_map([quoted], |r| r.get(0))?.flatten().collect();
        return Ok(ids);
    };
    // In scope: no BM25 (its statistics would scan every project's matches). Names that
    // contain the needle first, shortest first, then paths that do, shortest first.
    let kinds = prefilter(f, "fts_trigram");
    let mut out: Vec<i64> = vec![];
    for col in ["name", "path"] {
        if rs.is_empty() || out.len() >= k {
            break;
        }
        let inner = rs
            .iter()
            .map(|(lo, hi)| {
                format!(
                    "SELECT * FROM (SELECT rowid AS rid FROM fts_trigram WHERE fts_trigram MATCH ?1 AND rowid BETWEEN {lo} AND {hi}{kinds} LIMIT 5000)"
                )
            })
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        let order = if col == "name" { "length(n.name), length(n.path)" } else { "length(n.path), length(n.name)" };
        let sql = format!(
            "SELECT n.id FROM ({inner}) t CROSS JOIN node n ON n.id = t.rid WHERE 1{}{} ORDER BY {order}, n.id LIMIT {k}",
            f.kind_sql, f.scope_sql
        );
        let mut s = c.prepare_cached(&sql)?;
        let expr = format!("{{{col}}}: {quoted}");
        for id in s.query_map([expr], |r| r.get::<_, i64>(0))?.flatten() {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out.truncate(k);
    Ok(out)
}

fn exact_arm(c: &Connection, q: &str, f: &Filter, k: usize) -> Result<Vec<i64>> {
    let q = q.trim();
    let mut ids: Vec<i64> = {
        let sql = format!(
            "SELECT n.id FROM node n WHERE n.name = ?1 COLLATE NOCASE{}{} ORDER BY n.kind != 'symbol', n.id LIMIT {k}",
            f.kind_sql, f.scope_sql
        );
        let mut s = c.prepare_cached(&sql)?;
        s.query_map([q], |r| r.get(0))?.flatten().collect()
    };
    // `Type.member`: the member, restricted to that parent.
    if ids.is_empty()
        && let Some((parent, member)) = q.rsplit_once('.')
        && !member.is_empty()
    {
        let parent = parent.rsplit('.').next().unwrap_or(parent);
        let sql = format!(
            "SELECT n.id FROM node n WHERE n.name = ?1 COLLATE NOCASE AND json_extract(n.meta, '$.parent') = ?2 COLLATE NOCASE{}{} LIMIT {k}",
            f.kind_sql, f.scope_sql
        );
        let mut s = c.prepare_cached(&sql)?;
        ids = s.query_map([member.split('(').next().unwrap_or(member), parent], |r| r.get(0))?.flatten().collect();
    }
    Ok(ids)
}

/// Neighbours (callers and callees) of the symbols named `ident` in project `pid`, most connected
/// first. Uses the warm CSR graph when given, else SQL.
pub fn graph_arm(c: &Connection, g: Option<&Graph>, ident: &str, pid: i64, k: usize) -> Result<Vec<i64>> {
    let seeds: Vec<i64> = {
        let mut s = c.prepare_cached(
            "SELECT id FROM node WHERE project_id = ?1 AND kind = 'symbol' AND name = ?2 COLLATE NOCASE ORDER BY id LIMIT 20",
        )?;
        s.query_map(rusqlite::params![pid, ident], |r| r.get(0))?.flatten().collect()
    };
    if seeds.is_empty() {
        return Ok(vec![]);
    }
    let seed_set: HashSet<i64> = seeds.iter().copied().collect();
    let mut count: HashMap<i64, usize> = HashMap::new();
    match g {
        Some(g) => {
            for &s in &seeds {
                for nb in g.callees(s).chain(g.callers(s)) {
                    if !seed_set.contains(&nb) {
                        *count.entry(nb).or_default() += 1;
                    }
                }
            }
        }
        None => {
            let ids = seeds.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT nb FROM (
                   SELECT DISTINCT src, dst AS nb FROM ts_ref WHERE src IN ({ids}) AND src != 0 AND dst != 0
                   UNION SELECT DISTINCT dst, src FROM ts_ref WHERE dst IN ({ids}) AND dst != 0 AND src != 0
                   UNION SELECT src, dst FROM edge WHERE src IN ({ids}) AND kind IN ('CALLS','REFERENCES')
                   UNION SELECT dst, src FROM edge WHERE dst IN ({ids}) AND kind IN ('CALLS','REFERENCES'))"
            );
            let mut s = c.prepare(&sql)?;
            for nb in s.query_map([], |r| r.get::<_, i64>(0))?.flatten() {
                if !seed_set.contains(&nb) {
                    *count.entry(nb).or_default() += 1;
                }
            }
        }
    }
    let mut v: Vec<(i64, usize)> = count.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Ok(v.into_iter().take(k).map(|x| x.0).collect())
}

pub fn is_test_path(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    l.contains("/test/")
        || l.contains("/tests/")
        || l.starts_with("test/")
        || l.starts_with("tests/")
        || l.contains("/androidtest/")
        || l.contains("__tests__")
        || l.contains("/spec/")
        || l.contains(".spec.")
        || l.contains(".test.")
        || l.contains("_test.")
        || p.rsplit('/').next().is_some_and(|f| f.contains("Test.") || f.contains("Tests.") || f.starts_with("test_"))
}

pub fn is_vendor_or_generated(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    ["vendor/", "third_party/", "thirdparty/", "external/", "pods/", "node_modules/", "generated/", "/build/", "dist/", "target/"]
        .iter()
        .any(|d| l.starts_with(d) || l.contains(&format!("/{d}")))
        || [".g.dart", ".pb.go", "_pb2.py", ".min.js", ".generated.", "_generated."].iter().any(|s| l.contains(s))
}

/// Fused candidates in final order. `arms` are (weight, ranked ids).
fn fuse(arms: &[(f64, Vec<i64>)]) -> Vec<(i64, f64)> {
    let mut score: HashMap<i64, f64> = HashMap::new();
    for (w, ids) in arms {
        for (r, id) in ids.iter().enumerate() {
            *score.entry(*id).or_default() += w / (RRF_K + r as f64 + 1.0);
        }
    }
    let mut v: Vec<(i64, f64)> = score.into_iter().collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
    v
}

const HIT_COLS: &str = "n.id, n.kind, n.name, n.path, n.line, n.tier, n.size, n.meta, p.name, n.stale, n.sig";

fn load_hits(c: &Connection, ids: &[i64], f: &Filter) -> Result<HashMap<i64, Hit>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let sql =
        format!("SELECT {HIT_COLS} FROM node n JOIN project p ON p.id = n.project_id WHERE n.id IN ({list}){}{}", f.kind_sql, f.scope_sql);
    let mut s = c.prepare(&sql)?;
    let rows = s.query_map([], |r| {
        Ok(Hit {
            id: r.get(0)?,
            kind: r.get(1)?,
            name: r.get(2)?,
            path: r.get(3)?,
            line: r.get(4)?,
            tier: r.get(5)?,
            size: r.get(6)?,
            meta: r.get(7)?,
            project: r.get(8)?,
            stale: r.get::<_, i64>(9)? != 0,
            sig: r.get(10)?,
            refs: None,
        })
    })?;
    Ok(rows.flatten().map(|h| (h.id, h)).collect())
}

/// Code-aware multipliers on top of the fused score.
fn feature_boost(h: &Hit, q: &str, ident: Option<&str>) -> f64 {
    let mut m = 1.0;
    let path = h.path.as_deref().unwrap_or("");
    let name_eq = |s: &str| ident.is_some_and(|i| s.eq_ignore_ascii_case(i)) || s.eq_ignore_ascii_case(q.trim());
    match h.kind.as_str() {
        // The definition of the queried name.
        "symbol" if name_eq(&h.name) => m *= 3.0,
        // A heading whose title is the query is its definition too.
        "doc_section" | "milestone" if name_eq(&h.name) => m *= 3.0,
        "file" => {
            let stem = h.name.split('.').next().unwrap_or(&h.name);
            if name_eq(stem) || name_eq(&h.name) {
                m *= 1.5;
            }
        }
        _ => {}
    }
    if is_test_path(path) {
        m *= 0.3;
    }
    if is_vendor_or_generated(path) {
        m *= 0.3;
    }
    m
}

/// Same-file damping for identifier lookups: each further hit from a file counts half again.
const DAMP_NAMES: f64 = 0.5;
/// For questions in words, the answer often sits beside a better-scoring sibling in its file.
/// Fused scores are flat (1/61, 1/62, ...), so halving one moved it some sixty places; this
/// moves a repeat a few places instead.
const DAMP_TEXT: f64 = 0.95;

/// Greedy re-order: each further hit from a file already shown counts `damp` times as much again.
fn damp_same_file(mut v: Vec<(Hit, f64)>, damp: f64) -> Vec<Hit> {
    let mut out = Vec::with_capacity(v.len());
    let mut per_file: HashMap<String, i32> = HashMap::new();
    while !v.is_empty() {
        let (mut best, mut best_s) = (0, f64::MIN);
        for (i, (h, s)) in v.iter().enumerate() {
            let n = h.path.as_ref().map(|p| per_file.get(p).copied().unwrap_or(0)).unwrap_or(0);
            let adj = s * damp.powi(n);
            if adj > best_s {
                best = i;
                best_s = adj;
            }
        }
        let (h, _) = v.remove(best);
        if let Some(p) = &h.path {
            *per_file.entry(p.clone()).or_default() += 1;
        }
        out.push(h);
    }
    out
}

type ArmFn<'a> = Box<dyn FnOnce(&Connection) -> Result<Vec<i64>> + Send + 'a>;

/// Readers on derived indexes that a long-lived process keeps between searches.
#[cfg(feature = "tantivy")]
pub type DerivedCache = HashMap<i64, crate::tantivy_index::Cached>;
#[cfg(not(feature = "tantivy"))]
pub type DerivedCache = ();

/// The engine behind the keyword and substring arms for one search.
#[derive(Clone)]
enum Engine {
    Fts5,
    #[cfg(feature = "tantivy")]
    Tantivy(crate::tantivy_index::Handle, Scope),
}

impl Engine {
    /// The scope's project's tantivy index when it has one that is current; FTS5 otherwise.
    #[allow(unused_variables)]
    fn choose(c: &Connection, sc: &Scope, cache: Option<&mut DerivedCache>) -> Engine {
        #[cfg(feature = "tantivy")]
        if let Some((h, rest)) = crate::tantivy_index::for_scope(c, sc, cache) {
            return Engine::Tantivy(h, rest);
        }
        Engine::Fts5
    }

    fn keyword(&self, c: &Connection, sc: &Scope, f: &Filter, t: &Terms, field: Field, k: usize) -> Result<Vec<i64>> {
        match self {
            Engine::Fts5 => Sqlite::with_filter(c, sc, f).keyword(t, field, sc, k),
            // An index that fails mid-search (a segment gone under a reader) or panics (a damaged
            // file) never empties the arm: FTS5 answers instead.
            #[cfg(feature = "tantivy")]
            Engine::Tantivy(h, rest) => {
                crate::tantivy_index::search(h.pid, || crate::tantivy_index::Tantivy { c, h, rest }.keyword(t, field, sc, k))
                    .or_else(|_| Sqlite::with_filter(c, sc, f).keyword(t, field, sc, k))
            }
        }
    }

    fn substring(&self, c: &Connection, sc: &Scope, f: &Filter, needle: &str, k: usize) -> Result<Vec<i64>> {
        match self {
            Engine::Fts5 => Sqlite::with_filter(c, sc, f).substring(needle, sc, k),
            #[cfg(feature = "tantivy")]
            Engine::Tantivy(h, rest) => {
                crate::tantivy_index::search(h.pid, || crate::tantivy_index::Tantivy { c, h, rest }.substring(needle, sc, k))
                    .or_else(|_| Sqlite::with_filter(c, sc, f).substring(needle, sc, k))
            }
        }
    }
}

/// Run arms concurrently: the first on `main`, the others on the pool's connections. Falls back
/// to running them one after another on `main` when the pool is too small.
fn run_arms(main: &Connection, pool: &mut [Connection], arms: Vec<(f64, ArmFn<'_>)>) -> Vec<(f64, Vec<i64>)> {
    if arms.is_empty() {
        return vec![];
    }
    if pool.len() + 1 < arms.len() {
        return arms
            .into_iter()
            .enumerate()
            .map(|(i, (w, f))| {
                let t = std::time::Instant::now();
                let r = (w, f(main).unwrap_or_default());
                trace(&format!("arm {i}"), t);
                r
            })
            .collect();
    }
    std::thread::scope(|s| {
        let mut it = arms.into_iter();
        let (w0, f0) = it.next().unwrap();
        let handles: Vec<_> = it
            .zip(pool.iter_mut())
            .enumerate()
            .map(|(i, ((w, f), conn))| {
                s.spawn(move || {
                    let t = std::time::Instant::now();
                    let r = (w, f(conn).unwrap_or_default());
                    trace(&format!("arm {} (parallel)", i + 1), t);
                    r
                })
            })
            .collect();
        let mut out = vec![(w0, f0(main).unwrap_or_default())];
        for h in handles {
            if let Ok(r) = h.join() {
                out.push(r);
            }
        }
        out
    })
}

/// Shared search state for long-lived processes: CSR graph, connection pool and a result cache,
/// all keyed by the index generation. The arms themselves are the same as the cold path's, so a
/// warm answer is byte-identical to a cold one.
#[derive(Default)]
pub struct Warm {
    generation: Option<i64>,
    /// Per project: (project generation it was built at, graph).
    graphs: HashMap<i64, (i64, Graph)>,
    /// Graphs being built in the background for searches: (project generation, builder).
    pending: HashMap<i64, (i64, std::thread::JoinHandle<Option<Graph>>)>,
    graph_loads: usize,
    pool: Vec<Connection>,
    cache: HashMap<String, String>,
    hits: usize,
    derived: DerivedCache,
}

const CACHE_MAX: usize = 1024;

impl Warm {
    /// Drop cached answers if the index generation moved. Cheap when it did not. Graphs are
    /// rebuilt lazily, per project, by [`Warm::graph`].
    pub fn refresh(&mut self, c: &Connection) -> Result<bool> {
        let g = crate::db::generation(c);
        if self.generation == Some(g) {
            return Ok(false);
        }
        self.cache.clear();
        self.generation = Some(g);
        Ok(true)
    }

    /// Project `pid`'s call graph, rebuilt only when that project's own generation moved.
    pub fn graph(&mut self, c: &Connection, pid: i64) -> Result<&Graph> {
        let g = crate::db::project_generation(c, pid);
        if self.graphs.get(&pid).is_none_or(|x| x.0 != g) {
            let t = std::time::Instant::now();
            let graph = Graph::load_project(c, pid)?;
            trace("graph load", t);
            self.graph_loads += 1;
            self.graphs.insert(pid, (g, graph));
        }
        Ok(&self.graphs[&pid].1)
    }

    /// Project `pid`'s graph if it is current; otherwise start building it in the background
    /// (once per generation) and return None, so the caller walks SQL instead of waiting.
    fn graph_now(&mut self, c: &Connection, pid: i64) -> Option<&Graph> {
        self.collect(false);
        let g = crate::db::project_generation(c, pid);
        if self.graphs.get(&pid).is_some_and(|x| x.0 == g) {
            return Some(&self.graphs[&pid].1);
        }
        if self.pending.get(&pid).is_none_or(|x| x.0 != g) {
            let h = std::thread::spawn(move || {
                let t = std::time::Instant::now();
                let graph = Graph::load_project(&crate::db::open_ro()?, pid).ok();
                trace("graph load (background)", t);
                graph
            });
            // A builder for an older generation is left to finish on its own; its graph is dropped.
            self.pending.insert(pid, (g, h));
        }
        None
    }

    /// Install background graphs that finished (all of them, waiting, when `wait`).
    fn collect(&mut self, wait: bool) {
        let done: Vec<i64> = self.pending.iter().filter(|(_, x)| wait || x.1.is_finished()).map(|(p, _)| *p).collect();
        for pid in done {
            let Some((g, h)) = self.pending.remove(&pid) else { continue };
            if let Ok(Some(graph)) = h.join()
                && self.graphs.get(&pid).is_none_or(|x| x.0 < g)
            {
                self.graph_loads += 1;
                self.graphs.insert(pid, (g, graph));
            }
        }
    }

    /// Wait for background graph builds and install them.
    pub fn settle(&mut self) {
        self.collect(true);
    }

    /// How many project graphs were built so far.
    pub fn graph_loads(&self) -> usize {
        self.graph_loads
    }

    /// `key`'s answer from the generation-keyed cache, or `f()`'s, cached. For read verbs whose
    /// answer depends only on the index (not on files read live from disk).
    pub fn cached(&mut self, c: &Connection, key: String, f: impl FnOnce() -> Result<String>) -> Result<String> {
        self.refresh(c)?;
        if let Some(hit) = self.cache.get(&key) {
            self.hits += 1;
            return Ok(hit.clone());
        }
        let out = f()?;
        if self.cache.len() >= CACHE_MAX {
            self.cache.clear();
        }
        self.cache.insert(key, out.clone());
        Ok(out)
    }

    pub fn cache_hits(&self) -> usize {
        self.hits
    }

    pub fn generation(&self) -> Option<i64> {
        self.generation
    }
}

fn trace(what: &str, t0: std::time::Instant) {
    if std::env::var_os("FOXDB_TRACE").is_some() {
        eprintln!("{what} {:?}", t0.elapsed());
    }
}

/// Cold search: no warm state; arms run in parallel on short-lived read-only connections.
pub fn find(c: &Connection, cur: Option<&Project>, q: &str, o: &FindOpts) -> Result<String> {
    let t0 = std::time::Instant::now();
    let mut pool: Vec<Connection> = (0..4).filter_map(|_| crate::db::open_ro()).collect();
    trace("search pool", t0);
    find_inner(c, &mut pool, None, None, cur, q, o)
}

/// Warm search: graph and connections from `w`, and the generation-keyed result cache.
pub fn find_warm(w: &mut Warm, c: &Connection, cur: Option<&Project>, q: &str, o: &FindOpts) -> Result<String> {
    w.refresh(c)?;
    let key = format!("{:?}|{}|{q}|{}|{}|{}|{}", cur.map(|p| p.id), o.kind, o.all, o.budget, o.limit, o.offset);
    if let Some(hit) = w.cache.get(&key) {
        w.hits += 1;
        return Ok(hit.clone());
    }
    while w.pool.len() < 4 {
        match crate::db::open_ro() {
            Some(x) => w.pool.push(x),
            None => break,
        }
    }
    // Only the graph arm reads the graph. A stale or missing one is rebuilt in the background
    // while this search walks SQL: the same neighbours, so the same answer, without the wait.
    let current = match cur {
        Some(p) if identifier(q).is_some() && matches!(o.kind, "any" | "symbol") => w.graph_now(c, p.id).is_some(),
        _ => false,
    };
    let Warm { graphs, pool, derived, .. } = w;
    let warm = WarmRefs { graph: cur.filter(|_| current).and_then(|p| graphs.get(&p.id)).map(|x| &x.1) };
    let out = find_inner(c, pool, Some(warm), Some(derived), cur, q, o)?;
    if w.cache.len() >= CACHE_MAX {
        w.cache.clear();
    }
    w.cache.insert(key, out.clone());
    Ok(out)
}

#[derive(Clone, Copy)]
struct WarmRefs<'a> {
    graph: Option<&'a Graph>,
}

fn find_inner(
    c: &Connection,
    pool: &mut [Connection],
    warm: Option<WarmRefs<'_>>,
    derived: Option<&mut DerivedCache>,
    cur: Option<&Project>,
    q: &str,
    o: &FindOpts,
) -> Result<String> {
    let hits = ranked(c, pool, warm, derived, cur, q, o, o.offset + o.limit.max(1))?;
    let show_project = o.all || cur.is_none();
    let mut rows: Vec<Row> =
        hits.iter().map(|h| Row { full: render(h, show_project), skeleton: render_skeleton(h, show_project), id: render_id(h) }).collect();
    if hits.is_empty() {
        let none = format!("no matches for {q:?}");
        rows.push(Row { full: none.clone(), skeleton: none.clone(), id: none });
    }
    Ok(tiered(&rows, o.offset, o.budget, o.limit).0)
}

/// Ranked hits for `q`, best first, without rendering (`fox eval` reads these). The first
/// `show` symbols carry their reference counts.
pub fn hits(c: &Connection, cur: Option<&Project>, q: &str, o: &FindOpts, show: usize) -> Result<Vec<Hit>> {
    let mut pool: Vec<Connection> = (0..4).filter_map(|_| crate::db::open_ro()).collect();
    ranked(c, &mut pool, None, None, cur, q, o, show)
}

#[allow(clippy::too_many_arguments)]
fn ranked(
    c: &Connection,
    pool: &mut [Connection],
    warm: Option<WarmRefs<'_>>,
    derived: Option<&mut DerivedCache>,
    cur: Option<&Project>,
    q: &str,
    o: &FindOpts,
    show: usize,
) -> Result<Vec<Hit>> {
    let sc = scope(c, cur, o);
    let engine = Engine::choose(c, &sc, derived);
    let f = Filter::render(Some(c), &sc);
    let t = Terms::new(q);
    let ident = t.ident.clone();
    let route = route(q);

    let mut arms: Vec<(f64, ArmFn)> = vec![];
    {
        let (q, f) = (q.to_string(), f.clone());
        arms.push((W_EXACT, Box::new(move |c: &Connection| exact_arm(c, &q, &f, ARM_K))));
    }
    {
        // The route: body text for words, names for an identifier or any other single token.
        let field = if route == Route::Text { Field::Body } else { Field::Names };
        let (t, sc, f, e) = (t.clone(), sc.clone(), f.clone(), engine.clone());
        arms.push((W_KEYWORD, Box::new(move |c: &Connection| e.keyword(c, &sc, &f, &t, field, ARM_K))));
    }
    // Trigram substring of a single token (a path fragment, part of a name). Words skip it:
    // one word of a sentence matched inside paths agreed with the body arm on the wrong rows.
    if route != Route::Text {
        let (sc, f, needle, e) = (sc.clone(), f.clone(), q.trim().to_string(), engine.clone());
        arms.push((W_SUBSTRING, Box::new(move |c: &Connection| e.substring(c, &sc, &f, &needle, ARM_K))));
    }
    if let Some(id) = ident.clone() {
        let (sc, f) = (sc.clone(), f.clone());
        arms.push((W_NAME, Box::new(move |c: &Connection| Sqlite::with_filter(c, &sc, &f).prefix(&id, &sc, ARM_K))));
    }
    if let (Some(id), Some(p)) = (ident.clone(), cur)
        && matches!(o.kind, "any" | "symbol")
    {
        let g = warm.and_then(|w| w.graph);
        let pid = p.id;
        arms.push((W_GRAPH, Box::new(move |c: &Connection| graph_arm(c, g, &id, pid, 20))));
    }
    // The opt-in vector arm (feature `vectors`, `vectors = true`): sentences of five words or
    // more only, never an identifier, a token or a short phrase; symbols and doc headings of the
    // current project. Off, or without a
    // current index, it adds nothing and the answer is the one above.
    #[cfg(feature = "vectors")]
    if let Some(p) = cur
        && route == Route::Text
        && matches!(o.kind, "any" | "symbol" | "doc")
        && crate::vectors::wants(q)
        && crate::config::vectors_on()
    {
        let (pid, q) = (p.id, q.to_string());
        arms.push((crate::vectors::W_VECTOR, Box::new(move |c: &Connection| crate::vectors::arm(c, pid, &q, ARM_K))));
    }
    let t0 = std::time::Instant::now();
    let ranked = run_arms(c, pool, arms);
    trace("search arms", t0);

    let fused = fuse(&ranked);
    let want = show * 3 + 30;
    let top: Vec<(i64, f64)> = fused.into_iter().take(want.max(90)).collect();
    let ids: Vec<i64> = top.iter().map(|x| x.0).collect();
    let mut hits = load_hits(c, &ids, &f)?;
    let scored: Vec<(Hit, f64)> = top
        .into_iter()
        .filter_map(|(id, s)| hits.remove(&id).map(|h| (h, s)))
        .map(|(h, s)| {
            let b = feature_boost(&h, q, ident.as_deref());
            (h, s * b)
        })
        .collect();
    let mut hits = damp_same_file(scored, if route == Route::Text { DAMP_TEXT } else { DAMP_NAMES });
    for h in hits.iter_mut().take(show).filter(|h| h.kind == "symbol") {
        h.refs = c
            .query_row(
                "SELECT (SELECT COUNT(*) FROM edge WHERE dst = ?1 AND kind IN ('CALLS','REFERENCES')) + (SELECT COUNT(*) FROM ts_ref WHERE dst = ?1 AND dst != 0)",
                [h.id],
                |r| r.get(0),
            )
            .ok();
    }
    trace("search rank+refs", t0);
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_parts_cover_camel_snake_acronyms_and_digits() {
        assert_eq!(ident_parts("PlayerViewModel"), ["player", "view", "model"]);
        assert_eq!(ident_parts("load_user_name"), ["load", "user", "name"]);
        assert_eq!(ident_parts("HTTPServer2Config"), ["http", "server", "2", "config"]);
        assert_eq!(ident_parts("src/ui/HomeShelf.kt"), ["src", "ui", "home", "shelf", "kt"]);
    }

    #[test]
    fn terms_drop_stopwords_keep_whole_identifiers_and_cap() {
        let t = terms("how does subtitleSelection work");
        assert!(t.contains(&"subtitleselection".to_string()) && t.contains(&"subtitle".to_string()), "{t:?}");
        assert!(!t.iter().any(|x| x == "how" || x == "does" || x == "work"), "{t:?}");
        assert!(terms("a b c d e f g h i j k l m n o p q r s t u v w x y z aa bb cc dd ee ff gg hh ii jj").len() <= 8);
    }

    #[test]
    fn rrf_rewards_agreement_between_arms() {
        let f = fuse(&[(1.0, vec![1, 2, 3]), (1.0, vec![3, 1]), (0.5, vec![3])]);
        assert_eq!(f[0].0, 3);
        assert_eq!(f[1].0, 1);
    }

    #[test]
    fn path_classes() {
        assert!(is_test_path("app/src/test/kotlin/FooTest.kt"));
        assert!(is_test_path("web/src/foo.spec.ts"));
        assert!(!is_test_path("app/src/main/kotlin/Contest.kt"));
        assert!(is_vendor_or_generated("vendor/lib/x.go"));
        assert!(is_vendor_or_generated("app/build/generated/R.kt"));
        assert!(!is_vendor_or_generated("app/src/main/Builder.kt"));
    }
}
