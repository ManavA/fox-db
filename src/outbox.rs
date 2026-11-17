//! The change log a derived search index (tantivy) is fed from. SQLite stays the system of
//! record: every write to a full-text row of a project that has a derived index also appends one
//! row here, inside the same transaction, so SQLite's commit is the single point of truth. `foxd`
//! applies the rows in `seq` order, commits the derived index with the last `seq` as its
//! payload, and only then deletes them (see `tantivy_index`).
//!
//! A store without a derived index has neither table: [`Sink::open`] costs one `sqlite_master`
//! lookup per index run, and every write call is a no-op.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, ToSql, params};

/// Which full-text table a logged row mirrors.
pub const TBL_NODE: i64 = 0;
pub const TBL_TRI: i64 = 1;
pub const TBL_BODY: i64 = 2;

pub const OP_UPSERT: i64 = 0;
pub const OP_DELETE: i64 = 1;
/// Drop everything the derived index holds for the project (the first row of a fill).
pub const OP_RESET: i64 = 2;

/// The tables, created when the first project gets a derived index. `search_index.state` is
/// `fill` (the next index run of the project writes every row to the log once) or `live`;
/// `fill_seq` is the last `seq` of the fill, which the derived index must have applied before
/// any query may use it.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS search_index(
  project_id INTEGER PRIMARY KEY, backend TEXT NOT NULL, state TEXT NOT NULL,
  fill_seq INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS search_outbox(
  seq INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL, rowid INTEGER NOT NULL,
  tbl INTEGER NOT NULL, op INTEGER NOT NULL, c0 TEXT, c1 TEXT, c2 TEXT, c3 TEXT);
CREATE INDEX IF NOT EXISTS search_outbox_project ON search_outbox(project_id, seq);";

/// Does this store have a derived index at all?
pub fn exists(c: &Connection) -> bool {
    c.query_row("SELECT 1 FROM sqlite_master WHERE name = 'search_index'", [], |_| Ok(())).is_ok()
}

/// The projects whose full-text writes are logged, for one write transaction.
#[derive(Debug, Default, Clone)]
pub struct Sink {
    pids: Vec<i64>,
}

impl Sink {
    /// Logging off (no derived index anywhere).
    pub fn off() -> Sink {
        Sink::default()
    }

    /// The projects with a derived index in this store (none: every call is a no-op).
    pub fn open(c: &Connection) -> Sink {
        if !exists(c) {
            return Sink::off();
        }
        let pids = c
            .prepare("SELECT project_id FROM search_index")
            .and_then(|mut q| q.query_map([], |r| r.get(0)).map(|r| r.flatten().collect()))
            .unwrap_or_default();
        Sink { pids }
    }

    pub fn on(&self, pid: i64) -> bool {
        !self.pids.is_empty() && self.pids.contains(&pid)
    }

