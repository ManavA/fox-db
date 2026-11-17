//! SQLite store: `~/.foxdb/fox.db` (override with FOXDB_HOME). WAL, single file.

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;

pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("FOXDB_HOME") {
        return PathBuf::from(h);
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".foxdb")
}

pub fn db_path() -> PathBuf {
    home().join("fox.db")
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS project(
  id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL, root TEXT NOT NULL,
  git_common_dir TEXT UNIQUE, kind TEXT NOT NULL DEFAULT 'repo',
  created_at INTEGER NOT NULL, indexed_at INTEGER, index_ms INTEGER, truncated INTEGER DEFAULT 0);
CREATE TABLE IF NOT EXISTS node(
  id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, kind TEXT NOT NULL, key TEXT UNIQUE NOT NULL,
  name TEXT NOT NULL, path TEXT, line INTEGER, end_line INTEGER, sig TEXT, lang TEXT, tier INTEGER,
  content_hash TEXT, size INTEGER, mtime INTEGER, source TEXT NOT NULL,
  stale INTEGER NOT NULL DEFAULT 0, meta TEXT, updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS node_project_kind ON node(project_id, kind);
CREATE INDEX IF NOT EXISTS node_project_path ON node(project_id, path);
CREATE INDEX IF NOT EXISTS node_name ON node(name COLLATE NOCASE);
CREATE TABLE IF NOT EXISTS edge(
  src INTEGER NOT NULL, dst INTEGER NOT NULL, kind TEXT NOT NULL, source TEXT NOT NULL,
  confidence REAL NOT NULL DEFAULT 1.0, meta TEXT, PRIMARY KEY(src, dst, kind)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS edge_dst ON edge(dst, kind);
CREATE VIRTUAL TABLE IF NOT EXISTS fts_node USING fts5(name, sig, path, body, tokenize='unicode61 remove_diacritics 2', content='', contentless_delete=1);
CREATE VIRTUAL TABLE IF NOT EXISTS fts_trigram USING fts5(path, name, tokenize='trigram', content='', contentless_delete=1);
CREATE TABLE IF NOT EXISTS anchor(memory_id INTEGER NOT NULL, node_id INTEGER NOT NULL, anchor_hash TEXT, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS ingest_state(source TEXT NOT NULL, project_id INTEGER NOT NULL, cursor TEXT, updated_at INTEGER NOT NULL, PRIMARY KEY(source, project_id));
PRAGMA user_version = 1;
"#;

/// v2 (M2): Swift index-store import state and raw occurrences. Edges are derived from `occ`.
const SCHEMA_V2: &str = r#"
CREATE TABLE IF NOT EXISTS swift_unit(
  id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, store TEXT NOT NULL, unit TEXT NOT NULL,
  mtime INTEGER NOT NULL, module TEXT, UNIQUE(store, unit));
CREATE TABLE IF NOT EXISTS swift_rec(id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL, path TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS swift_file(unit INTEGER NOT NULL, rec INTEGER NOT NULL, PRIMARY KEY(unit, rec)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS swift_file_rec ON swift_file(rec);
CREATE TABLE IF NOT EXISTS occ(
  rec INTEGER NOT NULL, sym INTEGER NOT NULL, line INTEGER NOT NULL, col INTEGER NOT NULL,
  roles INTEGER NOT NULL, rel_roles INTEGER NOT NULL, rel_sym INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS occ_rec ON occ(rec);
CREATE INDEX IF NOT EXISTS occ_sym ON occ(sym);
CREATE INDEX IF NOT EXISTS occ_rel ON occ(rel_sym) WHERE rel_sym != 0;
PRAGMA user_version = 2;
"#;

/// v3: hot-path indexes found by the `fox code` latency loop.
const SCHEMA_V3: &str = r#"
DROP INDEX IF EXISTS occ_rel;
CREATE INDEX IF NOT EXISTS occ_rel ON occ(rel_sym, roles);
CREATE INDEX IF NOT EXISTS node_stale ON node(project_id, path) WHERE stale = 1;
CREATE INDEX IF NOT EXISTS node_sym_probe ON node(project_id, source, kind) WHERE kind = 'symbol';
CREATE INDEX IF NOT EXISTS node_sym_name ON node(project_id, name COLLATE NOCASE) WHERE kind = 'symbol';
CREATE INDEX IF NOT EXISTS node_sym_pid ON node(json_extract(meta, '$.pid')) WHERE kind = 'symbol';
PRAGMA user_version = 3;
"#;

/// v4: native tree-sitter index (per-file hash state, raw references with resolved targets) and
/// the token-savings ledger.
const SCHEMA_V4: &str = r#"
CREATE TABLE IF NOT EXISTS ts_file(
  project_id INTEGER NOT NULL, path TEXT NOT NULL, lang TEXT NOT NULL, hash TEXT NOT NULL,
  size INTEGER NOT NULL, mtime INTEGER NOT NULL, PRIMARY KEY(project_id, path)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS ts_ref(
  project_id INTEGER NOT NULL, path TEXT NOT NULL, src INTEGER NOT NULL, name TEXT NOT NULL,
  line INTEGER NOT NULL, kind TEXT NOT NULL, recv INTEGER NOT NULL DEFAULT 0, dst INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS ts_ref_path ON ts_ref(project_id, path);
CREATE INDEX IF NOT EXISTS ts_ref_dst ON ts_ref(dst) WHERE dst != 0;
CREATE INDEX IF NOT EXISTS ts_ref_src ON ts_ref(src) WHERE src != 0;
CREATE TABLE IF NOT EXISTS saving(
  day INTEGER NOT NULL, tool TEXT NOT NULL, answers INTEGER NOT NULL DEFAULT 0, saved INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(day, tool)) WITHOUT ROWID;
PRAGMA user_version = 4;
"#;

/// v5: shared state for the daemon, handoffs and per-session dedup. The FTS tables become
/// contentless (see `migrate_contentless`): the text lives in the files and `node`, not twice.
const SCHEMA_V5: &str = r#"
CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v INTEGER NOT NULL) WITHOUT ROWID;
INSERT OR IGNORE INTO meta(k, v) VALUES ('generation', 0);
CREATE TABLE IF NOT EXISTS handoff(
  id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, kind TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL,
  scope TEXT, harness TEXT, session TEXT, agent TEXT, evidence TEXT,
  created_at INTEGER NOT NULL, claimed_by TEXT, claimed_at INTEGER);
CREATE INDEX IF NOT EXISTS handoff_recent ON handoff(project_id, id);
CREATE TABLE IF NOT EXISTS seen(session TEXT NOT NULL, hash TEXT NOT NULL, handle TEXT NOT NULL, at INTEGER NOT NULL,
  PRIMARY KEY(session, hash)) WITHOUT ROWID;
PRAGMA user_version = 5;
"#;

/// v6 (milestone 2): the agent registry, per-repo writer leases, per-agent board cursors, and
/// handoff topics, lineage and take leases.
const SCHEMA_V6: &str = r#"
CREATE TABLE IF NOT EXISTS agent(
  id TEXT PRIMARY KEY, harness TEXT NOT NULL, model TEXT, role TEXT, label TEXT, parent TEXT,
  project_id INTEGER, cwd TEXT, status TEXT NOT NULL DEFAULT 'running', pid INTEGER, pid_start INTEGER,
  child_pid INTEGER, session TEXT, thread TEXT, transcript TEXT, task TEXT, writer INTEGER NOT NULL DEFAULT 0,
  started_at INTEGER NOT NULL, heartbeat_at INTEGER NOT NULL, ended_at INTEGER, report INTEGER);
CREATE INDEX IF NOT EXISTS agent_parent ON agent(parent);
CREATE INDEX IF NOT EXISTS agent_status ON agent(status);
CREATE TABLE IF NOT EXISTS lease(
  repo TEXT PRIMARY KEY, agent TEXT NOT NULL, pid INTEGER, pid_start INTEGER, taken_at INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS read_cursor(
  agent TEXT NOT NULL, project_id INTEGER NOT NULL, last INTEGER NOT NULL, PRIMARY KEY(agent, project_id)) WITHOUT ROWID;
INSERT OR IGNORE INTO meta(k, v) VALUES ('agent_seq', 0);
"#;

/// Add a column unless it is already there (ALTER TABLE has no IF NOT EXISTS).
fn add_column(c: &Connection, table: &str, col: &str, decl: &str) -> Result<()> {
    let has: i64 = c.query_row(&format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"), [col], |r| r.get(0))?;
    if has == 0 {
        c.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"))?;
    }
    Ok(())
}

/// Rebuild a full-content FTS5 table as a contentless one, keeping rowids and every token.
/// FTS5 cannot read text back out of a contentless table, so the copy comes from the old one.
/// The layout is checked inside the write transaction: a table another connection already
/// migrated is left alone (copying out of it would read NULL columns and empty the index).
fn migrate_contentless(c: &Connection, table: &str, cols: &str, opts: &str) -> Result<bool> {
    c.execute_batch("BEGIN IMMEDIATE")?;
    let r = (|| -> Result<bool> {
        let has_content: i64 =
            c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = ?1", [format!("{table}_content")], |r| r.get(0))?;
        if has_content == 0 {
            return Ok(false);
        }
        c.execute_batch(&format!(
            "DROP TABLE IF EXISTS {table}_v5;
             CREATE VIRTUAL TABLE {table}_v5 USING fts5({cols}, {opts}, content='', contentless_delete=1);
             INSERT INTO {table}_v5(rowid, {cols}) SELECT rowid, {cols} FROM {table};
             DROP TABLE {table};
             ALTER TABLE {table}_v5 RENAME TO {table};"
        ))?;
        Ok(true)
    })();
    match r {
        Ok(m) => {
            c.execute_batch("COMMIT")?;
            Ok(m)
        }
        Err(e) => {
            let _ = c.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Index generation: bumped by every index or memory write. Caches key on it.
pub fn generation(c: &Connection) -> i64 {
    c.query_row("SELECT v FROM meta WHERE k = 'generation'", [], |r| r.get(0)).unwrap_or(0)
}

pub fn bump_generation(c: &Connection) -> Result<()> {
    c.execute("UPDATE meta SET v = v + 1 WHERE k = 'generation'", [])?;
    Ok(())
}

/// Bump the global generation and project `pid`'s own, for writes to that project's code graph
/// or inventory. Warm per-project state (the CSR graph) keys on the project's generation only.
pub fn bump_project(c: &Connection, pid: i64) -> Result<()> {
    bump_generation(c)?;
    c.execute("INSERT INTO meta(k, v) VALUES ('gen:' || ?1, 1) ON CONFLICT(k) DO UPDATE SET v = v + 1", [pid])?;
    Ok(())
}

/// Project `pid`'s generation (0 before its first write).
pub fn project_generation(c: &Connection, pid: i64) -> i64 {
    c.query_row("SELECT v FROM meta WHERE k = 'gen:' || ?1", [pid], |r| r.get(0)).unwrap_or(0)
}

/// Keep planner statistics current. Without them SQLite drives per-file deletes from the
/// partial `node_sym_probe` index and scans every symbol of the project. `full` (a cold index,
/// or no statistics yet) runs a bounded ANALYZE; otherwise `PRAGMA optimize` re-analyzes only
/// the tables whose size moved enough to matter. Best effort: a busy store skips it.
pub fn optimize(c: &Connection, full: bool) {
    let has: bool =
        c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = 'sqlite_stat1'", [], |r| r.get::<_, i64>(0)).unwrap_or(0) > 0;
    let sql =
        if full || !has { "PRAGMA analysis_limit = 1000; ANALYZE;" } else { "PRAGMA analysis_limit = 1000; PRAGMA optimize = 0x10002;" };
    let _ = c.execute_batch(sql);
    let _ = sync_ranges(c);
    let _ = compact_ranges(c);
    // One bounded merge step after the write, so stores without a daemon (which merges when
    // idle) keep their segment count in check too.
    let _ = fts_merge(c, 64);
}

fn tune(c: &Connection) -> Result<()> {
    c.busy_timeout(std::time::Duration::from_millis(200))?;
    c.pragma_update(None, "synchronous", "NORMAL")?;
    c.pragma_update(None, "foreign_keys", "OFF")?;
    Ok(())
}

/// Read-write connection; creates the store and schema if needed.
pub fn open_rw() -> Result<Connection> {
    let h = home();
    if !h.exists() {
        std::fs::create_dir_all(&h)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&h, std::fs::Permissions::from_mode(0o700));
    }
    let c = Connection::open(db_path())?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    tune(&c)?;
    // Writers (background indexers of different projects) may queue behind each other.
    c.busy_timeout(std::time::Duration::from_secs(10))?;
    let need = |c: &Connection| -> Result<bool> {
        let v: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
        Ok(v < LATEST || c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = 'node_sym_name'", [], |r| r.get::<_, i64>(0))? == 0)
    };
    if need(&c)? {
        // Every opener (foxd, the CLI, hooks, background indexers) may arrive here at once after an
        // upgrade. One migrates under an exclusive file lock; the rest block on the lock rather
        // than on SQLite's busy timeout, then find the work done.
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(h.join("fox.db.migrate.lock"))?;
        lock.lock()?;
        if need(&c)? {
            migrate(&c)?;
        }
        drop(lock);
    }
    Ok(c)
}

/// Schema migrations, oldest first. Callers hold the migration lock.
pub(crate) fn migrate(c: &Connection) -> Result<()> {
    let v: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if v < 1 {
        c.execute_batch(SCHEMA)?;
    }
    if v < 2 {
        c.execute_batch(SCHEMA_V2)?;
    }
    if v < 3 || c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = 'node_sym_name'", [], |r| r.get::<_, i64>(0))? == 0 {
        c.execute_batch(SCHEMA_V3)?;
    }
    if v < 4 {
        c.execute_batch(SCHEMA_V4)?;
    }
    if v < 5 {
        let a = migrate_contentless(c, "fts_node", "name, sig, path, body", "tokenize='unicode61 remove_diacritics 2'")?;
        let b = migrate_contentless(c, "fts_trigram", "path, name", "tokenize='trigram'")?;
        c.execute_batch(SCHEMA_V5)?;
        if a || b {
            // Hand the freed pages (the old text copies) back to the filesystem, once.
            let _ = c.execute_batch("VACUUM");
        }
    }
    if v < 6 {
        c.execute_batch(SCHEMA_V6)?;
        add_column(c, "handoff", "topic", "TEXT")?;
        add_column(c, "handoff", "parent_id", "INTEGER")?;
        add_column(c, "handoff", "lease_until", "INTEGER")?;
        c.execute_batch(
            "CREATE INDEX IF NOT EXISTS handoff_parent ON handoff(parent_id) WHERE parent_id IS NOT NULL;
             PRAGMA user_version = 6;",
        )?;
    }
    if v < 7 {
        // A dispatched worker's child process, with its start time, so liveness covers it.
        add_column(c, "agent", "child_start", "INTEGER")?;
        c.execute_batch("PRAGMA user_version = 7;")?;
    }
    if v < 8 {
        // Milestone 3: the worktree a worker started from and left, as commit ids.
        add_column(c, "agent", "base_commit", "TEXT")?;
        add_column(c, "agent", "head_commit", "TEXT")?;
        c.execute_batch("PRAGMA user_version = 8;")?;
    }
    if v < 9 {
        // Milestone 4: fetch references by name for change-sized incremental resolution, and the
        // number of same-name candidates a guessed (name-only) resolution chose between.
        add_column(c, "ts_ref", "amb", "INTEGER NOT NULL DEFAULT 0")?;
        c.execute_batch(
            "CREATE INDEX IF NOT EXISTS ts_ref_name ON ts_ref(project_id, name);
             PRAGMA user_version = 9;",
        )?;
    }
    if v < 11 {
        // Milestone 4: which node ids belong to which project, as id ranges, so full-text search
        // can be limited to one project's rows before it ranks. One write transaction: a writer
        // that skips the migration lock (an older build mid-upgrade) cannot insert between the
        // scan and the sentinel, where its rows would fall below `range_max` yet outside every range.
        in_tx(c, || {
            c.execute_batch(ID_RANGE)?;
            backfill_ranges(c)?;
            c.execute_batch("PRAGMA user_version = 11;")?;
            Ok(())
        })?;
    }
    if v < 12 {
        // References scored before the ambiguity column read as certain: every project with
        // tree-sitter references re-scores them all once, on its next index run.
        c.execute_batch(
            "INSERT OR REPLACE INTO meta(k, v)
               SELECT 'rescore:' || p.id, 1 FROM project p WHERE EXISTS (SELECT 1 FROM ts_ref r WHERE r.project_id = p.id);
             PRAGMA user_version = 12;",
        )?;
    }
    if v < 13 {
        // Milestone 5: the body arm's table, merges moved out of the write path, and a one-time
        // body backfill. Existing symbols get their name, qualified name and path at once (so
        // the body arm never misses them); their doc and body text arrive on the next index run.
        in_tx(c, || {
            c.execute_batch(FTS_BODY)?;
            c.execute_batch(crate::swarm::SCHEMA)?;
            backfill_body_heads(c)?;
            c.execute_batch(
                "INSERT OR REPLACE INTO meta(k, v)
                   SELECT 'reindex:' || p.id, 1 FROM project p WHERE EXISTS (SELECT 1 FROM ts_file f WHERE f.project_id = p.id);
                 PRAGMA user_version = 13;",
            )?;
            Ok(())
        })?;
    }
    // Cheap when already set: the setting lives in each table's own config.
    for t in FTS_TABLES {
        defer_merges(c, t)?;
    }
    Ok(())
}

/// The full-text tables. Their merges run when the store is idle, never inside a write.
pub const FTS_TABLES: [&str; 3] = ["fts_node", "fts_trigram", "fts_body"];

/// Symbol text for the body arm: the name columns of `fts_node` plus the doc comment and the
/// identifier-split body (see `tscode::body_text`). Rowid is the node id.
/// The `_v` tables are read-only views of each index's vocabulary (documents per term), so a
/// search can leave out words that match a large share of everything.
const FTS_BODY: &str = "CREATE VIRTUAL TABLE IF NOT EXISTS fts_body USING fts5(name, sig, path, body, \
    tokenize='unicode61 remove_diacritics 2', content='', contentless_delete=1);
    CREATE VIRTUAL TABLE IF NOT EXISTS fts_body_v USING fts5vocab(fts_body, 'row');
    CREATE VIRTUAL TABLE IF NOT EXISTS fts_node_v USING fts5vocab(fts_node, 'row');";

/// FTS5 merges segments inline on every write by default (automerge) and rewrites segments
/// whose rows were deleted (deletemerge). On a big project that made a 100-file update spend
/// half a minute merging. Both are switched off; [`fts_merge`] and [`fts_optimize`] do the work
/// when nothing is waiting. The settings live in the table's own config, so this runs once.
fn defer_merges(c: &Connection, t: &str) -> Result<()> {
    let set = |k: &str| -> Result<bool> {
        let v: Option<i64> = c.query_row(&format!("SELECT v FROM {t}_config WHERE k = ?1"), [k], |r| r.get(0)).ok();
        Ok(v != Some(0))
    };
    if c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = ?1", [t], |r| r.get::<_, i64>(0))? == 0 {
        return Ok(());
    }
    for k in ["automerge", "deletemerge"] {
        if set(k)? {
            c.execute(&format!("INSERT INTO {t}({t}, rank) VALUES ('{k}', 0)"), [])?;
        }
    }
    Ok(())
}

/// One bounded merge step on every full-text table: at most `pages` pages written per table.
/// Returns whether anything was merged, so an idle loop can stop once the tables are settled.
pub fn fts_merge(c: &Connection, pages: i64) -> Result<bool> {
    let mut did = false;
    for t in FTS_TABLES {
        let before = c.total_changes();
        c.execute(&format!("INSERT INTO {t}({t}, rank) VALUES ('merge', ?1)"), [pages])?;
        did |= c.total_changes() > before + 1;
    }
    Ok(did)
}

/// Merge every full-text table into one segment (`fox index optimize`).
pub fn fts_optimize(c: &Connection) -> Result<()> {
    for t in FTS_TABLES {
        c.execute(&format!("INSERT INTO {t}({t}) VALUES ('optimize')"), [])?;
    }
    Ok(())
}

/// How many segments each full-text table holds (`fox status`, tests).
pub fn fts_segments(c: &Connection, t: &str) -> i64 {
    c.query_row(&format!("SELECT COUNT(DISTINCT segid) FROM {t}_idx"), [], |r| r.get(0)).unwrap_or(0)
}

fn backfill_body_heads(c: &Connection) -> Result<()> {
    let mut q = c.prepare(
        "SELECT id, name, COALESCE(json_extract(meta, '$.parent') || '.' || name, name), COALESCE(path, '') FROM node WHERE kind = 'symbol'",
    )?;
    let mut ins = c.prepare("INSERT INTO fts_body(rowid, name, sig, path, body) VALUES (?1, ?2, ?3, ?4, '')")?;
    let rows = q.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
    for (id, name, q, path) in rows.flatten() {
        ins.execute(rusqlite::params![id, crate::inventory::split_ident(&name), q, path])?;
    }
    Ok(())
}

/// Does project `pid` still owe a body backfill (or a rebuild someone asked for)?
pub fn reindex_pending(c: &Connection, pid: i64) -> bool {
    c.query_row("SELECT 1 FROM meta WHERE k = 'reindex:' || ?1", [pid], |_| Ok(())).is_ok()
}

/// Ask for project `pid`'s symbol text to be written again on its next index run.
pub fn request_reindex(c: &Connection, pid: i64) -> Result<()> {
    c.execute("INSERT OR REPLACE INTO meta(k, v) VALUES ('reindex:' || ?1, 1)", [pid])?;
    Ok(())
}

pub fn reindex_done(c: &Connection, pid: i64) -> Result<()> {
    c.execute("DELETE FROM meta WHERE k = 'reindex:' || ?1", [pid])?;
    Ok(())
}

// Ranges cover every id up to `range_max`, and `sync_ranges` appends the newer ids after each
// index write. SQLite hands out MAX(id) + 1, so an id at or below `range_max` could only come
// back if the newest rows were deleted: a sentinel row (project 0) holds `range_max` itself so
// that never happens, and every id is either in a stored range or above `range_max`.
const ID_RANGE: &str = r#"
CREATE TABLE IF NOT EXISTS id_range(project_id INTEGER NOT NULL, lo INTEGER NOT NULL, hi INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS id_range_hi ON id_range(hi);
CREATE INDEX IF NOT EXISTS id_range_project ON id_range(project_id, hi);
INSERT OR IGNORE INTO meta(k, v) VALUES ('range_max', 0);
DROP TRIGGER IF EXISTS node_id_range;
DROP TRIGGER IF EXISTS node_id_reused;
"#;

/// Runs of consecutive node ids owned by one project, for ids above `after`, in id order.
fn runs_after(c: &Connection, after: i64) -> Result<Vec<(i64, i64, i64)>> {
    let mut runs: Vec<(i64, i64, i64)> = vec![];
    let mut q = c.prepare_cached("SELECT id, project_id FROM node WHERE id > ?1 AND project_id != 0 ORDER BY id")?;
    let mut rows = q.query([after])?;
    while let Some(r) = rows.next()? {
        let (id, pid): (i64, i64) = (r.get(0)?, r.get(1)?);
        match runs.last_mut() {
            Some(last) if last.0 == pid => last.2 = id,
            _ => runs.push((pid, id, id)),
        }
    }
    Ok(runs)
}

fn range_max(c: &Connection) -> i64 {
    c.query_row("SELECT v FROM meta WHERE k = 'range_max'", [], |r| r.get(0)).unwrap_or(0)
}

/// Move the sentinel to the top of the id space and record its id as `range_max`.
fn raise_sentinel(c: &Connection) -> Result<()> {
    c.execute("DELETE FROM node WHERE project_id = 0 AND kind = '~range'", [])?;
    c.execute("INSERT INTO node(project_id, kind, key, name, source, updated_at) VALUES (0, '~range', '~range', '', '~range', 0)", [])?;
    c.execute("UPDATE meta SET v = ?1 WHERE k = 'range_max'", [c.last_insert_rowid()])?;
    Ok(())
}

fn in_tx(c: &Connection, f: impl FnOnce() -> Result<()>) -> Result<()> {
    c.execute_batch("BEGIN IMMEDIATE")?;
    match f() {
        Ok(()) => c.execute_batch("COMMIT").map_err(Into::into),
        Err(e) => {
            let _ = c.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Append the ranges of ids written since the last sync. Called after index writes.
pub fn sync_ranges(c: &Connection) -> Result<()> {
    in_tx(c, || {
        let runs = runs_after(c, range_max(c))?;
        if runs.is_empty() {
            return Ok(());
        }
        let mut ins = c.prepare_cached("INSERT INTO id_range(project_id, lo, hi) VALUES (?1, ?2, ?3)")?;
        let last: Option<(i64, i64)> =
            c.query_row("SELECT rowid, project_id FROM id_range ORDER BY hi DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?))).ok();
        for (i, (p, lo, hi)) in runs.into_iter().enumerate() {
            match last {
                // The newest stored range continues: its owner wrote again before anyone else
                // (the ids between are the old sentinel's and deleted rows).
                Some((rowid, owner)) if i == 0 && owner == p => {
                    c.execute("UPDATE id_range SET hi = ?2 WHERE rowid = ?1", [rowid, hi])?;
                }
                _ => {
                    ins.execute([p, lo, hi])?;
                }
            }
        }
        raise_sentinel(c)
    })
}

/// Rebuild `id_range` from `node`: one range per run of ids owned by one project.
fn backfill_ranges(c: &Connection) -> Result<()> {
    c.execute_batch("DELETE FROM id_range")?;
    let runs = runs_after(c, 0)?;
    let mut ins = c.prepare("INSERT INTO id_range(project_id, lo, hi) VALUES (?1, ?2, ?3)")?;
    for (p, lo, hi) in runs {
        ins.execute([p, lo, hi])?;
    }
    raise_sentinel(c)
}

/// Merge each project's neighbouring id ranges when no other project's range lies between them
/// (the gap then holds only deleted ids). Keeps the range list short as writers interleave.
pub fn compact_ranges(c: &Connection) -> Result<()> {
    in_tx(c, || compact_ranges_tx(c))
}

fn compact_ranges_tx(c: &Connection) -> Result<()> {
    let all: Vec<(i64, i64, i64, i64)> = {
        let mut q = c.prepare("SELECT rowid, project_id, lo, hi FROM id_range ORDER BY lo")?;
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.flatten().collect()
    };
    let mut merged: Vec<(i64, i64, i64, i64)> = vec![]; // (rowid, project, lo, hi)
    let mut drop: Vec<i64> = vec![];
    let mut last_of: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for r in all {
        // Another project's range ending after the candidate's end blocks a merge across it.
        let target = last_of.get(&r.1).copied().filter(|&i| {
            let (_, p, _, hi) = merged[i];
            merged.iter().all(|m| m.1 == p || m.3 < hi || m.2 > r.2)
        });
        match target {
            Some(i) => {
                merged[i].3 = merged[i].3.max(r.3);
                drop.push(r.0);
            }
            None => {
                last_of.insert(r.1, merged.len());
                merged.push(r);
            }
        }
    }
    if drop.is_empty() {
        return Ok(());
    }
    let mut upd = c.prepare("UPDATE id_range SET hi = ?2 WHERE rowid = ?1")?;
    for m in &merged {
        upd.execute([m.0, m.3])?;
    }
    let mut del = c.prepare("DELETE FROM id_range WHERE rowid = ?1")?;
    for d in drop {
        del.execute([d])?;
    }
    Ok(())
}

/// Does project `pid` still owe a full re-score of its references (see the v12 migration)?
pub fn rescore_pending(c: &Connection, pid: i64) -> bool {
    c.query_row("SELECT 1 FROM meta WHERE k = 'rescore:' || ?1", [pid], |_| Ok(())).is_ok()
}

pub fn rescore_done(c: &Connection, pid: i64) -> Result<()> {
    c.execute("DELETE FROM meta WHERE k = 'rescore:' || ?1", [pid])?;
    Ok(())
}

/// The id ranges that hold every node of `pids` and no other node, sorted and merged. Saves
/// that alternate between projects leave one range each, so the list grows with interleaving:
/// callers bound it with [`cover`]. None when the store has no range table (an older layout).
pub fn ranges(c: &Connection, pids: &[i64]) -> Option<Vec<(i64, i64)>> {
    let list = pids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let mut q = c.prepare_cached(&format!("SELECT lo, hi FROM id_range WHERE project_id IN ({list}) ORDER BY lo")).ok()?;
    let mut all: Vec<(i64, i64)> = q.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))).ok()?.flatten().collect();
    // Ids written since the last sync (a writer that does not sync, or one still running).
    all.extend(runs_after(c, range_max(c)).ok()?.into_iter().filter(|r| pids.contains(&r.0)).map(|r| (r.1, r.2)));
    all.sort_unstable();
    let mut out: Vec<(i64, i64)> = vec![];
    for (lo, hi) in all {
        match out.last_mut() {
            Some(l) if lo <= l.1 + 1 => l.1 = l.1.max(hi),
            _ => out.push((lo, hi)),
        }
    }
    Some(out)
}

/// At most `max` ranges that hold every id of `rs`: the `max - 1` widest gaps between them stay
/// gaps, and the narrower ones (other projects' interleaved saves, deleted ids) are bridged. The
/// result may hold other projects' rows, so its users filter by project per match.
pub fn cover(rs: &[(i64, i64)], max: usize) -> Vec<(i64, i64)> {
    if rs.len() <= max.max(1) {
        return rs.to_vec();
    }
    let mut gaps: Vec<(i64, usize)> = rs.windows(2).enumerate().map(|(i, w)| (w[1].0 - w[0].1, i)).collect();
    gaps.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut split: Vec<usize> = gaps.into_iter().take(max.max(1) - 1).map(|g| g.1).collect();
    split.sort_unstable();
    let mut out: Vec<(i64, i64)> = vec![];
    let mut start = rs[0].0;
    for i in split {
        out.push((start, rs[i].1));
        start = rs[i + 1].0;
    }
    out.push((start, rs[rs.len() - 1].1));
    out
}

/// How many ids `rs` spans.
pub fn span(rs: &[(i64, i64)]) -> i64 {
    rs.iter().map(|(lo, hi)| hi - lo + 1).sum()
}

/// The highest node id in the store.
pub fn max_id(c: &Connection) -> i64 {
    c.query_row("SELECT COALESCE(MAX(id), 0) FROM node", [], |r| r.get(0)).unwrap_or(0)
}

/// Do `ranges` hold under half of the store's node ids? Then other projects dominate the shared
/// full-text index, and search should read only these slices of it.
pub fn minority(c: &Connection, ranges: &[(i64, i64)]) -> bool {
    span(ranges) * 2 < max_id(c)
}

/// The schema version this build writes.
pub const LATEST: i64 = 13;

/// Read-only connection for the hot paths. None if the store does not exist yet.
pub fn open_ro() -> Option<Connection> {
    let p = db_path();
    if !p.exists() {
        return None;
    }
    let ro = || Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok();
    let mut c = ro()?;
    // A store from an older build: read-only callers (MCP-only harnesses have no hook that would
    // open it read-write) migrate it once, then read.
    if c.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0)).is_ok_and(|v| v < LATEST) && open_rw().is_ok() {
        c = ro()?;
    }
    // Agents share this store: a reader may meet a writer's checkpoint, so wait a little.
    c.busy_timeout(std::time::Duration::from_millis(500)).ok()?;
    // Map the file: pages come straight from the OS page cache, shared by every reader process,
    // instead of being copied into each connection's private cache.
    let _ = c.pragma_update(None, "mmap_size", 1i64 << 30);
    let _ = c.pragma_update(None, "temp_store", "MEMORY");
    Some(c)
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
