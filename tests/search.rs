//! Search: parallel arms (exact, keyword, trigram, fuzzy/prefix, graph) fused with RRF, code-aware
//! ranking (definitions up; tests, vendor and generated code down; same-file hits damped), exact
//! `path:line` on every row, and the generation-keyed result cache.

use foxdb::{db, inventory, project, query, search, tsindex};
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
        "app/src/main/kotlin/player/PlayerViewModel.kt",
        "package player\n\nclass PlayerViewModel {\n    val title: String = \"\"\n    val state: Int = 0\n    val queue: Int = 0\n    fun play() { selectTrack() }\n    fun selectTrack() {}\n}\n",
    );
    write(
        r,
        "app/src/test/kotlin/player/PlayerViewModelTest.kt",
        "package player\n\nclass PlayerViewModelTest {\n    fun plays() { PlayerViewModel().play() }\n}\n",
    );
    write(r, "vendor/media/PlayerViewModelShim.kt", "package media\n\nclass PlayerViewModelShim {\n}\n");
    write(
        r,
        "app/src/main/kotlin/subtitle/SubtitleSelectionPolicy.kt",
        "package subtitle\n\nobject SubtitleSelectionPolicy {\n    fun pickTrack(): Int = 0\n}\n",
    );
    write(r, "core/src/user.rs", "pub fn load_user_name() -> String {\n    String::new()\n}\n");
    write(r, "docs/guide.md", "# Guide\n\n## Subtitle selection\n\nThe subtitle selection policy picks one track.\n");
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(r)).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    let p = project::find(&c, &project::locate(r)).unwrap().unwrap();
    Env { _home: home, repo, c, p }
}

fn find(e: &Env, q: &str, kind: &str) -> String {
    query::find(&e.c, Some(&e.p), q, &query::FindOpts { kind, all: false, budget: 800, limit: 10, offset: 0 }).unwrap()
}

fn rows(out: &str) -> Vec<&str> {
    out.lines().filter(|l| !l.starts_with('+') && !l.is_empty()).collect()
}

fn rank(out: &str, needle: &str) -> Option<usize> {
    rows(out).iter().position(|l| l.contains(needle))
}

#[test]
fn natural_language_query_finds_the_policy_and_the_doc() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = setup();
    let out = find(&e, "how does subtitle selection work", "any");
    assert!(rank(&out, "SubtitleSelectionPolicy").is_some_and(|k| k < 3), "{out}");
    assert!(out.contains("Subtitle selection"), "{out}");
}

#[test]
fn definition_beats_test_and_vendor_copies() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = setup();
    let out = find(&e, "PlayerViewModel", "any");
    let first = rows(&out)[0];
    assert!(first.contains("class PlayerViewModel") && first.contains("app/src/main/kotlin/player/PlayerViewModel.kt:3"), "{out}");
    let def = rank(&out, "class PlayerViewModel ").unwrap();
    let test = rank(&out, "PlayerViewModelTest").unwrap_or(usize::MAX);
    let vendor = rank(&out, "vendor/").unwrap_or(usize::MAX);
    assert!(def < test && def < vendor, "{out}");
}

#[test]
fn repeat_hits_from_one_file_are_damped() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = setup();
    let out = find(&e, "PlayerViewModel", "any");
    let top: Vec<&str> = rows(&out).into_iter().take(4).collect();
    let same = top.iter().filter(|l| l.contains("main/kotlin/player/PlayerViewModel.kt")).count();
    assert!(same <= 2, "one file floods the top rows:\n{out}");
}

#[test]
fn every_row_has_an_exact_path_and_line() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = setup();
    let re = regex::Regex::new(r"\S+:\d+").unwrap();
    for q in ["PlayerViewModel", "subtitle", "guide", "load_user_name"] {
        let out = find(&e, q, "any");
        for l in rows(&out) {
            if l.starts_with("no matches") {
                continue;
            }
            assert!(re.is_match(l), "row without path:line for {q:?}: {l}");
        }
    }
}

#[test]
fn camel_case_query_matches_snake_case_definition() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = setup();
    let out = find(&e, "loadUserName", "any");
    assert!(rank(&out, "load_user_name").is_some_and(|k| k < 2), "{out}");
    let out = find(&e, "user name", "symbol");
    assert!(rank(&out, "load_user_name").is_some(), "{out}");
}

#[test]
fn prefix_names_rank_first_and_warm_equals_cold() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = setup();
    let out = find(&e, "SubtitleSel", "symbol");
    assert!(rank(&out, "SubtitleSelectionPolicy").is_some_and(|k| k == 0), "{out}");
    // The warm path (foxd, MCP) runs the same arms: same answer, byte for byte.
    let mut warm = search::Warm::default();
    warm.refresh(&e.c).unwrap();
    for (q, kind) in [("SubtitleSel", "symbol"), ("SubSelPol", "symbol"), ("subtitle", "any")] {
        let o = query::FindOpts { kind, all: false, budget: 800, limit: 10, offset: 0 };
        let warm_out = search::find_warm(&mut warm, &e.c, Some(&e.p), q, &o).unwrap();
        assert_eq!(warm_out, search::find(&e.c, Some(&e.p), q, &o).unwrap(), "{q}");
    }
}

#[test]
fn graph_neighbours_surface_related_symbols() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = setup();
    let ids = search::graph_arm(&e.c, None, "play", e.p.id, 10).unwrap();
    let names: Vec<String> =
        ids.iter().map(|id| e.c.query_row("SELECT name FROM node WHERE id = ?1", [id], |r| r.get::<_, String>(0)).unwrap()).collect();
    assert!(names.contains(&"selectTrack".to_string()), "callee of play() missing: {names:?}");
    // The warm CSR graph gives the same neighbours.
    let mut warm = search::Warm::default();
    warm.refresh(&e.c).unwrap();
    let ids2 = search::graph_arm(&e.c, Some(warm.graph(&e.c, e.p.id).unwrap()), "play", e.p.id, 10).unwrap();
    let mut a = ids.clone();
    let mut b = ids2.clone();
    a.sort();
    b.sort();
    assert_eq!(a, b);
}

#[test]
fn result_cache_is_keyed_by_generation() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = setup();
    let mut warm = search::Warm::default();
    warm.refresh(&e.c).unwrap();
    let o = query::FindOpts { kind: "any", all: false, budget: 800, limit: 10, offset: 0 };
    let a = search::find_warm(&mut warm, &e.c, Some(&e.p), "SubtitleSelectionPolicy", &o).unwrap();
    assert_eq!(warm.cache_hits(), 0);
    let b = search::find_warm(&mut warm, &e.c, Some(&e.p), "SubtitleSelectionPolicy", &o).unwrap();
    assert_eq!(a, b);
    assert_eq!(warm.cache_hits(), 1);
    // A new file bumps the generation; the cached answer is not reused.
    write(e.repo.path(), "app/src/main/kotlin/subtitle/SubtitleSelectionPolicyV2.kt", "object SubtitleSelectionPolicyV2 {}\n");
    inventory::index_project(&mut e.c, &e.p).unwrap();
    tsindex::index_project(&mut e.c, &e.p).unwrap();
    let c2 = search::find_warm(&mut warm, &e.c, Some(&e.p), "SubtitleSelectionPolicy", &o).unwrap();
    assert_eq!(warm.cache_hits(), 1);
    assert!(c2.contains("SubtitleSelectionPolicyV2"), "{c2}");
}
