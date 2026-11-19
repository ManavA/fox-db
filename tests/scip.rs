//! Milestone 6, the precise tier: `fox scip import` reads a SCIP index the user's own build
//! produced, upgrades matching tree-sitter references to precise ones, falls back per file once a
//! file changes, is replaced by a re-import, shows in `fox status`, and lets `fox drift` follow a
//! moved symbol by its SCIP id. The index is built programmatically (no indexer is run).

use foxdb::scip::{self, Document, Index, Occurrence};
use foxdb::{code, db, docref, inventory, project, query, tsindex};
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::process::Command;
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

fn index(c: &mut Connection, root: &Path) -> project::Project {
    let p = project::ensure(c, &project::locate(root)).unwrap();
    inventory::index_project(c, &p).unwrap();
    tsindex::index_project(c, &p).unwrap();
    project::find(c, &project::locate(root)).unwrap().unwrap()
}

const X: &str = "package a\n\ntype T struct{}\n\nfunc (t *T) Errorf() {}\n\nfunc Helper() {}\n";
const Y: &str = "package a\n\ntype U struct{}\n\nfunc (u *U) Errorf() {}\n";
const Z: &str = "package a\n\nfunc Use(t *T) {\n\tt.Errorf()\n\tHelper()\n}\n";

fn sym(desc: &str) -> String {
    format!("scip-go gomod example.com/m v1 `example.com/m/pkg/a`/{desc}")
}

fn occ(line: i64, start: i64, end: i64, symbol: &str, def: bool) -> Occurrence {
    Occurrence { range: vec![line, start, end], symbol: symbol.to_string(), roles: if def { scip::DEFINITION } else { 0 } }
}

/// The index scip-go would write for the repo, with `t.Errorf()` resolved to `target` (T or U),
/// `Errorf` defined in `t_file` for T. Each document embeds the file's text as it is now.
fn go_index(root: &Path, target: &str, t_file: &str, t_line: i64) -> Index {
    let doc = |path: &str, occurrences: Vec<Occurrence>| Document {
        relative_path: path.to_string(),
        occurrences,
        text: fs::read_to_string(root.join(path)).ok(),
        position_encoding: 1,
    };
    Index {
        tool: "scip-go 0.1.10".into(),
        project_root: format!("file://{}", root.display()),
        documents: vec![
            doc(t_file, vec![occ(t_line, 12, 18, &sym("T#Errorf()."), true)]),
            doc("pkg/a/y.go", vec![occ(4, 12, 18, &sym("U#Errorf()."), true)]),
            doc("pkg/a/z.go", vec![occ(3, 3, 9, &sym(&format!("{target}#Errorf().")), false), occ(4, 1, 7, &sym("Helper()."), false)]),
        ],
    }
}

fn save_index(dir: &Path, ix: &Index) -> std::path::PathBuf {
    let f = dir.join("index.scip");
    fs::write(&f, scip::encode(ix)).unwrap();
    f
}

