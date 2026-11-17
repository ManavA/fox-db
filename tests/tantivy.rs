//! Milestone 7: the tantivy index of a big project, derived from SQLite through the change log.
//! Built only with `--features tantivy`.
#![cfg(feature = "tantivy")]

use foxdb::search::{self, Field, Scope, SearchBackend, Sqlite, Terms};
use foxdb::tantivy_index::{self, Writers};
use foxdb::{db, inventory, outbox, project, query, tsindex};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

struct Env {
    _home: tempfile::TempDir,
    repo: tempfile::TempDir,
    c: rusqlite::Connection,
    p: project::Project,
}

fn index(c: &mut rusqlite::Connection, root: &Path) -> project::Project {
    let p = project::ensure(c, &project::locate(root)).unwrap();
    inventory::index_project(c, &p).unwrap();
    tsindex::index_project(c, &p).unwrap();
    project::find(c, &project::locate(root)).unwrap().unwrap()
}

fn setup(backend: &str) -> Env {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
        std::env::set_var("FOXDB_SEARCH_BACKEND", backend);
    }
    let r = repo.path();
    write(
        r,
        "src/net/retry.rs",
        "/// Make an HTTP request body readable again so it can be retried.\npub fn rewind_request(b: u8) -> u8 { b }\n\npub fn retryBackoff() {}\n",
    );
    write(
        r,
        "src/store/cache.rs",
        "/// Evict the least recently used entry when the cache is full.\npub fn evict_oldest() {}\npub struct CacheEntry { size: u32 }\n",
    );
    write(r, "src/ui/player_view.kt", "class PlayerViewModel {\n  fun loadSubtitleTrack() {}\n  fun seekTo(ms: Long) {}\n}\n");
    write(r, "docs/guide.md", "# Guide\n\n## Subtitle selection\n\nHow the player picks a subtitle track by language.\n");
    let mut c = db::open_rw().unwrap();
    let p = index(&mut c, r);
    Env { _home: home, repo, c, p }
}

fn opts() -> query::FindOpts<'static> {
    query::FindOpts { kind: "any", all: false, budget: usize::MAX, limit: 10, offset: 0 }
}

fn names(e: &Env, q: &str) -> Vec<String> {
    search::hits(&e.c, Some(&e.p), q, &opts(), 10).unwrap().into_iter().map(|h| h.name).collect()
}

/// Does a search in this project read the tantivy index right now?
fn reads_tantivy(e: &Env) -> bool {
    let sc = search::scope(&e.c, Some(&e.p), &opts());
    tantivy_index::for_scope(&e.c, &sc, None).is_some()
}

const QUERIES: &[&str] = &[
    "rewind_request",
    "retryBackoff",
    "PlayerViewModel",
    "evict",
    "Cache",
    "subtitle track by language",
    "player_v",
    "least recently used",
];

#[test]
fn a_small_store_on_auto_gets_no_index_tables_or_files() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = setup("auto");
    assert_eq!(tantivy_index::maintain(&e.c, e.p.id, &e.p.name).unwrap(), None);
    assert!(!outbox::exists(&e.c), "no outbox below the trigger");
    assert!(!tantivy_index::root().exists(), "no tantivy directory below the trigger");
    assert!(!reads_tantivy(&e));
    let mut w = Writers::default();
    assert_eq!(w.drain(&e.c).unwrap(), 0);
}

