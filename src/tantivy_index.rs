//! A tantivy index per big project, derived from SQLite (built with the `tantivy` feature).
//!
//! SQLite stays the system of record; this index can always be thrown away. Layout and rules:
//! - One index per project at `$FOXDB_HOME/tantivy/<project_id>/`. Each full-text row of
//!   SQLite (`fts_node`, `fts_trigram`, `fts_body`) is one document keyed by node id and table,
//!   with the node id as a fast field. Nothing is stored: hits go back to SQLite for display.
//!   Each arm reads its own fields, so no query needs a filter: names (`name`, `sig`, `path`,
//!   `body`) hold the `fts_node` rows; the body arm (`bname`, `bsig`, `bpath`, `bbody`, `body`)
//!   holds the `fts_body` rows of symbols and the `fts_node` rows of everything else; trigrams
//!   (`tri_path`, `tri_name`) hold the `fts_trigram` rows.
//! - `foxd` is the only writer. The CLI and the MCP server only open readers.
//! - Writes reach it through `search_outbox` (see `outbox`), appended in the same SQLite
//!   transaction as the full-text rows. `foxd` applies the log in `seq` order, commits with the
//!   last `seq` as payload, reloads, and only then deletes the applied rows.
//! - On open: a payload below the log replays from payload + 1 (the rows are still there); a
//!   payload above SQLite's highest `seq` means SQLite lost a transaction, and an unreadable index
//!   (unparsable metadata, a segment file missing or failing its checksum, no reader) is deleted:
//!   both rebuild the project (a fill), and FTS5 serves meanwhile. A missing or empty directory
//!   gets a new index, refilled unless the log still holds the whole fill.
//! - tantivy can panic on a damaged file. Every call into it runs under [`guard`], so a panic is
//!   an error: a search falls back to FTS5 and leaves a `<project_id>.broken` mark, after which
//!   `foxd` checks the index again (and rebuilds it if it is damaged).
//! - A query uses the index only when it has applied every logged row of the project (and the
//!   whole fill); otherwise that project falls back to FTS5, which is kept in full.
//! - Writer: one thread and `writer_mb` MB inside `foxd`, fills included,
//!   `del_docs_ratio_before_merge = 0.3`, readers on `ReloadPolicy::Manual`.

use crate::outbox::{self, OP_DELETE, OP_RESET, TBL_BODY, TBL_TRI};
use crate::search::{Field as Arm, Scope, SearchBackend, Terms};
use anyhow::Result;
use rusqlite::{Connection, params};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use tantivy::collector::TopDocs;
use tantivy::merge_policy::LogMergePolicy;
use tantivy::query::{BooleanQuery, BoostQuery, ConstScoreQuery, Occur, PhraseQuery, Query, TermQuery, TermSetQuery};
use tantivy::schema::{FAST, Field, INDEXED, IndexRecordOption, STRING, Schema, TextFieldIndexing, TextOptions};
use tantivy::tokenizer::{AsciiFoldingFilter, LowerCaser, SimpleTokenizer, TextAnalyzer, Token, TokenStream, Tokenizer};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term};

/// `$FOXDB_HOME/tantivy`: absent in a store where no project has an index.
pub fn root() -> PathBuf {
    crate::db::home().join("tantivy")
}

pub fn dir(pid: i64) -> PathBuf {
    root().join(pid.to_string())
}

/// The fields of every project index.
#[derive(Clone, Copy)]
pub struct Fields {
    /// node id * 4 + table: the delete key.
    key: Field,
    /// node id (fast field).
    id: Field,
    kind: Field,
    name: Field,
    sig: Field,
    path: Field,
    body: Field,
    bname: Field,
    bsig: Field,
    bpath: Field,
    bbody: Field,
    tri_path: Field,
    tri_name: Field,
}

fn schema() -> Schema {
    let mut b = Schema::builder();
    let words = TextOptions::default()
        .set_indexing_options(TextFieldIndexing::default().set_tokenizer("fts").set_index_option(IndexRecordOption::WithFreqs));
    let tri = TextOptions::default()
        .set_indexing_options(TextFieldIndexing::default().set_tokenizer("tri").set_index_option(IndexRecordOption::WithFreqsAndPositions));
    b.add_u64_field("key", INDEXED);
    b.add_u64_field("id", FAST);
    b.add_text_field("kind", STRING);
    for f in ["name", "sig", "path", "body", "bname", "bsig", "bpath", "bbody"] {
        b.add_text_field(f, words.clone());
    }
    b.add_text_field("tri_path", tri.clone());
    b.add_text_field("tri_name", tri);
    b.build()
}

