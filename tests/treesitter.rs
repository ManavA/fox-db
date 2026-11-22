//! Native tree-sitter indexer: per-language symbol/container/call extraction from
//! `tests/fixtures/<lang>/`, incremental re-indexing, `fox code` output shape and token budget,
//! and the token-savings ledger.

use foxdb::lang::{self, LANGS};
use foxdb::tsindex::{self, Parsed};
use foxdb::{code, db, inventory, project, savings};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn parse(rel: &str) -> Parsed {
    let l = lang::for_path(rel).unwrap_or_else(|| panic!("no grammar for {rel}"));
    let src = std::fs::read(fixtures().join(rel)).unwrap();
    tsindex::extract(l, &src)
}

fn qnames(p: &Parsed) -> Vec<String> {
    (0..p.defs.len())
        .map(|i| match p.container_of(i) {
            Some(c) => format!("{c}.{}", p.defs[i].name),
            None => p.defs[i].name.clone(),
        })
        .collect()
}

fn calls(p: &Parsed) -> Vec<(String, String)> {
    let q = qnames(p);
    p.refs.iter().filter(|r| r.kind == "call").map(|r| (r.caller.map(|i| q[i].clone()).unwrap_or_default(), r.name.clone())).collect()
}

/// (fixture, expected qualified definitions, expected (caller, callee) call edges)
type Case = (&'static str, &'static [&'static str], &'static [(&'static str, &'static str)]);
const CASES: &[Case] = &[
    (
        "kotlin/Greeter.kt",
        &["Speaker", "Speaker.speak", "Greeter", "Greeter.speak", "Greeter.greet", "Greeter.Companion", "Greeter.Companion.create", "main"],
        &[
            ("Greeter.speak", "greet"),
            ("Greeter.greet", "format"),
            ("Greeter.Companion.create", "Greeter"),
            ("main", "create"),
            ("main", "speak"),
        ],
    ),
    (
        "java/Greeter.java",
        &["Greeter", "Greeter.greet", "Greeter.format", "Greeter.main"],
        &[("Greeter.greet", "format"), ("Greeter.main", "greet")],
    ),
    (
        "python/greeter.py",
        &["Greeter", "Greeter.greet", "format_name", "main"],
        &[("Greeter.greet", "format_name"), ("main", "Greeter"), ("main", "greet")],
    ),
    ("typescript/greeter.ts", &["Greeter", "Greeter.greet", "format", "main"], &[("Greeter.greet", "format"), ("main", "greet")]),
    ("tsx/App.tsx", &["Button", "App", "title"], &[("App", "Button"), ("App", "title")]),
    ("javascript/greeter.js", &["Greeter", "Greeter.greet", "format", "main"], &[("Greeter.greet", "format"), ("main", "greet")]),
    ("go/greeter.go", &["Greeter", "Greeter.Greet", "format", "main"], &[("Greeter.Greet", "format"), ("main", "Greet")]),
    ("rust/greeter.rs", &["Greeter", "Greeter.greet", "format_name", "main"], &[("Greeter.greet", "format_name"), ("main", "greet")]),
    ("c/greeter.c", &["greeter", "format_name", "greet", "main"], &[("greet", "format_name"), ("main", "greet")]),
    ("cpp/greeter.cpp", &["format", "Greeter", "Greeter.greet", "main"], &[("Greeter.greet", "format"), ("main", "greet")]),
    (
        "csharp/Greeter.cs",
        &["Demo", "Demo.Greeter", "Demo.Greeter.Greet", "Demo.Greeter.Format", "Demo.Greeter.Main"],
        &[("Demo.Greeter.Greet", "Format"), ("Demo.Greeter.Main", "Greet")],
    ),
    ("swift/Greeter.swift", &["Greeter", "Greeter.greet", "format", "main"], &[("Greeter.greet", "format"), ("main", "greet")]),
    ("ruby/greeter.rb", &["Greeter", "Greeter.greet", "format_name", "main"], &[("Greeter.greet", "format_name"), ("main", "greet")]),
    ("php/greeter.php", &["Greeter", "Greeter.greet", "format_name", "main"], &[("Greeter.greet", "format_name"), ("main", "greet")]),
    ("bash/greet.sh", &["format_name", "greet", "main"], &[("greet", "format_name"), ("main", "greet")]),
    (
        "scala/Greeter.scala",
        &["Greeter", "Greeter.greet", "Main", "Main.main", "format"],
        &[("Greeter.greet", "format"), ("Main.main", "greet")],
    ),
    ("dart/greeter.dart", &["Greeter", "Greeter.greet", "format", "main"], &[("Greeter.greet", "format"), ("main", "greet")]),
    ("lua/greeter.lua", &["format_name", "greet", "main"], &[("greet", "format_name"), ("main", "greet")]),
    ("objc/Greeter.m", &["format", "Greeter", "Greeter.greet", "main"], &[("Greeter.greet", "format"), ("main", "greet")]),
];

#[test]
fn every_tag_query_compiles() {
    let bad: Vec<String> = LANGS.iter().filter_map(|l| l.query_error().map(|e| format!("{}: {e}", l.name))).collect();
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn every_language_has_a_fixture() {
    for l in LANGS {
        assert!(CASES.iter().any(|c| lang::for_path(c.0).is_some_and(|x| x.name == l.name)), "no fixture for {}", l.name);
    }
}

#[test]
fn fixtures_extract_symbols_containers_and_calls() {
    let mut fails = vec![];
    for (rel, defs, edges) in CASES {
        let p = parse(rel);
        let q = qnames(&p);
        let cs = calls(&p);
        for d in *defs {
            if !q.iter().any(|x| x == d) {
                fails.push(format!("{rel}: missing def {d} (got {q:?})"));
            }
        }
        for (a, b) in *edges {
            if !cs.iter().any(|(x, y)| x == a && y == b) {
                fails.push(format!("{rel}: missing call {a} -> {b} (got {cs:?})"));
            }
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

#[test]
fn kotlin_kinds_are_specific() {
    let p = parse("kotlin/Greeter.kt");
    let kind = |n: &str| p.defs.iter().find(|d| d.name == n).map(|d| d.kind.clone()).unwrap();
    assert_eq!(kind("Speaker"), "interface");
    assert_eq!(kind("Greeter"), "class");
    assert_eq!(kind("greet"), "method");
    assert_eq!(kind("main"), "function");
    let g = p.defs.iter().find(|d| d.name == "greet").unwrap();
    assert_eq!((g.line, g.end_line), (12, 14));
}

// ---- store-level tests ----

/// Call sites a callers answer lists as certain: all of them less those guessed by name. A tie
/// between same-score candidates is a guess, whichever of them it lands on.
fn certain_calls(out: &str) -> usize {
    let line = out.lines().find(|l| l.contains(" call sites in ")).unwrap_or("");
    let n = |s: &str| s.split_whitespace().next().and_then(|w| w.parse::<usize>().ok()).unwrap_or(0);
    let guessed = line.split_once(", ").map_or(0, |x| n(x.1));
    n(line) - guessed
}

fn setup(src: &Path) -> (tempfile::TempDir, tempfile::TempDir, project::Project) {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: tests touching FOXDB_HOME hold SERIAL.
    unsafe { std::env::set_var("FOXDB_HOME", home.path()) };
    let repo = tempfile::tempdir().unwrap();
    copy_dir(src, repo.path());
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    (home, repo, p)
}

fn copy_dir(from: &Path, to: &Path) {
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let dst = to.join(e.file_name());
        if e.path().is_dir() {
            std::fs::create_dir_all(&dst).unwrap();
            copy_dir(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

/// An answer without its accounting line and the groups at its end (unresolved mentions,
/// same-name definitions elsewhere, imports, ...): its resolved rows.
fn resolved(out: &str) -> String {
    const GROUPS: [&str; 8] = [
        "stores a function in it (not calls)",
        "unresolved mentions",
        "references, not calls",
        "declarations and prototypes",
        "same-name definitions elsewhere",
        "import lines",
        "comment lines",
        "strings (may be dynamic uses)",
    ];
    out.lines().skip(1).take_while(|l| !GROUPS.iter().any(|g| l.starts_with(g))).map(|l| format!("{l}\n")).collect()
}

fn fox_code(p: &project::Project, sym: &str, rel: &str, budget: usize) -> String {
    let c = db::open_ro().unwrap();
    code::run(&c, p, sym, &code::CodeOpts { rel, depth: 2, budget, cursor: 0, fallback: false }).unwrap()
}

#[test]
fn kotlin_project_index_resolves_calls_with_imports() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = setup(&fixtures().join("kotlin"));
    let mut c = db::open_rw().unwrap();
    let st = tsindex::index_project(&mut c, &p).unwrap();
    assert_eq!(st.parsed, 2);
    // `format` resolves through the import to util/Format.kt; `greet` to the same-file method, not Other.greet.
    let out = fox_code(&p, "format", "callers", 500);
    assert!(out.contains("\nGreeter.kt (1)") && out.contains(":13 Greeter.greet c:"), "{out}");
    let out = fox_code(&p, "Greeter.greet", "callers", 500);
    assert!(out.contains(":10 Greeter.speak"), "{out}");
    // `listOf(1).map {}` is a receiver call: not the unrelated, unimported top-level util `map`.
    let out = fox_code(&p, "map", "callers", 500);
    assert!(out.contains("0 call sites"), "{out}");
    let out = fox_code(&p, "Other.greet", "callers", 500);
    assert!(out.contains("0 call sites"), "{out}");
    let out = fox_code(&p, "Greeter", "outline", 500);
    assert!(out.contains("method greet") && out.contains("object Companion") && out.contains("property name"), "{out}");
    let out = fox_code(&p, "Greeter.greet", "callees", 500);
    assert!(out.contains("util/\n Format.kt (1)") && out.contains("function format"), "{out}");
    let out = fox_code(&p, "format", "impact", 500);
    assert!(out.contains("Greeter.greet") && out.contains("Greeter.speak"), "{out}");
    let out = fox_code(&p, "Greeter.greet", "src", 500);
    assert!(out.contains("12\t    fun greet") && out.contains("14\t    }"), "{out}");
    let out = fox_code(&p, "Greeter.kt", "outline", 500);
    assert!(out.starts_with("~outline Greeter.kt"), "{out}");
    // fox find shows tree-sitter symbols with c: handles and the ~ marker.
    let f = foxdb::query::find(
        &c,
        Some(&p),
        "greet",
        &foxdb::query::FindOpts { kind: "symbol", all: false, budget: 500, limit: 10, offset: 0 },
    )
    .unwrap();
    assert!(f.lines().any(|l| l.starts_with("~c:") && l.contains("method Greeter.greet")), "{f}");
}

#[test]
fn unchanged_files_are_not_reparsed() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, p) = setup(&fixtures().join("kotlin"));
    let mut c = db::open_rw().unwrap();
    assert_eq!(tsindex::index_project(&mut c, &p).unwrap().parsed, 2);
    let again = tsindex::index_project(&mut c, &p).unwrap();
    assert_eq!((again.parsed, again.unchanged), (0, 0));
    // Touch without changing content: mtime moves, hash matches, still no parse.
    let f = repo.path().join("Greeter.kt");
    let t = std::fs::read(&f).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&f, &t).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    let touched = tsindex::index_project(&mut c, &p).unwrap();
    assert_eq!(touched.parsed, 0);
    // A real edit re-parses just that file.
    std::fs::write(&f, String::from_utf8(t).unwrap() + "\nfun extra() = main()\n").unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    let edited = tsindex::index_project(&mut c, &p).unwrap();
    assert_eq!(edited.parsed, 1);
    assert!(fox_code(&p, "main", "callers", 500).contains("extra c:"));
    // Incremental (scoped) resolution matches a full re-resolution.
    let snap = |c: &rusqlite::Connection| -> Vec<(i64, i64)> {
        let mut q = c.prepare("SELECT rowid, dst FROM ts_ref ORDER BY rowid").unwrap();
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().flatten().collect()
    };
    let edges = |c: &rusqlite::Connection| -> i64 {
        c.query_row("SELECT COUNT(*) FROM edge WHERE source = 'treesitter'", [], |r| r.get(0)).unwrap()
    };
    let (inc, inc_e) = (snap(&c), edges(&c));
    tsindex::resolve(&c, p.id).unwrap();
    assert_eq!((snap(&c), edges(&c)), (inc, inc_e));
    // Removal drops the file's symbols.
    std::fs::remove_file(repo.path().join("util/Format.kt")).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    assert_eq!(tsindex::index_project(&mut c, &p).unwrap().removed, 1);
    assert!(!fox_code(&p, "Other", "outline", 500).contains("c:"));
}

/// A typical Kotlin `callers` answer: grouped by file, no repeated paths, line numbers only, and
/// well inside its token budget.
#[test]
fn kotlin_callers_answer_stays_within_token_budget() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("FOXDB_HOME", home.path()) };
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path().join("app/src/main/kotlin/demo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Repo.kt"), "package demo\n\nclass Repo {\n    fun load(id: String): String = id\n}\n").unwrap();
    for i in 0..12 {
        let body: String = (0..3).map(|j| format!("    fun use{j}(r: Repo) = r.load(\"{i}{j}\")\n")).collect();
        std::fs::write(dir.join(format!("Screen{i}.kt")), format!("package demo\n\nclass Screen{i} {{\n{body}}}\n")).unwrap();
    }
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    let out = fox_code(&p, "Repo.load", "callers", 500);
    assert!(out.contains("36 call sites in 12 files"), "{out}");
    let toks = out.len() / 4;
    assert!(toks <= 400, "callers answer is {toks} tokens:\n{out}");
    // The directory appears once, each file once; rows carry only `:lines Caller handle`, one per caller.
    assert_eq!(out.matches("app/src/main/kotlin/demo/").count(), 2, "{out}"); // target row + one dir header
    assert_eq!(out.matches("Screen3.kt").count(), 1, "{out}");
    assert!(out.contains("  :4 Screen3.use0 c:"), "{out}");
    assert!(out.lines().filter(|l| l.starts_with("  :")).all(|l| !l.contains(".kt") && l.len() < 40), "{out}");
    assert!(savings::cited_paths(&out).contains(&"app/src/main/kotlin/demo/Screen3.kt".to_string()));
}

#[test]
fn savings_ledger_counts_and_notes_occasionally() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = setup(&fixtures().join("kotlin"));
    let mut c = db::open_rw().unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    let out = fox_code(&p, "format", "callers", 500);
    assert_eq!(savings::cited_paths(&out), vec!["util/Format.kt".to_string(), "Greeter.kt".to_string()]);
    let size = |f: &str| savings::tokens(std::fs::metadata(_r.path().join(f)).unwrap().len() as usize);
    assert_eq!(savings::estimate(&c, Some(p.id), &out), (size("Greeter.kt") + size("util/Format.kt") - savings::tokens(out.len())).max(0));
    for _ in 0..40 {
        savings::record(&c, "code callers", 100).unwrap();
    }
    let s = savings::stats(&c).unwrap();
    assert!(s.contains("today: ~4000 in 40 answers") && s.contains("top: code callers ~4000/40"), "{s}");
    assert!(savings::brief_line(&c).unwrap().contains("~4000 tokens this week"));
}

/// A project from inline files under a scratch FOXDB_HOME, indexed.
fn inline_project(files: &[(&str, &str)]) -> (tempfile::TempDir, tempfile::TempDir, project::Project) {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: tests touching FOXDB_HOME hold SERIAL.
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

/// C has one namespace for non-static functions: a call in any directory reaches the one
/// definition, not its header prototype; `static` functions stay in their own file.
#[test]
fn c_calls_resolve_across_directories_and_static_stays_file_local() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("include/linux/sched.h", "struct task;\nint wake_task(struct task *p);\n"),
        ("kernel/sched/core.c", "int wake_task(struct task *p) { return 1; }\nstatic int pick(void) { return wake_task(0); }\n"),
        ("kernel/kthread.c", "int kthread_start(void) { return wake_task(0); }\n"),
        ("fs/exec.c", "static int helper(void) { return 0; }\nint do_exec(void) { helper(); return wake_task(0); }\n"),
        ("drivers/tty/tty.c", "static int helper(void) { return 2; }\nint tty_open(void) { return helper(); }\n"),
        ("tools/perf/util/wake.c", "int wake_task(void *p) { return 0; }\n"),
        ("fs/ops.c", "struct ops { int (*open)(void); };\nstatic struct ops fops = { .open = tty_open };\n"),
    ]);
    let out = fox_code(&p, "wake_task", "callers", 800);
    assert!(out.contains("~c:") && out.contains("kernel/sched/core.c:1"), "{out}");
    assert!(out.contains("3 call sites in 3 files"), "{out}");
    assert!(out.contains("kthread.c (1)") && out.contains("exec.c (1)") && out.contains("core.c (1)"), "{out}");
    // Two of the calls are guesses among same-name definitions: the verdict says so.
    assert!(out.contains("may be incomplete: 2 lines reach the target only through a guess by name"), "{out}");
    // `src` leads with the definition, not the prototype or the tools/ copy.
    let out = fox_code(&p, "wake_task", "src", 800);
    assert!(out.contains("kernel/sched/core.c:1-1"), "{out}");
    // Each static helper is called only from its own file.
    let out = fox_code(&p, "helper", "callers", 800);
    assert!(out.contains("2 matches") && out.contains("1 call sites in 1 files"), "{out}");
    assert!(out.contains(" exec.c (1)\n  :2 do_exec") || out.contains(" tty.c (1)\n  :2 tty_open"), "{out}");
    // A function named in a designated initializer is a reference.
    let out = fox_code(&p, "tty_open", "refs", 800);
    assert!(out.contains("fs/\n ops.c (1)"), "{out}");
}

/// Kotlin calls through a receiver chain or a property reach a member of another package
/// (as a name guess), a function reference `::fn` is a reference, and the name as typed beats
/// a same-name class in another case.
#[test]
fn kotlin_receiver_chains_function_references_and_exact_case() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "app/ui/theme/Palette.kt",
            "package app.ui.theme\n\nclass Palette {\n    fun tintFor(x: Int): Int = x\n    fun glowFor(x: Int): Int = x\n}\n",
        ),
        ("app/plan/Inputs.kt", "package app.plan\n\nclass Inputs {\n    fun render(plan: String): String = plan\n}\n"),
        ("app/plan/Render.kt", "package app.plan\n\nclass Render\n"),
        ("app/math/Steps.kt", "package app.math\n\nfun stepOnce(acc: Int, x: Int): Int = acc + x\n"),
        (
            "app/feature/Screen.kt",
            "package app.feature\n\nimport app.math.stepOnce\nimport app.plan.Inputs\n\nclass Screen(val theme: Theme, val inputs: Inputs) {\n    fun draw() = theme.current.palette.tintFor(1)\n    fun show(plan: String) = inputs.render(plan)\n    fun total(xs: List<Int>) = xs.fold(0, ::stepOnce)\n    fun glow() = theme.current.colors.glowFor(2)\n}\n",
        ),
    ]);
    let out = fox_code(&p, "tintFor", "callers", 800);
    // The receiver `palette` names the type `Palette`, which is not imported: a guess.
    assert!(out.contains("1 guessed by name") && out.contains(":7 Screen.draw c:") && out.contains(" guess by name"), "{out}");
    // `render` is the method, not the class `Render`; its call through an imported type's property is found.
    let out = fox_code(&p, "render", "impact", 800);
    assert!(out.contains("method Inputs.render") && out.contains("Screen.show"), "{out}");
    // A receiver named unlike any type: a guess by name.
    let out = fox_code(&p, "glowFor", "callers", 800);
    assert!(out.contains("1 guessed by name") && out.contains(":10 Screen.glow c:") && out.contains(" guess by name"), "{out}");
    let out = fox_code(&p, "stepOnce", "impact", 800);
    assert!(out.contains("Screen.total"), "{out}");
}

/// Lines naming the function that are not calls (callbacks) are counted and listed apart;
/// when every line is a call or the definition, nothing is listed.
#[test]
fn callers_say_when_the_name_occurs_in_unlisted_files() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("kernel/core.c", "int wake_task(int p) { return p; }\nint run(void) { return wake_task(1); }\n"),
        ("mm/a.c", "int (*cb)(int);\nint use_a(void) { register_cb(wake_task); return 0; }\n"),
        ("mm/b.c", "int use_b(void) { return queue_work(wake_task, 2); }\n"),
    ]);
    let out = fox_code(&p, "wake_task", "callers", 800);
    // Callbacks are references, not calls: counted and listed, and what calls through them is unknown.
    assert!(out.starts_with("4 lines name `wake_task`: 1 call, 1 definition, 2 references, not calls; may be incomplete: 2 lines pass it as a value, and calls through them are not followed (rel=impact lists the code passing it)\n"), "{out}");
    assert!(out.contains("1 call sites in 1 files\n"), "{out}");
    // The mentions are listed with their code, not left to a grep.
    assert!(
        out.contains(
            "references, not calls (what is called through them is not followed):
  mm/a.c:2 | int use_a(void) { register_cb(wake_task); return 0; }
  mm/b.c:1 |"
        ),
        "{out}"
    );
    assert!(!out.contains("rg -nw"), "{out}");
    // The callbacks are references: impact lists their users, and nothing is missing.
    let out = fox_code(&p, "wake_task", "impact", 800);
    assert!(out.contains(":2 use_a c:") && out.contains(":1 use_b c:") && !out.contains("partial"), "{out}");
    let out = fox_code(&p, "run", "callers", 800);
    assert!(
        !out.contains("may be incomplete")
            && out.starts_with("1 line names `run`: 0 calls, 1 definition; all accounted for in indexed C/C++/Objective-C/Swift code"),
        "{out}"
    );
}

