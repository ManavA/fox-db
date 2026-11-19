//! M2/M3 tests: Swift graph import (synthetic index-store data), `fox code` relations, staleness,
//! MCP tool surface and budgets, tree-sitter outline fallback, watcher classification.

use foxdb::indexstore::*;
use foxdb::swiftimport::{RecData, StoreData, UnitData};
use foxdb::{code, db, mcp, project, query, swiftimport, watch};
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn sym(usr: &str, name: &str, kind: i32) -> SymInfo {
    SymInfo { usr: usr.into(), name: name.into(), kind, subkind: 0, lang: 100 }
}
fn occ(sym: u32, roles: u64, line: u32, rel_roles: u64, rel_sym: u32) -> OccRow {
    OccRow { sym, roles, line, col: 1, rel_roles, rel_sym }
}

// symbol indices
const SHAPE: u32 = 0;
const SHAPE_AREA: u32 = 1;
const CIRCLE: u32 = 2;
const HASHABLE: u32 = 3;
const EXT: u32 = 4;
const CIRCLE_AREA: u32 = 5;
const RENDER: u32 = 6;
const LOCAL_PARAM: u32 = 7;
const GETTER: u32 = 8;
const CIRCLE_RADIUS: u32 = 9;

fn syms() -> Vec<SymInfo> {
    let mut g = sym("s:g", "getter:radius", 18);
    g.subkind = 3;
    vec![
        sym("s:Shape", "Shape", 8),
        sym("s:Shape.area", "area()", 16),
        sym("s:Circle", "Circle", 6),
        sym("s:Hashable", "Hashable", 8),
        sym("s:ext", "Circle", 9),
        sym("s:Circle.area", "area()", 16),
        sym("s:render", "render(_:)", 12),
        sym("s:param", "c", 25),
        g,
        sym("s:Circle.radius", "radius", 19),
    ]
}

const DEF: u64 = R_DEF;
const REF: u64 = R_REF;
const CALL: u64 = R_CALL | R_REF;

fn rec_a() -> RecData {
    RecData {
        name: "A.swift-1".into(),
        path: "Sources/A.swift".into(),
        occs: vec![
            occ(SHAPE, DEF, 1, 0, NONE),
            occ(SHAPE_AREA, DEF | REL_CHILD_OF, 2, REL_CHILD_OF, SHAPE),
            occ(CIRCLE, DEF, 4, 0, NONE),
            occ(HASHABLE, REF | REL_BASE_OF, 4, REL_BASE_OF, CIRCLE),
            occ(EXT, DEF, 10, 0, NONE),
            occ(CIRCLE, REF | REL_EXTENDED_BY, 10, REL_EXTENDED_BY, EXT),
            occ(SHAPE, REF | REL_BASE_OF, 10, REL_BASE_OF, EXT),
            occ(CIRCLE_AREA, DEF | REL_CHILD_OF, 11, REL_CHILD_OF, EXT),
            occ(CIRCLE_AREA, DEF | REL_OVERRIDE_OF, 11, REL_OVERRIDE_OF, SHAPE_AREA),
            occ(CIRCLE_RADIUS, DEF | REL_CHILD_OF, 12, REL_CHILD_OF, EXT),
            occ(GETTER, DEF | REL_ACCESSOR_OF, 12, REL_ACCESSOR_OF, CIRCLE_RADIUS),
        ],
    }
}
fn rec_b(extra_call: bool) -> RecData {
    let mut occs = vec![
        occ(RENDER, DEF, 1, 0, NONE),
        occ(LOCAL_PARAM, DEF | REL_CHILD_OF, 1, REL_CHILD_OF, RENDER),
        occ(CIRCLE_AREA, CALL | REL_CALLED_BY | REL_CONTAINED_BY, 5, REL_CALLED_BY | REL_CONTAINED_BY, RENDER),
        // a property read inside the render body, via its getter accessor
        occ(CIRCLE_RADIUS, REF, 7, REL_CONTAINED_BY, RENDER),
    ];
    if extra_call {
        occs.push(occ(SHAPE_AREA, CALL | REL_CALLED_BY | REL_CONTAINED_BY, 6, REL_CALLED_BY | REL_CONTAINED_BY, RENDER));
    }
    RecData { name: if extra_call { "B.swift-1".into() } else { "B.swift-2".into() }, path: "Sources/B.swift".into(), occs }
}