fn errorf_ref(c: &Connection) -> (String, i64) {
    c.query_row(
        "SELECT d.path, r.amb FROM ts_ref r JOIN node d ON d.id = r.dst WHERE r.path = 'pkg/a/z.go' AND r.name = 'Errorf'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

fn callers(p: &project::Project, sym: &str) -> String {
    let ro = db::open_ro().unwrap();
    code::run(&ro, p, sym, &code::CodeOpts { rel: "callers", depth: 1, budget: 800, cursor: 0, fallback: false }).unwrap()
}

#[test]
fn decode_reads_what_encode_wrote_and_descriptors_parse() {
    let ix = Index {
        tool: "rust-analyzer 0.3.2".into(),
        project_root: "file:///w/demo".into(),
        documents: vec![Document {
            relative_path: "src/lib.rs".into(),
            occurrences: vec![
                Occurrence { range: vec![3, 11, 16], symbol: "rust-analyzer cargo demo 0.1.0 util/Greeter#greet().".into(), roles: 1 },
                Occurrence { range: vec![9, 4, 10, 2], symbol: "local 7".into(), roles: 0 },
            ],
            text: Some("fn x() {}\n".into()),
            position_encoding: 1,
        }],
    };
    assert_eq!(scip::decode(&scip::encode(&ix)).unwrap(), ix);
    assert!(scip::decode(&[0x12, 0x40]).is_err(), "truncated input is an error, not a panic");
    assert_eq!(scip::descriptor_names("rust-analyzer cargo demo 0.1.0 util/Greeter#greet()."), ["util", "Greeter", "greet"]);
    assert_eq!(scip::descriptor_names("scip-java maven com.x 1.0 com/x/Repo#load(+1)."), ["com", "x", "Repo", "load"]);
    assert_eq!(
        scip::descriptor_names("scip-go gomod example.com/m v1 `example.com/m/pkg/a`/T#Errorf()."),
        ["example.com/m/pkg/a", "T", "Errorf"]
    );
    assert!(scip::descriptor_names("local 3").is_empty());
}

#[test]
fn import_upgrades_guesses_falls_back_per_file_and_is_replaced_by_a_reimport() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let repo = tempfile::tempdir().unwrap();
    let r = repo.path();
    let (x, y, z) = ("pkg/a/x.go", "pkg/a/y.go", "pkg/a/z.go");
    write(r, x, X);
    write(r, y, Y);
    write(r, z, Z);
    let mut c = db::open_rw().unwrap();
    let p = index(&mut c, r);
    // No import yet: no SCIP tables in the store, and `t.Errorf()` is a name guess.
    let tables: i64 = c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'scip%'", [], |r| r.get(0)).unwrap();
    assert_eq!(tables, 0);
    let (guessed, amb) = errorf_ref(&c);
    assert_eq!(amb, 2);
    let status_before = query::status(&c, Some(&p)).unwrap();
    assert!(!status_before.contains("scip"), "{status_before}");
    // SCIP says the call reaches the definition the name guess did not pick.
    let (want_file, want, other) = if guessed == x { (y, "U", "T") } else { (x, "T", "U") };
    let out_dir = tempfile::tempdir().unwrap();
    let f = save_index(out_dir.path(), &go_index(r, want, x, 4));
    let st = scip::import(&mut c, &p, &f, false).unwrap();
    assert_eq!((st.documents, st.fresh, st.stale, st.outside), (3, 3, 0, 0), "{st:?}");
    assert_eq!((st.definitions, st.references), (2, 2), "{st:?}");
    // The Errorf call is now precise; `Helper` has no SCIP definition, so it keeps its own target.
    assert_eq!((st.precise, st.upgraded), (1, 1), "{st:?}");
    assert_eq!(errorf_ref(&c), (want_file.to_string(), -1));
    let out = callers(&p, &format!("{want}.Errorf"));
    assert!(out.contains("Use c:") && !out.contains("guess"), "{out}");
    assert!(callers(&p, &format!("{other}.Errorf")).contains("0 call sites"));
    let status = query::status(&c, Some(&p)).unwrap();
    assert!(status.contains("scip: scip-go 0.1.10, imported") && status.contains("3 of 3 files fresh"), "{status}");
    // Incremental resolution keeps the precise answer, and matches a full re-resolution.
    tsindex::resolve(&c, p.id).unwrap();
    assert_eq!(errorf_ref(&c), (want_file.to_string(), -1));
    // The calling file changes: its references fall back to tree-sitter until a fresh import.
    write(r, z, &format!("{Z}// edited\n"));
    let p = index(&mut c, r);
    assert_eq!(errorf_ref(&c), (guessed.clone(), 2));
    assert!(query::status(&c, Some(&p)).unwrap().contains("2 of 3 files fresh"));
    // A re-import replaces the previous one (no duplicate rows) and covers the edited file again.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let f = save_index(out_dir.path(), &go_index(r, want, x, 4));
    scip::import(&mut c, &p, &f, false).unwrap();
    assert_eq!(errorf_ref(&c), (want_file.to_string(), -1));
    let occs: i64 = c.query_row("SELECT COUNT(*) FROM scip_occ WHERE project_id = ?1", [p.id], |r| r.get(0)).unwrap();
    assert_eq!(occs, 2);
    // The definition's file changes: references into it fall back too.
    write(r, want_file, &format!("{}// edited\n", if want == "T" { X } else { Y }));
    index(&mut c, r);
    assert_eq!(errorf_ref(&c), (guessed, 2));
}

#[test]
fn files_changed_before_the_import_are_ignored() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let repo = tempfile::tempdir().unwrap();
    let r = repo.path();
    write(r, "pkg/a/x.go", X);
    write(r, "pkg/a/y.go", Y);
    let mut c = db::open_rw().unwrap();
    let out_dir = tempfile::tempdir().unwrap();
    let mut ix = go_index(r, "U", "pkg/a/x.go", 4);
    // The index embeds z.go's text as it was when built; the file has changed since.
    ix.documents[2].text = Some(Z.replace("Use", "Old"));
    let f = save_index(out_dir.path(), &ix);
    write(r, "pkg/a/z.go", Z);
    let p = index(&mut c, r);
    let (guessed, _) = errorf_ref(&c);
    let st = scip::import(&mut c, &p, &f, false).unwrap();
    assert_eq!((st.fresh, st.stale, st.precise), (2, 1, 0), "{st:?}");
    assert_eq!(errorf_ref(&c), (guessed, 2));
    // A document outside the project is counted and skipped.
    ix.documents[2].relative_path = "../elsewhere/z.go".into();
    let f = save_index(out_dir.path(), &ix);
    assert_eq!(scip::import(&mut c, &p, &f, false).unwrap().outside, 1);
    // Not a SCIP file: an error, nothing written.
    fs::write(out_dir.path().join("bad.scip"), b"\xff\xff\xff").unwrap();
    assert!(scip::import(&mut c, &p, &out_dir.path().join("bad.scip"), false).is_err());
}