/// Mentions that cannot be calls (a comment, an import, a string) are counted and listed by
/// group and do not make the answer read as incomplete.
#[test]
fn non_call_mentions_do_not_read_as_incomplete() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("src/plan.c", "int plan_query(int q) { return q; }\nint run(void) { return plan_query(1); }\n"),
        ("include/proto.h", "/* see plan_query */\nextern int plan_query_all(int q);\n#include \"plan_query.h\"\n"),
        ("src/hooks.c", "/* plan_query hook */\nint other(void) { return 0; } // see plan_query\n"),
        ("web/names.ts", "import { plan_query } from './x'\nconst label = 'plan_query'\n"),
    ]);
    let out = fox_code(&p, "plan_query", "callers", 800);
    assert!(!out.contains("may be incomplete"), "{out}");
    // The `#include "plan_query.h"` path is a string; files of another language (web/names.ts)
    // are not counted; a complete answer lists no extra rows.
    assert!(
        out.starts_with(
            "6 lines name `plan_query`: 1 call, 1 definition, 3 comments, 1 in strings (may be dynamic uses); all accounted for in indexed C/C++/Objective-C/Swift code\n"
        ),
        "{out}"
    );
    assert!(!out.contains("web/names.ts") && !out.contains("include/proto.h"), "{out}");
    let out = fox_code(&p, "plan_query", "impact", 800);
    assert!(!out.contains("partial"), "{out}");
    // A line that may call it reads as incomplete; the others are still accounted for.
    let (_h, _r, p) = inline_project(&[
        ("src/plan.c", "int plan_query(int q) { return q; }\nint run(void) { return plan_query(1); }\n"),
        ("include/proto.h", "#include \"plan_query.h\"\n"),
        ("mm/b.c", "#define USE_B() plan_query(2)\n"),
    ]);
    let out = fox_code(&p, "plan_query", "callers", 800);
    assert!(
        out.starts_with("4 lines name `plan_query`: 1 call, 1 definition, 1 in strings (may be dynamic uses), 1 unresolved; may be incomplete: 1 unresolved line may use it\n"),
        "{out}"
    );
    assert!(out.contains("unresolved mentions:\n  mm/b.c:1 | #define USE_B() plan_query(2)") && !out.contains("include/proto.h"), "{out}");
}

/// A first-level impact row shows the line that uses the target, not only the dependent's
/// definition line; a top-level use shows its line too.
#[test]
fn impact_rows_show_the_use_line() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("lib/err.py", "class HttpError:\n    def __init__(self, code):\n        self.code = code\n"),
        (
            "app/auth.py",
            "from lib.err import HttpError\n\ndef check(ctx):\n    if not ctx:\n        raise HttpError(401)\n    return ctx\n",
        ),
        ("app/main.py", "from lib.err import HttpError\n\nprint(HttpError(500))\n"),
    ]);
    let out = fox_code(&p, "HttpError", "impact", 2000);
    assert!(out.contains("  :3 check c:") && out.contains(" | :5 raise HttpError(401)"), "{out}");
    assert!(out.contains("  :3 (top) | print(HttpError(500))"), "{out}");
}

/// A Go call through an embedded field (`pl.Evaluator.Preempt()`) reaches the method of the type
/// the field is named after, not a same-name method of another type in the imported package.
#[test]
fn go_embedded_field_call_reaches_the_named_type() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "pkg/preemption/group.go",
            "package preemption\n\ntype GroupEvaluator struct{}\n\nfunc (g *GroupEvaluator) Preempt() int { return 0 }\n",
        ),
        (
            "pkg/preemption/preemption.go",
            "package preemption\n\ntype Evaluator struct{}\n\nfunc (ev *Evaluator) Preempt() int { return 1 }\n",
        ),
        (
            "pkg/plugins/defaultpreemption/plugin.go",
            "package defaultpreemption\n\nimport \"example.com/pkg/preemption\"\n\ntype groupEvaluator interface {\n\tPreempt() int\n}\n\ntype Plugin struct {\n\t*preemption.Evaluator\n\tgroup groupEvaluator\n}\n\nfunc (pl *Plugin) PostFilter() int { return pl.Evaluator.Preempt() + pl.group.Preempt() }\n",
        ),
    ]);
    let out = fox_code(&p, "Evaluator.Preempt", "callers", 800);
    assert!(out.contains("1 call sites in 1 files") && out.contains("Plugin.PostFilter"), "{out}");
    // `pl.group.Preempt()` (an interface field) is a tie: at most a guess here.
    let out = fox_code(&p, "GroupEvaluator.Preempt", "callers", 800);
    assert!(out.contains(" call sites in ") && certain_calls(&out) == 0, "{out}");
}

/// A one-word name whose other uses have no reference row (a function-pointer initializer, a
/// macro body) still gets the completeness line, from every identifier of every file, not only
/// from indexed symbol text.
#[test]
fn completeness_counts_one_word_names_and_uses_outside_symbols() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("lib/helper.c", "int helper(int x) { return x; }\n"),
        ("src/direct.c", "int direct(void) { return helper(1); }\n"),
        ("src/fp.c", "static int (*fp)(int) = helper;\n"),
        ("src/field.c", "struct o { int (*cb)(int); };\nvoid set(struct o *o) { o->cb = helper; }\n"),
        ("src/macro.c", "#define CALL(x) helper(x)\n"),
        ("src/arg.c", "int apply(int (*g)(int), int v);\nint a(void) { return apply(helper, 1); }\n"),
        ("src/other.c", "int helpers(void) { return 0; }\n"),
        ("lib/fs.c", "int simple_open(void) { return 0; }\n"),
        ("drivers/x.c", "#define FOPS(n) static const struct fops n = { .open = simple_open }\n"),
    ]);
    // The argument in src/arg.c and the member assignment in src/field.c are resolved references:
    // listed by `refs` and `impact`, still unknown to `callers` (what is called through them is
    // not followed).
    for (rel, head) in [
        (
            "callers",
            "6 lines name `helper`: 1 call, 1 definition, 2 references, not calls, 2 unresolved; may be incomplete: 2 unresolved lines",
        ),
        ("refs", "6 lines name `helper`: 3 refs, 1 definition, 2 unresolved; may be incomplete: 2 unresolved lines"),
        ("impact", "6 lines name `helper`: 3 uses (d1), 1 definition, 2 unresolved; partial at d1: 2 unresolved lines"),
    ] {
        let out = fox_code(&p, "helper", rel, 800);
        assert!(out.starts_with(head), "{rel}: {out}");
        assert!(
            out.contains(
                "unresolved mentions:\n  src/fp.c:1 | static int (*fp)(int) = helper;\n  src/macro.c:1 | #define CALL(x) helper(x)\n"
            ),
            "{rel}: {out}"
        );
    }
    let out = fox_code(&p, "simple_open", "refs", 800);
    assert!(out.starts_with("2 lines name `simple_open`: 0 refs, 1 definition, 1 unresolved; may be incomplete"), "{out}");
}

/// Directories named `target` are build output only beside a build manifest (or with a cache
/// tag); a source directory of that name (Linux `drivers/target`) is indexed.
#[test]
fn source_directories_named_target_are_indexed() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("drivers/target/core.c", "int target_core(void) { return 0; }\n"),
        ("tool/Cargo.toml", "[package]\nname = \"tool\"\n"),
        ("tool/target/debug/build/out.rs", "pub fn generated_only() {}\n"),
    ]);
    let out = fox_code(&p, "target_core", "src", 800);
    assert!(out.contains("drivers/target/core.c:1"), "{out}");
    let c = db::open_ro().unwrap();
    let out = code::run(&c, &p, "generated_only", &code::CodeOpts { rel: "src", depth: 2, budget: 800, cursor: 0, fallback: false });
    assert!(out.as_deref().is_ok_and(|o| !o.contains("out.rs")), "{out:?}");
}

/// A receiver named like a type in another file is no proof of the receiver's type: a one-word
/// method stays unresolved, a longer name is a guess.
#[test]
fn receiver_named_like_a_type_is_not_certain() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("a/State.kt", "package a\n\nclass State {\n    fun update() {}\n}\n\nclass PlaybackSession {\n    fun close() {}\n}\n"),
        (
            "b/Use.kt",
            "package b\n\nfun use() {\n    val state = MutableStateFlow(0)\n    state.update()\n    val session = android.media.session.MediaSession(null, \"x\")\n    session.close()\n}\n",
        ),
    ]);
    for q in ["State.update", "PlaybackSession.close"] {
        let out = fox_code(&p, q, "callers", 800);
        assert!(out.contains("0 call sites"), "{q}: {out}");
        let out = fox_code(&p, q, "impact", 800);
        assert!(out.contains(": 0 symbols"), "{q}: {out}");
    }
}

/// Standard-library method names (`toString`, `hashCode`, `is_empty`, `to_vec`) called through
/// a receiver are no guess at a project's own same-name method.
#[test]
fn library_method_names_are_not_guessed() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("a/Foo.kt", "package a\n\nclass Foo {\n    override fun toString(): String = \"f\"\n    override fun hashCode(): Int = 1\n}\n"),
        ("b/Use.kt", "package b\n\nfun use(sb: StringBuilder) {\n    val s = sb.toString()\n    val h = listOf(1).hashCode()\n}\n"),
        (
            "src/list.rs",
            "pub struct MyList;\n\nimpl MyList {\n    pub fn is_empty(&self) -> bool { true }\n    pub fn to_vec(&self) -> Vec<u8> { vec![] }\n}\n",
        ),
        ("src/ui/view.rs", "fn view(items: &Vec<u8>, v: String) -> bool {\n    let _ = items.to_vec();\n    v.is_empty()\n}\n"),
    ]);
    for q in ["Foo.toString", "Foo.hashCode", "MyList.is_empty", "MyList.to_vec"] {
        let out = fox_code(&p, q, "callers", 800);
        assert!(out.contains("0 call sites"), "{q}: {out}");
    }
}

/// A parameter or local in a keyed literal or designated initializer (`Name: name`,
/// `.size = size`) is not a reference to a same-name function; a function value still is.
#[test]
fn local_values_in_initializers_are_not_function_references() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "pkg/a/a.go",
            "package a\n\nfunc name() string { return \"n\" }\n\nfunc runIt() {}\n\ntype T struct {\n\tName string\n\tRun  func()\n}\n\nfunc mk() T {\n\tname := \"x\"\n\treturn T{Name: name, Run: runIt}\n}\n",
        ),
        ("pkg/a/c.go", "package a\n\nfunc use(name string) interface{} { return struct{ N string }{N: name} }\n"),
        ("lib/size.c", "int size(void) { return 1; }\nint open_dev(void) { return 0; }\n"),
        (
            "lib/ops.c",
            "struct ops { int size; int (*open)(void); };\nvoid f(int size) { struct ops o = { .size = size, .open = open_dev }; (void)o; }\n",
        ),
    ]);
    for q in ["name", "size"] {
        let out = fox_code(&p, q, "refs", 800);
        assert!(out.contains("0 refs"), "{q}: {out}");
    }
    assert!(fox_code(&p, "runIt", "refs", 800).contains("1 refs in 1 files"));
    assert!(fox_code(&p, "open_dev", "refs", 800).contains("1 refs in 1 files"));
}

/// A use at file top level (a C command table, a Python module-level call, a test registered
/// inside an anonymous callback) makes its file a dependent: the file is listed with its lines
/// as `(top)` and counted, and the walk stops there (no symbol to expand).
#[test]
fn impact_lists_top_level_uses_as_file_dependents() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("lib/run.c", "int run_one(int a) { return a; }\n"),
        ("builtin/add.c", "int cmd_add(int a) { return run_one(a); }\n"),
        ("git.c", "struct cmd { const char *n; int (*f)(int); };\nstatic struct cmd commands[] = {\n\t{ \"add\", cmd_add },\n};\n"),
        ("tools/fmt.py", "def fmt(x):\n    return x\n"),
        ("tools/main.py", "from tools.fmt import fmt\n\nprint(fmt(1))\n"),
        ("src/utils/color.ts", "export function getColor(): boolean {\n  return true\n}\n"),
        (
            "src/utils/color.test.ts",
            "import { getColor } from './color'\n\ndescribe('color', () => {\n  it('works', () => {\n    expect(getColor()).toBe(true)\n  })\n})\n",
        ),
    ]);
    let out = fox_code(&p, "run_one", "impact", 2000);
    assert!(out.contains("d1\n builtin/add.c\n  :1 cmd_add c:"), "{out}");
    assert!(out.contains("d2\n git.c\n  :3 (top)\n"), "{out}");
    assert!(out.contains(": 1 symbols, 1 files at top level; per depth 1/1"), "{out}");
    assert!(!out.contains("partial"), "a complete answer carries no warning: {out}");
    let out = fox_code(&p, "fmt", "impact", 2000);
    assert!(out.contains(" tools/main.py\n  :3 (top) | print(fmt(1))\n"), "{out}");
    let out = fox_code(&p, "getColor", "impact", 2000);
    assert!(out.contains(" src/utils/color.test.ts\n  :5 (top) | expect(getColor()).toBe(true)\n"), "{out}");
}

/// A function passed as an argument (a C callback, a Go function value, a Python bound method
/// or keyword callback, a JS handler) is a reference, so its user is a dependent; a parameter
/// or local of the same name passed along is not.
#[test]
fn functions_passed_as_arguments_are_dependents() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "revision.c",
            "static int handle_one(const char *r, void *cb) { return 0; }\nint each_ref(int (*fn)(const char *, void *), void *cb);\nint add_all(void *cb) { return each_ref(handle_one, cb); }\nint pass_on(void *handle_one) { return each_ref(0, handle_one); }\n",
        ),
        (
            "pkg/a/a.go",
            "package a\n\nfunc worker() {}\n\nfunc Until(f func()) {}\n\nfunc Run() { Until(worker) }\n\nfunc Other(worker func()) { Until(worker) }\n",
        ),
        (
            "ext/logcount.py",
            "class LogCount:\n    @classmethod\n    def from_crawler(cls, crawler):\n        o = cls()\n        crawler.signals.connect(o.spider_opened, signal=1)\n        crawler.engine.schedule(callback=o.spider_closed)\n        return o\n\n    def spider_opened(self, spider):\n        pass\n\n    def spider_closed(self, spider):\n        pass\n",
        ),
        ("src/handler.ts", "export function onError(e: Error) {\n  return e\n}\n"),
        (
            "src/app.ts",
            "import { onError } from './handler'\n\nexport function setup(app: any) {\n  app.onError(onError)\n}\n\nexport function relay(onError: any) {\n  return [onError]\n}\n",
        ),
    ]);
    let out = fox_code(&p, "handle_one", "impact", 2000);
    assert!(out.contains(":3 add_all c:") && !resolved(&out).contains("pass_on"), "{out}");
    let out = fox_code(&p, "worker", "impact", 2000);
    assert!(out.contains(":7 Run c:") && !resolved(&out).contains("Other"), "{out}");
    let out = fox_code(&p, "spider_opened", "impact", 2000);
    assert!(out.contains(":3 LogCount.from_crawler c:"), "{out}");
    let out = fox_code(&p, "spider_closed", "impact", 2000);
    assert!(out.contains(":3 LogCount.from_crawler c:"), "{out}");
    let out = fox_code(&p, "onError", "impact", 2000);
    assert!(out.contains(":3 setup c:") && !resolved(&out).contains("relay"), "{out}");
}

/// Dispatch: a call through an interface (or base) method may reach each implementation, so
/// impact of an implementation lists the interface's callers, and impact of an interface method
/// lists its implementations and their callers. Go types implement interfaces by their method
/// sets; TypeScript and Python name their supertypes.
#[test]
fn impact_follows_interface_dispatch() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("src/router.ts", "export interface Router {\n  add(path: string): void\n  match(path: string): string\n}\n"),
        (
            "src/router/linear.ts",
            "import { Router } from '../router'\n\nexport class LinearRouter implements Router {\n  add(path: string) {}\n  match(path: string) { return path }\n}\n",
        ),
        ("src/router/use.ts", "import { LinearRouter } from './linear'\n\nexport function build() {\n  new LinearRouter().add('/')\n}\n"),
        (
            "src/server.ts",
            "import { Router } from './router'\n\nexport class Server {\n  router: Router\n  route(p: string) {\n    this.router.add(p)\n  }\n}\n",
        ),
        (
            "pkg/cfg/flags.go",
            "package cfg\n\ntype Getter interface {\n\tToConfig() int\n\tToMapper() int\n}\n\ntype Flags struct{}\n\nfunc (f *Flags) ToConfig() int { return 1 }\n\nfunc (f *Flags) ToMapper() int { return 2 }\n\ntype Partial struct{}\n\nfunc (p *Partial) ToConfig() int { return 3 }\n",
        ),
        ("pkg/cmd/run.go", "package cmd\n\nimport \"example.com/pkg/cfg\"\n\nfunc Run(g cfg.Getter) int { return g.ToConfig() }\n"),
        ("src/store.ts", "export interface Store {\n  save(): void\n}\n"),
        ("src/service.ts", "import { Store } from './store'\n\nexport class Service {\n  store: Store\n  save() {}\n}\n"),
        ("src/persist.ts", "import { Store } from './store'\n\nexport function persist(s: Store) {\n  s.save()\n}\n"),
        ("ext/base.py", "class Base:\n    def close(self):\n        pass\n"),
        ("ext/disk.py", "from ext.base import Base\n\n\nclass Disk(Base):\n    def close(self):\n        pass\n"),
        ("ext/use.py", "from ext.base import Base\n\n\ndef shutdown(b: Base):\n    Base.close(b)\n"),
    ]);
    // An implementation: the interface's callers.
    let out = fox_code(&p, "LinearRouter.add", "impact", 2000);
    assert!(out.contains(":5 Server.route c:") && out.contains(" via Router.add"), "{out}");
    assert!(out.contains(":3 build c:"), "{out}");
    // The interface method: its implementations, then their callers.
    let out = fox_code(&p, "Router.add", "impact", 2000);
    assert!(out.contains(":4 LinearRouter.add c:") && out.contains(" implements Router.add"), "{out}");
    assert!(out.contains(":5 Server.route c:") && out.contains(":3 build c:"), "{out}");
    // Go: Flags has every method of Getter, Partial does not.
    let out = fox_code(&p, "Flags.ToConfig", "impact", 2000);
    assert!(out.contains(":5 Run c:") && out.contains(" via Getter.ToConfig"), "{out}");
    let out = resolved(&fox_code(&p, "Partial.ToConfig", "impact", 2000));
    assert!(!out.contains("Run"), "{out}");
    let out = resolved(&fox_code(&p, "Getter.ToConfig", "impact", 2000));
    assert!(out.contains("Flags.ToConfig") && !out.contains("Partial.ToConfig"), "{out}");
    // A type named in a class body (a field's type) is no supertype.
    let out = resolved(&fox_code(&p, "Service.save", "impact", 2000));
    assert!(!out.contains("persist"), "{out}");
    // Python: a subclass override is reached through calls to the base method.
    let out = fox_code(&p, "Disk.close", "impact", 2000);
    assert!(out.contains(":4 shutdown c:") && out.contains(" via Base.close"), "{out}");
}

/// The walk says when it may have missed something: it stopped past the deepest level it walks
/// while dependents remain, or a call named like a dependent was guessed to another same-name
/// definition. A walk that ends on its own, or whose guesses all land on its own rows, says
/// nothing.
#[test]
fn impact_says_when_the_walk_was_cut_or_guessed() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "chain.c",
            "int f0(void) { return 0; }\nint f1(void) { return f0(); }\nint f2(void) { return f1(); }\nint f3(void) { return f2(); }\nint f4(void) { return f3(); }\nint f5(void) { return f4(); }\n",
        ),
        ("lib/core.c", "int base(void) { return 0; }\n"),
        // Sorts after y/: the z/ tie breaks to y/b.c's `pick_one`, not this one.
        ("zz/a.c", "int pick_one(void) { return base(); }\n"),
        ("y/b.c", "int pick_one(void) { return 1; }\n"),
        ("y/sub/use.c", "int user(void) { return pick_one(); }\n"),
        ("z/use.c", "int other(void) { return pick_one(); }\n"),
    ]);
    let c = db::open_ro().unwrap();
    let run = |sym: &str, depth: usize| {
        code::run(&c, &p, sym, &code::CodeOpts { rel: "impact", depth, budget: 2000, cursor: 0, fallback: false }).unwrap()
    };
    let out = run("f0", 6);
    assert!(out.contains("\npartial at d4: dependents continue past depth 4"), "{out}");
    let out = run("f0", 4);
    assert!(!out.contains("partial"), "the depth asked for is no cut: {out}");
    let out = run("f3", 4);
    assert!(!out.contains("partial"), "{out}");
    // `pick_one` has two definitions: the call in y/sub/ is guessed to the one nearer it (no
    // tie, so not ours); the call in z/ is a tie between both, so it may still reach `base`.
    let out = run("base", 2);
    assert!(out.contains(":1 pick_one c:") && !out.contains("user c:") && !out.contains("other c:"), "{out}");
    assert!(
        out.contains("\npartial at d2: calls in 1 other file were guessed to another `pick_one`; they may reach a d1 dependent"),
        "{out}"
    );
    // The guessed call, as a dependent: marked, and nothing is missing.
    let dst: i64 = c.query_row("SELECT dst FROM ts_ref WHERE path = 'y/sub/use.c' AND name = 'pick_one'", [], |r| r.get(0)).unwrap();
    let out = run(&format!("c:{dst:x}"), 2);
    assert!(out.contains(":1 user c:") && out.contains("guess 1 of 2") && !out.contains("partial at d2"), "{out}");
}