fn store(units_mtime: i64, b_extra: bool, unit: &str) -> StoreData {
    let a = rec_a();
    let b = rec_b(b_extra);
    let recs = vec![(a.name.clone(), a.path.clone()), (b.name.clone(), b.path.clone())];
    StoreData {
        store: "/fake/DataStore".into(),
        syms: syms(),
        units: vec![UnitData { unit: unit.into(), mtime: units_mtime, module: "AppTV".into(), recs }],
        new_recs: vec![a, b],
        present: HashSet::from([unit.to_string()]),
    }
}

struct Env {
    _home: tempfile::TempDir,
    repo: tempfile::TempDir,
    c: rusqlite::Connection,
    p: project::Project,
}

fn setup() -> Env {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("none"));
    }
    std::fs::create_dir_all(repo.path().join("Sources")).unwrap();
    std::fs::write(repo.path().join("Sources/A.swift"), "protocol Shape {\n    func area() -> Double\n}\n\nstruct Circle: Hashable {\n    let radius: Double\n}\n\n\nextension Circle: Shape {\n    func area() -> Double { 3 * radius }\n    var r2: Double { radius }\n}\n").unwrap();
    std::fs::write(repo.path().join("Sources/B.swift"), "func render(_ c: Circle) {\n    // one\n    // two\n    // three\n    _ = c.area()\n    _ = (c as Shape).area()\n    _ = c.radius\n}\n").unwrap();
    let c = db::open_rw().unwrap();
    let loc = project::locate(repo.path());
    let p = project::ensure(&c, &loc).unwrap();
    Env { _home: home, repo, c, p }
}

fn far_future() -> i64 {
    db::now() * 1000 + 3_600_000
}

fn graph(e: &Env, sym: &str, rel: &str) -> String {
    let o = code::CodeOpts { rel, depth: 2, budget: 800, cursor: 0, fallback: false };
    code::run(&e.c, &e.p, sym, &o).unwrap()
}

#[test]
fn import_builds_symbols_and_all_edge_kinds() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = setup();
    let st =
        swiftimport::apply(&mut e.c, &e.p, &[store(far_future(), true, "u1")], &HashSet::from(["/fake/DataStore".to_string()])).unwrap();
    assert!(st.symbols >= 7, "{st:?}");
    // Locals (params) and accessors are not symbols.
    let n: i64 =
        e.c.query_row("SELECT COUNT(*) FROM node WHERE kind='symbol' AND name IN ('c','getter:radius')", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
    let kinds: Vec<String> = {
        let mut q = e.c.prepare("SELECT DISTINCT kind FROM edge WHERE source='indexstore' ORDER BY 1").unwrap();
        q.query_map([], |r| r.get(0)).unwrap().flatten().collect()
    };
    for k in ["CALLS", "CONFORMS_TO", "EXTENDS", "OVERRIDES", "REFERENCES"] {
        assert!(kinds.iter().any(|x| x == k), "missing {k}: {kinds:?}");
    }
    // Circle: Hashable (external) is an INHERITS/CONFORMS_TO edge to an external node.
    let ext: i64 =
        e.c.query_row(
            "SELECT COUNT(*) FROM edge e JOIN node d ON d.id=e.dst WHERE e.kind='CONFORMS_TO' AND d.name='Hashable' AND d.path IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ext, 1);
}

#[test]
fn callers_cross_file_via_protocol_and_conformers_through_extension() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = setup();
    swiftimport::apply(&mut e.c, &e.p, &[store(far_future(), true, "u1")], &HashSet::from(["/fake/DataStore".to_string()])).unwrap();
    let out = graph(&e, "Circle.area", "callers");
    assert!(out.contains("Sources/B.swift"), "{out}");
    assert!(out.contains(":5 render"), "direct call missing: {out}");
    assert!(out.contains("  :6 ") && out.contains("via Shape.area"), "call through the protocol requirement missing: {out}");
    // Conformance declared in an extension is attributed to the type.
    let out = graph(&e, "Shape", "conforms");
    assert!(out.contains("struct Circle"), "{out}");
    assert!(!out.contains("(external)"), "{out}");
    let out = graph(&e, "Circle", "extensions");
    assert!(out.contains("1 extensions"), "{out}");
    let out = graph(&e, "Circle.area", "overrides");
    assert!(out.contains("overrides / witnesses") && out.contains("Shape.area"), "{out}");
    let out = graph(&e, "Circle.area", "callees");
    assert!(out.contains("0 callees"), "{out}");
    let out = graph(&e, "render", "callees");
    assert!(out.contains("Circle.area"), "{out}");
    // Property reads count as references, attributed to the enclosing function.
    let out = graph(&e, "Circle.radius", "refs");
    assert!(out.contains(":7 render"), "{out}");
    // Impact: render is affected by changing Circle.area.
    let out = graph(&e, "Circle.area", "impact");
    assert!(out.contains("render"), "{out}");
}

