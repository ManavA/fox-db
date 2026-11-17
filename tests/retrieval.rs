//! Milestone 5 search quality: merges kept out of the write path, snake_case names as whole
//! terms, the body arm (doc comments and bodies) for questions in words, routing of bare
//! identifiers to names only, and the backend seam.

use foxdb::search::{self, Field, Route, Scope, SearchBackend, Sqlite, Terms};
use foxdb::{db, inventory, project, query, tsindex};
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

fn setup() -> Env {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    let r = repo.path();
    write(
        r,
        "transport/pool.go",
        "package transport\n\n// rewindRequest makes the body readable again so the request can be retried on a fresh connection.\nfunc rewindRequest(req *Request) error {\n\treturn seekBody(req)\n}\n\nfunc seekBody(req *Request) error { return nil }\n",
    );
    write(
        r,
        "cache/fifo.go",
        "package cache\n\n// Collapse duplicate consecutive notifications for one object in the queue.\nfunc dedupDeltas(d Deltas) Deltas {\n\treturn d\n}\n\n// queueAction mentions dedupDeltas in a comment only.\nfunc queueAction() {}\n",
    );
    write(
        r,
        "dma/omap.c",
        "/* Context save hooks. */\nint omap_dma_context_notifier(int x) { return x; }\nint omap_dma_other(int x) { return x; }\n",
    );
    write(r, "docs/guide.md", "# Guide\n\n## Retrying requests\n\nA request is retried on a fresh connection after a reset.\n");
    let mut c = db::open_rw().unwrap();
    let p = index(&mut c, r);
    Env { _home: home, repo, c, p }
}

fn find(e: &Env, q: &str) -> String {
    query::find(&e.c, Some(&e.p), q, &query::FindOpts { kind: "any", all: false, budget: 2000, limit: 10, offset: 0 }).unwrap()
}

fn rank(out: &str, needle: &str) -> Option<usize> {
    out.lines().filter(|l| !l.starts_with('+') && !l.is_empty()).position(|l| l.contains(needle))
}

fn config(c: &rusqlite::Connection, t: &str, k: &str) -> Option<i64> {
    c.query_row(&format!("SELECT v FROM {t}_config WHERE k = ?1"), [k], |r| r.get(0)).ok()
}

#[test]
fn full_text_tables_merge_when_idle_not_inside_writes() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = setup();
    for t in db::FTS_TABLES {
        assert_eq!(config(&e.c, t, "automerge"), Some(0), "{t}");
        assert_eq!(config(&e.c, t, "deletemerge"), Some(0), "{t}");
    }
    // Several small writes leave several segments; idle merges and optimize fold them.
    for i in 0..6 {
        write(e.repo.path(), &format!("extra/f{i}.go"), &format!("package extra\n\nfunc extraFunc{i}() {{}}\n"));
        let mut c = db::open_rw().unwrap();
        index(&mut c, e.repo.path());
    }
    let before = db::fts_segments(&e.c, "fts_node");
    assert!(before > 1, "{before}");
    let mut steps = 0;
    while db::fts_merge(&e.c, 500).unwrap() && steps < 50 {
        steps += 1;
    }
    db::fts_optimize(&e.c).unwrap();
    assert_eq!(db::fts_segments(&e.c, "fts_node"), 1);
    assert!(!db::fts_merge(&e.c, 500).unwrap(), "nothing left to merge");
    // Search still answers after the merge.
    assert_eq!(rank(&find(&e, "extraFunc3"), "extraFunc3"), Some(0));
}

#[test]
fn snake_case_names_are_whole_terms_as_well_as_parts() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = setup();
    let n = |m: &str| -> i64 { e.c.query_row("SELECT COUNT(*) FROM fts_node WHERE fts_node MATCH ?1", [m], |r| r.get(0)).unwrap() };
    assert_eq!(n("\"omapdmacont\"*"), 1, "the whole name is one term, prefix-searchable");
    assert!(n("omap AND dma AND context") >= 1, "and its parts still match");
    assert_eq!(search::terms("omap_dma_cont")[0], "omapdmacont");
    assert_eq!(rank(&find(&e, "omap_dma_cont"), "omap_dma_context_notifier"), Some(0));
}

#[test]
fn doc_comments_and_bodies_answer_questions_in_words() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = setup();
    let out = find(&e, "make the request body readable again so it can be retried");
    assert!(rank(&out, "rewindRequest").is_some_and(|k| k < 3), "{out}");
    let out = find(&e, "collapse duplicate consecutive notifications");
    assert!(rank(&out, "dedupDeltas").is_some_and(|k| k < 3), "{out}");
    // Docs still answer sentences through the same arm.
    assert!(find(&e, "retried on a fresh connection after a reset").contains("Retrying requests"));
}

#[test]
fn bare_identifiers_read_names_only() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = setup();
    assert_eq!(search::route("dedupDeltas"), Route::Ident("dedupDeltas".into()));
    assert_eq!(search::route("Queue.dedupDeltas"), Route::Ident("dedupDeltas".into()));
    assert_eq!(search::route("collapse duplicate notifications"), Route::Text);
    // `queueAction` mentions the name only in its comment: the body arm would find it, the
    // names arm does not.
    let out = find(&e, "dedupDeltas");
    assert_eq!(rank(&out, "dedupDeltas"), Some(0), "{out}");
    assert!(rank(&out, "queueAction").is_none(), "{out}");
    let sc = Scope { projects: vec![e.p.id], ..Scope::default() };
    let be = Sqlite::new(&e.c);
    let t = Terms::new("dedupDeltas");
    let body = be.keyword(&t, Field::Body, &sc, 10).unwrap();
    let names = be.keyword(&t, Field::Names, &sc, 10).unwrap();
    assert!(body.len() > names.len(), "body {body:?} names {names:?}");
}

