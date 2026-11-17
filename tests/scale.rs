//! Milestone 4, scale: planner statistics after indexing, change-sized incremental resolution,
//! project filtering inside FTS ranking, per-project warm graphs, and name-guessed call labels.

use foxdb::{code, db, inventory, project, query, search, tsindex};
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: tests touching the environment hold SERIAL.
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
        std::env::set_var("FOXDB_NO_DAEMON", "1");
    }
    home
}

fn index(c: &mut Connection, root: &Path) -> (project::Project, tsindex::Stats) {
    let p = project::ensure(c, &project::locate(root)).unwrap();
    inventory::index_project(c, &p).unwrap();
    let st = tsindex::index_project(c, &p).unwrap();
    (project::find(c, &project::locate(root)).unwrap().unwrap(), st)
}

/// A small Go module: same-name methods in one package, unique helpers, and leaf files.
fn go_repo(root: &Path) {
    write(root, "pkg/a/x.go", "package a\n\ntype T struct{}\n\nfunc (t *T) Errorf() {}\n\nfunc Helper() {}\n");
    write(root, "pkg/a/y.go", "package a\n\ntype U struct{}\n\nfunc (u *U) Errorf() {}\n");
    write(root, "pkg/a/z.go", "package a\n\nfunc Use(t *T) {\n\tt.Errorf()\n\tHelper()\n}\n");
    for i in 0..12 {
        write(
            root,
            &format!("pkg/b/leaf{i}.go"),
            &format!("package b\n\nfunc Leaf{i}() {{\n\tHelper()\n\tString()\n}}\n\nfunc String() string {{ return \"\" }}\n"),
        );
    }
}

/// (path, line, name, dst path:line or "", ambiguity) for every reference, ids replaced by locations.
fn refs_by_location(c: &Connection) -> Vec<(String, i64, String, String, i64)> {
    let mut q = c
        .prepare(
            "SELECT r.path, r.line, r.name, COALESCE(d.path || ':' || d.line, ''), r.amb FROM ts_ref r LEFT JOIN node d ON d.id = r.dst
             ORDER BY 1, 2, 3, 4",
        )
        .unwrap();
    q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap().flatten().collect()
}

fn edges_by_location(c: &Connection) -> Vec<(String, String, String)> {
    let mut q = c
        .prepare(
            "SELECT s.path || ':' || s.line, d.path || ':' || d.line, e.kind FROM edge e JOIN node s ON s.id = e.src
             JOIN node d ON d.id = e.dst WHERE e.source = 'treesitter' ORDER BY 1, 2, 3",
        )
        .unwrap();
    q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().flatten().collect()
}

#[test]
fn indexing_leaves_planner_statistics_so_per_file_deletes_use_the_path_index() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let repo = tempfile::tempdir().unwrap();
    go_repo(repo.path());
    let mut c = db::open_rw().unwrap();
    index(&mut c, repo.path());
    let stats: i64 = c.query_row("SELECT COUNT(*) FROM sqlite_stat1 WHERE tbl = 'node'", [], |r| r.get(0)).unwrap();
    assert!(stats > 0, "no planner statistics for node after indexing");
    let plan: String = c
        .prepare(
            "EXPLAIN QUERY PLAN SELECT id FROM node WHERE project_id = 1 AND path = 'pkg/a/x.go' AND kind = 'symbol' AND source = 'treesitter'",
        )
        .unwrap()
        .query_map([], |r| r.get::<_, String>(3))
        .unwrap()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ");
    assert!(!plan.contains("node_sym_probe"), "per-file delete scans every project symbol: {plan}");
}