fn fields(s: &Schema) -> Result<Fields> {
    let g = |n: &str| s.get_field(n).map_err(|e| anyhow::anyhow!("tantivy index without field {n}: {e}"));
    Ok(Fields {
        key: g("key")?,
        id: g("id")?,
        kind: g("kind")?,
        name: g("name")?,
        sig: g("sig")?,
        path: g("path")?,
        body: g("body")?,
        bname: g("bname")?,
        bsig: g("bsig")?,
        bpath: g("bpath")?,
        bbody: g("bbody")?,
        tri_path: g("tri_path")?,
        tri_name: g("tri_name")?,
    })
}

/// FTS5 `unicode61 remove_diacritics 2`, as near as tantivy has it: runs of letters and digits,
/// lowercased, accents folded.
fn words_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default()).filter(LowerCaser).filter(AsciiFoldingFilter).build()
}

/// FTS5 `trigram`: every three consecutive characters, lowercased, on consecutive positions, so a
/// phrase of a needle's trigrams is exactly a substring match.
#[derive(Clone, Default)]
struct Trigrams {
    toks: Vec<Token>,
}

struct Stream<'a> {
    toks: &'a mut [Token],
    i: usize,
}

impl TokenStream for Stream<'_> {
    fn advance(&mut self) -> bool {
        self.i += 1;
        self.i <= self.toks.len()
    }
    fn token(&self) -> &Token {
        &self.toks[self.i - 1]
    }
    fn token_mut(&mut self) -> &mut Token {
        &mut self.toks[self.i - 1]
    }
}

impl Tokenizer for Trigrams {
    type TokenStream<'a> = Stream<'a>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> Stream<'a> {
        self.toks.clear();
        let lower = text.to_lowercase();
        let cs: Vec<(usize, char)> = lower.char_indices().collect();
        for i in 0..cs.len().saturating_sub(2) {
            let s = cs[i].0;
            let e = cs.get(i + 3).map(|x| x.0).unwrap_or(lower.len());
            self.toks.push(Token { offset_from: s, offset_to: e, position: i, text: lower[s..e].to_string(), position_length: 1 });
        }
        Stream { toks: &mut self.toks, i: 0 }
    }
}

fn register(index: &Index) {
    index.tokenizers().register("fts", words_analyzer());
    index.tokenizers().register("tri", TextAnalyzer::builder(Trigrams::default()).build());
}

/// The trigrams of `s` as the index holds them.
pub fn trigrams(s: &str) -> Vec<String> {
    let mut t = Trigrams::default();
    let mut st = t.token_stream(s);
    let mut v = vec![];
    while st.advance() {
        v.push(st.token().text.clone());
    }
    v
}

/// `s` as the word fields hold it.
pub fn words(s: &str) -> Vec<String> {
    let mut a = words_analyzer();
    let mut st = a.token_stream(s);
    let mut v = vec![];
    while st.advance() {
        v.push(st.token().text.clone());
    }
    v
}

/// Set when code in tantivy panicked in this process, on any thread (its merge threads catch
/// their own panics): `foxd` then checks every index it keeps open.
static PANICKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Note panics raised in tantivy's code (once per process; the previous hook still runs).
fn watch_panics() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if info.location().is_some_and(|l| l.file().contains("tantivy")) {
                PANICKED.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            prev(info)
        }));
    });
}

/// Run `f`, turning a panic inside tantivy (a damaged file can index out of bounds) into an error.
pub fn guard<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|p| {
        PANICKED.store(true, std::sync::atomic::Ordering::SeqCst);
        let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
        Err(anyhow::anyhow!("tantivy panicked: {msg}"))
    })
}

/// A search of project `pid`'s index under [`guard`]; a panic also marks the index damaged.
pub fn search<T>(pid: i64, f: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            PANICKED.store(true, std::sync::atomic::Ordering::SeqCst);
            report_broken(pid);
            Err(anyhow::anyhow!("tantivy panicked"))
        }
    }
}

/// While this file exists, searches do not read project `pid`'s index, and `foxd` checks it
/// again (rebuilding it when damaged), then removes the file. Left by a search that panicked and
/// by `fox status` finding a damaged file.
fn broken_mark(pid: i64) -> PathBuf {
    root().join(format!("{pid}.broken"))
}

/// Project `pid`'s index looks damaged: searches stop reading it until `foxd` has checked it.
pub fn report_broken(pid: i64) {
    if root().is_dir() {
        let _ = std::fs::write(broken_mark(pid), b"");
    }
}

/// Is every file of the index's committed segments present with a good checksum, and does a
/// reader open? (Reads the whole index once: `foxd` does it when it opens one, `fox status` when
/// queries would read it.)
fn sound(index: &Index) -> Result<()> {
    guard(|| {
        let bad = index.validate_checksum()?;
        if !bad.is_empty() {
            anyhow::bail!("damaged files: {bad:?}");
        }
        let r: IndexReader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
        let _ = r.searcher();
        Ok(())
    })
}

fn open_index(pid: i64) -> Result<Index> {
    let index = Index::open_in_dir(dir(pid))?;
    register(&index);
    Ok(index)
}