/// Go: a call through a package name reaches the package its import path names, not a
/// same-name function in another package with the same last directory.
#[test]
fn go_package_calls_follow_the_full_import_path() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("pkg/a/stats/stats.go", "package stats\n\nfunc Register() {}\n"),
        ("pkg/b/stats/stats.go", "package stats\n\nfunc Register() {}\n"),
        ("pkg/c/stats/stats.go", "package stats\n\nfunc Register() {}\n"),
        ("pkg/b/ctl.go", "package b\n\nimport \"example.com/pkg/b/stats\"\n\nfunc NewController() { stats.Register() }\n"),
    ]);
    let c = db::open_ro().unwrap();
    let id = |dir: &str| -> i64 {
        c.query_row("SELECT id FROM node WHERE name = 'Register' AND path = ?1", [format!("pkg/{dir}/stats/stats.go")], |r| r.get(0))
            .unwrap()
    };
    let out = fox_code(&p, &format!("c:{:x}", id("b")), "callers", 800);
    assert!(out.contains("1 call sites in 1 files") && out.contains("NewController"), "{out}");
    for other in ["a", "c"] {
        let out = fox_code(&p, &format!("c:{:x}", id(other)), "callers", 800);
        assert!(out.contains("0 call sites"), "{other}: {out}");
    }
}

/// Finding a function's parameters and locals is linear in its size: a function nested
/// thousands of levels deep (a long `else if` chain) indexes in about the time of a flat one.
#[test]
fn deeply_nested_functions_index_in_linear_time() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut body = String::from("int complete(const char *t, int n)\n{\n  if (match(t, \"A\"))\n    n = take(n, 0);\n");
    for i in 0..3000 {
        body += &format!("  else if (match(t, \"A{i}\"))\n    n = take(n, {i});\n");
    }
    body += "  return n;\n}\nint match(const char *a, const char *b) { return 0; }\nint take(int a, int b) { return a; }\n";
    let t = std::time::Instant::now();
    let (_h, _r, p) = inline_project(&[("src/tab.c", body.as_str())]);
    let took = t.elapsed();
    assert!(took < std::time::Duration::from_secs(4), "indexing took {took:?}");
    let out = fox_code(&p, "take", "callers", 400);
    assert!(out.contains("3001 call sites in 1 files"), "{out}");
}

/// Go dispatch needs evidence beyond matching names: the interface in the same directory as
/// the type, or a file of the type's package that imports the interface's package and names
/// the interface. A one-method interface elsewhere links nothing on its name alone. Callers of
/// the interface method found by name guesses are listed but not followed further.
#[test]
fn go_dispatch_needs_evidence_and_does_not_multiply_guesses() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("pkg/utilnet/net.go", "package utilnet\n\ntype Closeable interface {\n\tClose() error\n}\n"),
        ("pkg/web/writer.go", "package web\n\ntype Writer struct{}\n\nfunc (w *Writer) Close() error { return nil }\n"),
        (
            "pkg/store/conn.go",
            "package store\n\nimport \"example.com/pkg/utilnet\"\n\ntype Conn struct{}\n\nfunc (c *Conn) Close() error { return nil }\n\nvar _ utilnet.Closeable = (*Conn)(nil)\n",
        ),
        ("pkg/app/use.go", "package app\n\nimport \"example.com/pkg/utilnet\"\n\nfunc Shut(c utilnet.Closeable) { c.Close() }\n"),
        ("pkg/app/main.go", "package app\n\nfunc Main() { Shut(nil) }\n"),
    ]);
    let out = resolved(&fox_code(&p, "Writer.Close", "impact", 2000));
    assert!(!out.contains("Shut") && !out.contains("via"), "{out}");
    let out = fox_code(&p, "Conn.Close", "impact", 2000);
    assert!(out.contains(":5 Shut c:") && out.contains(" via Closeable.Close"), "{out}");
    // Shut reached Close only through a name guess: it is not followed to Main.
    assert!(!out.contains("Main"), "{out}");
    let out = fox_code(&p, "Closeable.Close", "impact", 2000);
    assert!(out.contains("Conn.Close") && !resolved(&out).contains("Writer.Close"), "{out}");
}

/// Go method sets include embedded interfaces; an interface embedding one that is not in the
/// project is skipped, not treated as smaller. A capped candidate list says so.
#[test]
fn go_interface_method_sets_expand_embeds_and_caps_are_reported() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut many = String::from("package many\n\ntype Stopper interface {\n\tStop() error\n}\n");
    for i in 0..320 {
        many += &format!("\ntype T{i} struct{{}}\n\nfunc (T{i}) Stop() error {{ return nil }}\n");
    }
    let (_h, _r, p) = inline_project(&[
        (
            "pkg/rw/rw.go",
            "package rw\n\nimport \"io\"\n\ntype Reader interface {\n\tRead() int\n}\n\ntype ReadCloser interface {\n\tReader\n\tClose() error\n}\n\ntype IOCloser interface {\n\tio.Reader\n\tClose() error\n}\n\ntype File struct{}\n\nfunc (f *File) Close() error { return nil }\n\ntype Pipe struct{}\n\nfunc (p *Pipe) Close() error { return nil }\n\nfunc (p *Pipe) Read() int { return 0 }\n",
        ),
        ("pkg/many/many.go", many.as_str()),
    ]);
    let out = fox_code(&p, "ReadCloser.Close", "impact", 2000);
    assert!(out.contains("Pipe.Close") && !resolved(&out).contains("File.Close"), "{out}");
    let out = resolved(&fox_code(&p, "IOCloser.Close", "impact", 2000));
    assert!(!out.contains("Pipe.Close") && !out.contains("File.Close"), "{out}");
    // More same-name methods than the name lookup lists: ask by handle.
    let id: i64 = db::open_ro()
        .unwrap()
        .query_row("SELECT id FROM node WHERE name = 'Stop' AND json_extract(meta, '$.parent') = 'Stopper'", [], |r| r.get(0))
        .unwrap();
    let out = fox_code(&p, &format!("c:{id:x}"), "impact", 400);
    assert!(out.contains("\npartial at d1: dispatch checked the first 300 same-name methods"), "{out}");
}

/// Parameters and locals in every declaration form are not references to a same-name
/// function, and a bare name never resolves to a method in Python or JS.
#[test]
fn locals_in_every_declaration_form_are_not_references() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "lk/m.go",
            "package m\n\nfunc v() int { return 1 }\n\nfunc run(ch chan int, x interface{}) {\n\tselect {\n\tcase v := <-ch:\n\t\tuse(v)\n\t}\n\tswitch v := x.(type) {\n\tdefault:\n\t\tuse(v)\n\t}\n}\n\nfunc use(a interface{}) {}\n",
        ),
        (
            "lk/m.js",
            "class Ctx {\n  get id() { return 1; }\n  args() { return 2; }\n  err() { return 3; }\n}\nfunction run(params, ...args) {\n  const { script: id } = params;\n  log(id);\n  log(args);\n  try { log(1); } catch (err) { log(err); }\n}\nfunction log(x) { return x; }\n",
        ),
        (
            "lk/m.py",
            "class Rec:\n    @property\n    def name(self):\n        return \"n\"\n\n    def args(self):\n        return 1\n\ndef show(names, *args, **kwargs):\n    print([emit(name) for name in names])\n    emit(args)\n    emit(kwargs)\n    if (n := len(names)):\n        emit(n)\n\ndef emit(x):\n    return x\n",
        ),
        ("pkg/conf.py", "def path(route, view):\n    return (route, view)\n\ndef item():\n    return 1\n"),
        (
            "pkg/use.py",
            "from pkg.conf import path\n\ndef resolve_all(paths, items):\n    out = [resolve(path) for path in paths]\n    with open(\"x\") as item:\n        consume(item)\n    try:\n        pass\n    except Exception as path2:\n        log(path2)\n    return out\n\ndef resolve(p):\n    return p\n\ndef consume(x):\n    pass\n\ndef log(x):\n    pass\n\nfor item in [1, 2]:\n    consume(item)\n",
        ),
        (
            "src/handle.c",
            "int handle(int x) { return x; }\nint walk(int (*cb)(int), int v);\nint run(int handle, int v)\n{\n#ifdef FAST\n  return walk(handle, v);\n#else\n  return walk(handle, v + 1);\n#endif\n}\n",
        ),
    ]);
    for q in ["v", "Ctx.id", "Ctx.args", "Ctx.err", "Rec.name", "Rec.args", "handle"] {
        let out = fox_code(&p, q, "refs", 800);
        assert!(out.contains("0 refs"), "{q}: {out}");
    }
    let out = resolved(&fox_code(&p, "path", "refs", 800));
    assert!(!out.contains(":4 "), "{out}");
    let out = resolved(&fox_code(&p, "item", "refs", 800));
    assert!(!out.contains(":6 ") && !out.contains(":23 "), "{out}");
}

/// A member value passed as an argument (`log(request.id)`) is not a certain reference to a
/// same-file member of another type; through `this` or `self` it is.
#[test]
fn member_values_are_not_same_file_members_of_other_types() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "src/m.js",
            "class Ctx {\n  get id() { return 1; }\n  show() { log(this.id); }\n}\nfunction handle(request) {\n  log(request.id);\n}\nfunction log(x) { return x; }\n",
        ),
        (
            "app/m.py",
            "class Rec:\n    def name(self):\n        return 1\n\ndef show(obj):\n    emit(obj.name)\n\ndef emit(x):\n    return x\n",
        ),
    ]);
    let out = fox_code(&p, "Ctx.id", "refs", 800);
    assert!(out.contains(":3 Ctx.show") && !out.contains("handle"), "{out}");
    let out = fox_code(&p, "Rec.name", "refs", 800);
    assert!(out.contains("0 refs"), "{out}");
}

/// Supertypes named on a later line than the class name count (`export class Multi\n
/// implements Router`, `class A(\n    Base)`), and plain JS `extends` gives dispatch too.
#[test]
fn supertypes_on_later_lines_and_js_extends_dispatch() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "src/iface.ts",
            "export interface Router {\n  route(p: string): void;\n}\nexport class Multi\n  implements Router {\n  route(p: string): void {}\n}\n",
        ),
        ("ext/base.py", "class Base:\n    def run(self):\n        pass\n\n    def start(self):\n        self.run()\n"),
        ("ext/a.py", "from ext.base import Base\n\n\nclass A(\n    Base,\n):\n    def run(self):\n        pass\n"),
        ("js/base.js", "export class Base {\n  run() { return 1; }\n}\nexport function go(base) { return base.run(); }\n"),
        ("js/child.js", "import { Base } from './base';\nexport class Child extends Base {\n  run() { return 2; }\n}\n"),
    ]);
    let out = fox_code(&p, "Router.route", "impact", 2000);
    assert!(out.contains("Multi.route") && out.contains(" implements Router.route"), "{out}");
    let out = fox_code(&p, "A.run", "impact", 2000);
    assert!(out.contains("Base.start c:") && out.contains(" via Base.run"), "{out}");
    let out = fox_code(&p, "Child.run", "impact", 2000);
    assert!(out.contains(":4 go c:") && out.contains(" via Base.run"), "{out}");
}

/// Dispatch in Python and JS follows calls through `self`/`this` or a receiver named like the
/// base type, not a call on some other object that a name guess put on the base method.
#[test]
fn dynamic_dispatch_ignores_calls_on_other_objects() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "db/base.py",
            "class BaseDatabaseWrapper:\n    def close(self):\n        pass\n\n    def _close(self):\n        self.connection.close()\n\n    def __del__(self):\n        self.close()\n",
        ),
        (
            "db/sqlite.py",
            "from db.base import BaseDatabaseWrapper\n\n\nclass DatabaseWrapper(BaseDatabaseWrapper):\n    def close(self):\n        pass\n",
        ),
    ]);
    let out = fox_code(&p, "DatabaseWrapper.close", "impact", 2000);
    assert!(out.contains("__del__") && !out.contains("_close"), "{out}");
}

/// C and C++ constants and macros passed as arguments (`elog(ERROR, ...)`) are no references.
#[test]
fn c_all_caps_arguments_are_not_stored() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[(
        "src/e.c",
        "void elog(int level, const char *m);\nint f(void) { elog(ERROR, \"x\"); elog(LOG_LEVEL, \"y\"); return 0; }\n",
    )]);
    let c = db::open_ro().unwrap();
    let n: i64 =
        c.query_row("SELECT COUNT(*) FROM ts_ref WHERE project_id = ?1 AND name IN ('ERROR', 'LOG_LEVEL')", [p.id], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
}

/// A name declared at file level hides an argument only when it is a plain variable: a name
/// destructured from `require(...)` is an import, and a module-level rebinding of a definition
/// (`main = click.command(main)`) is still that definition.
#[test]
fn file_level_imports_and_rebindings_are_not_locals() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("js2/errors.js", "function onError(err) {\n  console.error(err);\n}\nmodule.exports = { onError };\n"),
        (
            "js2/server.js",
            "const express = require(\"express\");\nconst { onError } = require(\"./errors\");\n\nconst app = express();\napp.use(onError);\n\nfunction start() {\n  app.use(onError);\n}\nmodule.exports = { start };\n",
        ),
        (
            "py3/cli.py",
            "import click\n\n\ndef main(path):\n    return path\n\n\nmain = click.command(main)\n\n\ndef run():\n    dispatch(main)\n\n\ndef dispatch(f):\n    return f\n",
        ),
    ]);
    let out = fox_code(&p, "onError", "impact", 2000);
    assert!(out.contains(":7 start c:") && out.contains(":5 (top)"), "{out}");
    let c = db::open_ro().unwrap();
    let id: i64 = c.query_row("SELECT id FROM node WHERE name = 'main' AND path = 'py3/cli.py' AND line = 4", [], |r| r.get(0)).unwrap();
    let out = fox_code(&p, &format!("c:{id:x}"), "impact", 2000);
    assert!(out.contains(":11 run c:"), "{out}");
}

/// A parameter hides a name only inside its own function, lambda or comprehension, not in the
/// rest of the enclosing function.
#[test]
fn nested_parameters_hide_names_only_in_their_own_scope() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "py1/app.py",
            "def handler(event):\n    return event\n\n\ndef register(cb):\n    return cb\n\n\ndef setup(items):\n    register(handler)\n    return sorted(items, key=lambda handler: handler.priority)\n",
        ),
        ("js1/errors.js", "function onError(err) {\n  console.error(err);\n}\nmodule.exports = { onError };\n"),
        (
            "js1/server.test.js",
            "import { onError } from \"./errors\";\n\ndescribe(\"server\", () => {\n  it(\"logs failures\", (done) => {\n    app.use(onError);\n    done();\n  });\n\n  it(\"wraps\", () => {\n    wrap((onError) => onError);\n  });\n});\n",
        ),
        (
            "go2/main.go",
            "package main\n\nfunc worker() {}\n\nfunc until(f func()) { f() }\n\nfunc main() {\n\tuntil(worker)\n\teach(func(worker func()) { worker() })\n}\n\nfunc each(f func(func())) { f(nil) }\n",
        ),
    ]);
    let out = fox_code(&p, "handler", "impact", 2000);
    assert!(out.contains(":9 setup c:"), "{out}");
    let out = fox_code(&p, "onError", "impact", 2000);
    assert!(out.contains(" js1/server.test.js\n  :5 (top)"), "{out}");
    let out = fox_code(&p, "worker", "impact", 2000);
    assert!(out.contains(":7 main c:"), "{out}");
}

/// A supertype is followed for dispatch only when its link is certain: a base class imported
/// from a package that re-exports it names that package's class, and a type imported from a
/// module outside the project is not a same-name project interface.
#[test]
fn supertypes_follow_explicit_imports() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("py5/pkg/__init__.py", ""),
        ("py5/pkg/core/__init__.py", "from pkg.core.base import Base\n"),
        ("py5/pkg/core/base.py", "class Base:\n    def run(self):\n        return 2\n\n    def go(self):\n        return self.run()\n"),
        ("py5/pkg/legacy/__init__.py", ""),
        (
            "py5/pkg/legacy/base.py",
            "class Base:\n    def run(self):\n        return 1\n\n    def start(self):\n        return self.run()\n",
        ),
        ("py5/pkg/legacy/child.py", "from pkg.core import Base\n\n\nclass Child(Base):\n    def run(self):\n        return 3\n"),
        (
            "ts1/src/ext.ts",
            "import { Handler } from \"some-external-lib\";\n\nexport class ExtHandler implements Handler {\n  handle(req: string): void {\n    console.log(req);\n  }\n}\n",
        ),
        (
            "ts1/src/local.ts",
            "export interface Handler {\n  handle(req: string): void;\n}\n\nexport function useLocal(h: Handler) {\n  h.handle(\"x\");\n}\n",
        ),
    ]);
    let out = fox_code(&p, "Child.run", "impact", 2000);
    assert!(!out.contains("Base.start"), "{out}");
    // `h.handle()` is a tie between the local interface and `ExtHandler`: at most a guess.
    let out = fox_code(&p, "ExtHandler.handle", "impact", 2000);
    assert!(out.lines().filter(|l| l.contains("useLocal")).all(|l| l.contains("guess")), "{out}");
}

/// Go: a standard library import (`"log"`) is no evidence for, nor a call into, a project
/// package with the same last directory (`internal/log`); a local variable named like an
/// imported package (`user := ...; user.Save()`) is no package qualifier.
#[test]
fn go_stdlib_imports_and_local_receivers_are_not_project_packages() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "go3/internal/log/log.go",
            "package log\n\ntype Logger interface {\n\tPrintf(format string, args ...any)\n}\n\nfunc Use(l Logger) {\n\tl.Printf(\"hello\")\n}\n",
        ),
        (
            "go3/app/app.go",
            "package app\n\nimport \"log\"\n\nvar std *log.Logger\n\ntype quiet struct{}\n\nfunc (q quiet) Printf(format string, args ...any) {}\n",
        ),
        (
            "go1/handler.go",
            "package main\n\nimport (\n\t\"example.com/m/models\"\n\t\"example.com/m/user\"\n)\n\nfunc touch() error {\n\treturn user.Save()\n}\n\nfunc update(name string) error {\n\tuser := &models.User{Name: name}\n\treturn user.Save()\n}\n",
        ),
        ("go1/models/user.go", "package models\n\ntype User struct{ Name string }\n\nfunc (u *User) Save() error {\n\treturn nil\n}\n"),
        ("go1/user/user.go", "package user\n\nfunc Save() error {\n\treturn nil\n}\n"),
    ]);
    let out = resolved(&fox_code(&p, "Logger.Printf", "impact", 2000));
    assert!(!out.contains("quiet.Printf"), "{out}");
    let out = fox_code(&p, "quiet.Printf", "impact", 2000);
    assert!(!out.contains("Use"), "{out}");
    let out = fox_code(&p, "User.Save", "impact", 2000);
    assert!(out.contains("update"), "{out}");
    let c = db::open_ro().unwrap();
    let id: i64 = c.query_row("SELECT id FROM node WHERE name = 'Save' AND path = 'go1/user/user.go'", [], |r| r.get(0)).unwrap();
    let out = fox_code(&p, &format!("c:{id:x}"), "callers", 800);
    assert!(out.contains("touch") && !out.contains("update"), "{out}");
}

/// Python: a base class imported from a package that re-exports it (`from dj.db.models import
/// Field`) names the package's class, with certainty, not a same-name test class.
#[test]
fn python_base_classes_follow_package_reexports() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("dj/__init__.py", ""),
        ("dj/db/__init__.py", ""),
        ("dj/db/models/__init__.py", "from dj.db.models.fields import Field\n"),
        ("dj/db/models/fields/__init__.py", "class Field:\n    def clean(self):\n        pass\n"),
        ("tests/model_fields/tests.py", "class Field:\n    pass\n"),
        ("tests/forms/tests.py", "class Field:\n    pass\n"),
        ("dj/contrib/pg/array.py", "from dj.db.models import Field\n\n\nclass ArrayField(Field):\n    pass\n"),
    ]);
    let c = db::open_ro().unwrap();
    let id: i64 =
        c.query_row("SELECT id FROM node WHERE name = 'Field' AND path = 'dj/db/models/fields/__init__.py'", [], |r| r.get(0)).unwrap();
    let out = fox_code(&p, &format!("c:{id:x}"), "impact", 2000);
    assert!(out.contains("ArrayField c:") && !out.contains("guess"), "{out}");
}