#[test]
fn incremental_resolve_rescores_only_the_change_and_matches_a_full_resolve() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let repo = tempfile::tempdir().unwrap();
    go_repo(repo.path());
    let mut c = db::open_rw().unwrap();
    let (p, cold) = index(&mut c, repo.path());
    let total: usize = c.query_row("SELECT COUNT(*) FROM ts_ref WHERE kind != 'import'", [], |r| r.get::<_, i64>(0)).unwrap() as usize;
    assert_eq!(cold.rescored, total, "a cold index scores every reference");

    // A leaf edit: only that file's references (its names are unique) are scored again.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(repo.path(), "pkg/b/leaf3.go", "package b\n\nfunc Leaf3() {\n\tHelper()\n}\n");
    inventory::index_project(&mut c, &p).unwrap();
    let st = tsindex::index_project(&mut c, &p).unwrap();
    assert_eq!(st.parsed, 1);
    let in_file: usize = c
        .query_row("SELECT COUNT(*) FROM ts_ref WHERE path = 'pkg/b/leaf3.go' AND kind != 'import'", [], |r| r.get::<_, i64>(0))
        .unwrap() as usize;
    // The file no longer defines `String`, so `String` references elsewhere are re-scored too.
    let named: usize = c
        .query_row("SELECT COUNT(*) FROM ts_ref WHERE path != 'pkg/b/leaf3.go' AND name IN ('String', 'Leaf3')", [], |r| r.get::<_, i64>(0))
        .unwrap() as usize;
    assert_eq!(st.rescored, in_file + named, "{st:?}");
    assert!(st.rescored < total / 2, "{st:?} of {total}");

    // A new definition of a shared name, then a removal: incremental equals a full re-resolve.
    write(repo.path(), "pkg/a/w.go", "package a\n\ntype V struct{}\n\nfunc (v *V) Errorf() {}\n\nfunc Helper2() { Helper() }\n");
    fs::remove_file(repo.path().join("pkg/b/leaf5.go")).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    let (inc_r, inc_e) = (refs_by_location(&c), edges_by_location(&c));
    tsindex::resolve(&c, p.id).unwrap();
    assert_eq!(refs_by_location(&c), inc_r);
    assert_eq!(edges_by_location(&c), inc_e);
}

#[test]
fn receiver_calls_with_several_same_name_targets_are_labelled_guesses() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let repo = tempfile::tempdir().unwrap();
    go_repo(repo.path());
    let mut c = db::open_rw().unwrap();
    let (p, _) = index(&mut c, repo.path());
    let amb = |name: &str| -> i64 {
        c.query_row("SELECT amb FROM ts_ref WHERE path = 'pkg/a/z.go' AND name = ?1", [name], |r| r.get(0)).unwrap()
    };
    // `t.Errorf()`: two methods named Errorf in the package, the receiver type is unknown.
    assert_eq!(amb("Errorf"), 2);
    // `Helper()`: one definition, so it is not a guess.
    assert_eq!(amb("Helper"), 0);
    let ro = db::open_ro().unwrap();
    let target: String = ro
        .query_row("SELECT d.path FROM ts_ref r JOIN node d ON d.id = r.dst WHERE r.path = 'pkg/a/z.go' AND r.name = 'Errorf'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let parent = if target.ends_with("x.go") { "T" } else { "U" };
    let opts = code::CodeOpts { rel: "callers", depth: 1, budget: 800, cursor: 0, fallback: false };
    let out = code::run(&ro, &p, &format!("{parent}.Errorf"), &opts).unwrap();
    assert!(out.contains("Use c:") && out.contains("guess 1 of 2"), "{out}");
    assert!(out.contains("1 guessed by name"), "{out}");
    let out = code::run(&ro, &p, "Helper", &opts).unwrap();
    assert!(out.contains("Use c:") && !out.contains("guess"), "{out}");
}

#[test]
fn project_scope_is_applied_before_ranking_so_a_busy_neighbour_hides_nothing() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let small = tempfile::tempdir().unwrap();
    let busy = tempfile::tempdir().unwrap();
    write(small.path(), "csi/handlers.rs", "pub fn csi_driver_attach_volume_handler() {}\npub fn csi_driver_detach_volume_handler() {}\n");
    for f in 0..6 {
        let body: String = (0..300).map(|i| format!("pub fn driver{f}x{i}() {{}}\n")).collect();
        write(busy.path(), &format!("drivers/d{f}.rs"), &body);
    }
    let mut c = db::open_rw().unwrap();
    let (sp, _) = index(&mut c, small.path());
    index(&mut c, busy.path());
    let o = query::FindOpts { kind: "symbol", all: false, budget: 2000, limit: 20, offset: 0 };
    let out = query::find(&c, Some(&sp), "driver", &o).unwrap();
    assert!(out.contains("csi_driver_attach_volume_handler") && out.contains("csi_driver_detach_volume_handler"), "{out}");
    let out = query::find(&c, Some(&sp), "driver_attach", &o).unwrap();
    assert!(out.contains("csi_driver_attach_volume_handler"), "{out}");
}