/// The `seq` an index's last commit applied (0 for none).
fn payload(index: &Index) -> Result<i64> {
    Ok(index.load_metas()?.payload.and_then(|p| p.parse().ok()).unwrap_or(0))
}

// ----------------------------------------------------------------------------- writer (foxd)

struct Proj {
    /// The index's one writer (tantivy allows one per index at a time).
    writer: Option<IndexWriter>,
    f: Fields,
    applied: i64,
}

/// The writer side, owned by `foxd`: one writer per project with an index.
#[derive(Default)]
pub struct Writers {
    open: HashMap<i64, Proj>,
}

/// Rows applied per commit while catching up.
const BATCH: i64 = 20_000;

/// The one writer of an index: one thread and `writer_mb` MB, a fill included.
fn new_writer(index: &Index) -> Result<IndexWriter> {
    let mb = crate::config::get().search.writer_mb.max(16);
    let w: IndexWriter = index.writer_with_num_threads(1, mb * 1_000_000)?;
    let mut mp = LogMergePolicy::default();
    // foxdb deletes and re-adds a file's rows on every save.
    mp.set_del_docs_ratio_before_merge(0.3);
    w.set_merge_policy(Box::new(mp));
    Ok(w)
}

impl Writers {
    /// Apply every project's logged rows. Cheap when the store has no derived index: one
    /// `sqlite_master` lookup. Returns how many rows were applied.
    pub fn drain(&mut self, c: &Connection) -> Result<usize> {
        if !outbox::exists(c) {
            self.open.clear();
            return Ok(0);
        }
        let projects: Vec<i64> = {
            let mut q = c.prepare("SELECT project_id FROM search_index")?;
            q.query_map([], |r| r.get(0))?.flatten().collect()
        };
        // Projects whose index was switched off: drop the writer and the files.
        let gone: Vec<i64> = self.open.keys().filter(|p| !projects.contains(p)).copied().collect();
        for pid in gone {
            self.open.remove(&pid);
            let _ = std::fs::remove_dir_all(dir(pid));
        }
        if let Ok(rd) = std::fs::read_dir(root()) {
            for e in rd.flatten() {
                if let Some(pid) = e.file_name().to_str().and_then(|n| n.trim_end_matches(".broken").parse::<i64>().ok())
                    && !projects.contains(&pid)
                {
                    let _ = std::fs::remove_dir_all(e.path());
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        watch_panics();
        if PANICKED.swap(false, std::sync::atomic::Ordering::SeqCst) {
            // A panic in tantivy (a merge thread reading a damaged segment, say): check them all.
            self.open.clear();
        }
        let mut n = 0;
        for pid in projects {
            // Marked damaged: open (and so check) the index again before the mark goes.
            let marked = broken_mark(pid).exists();
            if marked {
                self.open.remove(&pid);
            }
            match guard(|| self.drain_project(c, pid)) {
                Ok(k) => {
                    n += k;
                    if marked {
                        let _ = std::fs::remove_file(broken_mark(pid));
                    }
                }
                Err(e) => {
                    // The index stops moving, so queries stop reading it (they use FTS5); the
                    // writer is reopened on the next round. An unreadable index is rebuilt there.
                    if std::env::var_os("FOXDB_TRACE").is_some() {
                        eprintln!("tantivy {pid}: {e:#}");
                    }
                    self.open.remove(&pid);
                }
            }
        }
        Ok(n)
    }

    fn project(&mut self, c: &Connection, pid: i64) -> Result<&mut Proj> {
        #[allow(clippy::map_entry)]
        if !self.open.contains_key(&pid) {
            let d = dir(pid);
            let empty = std::fs::read_dir(&d).map(|mut r| r.next().is_none()).unwrap_or(true);
            std::fs::create_dir_all(&d)?;
            // An index from another layout (an older build) counts as unreadable, and so does one
            // with a missing or damaged segment file.
            let opened = guard(|| {
                let i = Index::open_in_dir(&d)?;
                fields(&i.schema())?;
                sound(&i)?;
                Ok(i)
            });
            let index = match opened {
                Ok(i) => i,
                Err(_) if empty => {
                    // No index (a new one, or its directory was deleted): the log rebuilds it only
                    // while it still holds the whole fill; otherwise ask for a new fill.
                    if !outbox::log_is_whole(c, pid) {
                        outbox::request_fill(c, pid, "tantivy")?;
                    }
                    Index::create_in_dir(&d, schema())?
                }
                Err(e) => {
                    // Unreadable: delete it and rebuild from SQLite; FTS5 serves meanwhile.
                    if std::env::var_os("FOXDB_TRACE").is_some() {
                        eprintln!("tantivy {pid}: rebuilding: {e:#}");
                    }
                    std::fs::remove_dir_all(&d)?;
                    std::fs::create_dir_all(&d)?;
                    outbox::request_fill(c, pid, "tantivy")?;
                    Index::create_in_dir(&d, schema())?
                }
            };
            register(&index);
            let f = fields(&index.schema())?;
            let mut applied = payload(&index)?;
            let mut writer = new_writer(&index)?;
            if applied > outbox::high_water(c) {
                // The index is ahead of SQLite: SQLite lost a committed transaction (possible with
                // synchronous=NORMAL on power loss). Rebuild from what SQLite has.
                writer.delete_all_documents()?;
                let mut pc = writer.prepare_commit()?;
                pc.set_payload("0");
                pc.commit()?;
                applied = 0;
                outbox::request_fill(c, pid, "tantivy")?;
            }
            let p = Proj { writer: Some(writer), f, applied };
            // Rows at or below the payload were applied before a crash and not yet deleted.
            c.execute("DELETE FROM search_outbox WHERE project_id = ?1 AND seq <= ?2", params![pid, applied])?;
            self.open.insert(pid, p);
        }
        Ok(self.open.get_mut(&pid).expect("opened above"))
    }

    fn drain_project(&mut self, c: &Connection, pid: i64) -> Result<usize> {
        let backlog = {
            let p = self.project(c, pid)?;
            outbox::pending(c, pid, p.applied)
        };
        if backlog == 0 {
            return Ok(0);
        }
        let mut n = 0;
        let mut kind_q = c.prepare_cached("SELECT kind FROM node WHERE id = ?1")?;
        let mut rows_q = c.prepare_cached(
            "SELECT seq, rowid, tbl, op, COALESCE(c0, ''), COALESCE(c1, ''), COALESCE(c2, ''), COALESCE(c3, '')
             FROM search_outbox WHERE project_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let p = self.open.get_mut(&pid).expect("opened");
        let w = p.writer.as_mut().expect("writer");
        loop {
            type Row = (i64, i64, i64, i64, String, String, String, String);
            let rows: Vec<Row> = rows_q
                .query_map(params![pid, p.applied, BATCH], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
                })?
                .flatten()
                .collect();
            let mut rows = rows;
            // A reset is applied on its own: commit what came before it, then drop everything
            // (tantivy's delete-all also discards documents added in the same commit).
            if let Some(i) = rows.iter().position(|r| r.3 == OP_RESET) {
                if i == 0 {
                    let seq = rows[0].0;
                    w.delete_all_documents()?;
                    let mut pc = w.prepare_commit()?;
                    pc.set_payload(&seq.to_string());
                    pc.commit()?;
                    p.applied = seq;
                    n += 1;
                    c.execute("DELETE FROM search_outbox WHERE project_id = ?1 AND seq <= ?2", params![pid, seq])?;
                    continue;
                }
                rows.truncate(i);
            }
            let Some(last) = rows.last().map(|r| r.0) else { break };
            let mut kinds: HashMap<i64, Option<String>> = HashMap::new();
            for (_, rowid, tbl, op, c0, c1, c2, c3) in &rows {
                let key = (*rowid as u64) * 4 + *tbl as u64;
                w.delete_term(Term::from_field_u64(p.f.key, key));
                if *op == OP_DELETE {
                    continue;
                }
                let kind = kinds.entry(*rowid).or_insert_with(|| kind_q.query_row([rowid], |r| r.get(0)).ok()).clone();
                // A row whose node is gone was deleted by a later write (logged after this one).
                let Some(kind) = kind else { continue };
                let f = p.f;
                let mut d = TantivyDocument::default();
                d.add_u64(f.key, key);
                d.add_u64(f.id, *rowid as u64);
                d.add_text(f.kind, &kind);
                match *tbl {
                    TBL_TRI => {
                        d.add_text(f.tri_path, c0);
                        d.add_text(f.tri_name, c1);
                    }
                    TBL_BODY => {
                        d.add_text(f.bname, c0);
                        d.add_text(f.bsig, c1);
                        d.add_text(f.bpath, c2);
                        d.add_text(f.bbody, c3);
                    }
                    _ => {
                        d.add_text(f.name, c0);
                        d.add_text(f.sig, c1);
                        d.add_text(f.path, c2);
                        // A symbol's `fts_node` row has no body; everything else's body arm is
                        // its `fts_node` row (the names in the body arm's fields).
                        d.add_text(f.body, c3);
                        if kind != "symbol" {
                            d.add_text(f.bname, c0);
                            d.add_text(f.bsig, c1);
                            d.add_text(f.bpath, c2);
                        }
                    }
                }
                w.add_document(d)?;
            }
            let mut pc = w.prepare_commit()?;
            pc.set_payload(&last.to_string());
            pc.commit()?;
            p.applied = last;
            n += rows.len();
            // Only now, with the index durable, are the applied rows dropped.
            c.execute("DELETE FROM search_outbox WHERE project_id = ?1 AND seq <= ?2", params![pid, last])?;
        }
        Ok(n)
    }
}

/// Which projects should have an index, after an index run of `pid` (called by `fox index`).
/// `auto` gives one to a project with at least `tantivy_min_docs` rows, `tantivy` to any,
/// `fts5` removes it. A store whose highest node id is below the threshold has no project that
/// big, so for small stores this is one primary-key lookup.
pub fn maintain(c: &Connection, pid: i64, name: &str) -> Result<Option<&'static str>> {
    let cfg = &crate::config::get().search;
    let want = cfg.backend_for(name);
    let has = outbox::state(c, pid).is_some();
    // Its directory is gone (deleted by hand, or a store restored without it): fill it again.
    if has && want != "fts5" && !dir(pid).is_dir() && !outbox::fill_pending(c, pid) {
        enable(c, pid)?;
        return Ok(Some("enabled"));
    }
    match want.as_str() {
        "fts5" => {
            if has {
                outbox::disable(c, pid)?;
                return Ok(Some("disabled"));
            }
        }
        "tantivy" if !has => {
            enable(c, pid)?;
            return Ok(Some("enabled"));
        }
        "auto" if !has => {
            if crate::db::max_id(c) < cfg.tantivy_min_docs {
                return Ok(None);
            }
            let rows: i64 = c.query_row("SELECT COUNT(*) FROM node WHERE project_id = ?1", [pid], |r| r.get(0))?;
            if rows >= cfg.tantivy_min_docs {
                enable(c, pid)?;
                return Ok(Some("enabled"));
            }
        }
        _ => {}
    }
    Ok(None)
}

/// Give project `pid` an index (or rebuild it): its next index run fills the log, and `foxd`
/// builds the index from it.
pub fn enable(c: &Connection, pid: i64) -> Result<()> {
    std::fs::create_dir_all(dir(pid))?;
    outbox::request_fill(c, pid, "tantivy")
}

// ----------------------------------------------------------------------------- readers

/// A reader on one project's index, current at the time it was checked.
#[derive(Clone)]
pub struct Handle {
    pub pid: i64,
    pub searcher: Searcher,
    f: Fields,
    pub payload: i64,
}

/// A long-lived reader (the MCP server or `foxd` keep one per project).
pub struct Cached {
    index: Index,
    reader: IndexReader,
    f: Fields,
}

impl Cached {
    pub fn open(pid: i64) -> Result<Cached> {
        guard(|| {
            let index = open_index(pid)?;
            let f = fields(&index.schema())?;
            let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
            Ok(Cached { index, reader, f })
        })
    }

    /// The current snapshot, if it has applied everything SQLite logged for `pid`.
    pub fn handle(&self, c: &Connection, pid: i64) -> Option<Handle> {
        // The payload is read before the reload: the snapshot then holds at least that commit.
        let payload = payload(&self.index).ok()?;
        if !fresh(c, pid, payload) {
            return None;
        }
        guard(|| Ok(self.reader.reload()?)).ok()?;
        Some(Handle { pid, searcher: self.reader.searcher(), f: self.f, payload })
    }
}

/// May a query read project `pid`'s index at `payload`? Only when it is filled, has applied
/// every logged row of the project, and is not ahead of SQLite.
fn fresh(c: &Connection, pid: i64, payload: i64) -> bool {
    let Some((_, state, fill_seq)) = outbox::state(c, pid) else { return false };
    state == "live" && payload >= fill_seq && payload <= outbox::high_water(c) && !outbox::behind(c, pid, payload)
}

/// The project in `sc` with a current index, if any: (handle, the rest of the scope). A scope
/// over every project (`--all`) or with no project never uses one. A store without any index
/// pays one directory lookup.
pub fn for_scope(c: &Connection, sc: &Scope, cache: Option<&mut HashMap<i64, Cached>>) -> Option<(Handle, Scope)> {
    if sc.projects.is_empty() || !root().is_dir() || !outbox::exists(c) {
        return None;
    }
    let live: Vec<i64> = sc.projects.iter().copied().filter(|p| outbox::state(c, *p).is_some_and(|s| s.1 == "live")).collect();
    let [pid] = live.as_slice() else { return None };
    let pid = *pid;
    if broken_mark(pid).exists() {
        return None;
    }
    let name: Option<String> = c.query_row("SELECT name FROM project WHERE id = ?1", [pid], |r| r.get(0)).ok();
    if name.is_some_and(|n| crate::config::get().search.backend_for(&n) == "fts5") {
        return None;
    }
    let h = match cache {
        Some(m) => {
            if let std::collections::hash_map::Entry::Vacant(v) = m.entry(pid) {
                v.insert(Cached::open(pid).ok()?);
            }
            m[&pid].handle(c, pid)?
        }
        None => Cached::open(pid).ok()?.handle(c, pid)?,
    };
    let rest = Scope { projects: sc.projects.iter().copied().filter(|p| *p != pid).collect(), ..sc.clone() };
    Some((h, rest))
}

/// Status of project `pid`'s index for `fox status`: backend, state, lag, segments.
pub fn status_line(c: &Connection, pid: i64) -> Option<String> {
    let (backend, state, fill_seq) = outbox::state(c, pid)?;
    let opened = guard(|| {
        let i = open_index(pid)?;
        let p = payload(&i).unwrap_or(0);
        let segs = i.searchable_segment_metas().map(|v| v.len()).unwrap_or(0);
        let docs = guard(|| Ok(i.reader()?.searcher().num_docs())).unwrap_or(0);
        Ok((i, p, segs, docs))
    });
    let (payload, segments, docs) = match &opened {
        Ok((_, p, segs, docs)) => (Some(*p), *segs, *docs),
        Err(_) => (None, 0, 0),
    };
    let lag = outbox::pending(c, pid, payload.unwrap_or(0));
    let current = payload.is_some_and(|p| fresh(c, pid, p));
    // A current index is checked file by file before status says queries read it.
    let damaged = current && opened.as_ref().is_ok_and(|o| sound(&o.0).is_err());
    if damaged {
        report_broken(pid);
    }
    Some(format!(
        "    search: {backend} ({state}{}), applied seq {}, {lag} row(s) behind, {segments} segment(s), {docs} docs; {}",
        if state == "live" { format!(", filled at seq {fill_seq}") } else { String::new() },
        payload.map(|p| p.to_string()).unwrap_or_else(|| "none".into()),
        if damaged {
            "damaged: queries use FTS5 until foxd rebuilds it"
        } else if current {
            "queries read it"
        } else {
            "queries use FTS5 until it catches up (foxd applies the log)"
        }
    ))
}

// ----------------------------------------------------------------------------- queries

/// FTS5 column weights, as `bm25(t, 10, 4, 2, 1)` gives them, over the fields an arm reads.
fn weighted(f: &Fields, arm: Arm) -> Vec<(Field, f32)> {
    match arm {
        Arm::Names => vec![(f.name, 10.0), (f.sig, 4.0), (f.path, 2.0), (f.body, 1.0)],
        Arm::Body => vec![(f.bname, 10.0), (f.bsig, 4.0), (f.bpath, 2.0), (f.bbody, 1.0), (f.body, 1.0)],
    }
}

fn str_upper_bound(p: &str) -> Vec<u8> {
    let mut b = p.as_bytes().to_vec();
    while let Some(last) = b.pop() {
        if last < 0xff {
            b.push(last + 1);
            return b;
        }
    }
    vec![0xff]
}

/// Most dictionary terms one prefix expands to per field (the shortest ones).
const PREFIX_TERMS: usize = 128;

fn expand_prefix(s: &Searcher, field: Field, p: &str) -> Result<Vec<String>> {
    let mut set = BTreeSet::new();
    let hi = str_upper_bound(p);
    for seg in s.segment_readers() {
        let inv = seg.inverted_index(field)?;
        let mut st = inv.terms().range().ge(p.as_bytes()).lt(&hi).into_stream()?;
        while st.advance() {
            set.insert(String::from_utf8_lossy(st.key()).to_string());
            if set.len() >= PREFIX_TERMS * 8 {
                break;
            }
        }
    }
    let mut v: Vec<String> = set.into_iter().collect();
    v.sort_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));
    v.truncate(PREFIX_TERMS);
    Ok(v)
}