/// A guess among many same-name definitions (`response.close()` with a dozen `close` methods
/// in the project) is no dependent or caller: it is counted, not listed.
#[test]
fn weak_guesses_are_counted_not_listed() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut files: Vec<(String, String)> =
        (0..12).map(|i| (format!("m{i}/conn.py"), format!("class Conn{i}:\n    def close(self):\n        pass\n"))).collect();
    files.push(("t/test_resp.py".into(), "from shared import conn\n\n\ndef test_close(response):\n    response.close()\n".into()));
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let (_h, _r, p) = inline_project(&refs);
    let c = db::open_ro().unwrap();
    let (dst, amb): (i64, i64) = c
        .query_row("SELECT dst, amb FROM ts_ref WHERE path = 't/test_resp.py' AND name = 'close'", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert!(dst != 0 && amb > 8, "the call is a guess among many: {dst} {amb}");
    let out = fox_code(&p, &format!("c:{dst:x}"), "callers", 800);
    assert!(!out.contains("test_close") && out.contains("1 weak guess left out"), "{out}");
    let out = fox_code(&p, &format!("c:{dst:x}"), "impact", 800);
    assert!(!out.contains("test_close") && out.contains("1 weak guess left out"), "{out}");
    assert!(out.contains("\npartial at d1: 1 call guessed among many same-name definitions was left out"), "{out}");
}

/// A definition inside a function is visible only inside it, a parameter or local of the same
/// name shadows a call too, and a name imported from a module outside the project (`json`)
/// names none of the project's definitions.
#[test]
fn nested_definitions_locals_and_outside_imports_do_not_claim_calls() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "py4/m.py",
            "def parse(s):\n    return s\n\ndef load2(parse):\n    return parse(\"y\")\n\ndef load3():\n    parse = make()\n    return parse(\"z\")\n\ndef load4():\n    return [parse(x) for parse in []]\n\ndef make():\n    return None\n\ndef real(s):\n    return parse(s)\n",
        ),
        ("py4/n.py", "from json import loads as parse\ndef load(s):\n    return parse(s)\n"),
        ("py4/o.py", "from json import parse\ndef loadx(s):\n    return parse(s)\n"),
        (
            "py3/svc.py",
            "from json import loads as parse\n\ndef helper():\n    def parse(s):\n        return s.strip()\n    return parse(\" x \")\n\ndef load(s):\n    return parse(s)\n",
        ),
        (
            "feed/gen.py",
            "class Feed:\n    def __init__(self):\n        def to_str(s):\n            return s\n        self.t = to_str(1)\n\n    def add_item(self):\n        def to_str(s):\n            return str(s)\n        return to_str(2)\n",
        ),
        (
            "js/app/use.js",
            "function parseIt(x) { return x; }\nfunction u4() { const parseIt = (z) => z; return parseIt(4); }\nfunction u5() { try { } catch (parseIt) { return parseIt(5); } }\nfunction u6() { return parseIt(6); }\n",
        ),
    ]);
    let out = fox_code(&p, "py4/m.py", "outline", 800);
    let id = |out: &str, line: &str| -> String {
        out.lines().find(|l| l.starts_with(line)).unwrap().split_whitespace().find(|w| w.starts_with("c:")).unwrap().to_string()
    };
    let parse = id(&out, "1 function parse");
    let out = fox_code(&p, &parse, "callers", 800);
    assert!(out.contains("1 call sites in 1 files") && out.contains(":18 real c:"), "{out}");
    let out = fox_code(&p, "helper.parse", "callers", 800);
    assert!(out.contains("1 call sites") && out.contains(":6 helper c:"), "{out}");
    let out = fox_code(&p, "Feed.__init__.to_str", "callers", 800);
    assert!(out.contains("1 call sites") && out.contains(":5 Feed.__init__"), "{out}");
    let out = fox_code(&p, "js/app/use.js", "outline", 800);
    let parse_it = id(&out, "1 function parseIt");
    let out = fox_code(&p, &parse_it, "callers", 800);
    assert!(out.contains("1 call sites") && out.contains(":4 u6 c:"), "{out}");
}

/// Go: a qualifier is the name the file uses for an import (its alias, else its last
/// segment): the standard library `log.Printf` is not `internal/log.Printf` even when the file
/// imports that package under another name; a local closure shadows a bare call.
#[test]
fn go_qualifiers_follow_import_aliases() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("go.mod", "module example.com/p2\n\ngo 1.21\n"),
        ("internal/log/log.go", "package log\n\nfunc Printf(f string) {}\n"),
        (
            "cmd/x/main.go",
            "package main\n\nimport (\n\t\"log\"\n\tilog \"example.com/p2/internal/log\"\n)\n\nfunc stdonly() { log.Printf(\"x\") }\nfunc proj() { ilog.Printf(\"z\") }\nfunc local() {\n\tPrintf := func(s string) {}\n\tPrintf(\"y\")\n}\n",
        ),
    ]);
    let out = fox_code(&p, "Printf", "callers", 800);
    assert!(out.contains("1 call sites in 1 files") && out.contains(":9 proj c:"), "{out}");
}

// ---- first answers (round 3 transcripts) ----

/// A Go target qualified by its package (`pod.GetContainerStatus`), its path in the repo
/// (`pkg/api/v1/pod.GetContainerStatus`) or its full import path resolves to that package's
/// function, not to a same-name function of another package or a method.
#[test]
fn go_package_qualified_targets_resolve() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("go.mod", "module example.com/k\n\ngo 1.21\n"),
        ("pkg/api/v1/pod/util.go", "package pod\n\nfunc GetContainerStatus(name string) (int, bool) { return 0, true }\n"),
        ("pkg/api/pod/util.go", "package pod\n\nfunc GetContainerStatus(name string) (int, bool) { return 1, true }\n"),
        ("pkg/kubelet/rt/m.go", "package rt\n\ntype mgr struct{}\n\nfunc (m *mgr) GetContainerStatus() int { return 2 }\n"),
        (
            "pkg/kubelet/pods.go",
            "package kubelet\n\nimport podutil \"example.com/k/pkg/api/v1/pod\"\n\nfunc getPhase() { podutil.GetContainerStatus(\"a\") }\n",
        ),
    ]);
    for target in ["pkg/api/v1/pod.GetContainerStatus", "example.com/k/pkg/api/v1/pod.GetContainerStatus", "v1/pod.GetContainerStatus"] {
        let out = fox_code(&p, target, "callers", 800);
        assert!(out.contains("function GetContainerStatus  pkg/api/v1/pod/util.go:3"), "{target}: {out}");
        assert!(out.contains("1 call sites in 1 files") && out.contains("getPhase"), "{target}: {out}");
    }
    // The package name alone: one of the two `pod` packages, never the method.
    let out = fox_code(&p, "pod.GetContainerStatus", "callers", 800);
    assert!(out.contains("callers of ~c:") && out.contains("function GetContainerStatus  pkg/api/"), "{out}");
    // fox_find takes the same qualified name.
    let c = db::open_ro().unwrap();
    let o = foxdb::query::FindOpts { kind: "any", all: false, budget: 800, limit: 10, offset: 0 };
    let out = foxdb::query::find(&c, Some(&p), "pkg/api/v1/pod.GetContainerStatus", &o).unwrap();
    assert!(out.lines().next().unwrap_or("").contains("pkg/api/v1/pod/util.go:3"), "{out}");
}

/// `overrides` of a method lists the same-name methods of its subclasses, at any depth, and the
/// method it overrides; a same-name method of an unrelated class is not one.
#[test]
fn python_overrides_list_subclass_methods() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("pkg/__init__.py", ""),
        (
            "pkg/fields/__init__.py",
            "class Field:\n    def get_db_prep_value(self, value):\n        return value\n\n\nclass IntegerField(Field):\n    def get_db_prep_value(self, value):\n        return int(value)\n\n\nclass BigIntegerField(IntegerField):\n    pass\n\n\nclass PositiveBigIntegerField(BigIntegerField):\n    def get_db_prep_value(self, value):\n        return abs(value)\n",
        ),
        (
            "pkg/fields/json.py",
            "from pkg.fields import Field\n\n\nclass JSONField(Field):\n    def get_db_prep_value(self, value):\n        return str(value)\n",
        ),
        ("pkg/other.py", "class Other:\n    def get_db_prep_value(self, value):\n        return None\n"),
    ]);
    let out = fox_code(&p, "Field.get_db_prep_value", "overrides", 800);
    assert!(out.contains("3 overrides"), "{out}");
    for m in ["IntegerField.get_db_prep_value", "PositiveBigIntegerField.get_db_prep_value", "JSONField.get_db_prep_value"] {
        assert!(out.contains(m), "{m}: {out}");
    }
    assert!(!out.contains("Other."), "{out}");
    let out = fox_code(&p, "IntegerField.get_db_prep_value", "overrides", 800);
    assert!(out.contains("overrides ~c:") && out.contains("method Field.get_db_prep_value"), "{out}");
    assert!(out.contains("PositiveBigIntegerField.get_db_prep_value") && !out.contains("JSONField"), "{out}");
}

/// Struct and class fields are symbols of kind `field` inside their type; locals are not.
#[test]
fn struct_and_class_fields_are_symbols() {
    let cases: &[(&str, &str, &[&str])] = &[
        (
            "x.go",
            "package x\n\ntype KubeletConfiguration struct {\n\tContainerLogMaxWorkers *int32 `json:\"containerLogMaxWorkers,omitempty\"`\n\tA, B int\n}\n\nfunc f() {\n\ttype local struct{ Hidden int }\n}\n",
            &["KubeletConfiguration.ContainerLogMaxWorkers", "KubeletConfiguration.A", "KubeletConfiguration.B"],
        ),
        (
            "x.c",
            "typedef struct IndexStmt {\n\tint type;\n\tchar *idxname;\n\tchar **names;\n\tint cols[4];\n} IndexStmt;\n",
            &["IndexStmt.IndexStmt.type", "IndexStmt.IndexStmt.idxname", "IndexStmt.IndexStmt.names", "IndexStmt.IndexStmt.cols"],
        ),
        ("x.cpp", "struct Opts {\n  int depth = 2;\n  void run();\n};\n", &["Opts.depth"]),
        ("x.rs", "pub struct Config {\n    pub max_workers: u32,\n}\n", &["Config.max_workers"]),
        ("X.java", "class Config {\n    private int maxWorkers = 2;\n}\n", &["Config.maxWorkers"]),
        ("X.cs", "class Config {\n    private int maxWorkers = 2;\n}\n", &["Config.maxWorkers"]),
        ("x.ts", "class Config {\n  maxWorkers: number = 2\n}\n", &["Config.maxWorkers"]),
        ("x.js", "class Config {\n  maxWorkers = 2\n}\n", &["Config.maxWorkers"]),
        (
            "x.py",
            "class Book(Model):\n    title = CharField()\n    pages: int = 0\n\n    def f(self):\n        hidden = 1\n",
            &["Book.title", "Book.pages"],
        ),
    ];
    let mut fails = vec![];
    for (path, src, want) in cases {
        let l = lang::for_path(path).unwrap();
        let p = tsindex::extract(l, src.as_bytes());
        let q = qnames(&p);
        for w in *want {
            match q.iter().position(|x| x == w) {
                Some(i) if p.defs[i].kind == "field" => {}
                Some(i) => fails.push(format!("{path}: {w} is a {}", p.defs[i].kind)),
                None => fails.push(format!("{path}: missing field {w} (got {q:?})")),
            }
        }
        if q.iter().any(|x| x.ends_with("Hidden") || x.ends_with("hidden")) {
            fails.push(format!("{path}: a local is a field: {q:?}"));
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

/// A field is found by name, first; it is no call target, so a same-name function keeps its
/// callers certain.
#[test]
fn fields_are_found_but_are_no_call_targets() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("go.mod", "module example.com/k\n\ngo 1.21\n"),
        ("config/types.go", "package config\n\ntype KubeletConfiguration struct {\n\tContainerLogMaxWorkers int32\n\tWorkers int\n}\n"),
        ("pool/pool.go", "package pool\n\nfunc Workers() int { return 1 }\n"),
        ("pool/use.go", "package pool\n\nfunc Use() int { return Workers() }\n"),
    ]);
    let c = db::open_ro().unwrap();
    let o = foxdb::query::FindOpts { kind: "any", all: false, budget: 800, limit: 10, offset: 0 };
    let out = foxdb::query::find(&c, Some(&p), "ContainerLogMaxWorkers", &o).unwrap();
    assert!(out.lines().next().unwrap_or("").contains("field KubeletConfiguration.ContainerLogMaxWorkers  config/types.go:4"), "{out}");
    let out = fox_code(&p, "pool.Workers", "callers", 800);
    assert!(out.contains("1 call sites in 1 files") && out.contains(":3 Use c:") && !out.contains("guess"), "{out}");
}

/// An identifier that names a symbol finds that symbol and near names; rows that only share a
/// word with it (`i` and `contains` for `IContains`) or only call it are padding, and left out.
/// A text file holding the identifier itself stays.
#[test]
fn find_by_exact_name_drops_padding() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "lookups.py",
            "class Contains:\n    pass\n\n\nclass IContains(Contains):\n    pass\n\n\nclass KeyTransformIContains(IContains):\n    pass\n",
        ),
        ("query.py", "from lookups import IContains\n\n\ndef build_lookup():\n    return IContains()\n"),
        ("docs/faq.txt", "I think this page contains the answers you need.\n"),
        ("docs/ref.txt", "Use icontains for a case-insensitive match.\n"),
        (
            "postgres.c",
            "static void helper_a(void) {}\nstatic void helper_b(void) {}\nvoid exec_simple_query(const char *q) { helper_a(); helper_b(); }\nint exec_is_simple_query(void) { return 0; }\n",
        ),
    ]);
    let c = db::open_ro().unwrap();
    let o = foxdb::query::FindOpts { kind: "any", all: false, budget: 800, limit: 20, offset: 0 };
    let out = foxdb::query::find(&c, Some(&p), "IContains", &o).unwrap();
    assert!(out.lines().next().unwrap_or("").contains("class IContains  lookups.py:5"), "{out}");
    assert!(out.contains("KeyTransformIContains") && out.contains("docs/ref.txt"), "{out}");
    assert!(!out.contains("docs/faq.txt") && !out.contains("build_lookup"), "{out}");
    let out = foxdb::query::find(&c, Some(&p), "exec_simple_query", &o).unwrap();
    assert!(out.lines().next().unwrap_or("").contains("function exec_simple_query  postgres.c:3"), "{out}");
    assert!(!out.contains("helper_a") && !out.contains("helper_b"), "{out}");
}

/// The same attribute repeated in every subclass (`requires_crawler_process` in each command)
/// does not crowd the first page: a field that only contains the name ranks low, and each
/// further symbol of a name already shown counts less.
#[test]
fn find_does_not_pad_with_repeated_fields() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut files: Vec<(String, String)> = vec![
        ("pkg/crawler.py".into(), "class CrawlerProcessBase:\n    pass\n\n\nclass CrawlerProcess(CrawlerProcessBase):\n    pass\n".into()),
        (
            "pkg/run.py".into(),
            "from pkg.crawler import CrawlerProcess\n\n\ndef start_crawler_process():\n    return CrawlerProcess()\n".into(),
        ),
    ];
    for i in 0..8 {
        files.push((format!("pkg/commands/c{i}.py"), "class Command:\n    requires_crawler_process = True\n".into()));
    }
    files.push(("pkg/settings.py".into(), "FORCE_CRAWLER_PROCESS = False\n".into()));
    files.push(("pkg/commands/base.py".into(), "class ScrapyCommand:\n    crawler_process = None\n".into()));
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let (_h, _r, p) = inline_project(&refs);
    let c = db::open_ro().unwrap();
    let o = foxdb::query::FindOpts { kind: "any", all: false, budget: 800, limit: 20, offset: 0 };
    let out = foxdb::query::find(&c, Some(&p), "CrawlerProcess", &o).unwrap();
    let head: Vec<&str> = out.lines().take(6).collect();
    assert!(head[0].contains("class CrawlerProcess  pkg/crawler.py:5"), "{out}");
    assert!(head.iter().filter(|l| l.contains("requires_crawler_process")).count() <= 1, "{out}");
    for want in ["CrawlerProcessBase", "start_crawler_process", "FORCE_CRAWLER_PROCESS", "ScrapyCommand.crawler_process"] {
        assert!(head.iter().any(|l| l.contains(want)), "{want}: {out}");
    }
}

/// A qualifier naming a directory with a dot in it (`gopkg.in/yaml.v3`) resolves, written as an
/// import path or with dots alone (`yaml.v3.Marshal`).
#[test]
fn dotted_directory_qualifiers_resolve() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("vendor/gopkg.in/yaml.v3/yaml.go", "package yaml\n\nfunc Marshal(v any) ([]byte, error) { return nil, nil }\n"),
        ("vendor/gopkg.in/yaml.v2/yaml.go", "package yaml\n\nfunc Marshal(v any) ([]byte, error) { return nil, nil }\n"),
        ("pkg/enc/enc.go", "package enc\n\nfunc Marshal(v any) ([]byte, error) { return nil, nil }\n"),
    ]);
    for q in ["yaml.v3.Marshal", "gopkg.in/yaml.v3.Marshal"] {
        let out = fox_code(&p, q, "callers", 800);
        assert!(out.contains("callers of ~c:") && out.contains("vendor/gopkg.in/yaml.v3/yaml.go:3"), "{q}: {out}");
    }
}

/// reStructuredText and AsciiDoc files are split into sections like Markdown: a question about
/// the docs finds the section, with its line.
#[test]
fn rst_and_asciidoc_docs_are_sections() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "docs/topics/settings.rst",
            "========\nSettings\n========\n\nIntro text.\n\nDownload delay\n--------------\n\nThe amount of time the downloader waits between pages.\n\nConcurrency\n-----------\n\nHow many requests run at once.\n",
        ),
        ("docs/guide.adoc", "= Guide\n\n== Retry policy\n\nFailed requests are retried three times.\n"),
        ("app.py", "def run():\n    pass\n"),
    ]);
    let c = db::open_ro().unwrap();
    let o = foxdb::query::FindOpts { kind: "doc", all: false, budget: 800, limit: 10, offset: 0 };
    let out = foxdb::query::find(&c, Some(&p), "how long does the downloader wait between pages", &o).unwrap();
    assert!(out.lines().next().unwrap_or("").contains("Download delay") && out.contains("docs/topics/settings.rst:7"), "{out}");
    let out = foxdb::query::find(&c, Some(&p), "retried failed requests", &o).unwrap();
    assert!(out.lines().next().unwrap_or("").contains("Retry policy") && out.contains("docs/guide.adoc:3"), "{out}");
}

// ---- answers carry code ----

/// Each caller row carries its call site's code: inline after ` | ` for one site, one
/// `   :line | code` row per site for several. An answer whose code would not fit the budget
/// drops the code (and says so) when the locations alone fit.
#[test]
fn callers_rows_carry_the_call_site_code() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "kernel/core.c",
            "int wake_task(int p) { return p; }\nint run(void) {\n    int a = wake_task(1);\n    return wake_task(a) + 1;\n}\n",
        ),
        ("kernel/kthread.c", "int kthread_start(void)\n{\n\treturn   wake_task(0);\n}\n"),
    ]);
    let out = fox_code(&p, "wake_task", "callers", 800);
    assert!(out.contains("  :3 kthread_start c:") && out.contains(" | return   wake_task(0);"), "{out}");
    assert!(out.contains("  :3,4 run c:"), "{out}");
    assert!(out.contains("\n   :3 | int a = wake_task(1);\n   :4 | return wake_task(a) + 1;\n"), "{out}");
    // A d1 impact row carries the dependent's first line using the target.
    let out = fox_code(&p, "wake_task", "impact", 800);
    assert!(out.contains(":2 run c:") && out.contains(" | :3 int a = wake_task(1);"), "{out}");
    // A budget the code does not fit in: locations only, and a note.
    let out = fox_code(&p, "wake_task", "callers", 85);
    assert!(out.len() <= 85 * 4 + 80, "{} bytes: {out}", out.len());
    assert!(out.contains("kthread_start") && !out.contains(" | ") && out.contains("code left out to fit the budget"), "{out}");
}

/// `fox_read` of a symbol name returns its body; of a file, its declarations with the line
/// range of each and how to read one.
#[test]
fn fox_read_returns_bodies_and_outlines_with_ranges() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, r, _p) = inline_project(&[(
        "pkg/shapes.py",
        "class Circle:\n    radius = 1\n\n    def area(self):\n        return 3 * self.radius ** 2\n\n\ndef unit():\n    return Circle()\n",
    )]);
    std::env::set_current_dir(r.path()).unwrap();
    let call = |args: serde_json::Value| -> String {
        let r = foxdb::mcp::handle(
            &serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fox_read","arguments":args}}),
        )
        .unwrap();
        r["result"]["content"][0]["text"].as_str().unwrap().to_string()
    };
    let out = call(serde_json::json!({"target": "Circle.area", "fresh": true}));
    assert!(out.contains("pkg/shapes.py:4-5") && out.contains("return 3 * self.radius ** 2"), "{out}");
    let out = call(serde_json::json!({"target": "pkg/shapes.py", "fresh": true}));
    assert!(out.contains("1 class Circle c:") && out.contains(" ..5"), "{out}");
    assert!(out.contains(" 4 method area c:") && out.contains("8 function unit c:"), "{out}");
    assert!(out.contains("fox_read target=<handle> or lines=a-b reads one"), "{out}");
}