#[test]
fn warm_graphs_reload_only_the_project_whose_index_moved() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    write(a.path(), "src/lib.rs", "pub fn alpha() { beta() }\npub fn beta() {}\n");
    write(b.path(), "src/lib.rs", "pub fn gamma() { delta() }\npub fn delta() {}\n");
    let mut c = db::open_rw().unwrap();
    let (pa, _) = index(&mut c, a.path());
    let (pb, _) = index(&mut c, b.path());
    let mut warm = search::Warm::default();
    warm.refresh(&c).unwrap();
    let ids = search::graph_arm(&c, Some(warm.graph(&c, pa.id).unwrap()), "alpha", pa.id, 10).unwrap();
    assert_eq!(ids.len(), 1);
    search::graph_arm(&c, Some(warm.graph(&c, pb.id).unwrap()), "gamma", pb.id, 10).unwrap();
    assert_eq!(warm.graph_loads(), 2);
    // Project b changes: a's graph is kept, b's is rebuilt and sees the new caller.
    write(b.path(), "src/more.rs", "pub fn epsilon() { delta() }\n");
    inventory::index_project(&mut c, &pb).unwrap();
    tsindex::index_project(&mut c, &pb).unwrap();
    warm.refresh(&c).unwrap();
    warm.graph(&c, pa.id).unwrap();
    assert_eq!(warm.graph_loads(), 2, "an unrelated project's graph was reloaded");
    let delta: i64 = c.query_row("SELECT id FROM node WHERE name = 'delta' AND kind = 'symbol'", [], |r| r.get(0)).unwrap();
    let beta: i64 = c.query_row("SELECT id FROM node WHERE name = 'beta' AND kind = 'symbol'", [], |r| r.get(0)).unwrap();
    let g = warm.graph(&c, pb.id).unwrap();
    assert_eq!(g.callers(delta).count(), 2);
    // No edges of project a leak into b's graph.
    assert_eq!(g.callers(beta).count(), 0);
    assert_eq!(warm.graph_loads(), 3);
}

#[test]
fn id_ranges_cover_every_node_of_a_project_through_interleaved_writes_and_compaction() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    write(a.path(), "src/lib.rs", "pub fn alpha() {}\n");
    write(b.path(), "src/lib.rs", "pub fn gamma() {}\n");
    let mut c = db::open_rw().unwrap();
    let (pa, _) = index(&mut c, a.path());
    let (pb, _) = index(&mut c, b.path());
    // Interleave: a grows after b, b after that, a file of a is removed and re-added.
    for i in 0..3 {
        write(a.path(), &format!("src/a{i}.rs"), &format!("pub fn a{i}() {{ alpha() }}\n"));
        inventory::index_project(&mut c, &pa).unwrap();
        tsindex::index_project(&mut c, &pa).unwrap();
        write(b.path(), &format!("src/b{i}.rs"), &format!("pub fn b{i}() {{ gamma() }}\n"));
        inventory::index_project(&mut c, &pb).unwrap();
        tsindex::index_project(&mut c, &pb).unwrap();
    }
    fs::remove_file(a.path().join("src/a0.rs")).unwrap();
    inventory::index_project(&mut c, &pa).unwrap();
    tsindex::index_project(&mut c, &pa).unwrap();
    let covered = |c: &Connection| {
        for p in [pa.id, pb.id] {
            let rs = db::ranges(c, &[p]).unwrap();
            let mut q = c.prepare("SELECT id FROM node WHERE project_id = ?1").unwrap();
            for id in q.query_map([p], |r| r.get::<_, i64>(0)).unwrap().flatten() {
                assert!(rs.iter().any(|(lo, hi)| (*lo..=*hi).contains(&id)), "node {id} of project {p} outside {rs:?}");
            }
        }
    };
    covered(&c);
    // The newest project row is deleted, and another project writes next.
    let top: i64 = c.query_row("SELECT MAX(id) FROM node WHERE project_id != 0", [], |r| r.get(0)).unwrap();
    let owner: i64 = c.query_row("SELECT project_id FROM node WHERE id = ?1", [top], |r| r.get(0)).unwrap();
    let other = if owner == pa.id { pb.id } else { pa.id };
    c.execute("DELETE FROM node WHERE id = ?1", [top]).unwrap();
    c.execute(
        "INSERT INTO node(project_id, kind, key, name, source, updated_at) VALUES (?1, 'symbol', 'reused', 'reused', 'test', 0)",
        [other],
    )
    .unwrap();
    covered(&c);
    // Rows written without a sync (memory notes, an indexer still running) are covered too.
    c.execute(
        "INSERT INTO node(project_id, kind, key, name, source, updated_at) VALUES (?1, 'symbol', 'unsynced', 'unsynced', 'test', 0)",
        [owner],
    )
    .unwrap();
    covered(&c);
    db::sync_ranges(&c).unwrap();
    covered(&c);
    let before: i64 = c.query_row("SELECT COUNT(*) FROM id_range", [], |r| r.get(0)).unwrap();
    db::compact_ranges(&c).unwrap();
    covered(&c);
    let after: i64 = c.query_row("SELECT COUNT(*) FROM id_range", [], |r| r.get(0)).unwrap();
    assert!(after <= before, "{after} > {before}");
}