/// One query term over the four word fields, weighted; a prefix term matches the dictionary
/// terms it starts (best one per field). A term the analyzer splits matches all its parts.
fn term_query(h: &Handle, arm: Arm, t: &str, prefix: bool) -> Result<Option<Box<dyn Query>>> {
    let toks = words(t);
    if toks.is_empty() {
        return Ok(None);
    }
    let n = toks.len();
    let mut parts: Vec<(Occur, Box<dyn Query>)> = vec![];
    for (i, tok) in toks.iter().enumerate() {
        let mut fields: Vec<(Occur, Box<dyn Query>)> = vec![];
        for (fl, w) in weighted(&h.f, arm) {
            let q: Option<Box<dyn Query>> = if prefix && i + 1 == n {
                // FTS5 scores a prefix as one term over every row it matches, so a prefix that
                // matches much weighs little. Here each match scores the prefix's own IDF (from the
                // documents its expansions reach), not the rarer expanded word's.
                let terms = expand_prefix(&h.searcher, fl, tok)?;
                let total = h.searcher.num_docs() as f32;
                let reach = terms.iter().map(|x| h.searcher.doc_freq(&Term::from_field_text(fl, x)).unwrap_or(0)).sum::<u64>() as f32;
                let idf = (1.0 + (total - reach.min(total) + 0.5) / (reach.min(total) + 0.5)).ln();
                let ds: Vec<(Occur, Box<dyn Query>)> = terms
                    .into_iter()
                    .map(|x| {
                        (Occur::Should, Box::new(TermQuery::new(Term::from_field_text(fl, &x), IndexRecordOption::Basic)) as Box<dyn Query>)
                    })
                    .collect();
                (!ds.is_empty()).then(|| Box::new(ConstScoreQuery::new(Box::new(BooleanQuery::new(ds)), idf)) as Box<dyn Query>)
            } else {
                Some(Box::new(TermQuery::new(Term::from_field_text(fl, tok), IndexRecordOption::WithFreqs)))
            };
            if let Some(q) = q {
                fields.push((Occur::Should, Box::new(BoostQuery::new(q, w))));
            }
        }
        if fields.is_empty() {
            return Ok(None);
        }
        parts.push((Occur::Must, Box::new(BooleanQuery::new(fields))));
    }
    Ok(Some(if parts.len() == 1 { parts.pop().expect("one").1 } else { Box::new(BooleanQuery::new(parts)) }))
}