// ---- review fixes: correctness ----

/// Every segment of a qualifier must match: `pkga.Server.Start` is pkga's, Java `p.A.run` is
/// p/A's; an import path prefers the longest directory match (vendor/ over a one-segment dir);
/// a Go file stem is no package.
#[test]
fn qualified_targets_match_every_segment() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("go.mod", "module example.com/proj\n\ngo 1.21\n"),
        ("pkga/server.go", "package pkga\n\ntype Server struct{}\n\nfunc (s *Server) Start() {}\n"),
        ("pkgb/server.go", "package pkgb\n\ntype Server struct{}\n\nfunc (s *Server) Start() {}\n"),
        ("pod/pod.go", "package pod\n\nfunc GetStatus() int { return 1 }\n"),
        ("vendor/github.com/acme/pod/pod.go", "package pod\n\nfunc GetStatus() int { return 2 }\n"),
        ("svc/user.go", "package svc\n\nfunc Load() {}\n"),
        ("users/load.go", "package users\n\nfunc Load() {}\n"),
        ("p/A.java", "package p;\n\nclass A {\n    void run() {}\n}\n"),
        ("q/A.java", "package q;\n\nclass A {\n    void run() {}\n}\n"),
    ]);
    let head = |t: &str| fox_code(&p, t, "src", 800).lines().find(|l| l.starts_with("~c:")).unwrap_or_default().to_string();
    assert!(head("pkga.Server.Start").contains("pkga/server.go"), "{}", head("pkga.Server.Start"));
    assert!(head("pkgb.Server.Start").contains("pkgb/server.go"), "{}", head("pkgb.Server.Start"));
    assert!(head("p.A.run").contains("p/A.java"), "{}", head("p.A.run"));
    assert!(head("q.A.run").contains("q/A.java"), "{}", head("q.A.run"));
    assert!(
        head("github.com/acme/pod.GetStatus").contains("vendor/github.com/acme/pod/pod.go"),
        "{}",
        head("github.com/acme/pod.GetStatus")
    );
    // `user` is only a file stem in Go: no package of that name.
    let out = fox_code(&p, "user.Load", "src", 800);
    assert!(out.contains("no symbol matches"), "{out}");
    assert!(head("users.Load").contains("users/load.go"), "{}", head("users.Load"));
    let c = db::open_ro().unwrap();
    let o = foxdb::query::FindOpts { kind: "symbol", all: false, budget: 800, limit: 10, offset: 0 };
    let out = foxdb::query::find(&c, Some(&p), "pkgb.Server.Start", &o).unwrap();
    assert!(out.lines().next().unwrap_or("").contains("pkgb/server.go"), "{out}");
}

/// A field target never answers a confident 0: its uses are not tracked, so the answer says so
/// and lists the files naming it. A JS/TS class field holding an arrow function is a method.
#[test]
fn field_targets_say_uses_are_not_tracked() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("go.mod", "module example.com/k\n\ngo 1.21\n"),
        ("config/types.go", "package config\n\ntype Config struct {\n\tMaxWorkers int32\n}\n"),
        ("config/use.go", "package config\n\nfunc Workers(c Config) int32 { return c.MaxWorkers }\n"),
        ("web/app.js", "class App {\n  handleClick = () => { return 1 }\n  render() { return this.handleClick() }\n}\n"),
    ]);
    for rel in ["callers", "refs", "impact"] {
        let out = fox_code(&p, "Config.MaxWorkers", rel, 800);
        assert!(out.contains("field uses are not tracked"), "{rel}: {out}");
        assert!(out.contains("config/use.go:3 | func Workers(c Config) int32 { return c.MaxWorkers }"), "{rel}: {out}");
        assert!(!out.contains("0 call sites") && !out.contains("0 refs"), "{rel}: {out}");
    }
    let out = fox_code(&p, "App.handleClick", "callers", 800);
    assert!(out.contains("method App.handleClick") && out.contains("App.render"), "{out}");
}

/// A doc that holds a snake_case identifier (`get_user`) is no padding.
#[test]
fn find_keeps_docs_holding_a_snake_case_name() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("app/users.py", "def get_user(uid):\n    return uid\n"),
        ("docs/guide.md", "# Guide\n\n## Users\n\nCall get_user to load one.\n"),
    ]);
    let c = db::open_ro().unwrap();
    let o = foxdb::query::FindOpts { kind: "any", all: false, budget: 800, limit: 20, offset: 0 };
    let out = foxdb::query::find(&c, Some(&p), "get_user", &o).unwrap();
    assert!(out.contains("function get_user") && out.contains("docs/guide.md"), "{out}");
}

/// Code shown on a row is the line that names the target, now: a file changed since it was
/// indexed shows no code for a line that moved; a long line is clipped around the name.
#[test]
fn row_code_names_the_target_and_is_clipped_around_it() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let long = format!("int far(void) {{ int x = {}; return wake_task(x); }}\n", "1 + ".repeat(60) + "1");
    let (_h, r, p) = inline_project(&[
        ("core.c", "int wake_task(int p) { return p; }\n"),
        ("a.c", "int a_start(void)\n{\n\treturn wake_task(0);\n}\n"),
        ("far.c", long.as_str()),
    ]);
    let out = fox_code(&p, "wake_task", "callers", 2000);
    let far = out.lines().find(|l| l.contains(" far c:")).unwrap_or_default();
    assert!(far.contains("return wake_task(x);") && far.chars().count() < 220, "{far}");
    std::fs::write(r.path().join("a.c"), "// moved\n\nint a_start(void)\n{\n\treturn wake_task(0);\n}\n").unwrap();
    let out = fox_code(&p, "wake_task", "callers", 2000);
    for l in out.lines().filter(|l| l.contains(" | ")) {
        assert!(l.contains("wake_task"), "a row shows code that does not name the target: {l}\n{out}");
    }
}

/// An upgrade that parses most of a project again leaves its full-text tables merged.
#[test]
fn upgrade_reparse_merges_the_full_text_tables() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let files: Vec<(String, String)> =
        (0..240).map(|i| (format!("m{i}.go"), format!("package m\n\nfunc F{i}() int {{ return {i} }}\n"))).collect();
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let (_h, _r, p) = inline_project(&refs);
    let mut c = db::open_rw().unwrap();
    c.execute("UPDATE ts_file SET hash = '', size = -1", []).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    for t in ["fts_node", "fts_body"] {
        assert!(db::fts_segments(&c, t) <= 1, "{t}: {} segments", db::fts_segments(&c, t));
    }
}

// ---- review fixes: scale and budget ----

/// Code rides along only when the whole answer with code fits the budget asked for; otherwise
/// the rows are locations only, and the hint names a budget that does show the code, only when
/// one exists. A larger budget never shows less.
#[test]
fn code_rides_only_when_the_whole_answer_fits() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut files: Vec<(String, String)> = vec![("core.c".into(), "int wake_task(int p) { return p; }\n".into())];
    for i in 0..40 {
        files.push((format!("d{i}/use.c"), format!("int caller_{i}(void)\n{{\n\treturn wake_task({i}) + wake_task({i} + 1);\n}}\n")));
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let (_h, _r, p) = inline_project(&refs);
    let has_code = |o: &str| o.lines().any(|l| l.starts_with("  :") && l.contains(" | "));
    let small = fox_code(&p, "wake_task", "callers", 300);
    assert!(!has_code(&small), "{small}");
    let need: usize = small
        .lines()
        .find_map(|l| l.strip_prefix("code left out to fit the budget; budget="))
        .and_then(|x| x.split_whitespace().next())
        .and_then(|x| x.parse().ok())
        .unwrap_or_else(|| panic!("no hint: {small}"));
    assert!(need > 300 && need <= 4000, "{need}");
    let at = fox_code(&p, "wake_task", "callers", need);
    assert!(has_code(&at) && !at.contains("cursor="), "budget {need} must show the code whole:\n{at}");
    let more = fox_code(&p, "wake_task", "callers", 4000);
    assert!(has_code(&more), "{more}");
    // Locations only cost what they did before: the hint is the only extra line.
    assert!(!small.contains(" | "), "{small}");
}

/// An answer whose code could never fit names no budget: it would show nothing more.
#[test]
fn no_budget_hint_when_the_code_cannot_fit() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut files: Vec<(String, String)> = vec![("core.c".into(), "int wake_task(int p) { return p; }\n".into())];
    for i in 0..300 {
        files.push((format!("d{i}/use.c"), format!("int caller_{i}(void) {{ return wake_task({i}); /* {} */ }}\n", "x".repeat(60))));
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let (_h, _r, p) = inline_project(&refs);
    let out = fox_code(&p, "wake_task", "callers", 4000);
    assert!(!out.contains(" | ") && !out.contains("budget=4000") && out.contains("code left out"), "{out}");
}

/// A file outline lists types and functions; fields are counted, not listed, and a type's own
/// outline lists them.
#[test]
fn file_outlines_leave_fields_to_their_type() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[(
        "api/types.go",
        "package api\n\ntype Pod struct {\n\tName string\n\tSpec string\n\tStatus string\n}\n\nfunc New() Pod { return Pod{} }\n",
    )]);
    let out = fox_code(&p, "api/types.go", "outline", 800);
    assert!(out.contains("struct Pod") && out.contains("function New") && !out.contains("field "), "{out}");
    assert!(out.contains("3 fields not listed"), "{out}");
    let out = fox_code(&p, "Pod", "outline", 800);
    assert!(out.contains("field Name") || out.contains(" field Name"), "{out}");
}

// ---- verification fixes ----

/// A field's answer lists lines of its own file that name it (not the definition line), and
/// an answer with no mentions cannot read as "unused".
#[test]
fn field_answers_show_own_file_mentions_and_never_read_as_unused() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("x/p.go", "package x\n\ntype P struct {\n\txval int\n}\n\nfunc (p *P) Get() int { return p.xval }\n"),
        ("y/q.go", "package y\n\ntype Q struct {\n\tlonely int\n}\n"),
    ]);
    let out = fox_code(&p, "P.xval", "refs", 800);
    assert!(out.contains("x/p.go:7 | func (p *P) Get() int { return p.xval }") && !out.contains("x/p.go:4 |"), "{out}");
    let out = fox_code(&p, "Q.lonely", "refs", 800);
    assert!(out.contains("does not mean it is unused") && !out.contains("no other file names"), "{out}");
}

/// A call through a receiver unrelated to the class reaches a function-holding field only as
/// a guess; through `this` it is certain.
#[test]
fn arrow_field_methods_need_receiver_evidence() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "web/w.ts",
            "export class Btn {\n  dispatch = (a: number) => { this.handle(); };\n  handle(): void {}\n  click() { this.dispatch(1); }\n}\n",
        ),
        (
            "web/use.ts",
            "import { createStore } from \"redux\";\nconst store = createStore(() => 0);\nexport function fire() {\n  store.dispatch({ type: \"x\" });\n}\n",
        ),
    ]);
    let out = fox_code(&p, "Btn.dispatch", "callers", 800);
    let fire = out.lines().find(|l| l.contains(" fire c:")).unwrap_or_default();
    assert!(fire.is_empty() || fire.contains("guess"), "{out}");
    let click = out.lines().find(|l| l.contains("Btn.click c:")).unwrap_or_default();
    assert!(!click.is_empty() && !click.contains("guess"), "{out}");
}

/// Row code is clipped around the call, not the first mention of the name.
#[test]
fn row_code_centres_on_the_call() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let pad = "x".repeat(200);
    let src = format!("int far(void) {{ /* wake_task {pad} */ return wake_task(1); }}\n");
    let (_h, _r, p) = inline_project(&[("core.c", "int wake_task(int p) { return p; }\n"), ("far.c", src.as_str())]);
    let out = fox_code(&p, "wake_task", "callers", 2000);
    let far = out.lines().find(|l| l.contains(" far c:")).unwrap_or_default();
    assert!(far.contains("return wake_task(1);"), "{out}");
}

/// Equally good matches for a qualified target are named, not broken silently.
#[test]
fn equal_qualified_matches_are_named() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("go.mod", "module example.com/m\n\ngo 1.21\n"),
        ("vendor/github.com/acme/yaml/y.go", "package yaml\n\nfunc Marshal() int { return 1 }\n"),
        ("third/github.com/acme/yaml/y.go", "package yaml\n\nfunc Marshal() int { return 2 }\n"),
    ]);
    let out = fox_code(&p, "github.com/acme/yaml.Marshal", "callers", 800);
    assert!(out.contains("2 equally good matches"), "{out}");
}

/// C and C++ function-pointer members (`void (*irq_mask)(struct irq_data *)`) are members of
/// their struct, callable, and listed in its outline beside the plain fields.
#[test]
fn c_function_pointer_members_are_struct_members() {
    let src = b"struct irq_chip {\n\tconst char *name;\n\tvoid (*irq_mask)(struct irq_data *data);\n\tstruct page *(*alloc)(int n);\n\tint (*const probe)(void);\n};\n";
    for l in ["c", "cpp"] {
        let p = tsindex::extract(lang::by_name(l).unwrap(), src);
        let q = qnames(&p);
        for want in ["irq_chip.name", "irq_chip.irq_mask", "irq_chip.alloc", "irq_chip.probe"] {
            assert!(q.iter().any(|x| x == want), "{l}: missing {want} in {q:?}");
        }
        let kind = |n: &str| p.defs.iter().find(|d| d.name == n).map(|d| d.kind.clone()).unwrap();
        assert_eq!(kind("name"), "field", "{l}");
        assert_eq!(kind("irq_mask"), "method", "{l}");
        assert!(p.defs.iter().find(|d| d.name == "irq_mask").unwrap().field_fn, "{l}");
        // A parameter of the member's own type is no member.
        assert!(!q.iter().any(|x| x.ends_with(".data") || x.ends_with(".n")), "{l}: {q:?}");
    }
}

fn c_ops_project() -> (tempfile::TempDir, tempfile::TempDir, project::Project) {
    inline_project(&[
        (
            "include/linux/irq.h",
            "struct irq_data;\nstruct irq_chip {\n\tconst char *name;\n\tvoid (*irq_mask)(struct irq_data *data);\n};\nstruct irq_data {\n\tunsigned int irq;\n\tstruct irq_chip *chip;\n};\nstruct irq_desc {\n\tstruct irq_data irq_data;\n};\ntypedef struct irq_chip irq_chip_t;\nstruct irq_chip *irq_desc_get_chip(struct irq_desc *desc);\n",
        ),
        ("include/linux/other.h", "struct other_ops {\n\tvoid (*irq_mask)(int x);\n};\n"),
        (
            "include/linux/fs.h",
            "struct address_space_operations {\n\tint (*bmap)(struct address_space *m, long b);\n};\nstruct address_space {\n\tconst struct address_space_operations *a_ops;\n};\nstruct inode {\n\tstruct address_space *i_mapping;\n};\nint bmap(struct inode *inode, long *block);\n",
        ),
        ("kernel/irq/mask.c", "void irq_mask(int x) { }\n"),
        ("kernel/irq/chip.c", "void mask_irq(struct irq_desc *desc)\n{\n\tdesc->irq_data.chip->irq_mask(&desc->irq_data);\n}\n"),
        (
            "include/linux/irqchip/chained_irq.h",
            "static inline void chained_irq_enter(struct irq_chip *chip, struct irq_desc *desc)\n{\n\tif (chip->irq_mask)\n\t\tchip->irq_mask(&desc->irq_data);\n}\n",
        ),
        ("kernel/irq/cast.c", "void cast_mask(void *p)\n{\n\t((struct irq_chip *)p)->irq_mask(0);\n}\n"),
        ("kernel/irq/ret.c", "void ret_mask(struct irq_desc *desc)\n{\n\tirq_desc_get_chip(desc)->irq_mask(0);\n}\n"),
        ("kernel/irq/alias.c", "void alias_mask(irq_chip_t *c)\n{\n\tc->irq_mask(0);\n}\n"),
        ("drivers/other/o.c", "void other_mask(struct other_ops *o)\n{\n\to->irq_mask(1);\n}\n"),
        (
            "drivers/irqchip/irq-foo.c",
            "static void foo_mask(struct irq_data *d) { }\nstatic void foo_mask2(struct irq_data *d) { }\nstatic struct irq_chip foo_chip = {\n\t.name = \"foo\",\n\t.irq_mask = foo_mask,\n};\nvoid foo_init(struct irq_chip *c)\n{\n\tc->irq_mask = foo_mask2;\n}\n",
        ),
        (
            "fs/inode.c",
            "int bmap(struct inode *inode, long *block)\n{\n\t*block = inode->i_mapping->a_ops->bmap(inode->i_mapping, *block);\n\treturn 0;\n}\n",
        ),
        ("fs/ioctl.c", "int ioctl_fibmap(struct inode *inode)\n{\n\tlong b;\n\treturn bmap(inode, &b);\n}\n"),
        (
            "fs/ext4/inode.c",
            "static int ext4_bmap(struct address_space *m, long b)\n{\n\treturn 0;\n}\nstatic const struct address_space_operations ext4_aops = {\n\t.bmap = ext4_bmap,\n};\n",
        ),
    ])
}

fn c_sym(p: &project::Project, path: &str, name: &str) -> String {
    let c = db::open_ro().unwrap();
    let id: i64 = c
        .query_row(
            "SELECT id FROM node WHERE project_id = ?1 AND path = ?2 AND name = ?3 AND kind = 'symbol'",
            rusqlite::params![p.id, path, name],
            |r| r.get(0),
        )
        .unwrap();
    format!("c:{id:x}")
}

/// A C call through a member (`chip->irq_mask(d)`, `inode->i_mapping->a_ops->bmap(...)`)
/// reaches the struct's function-pointer member, found from the receiver's declared type through
/// the fields on the way, a cast, a function's return type or a typedef; never a same-name
/// global function, and never another struct's member of that name.
#[test]
fn c_member_calls_resolve_to_the_struct_member() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = c_ops_project();
    let out = fox_code(&p, "irq_chip.irq_mask", "callers", 4000);
    let r = resolved(&out);
    for f in ["chip.c (1)", "chained_irq.h (1)", "cast.c (1)", "ret.c (1)", "alias.c (1)"] {
        assert!(r.contains(f), "missing {f}: {out}");
    }
    assert!(!r.contains("o.c") && !r.contains("guess"), "{out}");
    assert!(r.contains("5 call sites in 5 files"), "{out}");
    let out = fox_code(&p, "other_ops.irq_mask", "callers", 4000);
    assert!(resolved(&out).contains("1 call sites in 1 files") && out.contains("o.c (1)"), "{out}");
    // The global function of the same name has no caller.
    let out = fox_code(&p, &c_sym(&p, "kernel/irq/mask.c", "irq_mask"), "callers", 4000);
    assert!(resolved(&out).contains("0 call sites"), "{out}");
    // `bmap()` is called from ioctl.c; the call through `a_ops->bmap` is the member's.
    let out = fox_code(&p, &c_sym(&p, "fs/inode.c", "bmap"), "callers", 4000);
    let r = resolved(&out);
    assert!(r.contains("1 call sites in 1 files") && r.contains("ioctl.c (1)") && !r.contains("guess"), "{out}");
    let out = fox_code(&p, "address_space_operations.bmap", "callers", 4000);
    assert!(resolved(&out).contains("1 call sites in 1 files") && out.contains(":3 bmap c:"), "{out}");
}

/// A function assigned to a member (`.bmap = ext4_bmap`, `c->irq_mask = foo_mask2`) is reached
/// through calls of that member: impact of the function follows them, and impact of the member
/// lists the functions assigned to it.
#[test]
fn c_functions_assigned_to_members_are_dispatch_targets() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = c_ops_project();
    let out = fox_code(&p, "ext4_bmap", "impact", 4000);
    assert!(out.contains(":1 bmap c:") && out.contains("via address_space_operations.bmap"), "{out}");
    assert!(out.contains("ioctl_fibmap"), "{out}");
    let out = fox_code(&p, "irq_chip.irq_mask", "impact", 4000);
    assert!(out.contains("foo_mask c:") && out.contains("foo_mask2 c:") && out.contains("implements irq_chip.irq_mask"), "{out}");
    assert!(out.contains("mask_irq") && !out.contains("other_mask"), "{out}");
    let out = fox_code(&p, "foo_mask", "impact", 4000);
    assert!(out.contains("mask_irq") && out.contains("chained_irq_enter"), "{out}");
}

/// A line storing a function in the member (`.bmap = ext4_bmap,`) is accounted for: a store,
/// not a call, for `callers`, resolved for `impact`; never unresolved.
#[test]
fn c_member_store_lines_are_accounted_for() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = c_ops_project();
    let out = fox_code(&p, "address_space_operations.bmap", "callers", 4000);
    let head = out.lines().next().unwrap_or("");
    assert!(head.contains("1 line stores a function in it (not a call)") && !head.contains("unresolved"), "{out}");
    assert!(out.contains("stores a function in it (not calls): 1\n  fs/ext4/inode.c:6"), "{out}");
    assert!(!out.contains("references, not calls"), "{out}");
    let out = fox_code(&p, "address_space_operations.bmap", "impact", 4000);
    assert!(!out.contains("unresolved mentions:\n  fs/ext4/inode.c:6"), "{out}");
}

