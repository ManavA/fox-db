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

// ---- graph-expanded find ----

/// A question in words whose answer is split over files: the function the words describe calls a
/// helper in another file that shares none of the words. The top symbol hits anchor a one-hop
/// walk of the call graph, so the helper's file is in the answer; without the walk it is not.
fn expand_fixture() -> Env {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    let r = repo.path();
    write(
        r,
        "app/headers.py",
        "from app.wire.pairs import split_pair\n\n\ndef parse_header(raw):\n    \"\"\"Parse one request header line into its name and value.\"\"\"\n    name, value = split_pair(raw)\n    return name, value\n",
    );
    write(r, "app/wire/pairs.py", "def split_pair(raw):\n    i = raw.index(\":\")\n    return raw[:i].strip(), raw[i + 1 :].strip()\n");
    // Lexical noise: many functions that share the question's words.
    for i in 0..6 {
        write(
            r,
            &format!("app/noise/n{i}.py"),
            &format!("def request_line_{i}(name, value):\n    \"\"\"A request header value and name, line {i}.\"\"\"\n    return name\n"),
        );
    }
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(r)).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    let p = project::find(&c, &project::locate(r)).unwrap().unwrap();
    Env { _home: home, repo, c, p }
}

fn files_in(out: &str) -> Vec<String> {
    let mut v: Vec<String> = vec![];
    for l in out.lines() {
        if let Some(p) = l
            .split_whitespace()
            .find_map(|w| w.rsplit_once(':').filter(|(p, n)| p.contains('.') && n.parse::<u32>().is_ok()).map(|(p, _)| p))
            && !v.iter().any(|x| x == p)
        {
            v.push(p.to_string());
        }
    }
    v
}

#[test]
fn a_question_in_words_finds_the_files_its_top_hits_call_into() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = expand_fixture();
    let q = "parse a request header line into name and value";
    let with = find(&e, q, "any");
    let files = files_in(&with);
    let at = files.iter().position(|f| f == "app/wire/pairs.py");
    assert!(at.is_some_and(|i| i < 10), "the callee's file is in the top files: {files:?}\n{with}");
    // The same search with the walk switched off misses it: the walk is what found it.
    let without = search::with_expansion(false, || find(&e, q, "any"));
    assert!(!files_in(&without).iter().any(|f| f == "app/wire/pairs.py"), "{without}");
    // Deterministic, and bounded: the walk adds at most EXPAND_ANCHORS x EXPAND_PER_ANCHOR rows.
    assert_eq!(with, find(&e, q, "any"));
    let all = |on: bool| {
        let o = query::FindOpts { kind: "any", all: false, budget: 4000, limit: 200, offset: 0 };
        search::with_expansion(on, || query::find(&e.c, Some(&e.p), q, &o).unwrap()).lines().count()
    };
    let added = all(true) - all(false);
    assert!((1..=search::EXPAND_ANCHORS * search::EXPAND_PER_ANCHOR).contains(&added), "{added} rows added");
}

/// An identifier is not expanded: one with a definition is already precise, and expanding one
/// without cost recall on the localization tasks. The answer is the same bytes as without the walk.
#[test]
fn identifier_queries_are_not_expanded() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = expand_fixture();
    for q in ["parse_header", "split_pair", "HeaderParser", "app/wire"] {
        let with = find(&e, q, "any");
        let without = search::with_expansion(false, || find(&e, q, "any"));
        assert_eq!(with, without, "{q}");
    }
}

/// Expansion never lifts a helper above the definition that calls it. The definition here is the
/// second hit in its file, so same-file damping lowers it; its callees, lifted by the walk, must
/// still rank after it (a kubernetes question once put `TaintToleration.Filter`'s callees first).
#[test]
fn expansion_keeps_the_definition_above_its_callees_after_damping() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    let r = repo.path();
    write(
        r,
        "sched/taint.py",
        "from sched.helpers.a import tolerates\nfrom sched.helpers.b import fits\nfrom sched.helpers.c import ready\n\n\n\
         def scheduler_filter_nodes_by_taint_toleration(node):\n    \"\"\"Scheduler: filter nodes by taint toleration.\"\"\"\n    return 0\n\n\n\
         def check(pod, nodes):\n    \"\"\"Filter nodes by taint toleration for the pod.\"\"\"\n    return [n for n in nodes if tolerates(pod, n) and fits(pod, n) and ready(n)]\n",
    );
    for (f, name) in [("a", "tolerates"), ("b", "fits"), ("c", "ready")] {
        write(r, &format!("sched/helpers/{f}.py"), &format!("def {name}(*args):\n    return True\n"));
    }
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(r)).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    let p = project::find(&c, &project::locate(r)).unwrap().unwrap();
    let e = Env { _home: home, repo, c, p };
    let q = "where does the scheduler filter nodes by taint toleration";
    let with = find(&e, q, "any");
    let pos = |name: &str| with.lines().position(|l| l.contains(&format!(" {name} "))).unwrap_or_else(|| panic!("{name} missing:\n{with}"));
    let def = pos("check");
    for helper in ["tolerates", "fits", "ready"] {
        assert!(pos(helper) > def, "{helper} above the definition that calls it:\n{with}");
    }
    let without = search::with_expansion(false, || find(&e, q, "any"));
    let def_off = without.lines().position(|l| l.contains(" check ")).unwrap();
    assert_eq!(def, def_off, "expansion moved the definition:\n{with}\n---\n{without}");
}