#[test]
fn filled_index_answers_like_fts5_and_follows_saves() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = setup("tantivy");
    assert_eq!(tantivy_index::maintain(&e.c, e.p.id, &e.p.name).unwrap(), Some("enabled"));
    // The fill: the next index run writes every row to the log.
    e.p = index(&mut e.c, e.repo.path());
    assert_eq!(outbox::state(&e.c, e.p.id).unwrap().1, "live");
    assert!(outbox::pending(&e.c, e.p.id, 0) > 0);
    // Not applied yet: searches use FTS5.
    assert!(!reads_tantivy(&e));
    let fts: Vec<Vec<String>> = QUERIES.iter().map(|q| names(&e, q)).collect();
    let mut w = Writers::default();
    assert!(w.drain(&e.c).unwrap() > 0);
    assert_eq!(outbox::pending(&e.c, e.p.id, 0), 0, "applied rows are deleted");
    assert!(reads_tantivy(&e));
    for (q, want) in QUERIES.iter().zip(&fts) {
        let got = names(&e, q);
        // Ranking may differ (per-field BM25 against FTS5's one weighted score); the best hit stays near the top.
        assert!(want.first().is_none_or(|w| got.iter().take(3).any(|g| g == w)), "{q}: tantivy {got:?} vs fts5 {want:?}");
        let overlap = got.iter().filter(|n| want.contains(n)).count();
        assert!(overlap * 4 >= want.len().min(10) * 3, "{q}: tantivy {got:?} vs fts5 {want:?}");
    }
    // The backends agree arm by arm on substring and keyword sets.
    let sc = search::scope(&e.c, Some(&e.p), &opts());
    let (h, rest) = tantivy_index::for_scope(&e.c, &sc, None).unwrap();
    let tv = tantivy_index::Tantivy { c: &e.c, h: &h, rest: &rest };
    let sq = Sqlite::new(&e.c);
    let sorted = |mut v: Vec<i64>| {
        v.sort();
        v
    };
    for needle in ["layer", "cache", "retry"] {
        assert_eq!(sorted(tv.substring(needle, &sc, 60).unwrap()), sorted(sq.substring(needle, &sc, 60).unwrap()), "{needle}");
    }
    for q in ["rewind_request", "loadSubtitleTrack", "cache entry"] {
        let t = Terms::new(q);
        let f = if q.contains(' ') { Field::Body } else { Field::Names };
        assert_eq!(sorted(tv.keyword(&t, f, &sc, 60).unwrap()), sorted(sq.keyword(&t, f, &sc, 60).unwrap()), "{q}");
    }
    let _ = Scope::default();

    // A save: logged in the same transaction, served by FTS5 until applied, then by tantivy.
    write(e.repo.path(), "src/net/retry.rs", "pub fn zebraCrossingSignal() {}\n");
    e.p = index(&mut e.c, e.repo.path());
    assert!(!reads_tantivy(&e), "a lagging index is not read");
    assert_eq!(names(&e, "zebraCrossingSignal").first().map(String::as_str), Some("zebraCrossingSignal"));
    assert!(!names(&e, "rewind_request").iter().any(|n| n == "rewind_request"));
    // A new writer (a restarted foxd) replays from its payload.
    drop(w);
    let mut w = Writers::default();
    assert!(w.drain(&e.c).unwrap() > 0);
    assert!(reads_tantivy(&e));
    assert_eq!(names(&e, "zebraCrossingSignal").first().map(String::as_str), Some("zebraCrossingSignal"));
    assert!(!names(&e, "rewind_request").iter().any(|n| n == "rewind_request"), "deleted rows leave the index");
    // Readers open while the writer holds its lock: the CLI never needs the writer.
    assert!(reads_tantivy(&e));
}

#[test]
fn broken_or_ahead_indexes_fall_back_and_rebuild() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = setup("tantivy");
    tantivy_index::maintain(&e.c, e.p.id, &e.p.name).unwrap();
    e.p = index(&mut e.c, e.repo.path());
    let want = names(&e, "PlayerViewModel");
    Writers::default().drain(&e.c).unwrap();
    assert!(reads_tantivy(&e));

    // An index ahead of SQLite (SQLite lost a transaction): never read; rebuilt.
    e.c.execute("UPDATE sqlite_sequence SET seq = 1 WHERE name = 'search_outbox'", []).unwrap();
    assert!(!reads_tantivy(&e));
    assert_eq!(names(&e, "PlayerViewModel"), want, "FTS5 answers meanwhile");
    Writers::default().drain(&e.c).unwrap();
    assert_eq!(outbox::state(&e.c, e.p.id).unwrap().1, "fill");
    e.p = index(&mut e.c, e.repo.path());
    Writers::default().drain(&e.c).unwrap();
    assert!(reads_tantivy(&e));
    assert_eq!(names(&e, "PlayerViewModel").first(), want.first());

    // An unreadable index: deleted and rebuilt, FTS5 meanwhile.
    fs::write(tantivy_index::dir(e.p.id).join("meta.json"), "{ not json").unwrap();
    assert!(!reads_tantivy(&e));
    assert_eq!(names(&e, "PlayerViewModel").first(), want.first());
    Writers::default().drain(&e.c).unwrap();
    assert_eq!(outbox::state(&e.c, e.p.id).unwrap().1, "fill");
    e.p = index(&mut e.c, e.repo.path());
    Writers::default().drain(&e.c).unwrap();
    assert!(reads_tantivy(&e));

    // Switched off: the log and the files go.
    unsafe { std::env::set_var("FOXDB_SEARCH_BACKEND", "fts5") };
    assert_eq!(tantivy_index::maintain(&e.c, e.p.id, &e.p.name).unwrap(), Some("disabled"));
    Writers::default().drain(&e.c).unwrap();
    assert!(!tantivy_index::dir(e.p.id).exists());
    assert!(!reads_tantivy(&e));
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// The segment files of the project's index with extension `ext`.
fn segment_files(e: &Env, ext: &str) -> Vec<std::path::PathBuf> {
    fs::read_dir(tantivy_index::dir(e.p.id))
        .unwrap()
        .flatten()
        .map(|x| x.path())
        .filter(|p| p.extension().is_some_and(|x| x == ext))
        .collect()
}