fn any_of(qs: Vec<Box<dyn Query>>) -> Option<Box<dyn Query>> {
    match qs.len() {
        0 => None,
        1 => qs.into_iter().next(),
        _ => Some(Box::new(BooleanQuery::new(qs.into_iter().map(|q| (Occur::Should, q)).collect()))),
    }
}

/// The FTS5 expressions of `search::ident_expr` and `search::match_expr`, as tantivy queries.
fn names_query(h: &Handle, t: &Terms) -> Result<Option<Box<dyn Query>>> {
    match t.ident.as_deref() {
        Some(i) if !t.qualified => {
            let whole = term_query(h, Arm::Names, &crate::search::squash(i), true)?;
            let parts = crate::search::ident_parts(i);
            let mut out: Vec<Box<dyn Query>> = whole.into_iter().collect();
            if parts.len() >= 2 {
                let n = parts.len();
                let mut all: Vec<(Occur, Box<dyn Query>)> = vec![];
                for (k, p) in parts.iter().enumerate() {
                    match term_query(h, Arm::Names, p, k + 1 == n && p.chars().count() >= 3)? {
                        Some(q) => all.push((Occur::Must, q)),
                        None => {
                            all.clear();
                            break;
                        }
                    }
                }
                if !all.is_empty() {
                    out.push(Box::new(BooleanQuery::new(all)));
                }
            }
            Ok(any_of(out))
        }
        _ => {
            let mut out = vec![];
            for w in &t.words {
                out.extend(term_query(h, Arm::Names, w, w.chars().count() >= 4)?);
            }
            Ok(any_of(out))
        }
    }
}