/// A plain call of a function whose only visible declaration is a header prototype reaches the
/// prototype even when a struct has a function-pointer member of the same name.
#[test]
fn c_prototype_calls_survive_a_same_name_member() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("blk.h", "struct bio;\nstruct bdev_ops {\n\tvoid (*submit_bio)(struct bio *bio);\n};\nvoid submit_bio(struct bio *bio);\n"),
        ("use.c", "#include \"blk.h\"\nvoid go(struct bio *b)\n{\n\tsubmit_bio(b);\n}\n"),
    ]);
    let out = fox_code(&p, "go", "callees", 2000);
    assert!(out.contains("1 callees") && out.contains("blk.h") && out.contains("function submit_bio"), "{out}");
}

/// A receiver declared in a block is typed by that block's declaration only: the same name
/// outside the block (or in another `case`) keeps its own type.
#[test]
fn c_block_scoped_receivers_keep_their_own_types() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("ops.h", "struct a_ops {\n\tint (*op)(int);\n};\nstruct b_ops {\n\tint (*op)(int);\n};\n"),
        (
            "blk.c",
            "int f(struct a_ops *x, int k)\n{\n\tif (k) {\n\t\tstruct b_ops *x = 0;\n\t\treturn x->op(1);\n\t}\n\treturn x->op(2);\n}\n",
        ),
        (
            "sw.c",
            "int g(int k)\n{\n\tswitch (k) {\n\tcase 1: {\n\t\tstruct a_ops *y = 0;\n\t\treturn y->op(3);\n\t}\n\tcase 2: {\n\t\tstruct b_ops *y = 0;\n\t\treturn y->op(4);\n\t}\n\t}\n\treturn 0;\n}\n",
        ),
    ]);
    let a = fox_code(&p, "a_ops.op", "callers", 4000);
    let b = fox_code(&p, "b_ops.op", "callers", 4000);
    assert!(resolved(&a).contains("2 call sites in 2 files") && !resolved(&a).contains("guess"), "{a}");
    assert!(a.contains(":7 f c:") && a.contains(":6 g c:"), "{a}");
    assert!(resolved(&b).contains("2 call sites in 2 files") && b.contains(":5 f c:") && b.contains(":10 g c:"), "{b}");
}

/// A class field holding a function spans its body: calls inside it belong to it, not the class.
#[test]
fn arrow_function_fields_span_their_bodies() {
    let src = b"class Widget {\n  handleClick = (e) => {\n    const a = 1;\n    return helper(e);\n  };\n}\n";
    let p = tsindex::extract(lang::by_name("javascript").unwrap(), src);
    let cs = calls(&p);
    assert!(cs.iter().any(|(a, b)| a == "Widget.handleClick" && b == "helper"), "{cs:?}");
}

/// Re-parsing a header that declares a member keeps the functions stored in that member linked
/// to it, and a changed field type moves member calls through it to the new type's member.
#[test]
fn c_member_links_follow_header_edits() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, p) = inline_project(&[
        (
            "ops.h",
            "struct my_ops {\n\tint (*go)(int);\n};\nstruct alt_ops {\n\tint (*run)(int);\n};\nstruct cfg_ops {\n\tint (*run)(int);\n};\nstruct holder {\n\tstruct alt_ops *ops;\n};\n",
        ),
        ("impl.c", "#include \"ops.h\"\nstatic int my_go(int x) { return x; }\nstatic struct my_ops o = {\n\t.go = my_go,\n};\n"),
        ("call.c", "#include \"ops.h\"\nint use(struct my_ops *m) { return m->go(1); }\n"),
        ("hold.c", "#include \"ops.h\"\nint hold(struct holder *h) { return h->ops->run(1); }\n"),
    ]);
    let out = fox_code(&p, "my_go", "impact", 4000);
    assert!(out.contains("via my_ops.go"), "{out}");
    assert!(fox_code(&p, "alt_ops.run", "callers", 4000).contains("hold.c (1)"));
    let mut c = db::open_rw().unwrap();
    let h = repo.path().join("ops.h");
    let text = std::fs::read_to_string(&h).unwrap();
    std::fs::write(&h, format!("/* ops */\n{}", text.replace("struct alt_ops *ops;", "struct cfg_ops *ops;"))).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    assert_eq!(tsindex::index_project(&mut c, &p).unwrap().parsed, 1);
    let out = fox_code(&p, "my_go", "impact", 4000);
    assert!(out.contains("via my_ops.go") && out.contains("use c:"), "{out}");
    let alt = fox_code(&p, "alt_ops.run", "callers", 4000);
    let cfg = fox_code(&p, "cfg_ops.run", "callers", 4000);
    assert!(!alt.contains("hold.c (1)") && cfg.contains("hold.c (1)"), "{alt}\n{cfg}");
    // The scoped update matches a full re-resolution.
    let snap = |c: &rusqlite::Connection| -> Vec<(i64, i64, i64)> {
        let mut q = c.prepare("SELECT rowid, dst, slot FROM ts_ref ORDER BY rowid").unwrap();
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().flatten().collect()
    };
    let inc = snap(&c);
    tsindex::resolve(&c, p.id).unwrap();
    assert_eq!(snap(&c), inc);
}

/// Typedef names and struct tags are separate namespaces: `tagged *` is the typedef's struct,
/// `struct tagged *` the tag's.
#[test]
fn c_typedef_names_and_struct_tags_do_not_collide() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("t.h", "struct a_ops {\n\tint (*op)(int);\n};\nstruct tagged {\n\tint (*op)(int);\n};\ntypedef struct a_ops tagged;\n"),
        ("u.c", "#include \"t.h\"\nint t1(tagged *x) { return x->op(1); }\nint t2(struct tagged *y) { return y->op(2); }\n"),
    ]);
    let a = fox_code(&p, "a_ops.op", "callers", 4000);
    let t = fox_code(&p, "tagged.op", "callers", 4000);
    assert!(a.contains(":2 t1 c:") && !a.contains(":3 t2"), "{a}");
    assert!(t.contains(":3 t2 c:") && !t.contains(":2 t1"), "{t}");
}

/// A function stored in a member whose struct is ambiguous (two structs share the tag) or whose
/// receiver has no known type is linked to it only as a guess.
#[test]
fn c_uncertain_member_stores_are_guesses() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("d1/dup.h", "struct dup_ops {\n\tint (*cb)(int);\n};\n"),
        ("d2/dup.h", "struct dup_ops {\n\tint (*cb)(int);\n};\n"),
        ("d1/set.c", "static int f1(int x) { return x; }\nstatic struct dup_ops o = {\n\t.cb = f1,\n};\n"),
        ("d1/use.c", "int use1(struct dup_ops *p) { return p->cb(1); }\n"),
        ("o/only.h", "struct only_ops {\n\tint (*hook)(int);\n};\n"),
        ("o/set.c", "static int f2(int x) { return x; }\nvoid set2(void) { g->hook = f2; }\n"),
        ("o/use.c", "int use2(struct only_ops *p) { return p->hook(1); }\n"),
    ]);
    for (f, user) in [("f1", "use1"), ("f2", "use2")] {
        let out = fox_code(&p, f, "impact", 4000);
        let row = out.lines().find(|l| l.contains(&format!(" {user} c:"))).unwrap_or_else(|| panic!("{f}: {out}"));
        assert!(row.contains("guess") && row.contains(" via "), "{f}: {out}");
    }
}

/// A store that reached schema 23 without the type-path index (an earlier build of it) still
/// indexes: opening it adds the index, and a missing one never stops a run.
#[test]
fn indexing_works_without_the_type_path_index() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, p) = inline_project(&[
        ("web/a.ts", "export function a() { return 1 }\n"),
        ("c/ops.h", "struct o_ops {\n\tint (*op)(int);\n};\nstruct w {\n\tstruct o_ops *ops;\n};\n"),
        ("c/use.c", "#include \"ops.h\"\nint u(struct w *x) { return x->ops->op(1); }\n"),
    ]);
    let mut c = db::open_rw().unwrap();
    c.execute_batch("DROP INDEX ts_ref_rtype;").unwrap();
    std::fs::write(repo.path().join("web/a.ts"), "export function a() { return 2 }\n").unwrap();
    std::fs::write(repo.path().join("c/ops.h"), "struct o_ops {\n\tint (*op)(long);\n};\nstruct w {\n\tstruct o_ops *ops;\n\tint n;\n};\n")
        .unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    assert_eq!(tsindex::index_project(&mut c, &p).unwrap().parsed, 2);
    c.execute_batch("DROP INDEX IF EXISTS ts_ref_rtype; PRAGMA user_version = 23;").unwrap();
    drop(c);
    let c = db::open_rw().unwrap();
    let n: i64 = c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = 'ts_ref_rtype'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}

/// A typedef name or a field defined twice with different types has no known type: a member call
/// through it is a guess, not certain to the first definition.
#[test]
fn c_conflicting_type_definitions_leave_member_calls_guessed() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("ops.h", "struct a_ops {\n\tint (*op)(int);\n};\nstruct b_ops {\n\tint (*op)(int);\n};\n"),
        ("d1/t.h", "typedef struct a_ops ops_t;\nstruct h {\n\tstruct a_ops *ops;\n};\n"),
        ("d2/t.h", "typedef struct b_ops ops_t;\nstruct h {\n\tstruct b_ops *ops;\n};\n"),
        ("u.c", "int u(ops_t *x) { return x->op(1); }\nint v(struct h *p) { return p->ops->op(2); }\n"),
    ]);
    for t in ["a_ops.op", "b_ops.op"] {
        let out = fox_code(&p, t, "callers", 4000);
        assert!(out.contains(" call sites in ") && certain_calls(&out) == 0, "{t}: {out}");
    }
}

/// Parameter names of a prototype, a function-pointer local or a cast inside a body are no locals
/// of the body: they never shadow the function's own parameter.
#[test]
fn c_nested_parameter_names_do_not_shadow_the_functions_own() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("ops.h", "struct foo {\n\tint (*run)(int);\n};\nstruct bar {\n\tint (*run)(int);\n};\n"),
        (
            "f.c",
            "int f(struct foo *x)\n{\n\tint (*cb)(struct bar *x) = 0;\n\textern int h(struct bar *x);\n\tvoid *q = (void *)(int (*)(struct bar *x))0;\n\treturn x->run(1);\n}\n",
        ),
    ]);
    let out = fox_code(&p, "foo.run", "callers", 4000);
    assert!(resolved(&out).contains("1 call sites in 1 files") && certain_calls(&out) == 1, "{out}");
    let out = fox_code(&p, "bar.run", "callers", 4000);
    assert!(resolved(&out).contains("0 call sites"), "{out}");
}

/// A value stored in a member (`create->node = node;`) is a function or nothing: never a struct,
/// enum or type that happens to share its name.
#[test]
fn c_member_stores_reach_only_callables() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("t.h", "struct node {\n\tint a;\n};\nstruct kci {\n\tint node;\n\tint (*fn)(int);\n};\n"),
        (
            "k.c",
            "#include \"t.h\"\nvoid g(struct kci *create)\n{\n\tcreate->node = node;\n}\nstatic struct kci k = {\n\t.node = node,\n};\n",
        ),
    ]);
    let c = db::open_ro().unwrap();
    let n: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM ts_ref r JOIN node n ON n.id = r.dst WHERE r.project_id = ?1 AND r.name = 'node' AND r.dst != 0",
            [p.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
}

/// A local declared in an unbraced `case` arm belongs to the whole `switch` block: later arms
/// see it with its type.
#[test]
fn c_unbraced_case_locals_reach_later_arms() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("ops.h", "struct a_ops {\n\tint (*op)(int);\n};\nstruct b_ops {\n\tint (*op)(int);\n};\n"),
        (
            "sw.c",
            "int g(int k)\n{\n\tswitch (k) {\n\tcase 1:\n\t\t;\n\t\tstruct a_ops *y = 0;\n\t\ty->op(3);\n\t\tbreak;\n\tcase 2:\n\t\treturn y->op(4);\n\t}\n\treturn 0;\n}\n",
        ),
    ]);
    let out = fox_code(&p, "a_ops.op", "callers", 4000);
    assert!(resolved(&out).contains("2 call sites in 1 files") && certain_calls(&out) == 2, "{out}");
}

/// A function-pointer member whose type is a typedef (`typedef int (*op_fn)(int); struct ops {
/// op_fn run; };`, also a typedef of a typedef, a function typedef behind a `*`, a typedef in
/// another file) is a function member like `int (*run)(int)`: calls through it and functions
/// stored in it resolve to it. A plain data field of a typedef type is no function member.
#[test]
fn c_typedef_function_pointer_members_are_function_members() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "types.h",
            "typedef int (*op_fn)(int);\ntypedef op_fn op2_fn;\ntypedef int fn_t(int);\ntypedef unsigned int myint;\ntypedef myint myint2;\n",
        ),
        (
            "ops.h",
            "#include \"types.h\"\nstruct ops {\n\top_fn run;\n\top2_fn run2;\n\tfn_t *run3;\n\tmyint size;\n\tmyint2 size2;\n\top_fn *pp;\n};\nstruct ops *get_ops(void);\n",
        ),
        (
            "impl.c",
            "#include \"ops.h\"\nstatic int impl_run(int x) { return x; }\nstatic int impl_run2(int x) { return x; }\nstatic int impl_run3(int x) { return x; }\nstruct ops table = { .run = impl_run, .run2 = impl_run2, .run3 = impl_run3, .size = 4 };\n",
        ),
        ("use.c", "#include \"ops.h\"\nint go(struct ops *o, int x)\n{\n\to->run2(x);\n\to->run3(x);\n\treturn o->run(x) + o->size;\n}\n"),
    ]);
    for m in ["run", "run2", "run3"] {
        let out = fox_code(&p, &format!("ops.{m}"), "callers", 4000);
        assert!(resolved(&out).contains("1 call sites in 1 files") && out.contains("use.c (1)"), "{m}: {out}");
        let out = fox_code(&p, &format!("ops.{m}"), "impact", 4000);
        assert!(out.contains(&format!("impl_{m} c:")) && out.contains("implements"), "{m}: {out}");
    }
    let out = fox_code(&p, "impl_run", "impact", 4000);
    assert!(out.contains("go"), "{out}");
    let out = fox_code(&p, "ops.size", "callers", 4000);
    assert!(!out.contains("call sites in") || resolved(&out).contains("0 call sites"), "{out}");
    // Editing only the typedef re-types the member in another file, both ways.
    let reindex = |repo: &std::path::Path, types: &str| {
        std::fs::write(repo.join("types.h"), types).unwrap();
        let mut c = db::open_rw().unwrap();
        inventory::index_project(&mut c, &p).unwrap();
        tsindex::index_project(&mut c, &p).unwrap();
    };
    reindex(_r.path(), "typedef long op_fn;\ntypedef op_fn op2_fn;\ntypedef int fn_t(int);\n");
    let out = fox_code(&p, "ops.run", "callers", 4000);
    assert!(!out.contains("use.c (1)"), "{out}");
    let out = fox_code(&p, "ops.run2", "callers", 4000);
    assert!(!out.contains("use.c (1)"), "{out}");
    let out = fox_code(&p, "ops.run3", "callers", 4000);
    assert!(out.contains("use.c (1)"), "{out}");
    reindex(_r.path(), "typedef int (*op_fn)(int);\ntypedef op_fn op2_fn;\ntypedef int fn_t(int);\n");
    for m in ["run", "run2", "run3"] {
        let out = fox_code(&p, &format!("ops.{m}"), "callers", 4000);
        assert!(resolved(&out).contains("1 call sites in 1 files") && out.contains("use.c (1)"), "{m}: {out}");
    }
}

/// Call-site rows of a `callers` answer (`  :13 Child.go c:17 | ...`), joined.
fn rows(out: &str) -> String {
    out.lines()
        .filter(|l| l.trim_start().starts_with(':') && l.trim_start()[1..].starts_with(|c: char| c.is_ascii_digit()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Typedef'd function-pointer members (`typedef int (*op_fn)(int); struct ops { op_fn run; };`,
/// also a typedef of a typedef) behave like `int (*run)(int)`.
const C_TYPEDEF_FILES: &[(&str, &str)] = &[
    (
        "include/ops.h",
        "typedef int (*op_fn)(int);\ntypedef op_fn op_alias;\nstruct ops {\n\tint count;\n\top_fn run;\n\top_alias stop;\n};\n",
    ),
    (
        "core/call.c",
        "#include \"ops.h\"\nint drive(struct ops *o, int x)\n{\n\treturn o->run(x);\n}\nint halt(struct ops *o)\n{\n\treturn o->stop(0);\n}\n",
    ),
    (
        "drv/a.c",
        "#include \"ops.h\"\nstatic int a_run(int x) { return x; }\nstatic int a_stop(int x) { return x; }\nstatic struct ops a_ops = {\n\t.count = 3,\n\t.run = a_run,\n\t.stop = a_stop,\n};\n",
    ),
    ("drv/b.c", "#include \"ops.h\"\nstatic int b_run(int x) { return x; }\nvoid b_init(struct ops *o)\n{\n\to->run = b_run;\n}\n"),
];

#[test]
fn typedef_fn_pointer_member_callers_list_calls_and_stores() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(C_TYPEDEF_FILES);
    let out = fox_code(&p, "ops.run", "callers", 6000);
    assert!(out.contains("1 call sites in 1 files") && rows(&out).contains("drive"), "{out}");
    assert!(out.contains("stores a function in it (not calls): 2\n"), "{out}");
    assert!(out.contains("drv/a.c:6 |") && out.contains("drv/b.c:5 |"), "{out}");
    assert!(!out.contains("references, not calls") && !out.contains("may be incomplete"), "{out}");
}

#[test]
fn typedef_of_a_typedef_member_resolves_too() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(C_TYPEDEF_FILES);
    let out = fox_code(&p, "ops.stop", "callers", 6000);
    assert!(out.contains("1 call sites in 1 files") && rows(&out).contains("halt") && !rows(&out).contains("drive"), "{out}");
    assert!(out.contains("stores a function in it (not calls): 1\n") && out.contains("drv/a.c:7 |"), "{out}");
}

#[test]
fn function_stored_in_a_typedef_member_is_reached_through_its_calls() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(C_TYPEDEF_FILES);
    let out = fox_code(&p, "a_run", "impact", 6000);
    assert!(out.contains("drive") && out.contains("via ops.run"), "{out}");
    assert!(!out.contains("halt"), "{out}");
    let out = fox_code(&p, "b_run", "impact", 6000);
    assert!(out.contains("drive") && out.contains("via ops.run"), "{out}");
    let out = fox_code(&p, "ops.run", "impact", 6000);
    assert!(out.contains("a_run c:") && out.contains("b_run c:") && out.contains("implements ops.run"), "{out}");
    assert!(!out.contains("a_stop c:"), "{out}");
    let out = fox_code(&p, "ops.stop", "impact", 6000);
    assert!(out.contains("a_stop c:") && !out.contains("a_run c:") && !out.contains("b_run c:"), "{out}");
}

#[test]
fn plain_data_fields_with_a_typedef_type_are_not_function_members() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("include/t.h", "typedef unsigned long u64x;\nstruct stats { u64x hits; };\n"),
        (
            "src/s.c",
            "#include \"t.h\"\nint get(struct stats *s) { return s->hits; }\nint hits_fn(void) { return 1; }\nstatic struct stats g = { .hits = 3 };\nint use(void) { return hits_fn(); }\n",
        ),
    ]);
    // A call of the plain function stays the function's; the data field is not a call target.
    let out = fox_code(&p, "hits_fn", "callers", 6000);
    assert!(out.contains("1 call sites in 1 files") && rows(&out).contains("use"), "{out}");
    assert!(!rows(&out).contains("get"), "{out}");
}