#[test]
fn the_backend_seam_renders_scope_and_serves_prefix_and_substring() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = setup();
    let be = Sqlite::new(&e.c);
    let all = Scope { projects: vec![e.p.id], ..Scope::default() };
    let pre = be.prefix("omap_dma_c", &all, 10).unwrap();
    assert_eq!(pre.len(), 1);
    assert!(!be.substring("dma_cont", &all, 10).unwrap().is_empty());
    // Kinds and a path prefix narrow the result.
    let docs = Scope { kinds: vec!["doc_section".into()], ..all.clone() };
    let t = Terms::new("request retried fresh connection");
    let ids = be.keyword(&t, Field::Body, &docs, 10).unwrap();
    assert!(!ids.is_empty());
    let kinds: Vec<String> =
        ids.iter().map(|id| e.c.query_row("SELECT kind FROM node WHERE id = ?1", [id], |r| r.get(0)).unwrap()).collect();
    assert!(kinds.iter().all(|k| k == "doc_section"), "{kinds:?}");
    let under = Scope { path_prefix: Some("cache/".into()), ..all.clone() };
    let ids = be.keyword(&Terms::new("notifications object queue"), Field::Body, &under, 10).unwrap();
    assert!(!ids.is_empty());
    for id in ids {
        let p: String = e.c.query_row("SELECT path FROM node WHERE id = ?1", [id], |r| r.get(0)).unwrap();
        assert!(p.starts_with("cache/"), "{p}");
    }
    // The vector arm is a seam only: no neighbours until a backend is plugged in.
    use search::VectorBackend;
    assert!(search::NoVectors.nearest(&[0.0; 4], &all, 5).unwrap().is_empty());
}

#[test]
fn an_upgraded_store_backfills_body_text_without_changing_handles() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = setup();
    let id_before: i64 = e.c.query_row("SELECT id FROM node WHERE name = 'rewindRequest'", [], |r| r.get(0)).unwrap();
    // Simulate a store indexed before the body arm: empty body rows, a backfill owed.
    e.c.execute("DELETE FROM fts_body", []).unwrap();
    db::request_reindex(&e.c, e.p.id).unwrap();
    let mut c = db::open_rw().unwrap();
    let st = tsindex::index_project(&mut c, &e.p).unwrap();
    assert_eq!(st.parsed, 0, "unchanged files are not re-stored");
    assert!(!db::reindex_pending(&c, e.p.id));
    let id_after: i64 = e.c.query_row("SELECT id FROM node WHERE name = 'rewindRequest'", [], |r| r.get(0)).unwrap();
    assert_eq!(id_before, id_after);
    let out = find(&e, "make the request body readable again so it can be retried");
    assert!(rank(&out, "rewindRequest").is_some_and(|k| k < 3), "{out}");
}

#[test]
fn body_text_takes_the_comment_block_then_the_definition_capped() {
    let src = b"package x\n\n// Line one of the doc.\n// Line two.\nfunc loadUserName() {\n\tvalue := fetch_user_record()\n}\n";
    let l = foxdb::lang::for_path("x.go").unwrap();
    let parsed = tsindex::extract(l, src);
    let d = parsed.defs.iter().find(|d| d.name == "loadUserName").unwrap();
    let t = tsindex::body_text(src, d);
    assert!(t.starts_with("Line one of the doc"), "{t}");
    assert!(t.contains("loadUserName load user name"), "{t}");
    assert!(t.contains("fetch_user_record fetchuserrecord"), "{t}");
    let long = format!("func big() {{\n{}}}\n", "\tcall_something_long()\n".repeat(400));
    let parsed = tsindex::extract(l, long.as_bytes());
    let t = tsindex::body_text(long.as_bytes(), &parsed.defs[0]);
    assert!(t.len() < tsindex::BODY_CHARS * 3, "{}", t.len());
}

fn copy_dir(from: &Path, to: &Path) {
    for e in fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        let dst = to.join(e.file_name());
        if p.is_dir() {
            fs::create_dir_all(&dst).unwrap();
            copy_dir(&p, &dst);
        } else {
            fs::copy(&p, &dst).unwrap();
        }
    }
}

/// The checked-in fixture set is a permanent quality gate: recall@5 and MRR per query type may
/// not fall below `bench/eval/fixture-baseline.json`. Raise the baseline when quality improves.
#[test]
fn the_fixture_eval_does_not_regress() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/eval");
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    copy_dir(&root.join("fixture"), repo.path());
    let mut c = db::open_rw().unwrap();
    let p = index(&mut c, repo.path());
    let set = foxdb::eval::load(&root.join("fixture-retrieval.json")).unwrap();
    let r = foxdb::eval::run(&c, &p, &set, 50).unwrap();
    let base: foxdb::eval::Report = serde_json::from_str(&fs::read_to_string(root.join("fixture-baseline.json")).unwrap()).unwrap();
    let bad = foxdb::eval::regressions(&r, &base);
    assert!(bad.is_empty(), "{}\n{}", bad.join("\n"), foxdb::eval::render(&r));
    // Bare identifiers are always found first.
    assert_eq!(r.types["exact"].mrr, 1.0, "{}", foxdb::eval::render(&r));
    // The gate itself trips on a drop.
    let mut worse = r.clone();
    worse.types.get_mut("vague").unwrap().recall5 -= 0.2;
    assert_eq!(foxdb::eval::regressions(&worse, &r).len(), 1);
}