    /// Log an insert of `cols` (name, sig, path, body; `path, name` for the trigram table).
    pub fn put(&self, c: &Connection, pid: i64, tbl: i64, rowid: i64, cols: &[&str]) -> Result<()> {
        if !self.on(pid) {
            return Ok(());
        }
        let col = |i: usize| cols.get(i).copied().unwrap_or("");
        c.prepare_cached("INSERT INTO search_outbox(project_id, rowid, tbl, op, c0, c1, c2, c3) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?
            .execute(params![pid, rowid, tbl, OP_UPSERT, col(0), col(1), col(2), col(3)])?;
        Ok(())
    }

    /// Log a delete of one row.
    pub fn del(&self, c: &Connection, pid: i64, tbl: i64, rowid: i64) -> Result<()> {
        if !self.on(pid) {
            return Ok(());
        }
        c.prepare_cached("INSERT INTO search_outbox(project_id, rowid, tbl, op) VALUES (?1, ?2, ?3, ?4)")?
            .execute(params![pid, rowid, tbl, OP_DELETE])?;
        Ok(())
    }

    /// Log a delete of every row `sel` (a `SELECT id ...` over `node`) returns. Run it before
    /// the statement that deletes those `node` rows.
    pub fn del_where(&self, c: &Connection, pid: i64, tbl: i64, sel: &str, p: &[&dyn ToSql]) -> Result<()> {
        if !self.on(pid) {
            return Ok(());
        }
        let mut q = c.prepare_cached(sel)?;
        let ids: Vec<i64> = q.query_map(p, |r| r.get(0))?.flatten().collect();
        for id in ids {
            self.del(c, pid, tbl, id)?;
        }
        Ok(())
    }

    /// Log a reset: the derived index drops the project's rows before applying what follows.
    pub fn reset(&self, c: &Connection, pid: i64) -> Result<()> {
        if !self.on(pid) {
            return Ok(());
        }
        c.execute("INSERT INTO search_outbox(project_id, rowid, tbl, op) VALUES (?1, 0, 0, ?2)", params![pid, OP_RESET])?;
        Ok(())
    }
}

/// Project `pid`'s derived-index state: (backend, state, fill_seq), if it has one.
pub fn state(c: &Connection, pid: i64) -> Option<(String, String, i64)> {
    if !exists(c) {
        return None;
    }
    c.query_row("SELECT backend, state, fill_seq FROM search_index WHERE project_id = ?1", [pid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()
        .ok()
        .flatten()
}

/// Does project `pid` owe a fill (every full-text row written to the log once)?
pub fn fill_pending(c: &Connection, pid: i64) -> bool {
    state(c, pid).is_some_and(|s| s.1 == "fill")
}

/// Can a new, empty derived index of `pid` be built from the log alone? Only while a fill is
/// owed, or when the project's oldest logged row is the reset that starts its fill (nothing of
/// the fill was applied and deleted yet).
pub fn log_is_whole(c: &Connection, pid: i64) -> bool {
    fill_pending(c, pid)
        || c.query_row("SELECT op FROM search_outbox WHERE project_id = ?1 ORDER BY seq LIMIT 1", [pid], |r| r.get::<_, i64>(0))
            .is_ok_and(|op| op == OP_RESET)
}

/// Give project `pid` a derived index (or ask for its rebuild): its next index run writes every
/// full-text row to the log, after a reset.
pub fn request_fill(c: &Connection, pid: i64, backend: &str) -> Result<()> {
    c.execute_batch(SCHEMA)?;
    c.execute(
        "INSERT INTO search_index(project_id, backend, state, fill_seq, updated_at) VALUES (?1, ?2, 'fill', 0, ?3)
         ON CONFLICT(project_id) DO UPDATE SET backend = excluded.backend, state = 'fill', updated_at = excluded.updated_at",
        params![pid, backend, crate::db::now()],
    )?;
    // The fill starts with a reset and writes every row again: older rows are superseded.
    c.execute("DELETE FROM search_outbox WHERE project_id = ?1", [pid])?;
    Ok(())
}

/// The fill of `pid` was written: the derived index serves once it has applied up to here.
pub fn fill_done(c: &Connection, pid: i64) -> Result<()> {
    // The project's own last row: its index's payload is the last `seq` it applied. Rows drained
    // already are below whatever payload the index has committed.
    let seq: i64 = c.query_row("SELECT COALESCE(MAX(seq), 0) FROM search_outbox WHERE project_id = ?1", [pid], |r| r.get(0)).unwrap_or(0);
    c.execute(
        "UPDATE search_index SET state = 'live', fill_seq = ?2, updated_at = ?3 WHERE project_id = ?1",
        params![pid, seq, crate::db::now()],
    )?;
    Ok(())
}

/// Remove project `pid`'s derived index from the store (its log rows and state).
pub fn disable(c: &Connection, pid: i64) -> Result<()> {
    if !exists(c) {
        return Ok(());
    }
    c.execute("DELETE FROM search_outbox WHERE project_id = ?1", [pid])?;
    c.execute("DELETE FROM search_index WHERE project_id = ?1", [pid])?;
    Ok(())
}

/// The highest `seq` ever handed out (0 for none). Drained rows are deleted, so this comes from
/// `sqlite_sequence`, which AUTOINCREMENT keeps.
pub fn high_water(c: &Connection) -> i64 {
    c.query_row("SELECT seq FROM sqlite_sequence WHERE name = 'search_outbox'", [], |r| r.get(0)).unwrap_or(0)
}

/// Rows of project `pid` not yet applied by an index whose payload is `applied`.
pub fn pending(c: &Connection, pid: i64, applied: i64) -> i64 {
    c.query_row("SELECT COUNT(*) FROM search_outbox WHERE project_id = ?1 AND seq > ?2", params![pid, applied], |r| r.get(0)).unwrap_or(0)
}

/// Is any row of `pid` newer than `applied`? (One index probe.)
pub fn behind(c: &Connection, pid: i64, applied: i64) -> bool {
    c.query_row("SELECT 1 FROM search_outbox WHERE project_id = ?1 AND seq > ?2 LIMIT 1", params![pid, applied], |_| Ok(())).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE node(id INTEGER PRIMARY KEY, project_id INTEGER, path TEXT)").unwrap();
        c
    }

    #[test]
    fn off_without_tables_and_logs_only_enabled_projects() {
        let c = store();
        let s = Sink::open(&c);
        assert!(!s.on(1));
        s.put(&c, 1, TBL_NODE, 5, &["a", "b", "c", "d"]).unwrap();
        assert!(!exists(&c), "a store without a derived index gains no tables");
        request_fill(&c, 1, "tantivy").unwrap();
        let s = Sink::open(&c);
        s.put(&c, 1, TBL_NODE, 5, &["a", "b", "c", "d"]).unwrap();
        s.put(&c, 2, TBL_NODE, 6, &["a", "b", "c", "d"]).unwrap();
        c.execute("INSERT INTO node VALUES (7, 1, 'x'), (8, 1, 'x'), (9, 1, 'y')", []).unwrap();
        s.del_where(&c, 1, TBL_TRI, "SELECT id FROM node WHERE project_id = ?1 AND path = ?2", &[&1i64, &"x"]).unwrap();
        assert_eq!(pending(&c, 1, 0), 3);
        assert_eq!(pending(&c, 2, 0), 0);
        assert!(fill_pending(&c, 1));
        fill_done(&c, 1).unwrap();
        assert_eq!(state(&c, 1).unwrap().2, 3);
        assert!(behind(&c, 1, 2) && !behind(&c, 1, 3));
        assert_eq!(high_water(&c), 3);
    }
}