/// A typedef whose definitions disagree (a function pointer in one header, an integer in
/// another) does not make its members function members.
#[test]
fn typedef_with_disagreeing_definitions_keeps_members_fields() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("a/t.h", "typedef int (*cb_t)(int);\n"),
        ("b/t.h", "typedef int cb_t;\nstruct s { cb_t f; };\n"),
        ("u.c", "#include \"b/t.h\"\nint use(struct s *x) { return x->f(1); }\n"),
    ]);
    let c = db::open_ro().unwrap();
    let k: String = c
        .query_row(
            "SELECT json_extract(meta,'$.k') FROM node WHERE project_id = ?1 AND name = 'f' AND kind = 'symbol' AND path = 'b/t.h'",
            rusqlite::params![p.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(k, "field");
}

/// Python: `self.m()`, `cls.m()` and `super().m()` reach the method Python would run, by the
/// calling class's method resolution order, diamonds and overriding subclasses included.
#[test]
fn python_calls_through_self_and_super_follow_the_mro() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "mro/a.py",
            "class Base:\n    def step(self):\n        return 1\n\n\nclass Mixin:\n    def step(self):\n        return 2\n\n\nclass Child(Mixin, Base):\n    def run(self):\n        return self.step()\n\n\nclass Own(Mixin, Base):\n    def step(self):\n        return super().step()\n\n    def run(self):\n        return self.step()\n\n\nclass Grand(Own):\n    def go(self):\n        return self.step()\n",
        ),
        (
            "mro/d.py",
            "class A:\n    def hook(self):\n        return 0\n\n\nclass B(A):\n    def other(self):\n        return 1\n\n\nclass C(A):\n    def hook(self):\n        return 2\n\n\nclass D(B, C):\n    def run(self):\n        return self.hook()\n\n    def hook(self):\n        return super().hook()\n\n\nclass E(B, C):\n    def run(self):\n        return self.hook()\n\n    @classmethod\n    def make(cls):\n        return cls.hook(cls)\n",
        ),
    ]);
    let id_of = |path: &str, line: i64| -> String {
        let c = db::open_ro().unwrap();
        let id: i64 = c
            .query_row(
                "SELECT id FROM node WHERE name = ?1 AND path = ?2 AND line = ?3",
                rusqlite::params![if path.ends_with("a.py") { "step" } else { "hook" }, path, line],
                |r| r.get(0),
            )
            .unwrap();
        format!("c:{id:x}")
    };
    // Base.step (line 2): nothing in this hierarchy reaches it through Mixin.
    let out = fox_code(&p, &id_of("mro/a.py", 2), "callers", 800);
    assert!(out.contains("0 call sites"), "{out}");
    // Mixin.step (line 7): Child.run, and Own's super() call.
    let out = fox_code(&p, &id_of("mro/a.py", 7), "callers", 800);
    assert!(out.contains("Child.run") && out.contains("Own.step") && !out.contains("Own.run") && !out.contains("Grand.go"), "{out}");
    // Own.step (line 17): Own.run and the subclass Grand.go that does not override it.
    let out = fox_code(&p, &id_of("mro/a.py", 17), "callers", 800);
    assert!(out.contains("Own.run") && out.contains("Grand.go") && !out.contains("Child.run"), "{out}");
    // Diamond: D(B, C) -> D, B, C, A. A.hook is never reached; C.hook is reached by E and by
    // D's super() call.
    let out = fox_code(&p, &id_of("mro/d.py", 2), "callers", 800);
    assert!(out.contains("0 call sites"), "{out}");
    let out = fox_code(&p, &id_of("mro/d.py", 12), "callers", 800);
    assert!(out.contains("E.run") && out.contains("E.make") && out.contains("D.hook") && !out.contains("D.run"), "{out}");
    let out = fox_code(&p, &id_of("mro/d.py", 20), "callers", 800);
    assert!(out.contains("D.run"), "{out}");
}

/// Python method resolution order: mixins, `super()`, diamonds and overriding subclasses.
const PY_MRO_FILES: &[(&str, &str)] = &[
    ("pkg/__init__.py", ""),
    ("pkg/base.py", "class Base:\n    def run(self):\n        return self.step()\n\n    def step(self):\n        return 1\n"),
    ("pkg/mixins.py", "class Mixin:\n    def step(self):\n        return 2\n\n    def hook(self):\n        return 0\n"),
    (
        "pkg/child.py",
        "from pkg.base import Base\nfrom pkg.mixins import Mixin\n\n\nclass Child(Mixin, Base):\n    def go(self):\n        return self.step()\n\n    def both(self):\n        return super().step()\n\n\nclass Leaf(Child):\n    def step(self):\n        return super().step() + 1\n\n    def deep(self):\n        return self.step()\n\n    def inherited(self):\n        return self.go()\n",
    ),
    (
        "pkg/diamond.py",
        "class A:\n    def f(self):\n        return 0\n\n\nclass B(A):\n    def f(self):\n        return 1\n\n\nclass C(A):\n    def f(self):\n        return 2\n\n\nclass D(B, C):\n    def g(self):\n        return self.f()\n",
    ),
    (
        "pkg/skip.py",
        "class Q:\n    def other(self):\n        return 0\n\n\nclass P:\n    def hook(self):\n        return 1\n\n\nclass R(Q, P):\n    def go(self):\n        return self.hook()\n",
    ),
];

#[test]
fn self_call_reaches_the_first_override_in_mro_order() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(PY_MRO_FILES);
    let out = fox_code(&p, "Mixin.step", "callers", 6000);
    assert!(rows(&out).contains("Child.go"), "Child(Mixin, Base).go calls Mixin.step: {out}");
    let out = fox_code(&p, "Base.step", "callers", 6000);
    assert!(!rows(&out).contains("Child.go"), "Mixin overrides it for Child: {out}");
    assert!(rows(&out).contains("Base.run"), "{out}");
}

#[test]
fn super_calls_follow_the_mro_after_the_current_class() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(PY_MRO_FILES);
    let out = fox_code(&p, "Mixin.step", "callers", 6000);
    assert!(
        rows(&out).contains("Child.both") && rows(&out).contains("Leaf.step"),
        "super() in Child and in Leaf both reach Mixin.step: {out}"
    );
    let out = fox_code(&p, "Base.step", "callers", 6000);
    assert!(!rows(&out).contains("Child.both") && !rows(&out).contains("Leaf.step"), "{out}");
}

#[test]
fn a_subclass_override_takes_its_own_self_calls() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(PY_MRO_FILES);
    let out = fox_code(&p, "Leaf.step", "callers", 6000);
    assert!(rows(&out).contains("Leaf.deep"), "{out}");
    assert!(!rows(&out).contains("Child.go") && !rows(&out).contains("Leaf.inherited"), "{out}");
    let out = fox_code(&p, "Mixin.step", "callers", 6000);
    assert!(!rows(&out).contains("Leaf.deep"), "{out}");
}

#[test]
fn diamond_inheritance_picks_the_left_base_first() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(PY_MRO_FILES);
    let out = fox_code(&p, "B.f", "callers", 6000);
    assert!(rows(&out).contains("D.g"), "{out}");
    let out = fox_code(&p, "C.f", "callers", 6000);
    assert!(!rows(&out).contains("D.g"), "{out}");
    let out = fox_code(&p, "A.f", "callers", 6000);
    assert!(!rows(&out).contains("D.g"), "{out}");
}

#[test]
fn a_method_defined_only_further_along_the_mro_is_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(PY_MRO_FILES);
    let out = fox_code(&p, "P.hook", "callers", 6000);
    assert!(rows(&out).contains("R.go"), "{out}");
    let out = fox_code(&p, "Mixin.hook", "callers", 6000);
    assert!(!rows(&out).contains("R.go"), "{out}");
}

/// Python: a base the index cannot follow (a library class) ahead of the definition leaves the
/// call to name resolution as a guess, never a certain call of the later base's method.
#[test]
fn python_unknown_base_before_the_definition_keeps_the_call_a_guess() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("a.py", "class Base:\n    def step(self):\n        return 1\n"),
        (
            "b.py",
            "from external import External\nfrom a import Base\n\n\nclass Ext(External, Base):\n    def go(self):\n        return self.step()\n",
        ),
        ("c.py", "from a import Base\n\n\nclass Known(object, Base):\n    def run(self):\n        return self.step()\n"),
    ]);
    let out = fox_code(&p, "Base.step", "callers", 6000);
    let go = rows(&out).lines().find(|l| l.contains("Ext.go")).map(str::to_string);
    assert!(go.as_deref().is_none_or(|l| l.contains("guess")), "{out}");
    let run = rows(&out).lines().find(|l| l.contains("Known.run")).map(str::to_string);
    assert!(run.as_deref().is_some_and(|l| !l.contains("guess")), "{out}");
}

fn cpp_recv_project() -> (tempfile::TempDir, tempfile::TempDir, project::Project) {
    inline_project(&[
        ("src/engine.hpp", "class Engine {\npublic:\n    void start();\n    int rpm = 0;\n};\n"),
        ("src/engine.cpp", "#include \"engine.hpp\"\nvoid Engine::start() { rpm = 1; }\n"),
        ("src/pump.hpp", "class Pump {\npublic:\n    void start() {}\n};\nclass Valve : public Pump {};\n"),
        ("src/turbine.hpp", "namespace plant {\nclass Turbine {\npublic:\n    void start() {}\n};\n}\n"),
        (
            "src/uses.cpp",
            "#include <memory>\n#include \"engine.hpp\"\n#include \"pump.hpp\"\n#include \"turbine.hpp\"\n\
struct Car {\n    Engine engine;\n    Pump* pump;\n    std::unique_ptr<plant::Turbine> turbine;\n    void go() { engine.start(); }\n    void stop();\n};\n\
void Car::stop() { this->pump->start(); }\n\
void local_value() { Engine e; e.start(); }\n\
void by_ptr(Pump* p) { p->start(); }\n\
void by_ref(const Pump& p) { p.start(); }\n\
void by_car(Car& car) { car.engine.start(); car.pump->start(); car.turbine->start(); }\n\
void by_auto() { auto e = Engine(); e.start(); }\n\
void by_make() { auto p = std::make_unique<Pump>(); p->start(); }\n\
void by_new() { auto* t = new plant::Turbine(); t->start(); }\n\
void by_ctor() { Engine e(3); e.start(); }\n\
void shadow(Pump* e) { Engine e2; { Pump e; e.start(); } e2.start(); }\n\
void lambdas(Engine* x) { auto f = [](Pump* x) { x->start(); }; x->start(); }\n\
void unknown(auto& x) { x.start(); }\n\
void unknown2() { auto v = lookup(); v.start(); }\n",
        ),
        (
            "src/more.cpp",
            "#include <memory>\n#include <vector>\n#include \"engine.hpp\"\n#include \"pump.hpp\"\n\
Engine g_engine;\n\
void globals() { g_engine.start(); }\n\
void copy_init() { Engine e = Engine(); e.start(); }\n\
void shared() { std::shared_ptr<Pump> p = std::make_shared<Pump>(); p->start(); }\n\
void by_alias(Engine& e) { Engine& r = e; r.start(); }\n\
void ranged(std::vector<Engine>& es) { for (Engine& e : es) e.start(); }\n\
void ranged_auto(std::vector<Engine>& es) { for (auto& a : es) a.start(); }\n\
void casted(void* v) { static_cast<Pump*>(v)->start(); ((Engine*)v)->start(); }\n",
        ),
    ])
}

fn call_lines(out: &str) -> Vec<String> {
    resolved(out).lines().filter(|l| l.starts_with("  :")).map(|l| l.split(" | ").next().unwrap().to_string()).collect()
}

/// A call through a receiver whose type is plain in the code (a local, a parameter, a member of a
/// typed struct, `auto` initialized by a constructor, `new` or `make_unique`) reaches that
/// class's method with certainty, and no other class's; one whose type is unknown is a guess.
#[test]
fn cpp_member_calls_resolve_through_the_receivers_type() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = cpp_recv_project();
    let lines = |sym: &str| -> Vec<String> {
        let out = fox_code(&p, sym, "callers", 4000);
        call_lines(&out).into_iter().map(|l| l.trim().to_string()).collect()
    };
    let has = |ls: &[String], f: &str| ls.iter().any(|l| l.contains(&format!(" {f} ")));
    let engine = lines("Engine.start");
    for f in ["Car.go", "local_value", "by_car", "by_auto", "by_ctor", "shadow", "lambdas"] {
        assert!(
            has(&engine, f) && !engine.iter().filter(|l| l.contains(&format!(" {f} "))).any(|l| l.contains("guess")),
            "{f}: {engine:?}"
        );
    }
    for f in ["by_ptr", "by_ref", "by_make", "by_new", "Car.stop"] {
        assert!(!has(&engine, f), "{f}: {engine:?}");
    }
    for f in ["globals", "copy_init", "by_alias", "ranged", "casted"] {
        assert!(has(&engine, f), "{f}: {engine:?}");
    }
    // The receivers of unknown type (`auto x = lookup()`, `auto& a : es`) are guesses.
    assert!(engine.iter().filter(|l| l.contains("guess")).count() == 3 && has(&engine, "ranged_auto"), "{engine:?}");
    let pump = lines("Pump.start");
    for f in ["by_ptr", "by_ref", "by_car", "by_make", "shadow", "lambdas", "Car.stop", "shared", "casted"] {
        assert!(has(&pump, f), "{f}: {pump:?}");
    }
    assert!(pump.iter().all(|l| !l.contains("guess")), "{pump:?}");
    for f in ["Car.go", "local_value", "by_auto", "by_new", "by_ctor", "unknown"] {
        assert!(!has(&pump, f), "{f}: {pump:?}");
    }
    let turbine = lines("Turbine.start");
    assert!(has(&turbine, "by_car") && has(&turbine, "by_new") && turbine.len() == 2, "{turbine:?}");
    assert!(turbine.iter().all(|l| !l.contains("guess")), "{turbine:?}");
    let out = fox_code(&p, "Turbine.start", "callers", 4000);
    assert!(out.contains("2 call sites") && !out.contains("guessed by name"), "{out}");
}

/// A class that inherits the method, an external type and a name that is no local nor member of
/// the class stay guesses; a receiver in a header-only class and an edit to a field's type
/// follow.
#[test]
fn cpp_member_calls_without_a_known_class_stay_guesses_and_follow_edits() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, p) = inline_project(&[
        (
            "a.hpp",
            "struct Base { void run() {} };\nstruct Derived : Base {};\nstruct Other { void run() {} };\nstruct Holder { Base* b; };\n",
        ),
        (
            "use.cpp",
            "#include \"a.hpp\"\n#include <string>\nvoid inherited(Derived* d) { d->run(); }\nvoid external(std::string& s) { s.run(); }\nvoid held(Holder& h) { h.b->run(); }\nvoid global_name() { g_thing.run(); }\n",
        ),
    ]);
    let out = fox_code(&p, "Base.run", "callers", 4000);
    let r = resolved(&out);
    // The receiver of `held` is a plain `Base*`; the three others are guesses (or unresolved).
    assert!(r.contains(":5 held c:") && !r.lines().any(|l| l.contains(":5 held") && l.contains("guess")), "{out}");
    for f in [":3 inherited", ":4 external", ":6 global_name"] {
        assert!(!r.lines().any(|l| l.contains(f) && !l.contains("guess")), "{f}: {out}");
    }
    let before = fox_code(&p, "Other.run", "callers", 4000);
    assert!(!resolved(&before).contains(":5 held"), "{before}");
    // The field's type changes in the header alone: the call in the unchanged file follows.
    let mut c = db::open_rw().unwrap();
    let h = repo.path().join("a.hpp");
    let text = std::fs::read_to_string(&h).unwrap();
    std::fs::write(&h, text.replace("Base* b;", "Other* b;")).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    assert_eq!(tsindex::index_project(&mut c, &p).unwrap().parsed, 1);
    let base = fox_code(&p, "Base.run", "callers", 4000);
    let other = fox_code(&p, "Other.run", "callers", 4000);
    assert!(!resolved(&base).contains(":5 held") && resolved(&other).contains(":5 held c:"), "{base}\n{other}");
    let snap = |c: &rusqlite::Connection| -> Vec<(i64, i64, i64)> {
        let mut q = c.prepare("SELECT rowid, dst, amb FROM ts_ref ORDER BY rowid").unwrap();
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().flatten().collect()
    };
    let inc = snap(&c);
    tsindex::resolve(&c, p.id).unwrap();
    assert_eq!(snap(&c), inc);
}

/// C++ receivers of a known type reach their class; untyped ones stay guesses.
const CPP_RECV_FILES: &[(&str, &str)] = &[
    ("src/engine.hpp", "class Engine {\npublic:\n    int start(int x) { return x; }\n    int stop() { return 0; }\n};\n"),
    ("src/pump.hpp", "class Pump {\npublic:\n    int start(int x) { return x + 1; }\n};\n"),
    ("src/valve.hpp", "class Valve {\npublic:\n    int start(int x) { return x + 2; }\n};\n"),
    (
        "src/car.cpp",
        "#include \"engine.hpp\"\n#include \"pump.hpp\"\n#include \"valve.hpp\"\n#include <memory>\nstruct Car {\n    Engine eng;\n    Pump* pump;\n};\nint local_obj() {\n    Engine e;\n    return e.start(1);\n}\nint by_ptr(Pump* p) {\n    return p->start(2);\n}\nint by_ref(const Pump& p) {\n    return p.start(3);\n}\nint by_member(Car& c) {\n    return c.eng.start(4) + c.pump->start(5);\n}\nint by_auto_ctor() {\n    auto e = Engine();\n    return e.start(6);\n}\nint by_make_unique() {\n    auto p = std::make_unique<Pump>();\n    return p->start(7);\n}\nint by_valve(Valve v) {\n    return v.start(8);\n}\nint unknown(T t) {\n    return t.start(9);\n}\n",
    ),
];

fn cpp_row_for<'a>(out: &'a str, func: &str) -> Option<&'a str> {
    out.lines().find(|l| l.trim_start().starts_with(':') && l.contains(&format!(" {func} c:")))
}

fn cpp_certain(out: &str, funcs: &[&str]) {
    for f in funcs {
        let r = cpp_row_for(out, f).unwrap_or_else(|| panic!("{f} missing: {out}"));
        assert!(!r.contains("guess"), "{f} should be a certain call: {out}");
    }
}

fn cpp_absent(out: &str, funcs: &[&str]) {
    for f in funcs {
        assert!(cpp_row_for(out, f).is_none(), "{f} must not call it: {out}");
    }
}

#[test]
fn locals_params_and_members_of_a_known_type_reach_their_class() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(CPP_RECV_FILES);
    let out = fox_code(&p, "Engine.start", "callers", 6000);
    cpp_certain(&out, &["local_obj", "by_member", "by_auto_ctor"]);
    cpp_absent(&out, &["by_ptr", "by_ref", "by_make_unique", "by_valve"]);
}

#[test]
fn pointer_and_reference_receivers_reach_their_class() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(CPP_RECV_FILES);
    let out = fox_code(&p, "Pump.start", "callers", 6000);
    cpp_certain(&out, &["by_ptr", "by_ref", "by_member"]);
    cpp_absent(&out, &["local_obj", "by_valve", "by_auto_ctor"]);
}

#[test]
fn smart_pointer_factories_give_the_receiver_its_type() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(CPP_RECV_FILES);
    let out = fox_code(&p, "Pump.start", "callers", 6000);
    cpp_certain(&out, &["by_make_unique"]);
    let out = fox_code(&p, "Valve.start", "callers", 6000);
    cpp_certain(&out, &["by_valve"]);
    cpp_absent(&out, &["by_make_unique", "by_ptr"]);
}

#[test]
fn an_untyped_receiver_stays_a_guess_or_unresolved_never_certain() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(CPP_RECV_FILES);
    for q in ["Engine.start", "Pump.start", "Valve.start"] {
        let out = fox_code(&p, q, "callers", 6000);
        if let Some(r) = cpp_row_for(&out, "unknown") {
            assert!(r.contains("guess"), "{q}: {out}");
        }
    }
}

#[test]
fn only_the_untyped_receiver_is_still_a_guess() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(CPP_RECV_FILES);
    let out = fox_code(&p, "Engine.start", "callers", 6000);
    // Three typed calls are certain; only the receiver of unknown type is a guess.
    assert!(out.contains("4 call sites in 1 files, 1 guessed by name"), "{out}");
    // Engine.stop has one definition and no caller; nothing leaks onto it.
    let out = fox_code(&p, "Engine.stop", "callers", 6000);
    assert!(out.contains("0 call sites"), "{out}");
}

/// C++ too: a member declared through a function-pointer typedef is a function member; a
/// pointer to one and a class-typed member stay fields.
#[test]
fn cpp_typedef_function_pointer_members_are_function_members() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "ops.hpp",
            "typedef int (*op_fn)(int);\ntypedef op_fn op2_fn;\ntypedef op_fn *opp;\nclass Engine { public: int start(int x) { return x; } };\nstruct ops {\n\top_fn run;\n\top2_fn run2;\n\topp many;\n\tEngine eng;\n};\n",
        ),
        ("use.cpp", "#include \"ops.hpp\"\nint drive(ops *o) { return o->run(1) + o->run2(2); }\n"),
    ]);
    let c = db::open_ro().unwrap();
    let kind = |n: &str| -> String {
        c.query_row(
            "SELECT json_extract(meta,'$.k') FROM node WHERE project_id = ?1 AND name = ?2 AND kind = 'symbol' AND path = 'ops.hpp'",
            rusqlite::params![p.id, n],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(kind("run"), "method");
    assert_eq!(kind("run2"), "method");
    assert_eq!(kind("many"), "field");
    assert_eq!(kind("eng"), "field");
    let out = fox_code(&p, "ops.run", "callers", 6000);
    assert!(rows(&out).contains("drive"), "{out}");
}

/// A call row of `func` in a `callers` answer, if any.
fn row_of<'a>(out: &'a str, func: &str) -> Option<&'a str> {
    out.lines().find(|l| l.trim_start().starts_with(':') && l.contains(&format!(" {func} c:")))
}

/// `func`'s call is not credited to the target with certainty (absent or a guess), and the
/// answer does not read as complete because of it.
fn not_certain(out: &str, func: &str) {
    if let Some(r) = row_of(out, func) {
        assert!(r.contains("guess"), "{func} must not be certain: {out}");
    }
}