#[test]
fn incremental_reimport_replaces_changed_records_and_gc_old_ones() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = setup();
    let reached = HashSet::from(["/fake/DataStore".to_string()]);
    swiftimport::apply(&mut e.c, &e.p, &[store(far_future(), true, "u1")], &reached).unwrap();
    assert!(graph(&e, "Shape.area", "callers").contains("  :6 "));
    // New build of B without the protocol call: new record name, same unit.
    let mut s2 = store(far_future() + 1, false, "u1");
    s2.new_recs.remove(0); // A unchanged: record already stored, not re-read
    s2.units[0].recs[0] = ("A.swift-1".into(), "Sources/A.swift".into());
    s2.units[0].recs[1] = ("B.swift-2".into(), "Sources/B.swift".into());
    swiftimport::apply(&mut e.c, &e.p, &[s2], &reached).unwrap();
    assert!(!graph(&e, "Shape.area", "callers").contains("  :6 "), "stale record not replaced");
    assert!(graph(&e, "Circle.area", "callers").contains(":5 render"));
    let recs: i64 = e.c.query_row("SELECT COUNT(*) FROM swift_rec", [], |r| r.get(0)).unwrap();
    assert_eq!(recs, 2, "old record garbage-collected");
    // A vanished unit removes everything it contributed.
    swiftimport::apply(
        &mut e.c,
        &e.p,
        &[StoreData { store: "/fake/DataStore".into(), syms: vec![], units: vec![], new_recs: vec![], present: HashSet::new() }],
        &reached,
    )
    .unwrap();
    let n: i64 = e.c.query_row("SELECT COUNT(*) FROM node WHERE kind='symbol'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn symbols_go_stale_when_the_file_changes_after_its_unit() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = setup();
    let reached = HashSet::from(["/fake/DataStore".to_string()]);
    swiftimport::apply(&mut e.c, &e.p, &[store(far_future(), true, "u1")], &reached).unwrap();
    assert_eq!(swiftimport::refresh_stale(&e.c, &e.p).unwrap(), 0);
    assert!(!graph(&e, "Circle.area", "callers").contains('~'));
    // Unit written long ago: both files are newer than it, so symbols are stale and rows are marked `~`.
    swiftimport::apply(&mut e.c, &e.p, &[store(1000, true, "u1")], &reached).unwrap();
    assert_eq!(swiftimport::refresh_stale(&e.c, &e.p).unwrap(), 2);
    let out = graph(&e, "Circle.area", "callers");
    assert!(out.contains("~stale") && out.contains("~render"), "{out}");
    let stale: i64 = e.c.query_row("SELECT COUNT(*) FROM node WHERE kind='symbol' AND stale=1", [], |r| r.get(0)).unwrap();
    assert!(stale >= 5);
    // `src` of a stale symbol is located by name even when lines drifted.
    std::fs::write(
        e.repo.path().join("Sources/A.swift"),
        format!("// added\n// lines\n{}", std::fs::read_to_string(e.repo.path().join("Sources/A.swift")).unwrap()),
    )
    .unwrap();
    let out = graph(&e, "Circle.area", "src");
    assert!(out.contains("func area() -> Double { 3 * radius }"), "{out}");
    assert!(out.contains("located by name"), "{out}");
}

