//! `fox packet "<question>"` and `fox_graph rel=packet`: callers and 2-hop impact of the symbols a
//! question names, computed up front, certain rows apart from guesses, trimmed to a budget.

use foxdb::{db, inventory, mcp, project, qpacket, tsindex};
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn inline_project(files: &[(&str, &str)]) -> (tempfile::TempDir, tempfile::TempDir, project::Project) {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: every test in this file holds SERIAL.
    unsafe { std::env::set_var("FOXDB_HOME", home.path()) };
    let repo = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let f = repo.path().join(path);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, text).unwrap();
    }
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    (home, repo, p)
}

fn packet(p: &project::Project, q: &str, budget: usize) -> String {
    qpacket::run(&db::open_ro().unwrap(), p, q, budget).unwrap()
}

const CHAIN: &[(&str, &str)] = &[
    ("lib/core.c", "int leaf(int x) { return x + 1; }\nint mid(int x) { return leaf(x); }\nint top_fn(void) { return mid(1); }\n"),
    ("app/main.c", "int leaf(int x);\nint main(void) { return leaf(2); }\n"),
    (
        "app/more.c",
        "int first_far_caller(void) { return mid(2); }\nint second_far_caller(void) { return mid(3); }\nint third_far_caller(void) { return mid(4); }\n",
    ),
];

#[test]
fn packet_lists_definition_callers_and_two_hop_impact() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(CHAIN);
    let out = packet(&p, "List every function that calls `leaf`. Do not edit files.", 2000);
    assert!(out.contains("lib/core.c:1 | int leaf(int x)"), "{out}");
    assert!(out.contains("callers of ~c:") && out.contains(" mid c:") && out.contains(" main c:"), "{out}");
    assert!(out.contains("2 call sites in 2 files"), "{out}");
    let d2 = out.find("\nd2\n").unwrap_or_else(|| panic!("{out}"));
    assert!(out[d2..].contains("top_fn"), "{out}");
    // Impact d1 is the two callers again: counted, not repeated.
    assert!(out.contains("(2 more d1 dependents are the callers listed above, not repeated)"), "{out}");
    assert_eq!(out.matches(" mid c:").count(), 1, "{out}");
    // The accounting line keeps its verdict wording.
    assert!(out.contains("all accounted for in indexed"), "{out}");
    assert_eq!(out, packet(&p, "List every function that calls `leaf`. Do not edit files.", 2000), "deterministic");
}

#[test]
fn unindexed_names_are_left_out() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(CHAIN);
    let out = packet(&p, "Who calls mid_level and `leaf` on x86_64?", 2000);
    assert!(out.contains("callers of ~c:"), "{out}");
    assert!(!out.contains("x86_64") || out.contains("not indexed: x86_64"), "{out}");
    assert!(!out.contains("of `x86_64`"), "{out}");
}

#[test]
fn path_hint_picks_the_definition() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("fs/exec.c", "static int helper(void) { return 0; }\nint do_exec(void) { return helper(); }\n"),
        ("drivers/tty/tty.c", "static int helper(void) { return 2; }\nint tty_open(void) { return helper(); }\n"),
    ]);
    let out = packet(&p, "List every caller of `helper` (defined at drivers/tty/tty.c:1).", 2000);
    assert!(out.contains("drivers/tty/tty.c:1 | static int helper"), "{out}");
    assert!(out.contains("tty_open") && !out.contains("do_exec"), "{out}");
}

#[test]
fn guess_rows_sit_apart_from_certain_rows() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("include/linux/sched.h", "struct task;\nint wake_task(struct task *p);\n"),
        ("kernel/sched/core.c", "int wake_task(struct task *p) { return 1; }\nstatic int pick(void) { return wake_task(0); }\n"),
        ("kernel/kthread.c", "int kthread_start(void) { return wake_task(0); }\n"),
        ("fs/exec.c", "int do_exec(void) { return wake_task(0); }\n"),
        ("tools/perf/util/wake.c", "int wake_task(void *p) { return 0; }\n"),
    ]);
    let out = packet(&p, "Who calls `wake_task` (defined at kernel/sched/core.c:1)?", 2000);
    let split = out.find("guesses by name").unwrap_or_else(|| panic!("{out}"));
    let (certain, guesses) = out.split_at(split);
    assert!(!certain.lines().any(|l| l.starts_with("  :") && l.contains(" guess 1 of ")), "{out}");
    assert!(certain.contains(" pick c:"), "{out}");
    assert!(guesses.contains("kernel/kthread.c:1 kthread_start") && guesses.contains("fs/exec.c:1 do_exec"), "{out}");
    assert!(guesses.contains("guess 1 of 2"), "{out}");
    assert!(!certain.contains(" exec.c (1)") && !certain.contains("\nd2\n"), "{out}");
    assert!(out.contains("may be incomplete"), "{out}");
}

#[test]
fn budget_drops_the_deepest_rows_first() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(CHAIN);
    let q = "List every function that calls `leaf`.";
    let full = packet(&p, q, 2000);
    let budget = full.len() / 4 - 10;
    let out = packet(&p, q, budget);
    assert!(out.len() <= budget * 4, "{} > {}\n{out}", out.len(), budget * 4);
    assert!(!out.contains("third_far_caller") && out.contains(" mid c:") && out.contains(" main c:"), "{out}");
    assert!(out.contains("left out to fit the budget") && !out.contains("[packet cut"), "{out}");
}