/// rust-analyzer, scip-java, scip-python, scip-typescript and scip-go leave the document text out
/// by default. Such an index proves nothing about the file it read: one written after an edit it
/// did not see (a build still running, a copied CI artifact, a touched file) must not turn a
/// tree-sitter guess into a wrong "precise" answer. Without text, documents are unverified and
/// ignored unless the user opts in to trusting modification times.
#[test]
fn documents_without_text_are_not_trusted_unless_opted_in() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let repo = tempfile::tempdir().unwrap();
    let r = repo.path();
    write(r, "pkg/a/x.go", X);
    write(r, "pkg/a/y.go", Y);
    // z.go now calls U.Errorf, but the index (written afterwards) still says T.Errorf.
    let z_now = Z.replace("t *T", "u *U").replace("t.Errorf", "u.Errorf");
    write(r, "pkg/a/z.go", &z_now);
    let mut c = db::open_rw().unwrap();
    let p = index(&mut c, r);
    let before = errorf_ref(&c);
    let mut ix = go_index(r, "T", "pkg/a/x.go", 4);
    for d in &mut ix.documents {
        d.text = None;
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
    let out_dir = tempfile::tempdir().unwrap();
    let f = save_index(out_dir.path(), &ix);
    let st = scip::import(&mut c, &p, &f, false).unwrap();
    assert_eq!((st.fresh, st.stale, st.unverified, st.precise), (0, 0, 3, 0), "{st:?}");
    assert_eq!(errorf_ref(&c), before, "an unverifiable document keeps the tree-sitter answer");
    let status = query::status(&c, Some(&p)).unwrap();
    assert!(status.contains("0 of 3 files fresh"), "{status}");
    // Opting in trusts files not modified after the index file (the documented risk).
    let st = scip::import(&mut c, &p, &f, true).unwrap();
    assert_eq!((st.fresh, st.unverified, st.precise), (3, 0, 1), "{st:?}");
    // A file modified after the index file stays stale even when opted in.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(r, "pkg/a/z.go", &format!("{z_now}// edited\n"));
    let p = index(&mut c, r);
    let st = scip::import(&mut c, &p, &f, true).unwrap();
    assert_eq!((st.fresh, st.stale, st.precise), (2, 1, 0), "{st:?}");
    assert_eq!(errorf_ref(&c), before);
}

fn git(root: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn drift_follows_a_moved_symbol_by_its_scip_id() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let repo = tempfile::tempdir().unwrap();
    let r = repo.path();
    write(r, "pkg/a/x.go", X);
    write(r, "pkg/a/y.go", Y);
    write(r, "pkg/a/z.go", Z);
    write(r, "docs/design.md", "# Design\n\nErrors go through `T.Errorf`.\n");
    git(r, &["init", "-q"]);
    git(r, &["add", "."]);
    git(r, &["commit", "-q", "-m", "doc"]);
    let mut c = db::open_rw().unwrap();
    let p = index(&mut c, r);
    let out_dir = tempfile::tempdir().unwrap();
    scip::import(&mut c, &p, &save_index(out_dir.path(), &go_index(r, "T", "pkg/a/x.go", 4)), false).unwrap();
    // T.Errorf moves to its own file.
    write(r, "pkg/a/x.go", "package a\n\ntype T struct{}\n\nfunc Helper() {}\n");
    write(r, "pkg/a/w.go", "package a\n\nfunc (t *T) Errorf() {}\n");
    git(r, &["add", "."]);
    git(r, &["commit", "-q", "-m", "move"]);
    let p = index(&mut c, r);
    let before = docref::drift(&c, &p, "docs/design.md").unwrap();
    assert!(before.contains("1 changed, 0 moved"), "without a fresh import it reads as an edit: {before}");
    std::thread::sleep(std::time::Duration::from_millis(20));
    scip::import(&mut c, &p, &save_index(out_dir.path(), &go_index(r, "T", "pkg/a/w.go", 2)), false).unwrap();
    let after = docref::drift(&c, &p, "docs/design.md").unwrap();
    assert!(after.contains("0 changed, 1 moved"), "{after}");
    assert!(after.contains("moved    `T.Errorf` -> pkg/a/w.go:3 from pkg/a/x.go (SCIP scip-go"), "{after}");
}