fn reindex(c: &mut Connection, p: &project::Project) {
    inventory::index_project(c, p).unwrap();
    tsindex::index_project(c, p).unwrap();
}

#[test]
fn project_scope_survives_many_interleaved_saves() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let small = tempfile::tempdir().unwrap();
    let busy = tempfile::tempdir().unwrap();
    write(small.path(), "csi/handlers.rs", "pub fn csi_driver_attach_volume_handler() {}\n");
    for f in 0..6 {
        let body: String = (0..300).map(|i| format!("pub fn driver{f}x{i}() {{}}\n")).collect();
        write(busy.path(), &format!("drivers/d{f}.rs"), &body);
    }
    let mut c = db::open_rw().unwrap();
    let (sp, _) = index(&mut c, small.path());
    let (bp, _) = index(&mut c, busy.path());
    let o = query::FindOpts { kind: "symbol", all: false, budget: 2000, limit: 20, offset: 0 };
    // Each project saves in turn: every save leaves a range the other's next save separates.
    for i in 0..160 {
        let (p, root) = if i % 2 == 0 { (&sp, small.path()) } else { (&bp, busy.path()) };
        write(root, "src/edit.rs", &format!("pub fn edit_{i}() {{}}\n"));
        reindex(&mut c, p);
    }
    let rs = db::ranges(&c, &[sp.id]).expect("ranges gave up on a fragmented project");
    assert!(rs.len() > 64, "{} ranges: not fragmented enough to test", rs.len());
    // A few slices still hold the project, and they skip the busy neighbour.
    let cov = db::cover(&rs, 8);
    assert!(cov.len() <= 8 && db::span(&cov) * 2 < db::max_id(&c), "{cov:?}");
    let out = query::find(&c, Some(&sp), "driver", &o).unwrap();
    assert!(out.contains("csi_driver_attach_volume_handler"), "{out}");
}

#[test]
fn cover_bounds_the_range_count_and_keeps_every_id() {
    let rs: Vec<(i64, i64)> = (0..200).map(|i| (i * 10, i * 10 + 3)).chain([(100_000, 100_500)]).collect();
    let cov = db::cover(&rs, 8);
    assert!(cov.len() <= 8, "{cov:?}");
    for (lo, hi) in &rs {
        assert!(cov.iter().any(|(a, b)| a <= lo && hi <= b), "{lo}-{hi} not covered by {cov:?}");
    }
    // The widest gap (to the late block) stays a gap.
    assert!(!cov.iter().any(|(a, b)| *a <= 5000 && 99_000 <= *b), "{cov:?}");
}