/// Words a question keeps for the body arm: the ones in at most a tenth of the store's rows,
/// but at least the three rarest (see `search::prune_common`).
fn prune_common(c: &Connection, h: &Handle, words: &[String]) -> Vec<String> {
    let limit = (crate::db::max_id(c) as f64 * 0.1).max(50.0) as u64;
    let df = |w: &str| -> u64 {
        weighted(&h.f, Arm::Body).iter().map(|(fl, _)| h.searcher.doc_freq(&Term::from_field_text(*fl, w)).unwrap_or(0)).max().unwrap_or(0)
    };
    let mut d: Vec<(u64, &String)> = words.iter().map(|w| (df(w), w)).collect();
    if d.iter().filter(|x| x.0 <= limit).count() >= 3 {
        return d.into_iter().filter(|x| x.0 <= limit).map(|x| x.1.clone()).collect();
    }
    d.sort_by_key(|x| x.0);
    let keep: Vec<&String> = d.iter().take(3).map(|x| x.1).collect();
    words.iter().filter(|w| keep.contains(w)).cloned().collect()
}

fn filter(q: Box<dyn Query>) -> (Occur, Box<dyn Query>) {
    (Occur::Must, Box::new(ConstScoreQuery::new(q, 0.0)))
}

fn kind_is(h: &Handle, kinds: &[String]) -> Box<dyn Query> {
    Box::new(TermSetQuery::new(kinds.iter().map(|k| Term::from_field_text(h.f.kind, k))))
}