#[test]
fn src_outline_and_paging() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = setup();
    swiftimport::apply(&mut e.c, &e.p, &[store(far_future(), true, "u1")], &HashSet::from(["/fake/DataStore".to_string()])).unwrap();
    let out = graph(&e, "Shape", "src");
    assert!(out.contains("1\tprotocol Shape {") && out.contains("3\t}"), "{out}");
    let out = graph(&e, "Sources/A.swift", "outline");
    assert!(out.contains("outline Sources/A.swift") && out.contains("protocol Shape"), "{out}");
    let out = graph(&e, "Circle", "outline");
    assert!(out.contains("area") && out.contains("radius"), "members via extension: {out}");
    // Paging: tiny budget truncates with a cursor, and the cursor continues.
    let o = code::CodeOpts { rel: "callers", depth: 1, budget: 32, cursor: 0, fallback: false };
    let first = code::run(&e.c, &e.p, "Circle.area", &o).unwrap();
    assert!(first.contains("more (answer cut) \u{b7} cursor="), "{first}");
    let cur: usize = first.rsplit("cursor=").next().unwrap().trim().parse().unwrap();
    let o2 = code::CodeOpts { cursor: cur, ..o };
    let second = code::run(&e.c, &e.p, "Circle.area", &o2).unwrap();
    assert_ne!(first, second);
}

#[test]
fn find_returns_symbol_rows_with_handles() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = setup();
    swiftimport::apply(&mut e.c, &e.p, &[store(far_future(), true, "u1")], &HashSet::from(["/fake/DataStore".to_string()])).unwrap();
    let o = query::FindOpts { kind: "symbol", all: false, budget: 400, limit: 10, offset: 0 };
    let out = query::find(&e.c, Some(&e.p), "Circle", &o).unwrap();
    assert!(out.lines().any(|l| l.starts_with("s:") && l.contains("struct Circle") && l.contains("Sources/A.swift:4")), "{out}");
    assert!(!out.contains("Hashable"), "externals are not search results: {out}");
}

#[test]
fn mcp_tool_definitions_fit_the_token_budget() {
    let text = serde_json::to_string(&mcp::tools()).unwrap();
    // 4 bytes per token is the budgeter's convention; also check the stricter 3 bytes/token.
    assert!(text.len() / 4 <= 1500, "tool definitions are ~{} tokens", text.len() / 4);
    assert!(text.len() / 3 <= 1500, "tool definitions exceed 1500 tokens at 3 bytes/token: {} bytes", text.len());
    assert_eq!(mcp::tools().as_array().unwrap().len(), 4);
    let tools = mcp::tools();
    let names: Vec<&str> = tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["fox_find", "fox_graph", "fox_read", "fox_memory"]);
}