#[test]
fn a_majority_project_keeps_hits_a_neighbour_dominates() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let big = tempfile::tempdir().unwrap();
    let busy = tempfile::tempdir().unwrap();
    write(big.path(), "src/driver/attach.rs", "pub fn attach_volume() {}\n");
    for f in 0..8 {
        let body: String = (0..500).map(|i| format!("pub fn item{f}x{i}() {{}}\n")).collect();
        write(big.path(), &format!("src/items/i{f}.rs"), &body);
    }
    for f in 0..5 {
        let body: String = (0..300).map(|i| format!("pub fn driver_{f}_{i}() {{}}\n")).collect();
        write(busy.path(), &format!("src/d{f}.rs"), &body);
    }
    let mut c = db::open_rw().unwrap();
    let (bp, _) = index(&mut c, big.path());
    index(&mut c, busy.path());
    let o = query::FindOpts { kind: "symbol", all: false, budget: 2000, limit: 20, offset: 0 };
    let out = query::find(&c, Some(&bp), "driver", &o).unwrap();
    assert!(out.contains("attach_volume"), "{out}");
}

#[test]
fn a_minority_project_keeps_prefix_matches_in_body_text() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let small = tempfile::tempdir().unwrap();
    let busy = tempfile::tempdir().unwrap();
    write(small.path(), "notes/setup.md", "# Setup\n\nThe reconfiguration step runs once per configuration change.\n");
    for f in 0..6 {
        let body: String = (0..300).map(|i| format!("pub fn config_value{f}x{i}() {{}}\n")).collect();
        write(busy.path(), &format!("src/c{f}.rs"), &body);
    }
    let mut c = db::open_rw().unwrap();
    let (sp, _) = index(&mut c, small.path());
    index(&mut c, busy.path());
    let o = query::FindOpts { kind: "any", all: false, budget: 2000, limit: 20, offset: 0 };
    for q in ["config", "configur"] {
        let out = query::find(&c, Some(&sp), q, &o).unwrap();
        assert!(out.contains("notes/setup.md"), "{q}: {out}");
    }
}

#[test]
fn the_id_range_migration_covers_rows_a_concurrent_writer_adds() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    {
        let c = db::open_rw().unwrap();
        c.execute_batch(
            "INSERT INTO project(id, name, root, created_at) VALUES (2, 'two', '/nowhere/two', 0), (3, 'three', '/nowhere/three', 0);
             WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < 40000)
             INSERT INTO node(project_id, kind, key, name, source, updated_at) SELECT 2 + (i % 2), 'symbol', 'k' || i, 'n' || i, 'test', 0 FROM s;
             DELETE FROM id_range; PRAGMA user_version = 10;",
        )
        .unwrap();
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let stop = stop.clone();
        let path = db::db_path();
        std::thread::spawn(move || {
            let w = Connection::open(path).unwrap();
            w.busy_timeout(std::time::Duration::from_secs(10)).unwrap();
            let mut i = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                w.execute(
                    "INSERT INTO node(project_id, kind, key, name, source, updated_at) VALUES (3, 'symbol', 'w' || ?1, 'w', 'test', 0)",
                    [i],
                )
                .unwrap();
                i += 1;
            }
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(20));
    let c = db::open_rw().unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
    let rs = db::ranges(&c, &[3]).unwrap();
    let mut q = c.prepare("SELECT id FROM node WHERE project_id = 3").unwrap();
    let missing =
        q.query_map([], |r| r.get::<_, i64>(0)).unwrap().flatten().filter(|id| !rs.iter().any(|(lo, hi)| (*lo..=*hi).contains(id))).count();
    assert_eq!(missing, 0);
}