/// Ranked node ids of the project's documents matching `q` under `filters`.
fn top(h: &Handle, q: Box<dyn Query>, filters: Vec<(Occur, Box<dyn Query>)>, k: usize) -> Result<Vec<i64>> {
    let mut all = vec![(Occur::Must, q)];
    all.extend(filters);
    let q = BooleanQuery::new(all);
    let hits = h.searcher.search(&q, &TopDocs::with_limit(k).order_by_score())?;
    let mut cols: HashMap<u32, tantivy::columnar::Column<u64>> = HashMap::new();
    let mut ids = Vec::with_capacity(hits.len());
    for (_, a) in hits {
        if let std::collections::hash_map::Entry::Vacant(v) = cols.entry(a.segment_ord) {
            v.insert(h.searcher.segment_reader(a.segment_ord).fast_fields().u64("id")?);
        }
        if let Some(id) = cols[&a.segment_ord].first(a.doc_id) {
            ids.push(id as i64);
        }
    }
    Ok(ids)
}

/// `ids` (in order) that SQLite still holds inside the scope, at most `k`.
fn in_scope(c: &Connection, ids: Vec<i64>, s: &Scope, k: usize) -> Result<Vec<i64>> {
    if ids.is_empty() {
        return Ok(ids);
    }
    let f = crate::search::Filter::render(None, s);
    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let mut q = c.prepare(&format!("SELECT n.id FROM node n WHERE n.id IN ({list}){}{}", f.kind_sql, f.scope_sql))?;
    let ok: std::collections::HashSet<i64> = q.query_map([], |r| r.get(0))?.flatten().collect();
    Ok(ids.into_iter().filter(|id| ok.contains(id)).take(k).collect())
}