#[test]
fn mcp_protocol_budgets_cursor_and_notes() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = setup();
    swiftimport::apply(&mut e.c, &e.p, &[store(far_future(), true, "u1")], &HashSet::from(["/fake/DataStore".to_string()])).unwrap();
    std::env::set_current_dir(e.repo.path()).unwrap();
    let init = mcp::handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "foxdb");
    assert!(mcp::handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
    let call = |name: &str, args: serde_json::Value| -> (String, bool) {
        let r = mcp::handle(&json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":name,"arguments":args}})).unwrap();
        (r["result"]["content"][0]["text"].as_str().unwrap().to_string(), r["result"]["isError"].as_bool().unwrap())
    };
    // Every response fits its budget (4 bytes/token) plus the footer line.
    for (tool, args) in [
        ("fox_graph", json!({"target":"Circle.area","rel":"callers","budget":32})),
        ("fox_graph", json!({"target":"Shape","rel":"src","budget":40})),
        ("fox_find", json!({"q":"Circle","budget":32})),
        ("fox_read", json!({"target":"Shape","mode":"body","budget":32})),
    ] {
        let (t, _) = call(tool, args.clone());
        assert!(t.len() <= 32 * 4 + 40 || args["budget"] == 40, "{tool} {args} -> {} bytes\n{t}", t.len());
    }
    let (t, err) = call("fox_graph", json!({"target":"Circle.area","rel":"callers"}));
    assert!(!err && t.contains("Sources/B.swift"), "{t}");
    let (t, err) = call("fox_graph", json!({"target":"Circle.area","rel":"nonsense"}));
    assert!(err && t.contains("unknown --rel"), "{t}");
    let (t, _) = call("fox_find", json!({"q":"Circle","kind":"symbol","budget":2}));
    assert!(t.contains("cursor=") || t.contains("ids only to fit the budget"), "tiny budget must still say what it left out: {t}");
    // Handles round trip between tools.
    let (t, _) = call("fox_find", json!({"q":"render","kind":"symbol"}));
    let h = t.split_whitespace().next().unwrap().trim_start_matches('~').to_string();
    let (t, err) = call("fox_graph", json!({"target": h, "rel":"callees"}));
    assert!(!err && t.contains("Circle.area"), "{t}");
    let (t, err) = call("fox_read", json!({"target": h, "mode":"body"}));
    assert!(!err && t.contains("func render"), "{t}");
    // memory: note (with redaction) then recall.
    let (t, err) = call("fox_memory", json!({"op":"note","text":"decided to cache Shape lookups; token=abcdef1234567890SECRET"}));
    assert!(!err && t.contains("secrets redacted"), "{t}");
    let (t, _) = call("fox_memory", json!({"op":"recall","q":"cache Shape lookups"}));
    assert!(t.contains("cache Shape") && !t.contains("SECRET"), "{t}");
    assert!(!call("fox_memory", json!({"op":"recall","q":"zzzqq"})).1);
    assert!(call("nope", json!({})).1);
}

#[test]
fn watcher_classifies_store_source_and_ignored_paths() {
    let root = Path::new("/r");
    let stores = vec![std::path::PathBuf::from("/r/.DerivedData/Index.noindex/DataStore")];
    let mut d = watch::Dirty::default();
    watch::classify(root, &stores, Path::new("/r/.DerivedData/Index.noindex/DataStore/v5/units/Foo.o-ABC"), &mut d);
    assert_eq!(d, watch::Dirty { store: true, swift_src: false, other: false, ts_src: false });
    let mut d = watch::Dirty::default();
    watch::classify(root, &stores, Path::new("/r/Sources/A.swift"), &mut d);
    assert_eq!(d, watch::Dirty { store: false, swift_src: true, other: true, ts_src: false });
    let mut d = watch::Dirty::default();
    watch::classify(root, &stores, Path::new("/r/internal/api/server.go"), &mut d);
    assert_eq!(d, watch::Dirty { store: false, swift_src: false, other: true, ts_src: true });
    for ignored in ["/r/.git/index", "/r/.agent-review/x/a.swift", "/r/Config/Local.xcconfig", "/r/.env", "/r/Sources/AuthKey_X.p8"] {
        let mut d = watch::Dirty::default();
        watch::classify(root, &stores, Path::new(ignored), &mut d);
        assert_eq!(d, watch::Dirty::default(), "{ignored}");
    }
}

#[test]
fn watcher_sees_a_real_file_event() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let p = project::Project { id: 1, name: "t".into(), root: root.clone(), kind: "dir".into(), indexed_at: None };
    let (h, stores) = watch::start(&p).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    std::fs::write(root.join("X.swift"), "struct X {}\n").unwrap();
    let d = watch::next_batch(&h, &root, &stores, std::time::Duration::from_millis(300), std::time::Duration::from_secs(10));
    assert!(d.is_some_and(|d| d.swift_src), "FSEvents did not report the new file");
}

