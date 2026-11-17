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
    assert!(!out.contains("may be incomplete"), "{out}");
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
            "package app.feature\n\nimport app.math.stepOnce\n\nclass Screen(val theme: Theme, val inputs: Inputs) {\n    fun draw() = theme.current.palette.tintFor(1)\n    fun show(plan: String) = inputs.render(plan)\n    fun total(xs: List<Int>) = xs.fold(0, ::stepOnce)\n    fun glow() = theme.current.colors.glowFor(2)\n}\n",
        ),
    ]);
    let out = fox_code(&p, "tintFor", "callers", 800);
    // The receiver `palette` names the type `Palette`: no guess.
    assert!(out.contains("1 call sites in 1 files\n") && out.contains(":6 Screen.draw c:"), "{out}");
    // `render` is the method, not the class `Render`; its call through a property is found.
    let out = fox_code(&p, "render", "impact", 800);
    assert!(out.contains("method Inputs.render") && out.contains("Screen.show"), "{out}");
    // A receiver named unlike any type: a guess by name.
    let out = fox_code(&p, "glowFor", "callers", 800);
    assert!(out.contains("1 guessed by name") && out.contains(":9 Screen.glow c:") && out.contains(" guess by name"), "{out}");
    let out = fox_code(&p, "stepOnce", "impact", 800);
    assert!(out.contains("Screen.total"), "{out}");
}

/// When the name occurs in more files than the resolved sites, one line says so; when the
/// counts agree, nothing is added.
#[test]
fn callers_say_when_the_name_occurs_in_unlisted_files() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("kernel/core.c", "int wake_task(int p) { return p; }\nint run(void) { return wake_task(1); }\n"),
        ("mm/a.c", "int (*cb)(int);\nint use_a(void) { register_cb(wake_task); return 0; }\n"),
        ("mm/b.c", "int use_b(void) { return queue_work(wake_task, 2); }\n"),
    ]);
    let out = fox_code(&p, "wake_task", "callers", 800);
    assert!(
        out.contains(
            "1 call sites in 1 files\nmay be incomplete: `wake_task` also appears in 2 other files; confirm with: rg -nw wake_task"
        ),
        "{out}"
    );
    let out = fox_code(&p, "wake_task", "impact", 800);
    assert!(out.contains("\npartial at d1: `wake_task` also appears in 2 other files"), "{out}");
    let out = fox_code(&p, "run", "callers", 800);
    assert!(!out.contains("may be incomplete"), "{out}");
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
    let out = fox_code(&p, "GroupEvaluator.Preempt", "callers", 800);
    assert!(out.contains("0 call sites"), "{out}");
}