/// C++: a smart pointer's or optional's own members (`p.reset()`) are no pointee's method; `->`
/// reaches the pointee.
#[test]
fn cpp_smart_pointer_own_members_are_not_the_pointees() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[(
        "a.cpp",
        "#include <memory>\n#include <optional>\nclass Engine {\npublic:\n    void reset() {}\n    void start() {}\n};\nvoid by_up(std::unique_ptr<Engine> p) { p.reset(); }\nvoid by_make() { auto p = std::make_unique<Engine>(); p.reset(); }\nvoid by_opt() { auto o = std::make_optional<Engine>(); o.reset(); }\nvoid by_opt2(std::optional<Engine> o) { o.reset(); }\nvoid arrow(std::unique_ptr<Engine> p) { p->start(); }\nvoid deref(std::shared_ptr<Engine> p) { (*p).start(); }\n",
    )]);
    let out = fox_code(&p, "Engine.reset", "callers", 6000);
    for f in ["by_up", "by_make", "by_opt", "by_opt2"] {
        not_certain(&out, f);
    }
    assert!(!out.contains("all accounted for") || !rows(&out).contains("by_"), "{out}");
    let out = fox_code(&p, "Engine.start", "callers", 6000);
    for f in ["arrow", "deref"] {
        assert!(row_of(&out, f).is_some_and(|r| !r.contains("guess")), "{f}: {out}");
    }
}

/// C++: a qualified receiver type must name the candidate's own class, an unqualified one the
/// only class of that name; template parameters are no classes; overloads, virtual methods and
/// a call through a name that is also a member function stay guesses.
#[test]
fn cpp_receivers_in_doubt_stay_guesses() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "a.cpp",
            "#include <memory>\n#include <string>\nclass Pump { public: void reset() {} };\nclass Engine {\npublic:\n    void reset() {}\n    void put(int x) {}\n    void put(const char* s) {}\n};\nvoid by_up(std::unique_ptr<Engine> p) { p.reset(); }\nvoid over(Engine& e) { e.put(\"x\"); }\n",
        ),
        (
            "b.cpp",
            "namespace mylib { class string { public: int size() { return 0; } }; }\nstruct Base { void run() {} };\nnamespace other { struct Holder : Base {}; }\n",
        ),
        (
            "c.cpp",
            "#include <string>\nnamespace x { struct Holder { void run() {} }; }\nint ext(std::string& s) { return s.size(); }\nvoid ns(other::Holder& h) { h.run(); }\ntemplate <class Engine> void tpl(Engine e) { e.reset(); }\nstruct Base2 { virtual void go() {} };\nstruct Derived2 : Base2 { void go() override {} };\nvoid virt(Base2* b) { b->go(); }\nvoid qual(x::Holder& h) { h.run(); }\n",
        ),
        (
            "d.cpp",
            "class Motor { public: void spin() {} };\nclass Rotor { public: void spin() {} };\nMotor make() { return Motor(); }\nclass Car {\npublic:\n    Rotor make() { return Rotor(); }\n    void go() { auto p = make(); p.spin(); }\n};\n",
        ),
    ]);
    let out = fox_code(&p, "string.size", "callers", 6000);
    not_certain(&out, "ext");
    let out = fox_code(&p, "x.Holder.run", "callers", 6000);
    not_certain(&out, "ns");
    assert!(row_of(&out, "qual").is_some_and(|r| !r.contains("guess")), "{out}");
    let out = fox_code(&p, "Base.run", "callers", 6000);
    not_certain(&out, "ns");
    let out = fox_code(&p, "Engine.reset", "callers", 6000);
    not_certain(&out, "tpl");
    not_certain(&out, "by_up");
    let c = db::open_ro().unwrap();
    let mut q = c.prepare("SELECT id FROM node WHERE project_id = ?1 AND name = 'put' AND kind = 'symbol'").unwrap();
    let puts: Vec<i64> = q.query_map([p.id], |r| r.get(0)).unwrap().flatten().collect();
    assert_eq!(puts.len(), 2);
    for id in puts {
        let out = fox_code(&p, &format!("c:{id:x}"), "callers", 6000);
        not_certain(&out, "over");
    }
    let out = fox_code(&p, "Base2.go", "callers", 6000);
    not_certain(&out, "virt");
    let out = fox_code(&p, "Derived2.go", "callers", 6000);
    assert!(!out.lines().next().unwrap_or("").contains("all accounted for"), "{out}");
    let out = fox_code(&p, "Motor.spin", "callers", 6000);
    not_certain(&out, "Car.go");
}

/// C++: a class-typed member stays a field when a same-name typedef of a function pointer lives
/// in another namespace, and its qualified type still reaches its class.
#[test]
fn cpp_class_member_is_no_function_member_beside_a_same_name_typedef() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[(
        "a.hpp",
        "namespace cb { typedef void (*Engine)(int); }\nnamespace car { class Engine { public: void start() {} }; }\nstruct Car { car::Engine engine; void go() { engine.start(); } };\n",
    )]);
    let c = db::open_ro().unwrap();
    let k: String = c
        .query_row(
            "SELECT json_extract(meta,'$.k') FROM node WHERE project_id = ?1 AND name = 'engine' AND kind = 'symbol'",
            rusqlite::params![p.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(k, "field");
    let out = fox_code(&p, "Engine.start", "callers", 6000);
    assert!(row_of(&out, "Car.go").is_some_and(|r| !r.contains("guess")), "{out}");
}

/// C++ files reclassify typedef'd function-pointer members on an incremental run as a full one
/// does.
#[test]
fn cpp_typedef_members_incremental_matches_full() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, p) = inline_project(&[
        ("t.hpp", "typedef int (*op_fn)(int);\n"),
        ("a.cpp", "#include \"t.hpp\"\nstruct Ops { int run; };\nint use(Ops *p) { return p->run; }\n"),
    ]);
    let mut c = db::open_rw().unwrap();
    std::fs::write(repo.path().join("a.cpp"), "#include \"t.hpp\"\nstruct Ops { op_fn run; };\nint use(Ops *p) { return p->run(1); }\n")
        .unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    let kind = |c: &rusqlite::Connection| -> String {
        c.query_row("SELECT json_extract(meta,'$.k') FROM node WHERE project_id = ?1 AND name = 'run' AND kind = 'symbol'", [p.id], |r| {
            r.get(0)
        })
        .unwrap()
    };
    assert_eq!(kind(&c), "method");
    let snap = |c: &rusqlite::Connection| -> Vec<(i64, i64, i64)> {
        let mut q = c.prepare("SELECT rowid, dst, amb FROM ts_ref ORDER BY rowid").unwrap();
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().flatten().collect()
    };
    let inc = snap(&c);
    tsindex::resolve(&c, p.id).unwrap();
    assert_eq!(snap(&c), inc);
}

/// Python: doubt leaves a `self.` or `super()` call a guess: a dynamic base, a class attribute
/// shadowing the method, a guessed base link, a subclass that overrides the method, an explicit
/// `super(Other, self)`.
#[test]
fn python_mro_in_doubt_stays_a_guess() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "a.py",
            "from typing import Generic, TypeVar\nT = TypeVar(\"T\")\n\n\nclass Mixin(Generic[T]):\n    def step(self):\n        return 2\n\n\nclass Base:\n    def step(self):\n        return 1\n\n\nclass Child(Mixin[int], Base):\n    def go(self):\n        return self.step()\n\n\ndef make():\n    return object\n\n\nclass Dyn(make(), Base):\n    def go2(self):\n        return self.step()\n\n\nclass Tmpl:\n    def run(self):\n        return self.hook()\n\n    def hook(self):\n        return 0\n\n\nclass Sub(Tmpl):\n    def hook(self):\n        return 1\n",
        ),
        (
            "b.py",
            "class Base3:\n    def step3(self):\n        return 1\n\n\ndef _other(self):\n    return 0\n\n\nclass Child3(Base3):\n    step3 = _other\n\n    def go3(self):\n        return self.step3()\n",
        ),
        ("k.py", "class K2:\n    def m(self):\n        return 2\n\n\nclass K1(K2):\n    def other(self):\n        return 1\n"),
        ("p1/g.py", "from k import K2\n\n\nclass G(K2):\n    def m(self):\n        return 3\n"),
        ("p2/g.py", "class G:\n    def m(self):\n        return 4\n"),
        ("c.py", "from k import K1\nfrom p1.g import *\n\n\nclass A(K1, G):\n    def go(self):\n        return self.m()\n"),
        (
            "d.py",
            "class A4:\n    def m4(self):\n        return 0\n\n\nclass B4(A4):\n    def m4(self):\n        return 1\n\n\nclass C4(B4):\n    def m4(self):\n        return super(B4, self).m4()\n",
        ),
    ]);
    let out = fox_code(&p, "Mixin.step", "callers", 6000);
    assert!(row_of(&out, "Child.go").is_some_and(|r| !r.contains("guess")), "{out}");
    let out = fox_code(&p, "Base.step", "callers", 6000);
    not_certain(&out, "Child.go");
    not_certain(&out, "Dyn.go2");
    let out = fox_code(&p, "Tmpl.hook", "callers", 6000);
    not_certain(&out, "Tmpl.run");
    let out = fox_code(&p, "Base3.step3", "callers", 6000);
    not_certain(&out, "Child3.go3");
    let out = fox_code(&p, "K2.m", "callers", 6000);
    not_certain(&out, "A.go");
    let out = fox_code(&p, "B4.m4", "callers", 6000);
    not_certain(&out, "C4.m4");
}

/// Python: changing a class's bases re-scores the `self.` calls of its subclasses in other files.
#[test]
fn python_base_edit_rescopes_subclass_calls() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo, p) = inline_project(&[
        ("mixins.py", "class Mixin:\n    def m(self):\n        return 1\n"),
        ("other.py", "class Other:\n    def m(self):\n        return 2\n"),
        ("b.py", "from mixins import Mixin\nfrom other import Other\n\n\nclass B(Other):\n    def x(self):\n        return 0\n"),
        ("c.py", "from b import B\n\n\nclass C(B):\n    def go(self):\n        return self.m()\n"),
        ("d.py", "from c import C\n\n\nclass D(C):\n    def go4(self):\n        return self.m()\n"),
    ]);
    let out = fox_code(&p, "Other.m", "callers", 6000);
    assert!(row_of(&out, "C.go").is_some() && row_of(&out, "D.go4").is_some(), "{out}");
    let mut c = db::open_rw().unwrap();
    std::fs::write(
        repo.path().join("b.py"),
        "from mixins import Mixin\nfrom other import Other\n\n\nclass B(Mixin):\n    def x(self):\n        return 0\n",
    )
    .unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    drop(c);
    let out = fox_code(&p, "Mixin.m", "callers", 6000);
    assert!(row_of(&out, "C.go").is_some() && row_of(&out, "D.go4").is_some(), "{out}");
    let out = fox_code(&p, "Other.m", "callers", 6000);
    assert!(row_of(&out, "C.go").is_none() && row_of(&out, "D.go4").is_none(), "{out}");
    let c = db::open_rw().unwrap();
    let snap = |c: &rusqlite::Connection| -> Vec<(i64, i64, i64)> {
        let mut q = c.prepare("SELECT rowid, dst, amb FROM ts_ref ORDER BY rowid").unwrap();
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().flatten().collect()
    };
    let inc = snap(&c);
    tsindex::resolve(&c, p.id).unwrap();
    assert_eq!(snap(&c), inc);
}

/// Every reference's (target, ambiguity), in rowid order.
fn ref_snapshot(c: &rusqlite::Connection) -> Vec<(i64, i64, i64)> {
    let mut q = c.prepare("SELECT rowid, dst, amb FROM ts_ref ORDER BY rowid").unwrap();
    q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().flatten().collect()
}

/// Re-index after writing `files`, then check the incremental result equals a full resolve.
fn reindex_matches_full(repo: &Path, p: &project::Project, files: &[(&str, &str)]) {
    for (f, text) in files {
        std::fs::write(repo.join(f), text).unwrap();
    }
    let mut c = db::open_rw().unwrap();
    inventory::index_project(&mut c, p).unwrap();
    tsindex::index_project(&mut c, p).unwrap();
    let inc = ref_snapshot(&c);
    tsindex::resolve(&c, p.id).unwrap();
    assert_eq!(ref_snapshot(&c), inc);
}

/// Python: a subclass's base list changing, or a new subclass file, re-scores the `self.` calls
/// of its base classes in other files (a subclass's order decides whether they are certain).
#[test]
fn python_subclass_edits_rescore_base_class_calls() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let base = "class Base:\n    def run(self):\n        return self.hook()\n\n    def hook(self):\n        return 0\n";
    let mixin = "class Mixin:\n    def hook(self):\n        return 1\n";
    let (_h, repo, p) = inline_project(&[
        ("base.py", base),
        ("mixin.py", mixin),
        ("sub.py", "from base import Base\nfrom mixin import Mixin\n\n\nclass Sub(Base, Mixin):\n    pass\n"),
    ]);
    let out = fox_code(&p, "Base.hook", "callers", 6000);
    assert!(row_of(&out, "Base.run").is_some_and(|r| !r.contains("guess")), "{out}");
    reindex_matches_full(
        repo.path(),
        &p,
        &[("sub.py", "from base import Base\nfrom mixin import Mixin\n\n\nclass Sub(Mixin, Base):\n    pass\n")],
    );
    let out = fox_code(&p, "Base.hook", "callers", 6000);
    not_certain(&out, "Base.run");
    // A new subclass file.
    let (_h, repo, p) = inline_project(&[("base.py", base), ("mixin.py", mixin)]);
    let out = fox_code(&p, "Base.hook", "callers", 6000);
    assert!(row_of(&out, "Base.run").is_some_and(|r| !r.contains("guess")), "{out}");
    reindex_matches_full(
        repo.path(),
        &p,
        &[("sub.py", "from base import Base\nfrom mixin import Mixin\n\n\nclass Sub(Mixin, Base):\n    pass\n")],
    );
    let out = fox_code(&p, "Base.hook", "callers", 6000);
    not_certain(&out, "Base.run");
}

/// Python: a base edit far up a wide hierarchy still re-scores a deep subclass's calls.
#[test]
fn python_base_edit_under_a_wide_hierarchy_matches_full() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let leaves: String = (0..2100).map(|i| format!("class L{i}(B1):\n    pass\n\n")).collect();
    let leaves = format!("from b1 import B1\n\n{leaves}");
    let (_h, repo, p) = inline_project(&[
        ("mixins.py", "class Mixin:\n    def m(self):\n        return 1\n"),
        ("other.py", "class Other:\n    def m(self):\n        return 2\n"),
        ("b.py", "from mixins import Mixin\nfrom other import Other\n\n\nclass B(Other):\n    pass\n"),
        ("b1.py", "from b import B\n\n\nclass B1(B):\n    pass\n"),
        ("leaves.py", &leaves),
        ("c.py", "from leaves import L2099\n\n\nclass C(L2099):\n    def go(self):\n        return self.m()\n"),
    ]);
    reindex_matches_full(
        repo.path(),
        &p,
        &[("b.py", "from mixins import Mixin\nfrom other import Other\n\n\nclass B(Mixin):\n    pass\n")],
    );
    let out = fox_code(&p, "Other.m", "callers", 6000);
    assert!(row_of(&out, "C.go").is_none_or(|r| r.contains("guess")), "{out}");
}

/// C++: a qualified receiver the index cannot place (a `using` alias, a namespace alias) keeps the name-based pick, as before; only a library namespace
/// (`std::`) is outside the project.
#[test]
fn cpp_unplaced_qualified_receivers_keep_the_name_pick() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "a.hpp",
            "namespace b { struct Real { void start() {} }; }\nnamespace a { using Engine = b::Real; }\nnamespace c = b;\n#define NS_BEGIN namespace m {\n#define NS_END }\nNS_BEGIN\nstruct Motor { void spin() {} };\nNS_END\n",
        ),
        (
            "u.cpp",
            "#include \"a.hpp\"\nvoid f1(a::Engine& e) { e.start(); }\nvoid f2(c::Real& r) { r.start(); }\nvoid f3(m::Motor& x) { x.spin(); }\n",
        ),
    ]);
    let out = fox_code(&p, "Real.start", "callers", 6000);
    assert!(row_of(&out, "f1").is_some() && row_of(&out, "f2").is_some(), "{out}");
    // `std::string` where only `mylib::string` exists: never certain.
    let (_h, _r, p) = inline_project(&[
        ("a.hpp", "namespace mylib { class string { public: int size() { return 0; } }; }\n"),
        ("u.cpp", "#include <string>\nint ext(std::string& s) { return s.size(); }\n"),
    ]);
    let out = fox_code(&p, "string.size", "callers", 6000);
    not_certain(&out, "ext");
    // A project that is the library: nested and inline namespaces still reach its classes.
    let (_h, _r, p) = inline_project(&[
        (
            "a.hpp",
            "namespace llvm::sys { class Path { public: void norm() {} }; }\nnamespace absl { inline namespace lts { class Status { public: void ok() {} }; } }\nnamespace fmt { class Buf { public: void ok() {} }; }\n",
        ),
        (
            "u.cpp",
            "#include \"a.hpp\"\nvoid f1(llvm::sys::Path& p) { p.norm(); }\nvoid f2(absl::Status& s) { s.ok(); }\nvoid f3(fmt::Buf& b) { b.ok(); }\n",
        ),
    ]);
    for (sym, f) in [("Path.norm", "f1"), ("Status.ok", "f2"), ("Buf.ok", "f3")] {
        let out = fox_code(&p, sym, "callers", 6000);
        assert!(row_of(&out, f).is_some_and(|r| !r.contains("guess")), "{sym}: {out}");
    }
}

/// C++: adding a class re-scores only the type paths that name it, not every member call.
#[test]
fn cpp_new_class_rescores_only_paths_naming_it() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut files: Vec<(String, String)> = vec![("a.hpp".into(), "namespace n { class Engine { public: void start() {} }; }\n".into())];
    for i in 0..30 {
        files.push((format!("u{i}.cpp"), format!("#include \"a.hpp\"\nvoid f{i}(n::Engine& e) {{ e.start(); }}\n")));
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let (_h, repo, p) = inline_project(&refs);
    std::fs::write(repo.path().join("u0.cpp"), "#include \"a.hpp\"\nvoid f0(n::Engine& e) { e.start(); }\nclass Zed { int q; };\n")
        .unwrap();
    let mut c = db::open_rw().unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    let st = tsindex::index_project(&mut c, &p).unwrap();
    assert!(st.rescored < 10, "{}", st.rescored);
    let inc = ref_snapshot(&c);
    tsindex::resolve(&c, p.id).unwrap();
    assert_eq!(ref_snapshot(&c), inc);
}

/// Python: a new class outside every hierarchy re-scores only its own calls, not every `self.`
/// call of the project.
#[test]
fn python_lonely_class_rescores_little() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut files: Vec<(String, String)> = vec![("root.py".into(), "class Root:\n    def m0(self):\n        return 0\n".into())];
    for i in 0..30 {
        files.push((format!("f{i}.py"), format!("from root import Root\n\n\nclass C{i}(Root):\n    def m1(self):\n        return self.m0()\n\n    def m2(self):\n        return self.m1()\n")));
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let (_h, repo, p) = inline_project(&refs);
    std::fs::write(repo.path().join("lonely.py"), "class Lonely:\n    def zz(self):\n        return 1\n").unwrap();
    let mut c = db::open_rw().unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    let st = tsindex::index_project(&mut c, &p).unwrap();
    assert!(st.rescored < 10, "{}", st.rescored);
}

/// C++: a qualifier never skips a namespace that is not inline: `a::Engine` is no
/// `a::detail::Engine` (the real `a::Engine` may be declared by a macro the index cannot read).
#[test]
fn cpp_qualifier_skips_only_inline_namespaces() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "a.hpp",
            "#define DECLARE_ENGINE(n) class n { public: void start(); };\nnamespace a {\nDECLARE_ENGINE(Engine)\nnamespace detail { class Engine { public: void start() {} }; }\n}\n",
        ),
        ("u.cpp", "#include \"a.hpp\"\nvoid f(a::Engine& e) { e.start(); }\n"),
    ]);
    let out = fox_code(&p, "detail.Engine.start", "callers", 6000);
    not_certain(&out, "f");
}

/// Python: a base change under a ~400-class hierarchy re-indexes quickly.
#[test]
fn python_base_change_on_a_wide_hierarchy_is_fast() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut files: Vec<(String, String)> = vec![
        ("root.py".into(), "class Root:\n    def m0(self):\n        return 0\n".into()),
        ("other.py".into(), "class Other:\n    def m0(self):\n        return 1\n".into()),
    ];
    for f in 0..40 {
        let mut t = String::from("from root import Root\n\n");
        for c in 0..10 {
            t.push_str(&format!("\nclass C{f}_{c}(Root):\n"));
            for m in 1..6 {
                t.push_str(&format!("    def m{m}(self):\n        return self.m{}()\n\n", m - 1));
            }
        }
        files.push((format!("f{f}.py"), t));
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let (_h, repo, p) = inline_project(&refs);
    std::fs::write(repo.path().join("root.py"), "from other import Other\n\n\nclass Root(Other):\n    pass\n").unwrap();
    let mut c = db::open_rw().unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    let t = std::time::Instant::now();
    let st = tsindex::index_project(&mut c, &p).unwrap();
    let took = t.elapsed();
    assert!(took < std::time::Duration::from_millis(1500), "{took:?}");
    // Only the calls of names the changed classes and their bases define (`m0`, 400 of them),
    // not all 2000 `self.` calls of the hierarchy.
    assert!(st.rescored < 1000, "{}", st.rescored);
    let inc = ref_snapshot(&c);
    tsindex::resolve(&c, p.id).unwrap();
    assert_eq!(ref_snapshot(&c), inc);
}