/// Alternate two ranked lists (the indexed project first), without repeats, at most `k`.
fn interleave(a: Vec<i64>, b: Vec<i64>, k: usize) -> Vec<i64> {
    let mut out = Vec::with_capacity(k);
    let (mut ia, mut ib) = (a.into_iter(), b.into_iter());
    loop {
        let (x, y) = (ia.next(), ib.next());
        if x.is_none() && y.is_none() {
            break;
        }
        for id in x.into_iter().chain(y) {
            if out.len() < k && !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// The tantivy backend for one indexed project; the rest of the scope (the global notes) goes to
/// SQLite and the two lists are interleaved. Prefix lookups stay on SQLite's name index.
pub struct Tantivy<'a> {
    pub c: &'a Connection,
    pub h: &'a Handle,
    pub rest: &'a Scope,
}

impl Tantivy<'_> {
    fn mine(&self, s: &Scope) -> Scope {
        Scope { projects: vec![self.h.pid], ..s.clone() }
    }

    fn rest(&self, s: &Scope) -> Option<Scope> {
        (!self.rest.projects.is_empty()).then(|| Scope { projects: self.rest.projects.clone(), ..s.clone() })
    }
}

/// The inner window before the scope check, as the FTS5 queries use.
const WINDOW: usize = 1000;

impl SearchBackend for Tantivy<'_> {
    fn keyword(&self, t: &Terms, field: Arm, s: &Scope, k: usize) -> Result<Vec<i64>> {
        let h = self.h;
        let q = match field {
            Arm::Names => names_query(h, t)?,
            Arm::Body => {
                let mut out = vec![];
                for w in prune_common(self.c, h, &t.words) {
                    out.extend(term_query(h, Arm::Body, &w, false)?);
                }
                any_of(out)
            }
        };
        let mut mine = vec![];
        if let Some(q) = q {
            let mut fs = vec![];
            if !s.kinds.is_empty() {
                fs.push(filter(kind_is(h, &s.kinds)));
            }
            mine = in_scope(self.c, top(h, q, fs, WINDOW)?, &self.mine(s), k)?;
        }
        match self.rest(s) {
            Some(r) => Ok(interleave(mine, crate::search::Sqlite::new(self.c).keyword(t, field, &r, k)?, k)),
            None => Ok(mine),
        }
    }

    fn substring(&self, needle: &str, s: &Scope, k: usize) -> Result<Vec<i64>> {
        let h = self.h;
        let mut mine = vec![];
        let tg = trigrams(&needle.replace('"', ""));
        if needle.chars().count() >= 3 && !tg.is_empty() {
            let per = |fl: Field| -> Box<dyn Query> {
                if tg.len() == 1 {
                    Box::new(TermQuery::new(Term::from_field_text(fl, &tg[0]), IndexRecordOption::WithFreqs))
                } else {
                    Box::new(PhraseQuery::new(tg.iter().map(|t| Term::from_field_text(fl, t)).collect()))
                }
            };
            let q: Box<dyn Query> =
                Box::new(BooleanQuery::new(vec![(Occur::Should, per(h.f.tri_path)), (Occur::Should, per(h.f.tri_name))]));
            let mut fs = vec![];
            if !s.kinds.is_empty() {
                fs.push(filter(kind_is(h, &s.kinds)));
            }
            mine = in_scope(self.c, top(h, q, fs, 500)?, &self.mine(s), k)?;
        }
        match self.rest(s) {
            Some(r) => Ok(interleave(mine, crate::search::Sqlite::new(self.c).substring(needle, &r, k)?, k)),
            None => Ok(mine),
        }
    }

    fn prefix(&self, ident: &str, s: &Scope, k: usize) -> Result<Vec<i64>> {
        crate::search::Sqlite::new(self.c).prefix(ident, s, k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigrams_are_lowercase_windows_and_words_fold_like_unicode61() {
        assert_eq!(trigrams("AbcD"), ["abc", "bcd"]);
        assert!(trigrams("ab").is_empty());
        assert_eq!(words("loadUserName load_user Café"), ["loadusername", "load", "user", "cafe"]);
    }

    #[test]
    fn interleave_alternates_without_repeats() {
        assert_eq!(interleave(vec![1, 2, 3], vec![9, 2], 4), [1, 9, 2, 3]);
    }
}