/// A live index, applied; returns the FTS5 answers to `QUERIES`, taken before it served.
fn live(e: &mut Env) -> Vec<Vec<String>> {
    tantivy_index::maintain(&e.c, e.p.id, &e.p.name).unwrap();
    e.p = index(&mut e.c, e.repo.path());
    let fts: Vec<Vec<String>> = QUERIES.iter().map(|q| sorted(names(e, q))).collect();
    Writers::default().drain(&e.c).unwrap();
    assert!(reads_tantivy(e));
    fts
}

/// Every answer is the FTS5 one (as sets: the ranking may differ).
fn answers_complete(e: &Env, fts: &[Vec<String>], label: &str) {
    for (q, want) in QUERIES.iter().zip(fts) {
        assert_eq!(&sorted(names(e, q)), want, "{label}: {q}");
    }
}

#[test]
fn a_deleted_index_directory_is_refilled_never_served_empty() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = setup("tantivy");
    let fts = live(&mut e);

    // The project's directory deleted, then foxd restarted and one save.
    fs::remove_dir_all(tantivy_index::dir(e.p.id)).unwrap();
    let mut w = Writers::default();
    w.drain(&e.c).unwrap();
    assert_eq!(outbox::state(&e.c, e.p.id).unwrap().1, "fill", "the drained rows are gone: a refill is owed");
    assert!(!reads_tantivy(&e), "an index missing the drained rows is never read");
    answers_complete(&e, &fts, "directory deleted");
    // The next save fills it again.
    write(e.repo.path(), "docs/extra.md", "# Extra\n\nNothing to see.\n");
    e.p = index(&mut e.c, e.repo.path());
    w.drain(&e.c).unwrap();
    assert!(reads_tantivy(&e));
    answers_complete(&e, &fts, "refilled");

    // The whole tantivy directory deleted (foxd only drains while it exists): the next index
    // run asks for the refill and does it.
    fs::remove_dir_all(tantivy_index::root()).unwrap();
    assert!(!reads_tantivy(&e));
    e.p = index(&mut e.c, e.repo.path());
    assert_eq!(tantivy_index::maintain(&e.c, e.p.id, &e.p.name).unwrap(), Some("enabled"));
    e.p = index(&mut e.c, e.repo.path());
    Writers::default().drain(&e.c).unwrap();
    assert!(reads_tantivy(&e));
    answers_complete(&e, &fts, "root deleted, refilled");
    // A filled, applied index is not refilled again.
    assert_eq!(tantivy_index::maintain(&e.c, e.p.id, &e.p.name).unwrap(), None);
    Writers::default().drain(&e.c).unwrap();
    assert!(reads_tantivy(&e));
}

#[test]
fn damaged_segment_files_fall_back_to_fts5_and_are_rebuilt() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for (label, ext, damage) in [
        ("postings overwritten", "idx", 0u8),
        ("postings deleted", "idx", 1),
        ("terms truncated", "term", 2),
        ("fast fields overwritten", "fast", 0),
    ] {
        let mut e = setup("tantivy");
        let fts = live(&mut e);
        // A running foxd, with the index open since before the damage.
        let mut w = Writers::default();
        w.drain(&e.c).unwrap();
        for f in segment_files(&e, ext) {
            match damage {
                0 => {
                    let n = fs::metadata(&f).unwrap().len() as usize;
                    fs::write(&f, vec![0xa5u8; n]).unwrap();
                }
                1 => fs::remove_file(&f).unwrap(),
                _ => {
                    let b = fs::read(&f).unwrap();
                    fs::write(&f, &b[..b.len() / 2]).unwrap();
                }
            }
        }
        // Searches never fail or lose hits (FTS5 answers), whatever tantivy does on the files.
        answers_complete(&e, &fts, label);
        let st = tantivy_index::status_line(&e.c, e.p.id).unwrap();
        assert!(st.contains("damaged"), "{label}: {st}");
        assert!(!reads_tantivy(&e), "{label}: status found it damaged, so searches stop reading it");
        // The running foxd checks it again and asks for a rebuild, which the next index run fills.
        w.drain(&e.c).unwrap();
        assert_eq!(outbox::state(&e.c, e.p.id).unwrap().1, "fill", "{label}");
        e.p = index(&mut e.c, e.repo.path());
        w.drain(&e.c).unwrap();
        assert!(reads_tantivy(&e), "{label}");
        answers_complete(&e, &fts, label);
    }
}