#[test]
fn tree_sitter_style_outline_for_kotlin_and_python() {
    let dir = tempfile::tempdir().unwrap();
    let kt = dir.path().join("A.kt");
    std::fs::write(&kt, "package a\n\nclass Foo {\n    fun bar(x: Int) = x\n    private val z = 1\n}\n\nobject Baz\n").unwrap();
    let out = foxdb::codanna::outline_text(&kt, "A.kt");
    assert!(
        out.iter().any(|l| l.contains("class Foo"))
            && out.iter().any(|l| l.contains("fun bar"))
            && out.iter().any(|l| l.contains("object Baz")),
        "{out:?}"
    );
    let py = dir.path().join("a.py");
    std::fs::write(&py, "import os\n\ndef top():\n    pass\n\nclass K:\n    def m(self):\n        pass\n").unwrap();
    let out = foxdb::codanna::outline_text(&py, "a.py");
    assert!(
        out.iter().any(|l| l.contains("def top")) && out.iter().any(|l| l.contains("def m")) && out.iter().any(|l| l.contains("class K")),
        "{out:?}"
    );
}

/// Reads the real Physical/Harness stores when present (read-only); proves the dlopen FFI end to end.
#[test]
fn real_index_store_reads_through_libindexstore() {
    // Opt-in: FOXDB_TEST_SWIFT_REPO=<repo root containing .DerivedDataInstalledAppHarness>.
    let Some(repo) = std::env::var_os("FOXDB_TEST_SWIFT_REPO").map(std::path::PathBuf::from) else { return };
    let ds = repo.join(".DerivedDataInstalledAppHarness/Index.noindex/DataStore");
    let ds = ds.as_path();
    if !ds.join("v5").exists() {
        return;
    }
    let sd = swiftimport::read_store(ds, &repo, &Default::default(), &HashSet::new()).unwrap();
    assert!(sd.present.len() > 100 && !sd.new_recs.is_empty());
    assert!(sd.new_recs.iter().any(|r| r.path.starts_with("Vendor/") || r.path.starts_with("Sources/")));
    assert!(sd.new_recs.iter().all(|r| !r.path.starts_with(".DerivedData") && !r.path.starts_with(".claude/")));
    assert!(sd.new_recs.iter().flat_map(|r| &r.occs).any(|o| o.roles & R_CALL != 0 && o.rel_roles & REL_CALLED_BY != 0));
}

#[test]
fn watch_dirs_skip_ignored_and_artifact_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    for d in ["src/a", "node_modules/x", ".git/objects", "gen/out", ".DerivedDataX/Index.noindex"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(root.join(".gitignore"), "gen/\n").unwrap();
    let p = project::Project { id: 1, name: "t-watchdirs".into(), root: root.clone(), kind: "dir".into(), indexed_at: None };
    let got: HashSet<String> =
        watch::watch_dirs(&p).iter().map(|d| d.strip_prefix(&root).unwrap().to_string_lossy().into_owned()).collect();
    assert!(got.contains("") && got.contains("src") && got.contains("src/a"), "{got:?}");
    for bad in ["node_modules", "node_modules/x", ".git", ".git/objects", "gen", ".DerivedDataX"] {
        assert!(!got.contains(bad), "{bad} watched: {got:?}");
    }
}

#[test]
fn watcher_sees_an_event_in_a_subdirectory() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join("internal/api")).unwrap();
    let p = project::Project { id: 1, name: "t-sub".into(), root: root.clone(), kind: "dir".into(), indexed_at: None };
    let (h, stores) = watch::start(&p).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    std::fs::write(root.join("internal/api/x.go"), "package api\n").unwrap();
    let d = watch::next_batch(&h, &root, &stores, std::time::Duration::from_millis(200), std::time::Duration::from_secs(5)).expect("event");
    assert!(d.ts_src && d.other, "{d:?}");
}