#[test]
fn no_symbol_falls_back_to_find() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(CHAIN);
    let out = packet(&p, "where is the core library code", 800);
    assert!(out.starts_with("no name in the question resolves"), "{out}");
    assert!(out.contains("core.c"), "{out}");
}

#[test]
fn cli_and_mcp_serve_the_packet() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (h, r, p) = inline_project(CHAIN);
    let q = "List every function that calls `leaf`.";
    let want = packet(&p, q, 800);
    let run = |dir: &std::path::Path, extra: &[&str]| {
        let o = std::process::Command::new(env!("CARGO_BIN_EXE_fox"))
            .arg("packet")
            .arg(q)
            .args(extra)
            .current_dir(dir)
            .env("FOXDB_HOME", h.path())
            .env("FOXDB_NO_DAEMON", "1")
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap()
    };
    assert_eq!(run(r.path(), &["--budget", "800"]).trim_end(), want.trim_end());
    let elsewhere = tempfile::tempdir().unwrap();
    assert_eq!(run(elsewhere.path(), &["--project", &p.name, "--budget", "800"]).trim_end(), want.trim_end());

    let rels = mcp::tools()[1]["inputSchema"]["properties"]["rel"]["enum"].clone();
    assert!(rels.as_array().unwrap().iter().any(|v| v == "packet"), "{rels}");
    let here = std::env::current_dir().unwrap();
    std::env::set_current_dir(r.path()).unwrap();
    let got = mcp::call_tool("fox_graph", &serde_json::json!({"rel": "packet", "target": q, "budget": 800}));
    std::env::set_current_dir(here).unwrap();
    assert_eq!(got.unwrap().trim_end(), want.trim_end());
}

#[test]
fn unknown_qualifier_does_not_authorize_a_bare_name_match() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, _repo, p) = inline_project(CHAIN);
    let out = packet(&p, "Who calls `Nonexistent.leaf`?", 800);
    assert!(!out.contains("== Nonexistent.leaf"), "{out}");
}

#[test]
fn missing_names_fit_the_packet_budget() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, _repo, p) = inline_project(CHAIN);
    let q = (0..100).map(|i| format!("`not_indexed_{i}` ")).collect::<String>();
    let out = packet(&p, &q, 100);
    assert!(out.len() <= 400, "{} bytes", out.len());
}

#[test]
fn equally_plausible_definitions_are_not_presented_as_a_single_proven_target() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, _repo, p) = inline_project(&[
        ("a.c", "static int helper(void) { return 1; }\nint a(void) { return helper(); }\n"),
        ("b.c", "static int helper(void) { return 2; }\nint b(void) { return helper(); }\n"),
    ]);
    let out = packet(&p, "Who calls `helper`?", 800);
    let shows_both = ["a.c:1", "b.c:1"].iter().all(|&location| out.lines().any(|line| line.starts_with("def ") && line.contains(location)));
    assert!(shows_both || out.contains("ambiguous"), "{out}");
    assert!(!out.contains("== helper"), "{out}");
    let out = packet(&p, "Who calls `helper` (at a.c:1 and b.c:1)?", 800);
    assert!(out.contains("ambiguous: helper") && !out.contains("== helper"), "{out}");
}

#[test]
fn tiny_budgets_bound_regular_and_fallback_packets() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, _repo, p) = inline_project(CHAIN);
    for q in ["Who calls `leaf`?", "Who calls `nonexistent`?"] {
        for budget in [0, 1, 20] {
            let out = packet(&p, q, budget);
            assert!(out.len() <= 4 * budget, "{out}");
        }
    }
}

#[test]
fn mixed_indexes_resolve_only_a_unique_definition() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, _repo, p) = inline_project(CHAIN);
    db::open_rw()
        .unwrap()
        .execute(
            r#"INSERT INTO node(project_id, kind, key, name, sig, lang, tier, source, path, line, meta, updated_at)
         VALUES (?1, 'symbol', 'swift-leaf', 'leaf', 'leaf()', 'swift', 3, 'indexstore', 'App.swift', 1,
         '{"k":"func","parent":"A","mod":"App"}', 0)"#,
            [p.id],
        )
        .unwrap();
    let out = packet(&p, "Who calls `leaf`?", 800);
    assert!(out.contains("ambiguous: leaf") && !out.contains("== leaf"), "{out}");
    let out = packet(&p, "Who calls `leaf` at lib/core.c:1?", 800);
    assert!(out.contains("def ~c:") && out.contains("lib/core.c:1"), "{out}");
    let out = packet(&p, "Who calls `Fake.A.leaf`?", 800);
    assert!(out.contains("not indexed: Fake.A.leaf") && !out.contains("== Fake.A.leaf"), "{out}");
    let out = packet(&p, "Who calls `App.A.leaf`?", 800);
    assert!(out.contains("def s:") && out.contains("App.swift:1"), "{out}");
}

#[test]
fn full_spaced_location_beats_the_overlapping_suffix() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, _repo, p) = inline_project(&[
        ("src/with spaces/tool.c", "static int helper(void) { return 1; }\n"),
        ("spaces/tool.c", "static int helper(void) { return 2; }\n"),
    ]);
    let out = packet(&p, "Who calls `helper` at src/with spaces/tool.c:1?", 800);
    assert!(out.lines().any(|l| l.starts_with("def ") && l.contains("src/with spaces/tool.c:1")), "{out}");
}