#[test]
fn upgraded_stores_label_existing_guessed_calls_on_the_next_index() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let repo = tempfile::tempdir().unwrap();
    go_repo(repo.path());
    let p = {
        let mut c = db::open_rw().unwrap();
        let (p, _) = index(&mut c, repo.path());
        // A store indexed before the ambiguity column: every reference reads as certain.
        c.execute_batch("UPDATE ts_ref SET amb = 0; PRAGMA user_version = 11;").unwrap();
        p
    };
    let mut c = db::open_rw().unwrap();
    let st = tsindex::index_project(&mut c, &p).unwrap();
    assert_eq!(st.parsed, 0);
    let amb: i64 = c.query_row("SELECT amb FROM ts_ref WHERE path = 'pkg/a/z.go' AND name = 'Errorf'", [], |r| r.get(0)).unwrap();
    assert_eq!(amb, 2);
    // Once only: the next run has nothing to re-score.
    assert_eq!(tsindex::index_project(&mut c, &p).unwrap().rescored, 0);
}

#[test]
fn callees_and_impact_mark_name_guessed_edges() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let repo = tempfile::tempdir().unwrap();
    go_repo(repo.path());
    write(repo.path(), "pkg/a/top.go", "package a\n\nfunc Top() {\n\tUse(nil)\n}\n");
    let mut c = db::open_rw().unwrap();
    let (p, _) = index(&mut c, repo.path());
    let ro = db::open_ro().unwrap();
    let target: String = ro
        .query_row("SELECT d.path FROM ts_ref r JOIN node d ON d.id = r.dst WHERE r.path = 'pkg/a/z.go' AND r.name = 'Errorf'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let parent = if target.ends_with("x.go") { "T" } else { "U" };
    let run = |sym: &str, rel: &'static str, depth: usize| {
        code::run(&ro, &p, sym, &code::CodeOpts { rel, depth, budget: 2000, cursor: 0, fallback: false }).unwrap()
    };
    let out = run("Use", "callees", 1);
    let errorf = out.lines().find(|l| l.contains("Errorf")).unwrap_or_default();
    assert!(errorf.contains("guess 1 of 2"), "{out}");
    let helper = out.lines().find(|l| l.contains("Helper")).unwrap_or_default();
    assert!(!helper.contains("guess"), "{out}");
    assert!(out.contains("1 guessed by name"), "{out}");
    let out = run(&format!("{parent}.Errorf"), "impact", 2);
    let use_row = out.lines().find(|l| l.contains(" Use ")).unwrap_or_default();
    assert!(use_row.contains("guess 1 of 2"), "{out}");
    let top_row = out.lines().find(|l| l.contains(" Top ")).unwrap_or_default();
    assert!(top_row.contains("guess"), "{out}");
}

#[test]
fn warm_search_never_waits_for_a_graph_rebuild() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let a = tempfile::tempdir().unwrap();
    write(a.path(), "src/lib.rs", "pub fn alpha() { beta() }\npub fn beta() {}\n");
    let mut c = db::open_rw().unwrap();
    let (pa, _) = index(&mut c, a.path());
    let o = query::FindOpts { kind: "any", all: false, budget: 800, limit: 10, offset: 0 };
    let mut warm = search::Warm::default();
    // A sentence query has no graph arm: no graph is built for it.
    search::find_warm(&mut warm, &c, Some(&pa), "memory barrier", &o).unwrap();
    warm.settle();
    assert_eq!(warm.graph_loads(), 0);
    // An identifier query answers at once (the SQL walk, same answer) while the graph builds.
    let out = search::find_warm(&mut warm, &c, Some(&pa), "alpha", &o).unwrap();
    assert_eq!(out, search::find(&c, Some(&pa), "alpha", &o).unwrap());
    warm.settle();
    assert_eq!(warm.graph_loads(), 1);
    // After a save the stale graph is replaced in the background, and answers stay the same.
    write(a.path(), "src/more.rs", "pub fn gamma() { alpha() }\n");
    reindex(&mut c, &pa);
    let out = search::find_warm(&mut warm, &c, Some(&pa), "alpha", &o).unwrap();
    assert_eq!(out, search::find(&c, Some(&pa), "alpha", &o).unwrap());
    assert!(out.contains("gamma"), "{out}");
    warm.settle();
    assert_eq!(warm.graph_loads(), 2);
    let out = search::find_warm(&mut warm, &c, Some(&pa), "beta", &o).unwrap();
    assert_eq!(out, search::find(&c, Some(&pa), "beta", &o).unwrap());
}