/// A protobuf length-delimited field and a varint field, for occurrences `encode` cannot write.
fn pb_bytes(out: &mut Vec<u8>, field: u64, b: &[u8]) {
    pb_varint(out, field << 3 | 2);
    pb_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn pb_varint(out: &mut Vec<u8>, mut v: u64) {
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

fn pb_ints(out: &mut Vec<u8>, fields: &[(u64, u64)]) {
    for &(f, v) in fields {
        pb_varint(out, f << 3);
        pb_varint(out, v);
    }
}

#[test]
fn typed_ranges_decode_single_and_multi_line() {
    // Occurrence.single_line_range (8) {line, start_character, end_character} and
    // Occurrence.multi_line_range (9) {start_line, start_character, end_line, end_character};
    // zero fields are left out, as protobuf writers do.
    let mut single = vec![];
    pb_ints(&mut single, &[(1, 4), (3, 9)]);
    let mut multi = vec![];
    pb_ints(&mut multi, &[(1, 2), (2, 1), (3, 5), (4, 7)]);
    let mut doc = vec![];
    pb_bytes(&mut doc, 1, b"a.go");
    for (field, range) in [(8, &single), (9, &multi)] {
        let mut occ = vec![];
        pb_bytes(&mut occ, 2, b"local 1");
        pb_bytes(&mut occ, field, range);
        pb_bytes(&mut doc, 2, &occ);
    }
    let mut ix = vec![];
    pb_bytes(&mut ix, 2, &doc);
    let got = scip::decode(&ix).unwrap();
    let ranges: Vec<Vec<i64>> = got.documents[0].occurrences.iter().map(|o| o.range.clone()).collect();
    assert_eq!(ranges, [vec![4, 0, 9], vec![2, 1, 5, 7]]);
}

#[test]
fn a_scip_go_index_with_typed_ranges_imports() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scip");
    let bytes = fs::read(fx.join("shapes-scip-go-0.2.9.scip")).unwrap();
    let ix = scip::decode(&bytes).unwrap();
    assert!(ix.tool.starts_with("scip-go 0.2.9"), "{}", ix.tool);
    let occs: Vec<&Occurrence> = ix.documents.iter().flat_map(|d| &d.occurrences).collect();
    assert!(!occs.is_empty() && occs.iter().all(|o| o.range.len() >= 3), "every occurrence has a range");
    let total = occs.iter().find(|o| o.symbol.ends_with("/Total().") && o.roles & scip::DEFINITION != 0).unwrap();
    assert_eq!(total.range, [14, 5, 10]);
    // End to end: the index (no embedded text, written after the file) upgrades references.
    let repo = tempfile::tempdir().unwrap();
    let r = repo.path();
    write(r, "go.mod", "module example.com/shapes\n\ngo 1.21\n");
    write(r, "shapes.go", &fs::read_to_string(fx.join("shapes.go")).unwrap());
    let mut c = db::open_rw().unwrap();
    let p = index(&mut c, r);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let out_dir = tempfile::tempdir().unwrap();
    let f = out_dir.path().join("index.scip");
    fs::write(&f, &bytes).unwrap();
    let st = scip::import(&mut c, &p, &f, true).unwrap();
    assert_eq!(st.fresh, 1, "{st:?}");
    assert!(st.definitions > 0 && st.precise > 0, "{st:?}");
}

const SHAPE: &str = "export interface Shape {\n  area(): number;\n}\n";
const SQ: &str = "import { Shape } from \"./i\";\nexport class Sq implements Shape {\n  area() { return 1; }\n}\n";
const TOTAL: &str = "import { Shape } from \"./i\";\nexport function total(x: Shape) {\n  return x.area();\n}\n";

#[test]
fn precise_implements_rows_still_link_implementations() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let repo = tempfile::tempdir().unwrap();
    let r = repo.path();
    write(r, "i.ts", SHAPE);
    write(r, "s.ts", SQ);
    write(r, "u.ts", TOTAL);
    let mut c = db::open_rw().unwrap();
    let p = index(&mut c, r);
    let ts = |d: &str| format!("scip-typescript npm demo 1.0.0 {d}");
    let doc = |path: &str, occurrences: Vec<Occurrence>| Document {
        relative_path: path.to_string(),
        occurrences,
        text: fs::read_to_string(r.join(path)).ok(),
        position_encoding: 1,
    };
    let ix = Index {
        tool: "scip-typescript 0.3.15".into(),
        project_root: format!("file://{}", r.display()),
        documents: vec![
            doc("i.ts", vec![occ(0, 17, 22, &ts("`i.ts`/Shape#"), true), occ(1, 2, 6, &ts("`i.ts`/Shape#area()."), true)]),
            doc(
                "s.ts",
                vec![
                    occ(0, 9, 14, &ts("`i.ts`/Shape#"), false),
                    occ(1, 13, 15, &ts("`s.ts`/Sq#"), true),
                    occ(1, 27, 32, &ts("`i.ts`/Shape#"), false),
                    occ(2, 2, 6, &ts("`s.ts`/Sq#area()."), true),
                ],
            ),
            doc("u.ts", vec![occ(0, 9, 14, &ts("`i.ts`/Shape#"), false), occ(2, 11, 15, &ts("`i.ts`/Shape#area()."), false)]),
        ],
    };
    let out_dir = tempfile::tempdir().unwrap();
    let f = save_index(out_dir.path(), &ix);
    let st = scip::import(&mut c, &p, &f, false).unwrap();
    assert!(st.precise > 0, "{st:?}");
    let amb: i64 = c
        .query_row("SELECT amb FROM ts_ref WHERE path = 's.ts' AND name = 'Shape' AND line = 2 AND kind != 'import'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(amb, -1, "the implements row is precise");
    // Impact of the interface method reaches the implementation through that row.
    let ro = db::open_ro().unwrap();
    let opts = code::CodeOpts { rel: "impact", depth: 2, budget: 2000, cursor: 0, fallback: false };
    let after = code::run(&ro, &p, "Shape.area", &opts).unwrap();
    assert!(after.contains("Sq.area"), "a precise implements row still links Sq to Shape:\n{after}");
}

const BASE: &str = "export class B {\n  m() { return 1; }\n}\n";
const SUB: &str = "import { B } from \"./b\";\nexport class D extends B {\n  m() { return 2; }\n}\n";
const USE: &str = "import { B } from \"./b\";\nexport function f(x: B) {\n  return x.m();\n}\n";

#[test]
fn a_precise_answer_that_leaves_out_calls_through_an_overridden_method_says_so() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let repo = tempfile::tempdir().unwrap();
    let r = repo.path();
    write(r, "b.ts", BASE);
    write(r, "d.ts", SUB);
    write(r, "u.ts", USE);
    let mut c = db::open_rw().unwrap();
    let p = index(&mut c, r);
    let ts = |d: &str| format!("scip-typescript npm demo 1.0.0 {d}");
    let doc = |path: &str, occurrences: Vec<Occurrence>| Document {
        relative_path: path.to_string(),
        occurrences,
        text: fs::read_to_string(r.join(path)).ok(),
        position_encoding: 1,
    };
    let ix = Index {
        tool: "scip-typescript 0.3.15".into(),
        project_root: format!("file://{}", r.display()),
        documents: vec![
            doc("b.ts", vec![occ(0, 13, 14, &ts("`b.ts`/B#"), true), occ(1, 2, 3, &ts("`b.ts`/B#m()."), true)]),
            doc(
                "d.ts",
                vec![
                    occ(0, 9, 10, &ts("`b.ts`/B#"), false),
                    occ(1, 13, 14, &ts("`d.ts`/D#"), true),
                    occ(1, 23, 24, &ts("`b.ts`/B#"), false),
                    occ(2, 2, 3, &ts("`d.ts`/D#m()."), true),
                ],
            ),
            doc("u.ts", vec![occ(0, 9, 10, &ts("`b.ts`/B#"), false), occ(2, 11, 12, &ts("`b.ts`/B#m()."), false)]),
        ],
    };
    let out_dir = tempfile::tempdir().unwrap();
    let f = save_index(out_dir.path(), &ix);
    scip::import(&mut c, &p, &f, false).unwrap();
    let amb: i64 = c.query_row("SELECT amb FROM ts_ref WHERE path = 'u.ts' AND name = 'm' AND kind = 'call'", [], |r| r.get(0)).unwrap();
    assert_eq!(amb, -1, "the call is precise, to B.m");
    // `x.m()` may run D.m: the answer for D.m must not be a silent 0.
    let out = callers(&p, "D.m");
    assert!(out.contains("may be incomplete") && out.contains("u.ts"), "{out}");
}
