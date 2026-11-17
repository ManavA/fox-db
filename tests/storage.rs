//! Storage: contentless FTS5 (no duplicate text copy), the v4 -> v5 migration, and the index
//! generation counter that keys caches.

use foxdb::{db, inventory, project, query};
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    (home, repo)
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn has_table(c: &Connection, name: &str) -> bool {
    c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = ?1", [name], |r| r.get::<_, i64>(0)).unwrap() > 0
}

fn matches(c: &Connection, table: &str, q: &str) -> i64 {
    c.query_row(&format!("SELECT COUNT(*) FROM {table} WHERE {table} MATCH ?1"), [q], |r| r.get(0)).unwrap()
}

#[test]
fn fresh_store_has_contentless_fts_that_still_matches_and_deletes() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    write(repo.path(), "docs/guide.md", "# Guide\n\n## Rotation\n\nzebrafish rotation policy lives here\n");
    let mut c = db::open_rw().unwrap();
    assert!(!has_table(&c, "fts_node_content"), "fts_node keeps a duplicate copy of the text");
    assert!(!has_table(&c, "fts_trigram_content"), "fts_trigram keeps a duplicate copy of the text");
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    assert_eq!(matches(&c, "fts_node", "zebrafish"), 1);
    assert!(matches(&c, "fts_trigram", "\"guide.md\"") >= 1);
    // Deleting a file removes its rows from both indexes (contentless_delete).
    fs::remove_file(repo.path().join("docs/guide.md")).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    assert_eq!(matches(&c, "fts_node", "zebrafish"), 0);
    assert_eq!(matches(&c, "fts_trigram", "\"guide.md\""), 0);
}

#[test]
fn brief_gate_text_is_read_from_the_file_not_the_index() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    write(
        repo.path(),
        "AGENT_MILESTONE_LOG.md",
        "# Log\n\n## Current gate\n\n- Gate: QX-7 is active.\n\n## 2026-10-19 — QX-7 thing — IN_PROGRESS\n\nbody\n",
    );
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    let p = project::find(&c, &project::locate(repo.path())).unwrap().unwrap();
    let b = query::brief(&c, &p, Some("main"), 400).unwrap();
    assert!(b.contains("Gate: QX-7 is active."), "{b}");
}

/// The v4 layout kept full text in `fts_node_content`. Opening it migrates to contentless tables
/// and keeps every match.
#[test]
fn v4_store_migrates_to_contentless_and_keeps_matches() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (home, _repo) = setup();
    let _ = home;
    {
        // A current store, rewound to the v4 FTS layout (full-content tables).
        let c = db::open_rw().unwrap();
        c.execute_batch(
            "DROP TABLE fts_node; DROP TABLE fts_trigram;
             CREATE VIRTUAL TABLE fts_node USING fts5(name, sig, path, body, tokenize='unicode61 remove_diacritics 2');
             CREATE VIRTUAL TABLE fts_trigram USING fts5(path, name, tokenize='trigram');
             INSERT INTO fts_node(rowid, name, sig, path, body) VALUES (7, 'Widget', '', 'src/widget.rs', 'quokka body text');
             INSERT INTO fts_trigram(rowid, path, name) VALUES (7, 'src/widget.rs', 'Widget');
             PRAGMA user_version = 4;",
        )
        .unwrap();
        assert!(has_table(&c, "fts_node_content"));
    }
    let c = db::open_rw().unwrap();
    assert!(!has_table(&c, "fts_node_content"));
    assert!(!has_table(&c, "fts_trigram_content"));
    assert_eq!(matches(&c, "fts_node", "quokka"), 1);
    assert_eq!(matches(&c, "fts_trigram", "\"widget\""), 1);
    let rid: i64 = c.query_row("SELECT rowid FROM fts_node WHERE fts_node MATCH 'quokka'", [], |r| r.get(0)).unwrap();
    assert_eq!(rid, 7);
    let v: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert!(v >= 5);
    // Re-opening is a no-op.
    drop(c);
    let c = db::open_rw().unwrap();
    assert_eq!(matches(&c, "fts_node", "quokka"), 1);
}

#[test]
fn generation_moves_on_index_writes_only() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    write(repo.path(), "a.md", "# A\n\n## One\n\ntext\n");
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    let g0 = db::generation(&c);
    inventory::index_project(&mut c, &p).unwrap();
    let g1 = db::generation(&c);
    assert!(g1 > g0, "index did not bump the generation");
    // Reading does not move it.
    let p = project::find(&c, &project::locate(repo.path())).unwrap().unwrap();
    let _ = query::brief(&c, &p, None, 400).unwrap();
    assert_eq!(db::generation(&c), g1);
    // A read-only connection sees the same value.
    let ro = db::open_ro().unwrap();
    assert_eq!(db::generation(&ro), g1);
}

/// Several processes (foxd, the CLI, hooks, background indexers) can open a v4 store at the same
/// moment. Exactly one migrates; the others wait for it and then see the migrated store. No
/// opener fails and no token is lost.
#[test]
fn concurrent_openers_of_a_v4_store_migrate_once_and_keep_every_match() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, _repo) = setup();
    const N: i64 = 60_000;
    {
        let c = db::open_rw().unwrap();
        c.execute_batch(
            "DROP TABLE fts_node; DROP TABLE fts_trigram;
             CREATE VIRTUAL TABLE fts_node USING fts5(name, sig, path, body, tokenize='unicode61 remove_diacritics 2');
             CREATE VIRTUAL TABLE fts_trigram USING fts5(path, name, tokenize='trigram');",
        )
        .unwrap();
        c.execute(
            &format!(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {N})
                 INSERT INTO fts_node(rowid, name, sig, path, body)
                 SELECT i, 'Widget' || i, 'fn widget_' || i || '()', 'src/w' || i || '.rs', 'quokka body text number ' || i FROM n"
            ),
            [],
        )
        .unwrap();
        c.execute_batch("INSERT INTO fts_trigram(rowid, path, name) SELECT rowid, path, name FROM fts_node; PRAGMA user_version = 4;")
            .unwrap();
    }
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(6));
    let handles: Vec<_> = (0..6)
        .map(|i| {
            let b = barrier.clone();
            std::thread::spawn(move || {
                b.wait();
                std::thread::sleep(std::time::Duration::from_millis(i * 5));
                db::open_rw().map(|_| ()).map_err(|e| format!("{e:#}"))
            })
        })
        .collect();
    let results: Vec<Result<(), String>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(results.iter().all(Result::is_ok), "every opener succeeds: {results:?}");
    let c = db::open_rw().unwrap();
    assert!(!has_table(&c, "fts_node_content"));
    assert_eq!(matches(&c, "fts_node", "quokka"), N, "tokens lost in a concurrent migration");
    assert_eq!(matches(&c, "fts_trigram", "\"src/w\""), N);
}
